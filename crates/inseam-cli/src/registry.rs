//! `inseam plugin install` and `inseam plugin verify`: fetch a loaded-tier
//! plugin from a registry, prove it, and (for install) mount it
//! (`design/registry.md`).
//!
//! A registry is nothing but a directory tree — served over HTTPS (the
//! inseam repository's `plugins/` on GitHub is the default) or sitting on
//! local disk — containing `registry.toml` (the index: name, version,
//! publisher, artifact path, sha256), `publishers.toml` (the publisher
//! roster: ids and minisign public keys), `advisories.toml` (yanked
//! versions), and the plugin directories, each carrying a signed release
//! record beside its artifact. There is no proxy and no server to trust:
//! integrity comes from the artifact's sha256 in the reviewed index,
//! provenance from the publisher's signature over the release record, and
//! fitness from running the same conformance harness the node's admission
//! gate runs. Every fetched byte is checked against both anchors before
//! anything runs.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use inseam_kernel::substrate::Composition;

use crate::signing::{
    Publishers, ReleaseRecord, check_file, check_record_against_index, relative_path_is_confined,
    sha256_hex, verify_release,
};

/// The inseam repository's own plugins tree: registry v0.
const DEFAULT_REGISTRY: &str = "https://raw.githubusercontent.com/aiuitech/inseam/main/plugins";

/// Most bytes one registry file may be: an artifact is hundreds of
/// kilobytes, a fixture a few; a hundred megabytes is not a plugin.
const REGISTRY_FILE_BYTES_MAX: usize = 100 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryIndex {
    #[serde(default)]
    plugin: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    /// The publisher whose key signs this version's release record.
    publisher: String,
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
        if !relative_path_is_confined(relative) {
            bail!("refusing to read outside the registry: {relative}");
        }
        let bytes = match self {
            Self::Url(base) => {
                let url = format!("{base}/{relative}");
                let response = reqwest::get(&url)
                    .await
                    .with_context(|| format!("fetching {url}"))?
                    .error_for_status()
                    .with_context(|| format!("fetching {url}"))?;
                response.bytes().await?.to_vec()
            }
            Self::Dir(dir) => {
                let path = dir.join(relative);
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?
            }
        };
        if bytes.len() > REGISTRY_FILE_BYTES_MAX {
            bail!(
                "{relative} is {} bytes, over the {REGISTRY_FILE_BYTES_MAX} byte limit",
                bytes.len()
            );
        }
        Ok(bytes)
    }

    async fn fetch_text(&self, relative: &str) -> anyhow::Result<String> {
        String::from_utf8(self.fetch(relative).await?)
            .with_context(|| format!("{relative} is not UTF-8"))
    }

    async fn fetch_optional(&self, relative: &str) -> Option<Vec<u8>> {
        self.fetch(relative).await.ok()
    }
}

/// One plugin version, fetched and proven: every file the publisher
/// signed, hash-checked, and the artifact agreeing with the index too.
struct Verified {
    entry: IndexEntry,
    record: ReleaseRecord,
    /// Path relative to the plugin directory, and the bytes.
    files: Vec<(String, Vec<u8>)>,
    record_bytes: Vec<u8>,
    signature: String,
}

impl Verified {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, bytes)| bytes.as_slice())
    }
}

/// The index, and the entry for `name`.
async fn index_entry(source: &Source, name: &str) -> anyhow::Result<IndexEntry> {
    let index: RegistryIndex =
        toml::from_str(&source.fetch_text("registry.toml").await?).context("registry.toml")?;
    match index.plugin.iter().find(|p| p.name == name) {
        Some(entry) => Ok(entry.clone()),
        None => {
            let known: Vec<&str> = index.plugin.iter().map(|p| p.name.as_str()).collect();
            bail!(
                "no plugin `{name}` in the registry (available: {})",
                known.join(", ")
            )
        }
    }
}

async fn index_names(source: &Source) -> anyhow::Result<Vec<String>> {
    let index: RegistryIndex =
        toml::from_str(&source.fetch_text("registry.toml").await?).context("registry.toml")?;
    Ok(index.plugin.into_iter().map(|p| p.name).collect())
}

/// The advisory feed names what must not run.
async fn refuse_if_yanked(source: &Source, entry: &IndexEntry) -> anyhow::Result<()> {
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
            "{} {} is yanked: {} — refusing",
            entry.name,
            entry.version,
            advisory.reason
        );
    }
    Ok(())
}

