//! `inseam self update` (`design/releases.md`): one command, one manifest
//! format, any origin. An origin is a base URL or directory that serves
//! `manifest.json`, its detached minisign signature, and the artifacts the
//! manifest names. The verifying key is the distribution's and is compiled
//! in; the origin alone may be overridden at runtime, which turns a stock
//! binary into a mirror client and never into something that trusts a
//! different signer.
//!
//! The flow is: fetch manifest + signature, verify, pick the running cohort
//! and target, compare versions (inequality, not ordering — promoting an
//! older version is how rollback works), download the tarball, check its
//! sha256 against the manifest, extract the binary, rename it over the
//! running executable, and exit so the supervisor restarts it.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where a distribution's releases come from and which key signs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateChannel {
    /// Base URL (or local directory, for mirrors on disk) under which
    /// `manifest.json`, `manifest.json.minisig`, and artifacts are served.
    pub origin: String,
    /// Base64 minisign public key, as printed by `cargo xtask release keygen`.
    pub public_key: String,
}

impl UpdateChannel {
    /// The stock distribution: GitHub's `latest/download/` redirect is a
    /// plain asset path, so "latest" costs no API call and no rate-limit
    /// budget. The release workflow uploads the manifest as one more asset.
    pub fn first_party() -> Self {
        Self {
            origin: "https://github.com/aiuitech/inseam/releases/latest/download".to_string(),
            public_key: FIRST_PARTY_PUBLIC_KEY.to_string(),
        }
    }
}

/// The inseam open-source release signing key. Its secret half lives on a
/// maintainer's machine, never in CI (`design/releases.md`). Until a
/// maintainer runs `cargo xtask release keygen` and pastes the result here,
/// the stock binary refuses every manifest, which is the safe failure.
const FIRST_PARTY_PUBLIC_KEY: &str = "RWSurch1AVFj0KaQ7WxNWGK3f+LIsqtYBi44fpT2e11kbA/3IP6+ko/l";

/// Environment variable naming an alternative origin; `--origin` wins over it.
pub const ORIGIN_ENV: &str = "INSEAM_RELEASE_ORIGIN";
/// The target triple this binary was built for, exported by `build.rs`.
const RUNNING_TARGET: &str = env!("INSEAM_TARGET");
/// The version this binary reports; the manifest's version is compared to it.
const RUNNING_VERSION: &str = env!("CARGO_PKG_VERSION");
/// A manifest is a few cohorts of a few targets; anything larger is not ours.
const MANIFEST_BYTES_MAX: usize = 1 << 20;
/// A release tarball is tens of megabytes; a gigabyte is a hostile origin.
const ARTIFACT_BYTES_MAX: usize = 1 << 30;
/// Entries walked in a tarball before giving up on finding the binary.
const TAR_ENTRIES_MAX: u32 = 64;
/// The only manifest schema this binary reads.
const MANIFEST_SCHEMA: u32 = 1;
/// Name of the executable inside a release tarball.
const BINARY_NAME: &str = "inseam";

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub cohorts: BTreeMap<String, Cohort>,
}

#[derive(Debug, Deserialize)]
pub struct Cohort {
    pub version: String,
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    /// Relative to the origin (URL-join semantics for URLs, path-join for
    /// directories, where `..` is refused).
    pub path: String,
    /// Lowercase hex sha256 of the tarball bytes.
    pub sha256: String,
}

/// What `self update` decided before touching the filesystem.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The cohort names the version already running.
    Current { version: String },
    /// The cohort names a different version; `artifact` is what to fetch.
    Change {
        from: String,
        to: String,
        artifact: Artifact,
    },
}

/// An origin that serves bytes by relative path; one code path for the
/// network and for a directory, so mirrors on disk work and tests never
/// need the network (the registry makes the same move).
enum Origin {
    Url(String),
    Dir(PathBuf),
}

impl Origin {
    fn parse(origin: &str) -> Self {
        if origin.starts_with("http://") || origin.starts_with("https://") {
            Self::Url(origin.trim_end_matches('/').to_string())
        } else {
            Self::Dir(PathBuf::from(origin))
        }
    }

    async fn fetch(&self, relative: &str, bytes_max: usize) -> anyhow::Result<Vec<u8>> {
        let bytes = match self {
            Self::Url(base) => {
                let url = join_url(base, relative);
                let response = reqwest::get(&url)
                    .await
                    .with_context(|| format!("fetching {url}"))?
                    .error_for_status()
                    .with_context(|| format!("fetching {url}"))?;
                response.bytes().await?.to_vec()
            }
            Self::Dir(dir) => {
                if relative.split('/').any(|segment| segment == "..") {
                    bail!("refusing to read outside the origin directory: {relative}");
                }
                let path = dir.join(relative);
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?
            }
        };
        if bytes.len() > bytes_max {
            bail!(
                "{relative} is {} bytes, over the {bytes_max} byte limit",
                bytes.len()
            );
        }
        Ok(bytes)
    }
}

