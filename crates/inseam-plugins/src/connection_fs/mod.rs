//! The filesystem connection plugin: registers this machine's local
//! filesystem host into the `connections` seam. Enumerates sources
//! read-only, extracts envelopes, and serves fetches and scans.
//! Service-specific connections (Gmail, Slack, ...) register the same way
//! on the same seam, linked or loaded.

pub mod machine;

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tokio::sync::Semaphore;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{ApplyCx, Inject, Manifest, Plugin, PluginError, parse_config};
use inseam_seams::SeamError;
use inseam_seams::connection::{
    Capabilities, Connection, EnumeratedSource, HostDescription, HostKind, Registration,
    derive_host_id, register_as_effect,
};
use inseam_seams::text::{slice_lines, slice_lines_from_reader};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsConnectionConfig {
    /// The identity material this host's id is derived from, in place of
    /// the machine's own id ([`machine`]): a stable tenant name for a
    /// container, a fixture name for a test. Never the id itself — ids are
    /// derived, never configured (`design/addressing.md`).
    pub machine_id: Option<String>,
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
    /// The folders this host indexes, as absolute paths. Empty means the
    /// owner names a scope per run and any directory is one (the CLI's
    /// `inseam index <dir>`); once folders are configured, every scope on
    /// this host must lie inside one of them, and owner surfaces offer
    /// them as the roots to index. Absent from older overlays, hence the
    /// serde default.
    #[serde(default)]
    pub roots: Vec<String>,
}

impl Default for FsConnectionConfig {
    fn default() -> Self {
        Self {
            machine_id: None,
            skip_hidden: true,
            gitignore: true,
            ignore: Vec::new(),
            roots: Vec::new(),
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
        let keep_dir = cx.data_dir().join(cx.entry_id());
        let identity = machine::resolve(
            self.config.machine_id.as_deref(),
            machine::platform_machine_id(),
            &keep_dir,
        )
        .map_err(|e| PluginError(e.to_string()))?;
        let id = FsHost::derive_id(identity.principal());
        tracing::info!(host = %id, source = ?identity.source(), "filesystem host identity");
        let walk = WalkConfig::compile(&self.config).map_err(|e| PluginError(e.to_string()))?;
        let roots = configured_roots(&self.config.roots).map_err(|e| PluginError(e.to_string()))?;
        let host = FsHost::new(id, walk).with_roots(roots);
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
            roots: self.config.roots.clone(),
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

/// Content reads in flight at once, across every planner sharing this host.
/// The sweep parks thousands of planners on batch-lane calls; when the job
/// lands they all resume and read together, and every open file is a
/// descriptor a default 256-descriptor macOS shell does not have. Sixty-four
/// keeps the disk busy without touching the descriptor table, and readers
/// past the gate wait rather than fail.
const READS_IN_FLIGHT_MAX: usize = 64;
const _: () = assert!(READS_IN_FLIGHT_MAX >= 1);
const _: () = assert!(
    READS_IN_FLIGHT_MAX <= 256,
    "the gate must fit a default descriptor table"
);

/// The connection to the machine's filesystem host. Locators are absolute
/// paths with the leading `/` stripped, so any file the node can read is
/// addressable and `inseam://<host>/<path>` round-trips.
#[derive(Debug, Clone)]
pub struct FsHost {
    id: HostId,
    walk: WalkConfig,
    /// The configured folders, absolute; empty admits any scope.
    roots: Vec<PathBuf>,
    /// Shared by every clone of this host, so the bound is per node rather
    /// than per handle.
    reads: Arc<Semaphore>,
}

/// Most folders one host may be configured with.
pub const ROOTS_MAX: usize = 64;

/// The configured folders as paths: each absolute, no more than
/// [`ROOTS_MAX`], none listed twice. Existence is not required here — a
/// folder on an unmounted volume is still the owner's configuration — and
/// is reported when the folder is indexed.
pub fn configured_roots(roots: &[String]) -> Result<Vec<PathBuf>, SeamError> {
    if roots.len() > ROOTS_MAX {
        return Err(SeamError::failed(format!(
            "at most {ROOTS_MAX} folders may be configured; {} given",
            roots.len()
        )));
    }
    let mut paths: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let path = Path::new(root.trim());
        if !path.is_absolute() {
            return Err(SeamError::failed(format!(
                "folder `{root}` must be an absolute path"
            )));
        }
        if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(SeamError::failed(format!(
                "folder `{root}` must not contain `..`"
            )));
        }
        if paths.iter().any(|known| known == path) {
            return Err(SeamError::failed(format!(
                "folder `{root}` is listed twice"
            )));
        }
        paths.push(path.to_path_buf());
    }
    Ok(paths)
}

impl FsHost {
    pub fn new(id: HostId, walk: WalkConfig) -> Self {
        Self {
            id,
            walk,
            roots: Vec::new(),
            reads: Arc::new(Semaphore::new(READS_IN_FLIGHT_MAX)),
        }
    }

