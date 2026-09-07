//! GitHub repository connection: one repository as a host of kind `github`
//! (`design/connections.md`), stewarded through the node's guarded `fetch`
//! under the manifest's host allow list. Enumeration is one tree call
//! (`git/trees/<ref>?recursive=1`), every blob under the named scope
//! becoming a source whose locator is its path; reads come from the raw
//! content host; `describe` asks the contents API for one path. Private
//! repositories work when the entry names an OAuth grant and the config
//! sets `authorize = true` — the node attaches the bearer, this component
//! never sees it. Degrades to an error naming what failed whenever the
//! network refuses or the host answers something unusable; never panics.

wit_bindgen::generate!({
    path: "../../crates/inseam-wasm-host/wit",
    world: "connection-plugin",
});

use std::cell::RefCell;

use exports::inseam::plugin::connection::{
    ContentLength, EdgeCapabilities, Envelope, Guest, HostDescription, Source,
};
use inseam::plugin::fetch::{self, Request};
use inseam::plugin::host;
use serde::Deserialize;

/// Blobs one enumeration will keep, whatever the repository holds; the
/// sweep's own budget bounds what is indexed, this bounds what is listed.
const FILES_MAX_DEFAULT: u32 = 5_000;
/// Blobs larger than this are listed but not read: a source the node
/// cannot fetch is still a source it knows about.
const FILE_BYTES_MAX_DEFAULT: u64 = 2_000_000;
/// Longest repository name accepted (`owner/name`).
const REPOSITORY_CHARS_MAX: usize = 200;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// `owner/name`, as GitHub spells it.
    repository: String,
    /// Branch, tag, or commit to read; `HEAD` for the default branch.
    #[serde(default = "default_ref")]
    r#ref: String,
    #[serde(default = "default_api_base")]
    api_base: String,
    #[serde(default = "default_raw_base")]
    raw_base: String,
    /// Send the entry's OAuth bearer with every request (private
    /// repositories, higher rate limits).
    #[serde(default)]
    authorize: bool,
    #[serde(default = "default_files_max")]
    files_max: u32,
    #[serde(default = "default_file_bytes_max")]
    file_bytes_max: u64,
}

fn default_ref() -> String {
    "HEAD".to_string()
}
fn default_api_base() -> String {
    "https://api.github.com".to_string()
}
fn default_raw_base() -> String {
    "https://raw.githubusercontent.com".to_string()
}
fn default_files_max() -> u32 {
    FILES_MAX_DEFAULT
}
fn default_file_bytes_max() -> u64 {
    FILE_BYTES_MAX_DEFAULT
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        let repository = self.repository.trim();
        let well_formed = repository
            .split_once('/')
            .is_some_and(|(owner, name)| {
                !owner.is_empty()
                    && !name.is_empty()
                    && !name.contains('/')
                    && repository
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
            });
        if !well_formed {
            return Err(format!("`repository` must be `owner/name`, got {:?}", self.repository));
        }
        if repository.len() > REPOSITORY_CHARS_MAX {
            return Err("`repository` is too long".to_string());
        }
        if self.r#ref.trim().is_empty() || self.r#ref.contains(['/', '?', '#', ' ']) {
            return Err(format!("`ref` must be a branch, tag, or commit name, got {:?}", self.r#ref));
        }
        for (field, value) in [("api_base", &self.api_base), ("raw_base", &self.raw_base)] {
            if !(value.starts_with("https://") || value.starts_with("http://")) {
                return Err(format!("`{field}` must be an http(s) URL, got {value:?}"));
            }
        }
        if self.files_max == 0 {
            return Err("`files_max` must be at least 1".to_string());
        }
        Ok(())
    }

    fn repository(&self) -> String {
        self.repository.trim().to_string()
    }
}

thread_local! {
    /// The configuration `configure` received; the bridge keeps this
    /// instance alive for the life of the entry.
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
}

fn configured() -> Result<Config, String> {
    CONFIG.with(|c| c.borrow().clone()).ok_or_else(|| "not configured".to_string())
}

// ---------------------------------------------------------------------------
// GitHub's answers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Tree {
    #[serde(default)]
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Deserialize)]
struct Contents {
    name: String,
    #[serde(default)]
    size: u64,
}

/// One request through the node, with GitHub's API headers, answered only
/// when the status is a success; anything else is an error naming the
/// URL and status.
fn get(config: &Config, url: &str, accept: &str) -> Result<Vec<u8>, String> {
    let response = fetch::fetch(&Request {
        method: "GET".to_string(),
        url: url.to_string(),
        headers: vec![
            ("Accept".to_string(), accept.to_string()),
            ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
        ],
        body: None,
        authorize: config.authorize,
    })?;
    if !(200..300).contains(&response.status) {
        return Err(format!("GET {url}: status {}", response.status));
    }
    Ok(response.body)
}

fn tree_url(config: &Config) -> String {
    format!(
        "{}/repos/{}/git/trees/{}?recursive=1",
        config.api_base.trim_end_matches('/'),
        config.repository(),
        config.r#ref
    )
}