/// Fetch and prove one plugin: the publisher from the reviewed roster,
/// the signature over the release record, the record against the index,
/// and every signed file against its digest.
async fn fetch_verified(source: &Source, name: &str) -> anyhow::Result<Verified> {
    let entry = index_entry(source, name).await?;
    refuse_if_yanked(source, &entry).await?;
    let roster = Publishers::parse(&source.fetch_text("publishers.toml").await?)?;
    let Some(publisher) = roster.find(&entry.publisher) else {
        bail!(
            "the index says `{}` is published by `{}`, which publishers.toml does not list — refusing",
            entry.name,
            entry.publisher
        );
    };
    let record_rel = replace_extension(&entry.artifact, "release.toml");
    let record_bytes = source.fetch(&record_rel).await?;
    let signature = source.fetch_text(&format!("{record_rel}.minisig")).await?;
    let record = verify_release(&record_bytes, &signature, publisher)?;
    check_record_against_index(
        &record,
        &entry.name,
        &entry.version,
        &entry.publisher,
        &entry.sha256,
    )?;

    let plugin_dir = parent_of(&entry.artifact);
    let artifact_name = file_name(&entry.artifact);
    let stem = artifact_name
        .strip_suffix(".wasm")
        .unwrap_or(&artifact_name)
        .to_string();
    for required in [
        artifact_name.clone(),
        format!("{stem}.manifest.toml"),
        format!("{stem}.checks.toml"),
    ] {
        if !record.files.contains_key(&required) {
            bail!("the signed release record does not list `{required}` — refusing");
        }
    }
    let mut files = Vec::with_capacity(record.files.len());
    for path in record.files.keys() {
        let bytes = source.fetch(&format!("{plugin_dir}{path}")).await?;
        check_file(&record, path, &bytes)?;
        files.push((path.clone(), bytes));
    }
    let verified = Verified {
        entry,
        record,
        files,
        record_bytes,
        signature,
    };
    // Both anchors, checked on the bytes themselves: the index's digest
    // and the signed one already agree, and the artifact must hash to it.
    let artifact_bytes = verified
        .file(&artifact_name)
        .expect("the artifact was listed and fetched");
    let digest = sha256_hex(artifact_bytes);
    if !digest.eq_ignore_ascii_case(&verified.entry.sha256) {
        bail!(
            "sha256 mismatch for {}: registry index says {}, fetched bytes hash to {digest} — refusing",
            verified.entry.artifact,
            verified.entry.sha256
        );
    }
    // Every fixture the checks reference must itself be signed, or the
    // checks the node re-runs at admission could be steered.
    let manifest_raw = String::from_utf8_lossy(
        verified
            .file(&format!("{stem}.manifest.toml"))
            .expect("listed"),
    );
    let seam = toml::from_str::<inseam_wasm_host::ArtifactManifest>(&manifest_raw)
        .map(|m| m.seam)
        .unwrap_or_default();
    let checks_raw = String::from_utf8_lossy(
        verified
            .file(&format!("{stem}.checks.toml"))
            .expect("listed"),
    );
    for fixture in inseam_wasm_host::fixture_files(&seam, &checks_raw) {
        let relative = fixture.to_string_lossy().replace('\\', "/");
        if !verified.record.files.contains_key(&relative) {
            bail!(
                "the checks file references fixture `{relative}`, which the signed release record does not list — refusing"
            );
        }
    }
    Ok(verified)
}