    pub fn with_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        self
    }

    /// The canonical directory a scope names, refused when folders are
    /// configured and the scope lies outside every one of them. Roots are
    /// canonicalized here rather than at mount so a folder that appears
    /// later (a volume mounted after boot) still admits its scopes.
    fn admitted_scope(&self, root: &str) -> Result<PathBuf, SeamError> {
        let dir = Self::scope_path(root)
            .canonicalize()
            .map_err(|e| SeamError::failed(format!("cannot enumerate {root}: {e}")))?;
        if self.roots.is_empty() {
            return Ok(dir);
        }
        let inside = self
            .roots
            .iter()
            .filter_map(|configured| configured.canonicalize().ok())
            .any(|configured| dir.starts_with(&configured));
        if inside {
            Ok(dir)
        } else {
            let configured: Vec<String> =
                self.roots.iter().map(|p| p.display().to_string()).collect();
            Err(SeamError::Refused(format!(
                "{root} is outside the folders configured for host `{}` ({}); add it to the fs entry's `roots` or index inside one of them",
                self.id,
                configured.join(", ")
            )))
        }
    }

    /// Reads go through the runtime's blocking pool: many planners read at
    /// once during a sweep, and a blocking read on a worker thread would
    /// stall the others' transforms. The gate bounds how many are open at
    /// once (`READS_IN_FLIGHT_MAX`).
    async fn read_bytes_at(&self, path: &Path) -> Result<Vec<u8>, SeamError> {
        // The semaphore is never closed, so acquisition only fails if the
        // runtime is torn down under us; that is an operating error here.
        let _permit = self
            .reads
            .acquire()
            .await
            .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
        assert!(self.reads.available_permits() < READS_IN_FLIGHT_MAX);
        tokio::fs::read(path)
            .await
            .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))
    }

    async fn read_text_at(&self, path: &Path) -> Result<String, SeamError> {
        if path.is_dir() {
            return self.read_directory_text_at(path).await;
        }
        let bytes = self.read_bytes_at(path).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A folder's text is its name listing ([`directory_text`]): the fetch
    /// of a folder address. Under the same gate as a file read.
    async fn read_directory_text_at(&self, path: &Path) -> Result<String, SeamError> {
        let _permit = self
            .reads
            .acquire()
            .await
            .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
        assert!(self.reads.available_permits() < READS_IN_FLIGHT_MAX);
        let path = path.to_path_buf();
        let skip_hidden = self.walk.skip_hidden;
        tokio::task::spawn_blocking(move || directory_text(&path, skip_hidden))
            .await
            .map_err(|e| SeamError::failed(format!("directory read task failed: {e}")))?
    }

    /// Lines `start..=end` of a file, reading no further than line `end`:
    /// a scan near the top of a large file costs what it reads, not the
    /// file. The read holds a permit like every other, and runs on the
    /// blocking pool because it is buffered synchronous disk work.
    async fn read_lines_at(&self, path: &Path, start: u64, end: u64) -> Result<String, SeamError> {
        if path.is_dir() {
            let text = self.read_directory_text_at(path).await?;
            return slice_lines(&text, start, end);
        }
        let _permit = self
            .reads
            .acquire()
            .await
            .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
        assert!(self.reads.available_permits() < READS_IN_FLIGHT_MAX);
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(&path)
                .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
            slice_lines_from_reader(std::io::BufReader::new(file), start, end)
        })
        .await
        .map_err(|e| SeamError::failed(format!("line read task failed: {e}")))?
    }

    /// The host id for the filesystem of the machine identified by
    /// `principal`: the kind-separated fingerprint every connection mints
    /// (`design/addressing.md`), so the id survives a rename of the machine
    /// and two nodes on it agree without coordinating.
    pub fn derive_id(principal: &str) -> HostId {
        derive_host_id(&HostKind::filesystem(), principal)
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
    /// `/` — so `inseam index inseam://fs-3f9a1b2c4d5e6f70/Users/greg/Notes` and
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
        let mut folders: BTreeSet<PathBuf> = BTreeSet::new();
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
                facets: Vec::new(),
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
            collect_folders_above(entry.path(), dir, &mut folders);
        }
        // Folders are sources too (`design/indexing.md`): every directory
        // between an enumerated file and the scope root, the root included,
        // so a folder exists in the index exactly when something under it
        // does — an empty or wholly ignored directory is no source. They
        // follow the files, in path order, so a run lands them last.
        for folder in &folders {
            sources.push(self.folder_source(folder, observed)?);
        }
        Ok(sources)
    }

    /// A directory as a source: no bytes of its own (the sweep composes its
    /// content from its children), its name as the hint, its own timestamps.
    fn folder_source(
        &self,
        path: &Path,
        observed: Timestamp,
    ) -> Result<EnumeratedSource, SeamError> {
        let meta = std::fs::metadata(path)
            .map_err(|e| SeamError::failed(format!("metadata of {}: {e}", path.display())))?;
        assert!(meta.is_dir());
        Ok(EnumeratedSource {
            address: self.address_for(path)?,
            envelope: Envelope {
                source_type: "directory".to_string(),
                content_type: Mimetype::directory(),
                length: ContentLength::Bytes(0),
                created: meta.created().ok().map(Timestamp::from),
                modified: meta.modified().ok().map(Timestamp::from),
                observed,
                properties: Vec::new(),
                facets: Vec::new(),
                hint: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string),
                content_digest: None,
            },
            raw_bytes: 0,
        })
    }
}

