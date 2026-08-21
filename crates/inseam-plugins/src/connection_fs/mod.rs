//! The filesystem connection plugin: registers this machine's local
//! filesystem host into the `connections` seam. Enumerates sources
//! read-only, extracts envelopes, and serves fetches and scans.
//! Service-specific connections (Gmail, Slack, ...) register the same way
//! on the same seam, linked or loaded.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{parse_config, ApplyCx, Inject, Manifest, Plugin, PluginError};
use inseam_seams::connection::{
    register_as_effect, Capabilities, Connection, EnumeratedSource, HostDescription, HostKind,
    Registration,
};
use inseam_seams::text::slice_lines;
use inseam_seams::SeamError;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsConnectionConfig {
    /// Override the derived `fs-<hostname>` host id (tests, containers).
    pub host_id: Option<String>,
    /// Skip dot-named files and directories (the root the caller names is
    /// never skipped).
    pub skip_hidden: bool,
    /// Honor `.gitignore` files and `.git/info/exclude` found in the tree,
    /// whether or not the tree is a git repository. What git would not track
    /// — build output, dependencies, `.env` files — is rarely what an index
    /// should hold, so this is on by default.
    pub gitignore: bool,
    /// Patterns in gitignore syntax, anchored at the filesystem root: a
    /// pattern without a slash matches a name at any depth (`node_modules/`),
    /// one with a leading slash names an absolute path (`/Users/greg/Library/`),
    /// and `!` re-includes. Host-native and prunes the walk — a matched
    /// directory is never descended into.
    pub ignore: Vec<String>,
}

impl Default for FsConnectionConfig {
    fn default() -> Self {
        Self {
            host_id: None,
            skip_hidden: true,
            gitignore: true,
            ignore: Vec::new(),
        }
    }
}

/// Per-directory ignore file honored regardless of `gitignore`: the way to
/// keep a subtree out of inseam without touching git's view of it.
pub const INSEAM_IGNORE_FILENAME: &str = ".inseamignore";

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
        static INJECT: &[Inject] = &[Inject::required("connections")];
        Manifest {
            name: "connection-fs",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let id = match &self.config.host_id {
            Some(id) => HostId::new(id.clone()).map_err(|e| PluginError(e.to_string()))?,
            None => FsHost::local_id(),
        };
        let walk = WalkConfig::compile(&self.config).map_err(|e| PluginError(e.to_string()))?;
        let host = FsHost::new(id, walk);
        let registration = Registration {
            entry_id: cx.entry_id().to_string(),
            host: HostDescription {
                id: host.id().clone(),
                kind: HostKind::filesystem(),
                display_name: gethostname::gethostname().to_string_lossy().into_owned(),
            },
            // A plain filesystem walk: no FSEvents watcher yet, and never a
            // write path.
            capabilities: Capabilities::READ_ONLY,
            connection: Arc::new(host) as Arc<dyn Connection>,
        };
        register_as_effect(cx, registration)?;
        Ok(())
    }
}

/// The enumeration walk's ignore rules, compiled once from the entry config:
/// which ignore files to honor and the configured patterns as a gitignore
/// rooted at `/`, so a pattern reads like the absolute path it names.
#[derive(Debug, Clone)]
pub struct WalkConfig {
    skip_hidden: bool,
    gitignore: bool,
    patterns: Gitignore,
}

impl WalkConfig {
    pub fn compile(config: &FsConnectionConfig) -> Result<Self, SeamError> {
        let mut builder = GitignoreBuilder::new("/");
        for pattern in &config.ignore {
            // gitignore files skip blank and comment lines silently; in a
            // config list those are mistakes, and saying so beats ignoring
            // nothing by accident.
            let trimmed = pattern.trim();
            if trimmed.is_empty() {
                return Err(SeamError::failed("ignore pattern is empty"));
            }
            if trimmed.starts_with('#') {
                return Err(SeamError::failed(format!(
                    "ignore pattern `{pattern}` is a comment; patterns may not start with `#`"
                )));
            }
            builder
                .add_line(None, pattern)
                .map_err(|e| SeamError::failed(format!("ignore pattern `{pattern}`: {e}")))?;
        }
        let patterns = builder
            .build()
            .map_err(|e| SeamError::failed(format!("ignore patterns: {e}")))?;
        assert_eq!(
            patterns.num_ignores() + patterns.num_whitelists(),
            u64::try_from(config.ignore.len()).unwrap_or(u64::MAX),
            "every configured pattern compiled"
        );
        Ok(Self {
            skip_hidden: config.skip_hidden,
            gitignore: config.gitignore,
            patterns,
        })
    }

    /// Today's default walk: hidden skipped, gitignore honored, no patterns.
    pub fn standard() -> Self {
        Self::compile(&FsConnectionConfig::default()).expect("the default config compiles")
    }

    /// Whether the configured patterns ignore `path`; a matched directory
    /// prunes its whole subtree.
    fn ignores(&self, path: &Path, is_dir: bool) -> bool {
        self.patterns.matched(path, is_dir).is_ignore()
    }
}

