//! `inseam plugin install`: fetch a loaded-tier plugin from a registry, verify
//! it, and mount it (`design/registry.md`).
//!
//! A registry is nothing but a directory tree — served over HTTPS (the
//! inseam repository's `plugins/` on GitHub is the default) or sitting on
//! local disk — containing `registry.toml` (the index: name, version,
//! artifact path, sha256), `advisories.toml` (yanked versions), and the
//! plugin directories. There is no proxy and no server to trust: integrity
//! comes from verifying the artifact's sha256 against the reviewed,
//! CI-verified index, and fitness from running the same conformance harness
//! the node's admission gate runs.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use inseam_kernel::substrate::Composition;

/// The inseam repository's own plugins tree: registry v0.
const DEFAULT_REGISTRY: &str = "https://raw.githubusercontent.com/aiuitech/inseam/main/plugins";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryIndex {
    #[serde(default)]
    plugin: Vec<IndexEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    /// Artifact path relative to the registry root.
    artifact: String,
    /// Lowercase hex sha256 of the artifact bytes.
    sha256: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Advisories {
    #[serde(default)]
    advisory: Vec<Advisory>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Advisory {
    name: String,
    version: String,
    reason: String,
}

/// Where a registry lives: an `http(s)` base URL or a local directory.
/// One code path serves both, so the hosted registry and a checked-out
/// repository behave identically (and tests never need the network).
enum Source {
    Url(String),
    Dir(PathBuf),
}

impl Source {
    fn parse(registry: &str) -> Self {
        if registry.starts_with("http://") || registry.starts_with("https://") {
            Self::Url(registry.trim_end_matches('/').to_string())
        } else {
            Self::Dir(PathBuf::from(registry))
        }
    }

    async fn fetch(&self, relative: &str) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Url(base) => {
                let url = format!("{base}/{relative}");
                let response = reqwest::get(&url)
                    .await
                    .with_context(|| format!("fetching {url}"))?
                    .error_for_status()
                    .with_context(|| format!("fetching {url}"))?;
                Ok(response.bytes().await?.to_vec())
            }
            Self::Dir(dir) => {
                let path = dir.join(relative);
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
            }
        }
    }

    async fn fetch_optional(&self, relative: &str) -> Option<Vec<u8>> {
        self.fetch(relative).await.ok()
    }
}

