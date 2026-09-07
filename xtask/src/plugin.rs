//! `cargo xtask plugin` — the publisher half of `design/registry.md`: the
//! key ceremony and the signing of registry plugins, done on a
//! maintainer's machine, which is the only place a publisher's secret key
//! exists (CI verifies signatures; it never holds a key):
//!
//!   cargo xtask plugin keygen [--publisher inseam]   # once; enrolls the public key in publishers.toml
//!   cargo xtask plugin sign ocr github               # release records + signatures; updates the index
//!   cargo xtask plugin sign --all
//!
//! `sign` hashes every file a plugin release is made of — the artifact,
//! the manifest, the golden checks, and each fixture the checks name —
//! into `<name>.release.toml`, signs it with the publisher's minisign key
//! into `<name>.release.toml.minisig`, and rewrites the plugin's entry in
//! `registry.toml` (version from the manifest, the artifact's sha256, the
//! publisher), so the two anchors a node checks cannot disagree by
//! accident. Both files enter the tree through the same reviewed PR as
//! the artifact.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Where the secret key lives unless `--secret-key` says otherwise.
const SECRET_KEY_DEFAULT: &str = ".config/inseam/publisher.key";
/// The environment variable a non-interactive maintainer shell may hold
/// the key's passphrase in; unset, the key prompts.
const PASSWORD_ENV: &str = "INSEAM_PUBLISHER_PASSWORD";
/// The publisher id the first-party plugins ship under.
const PUBLISHER_DEFAULT: &str = "inseam";
/// Fixture references a checks file may name; more is a corpus.
const FIXTURES_MAX: usize = 60;
/// Blocks in a registry index worth walking.
const INDEX_ENTRIES_MAX: usize = 1_024;