/// The connection to the machine's filesystem host. Locators are absolute
/// paths with the leading `/` stripped, so any file the node can read is
/// addressable and `inseam://<host>/<path>` round-trips.
#[derive(Debug, Clone)]
pub struct FsHost {
    id: HostId,
    walk: WalkConfig,
}

impl FsHost {
    pub fn new(id: HostId, walk: WalkConfig) -> Self {
        Self { id, walk }
    }

    /// The host id for this machine: `fs-<hostname>`, sanitized.
    pub fn local_id() -> HostId {
        let raw = gethostname::gethostname().to_string_lossy().to_lowercase();
        let mut cleaned: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        cleaned.truncate(48);
        let cleaned = cleaned.trim_matches('-');
        let id = if cleaned.is_empty() { "local" } else { cleaned };
        HostId::new(format!("fs-{id}")).expect("sanitized host id is valid")
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

    /// The directory a sweep scope names: an absolute path as given, or a
    /// locator (what an `inseam://<host>/<root>` address carries) rooted at
    /// `/` — so `inseam index inseam://fs-mba/Users/greg/Notes` and
    /// `inseam index /Users/greg/Notes` are the same scope.
    fn scope_path(root: &str) -> PathBuf {
        if root.starts_with('/') {
            PathBuf::from(root)
        } else {
            Path::new("/").join(root)
        }
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

    /// The enumeration walk itself, synchronous: `enumerate` runs it on the
    /// blocking pool.
    fn walk_sources(&self, dir: &Path) -> Result<Vec<EnumeratedSource>, SeamError> {
        let observed = Timestamp::from(SystemTime::now());
        let mut sources = Vec::new();
        let patterns = self.walk.clone();
        let walker = WalkBuilder::new(dir)
            // Every filter stated explicitly: the crate's defaults are
            // tuned for ripgrep, not for us.
            .standard_filters(false)
            .hidden(self.walk.skip_hidden)
            .git_ignore(self.walk.gitignore)
            .git_exclude(self.walk.gitignore)
            .git_global(false)
            .ignore(false)
            .require_git(false)
            .parents(true)
            .add_custom_ignore_filename(INSEAM_IGNORE_FILENAME)
            .follow_links(false)
            .filter_entry(move |e| {
                // depth 0 is the root the caller named: never prune it, even
                // when a pattern would.
                let is_dir = e.file_type().is_some_and(|t| t.is_dir());
                e.depth() == 0 || !patterns.ignores(e.path(), is_dir)
            })
            .build();
        for entry in walker {
            let entry = entry.map_err(|e| SeamError::failed(format!("walk failed: {e}")))?;
            let is_file = entry.file_type().is_some_and(|t| t.is_file());
            if !is_file {
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
                // Enumeration is metadata-only — no content is read here, so
                // the digest is filled in when indexing first reads the bytes.
                content_digest: None,
            };
            sources.push(EnumeratedSource {
                address,
                envelope,
                raw_bytes: meta.len(),
            });
        }
        Ok(sources)
    }
}

#[async_trait::async_trait]
impl Connection for FsHost {
    /// Walk a directory and emit every enumerable source: regular, non-empty
    /// files the walk's ignore rules admit — hidden names, `.gitignore` and
    /// `.inseamignore` files in the tree, and the configured patterns all
    /// prune here, so an ignored file never becomes an address. Read-only;
    /// symlinks are not followed.
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let dir = Self::scope_path(root)
            .canonicalize()
            .map_err(|e| SeamError::failed(format!("cannot enumerate {root}: {e}")))?;
        // The walk is synchronous disk work; it runs on the blocking pool so
        // it never stalls the runtime the sweep's pipeline lives on.
        let host = FsHost::new(self.id.clone(), self.walk.clone());
        tokio::task::spawn_blocking(move || host.walk_sources(&dir))
            .await
            .map_err(|e| SeamError::failed(format!("enumeration task failed: {e}")))?
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        let canonical = Self::scope_path(root).canonicalize().ok()?;
        Some(self.address_for(&canonical).ok()?.locator.as_str().to_string())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let path = self.resolve(address)?;
        read_text_at(&path).await
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        let path = self.resolve(address)?;
        let text = read_text_at(&path).await?;
        slice_lines(&text, start, end).map_err(SeamError::failed)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let path = self.resolve(address)?;
        read_bytes_at(&path).await
    }
}

/// Reads go through the runtime's blocking pool: many planners read at once
/// during a sweep, and a blocking read on a worker thread would stall the
/// others' transforms.
async fn read_bytes_at(path: &Path) -> Result<Vec<u8>, SeamError> {
    tokio::fs::read(path)
        .await
        .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))
}