pub async fn install(
    name: &str,
    registry: Option<&str>,
    data_dir: &Path,
    composition_path: &Path,
) -> anyhow::Result<()> {
    let source = Source::parse(registry.unwrap_or(DEFAULT_REGISTRY));

    // The index names what exists; the advisory feed names what must not
    // run. Both are plain reviewed files in the registry tree.
    let index: RegistryIndex = toml::from_str(
        std::str::from_utf8(&source.fetch("registry.toml").await?)
            .context("registry.toml is not UTF-8")?,
    )
    .context("registry.toml")?;
    let Some(entry) = index.plugin.iter().find(|p| p.name == name) else {
        let known: Vec<&str> = index.plugin.iter().map(|p| p.name.as_str()).collect();
        bail!(
            "no plugin `{name}` in the registry (available: {})",
            known.join(", ")
        );
    };
    let advisories: Advisories = source
        .fetch_optional("advisories.toml")
        .await
        .and_then(|raw| toml::from_str(std::str::from_utf8(&raw).ok()?).ok())
        .unwrap_or_default();
    if let Some(advisory) = advisories
        .advisory
        .iter()
        .find(|a| a.name == entry.name && a.version == entry.version)
    {
        bail!(
            "{} {} is yanked: {} — refusing to install",
            entry.name,
            entry.version,
            advisory.reason
        );
    }

    // Fetch and verify: the artifact must hash to what the reviewed index
    // says, no matter what actually got served.
    println!(
        "fetching {} {} — {}",
        entry.name, entry.version, entry.description
    );
    let artifact_bytes = source.fetch(&entry.artifact).await?;
    let digest = hex(&Sha256::digest(&artifact_bytes));
    if !digest.eq_ignore_ascii_case(&entry.sha256) {
        bail!(
            "sha256 mismatch for {}: registry index says {}, fetched bytes hash to {digest} — \
             refusing to install",
            entry.artifact,
            entry.sha256
        );
    }
    println!("sha256 verified: {digest}");

    // The sidecars ride along: the manifest (required), the golden checks
    // and their fixtures (optional but expected of registry plugins).
    let manifest_rel = replace_extension(&entry.artifact, "manifest.toml");
    let manifest_bytes = source.fetch(&manifest_rel).await?;
    let checks_rel = replace_extension(&entry.artifact, "checks.toml");
    let checks_bytes = source.fetch_optional(&checks_rel).await;

    let install_dir = data_dir.join("plugins").join(&entry.name);
    std::fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let file_name = |relative: &str| {
        Path::new(relative)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| relative.to_string())
    };
    let artifact_path = install_dir.join(file_name(&entry.artifact));
    std::fs::write(&artifact_path, &artifact_bytes)?;
    std::fs::write(install_dir.join(file_name(&manifest_rel)), &manifest_bytes)?;
    if let Some(checks) = &checks_bytes {
        std::fs::write(install_dir.join(file_name(&checks_rel)), checks)?;
        let registry_dir = parent_of(&entry.artifact);
        for fixture in inseam_wasm_host::fixture_files(&String::from_utf8_lossy(checks)) {
            // Fixture paths come from a fetched file: confine them to the
            // install directory or refuse.
            if fixture.is_absolute()
                || fixture
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                bail!(
                    "checks file references fixture `{}`, which escapes the plugin directory — \
                     refusing to install",
                    fixture.display()
                );
            }
            let fixture_rel = format!("{registry_dir}{}", fixture.display());
            let bytes = source.fetch(&fixture_rel).await.with_context(|| {
                format!("fixture {} referenced by {checks_rel}", fixture.display())
            })?;
            let local = install_dir.join(&fixture);
            if let Some(dir) = local.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(local, bytes)?;
        }
    }

    // The same harness admission runs at mount: failing it here means the
    // plugin never touches the composition.
    println!("\nrunning conformance checks...");
    let report = inseam_wasm_host::check_artifact(&artifact_path).await;
    print!("{}", report.render());
    if !report.passed() {
        bail!(
            "{} failed its conformance checks; not mounting it",
            entry.name
        );
    }

    mount(&entry.name, &artifact_path, composition_path)?;
    println!(
        "\ninstalled and mounted `{}` ({}).\nIt activates on the next command; release cooldown \
         and capability gates still apply at mount. Unmount by removing its entry from {}.",
        entry.name,
        artifact_path.display(),
        composition_path.display()
    );
    Ok(())
}

/// Append the entry to the node's composition. Appending text (rather than
/// re-serializing) keeps the user's file exactly as they wrote it.
pub(crate) fn mount(name: &str, artifact: &Path, composition_path: &Path) -> anyhow::Result<()> {
    let existing = match std::fs::read_to_string(composition_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| composition_path.display().to_string()),
    };
    if !existing.is_empty() {
        let parsed = Composition::parse(&existing, &composition_path.display().to_string())?;
        if parsed.resolved().iter().any(|e| e.id == name) {
            bail!(
                "composition {} already has an entry `{name}`; edit it manually to point at {}",
                composition_path.display(),
                artifact.display()
            );
        }
    }
    let mut appended = existing;
    if !appended.is_empty() && !appended.ends_with("\n\n") {
        appended.push('\n');
    }
    appended.push_str(&format!(
        "[[entry]]\nid = \"{name}\"\nplugin = \"wasm:{}\"\n# [entry.config]\n# cooldown_days = 7  # soak newly observed versions before they may activate\n",
        artifact.display()
    ));
    if let Some(dir) = composition_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(composition_path, appended)
        .with_context(|| composition_path.display().to_string())?;
    Ok(())
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `ocr/ocr.wasm` -> `ocr/ocr.manifest.toml`, keeping the registry-relative
/// directory intact (Path::with_extension would also work, but strings keep
/// the separators exactly as the index wrote them).
fn replace_extension(artifact: &str, extension: &str) -> String {
    match artifact.rsplit_once('.') {
        Some((stem, _)) => format!("{stem}.{extension}"),
        None => format!("{artifact}.{extension}"),
    }
}

fn parent_of(artifact: &str) -> String {
    match artifact.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_replacement_keeps_registry_paths() {
        assert_eq!(
            replace_extension("ocr/ocr.wasm", "manifest.toml"),
            "ocr/ocr.manifest.toml"
        );
        assert_eq!(
            replace_extension("flat.wasm", "checks.toml"),
            "flat.checks.toml"
        );
    }

    #[test]
    fn hex_encodes_lowercase() {
        assert_eq!(hex(&[0x00, 0xAB, 0xFF]), "00abff");
    }
}