pub fn run(root: &Path, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("keygen") => keygen(root, &secret_key_path(args)?, publisher_id(args)),
        Some("sign") => {
            let names: Vec<String> = if args.iter().any(|a| a == "--all") {
                every_plugin(root)?
            } else {
                args[1..]
                    .iter()
                    .take_while(|a| !a.starts_with("--"))
                    .cloned()
                    .collect()
            };
            if names.is_empty() {
                bail!("usage: cargo xtask plugin sign <name>... | --all");
            }
            let secret_key = load_secret_key(&secret_key_path(args)?)?;
            let publisher = publisher_id(args);
            for name in &names {
                sign(root, name, &secret_key, publisher)?;
            }
            Ok(())
        }
        _ => bail!(
            "usage: cargo xtask plugin keygen [--publisher ID] [--secret-key PATH]\n       \
             cargo xtask plugin sign <name>... | --all [--publisher ID] [--secret-key PATH]"
        ),
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn publisher_id(args: &[String]) -> &str {
    flag(args, "--publisher").unwrap_or(PUBLISHER_DEFAULT)
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

fn password() -> Option<String> {
    std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty())
}

/// Generate an encrypted minisign keypair for a publisher and enroll its
/// public key in the registry's roster. The secret key is written once
/// and never read by anything but `sign`.
fn keygen(root: &Path, secret_key: &Path, publisher: &str) -> Result<()> {
    if secret_key.exists() {
        bail!(
            "{} already exists; refusing to overwrite a signing key",
            secret_key.display()
        );
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
        Some(&format!("inseam plugin publisher key: {publisher}")),
        password(),
    )
    .context("generating keypair")?;
    let public_key = pair.pk.to_base64();
    enroll_publisher(
        &root.join("plugins/publishers.toml"),
        publisher,
        &public_key,
    )?;
    println!("secret key written to {}", secret_key.display());
    println!("public key enrolled in plugins/publishers.toml as `{publisher}`:");
    println!("{public_key}");
    Ok(())
}

/// Add or replace the publisher's entry in the roster, keeping every other
/// line as written.
fn enroll_publisher(roster_path: &Path, publisher: &str, public_key: &str) -> Result<()> {
    let existing = std::fs::read_to_string(roster_path).unwrap_or_default();
    let block = format!(
        "[[publisher]]\nid = {publisher:?}\npublic_key = {public_key:?}\ndescription = \"\"\n"
    );
    let updated = replace_block(&existing, "[[publisher]]", "id", publisher, &block);
    std::fs::write(roster_path, updated).with_context(|| roster_path.display().to_string())?;
    Ok(())
}

fn load_secret_key(path: &Path) -> Result<minisign::SecretKey> {
    minisign::SecretKey::from_file(path, password()).with_context(|| {
        format!(
            "reading {}; run `cargo xtask plugin keygen`",
            path.display()
        )
    })
}

fn every_plugin(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root.join("plugins"))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().join(format!("{name}.wasm")).exists() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Build, sign, and index one plugin's release.
fn sign(root: &Path, name: &str, secret_key: &minisign::SecretKey, publisher: &str) -> Result<()> {
    let dir = root.join("plugins").join(name);
    let manifest_path = dir.join(format!("{name}.manifest.toml"));
    let manifest: toml::Table = toml::from_str(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| manifest_path.display().to_string())?,
    )
    .context("manifest")?;
    let version = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .context("manifest names no version")?
        .to_string();
    if manifest.get("name").and_then(|v| v.as_str()) != Some(name) {
        bail!(
            "{}: manifest name does not match the directory",
            manifest_path.display()
        );
    }
    let checks_path = dir.join(format!("{name}.checks.toml"));
    let checks: toml::Table = toml::from_str(
        &std::fs::read_to_string(&checks_path)
            .with_context(|| checks_path.display().to_string())?,
    )
    .context("checks")?;
    let mut paths: Vec<String> = vec![
        format!("{name}.wasm"),
        format!("{name}.manifest.toml"),
        format!("{name}.checks.toml"),
    ];
    paths.extend(fixture_references(&checks)?);
    paths.sort();
    paths.dedup();

    let mut files = BTreeMap::new();
    for path in &paths {
        let bytes =
            std::fs::read(dir.join(path)).with_context(|| format!("{name}: reading {path}"))?;
        files.insert(path.clone(), sha256_hex(&bytes));
    }
    let record = release_record(name, &version, publisher, &files);
    let record_path = dir.join(format!("{name}.release.toml"));
    std::fs::write(&record_path, &record)?;
    let signature = minisign::sign(
        None,
        secret_key,
        std::io::Cursor::new(record.as_bytes()),
        Some(&format!("{name} {version}")),
        None,
    )
    .context("signing the release record")?;
    std::fs::write(
        dir.join(format!("{name}.release.toml.minisig")),
        signature.into_string(),
    )?;
    let artifact_sha256 = files[&format!("{name}.wasm")].clone();
    update_index(
        &root.join("plugins/registry.toml"),
        name,
        &version,
        publisher,
        &artifact_sha256,
    )?;
    println!(
        "signed {name} {version} as `{publisher}` ({} files); index updated",
        files.len()
    );
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn release_record(
    name: &str,
    version: &str,
    publisher: &str,
    files: &BTreeMap<String, String>,
) -> String {
    let mut out = format!(
        "# The signed release record of `{name}` {version} (design/registry.md):\n\
         # every file this release is made of and its sha256, signed by the\n\
         # publisher in `{name}.release.toml.minisig`. Regenerate with\n\
         # `cargo xtask plugin sign {name}`; never edit by hand.\n\
         name = {name:?}\nversion = {version:?}\npublisher = {publisher:?}\n\n[files]\n"
    );
    for (path, digest) in files {
        out.push_str(&format!("{path:?} = {digest:?}\n"));
    }
    out
}

/// Every fixture a checks file names — `bytes_file` on a transform check,
/// `body_file` on a canned reply — found by walking the document, so the
/// signer needs no knowledge of either seam's schema.
fn fixture_references(checks: &toml::Table) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut pending: Vec<&toml::Value> = checks.values().collect();
    let mut visited = 0usize;
    while let Some(value) = pending.pop() {
        visited += 1;
        if visited > 100_000 {
            bail!("checks file is too large to walk");
        }
        match value {
            toml::Value::Table(table) => {
                for (key, inner) in table {
                    let names_fixture = key == "bytes_file" || key == "body_file";
                    if let Some(path) = inner.as_str().filter(|_| names_fixture) {
                        found.push(path.to_string());
                    }
                    pending.push(inner);
                }
            }
            toml::Value::Array(items) => pending.extend(items.iter()),
            _ => {}
        }
    }
    if found.len() > FIXTURES_MAX {
        bail!(
            "checks file names {} fixtures; at most {FIXTURES_MAX}",
            found.len()
        );
    }
    Ok(found)
}

