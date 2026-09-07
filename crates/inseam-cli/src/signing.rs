//! Per-publisher signing for the plugin registry (`design/registry.md`):
//! every plugin version ships a **release record** — `<name>.release.toml`,
//! the sha256 of every file that makes up the release — and a detached
//! minisign signature over it (`<name>.release.toml.minisig`) by the
//! publisher the index names. The registry's `publishers.toml` is the
//! roster of publisher ids and public keys, and it enters the tree only
//! through review, so a node verifying a release trusts a key a human
//! admitted, never the channel that served the bytes.
//!
//! Two anchors, both required: the index's sha256 (the reviewed tree) and
//! the release record's (the publisher's signature). Either can catch a
//! substitution the other missed — a mis-reviewed index entry, or a
//! publisher key that leaked — so verification refuses unless both agree
//! with the bytes and with each other. The verification is one function
//! over bytes, shared by `inseam plugin install`, `inseam plugin verify`,
//! and the registry's CI, so the three never drift.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Files one release may list: the artifact, its two sidecars, and a
/// handful of fixtures. More is a corpus, not a plugin.
pub const RELEASE_FILES_MAX: usize = 64;

/// Publishers a roster may hold.
pub const PUBLISHERS_MAX: usize = 256;

/// The roster of publishers: who may sign what the index lists.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publishers {
    #[serde(default)]
    pub publisher: Vec<Publisher>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publisher {
    /// The id index entries name (`publisher = "inseam"`).
    pub id: String,
    /// Base64 minisign public key, as `cargo xtask plugin keygen` prints it.
    pub public_key: String,
    #[serde(default)]
    pub description: String,
}

impl Publishers {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let roster: Self = toml::from_str(raw).context("publishers.toml")?;
        if roster.publisher.len() > PUBLISHERS_MAX {
            bail!(
                "publishers.toml lists {} publishers; at most {PUBLISHERS_MAX}",
                roster.publisher.len()
            );
        }
        let mut seen = std::collections::BTreeSet::new();
        for publisher in &roster.publisher {
            if !seen.insert(publisher.id.as_str()) {
                bail!("publishers.toml lists `{}` twice", publisher.id);
            }
            minisign_verify::PublicKey::from_base64(&publisher.public_key).with_context(|| {
                format!("publisher `{}` has a malformed public key", publisher.id)
            })?;
        }
        Ok(roster)
    }

    pub fn find(&self, id: &str) -> Option<&Publisher> {
        self.publisher.iter().find(|p| p.id == id)
    }
}

/// What a publisher signs: the identity of one version and the digest of
/// every file that makes it up, keyed by path relative to the plugin
/// directory. Signing the record binds all of them with one key operation
/// (the same move the release manifest makes for binaries).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRecord {
    pub name: String,
    pub version: String,
    pub publisher: String,
    /// Relative path -> lowercase hex sha256.
    pub files: BTreeMap<String, String>,
}

impl ReleaseRecord {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        let record: Self = toml::from_str(raw).context("release record")?;
        if record.files.is_empty() {
            bail!("release record lists no files");
        }
        if record.files.len() > RELEASE_FILES_MAX {
            bail!(
                "release record lists {} files; at most {RELEASE_FILES_MAX}",
                record.files.len()
            );
        }
        for (path, digest) in &record.files {
            if !relative_path_is_confined(path) {
                bail!("release record names `{path}`, which escapes the plugin directory");
            }
            if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("release record digest for `{path}` is not a hex sha256");
            }
        }
        Ok(record)
    }

    /// The file name of the artifact: the one `.wasm` the record lists.
    pub fn artifact(&self) -> anyhow::Result<&str> {
        let artifacts: Vec<&str> = self
            .files
            .keys()
            .map(String::as_str)
            .filter(|p| p.ends_with(".wasm"))
            .collect();
        match artifacts.as_slice() {
            [one] => Ok(one),
            [] => bail!("release record lists no `.wasm` artifact"),
            many => bail!(
                "release record lists {} artifacts; a release has one",
                many.len()
            ),
        }
    }

    /// The digest the record claims for the artifact.
    pub fn artifact_sha256(&self) -> anyhow::Result<&str> {
        let artifact = self.artifact()?;
        Ok(self.files[artifact].as_str())
    }

    #[cfg(test)]
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("a release record serializes")
    }
}

