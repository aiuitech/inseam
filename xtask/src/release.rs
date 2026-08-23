//! `cargo xtask release` — the operator half of `design/releases.md`. CI
//! builds and publishes a **draft** GitHub release with tarballs and
//! `checksums.txt`; a maintainer promotes it from their own machine, which
//! is the only place the signing key exists:
//!
//!   cargo xtask release keygen                 # once; prints the public key
//!   cargo xtask release promote v0.2.0         # sign manifest, upload, undraft
//!   cargo xtask release promote v0.1.0         # same command is the rollback
//!
//! `promote` builds `manifest.json` from the tag's `checksums.txt`, naming
//! artifacts by per-tag path (`../../download/<tag>/…`) so the manifest on
//! GitHub's `latest/download/` can point at any release's tarballs — which
//! is what makes rollback the same action as promotion. It then signs the
//! manifest, uploads both files to the *latest* release (undrafting the
//! tag first if it is the newest), and `inseam self update` sees it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Where the secret key lives unless `--secret-key` says otherwise.
const SECRET_KEY_DEFAULT: &str = ".config/inseam/release.key";
/// GitHub repository the stock distribution releases from.
const REPOSITORY: &str = "aiuitech/inseam";
/// Lines in a `checksums.txt` worth reading; one per target is the norm.
const CHECKSUM_LINES_MAX: u32 = 64;

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("keygen") => keygen(&secret_key_path(args)?),
        Some("promote") => {
            let tag = args.get(1).context("usage: cargo xtask release promote <tag>")?;
            let cohort = flag(args, "--cohort").unwrap_or("stable");
            promote(tag, cohort, &secret_key_path(args)?)
        }
        _ => bail!(
            "usage: cargo xtask release keygen [--secret-key PATH]\n       \
             cargo xtask release promote <tag> [--cohort NAME] [--secret-key PATH]"
        ),
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn secret_key_path(args: &[String]) -> Result<PathBuf> {
    match flag(args, "--secret-key") {
        Some(path) => Ok(PathBuf::from(path)),
        None => {
            let home = std::env::var_os("HOME").context("HOME is not set; pass --secret-key")?;
            Ok(PathBuf::from(home).join(SECRET_KEY_DEFAULT))
        }
    }
}

/// Generate an encrypted minisign keypair. The public key is printed for
/// pasting into the distribution; the secret key is written once and never
/// read by anything but `promote`.
fn keygen(secret_key: &Path) -> Result<()> {
    if secret_key.exists() {
        bail!("{} already exists; refusing to overwrite a signing key", secret_key.display());
    }
    if let Some(dir) = secret_key.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut public_key_text = Vec::new();
    let mut secret_key_file = std::fs::File::create(secret_key)
        .with_context(|| format!("creating {}", secret_key.display()))?;
    let pair = minisign::KeyPair::generate_and_write_encrypted_keypair(
        &mut public_key_text,
        &mut secret_key_file,
        Some("inseam release signing key"),
        None,
    )
    .context("generating keypair")?;
    println!("secret key written to {}", secret_key.display());
    println!("public key (paste into the distribution's UpdateChannel):");
    println!("{}", pair.pk.to_base64());
    Ok(())
}

fn promote(tag: &str, cohort: &str, secret_key: &Path) -> Result<()> {
    let checksums = gh_release_asset(tag, "checksums.txt")?;
    let manifest = manifest_json(tag, cohort, &checksums)?;
    let signature = sign(&manifest, secret_key)?;

    let staging = tempfile::tempdir()?;
    let manifest_path = staging.path().join("manifest.json");
    let signature_path = staging.path().join("manifest.json.minisig");
    std::fs::write(&manifest_path, &manifest)?;
    std::fs::write(&signature_path, &signature)?;

    // A draft never becomes `latest`, so undraft the tag first if it is the
    // newest release; a rollback to an older tag leaves drafts alone.
    if gh_release_is_draft(tag)? {
        gh(&["release", "edit", tag, "--repo", REPOSITORY, "--draft=false"])?;
        println!("published {tag}");
    }
    let latest = gh_latest_tag()?;
    gh(&[
        "release", "upload", &latest, "--repo", REPOSITORY, "--clobber",
        manifest_path.to_str().context("staging path is not UTF-8")?,
        signature_path.to_str().context("staging path is not UTF-8")?,
    ])?;
    println!("promoted {tag} to cohort {cohort}; manifest uploaded to {latest} (latest)");
    Ok(())
}

