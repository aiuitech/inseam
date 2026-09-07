//! `inseam plugin install` and `verify` against a registry on disk
//! (`design/registry.md`): a publisher key generated for the test signs the
//! OCR plugin's release record, the index names the publisher, and the
//! install proves signature, record, hashes, and harness before mounting.
//! Every way the tree can lie is refused by name: a tampered file, an
//! artifact swapped under a stale index, a publisher the roster does not
//! know, a signature by another key, a yanked version, an unsigned fixture.
//!
//! Requires the OCR artifact to be built (see `plugins/ocr/README.md`);
//! the tests skip with a notice when it is absent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn ocr_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ocr");
    dir.join("ocr.wasm")
        .exists()
        .then(|| dir.canonicalize().expect("canonicalizes"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A registry tree holding the OCR plugin, signed by a fresh test key.
struct TestRegistry {
    root: tempfile::TempDir,
    pair: minisign::KeyPair,
}

impl TestRegistry {
    fn build(ocr: &Path) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("ocr");
        std::fs::create_dir_all(dir.join("fixtures")).expect("dirs");
        for file in ["ocr.wasm", "ocr.manifest.toml", "ocr.checks.toml"] {
            std::fs::copy(ocr.join(file), dir.join(file)).expect("copies");
        }
        std::fs::copy(
            ocr.join("fixtures/pixel.png"),
            dir.join("fixtures/pixel.png"),
        )
        .expect("copies");
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        let registry = Self { root, pair };
        registry.write_roster("tests", &registry.pair.pk.to_base64());
        registry.sign("ocr", "0.2.0", "tests");
        registry.write_index("ocr", "0.2.0", "tests", &registry.artifact_digest("ocr"));
        registry
    }

    fn path(&self) -> String {
        self.root.path().display().to_string()
    }

    fn plugin_dir(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn artifact_digest(&self, name: &str) -> String {
        sha256_hex(
            &std::fs::read(self.plugin_dir(name).join(format!("{name}.wasm"))).expect("reads"),
        )
    }

    fn write_roster(&self, publisher: &str, public_key: &str) {
        std::fs::write(
            self.root.path().join("publishers.toml"),
            format!("[[publisher]]\nid = {publisher:?}\npublic_key = {public_key:?}\n"),
        )
        .expect("writes roster");
    }

    fn write_index(&self, name: &str, version: &str, publisher: &str, sha256: &str) {
        std::fs::write(
            self.root.path().join("registry.toml"),
            format!(
                "[[plugin]]\nname = {name:?}\nversion = {version:?}\npublisher = {publisher:?}\nartifact = \"{name}/{name}.wasm\"\nsha256 = {sha256:?}\n"
            ),
        )
        .expect("writes index");
    }

    /// Sign the plugin directory's files as the given publisher, with the
    /// test key (or another, for the forgery case).
    fn sign_with(
        &self,
        name: &str,
        version: &str,
        publisher: &str,
        key: &minisign::SecretKey,
        files: &[&str],
    ) {
        let dir = self.plugin_dir(name);
        let mut digests = BTreeMap::new();
        for file in files {
            digests.insert(
                file.to_string(),
                sha256_hex(&std::fs::read(dir.join(file)).expect("reads")),
            );
        }
        let mut record = format!(
            "name = {name:?}\nversion = {version:?}\npublisher = {publisher:?}\n\n[files]\n"
        );
        for (path, digest) in &digests {
            record.push_str(&format!("{path:?} = {digest:?}\n"));
        }
        std::fs::write(dir.join(format!("{name}.release.toml")), &record).expect("writes record");
        let signature = minisign::sign(
            None,
            key,
            std::io::Cursor::new(record.as_bytes()),
            None,
            None,
        )
        .expect("signs")
        .into_string();
        std::fs::write(dir.join(format!("{name}.release.toml.minisig")), signature)
            .expect("writes sig");
    }

    fn sign(&self, name: &str, version: &str, publisher: &str) {
        self.sign_with(
            name,
            version,
            publisher,
            &self.pair.sk,
            &[
                "ocr.wasm",
                "ocr.manifest.toml",
                "ocr.checks.toml",
                "fixtures/pixel.png",
            ],
        );
    }
}

#[tokio::test]
async fn a_signed_plugin_verifies_installs_and_mounts() {
    let Some(ocr) = ocr_dir() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let registry = TestRegistry::build(&ocr);
    inseam_cli::registry::verify(None, Some(&registry.path()))
        .await
        .expect("verifies the whole index");

    let node = tempfile::tempdir().expect("tempdir");
    let composition = node.path().join("composition.toml");
    inseam_cli::registry::install("ocr", Some(&registry.path()), node.path(), &composition)
        .await
        .expect("installs");
    let installed = node.path().join("plugins/ocr");
    for file in [
        "ocr.wasm",
        "ocr.manifest.toml",
        "ocr.checks.toml",
        "fixtures/pixel.png",
        "ocr.release.toml",
        "ocr.release.toml.minisig",
    ] {
        assert!(
            installed.join(file).exists(),
            "{file} installed beside the artifact"
        );
    }
    let written = std::fs::read_to_string(&composition).expect("composition written");
    assert!(written.contains("id = \"ocr\""));
    assert!(written.contains("plugin = \"wasm:"));
}

#[tokio::test]
async fn every_lie_in_the_tree_is_refused() {
    let Some(ocr) = ocr_dir() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let registry = TestRegistry::build(&ocr);
    let path = registry.path();
    let node = tempfile::tempdir().expect("tempdir");
    let composition = node.path().join("composition.toml");

    // A signed file tampered after signing.
    let checks = registry.plugin_dir("ocr").join("ocr.checks.toml");
    let original = std::fs::read(&checks).expect("reads");
    std::fs::write(&checks, [original.as_slice(), b"\n# tampered\n"].concat()).expect("writes");
    assert!(
        inseam_cli::registry::verify(Some("ocr"), Some(&path))
            .await
            .is_err()
    );
    std::fs::write(&checks, &original).expect("restores");
    inseam_cli::registry::verify(Some("ocr"), Some(&path))
        .await
        .expect("restored");

    // The index disagrees with the signed record about the artifact.
    registry.write_index("ocr", "0.2.0", "tests", &sha256_hex(b"other"));
    assert!(
        inseam_cli::registry::verify(Some("ocr"), Some(&path))
            .await
            .is_err()
    );
    registry.write_index("ocr", "0.2.0", "tests", &registry.artifact_digest("ocr"));

    // A publisher the roster does not know.
    registry.write_index("ocr", "0.2.0", "stranger", &registry.artifact_digest("ocr"));
    assert!(
        inseam_cli::registry::verify(Some("ocr"), Some(&path))
            .await
            .is_err()
    );
    registry.write_index("ocr", "0.2.0", "tests", &registry.artifact_digest("ocr"));

    // A record signed by a key that is not the publisher's.
    let other = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
    registry.sign_with(
        "ocr",
        "0.2.0",
        "tests",
        &other.sk,
        &[
            "ocr.wasm",
            "ocr.manifest.toml",
            "ocr.checks.toml",
            "fixtures/pixel.png",
        ],
    );
    assert!(
        inseam_cli::registry::verify(Some("ocr"), Some(&path))
            .await
            .is_err()
    );

    // A record that leaves a fixture the checks reference unsigned.
    registry.sign_with(
        "ocr",
        "0.2.0",
        "tests",
        &registry.pair.sk,
        &["ocr.wasm", "ocr.manifest.toml", "ocr.checks.toml"],
    );
    assert!(
        inseam_cli::registry::verify(Some("ocr"), Some(&path))
            .await
            .is_err()
    );
    registry.sign("ocr", "0.2.0", "tests");
    inseam_cli::registry::verify(Some("ocr"), Some(&path))
        .await
        .expect("restored");

    // A yanked version never installs.
    std::fs::write(
        registry.root.path().join("advisories.toml"),
        "[[advisory]]\nname = \"ocr\"\nversion = \"0.2.0\"\nreason = \"test yank\"\n",
    )
    .expect("writes advisories");
    let yanked = inseam_cli::registry::install("ocr", Some(&path), node.path(), &composition).await;
    assert!(yanked.is_err());
    assert!(!composition.exists(), "nothing touched the composition");
}