pub async fn install(
    name: &str,
    registry: Option<&str>,
    data_dir: &Path,
    composition_path: &Path,
) -> anyhow::Result<()> {
    let source = Source::parse(registry.unwrap_or(DEFAULT_REGISTRY));
    let verified = fetch_verified(&source, name).await?;
    println!(
        "fetched {} {} — {}\nsigned by `{}`; {} file(s) verified against the record and the index",
        verified.entry.name,
        verified.entry.version,
        verified.entry.description,
        verified.record.publisher,
        verified.files.len()
    );

    let install_dir = data_dir.join("plugins").join(&verified.entry.name);
    std::fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    for (relative, bytes) in &verified.files {
        let local = install_dir.join(relative);
        if let Some(dir) = local.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&local, bytes).with_context(|| local.display().to_string())?;
    }
    // Provenance stays beside the artifact: what was signed, and by whom.
    let record_name = replace_extension(&file_name(&verified.entry.artifact), "release.toml");
    std::fs::write(install_dir.join(&record_name), &verified.record_bytes)?;
    std::fs::write(
        install_dir.join(format!("{record_name}.minisig")),
        &verified.signature,
    )?;
    let artifact_path = install_dir.join(file_name(&verified.entry.artifact));

    // The same harness admission runs at mount: failing it here means the
    // plugin never touches the composition.
    println!("\nrunning conformance checks...");
    let report = inseam_wasm_host::check_artifact(&artifact_path).await;
    print!("{}", report.render());
    if !report.passed() {
        bail!(
            "{} failed its conformance checks; not mounting it",
            verified.entry.name
        );
    }

    mount(&verified.entry.name, &artifact_path, composition_path)?;
    println!(
        "\ninstalled and mounted `{}` ({}).\nIt activates on the next command; release cooldown \
         and capability gates still apply at mount. Unmount by removing its entry from {}.",
        verified.entry.name,
        artifact_path.display(),
        composition_path.display()
    );
    let seam = String::from_utf8_lossy(
        verified
            .file(&replace_extension(
                &file_name(&verified.entry.artifact),
                "manifest.toml",
            ))
            .unwrap_or_default(),
    )
    .parse::<toml::Table>()
    .ok()
    .and_then(|m| m.get("seam").and_then(|s| s.as_str()).map(str::to_string))
    .unwrap_or_default();
    if seam == "connection" {
        println!(
            "It is a connection: fill in `[entry.config.plugin]` on its entry (the plugin's README says what), \
             then `inseam hosts` shows the host it stewards."
        );
    }
    Ok(())
}

/// Prove one plugin, or every plugin the index lists, without installing:
/// the registry's own CI gate, and an owner's way to audit a tree.
pub async fn verify(name: Option<&str>, registry: Option<&str>) -> anyhow::Result<()> {
    let source = Source::parse(registry.unwrap_or(DEFAULT_REGISTRY));
    let names = match name {
        Some(one) => vec![one.to_string()],
        None => index_names(&source).await?,
    };
    if names.is_empty() {
        bail!("the registry index lists no plugins");
    }
    let mut failures = 0u32;
    for name in &names {
        match fetch_verified(&source, name).await {
            Ok(verified) => println!(
                "ok    {} {} — signed by `{}`, {} file(s) match the record and the index",
                verified.entry.name,
                verified.entry.version,
                verified.record.publisher,
                verified.files.len()
            ),
            Err(e) => {
                failures += 1;
                println!("FAIL  {name} — {e:#}");
            }
        }
    }
    if failures > 0 {
        bail!(
            "{failures} of {} plugin(s) failed verification",
            names.len()
        );
    }
    Ok(())
}

/// Append the entry to the node's composition. Appending text (rather than
/// re-serializing) keeps the user's file exactly as they wrote it. A
/// connection's entry gets the plugin-config stub it cannot run without,
/// commented, so the owner's next edit is obvious.
pub(crate) fn mount(name: &str, artifact: &Path, composition_path: &Path) -> anyhow::Result<()> {
    let seam = std::fs::read_to_string(artifact.with_extension("manifest.toml"))
        .ok()
        .and_then(|raw| toml::from_str::<inseam_wasm_host::ArtifactManifest>(&raw).ok())
        .map(|m| m.seam)
        .unwrap_or_default();
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
    let config_stub = if seam == "connection" {
        "[entry.config]\nroots = [\"\"]              # the scopes this host is indexed by; \"\" is the whole host\n# grant = \"<oauth grant id>\"   # when the plugin authorizes its requests\n# cooldown_days = 7        # soak newly observed versions before they may activate\n[entry.config.plugin]\n# REPLACE: the plugin's own config (its README says what); the entry fails by name until it is set\n"
    } else {
        "# [entry.config]\n# cooldown_days = 7  # soak newly observed versions before they may activate\n"
    };
    appended.push_str(&format!(
        "[[entry]]\nid = \"{name}\"\nplugin = \"wasm:{}\"\n{config_stub}",
        artifact.display()
    ));
    if let Some(dir) = composition_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(composition_path, appended)
        .with_context(|| composition_path.display().to_string())?;
    Ok(())
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

fn file_name(relative: &str) -> String {
    match relative.rsplit_once('/') {
        Some((_, name)) => name.to_string(),
        None => relative.to_string(),
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
        assert_eq!(parent_of("ocr/ocr.wasm"), "ocr/");
        assert_eq!(file_name("ocr/ocr.wasm"), "ocr.wasm");
    }

    #[tokio::test]
    async fn the_source_never_reads_outside_the_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = Source::parse(&dir.path().display().to_string());
        assert!(source.fetch("../etc/passwd").await.is_err());
        assert!(source.fetch("/etc/passwd").await.is_err());
    }
}
