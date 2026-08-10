//! The built-in filesystem connection: how this node stewards its local
//! filesystem host. Enumerates sources read-only, extracts envelopes, and
//! serves fetches and scans. Service-specific hosts (Gmail, Slack, ...) will
//! arrive as WASM connection plugins (`design/plugins.md`); the local
//! filesystem is core because a node always has one.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use thiserror::Error;

use crate::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use crate::fragment::Mimetype;

#[derive(Debug, Error)]
pub enum FsHostError {
    #[error("address {0} names host `{1}`, but this node stewards `{2}`")]
    ForeignHost(Address, HostId, HostId),
    #[error("locator `{0}` contains a parent-directory component")]
    Traversal(String),
    #[error("no file at {0}")]
    NotFound(PathBuf),
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is outside the filesystem root")]
    NotAbsolute(PathBuf),
    #[error("scan start line {start} is beyond the {len}-line source")]
    RangeBeyondEnd { start: u64, len: u64 },
    #[error("cannot build an address from path {0}")]
    BadPath(PathBuf),
}

/// A source found by enumeration: its address, envelope, and the local path
/// the steward uses to serve it.
#[derive(Debug, Clone)]
pub struct EnumeratedSource {
    pub address: Address,
    pub envelope: Envelope,
    pub path: PathBuf,
    /// On-disk size; the indexer stores it for change detection even after
    /// the envelope's length becomes line-based.
    pub raw_bytes: u64,
}

/// The steward's connection to the machine's filesystem host. Locators are
/// absolute paths with the leading `/` stripped, so any file the node can
/// read is addressable and `inseam://<host>/<path>` round-trips.
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

    pub fn address_for(&self, path: &Path) -> Result<Address, FsHostError> {
        let rel = path
            .to_str()
            .and_then(|p| p.strip_prefix('/'))
            .filter(|p| !p.is_empty())
            .ok_or_else(|| FsHostError::BadPath(path.to_path_buf()))?;
        let locator =
            Locator::new(rel).map_err(|_| FsHostError::BadPath(path.to_path_buf()))?;
        Ok(Address::new(self.id.clone(), locator))
    }

    /// The local path behind an address. Refuses foreign hosts and locators
    /// with parent-directory components.
    pub fn resolve(&self, address: &Address) -> Result<PathBuf, FsHostError> {
        if address.host != self.id {
            return Err(FsHostError::ForeignHost(
                address.clone(),
                address.host.clone(),
                self.id.clone(),
            ));
        }
        let rel = Path::new(address.locator.as_str());
        if rel
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(FsHostError::Traversal(address.locator.as_str().to_string()));
        }
        Ok(Path::new("/").join(rel))
    }

    /// Walk a directory and emit every enumerable source: regular,
    /// non-hidden, non-empty files. Read-only; symlinks are not followed.
    pub fn enumerate(&self, dir: &Path) -> Result<Vec<EnumeratedSource>, FsHostError> {
        let dir = dir.canonicalize().map_err(|source| FsHostError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let observed = Timestamp::from(SystemTime::now());
        let mut sources = Vec::new();
        let walker = WalkDir::new(&dir).follow_links(false).into_iter();
        // depth 0 is the root the caller named: never prune it, even when the
        // directory itself is dot-named.
        for entry in walker.filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name())) {
            let entry = entry.map_err(|e| {
                let path = e.path().map(Path::to_path_buf).unwrap_or_default();
                match e.into_io_error() {
                    Some(source) => FsHostError::Io { path, source },
                    None => FsHostError::NotFound(path),
                }
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let meta = entry.metadata().map_err(|e| FsHostError::Io {
                path: entry.path().to_path_buf(),
                source: e.into_io_error().unwrap_or_else(|| {
                    std::io::Error::other("walkdir metadata error without io cause")
                }),
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
                path: entry.path().to_path_buf(),
                raw_bytes: meta.len(),
            });
        }
        Ok(sources)
    }

    /// Full content of a text source, lossily decoded.
    pub fn read_text(&self, address: &Address) -> Result<String, FsHostError> {
        let path = self.resolve(address)?;
        read_text_at(&path)
    }

    /// Lines `start..=end` (1-based, inclusive) of a text source. The end is
    /// clamped to the file; a start beyond the file is an error.
    pub fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, FsHostError> {
        let path = self.resolve(address)?;
        let text = read_text_at(&path)?;
        slice_lines(&text, start, end)
    }

    /// Raw bytes of a source, for fetches of non-text content.
    pub fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, FsHostError> {
        let path = self.resolve(address)?;
        std::fs::read(&path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => FsHostError::NotFound(path.clone()),
            _ => FsHostError::Io {
                path: path.clone(),
                source,
            },
        })
    }
}

fn read_text_at(path: &Path) -> Result<String, FsHostError> {
    let bytes = std::fs::read(path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => FsHostError::NotFound(path.to_path_buf()),
        _ => FsHostError::Io {
            path: path.to_path_buf(),
            source,
        },
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Slice 1-based inclusive line range out of text, clamping the end.
pub fn slice_lines(text: &str, start: u64, end: u64) -> Result<String, FsHostError> {
    let len = count_lines(text);
    if start == 0 || start > len {
        return Err(FsHostError::RangeBeyondEnd { start, len });
    }
    let end = end.min(len).max(start);
    let out: Vec<&str> = text
        .lines()
        .skip(start as usize - 1)
        .take((end - start + 1) as usize)
        .collect();
    Ok(out.join("\n"))
}

/// Line count as `scan` and extents see it.
pub fn count_lines(text: &str) -> u64 {
    text.lines().count() as u64
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

/// Whether content of this type is worth reading as text for indexing.
pub fn is_texty(m: &Mimetype) -> bool {
    m.is_indexable_text()
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
        assert!(matches!(
            host().resolve(&a),
            Err(FsHostError::ForeignHost(..))
        ));
    }

    #[test]
    fn resolve_rejects_traversal() {
        let a: Address = "inseam://fs-test/tmp/../etc/passwd".parse().expect("parses");
        assert!(matches!(host().resolve(&a), Err(FsHostError::Traversal(_))));
    }

    #[test]
    fn enumerate_skips_hidden_and_empty_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.md"), "# hi\n").expect("write");
        std::fs::write(dir.path().join(".secret"), "x").expect("write");
        std::fs::write(dir.path().join("empty.txt"), "").expect("write");
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join(".git/config"), "x").expect("write");

        let sources = host().enumerate(dir.path()).expect("enumerates");
        let names: Vec<_> = sources
            .iter()
            .filter_map(|s| s.envelope.hint.as_deref())
            .collect();
        assert_eq!(names, vec!["note.md"]);
        assert_eq!(
            sources[0].envelope.content_type.essence(),
            "text/markdown"
        );
    }

    #[test]
    fn slice_lines_is_one_based_inclusive_and_clamps() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(slice_lines(text, 2, 3).expect("slices"), "b\nc");
        assert_eq!(slice_lines(text, 3, 99).expect("clamps"), "c\nd");
        assert!(matches!(
            slice_lines(text, 9, 12),
            Err(FsHostError::RangeBeyondEnd { .. })
        ));
        assert!(matches!(
            slice_lines(text, 0, 2),
            Err(FsHostError::RangeBeyondEnd { .. })
        ));
    }

    #[test]
    fn detects_code_files_as_text() {
        assert_eq!(
            detect_mimetype(Path::new("main.zsh")).essence(),
            "text/plain"
        );
        assert!(is_texty(&detect_mimetype(Path::new("config.json"))));
        assert!(!is_texty(&detect_mimetype(Path::new("photo.jpeg"))));
    }
}