/// Resolve `relative` against `base` with `..` support, so a GitHub manifest
/// on the `latest/download/` path can name artifacts on a per-tag path
/// (`../../download/v1.2.3/...`) — which is what makes rollback the same
/// action as promotion (`design/releases.md`).
fn join_url(base: &str, relative: &str) -> String {
    let mut segments: Vec<&str> = base.split('/').collect();
    for segment in relative.split('/') {
        match segment {
            "" | "." => {}
            // Never climb past the host: `scheme:`, ``, `host` stay fixed.
            ".." if segments.len() > 3 => {
                segments.pop();
            }
            ".." => {}
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Fetch and verify the manifest at `origin` against `public_key`. Nothing
/// in the manifest is trusted before this returns.
pub async fn fetch_manifest(origin: &str, public_key: &str) -> anyhow::Result<Manifest> {
    let source = Origin::parse(origin);
    let manifest_bytes = source.fetch("manifest.json", MANIFEST_BYTES_MAX).await?;
    let signature_bytes = source
        .fetch("manifest.json.minisig", MANIFEST_BYTES_MAX)
        .await?;
    let signature_text = String::from_utf8(signature_bytes).context("signature is not UTF-8")?;
    verify_manifest(&manifest_bytes, &signature_text, public_key)?;
    parse_manifest(&manifest_bytes)
}

/// Verify the signature before parsing: bytes that fail the key are never
/// handed to a parser.
pub fn verify_manifest(manifest: &[u8], signature: &str, public_key: &str) -> anyhow::Result<()> {
    let key = minisign_verify::PublicKey::from_base64(public_key).context(
        "release public key compiled into this binary is malformed; the distribution was \
             built without a key from `cargo xtask release keygen`",
    )?;
    let signature = minisign_verify::Signature::decode(signature)
        .context("manifest.json.minisig is not a minisign signature")?;
    key.verify(manifest, &signature, false)
        .context("manifest.json signature does not verify against this distribution's key")?;
    Ok(())
}

fn parse_manifest(bytes: &[u8]) -> anyhow::Result<Manifest> {
    let manifest: Manifest = serde_json::from_slice(bytes).context("parsing manifest.json")?;
    if manifest.schema != MANIFEST_SCHEMA {
        bail!(
            "manifest schema {} is not {MANIFEST_SCHEMA}; update this binary by hand",
            manifest.schema
        );
    }
    Ok(manifest)
}

/// Compare the cohort's version for `target` with the running one.
pub fn decide(
    mut manifest: Manifest,
    cohort: &str,
    target: &str,
    running_version: &str,
) -> anyhow::Result<Decision> {
    let Some(mut cohort_entry) = manifest.cohorts.remove(cohort) else {
        bail!("cohort {cohort:?} is not in the manifest");
    };
    if cohort_entry.version == running_version {
        return Ok(Decision::Current {
            version: cohort_entry.version,
        });
    }
    let Some(artifact) = cohort_entry.artifacts.remove(target) else {
        bail!(
            "version {} has no artifact for {target}; this platform must build from source",
            cohort_entry.version
        );
    };
    Ok(Decision::Change {
        from: running_version.to_string(),
        to: cohort_entry.version,
        artifact,
    })
}

/// Download `artifact`, verify its sha256, and return the executable bytes
/// inside the tarball.
pub async fn fetch_binary(origin: &str, artifact: &Artifact) -> anyhow::Result<Vec<u8>> {
    let source = Origin::parse(origin);
    let tarball = source.fetch(&artifact.path, ARTIFACT_BYTES_MAX).await?;
    let digest = hex(&Sha256::digest(&tarball));
    if !digest.eq_ignore_ascii_case(&artifact.sha256) {
        bail!(
            "sha256 mismatch for {}: manifest says {}, fetched bytes hash to {digest}",
            artifact.path,
            artifact.sha256
        );
    }
    extract_binary(&tarball)
}

fn extract_binary(tarball: &[u8]) -> anyhow::Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().context("reading release tarball")?;
    for (index, entry) in entries.enumerate() {
        if index >= TAR_ENTRIES_MAX as usize {
            bail!("release tarball has more than {TAR_ENTRIES_MAX} entries");
        }
        let mut entry = entry.context("reading release tarball entry")?;
        let path = entry.path().context("release tarball entry path")?;
        let is_binary = path.file_name().is_some_and(|name| name == BINARY_NAME);
        if is_binary {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("reading binary from tarball")?;
            return Ok(bytes);
        }
    }
    bail!("release tarball contains no `{BINARY_NAME}` executable")
}

/// Write `binary` beside `destination` and rename it into place. The rename
/// is atomic and a running process keeps its old inode, so the swap is safe
/// while the node serves; the supervisor restart makes it take effect.
pub fn replace_executable(destination: &Path, binary: &[u8]) -> anyhow::Result<()> {
    let directory = destination
        .parent()
        .context("executable path has no parent directory")?;
    let staged = directory.join(format!(".{BINARY_NAME}.update-{}", std::process::id()));
    std::fs::write(&staged, binary).with_context(|| format!("writing {}", staged.display()))?;
    set_executable(&staged)?;
    if let Err(error) = std::fs::rename(&staged, destination) {
        // Best effort: the staged file is the only residue of a failed swap.
        let _ = std::fs::remove_file(&staged);
        return Err(error).with_context(|| format!("replacing {}", destination.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// The command: resolve origin, verify, decide, and unless `check_only`,
/// swap the binary. Prints what it did; the caller exits.
pub async fn self_update(
    channel: &UpdateChannel,
    origin_override: Option<&str>,
    cohort: &str,
    check_only: bool,
) -> anyhow::Result<()> {
    let origin = origin_override.unwrap_or(&channel.origin);
    let manifest = fetch_manifest(origin, &channel.public_key).await?;
    match decide(manifest, cohort, RUNNING_TARGET, RUNNING_VERSION)? {
        Decision::Current { version } => {
            println!("inseam {version} is current (cohort {cohort}, origin {origin})");
        }
        Decision::Change { from, to, artifact } if check_only => {
            println!(
                "inseam {from} -> {to} available (cohort {cohort}, {})",
                artifact.path
            );
        }
        Decision::Change { from, to, artifact } => {
            let binary = fetch_binary(origin, &artifact).await?;
            let destination = std::env::current_exe().context("locating the running executable")?;
            replace_executable(&destination, &binary)?;
            println!(
                "inseam {from} -> {to} installed at {}; restart to run it",
                destination.display()
            );
        }
    }
    Ok(())
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct SignedOrigin {
        dir: tempfile::TempDir,
        public_key: String,
        secret_key: minisign::SecretKey,
    }

    impl SignedOrigin {
        fn new() -> Self {
            let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
            Self {
                dir: tempfile::tempdir().unwrap(),
                public_key: pair.pk.to_base64(),
                secret_key: pair.sk,
            }
        }

        fn origin(&self) -> String {
            self.dir.path().to_string_lossy().into_owned()
        }

        fn publish_manifest(&self, manifest: &str) {
            std::fs::write(self.dir.path().join("manifest.json"), manifest).unwrap();
            let signature = minisign::sign(None, &self.secret_key, manifest.as_bytes(), None, None)
                .unwrap()
                .into_string();
            std::fs::write(self.dir.path().join("manifest.json.minisig"), signature).unwrap();
        }

        /// Write a tarball holding `binary` and return the artifact entry.
        fn publish_tarball(&self, relative: &str, binary: &[u8]) -> String {
            let mut tar_bytes = Vec::new();
            {
                let encoder =
                    flate2::write::GzEncoder::new(&mut tar_bytes, flate2::Compression::fast());
                let mut builder = tar::Builder::new(encoder);
                let mut header = tar::Header::new_gnu();
                header.set_size(binary.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder
                    .append_data(&mut header, BINARY_NAME, binary)
                    .unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            let path = self.dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(&path)
                .unwrap()
                .write_all(&tar_bytes)
                .unwrap();
            hex(&Sha256::digest(&tar_bytes))
        }
    }

    fn manifest_json(version: &str, target: &str, path: &str, sha256: &str) -> String {
        serde_json::json!({
            "schema": 1,
            "cohorts": { "stable": { "version": version, "artifacts": {
                target: { "path": path, "sha256": sha256 }
            } } }
        })
        .to_string()
    }

    #[tokio::test]
    async fn verifies_and_parses_a_signed_manifest() {
        let origin = SignedOrigin::new();
        origin.publish_manifest(&manifest_json("9.9.9", "t", "9.9.9/x.tar.gz", "00"));
        let manifest = fetch_manifest(&origin.origin(), &origin.public_key)
            .await
            .unwrap();
        assert_eq!(manifest.cohorts["stable"].version, "9.9.9");
    }

    #[tokio::test]
    async fn rejects_a_manifest_signed_by_another_key() {
        let origin = SignedOrigin::new();
        let other = SignedOrigin::new();
        origin.publish_manifest(&manifest_json("9.9.9", "t", "x", "00"));
        let error = fetch_manifest(&origin.origin(), &other.public_key)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signature"), "{error:#}");
    }

    #[tokio::test]
    async fn rejects_a_tampered_manifest() {
        let origin = SignedOrigin::new();
        origin.publish_manifest(&manifest_json("9.9.9", "t", "x", "00"));
        std::fs::write(
            origin.dir.path().join("manifest.json"),
            manifest_json("6.6.6", "t", "x", "00"),
        )
        .unwrap();
        assert!(
            fetch_manifest(&origin.origin(), &origin.public_key)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_an_unknown_schema() {
        let origin = SignedOrigin::new();
        origin.publish_manifest(r#"{"schema":2,"cohorts":{}}"#);
        let error = fetch_manifest(&origin.origin(), &origin.public_key)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("schema"), "{error:#}");
    }

    #[test]
    fn decides_current_when_versions_match() {
        let manifest = parse_manifest(manifest_json("1.0.0", "t", "p", "00").as_bytes()).unwrap();
        let decision = decide(manifest, "stable", "t", "1.0.0").unwrap();
        assert_eq!(
            decision,
            Decision::Current {
                version: "1.0.0".into()
            }
        );
    }

    #[test]
    fn decides_change_in_either_direction() {
        for running in ["0.9.0", "1.1.0"] {
            let manifest =
                parse_manifest(manifest_json("1.0.0", "t", "p", "00").as_bytes()).unwrap();
            let decision = decide(manifest, "stable", "t", running).unwrap();
            let Decision::Change { from, to, .. } = decision else {
                panic!("expected change")
            };
            assert_eq!(from, running);
            assert_eq!(to, "1.0.0");
        }
    }

    #[test]
    fn rejects_unknown_cohort_and_target() {
        let manifest = parse_manifest(manifest_json("1.0.0", "t", "p", "00").as_bytes()).unwrap();
        assert!(decide(manifest, "canary", "t", "0.1.0").is_err());
        let manifest = parse_manifest(manifest_json("1.0.0", "t", "p", "00").as_bytes()).unwrap();
        assert!(decide(manifest, "stable", "other", "0.1.0").is_err());
    }

    #[tokio::test]
    async fn downloads_verifies_and_replaces_the_binary() {
        let origin = SignedOrigin::new();
        let sha = origin.publish_tarball("1.0.0/inseam-t.tar.gz", b"#!/bin/sh\necho new\n");
        let artifact = Artifact {
            path: "1.0.0/inseam-t.tar.gz".into(),
            sha256: sha,
        };
        let binary = fetch_binary(&origin.origin(), &artifact).await.unwrap();
        let destination = origin.dir.path().join("bin").join("inseam");
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(&destination, b"old").unwrap();
        replace_executable(&destination, &binary).unwrap();
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"#!/bin/sh\necho new\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(destination.parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(leftovers.len(), 1, "staged file must not remain");
    }

    #[tokio::test]
    async fn rejects_a_tarball_whose_hash_differs() {
        let origin = SignedOrigin::new();
        origin.publish_tarball("x.tar.gz", b"bytes");
        let artifact = Artifact {
            path: "x.tar.gz".into(),
            sha256: "00".repeat(32),
        };
        let error = fetch_binary(&origin.origin(), &artifact).await.unwrap_err();
        assert!(error.to_string().contains("sha256 mismatch"), "{error:#}");
    }

    #[tokio::test]
    async fn refuses_paths_that_escape_a_directory_origin() {
        let origin = SignedOrigin::new();
        let artifact = Artifact {
            path: "../etc/passwd".into(),
            sha256: "00".into(),
        };
        let error = fetch_binary(&origin.origin(), &artifact).await.unwrap_err();
        assert!(
            error.to_string().contains("outside the origin"),
            "{error:#}"
        );
    }

    #[test]
    fn joins_urls_with_parent_segments_but_never_past_the_host() {
        let base = "https://github.com/aiuitech/inseam/releases/latest/download";
        assert_eq!(
            join_url(base, "../../download/v1.2.3/inseam-x.tar.gz"),
            "https://github.com/aiuitech/inseam/releases/download/v1.2.3/inseam-x.tar.gz"
        );
        assert_eq!(
            join_url("https://r.example/hosted", "1.0.0/a.tar.gz"),
            "https://r.example/hosted/1.0.0/a.tar.gz"
        );
        assert_eq!(
            join_url("https://r.example/a", "../../../../b"),
            "https://r.example/b"
        );
    }
}