/// A path relative to the plugin directory that stays inside it.
pub fn relative_path_is_confined(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && !path.contains('\\')
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Verify the signature before parsing: bytes that fail the key are never
/// handed to a parser. Returns the parsed record.
pub fn verify_release(
    record_bytes: &[u8],
    signature_text: &str,
    publisher: &Publisher,
) -> anyhow::Result<ReleaseRecord> {
    let key = minisign_verify::PublicKey::from_base64(&publisher.public_key)
        .with_context(|| format!("publisher `{}` has a malformed public key", publisher.id))?;
    let signature = minisign_verify::Signature::decode(signature_text)
        .context("the release signature is not a minisign signature")?;
    key.verify(record_bytes, &signature, false)
        .with_context(|| {
            format!(
                "the release record's signature does not verify against publisher `{}`",
                publisher.id
            )
        })?;
    let record = ReleaseRecord::parse(
        std::str::from_utf8(record_bytes).context("release record is not UTF-8")?,
    )?;
    if record.publisher != publisher.id {
        bail!(
            "the release record names publisher `{}` but was verified against `{}`",
            record.publisher,
            publisher.id
        );
    }
    Ok(record)
}

/// The cross-check between the reviewed index and the signed record: the
/// same name, the same version, the same publisher, the same artifact
/// digest. A disagreement anywhere is a refusal, not a warning.
pub fn check_record_against_index(
    record: &ReleaseRecord,
    name: &str,
    version: &str,
    publisher: &str,
    artifact_sha256: &str,
) -> anyhow::Result<()> {
    if record.name != name {
        bail!("release record is for `{}`, not `{name}`", record.name);
    }
    if record.version != version {
        bail!(
            "release record is version {}, the index says {version}",
            record.version
        );
    }
    if record.publisher != publisher {
        bail!(
            "release record names publisher `{}`, the index says `{publisher}`",
            record.publisher
        );
    }
    let signed = record.artifact_sha256()?;
    if !signed.eq_ignore_ascii_case(artifact_sha256) {
        bail!("the signed artifact digest {signed} disagrees with the index's {artifact_sha256}");
    }
    Ok(())
}

/// One fetched file against the digest the record signed for it.
pub fn check_file(record: &ReleaseRecord, path: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let Some(expected) = record.files.get(path) else {
        bail!("`{path}` is not in the signed release record");
    };
    let actual = sha256_hex(bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("`{path}` hashes to {actual}, the signed record says {expected}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> (minisign::KeyPair, Publisher) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        let publisher = Publisher {
            id: "tests".into(),
            public_key: pair.pk.to_base64(),
            description: String::new(),
        };
        (pair, publisher)
    }

    fn sign(pair: &minisign::KeyPair, bytes: &[u8]) -> String {
        minisign::sign(None, &pair.sk, std::io::Cursor::new(bytes), None, None)
            .expect("signs")
            .into_string()
    }

    fn record() -> ReleaseRecord {
        let mut files = BTreeMap::new();
        files.insert("x.wasm".into(), sha256_hex(b"wasm"));
        files.insert("x.manifest.toml".into(), sha256_hex(b"manifest"));
        ReleaseRecord {
            name: "x".into(),
            version: "1.0.0".into(),
            publisher: "tests".into(),
            files,
        }
    }

    #[test]
    fn a_signed_record_verifies_and_a_tampered_one_does_not() {
        let (pair, publisher) = keypair();
        let record = record();
        let bytes = record.to_toml().into_bytes();
        let signature = sign(&pair, &bytes);
        let verified = verify_release(&bytes, &signature, &publisher).expect("verifies");
        assert_eq!(verified, record);

        let mut tampered = record.clone();
        tampered.files.insert("x.wasm".into(), sha256_hex(b"evil"));
        assert!(verify_release(tampered.to_toml().as_bytes(), &signature, &publisher).is_err());

        let (other, _) = keypair();
        let forged = sign(&other, &bytes);
        assert!(
            verify_release(&bytes, &forged, &publisher).is_err(),
            "another key does not sign for this publisher"
        );
    }

    #[test]
    fn the_record_must_name_the_publisher_it_verified_against() {
        let (pair, publisher) = keypair();
        let mut record = record();
        record.publisher = "someone-else".into();
        let bytes = record.to_toml().into_bytes();
        assert!(verify_release(&bytes, &sign(&pair, &bytes), &publisher).is_err());
    }

    #[test]
    fn the_index_and_the_record_must_agree() {
        let record = record();
        let artifact = sha256_hex(b"wasm");
        assert!(check_record_against_index(&record, "x", "1.0.0", "tests", &artifact).is_ok());
        assert!(check_record_against_index(&record, "y", "1.0.0", "tests", &artifact).is_err());
        assert!(check_record_against_index(&record, "x", "1.0.1", "tests", &artifact).is_err());
        assert!(check_record_against_index(&record, "x", "1.0.0", "other", &artifact).is_err());
        assert!(
            check_record_against_index(&record, "x", "1.0.0", "tests", &sha256_hex(b"other"))
                .is_err()
        );
    }

    #[test]
    fn files_are_checked_against_their_signed_digests() {
        let record = record();
        assert!(check_file(&record, "x.wasm", b"wasm").is_ok());
        assert!(check_file(&record, "x.wasm", b"nope").is_err());
        assert!(check_file(&record, "unlisted.bin", b"").is_err());
    }

    #[test]
    fn records_refuse_escaping_paths_and_bad_digests() {
        assert!(
            ReleaseRecord::parse(
                "name=\"x\"\nversion=\"1\"\npublisher=\"p\"\n[files]\n\"../x.wasm\" = \"00\"\n"
            )
            .is_err()
        );
        assert!(
            ReleaseRecord::parse(
                "name=\"x\"\nversion=\"1\"\npublisher=\"p\"\n[files]\n\"x.wasm\" = \"zz\"\n"
            )
            .is_err()
        );
        assert!(
            ReleaseRecord::parse("name=\"x\"\nversion=\"1\"\npublisher=\"p\"\n[files]\n").is_err()
        );
        let fine = ReleaseRecord::parse(&record().to_toml()).expect("parses");
        assert_eq!(fine.artifact().expect("artifact"), "x.wasm");
    }

    #[test]
    fn the_roster_refuses_duplicates_and_malformed_keys() {
        let (_, publisher) = keypair();
        let roster = format!(
            "[[publisher]]\nid = \"a\"\npublic_key = \"{key}\"\n[[publisher]]\nid = \"a\"\npublic_key = \"{key}\"\n",
            key = publisher.public_key
        );
        assert!(Publishers::parse(&roster).is_err());
        assert!(Publishers::parse("[[publisher]]\nid = \"a\"\npublic_key = \"nope\"\n").is_err());
        let one = format!(
            "[[publisher]]\nid = \"a\"\npublic_key = \"{}\"\n",
            publisher.public_key
        );
        assert_eq!(
            Publishers::parse(&one)
                .expect("parses")
                .find("a")
                .map(|p| p.id.as_str()),
            Some("a")
        );
    }
}