/// Rewrite the plugin's `[[plugin]]` block in the index, or append one,
/// keeping the file's header and other entries as written. The
/// description is preserved from the existing block when there is one.
fn update_index(
    index_path: &Path,
    name: &str,
    version: &str,
    publisher: &str,
    sha256: &str,
) -> Result<()> {
    let existing = std::fs::read_to_string(index_path).unwrap_or_default();
    let description =
        existing_field(&existing, "[[plugin]]", "name", name, "description").unwrap_or_default();
    let block = format!(
        "[[plugin]]\nname = {name:?}\nversion = {version:?}\ndescription = {description:?}\npublisher = {publisher:?}\nartifact = \"{name}/{name}.wasm\"\nsha256 = {sha256:?}\n"
    );
    let updated = replace_block(&existing, "[[plugin]]", "name", name, &block);
    std::fs::write(index_path, updated).with_context(|| index_path.display().to_string())?;
    Ok(())
}

/// Split a TOML document into its preamble and its array-of-tables
/// blocks under `header`.
fn blocks(document: &str, header: &str) -> (String, Vec<String>) {
    let mut preamble = String::new();
    let mut blocks: Vec<String> = Vec::new();
    for line in document.lines() {
        if line.trim() == header {
            blocks.push(String::new());
        }
        match blocks.last_mut() {
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
            None => {
                preamble.push_str(line);
                preamble.push('\n');
            }
        }
    }
    assert!(blocks.len() <= INDEX_ENTRIES_MAX);
    (preamble, blocks)
}

fn block_field(block: &str, key: &str) -> Option<String> {
    block
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix(key)?
                .trim()
                .strip_prefix('=')
                .map(str::trim)
                .map(str::to_string)
        })
        .map(|value| value.trim_matches('"').to_string())
}

fn existing_field(
    document: &str,
    header: &str,
    key: &str,
    key_value: &str,
    field: &str,
) -> Option<String> {
    let (_, blocks) = blocks(document, header);
    blocks
        .iter()
        .find(|b| block_field(b, key).as_deref() == Some(key_value))
        .and_then(|b| block_field(b, field))
}

/// Replace the block whose `key` is `key_value` with `replacement`, or
/// append it; blank lines between blocks are normalized to one.
fn replace_block(
    document: &str,
    header: &str,
    key: &str,
    key_value: &str,
    replacement: &str,
) -> String {
    let (preamble, blocks) = blocks(document, header);
    let mut replaced = false;
    let mut out = preamble.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push('\n');
    }
    for block in &blocks {
        let this_one = block_field(block, key).as_deref() == Some(key_value);
        let text = if this_one {
            replaced = true;
            replacement
        } else {
            block.trim_end_matches('\n')
        };
        out.push('\n');
        out.push_str(text.trim_end_matches('\n'));
        out.push('\n');
    }
    if !replaced {
        out.push('\n');
        out.push_str(replacement.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_blocks_are_replaced_in_place_and_appended_when_new() {
        let index = "# header\n\n[[plugin]]\nname = \"ocr\"\nversion = \"0.1.0\"\ndescription = \"reads images\"\nsha256 = \"aa\"\n";
        let updated = replace_block(
            index,
            "[[plugin]]",
            "name",
            "ocr",
            "[[plugin]]\nname = \"ocr\"\nversion = \"0.2.0\"\n",
        );
        assert!(
            updated.starts_with("# header\n\n[[plugin]]\nname = \"ocr\"\nversion = \"0.2.0\"\n")
        );
        assert!(!updated.contains("0.1.0"));
        let appended = replace_block(
            &updated,
            "[[plugin]]",
            "name",
            "github",
            "[[plugin]]\nname = \"github\"\n",
        );
        assert!(appended.contains("name = \"ocr\""));
        assert!(appended.ends_with("[[plugin]]\nname = \"github\"\n"));
        assert_eq!(
            existing_field(index, "[[plugin]]", "name", "ocr", "description").as_deref(),
            Some("reads images")
        );
    }

    #[test]
    fn fixture_references_are_found_on_either_seam() {
        let checks: toml::Table = toml::from_str(
            "[[check]]\nbytes_file = \"fixtures/a.png\"\n[[check]]\n[[check.fetch]]\nbody_file = \"fixtures/b.json\"\n",
        )
        .expect("toml");
        let mut found = fixture_references(&checks).expect("walks");
        found.sort();
        assert_eq!(found, vec!["fixtures/a.png", "fixtures/b.json"]);
    }
}