/// Paths a directory may nest below the scope root before the walk is
/// considered broken: a bound on the ancestor climb, not a limit users meet.
const FOLDER_DEPTH_MAX: u32 = 4_096;

/// Add every directory from `file`'s parent up to `root` (inclusive) to
/// `folders`, stopping early at one already recorded — its ancestors are
/// recorded too, since they were added on the same climb. The filesystem
/// root `/` has no locator and is never a folder source.
fn collect_folders_above(file: &Path, root: &Path, folders: &mut BTreeSet<PathBuf>) {
    let mut ancestor = file.parent();
    let mut climbed: u32 = 0;
    while let Some(dir) = ancestor {
        climbed += 1;
        assert!(
            climbed <= FOLDER_DEPTH_MAX,
            "the ancestor climb is bounded by the path depth"
        );
        if !dir.starts_with(root) || dir.parent().is_none() {
            break;
        }
        if !folders.insert(dir.to_path_buf()) {
            break;
        }
        ancestor = dir.parent();
    }
}

/// A directory's text, as `fetch` serves it: its entries' names, one per
/// line in name order, directories marked with a trailing `/`. Hidden
/// entries follow the walk's `skip_hidden` rule so the listing matches
/// what enumeration would admit at this level. Bounded by the directory's
/// entry count.
fn directory_text(path: &Path, skip_hidden: bool) -> Result<String, SeamError> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| SeamError::failed(format!("read {}: {e}", path.display())))?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if skip_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        names.push(if is_dir { format!("{name}/") } else { name });
    }
    names.sort();
    let mut text = names.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    Ok(text)
}