fn raw_url(config: &Config, path: &str) -> String {
    format!(
        "{}/{}/{}/{}",
        config.raw_base.trim_end_matches('/'),
        config.repository(),
        config.r#ref,
        path
    )
}

fn contents_url(config: &Config, path: &str) -> String {
    format!(
        "{}/repos/{}/contents/{}?ref={}",
        config.api_base.trim_end_matches('/'),
        config.repository(),
        path,
        config.r#ref
    )
}

/// The scope a root names: a directory prefix with one trailing slash, or
/// empty for the whole repository.
fn scope_prefix(root: &str) -> String {
    let trimmed = root.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// A locator that could be a path in a repository: no traversal, no
/// control characters, no leading slash.
fn well_formed_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && !path.chars().any(|c| c.is_control() || c == '?' || c == '#')
}

fn envelope_for(path: &str, size: u64) -> Envelope {
    let name = path.rsplit('/').next().unwrap_or(path);
    Envelope {
        source_type: "file".to_string(),
        content_type: mimetype_for(name).to_string(),
        length: ContentLength::Bytes(size),
        created: None,
        modified: None,
        hint: Some(name.to_string()),
    }
}

/// A content type from the file name's extension; the node's own
/// detection is not available across the boundary, and the extension is
/// what the sweep needs to decide whether to read text.
fn mimetype_for(name: &str) -> &'static str {
    let extension = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match extension.as_str() {
        "md" | "markdown" => "text/markdown",
        "txt" | "text" | "license" => "text/plain",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "jsx" => "text/jsx",
        "go" => "text/x-go",
        "java" => "text/x-java",
        "rb" => "text/x-ruby",
        "c" | "h" => "text/x-c",
        "cpp" | "cc" | "hpp" => "text/x-c++",
        "swift" => "text/x-swift",
        "sh" | "bash" | "zsh" => "text/x-shellscript",
        "json" => "application/json",
        "toml" => "application/toml",
        "yaml" | "yml" => "application/yaml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

struct Plugin;

impl Guest for Plugin {
    fn configure(config: String) -> Result<(), String> {
        let parsed: Config = toml::from_str(&config).map_err(|e| format!("plugin config: {e}"))?;
        parsed.validate()?;
        CONFIG.with(|c| *c.borrow_mut() = Some(parsed));
        Ok(())
    }

    fn describe_host() -> Result<HostDescription, String> {
        let config = configured()?;
        let repository = config.repository();
        Ok(HostDescription {
            kind: "github".to_string(),
            principal: repository.to_ascii_lowercase(),
            display_name: format!("GitHub · {repository}"),
        })
    }

    fn capabilities() -> EdgeCapabilities {
        EdgeCapabilities {
            enumerates: true,
            change_feed: false,
            writable: false,
        }
    }

    fn enumerate(root: String) -> Result<Vec<Source>, String> {
        let config = configured()?;
        let prefix = scope_prefix(&root);
        let body = get(&config, &tree_url(&config), "application/vnd.github+json")?;
        let tree: Tree = serde_json::from_slice(&body).map_err(|e| format!("tree: {e}"))?;
        if tree.truncated {
            host::log("github: the tree was truncated by GitHub; some files are not listed");
        }
        let files_max = usize::try_from(config.files_max).unwrap_or(usize::MAX);
        let sources: Vec<Source> = tree
            .tree
            .iter()
            .filter(|entry| entry.kind == "blob")
            .filter(|entry| entry.path.starts_with(&prefix))
            .filter(|entry| well_formed_path(&entry.path))
            .take(files_max)
            .map(|entry| Source {
                locator: entry.path.clone(),
                envelope: envelope_for(&entry.path, entry.size),
                raw_bytes: entry.size,
            })
            .collect();
        host::log(&format!("github: {} source(s) under {:?}", sources.len(), prefix));
        Ok(sources)
    }

    fn locator_prefix(root: String) -> Option<String> {
        Some(scope_prefix(&root).trim_end_matches('/').to_string())
    }

    fn read_bytes(locator: String) -> Result<Vec<u8>, String> {
        let config = configured()?;
        if !well_formed_path(&locator) {
            return Err(format!("{locator:?} is not a repository path"));
        }
        let bytes = get(&config, &raw_url(&config, &locator), "application/octet-stream")?;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > config.file_bytes_max {
            return Err(format!(
                "{locator} is {size} bytes; this connection reads at most {}",
                config.file_bytes_max
            ));
        }
        Ok(bytes)
    }

    fn describe(locator: String) -> Result<Envelope, String> {
        let config = configured()?;
        if !well_formed_path(&locator) {
            return Err(format!("{locator:?} is not a repository path"));
        }
        let body = get(&config, &contents_url(&config, &locator), "application/vnd.github+json")?;
        let contents: Contents = serde_json::from_slice(&body).map_err(|e| format!("contents: {e}"))?;
        let mut envelope = envelope_for(&locator, contents.size);
        envelope.hint = Some(contents.name);
        Ok(envelope)
    }
}

export!(Plugin);
