//! The filesystem connection plugin: binds the `connection` seam for this
//! machine's local filesystem host. Enumerates sources read-only, extracts
//! envelopes, and serves fetches and scans. Service-specific connections
//! (Gmail, Slack, ...) arrive as loaded plugins on the same seam.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use walkdir::WalkDir;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError,
};
use inseam_kernel::text::slice_lines;
use inseam_seams::connection::{self, Connection, EnumeratedSource, CONNECTION};
use inseam_seams::SeamError;

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsConnectionConfig {
    /// Override the derived `fs-<hostname>` host id (tests, containers).
    pub host_id: Option<String>,
}

/// The provider plugin.
pub struct FsConnection {
    config: FsConnectionConfig,
}

impl FsConnection {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        Ok(Self {
            config: parse_config(config)?,
        })
    }
}

pub struct FsConnectionFactory;

impl inseam_kernel::substrate::PluginFactory for FsConnectionFactory {
    fn name(&self) -> &str {
        "connection-fs"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(FsConnection::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for FsConnection {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "connection-fs",
            inject: INJECT,
            provides: &["connection"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let host = match &self.config.host_id {
            Some(id) => FsHost::new(
                HostId::new(id.clone()).map_err(|e| PluginError(e.to_string()))?,
            ),
            None => FsHost::local(),
        };
        let facts = Facts::new()
            .with(connection::facts::CHANGE_FEED, false)
            .with(connection::facts::HOST, host.id().as_str());
        cx.provide(&CONNECTION, Arc::new(host) as Arc<dyn Connection>, facts)?;
        Ok(())
    }
}

/// The connection to the machine's filesystem host. Locators are absolute
/// paths with the leading `/` stripped, so any file the node can read is
/// addressable and `inseam://<host>/<path>` round-trips.
#[derive(Debug, Clone)]
pub struct FsHost {
    id: HostId,
}

impl FsHost {
    pub fn new(id: HostId) -> Self {
        Self { id }
    }

    /// The host for this machine, identified as `fs-<hostname>`.
    pub fn local() -> Self {
        let raw = gethostname::gethostname().to_string_lossy().to_lowercase();
        let mut cleaned: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        cleaned.truncate(48);
        let cleaned = cleaned.trim_matches('-');
        let id = if cleaned.is_empty() { "local" } else { cleaned };
        Self {
            id: HostId::new(format!("fs-{id}")).expect("sanitized host id is valid"),
        }
    }

    pub fn id(&self) -> &HostId {
        &self.id
    }

    pub fn address_for(&self, path: &Path) -> Result<Address, SeamError> {
        let rel = path
            .to_str()
            .and_then(|p| p.strip_prefix('/'))
            .filter(|p| !p.is_empty())
            .ok_or_else(|| SeamError::failed(format!("cannot address path {}", path.display())))?;
        let locator = Locator::new(rel)
            .map_err(|_| SeamError::failed(format!("cannot address path {}", path.display())))?;
        Ok(Address::new(self.id.clone(), locator))
    }

    /// The local path behind an address. Refuses foreign hosts and locators
    /// with parent-directory components.
    pub fn resolve(&self, address: &Address) -> Result<PathBuf, SeamError> {
        if address.host != self.id {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, but this node stewards `{}`",
                address.host, self.id
            )));
        }
        let rel = Path::new(address.locator.as_str());
        if rel.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(SeamError::Refused(format!(
                "locator `{}` contains a parent-directory component",
                address.locator.as_str()
            )));
        }
        Ok(Path::new("/").join(rel))
    }
}

#[async_trait::async_trait]
impl Connection for FsHost {
    fn host(&self) -> &HostId {
        &self.id
    }