#[async_trait::async_trait]
impl Connection for FsHost {
    /// Walk a directory and emit every enumerable source: regular, non-empty
    /// files the walk's ignore rules admit — hidden names, `.gitignore` and
    /// `.inseamignore` files in the tree, and the configured patterns all
    /// prune here, so an ignored file never becomes an address. Read-only;
    /// symlinks are not followed.
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let dir = self.admitted_scope(root)?;
        // The walk is synchronous disk work; it runs on the blocking pool so
        // it never stalls the runtime the sweep's pipeline lives on.
        let host = self.clone();
        tokio::task::spawn_blocking(move || host.walk_sources(&dir))
            .await
            .map_err(|e| SeamError::failed(format!("enumeration task failed: {e}")))?
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        let canonical = Self::scope_path(root).canonicalize().ok()?;
        Some(
            self.address_for(&canonical)
                .ok()?
                .locator
                .as_str()
                .to_string(),
        )
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let path = self.resolve(address)?;
        self.read_text_at(&path).await
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        let path = self.resolve(address)?;
        self.read_lines_at(&path, start, end).await
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let path = self.resolve(address)?;
        self.read_bytes_at(&path).await
    }
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
        FsHost::new(
            HostId::new("fs-test").expect("valid host id"),
            WalkConfig::standard(),
        )
    }

    fn host_with(config: FsConnectionConfig) -> FsHost {
        let walk = WalkConfig::compile(&config).expect("config compiles");
        FsHost::new(HostId::new("fs-test").expect("valid host id"), walk)
    }

    /// The files enumerated under `dir`, relative to it. Folders are
    /// sources too, but the ignore tests are about which files survive;
    /// the folder test covers folders.
    async fn names(host: &FsHost, dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = host
            .enumerate(dir.to_str().expect("utf8"))
            .await
            .expect("enumerates")
            .iter()
            .filter(|s| s.envelope.source_type == "file")
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
        assert_eq!(
            a.to_string(),
            "inseam://fs-test/Users/greg/Data/notes/reno.md"
        );
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
        let a: Address = "inseam://fs-test/tmp/../etc/passwd"
            .parse()
            .expect("parses");
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
            .filter(|s| s.envelope.source_type == "file")
            .filter_map(|s| s.envelope.hint.as_deref())
            .collect();
        assert_eq!(names, vec!["note.md"]);
        assert_eq!(sources[0].envelope.content_type.essence(), "text/markdown");
    }

    #[tokio::test]
    async fn enumerate_lists_the_folders_above_every_file_after_the_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/deep")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("empty")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("ignored")).expect("mkdir");
        std::fs::write(dir.path().join("a/deep/note.md"), "# hi\n").expect("write");
        std::fs::write(dir.path().join("top.txt"), "top\n").expect("write");
        std::fs::write(dir.path().join("ignored/x.txt"), "x\n").expect("write");
        std::fs::write(dir.path().join(".inseamignore"), "ignored/\n").expect("write");

        let sources = host()
            .enumerate(dir.path().to_str().expect("utf8"))
            .await
            .expect("enumerates");
        let root = dir.path().canonicalize().expect("canonical");
        let root_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf8")
            .to_string();
        let listed: Vec<(String, &str)> = sources
            .iter()
            .map(|s| {
                (
                    s.envelope.hint.clone().expect("hint"),
                    s.envelope.source_type.as_str(),
                )
            })
            .collect();
        let files = listed.iter().filter(|(_, t)| *t == "file").count();
        assert_eq!(files, 2);
        let folders: Vec<&str> = listed
            .iter()
            .skip(files)
            .map(|(name, kind)| {
                assert_eq!(*kind, "directory");
                name.as_str()
            })
            .collect();
        // The scope root, `a`, and `a/deep`; never the empty directory nor
        // the ignored one, and folders come after every file.
        assert_eq!(folders, vec![root_name.as_str(), "a", "deep"]);
        let folder = sources
            .iter()
            .find(|s| s.envelope.hint.as_deref() == Some("deep"))
            .expect("deep");
        assert!(folder.envelope.content_type.is_directory());
        assert_eq!(folder.raw_bytes, 0);
        assert!(folder.address.locator.as_str().ends_with("/a/deep"));
    }

    #[tokio::test]
    async fn a_folder_reads_as_its_name_listing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("b.md"), "b\n").expect("write");
        std::fs::write(dir.path().join("a.md"), "a\n").expect("write");
        std::fs::write(dir.path().join(".hidden"), "h\n").expect("write");
        let h = host();
        let address = h
            .address_for(&dir.path().canonicalize().expect("canonical"))
            .expect("addr");
        let text = h.read_text(&address).await.expect("reads");
        assert_eq!(text, "a.md\nb.md\nsub/\n");
        assert_eq!(
            h.read_lines(&address, 2, 3).await.expect("reads"),
            "b.md\nsub/"
        );
        assert!(
            h.read_bytes(&address).await.is_err(),
            "a folder has no bytes"
        );
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
            vec![
                "build.log",
                "src/main.rs",
                "sub/open.md",
                "sub/secret.md",
                "target/debug/app"
            ]
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
        assert_eq!(
            names(&host_with(config), dir.path()).await,
            vec!["final.md"]
        );
    }

    #[tokio::test]
    async fn configured_folders_bound_every_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "notes/a.md", "# a\n");
        write(dir.path(), "elsewhere/b.md", "# b\n");
        let notes = dir.path().join("notes");
        let bounded = host().with_roots(vec![notes.clone()]);
        assert_eq!(names(&bounded, &notes).await, vec!["a.md"]);
        let outside = bounded
            .enumerate(dir.path().join("elsewhere").to_str().expect("utf8"))
            .await;
        assert!(matches!(outside, Err(SeamError::Refused(_))), "{outside:?}");
        // The whole tree is not inside `notes` either.
        assert!(matches!(
            bounded.enumerate(dir.path().to_str().expect("utf8")).await,
            Err(SeamError::Refused(_))
        ));
        // No folders configured: any scope is in play, as before.
        assert_eq!(names(&host(), &notes).await, vec!["a.md"]);
    }

    #[test]
    fn configured_folders_must_be_absolute_and_distinct() {
        assert!(configured_roots(&["relative/notes".to_string()]).is_err());
        assert!(configured_roots(&["/a/../b".to_string()]).is_err());
        assert!(configured_roots(&["/a".to_string(), "/a".to_string()]).is_err());
        assert_eq!(configured_roots(&["/a".to_string()]).unwrap().len(), 1);
        let many: Vec<String> = (0..=ROOTS_MAX).map(|i| format!("/r{i}")).collect();
        assert!(configured_roots(&many).is_err());
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
    async fn thousands_of_concurrent_reads_all_land() {
        // The sweep resumes every parked planner at once when a batch job
        // lands; the host must serve them without opening thousands of
        // files together. Every clone shares the gate.
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..64 {
            write(
                dir.path(),
                &format!("doc-{index}.txt"),
                &format!("body {index}\n"),
            );
        }
        let host = host();
        let sources = host
            .enumerate(dir.path().to_str().expect("utf8"))
            .await
            .expect("enumerate");
        assert_eq!(sources.len(), 65, "64 files and the folder holding them");
        let files: Vec<&EnumeratedSource> = sources
            .iter()
            .filter(|s| s.envelope.source_type == "file")
            .collect();
        assert_eq!(files.len(), 64);
        let reads_total: usize = 2_048;
        let mut tasks = Vec::with_capacity(reads_total);
        for index in 0..reads_total {
            let host = host.clone();
            let address = files[index % files.len()].address.clone();
            tasks.push(tokio::spawn(async move { host.read_bytes(&address).await }));
        }
        let mut landed: usize = 0;
        for task in tasks {
            let bytes = task.await.expect("task").expect("read");
            assert!(bytes.starts_with(b"body "));
            landed += 1;
        }
        assert_eq!(landed, reads_total);
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
        assert_eq!(
            names(&host_with(config), dir.path()).await,
            vec!["important.log"]
        );
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
        assert_eq!(
            names(&host_with(config), dir.path()).await,
            vec![".env", "a.md"]
        );
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
            assert!(
                WalkConfig::compile(&config).is_err(),
                "`{bad}` must be refused"
            );
        }
    }

    #[test]
    fn detects_code_files_as_text() {
        assert_eq!(
            detect_mimetype(Path::new("main.zsh")).essence(),
            "text/plain"
        );
        use inseam_seams::text::is_indexable_text;
        assert!(is_indexable_text(&detect_mimetype(Path::new(
            "config.json"
        ))));
        assert!(!is_indexable_text(&detect_mimetype(Path::new(
            "photo.jpeg"
        ))));
    }
}