async fn read_text_at(path: &Path) -> Result<String, SeamError> {
    let bytes = read_bytes_at(path).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
        FsHost::new(HostId::new("fs-test").expect("valid host id"), WalkConfig::standard())
    }

    fn host_with(config: FsConnectionConfig) -> FsHost {
        let walk = WalkConfig::compile(&config).expect("config compiles");
        FsHost::new(HostId::new("fs-test").expect("valid host id"), walk)
    }

    async fn names(host: &FsHost, dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = host
            .enumerate(dir.to_str().expect("utf8"))
            .await
            .expect("enumerates")
            .iter()
            .map(|s| {
                s.address
                    .locator
                    .as_str()
                    .strip_prefix(
                        dir.canonicalize()
                            .expect("canonical")
                            .to_str()
                            .expect("utf8")
                            .trim_start_matches('/'),
                    )
                    .expect("under root")
                    .trim_start_matches('/')
                    .to_string()
            })
            .collect();
        names.sort();
        names
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, body).expect("write");
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

    #[tokio::test]
    async fn gitignore_files_prune_the_walk_without_a_git_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), ".gitignore", "target/\n*.log\n");
        write(dir.path(), "src/main.rs", "fn main() {}\n");
        write(dir.path(), "target/debug/app", "binary\n");
        write(dir.path(), "build.log", "noise\n");
        write(dir.path(), "sub/.gitignore", "secret.md\n");
        write(dir.path(), "sub/secret.md", "# hush\n");
        write(dir.path(), "sub/open.md", "# hello\n");

        assert_eq!(
            names(&host(), dir.path()).await,
            vec!["src/main.rs", "sub/open.md"]
        );
        let config = FsConnectionConfig {
            gitignore: false,
            ..FsConnectionConfig::default()
        };
        assert_eq!(
            names(&host_with(config), dir.path()).await,
            vec!["build.log", "src/main.rs", "sub/open.md", "sub/secret.md", "target/debug/app"]
        );
    }

    #[tokio::test]
    async fn inseamignore_files_are_honored_even_with_gitignore_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), ".inseamignore", "drafts/\n");
        write(dir.path(), "drafts/wip.md", "# wip\n");
        write(dir.path(), "final.md", "# done\n");
        let config = FsConnectionConfig {
            gitignore: false,
            ..FsConnectionConfig::default()
        };
        assert_eq!(names(&host_with(config), dir.path()).await, vec!["final.md"]);
    }

    #[tokio::test]
    async fn configured_patterns_anchor_at_the_filesystem_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "app/node_modules/left-pad/index.js", "x\n");
        write(dir.path(), "app/src/index.js", "x\n");
        write(dir.path(), "Archive/old.md", "# old\n");
        write(dir.path(), "notes/Archive/keep.md", "# keep\n");
        let canonical = dir.path().canonicalize().expect("canonical");
        let config = FsConnectionConfig {
            ignore: vec![
                "node_modules/".into(),
                format!("{}/Archive/", canonical.display()),
            ],
            ..FsConnectionConfig::default()
        };
        assert_eq!(
            names(&host_with(config), dir.path()).await,
            vec!["app/src/index.js", "notes/Archive/keep.md"]
        );
    }

    #[tokio::test]
    async fn configured_patterns_support_reinclusion() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.log", "x\n");
        write(dir.path(), "important.log", "x\n");
        let config = FsConnectionConfig {
            ignore: vec!["*.log".into(), "!important.log".into()],
            ..FsConnectionConfig::default()
        };
        assert_eq!(names(&host_with(config), dir.path()).await, vec!["important.log"]);
    }

    #[tokio::test]
    async fn hidden_entries_are_indexed_when_skip_hidden_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), ".env", "SECRET=1\n");
        write(dir.path(), "a.md", "# a\n");
        let config = FsConnectionConfig {
            skip_hidden: false,
            ..FsConnectionConfig::default()
        };
        assert_eq!(names(&host_with(config), dir.path()).await, vec![".env", "a.md"]);
    }

    #[tokio::test]
    async fn a_named_root_is_never_pruned_by_its_own_patterns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hidden = dir.path().join(".workspace");
        write(&hidden, "a.md", "# a\n");
        let canonical = hidden.canonicalize().expect("canonical");
        let config = FsConnectionConfig {
            ignore: vec![format!("{}/", canonical.display())],
            ..FsConnectionConfig::default()
        };
        assert_eq!(names(&host_with(config), &hidden).await, vec!["a.md"]);
    }

    #[test]
    fn rejects_malformed_empty_and_comment_patterns() {
        for bad in ["docs/[z-a]", "", "   ", "# not a pattern"] {
            let config = FsConnectionConfig {
                ignore: vec![bad.into()],
                ..FsConnectionConfig::default()
            };
            assert!(WalkConfig::compile(&config).is_err(), "`{bad}` must be refused");
        }
    }

    #[test]
    fn detects_code_files_as_text() {
        assert_eq!(detect_mimetype(Path::new("main.zsh")).essence(), "text/plain");
        use inseam_seams::text::is_indexable_text;
        assert!(is_indexable_text(&detect_mimetype(Path::new("config.json"))));
        assert!(!is_indexable_text(&detect_mimetype(Path::new("photo.jpeg"))));
    }
}