/// Build the manifest from a release's `checksums.txt` (lines of
/// `<sha256>  inseam-<target>.tar.gz`).
fn manifest_json(tag: &str, cohort: &str, checksums: &str) -> Result<String> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    let mut artifacts = serde_json::Map::new();
    for (index, line) in checksums.lines().enumerate() {
        if index >= CHECKSUM_LINES_MAX as usize {
            bail!("checksums.txt has more than {CHECKSUM_LINES_MAX} lines");
        }
        let (sha256, asset) = line
            .split_once("  ")
            .with_context(|| format!("malformed checksums.txt line: {line:?}"))?;
        let target = asset
            .strip_prefix("inseam-")
            .and_then(|rest| rest.strip_suffix(".tar.gz"))
            .with_context(|| format!("asset {asset:?} is not inseam-<target>.tar.gz"))?;
        artifacts.insert(
            target.to_string(),
            serde_json::json!({
                "path": format!("../../download/{tag}/{asset}"),
                "sha256": sha256,
            }),
        );
    }
    if artifacts.is_empty() {
        bail!("checksums.txt for {tag} names no artifacts");
    }
    let manifest = serde_json::json!({
        "schema": 1,
        "cohorts": { cohort: { "version": version, "artifacts": artifacts } },
    });
    Ok(serde_json::to_string_pretty(&manifest)?)
}

fn sign(manifest: &str, secret_key: &Path) -> Result<String> {
    let key_box = minisign::SecretKeyBox::from_string(
        &std::fs::read_to_string(secret_key)
            .with_context(|| format!("reading {}; run `cargo xtask release keygen`", secret_key.display()))?,
    )?;
    let key = key_box.into_secret_key(None).context("unlocking secret key")?;
    let signature = minisign::sign(None, &key, manifest.as_bytes(), Some("inseam release manifest"), None)?;
    Ok(signature.into_string())
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .context("running gh; is the GitHub CLI installed and authenticated?")?;
    if !output.status.success() {
        bail!("gh {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn gh_release_asset(tag: &str, asset: &str) -> Result<String> {
    let staging = tempfile::tempdir()?;
    let dir = staging.path().to_str().context("staging path is not UTF-8")?;
    gh(&["release", "download", tag, "--repo", REPOSITORY, "--pattern", asset, "--dir", dir])?;
    std::fs::read_to_string(staging.path().join(asset))
        .with_context(|| format!("{asset} missing from release {tag}"))
}

fn gh_release_is_draft(tag: &str) -> Result<bool> {
    let json = gh(&["release", "view", tag, "--repo", REPOSITORY, "--json", "isDraft"])?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    value["isDraft"].as_bool().context("gh release view returned no isDraft")
}

fn gh_latest_tag() -> Result<String> {
    let json = gh(&["release", "view", "--repo", REPOSITORY, "--json", "tagName"])?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    value["tagName"]
        .as_str()
        .map(str::to_string)
        .context("no published release is latest; publish one first")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_artifacts_by_per_tag_path() {
        let checksums = "aa  inseam-x86_64-unknown-linux-gnu.tar.gz\nbb  inseam-aarch64-apple-darwin.tar.gz\n";
        let manifest = manifest_json("v1.2.3", "stable", checksums).unwrap();
        let value: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(value["cohorts"]["stable"]["version"], "1.2.3");
        assert_eq!(
            value["cohorts"]["stable"]["artifacts"]["aarch64-apple-darwin"]["path"],
            "../../download/v1.2.3/inseam-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(value["cohorts"]["stable"]["artifacts"]["x86_64-unknown-linux-gnu"]["sha256"], "aa");
    }

    #[test]
    fn rejects_checksums_that_name_foreign_assets() {
        assert!(manifest_json("v1.0.0", "stable", "aa  other.zip\n").is_err());
        assert!(manifest_json("v1.0.0", "stable", "").is_err());
    }
}