    /// Walk a directory and emit every enumerable source: regular,
    /// non-hidden, non-empty files. Read-only; symlinks are not followed.
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let dir = Path::new(root)
            .canonicalize()
            .map_err(|e| SeamError::failed(format!("cannot enumerate {root}: {e}")))?;
        let observed = Timestamp::from(SystemTime::now());
        let mut sources = Vec::new();
        let walker = WalkDir::new(&dir).follow_links(false).into_iter();
        // depth 0 is the root the caller named: never prune it, even when
        // the directory itself is dot-named.
        for entry in walker.filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name())) {
            let entry = entry.map_err(|e| SeamError::failed(format!("walk failed: {e}")))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let meta = entry.metadata().map_err(|e| {
                SeamError::failed(format!("metadata of {}: {e}", entry.path().display()))
            })?;
            if meta.len() == 0 {
                continue;
            }
            let address = self.address_for(entry.path())?;
            let envelope = Envelope {
                source_type: "file".to_string(),
                content_type: detect_mimetype(entry.path()),
                length: ContentLength::Bytes(meta.len()),
                created: meta.created().ok().map(Timestamp::from),
                modified: meta.modified().ok().map(Timestamp::from),
                observed,
                properties: Vec::new(),
                hint: entry.file_name().to_str().map(str::to_string),
            };
            sources.push(EnumeratedSource {
                address,
                envelope,
                raw_bytes: meta.len(),
            });
        }
        Ok(sources)
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        let canonical = Path::new(root).canonicalize().ok()?;
        Some(self.address_for(&canonical).ok()?.locator.as_str().to_string())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let path = self.resolve(address)?;
        read_text_at(&path)
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        let path = self.resolve(address)?;
        let text = read_text_at(&path)?;
        slice_lines(&text, start, end).map_err(SeamError::failed)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let path = self.resolve(address)?;
        std::fs::read(&path).map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))
    }
}

fn read_text_at(path: &Path) -> Result<String, SeamError> {
    let bytes = std::fs::read(path)
        .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| n.starts_with('.'))
}

/// Extensions that mime_guess maps poorly or not at all but that are plainly
/// text for indexing purposes.
const TEXT_EXTENSIONS: &[&str] = &[
    "rs", "go", "py", "ts", "tsx", "js", "jsx", "swift", "c", "h", "cpp", "hpp", "sh", "zsh",
    "bash", "fish", "sql", "ini", "cfg", "conf", "log", "env", "lock", "canvas",
];

pub fn detect_mimetype(path: &Path) -> Mimetype {
    let guessed = mime_guess::from_path(path)
        .first()
        .and_then(|m| Mimetype::parse(m.essence_str()).ok());
    if let Some(m) = guessed {
        return m;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext {
        Some(e) if TEXT_EXTENSIONS.contains(&e.as_str()) => Mimetype::text_plain(),
        _ => Mimetype::parse("application/octet-stream").expect("literal mimetype is valid"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> FsHost {
        FsHost::new(HostId::new("fs-test").expect("valid host id"))
    }

    #[test]
    fn address_for_round_trips_through_resolve() {
        let h = host();
        let a = h
            .address_for(Path::new("/Users/greg/Data/notes/reno.md"))
            .expect("addressable");
        assert_eq!(a.to_string(), "inseam://fs-test/Users/greg/Data/notes/reno.md");
        let p = h.resolve(&a).expect("resolves");
        assert_eq!(p, Path::new("/Users/greg/Data/notes/reno.md"));
    }

    #[test]
    fn resolve_rejects_foreign_hosts() {
        let a: Address = "inseam://someone-else/etc/hosts".parse().expect("parses");
        assert!(host().resolve(&a).is_err());
    }

    #[test]
    fn resolve_rejects_traversal() {
        let a: Address = "inseam://fs-test/tmp/../etc/passwd".parse().expect("parses");
        assert!(matches!(host().resolve(&a), Err(SeamError::Refused(_))));
    }

    #[tokio::test]
    async fn enumerate_skips_hidden_and_empty_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.md"), "# hi\n").expect("write");
        std::fs::write(dir.path().join(".secret"), "x").expect("write");
        std::fs::write(dir.path().join("empty.txt"), "").expect("write");
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join(".git/config"), "x").expect("write");

        let sources = host()
            .enumerate(dir.path().to_str().expect("utf8"))
            .await
            .expect("enumerates");
        let names: Vec<_> = sources
            .iter()
            .filter_map(|s| s.envelope.hint.as_deref())
            .collect();
        assert_eq!(names, vec!["note.md"]);
        assert_eq!(sources[0].envelope.content_type.essence(), "text/markdown");
    }

    #[test]
    fn detects_code_files_as_text() {
        assert_eq!(detect_mimetype(Path::new("main.zsh")).essence(), "text/plain");
        assert!(detect_mimetype(Path::new("config.json")).is_indexable_text());
        assert!(!detect_mimetype(Path::new("photo.jpeg")).is_indexable_text());
    }
}
