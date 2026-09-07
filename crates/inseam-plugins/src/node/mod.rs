//! The `node` plugin: this node's own identity (`design/roster.md`), the
//! seam's provider. A node *is* its Ed25519 keypair — the public half is
//! its [`NodeId`], the dial target and the origin of every record it
//! publishes — so the key is minted exactly once, on the first apply, and
//! read back on every boot after that. It lives in
//! `<data_dir>/node/secret.key` beside the other owner-private files
//! (credential files, a minted machine id), is never configured, and never
//! syncs: the roster carries the public half and nothing else.
//!
//! The public key is derived the way the iroh transport derives it, through
//! iroh's own key type, so the id this plugin announces is byte-for-byte
//! the id a peer authenticates on the wire. The presentation — a display
//! name and the capability flags — comes from the composition; a renamed
//! node is a restarted node, exactly as the seam says.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_kernel::network::{NodeCapabilities, NodeId, DISPLAY_NAME_CHARS_MAX};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::node::{Node, SecretKeyBytes, NODE};

/// The plugin's subdirectory of the node's data directory.
pub const KEY_DIRNAME: &str = "node";
/// The key file inside it: 64 lowercase hex characters and a newline.
pub const KEY_FILENAME: &str = "secret.key";

/// Bytes in an Ed25519 secret; any 32 random bytes are a valid one.
const SECRET_BYTES: usize = 32;
/// Characters of the key file's payload: two hex digits per byte.
const SECRET_HEX_CHARS: usize = SECRET_BYTES * 2;
const _: () = assert!(SECRET_HEX_CHARS == 64, "a key file is 64 hex characters");
/// Most bytes read from a key file before it is refused as not a key file:
/// the payload, its newline, and slack for a stray carriage return.
const KEY_FILE_BYTES_MAX: usize = SECRET_HEX_CHARS + 8;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    /// What owners see in listings; the machine's hostname when unset.
    pub display_name: Option<String>,
    /// The node intends to be reachable at all times at a stable endpoint
    /// — the backbone convention. A laptop says `false`.
    pub always_on: bool,
    /// The node deep-indexes the content of the hosts it stewards.
    pub deep_index: bool,
    /// The node forwards requests for hosts it does not steward.
    pub relays: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            display_name: None,
            always_on: false,
            deep_index: true,
            relays: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("display_name may not be empty; leave it unset to use this machine's hostname")]
    EmptyDisplayName,
    #[error("display_name is longer than {DISPLAY_NAME_CHARS_MAX} characters")]
    DisplayNameTooLong,
    #[error(
        "node key file {path} is not {SECRET_HEX_CHARS} lowercase hex characters; restore it \
         from a backup, or move it aside to mint a new identity (a new identity is a new node)"
    )]
    Malformed { path: PathBuf },
    #[error("cannot read node key file {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot keep node key file {path}: {source}")]
    Unwritable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Where the key came from this boot — logged so an operator can tell a
/// first boot from a reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Read back from the data directory.
    Kept,
    /// Minted this boot and written to the data directory.
    Minted,
}

/// The seam provider: the key, the id derived from it, and the configured
/// presentation. Every answer is fixed for the life of the process.
/// `Debug` is derived because the secret redacts itself (`SecretKeyBytes`);
/// the test below pins that on this path too.
#[derive(Debug)]
pub struct Identity {
    secret: SecretKeyBytes,
    id: NodeId,
    display_name: String,
    capabilities: NodeCapabilities,
    source: KeySource,
}

impl Identity {
    /// Load the key kept under `dir`, minting one on first boot, and pair it
    /// with the configured presentation. A key file that exists but is not
    /// a key is refused by name rather than replaced: replacing it would
    /// silently mint a new node.
    pub async fn load(dir: &Path, config: &NodeConfig) -> Result<Self, IdentityError> {
        let display_name = match &config.display_name {
            Some(name) => check_display_name(name)?,
            None => hostname_display_name(),
        };
        let path = dir.join(KEY_FILENAME);
        let (secret, source) = load_or_mint_secret(&path).await?;
        let id = node_id_of(&secret);
        // The display name never exceeds the roster's bound on either path:
        // the configured one was checked, the hostname was truncated.
        assert!(display_name.chars().count() <= DISPLAY_NAME_CHARS_MAX);
        assert!(!display_name.is_empty());
        Ok(Self {
            secret,
            id,
            display_name,
            capabilities: NodeCapabilities {
                always_on: config.always_on,
                deep_index: config.deep_index,
                relays: config.relays,
            },
            source,
        })
    }

    pub fn source(&self) -> KeySource {
        self.source
    }
}

impl Node for Identity {
    fn id(&self) -> NodeId {
        self.id
    }

    fn display_name(&self) -> String {
        self.display_name.clone()
    }

    fn capabilities(&self) -> NodeCapabilities {
        self.capabilities
    }

    fn secret_key(&self) -> SecretKeyBytes {
        self.secret.clone()
    }
}

/// The id of a secret, derived through iroh's key type so it is exactly
/// the id the transport authenticates on the wire.
pub fn node_id_of(secret: &SecretKeyBytes) -> NodeId {
    let public = iroh::SecretKey::from_bytes(secret.as_bytes()).public();
    NodeId::from_bytes(*public.as_bytes())
}

fn check_display_name(name: &str) -> Result<String, IdentityError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(IdentityError::EmptyDisplayName);
    }
    if trimmed.chars().count() > DISPLAY_NAME_CHARS_MAX {
        return Err(IdentityError::DisplayNameTooLong);
    }
    Ok(trimmed.to_string())
}

/// The machine's hostname, truncated to the roster's bound; a machine
/// with no hostname at all is named after nothing — the caller's id is
/// the identity, the name is presentation only.
fn hostname_display_name() -> String {
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    let truncated: String = hostname.trim().chars().take(DISPLAY_NAME_CHARS_MAX).collect();
    if truncated.is_empty() {
        return "inseam node".to_string();
    }
    truncated
}

async fn load_or_mint_secret(path: &Path) -> Result<(SecretKeyBytes, KeySource), IdentityError> {
    match tokio::fs::read(path).await {
        Ok(raw) => {
            warn_if_loose(path).await;
            let secret = parse_key_file(&raw).ok_or_else(|| IdentityError::Malformed {
                path: path.to_path_buf(),
            })?;
            Ok((secret, KeySource::Kept))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let secret = mint_secret();
            write_key_file(path, &secret).await?;
            // Read back what was written: the file is the identity from now
            // on, so the bytes on disk must parse to the bytes in memory.
            let kept = tokio::fs::read(path).await.map_err(|source| IdentityError::Unreadable {
                path: path.to_path_buf(),
                source,
            })?;
            let reread = parse_key_file(&kept).ok_or_else(|| IdentityError::Malformed {
                path: path.to_path_buf(),
            })?;
            assert_eq!(reread.as_bytes(), secret.as_bytes(), "the key file round-trips");
            Ok((secret, KeySource::Minted))
        }
        Err(source) => Err(IdentityError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn mint_secret() -> SecretKeyBytes {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    assert!(bytes.iter().any(|b| *b != 0), "the generator produced no entropy");
    SecretKeyBytes::from_bytes(bytes)
}

/// The key file's payload as bytes: exactly 64 lowercase hex characters,
/// optionally followed by a newline. Anything else is not a key file.
fn parse_key_file(raw: &[u8]) -> Option<SecretKeyBytes> {
    if raw.len() > KEY_FILE_BYTES_MAX {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    let payload = text.trim_end_matches(['\n', '\r']);
    if payload.len() != SECRET_HEX_CHARS {
        return None;
    }
    if !payload.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut bytes = [0u8; SECRET_BYTES];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&payload[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(SecretKeyBytes::from_bytes(bytes))
}

fn render_key_file(secret: &SecretKeyBytes) -> String {
    let mut text: String = secret.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(text.len(), SECRET_HEX_CHARS);
    text.push('\n');
    text
}

/// Keep the key the way the oauth plugin keeps credential files: directory
/// private to the owner, file written whole then renamed into place so a
/// crash mid-write never leaves a half key, mode 0600.
async fn write_key_file(path: &Path, secret: &SecretKeyBytes) -> Result<(), IdentityError> {
    let unwritable = |source: io::Error| IdentityError::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    let parent = path.parent().ok_or_else(|| {
        unwritable(io::Error::other("key path has no parent directory"))
    })?;
    tokio::fs::create_dir_all(parent).await.map_err(unwritable)?;
    set_private(parent, 0o700).await.map_err(unwritable)?;
    let temporary = path.with_extension("key.tmp");
    tokio::fs::write(&temporary, render_key_file(secret))
        .await
        .map_err(unwritable)?;
    set_private(&temporary, 0o600).await.map_err(unwritable)?;
    tokio::fs::rename(&temporary, path).await.map_err(unwritable)?;
    Ok(())
}

#[cfg(unix)]
async fn set_private(path: &Path, mode: u32) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await
}

#[cfg(not(unix))]
async fn set_private(_path: &Path, _mode: u32) -> Result<(), io::Error> {
    Ok(())
}

/// A key readable by other accounts on the machine lets them be this node.
/// The oauth plugin's posture for its credential files is to keep them
/// private on write and not refuse on read; this mirrors it and says so.
#[cfg(unix)]
async fn warn_if_loose(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = tokio::fs::metadata(path).await {
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                "node key file is readable by other accounts (mode {mode:o}); chmod 600 it"
            );
        }
    }
}

#[cfg(not(unix))]
async fn warn_if_loose(_path: &Path) {}

pub struct NodePlugin {
    config: NodeConfig,
}

pub struct NodeFactory;

impl PluginFactory for NodeFactory {
    fn name(&self) -> &str {
        "node"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        let config: NodeConfig = parse_config(config)?;
        if let Some(name) = &config.display_name {
            check_display_name(name).map_err(|e| PluginError(format!("config: {e}")))?;
        }
        Ok(Box::new(NodePlugin { config }))
    }
}

#[async_trait::async_trait]
impl Plugin for NodePlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "node",
            inject: INJECT,
            provides: &["node"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let dir = cx.data_dir().join(KEY_DIRNAME);
        let identity = Identity::load(&dir, &self.config)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        tracing::info!(
            id = %identity.id().short(),
            name = %identity.display_name(),
            source = ?identity.source(),
            "node identity ready"
        );
        let facts = Facts::new()
            .with("id", identity.id().to_hex())
            .with("display_name", identity.display_name());
        cx.provide(&NODE, Arc::new(identity) as Arc<dyn Node>, facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_apply_mints_a_key_and_the_second_reuses_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = NodeConfig::default();
        let first = Identity::load(dir.path(), &config).await.expect("mints");
        assert_eq!(first.source(), KeySource::Minted);
        let second = Identity::load(dir.path(), &config).await.expect("reads back");
        assert_eq!(second.source(), KeySource::Kept);
        assert_eq!(first.id(), second.id());
        assert_eq!(first.secret_key().as_bytes(), second.secret_key().as_bytes());

        let file = dir.path().join(KEY_FILENAME);
        let text = std::fs::read_to_string(&file).expect("key file");
        assert_eq!(text.len(), SECRET_HEX_CHARS + 1);
        assert!(text.ends_with('\n'));
        assert!(text.trim_end().bytes().all(|b| b.is_ascii_hexdigit()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the key file is owner-private");
            let dir_mode = std::fs::metadata(dir.path()).expect("meta").permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "the key directory is owner-private");
        }
    }

    #[tokio::test]
    async fn two_directories_mint_two_identities() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let config = NodeConfig::default();
        let first = Identity::load(a.path(), &config).await.expect("mints");
        let second = Identity::load(b.path(), &config).await.expect("mints");
        assert_ne!(first.id(), second.id());
    }

    #[tokio::test]
    async fn a_corrupt_key_file_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(KEY_FILENAME);
        let cases: [&[u8]; 5] = [
            b"not a key\n",
            b"",
            &[b'a'; 63],
            &[b'A'; 64],
            &[0xff; 64],
        ];
        for raw in cases {
            std::fs::write(&file, raw).expect("write");
            let refused = Identity::load(dir.path(), &NodeConfig::default()).await;
            match refused {
                Err(IdentityError::Malformed { path }) => assert_eq!(path, file),
                other => panic!("{raw:?} should be refused as malformed, got {other:?}"),
            }
            assert_eq!(std::fs::read(&file).expect("read"), raw, "a corrupt key is never replaced");
        }
    }

    #[tokio::test]
    async fn a_key_file_without_a_trailing_newline_still_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(KEY_FILENAME);
        std::fs::write(&file, "ab".repeat(32)).expect("write");
        let identity = Identity::load(dir.path(), &NodeConfig::default()).await.expect("parses");
        assert_eq!(identity.secret_key().as_bytes(), &[0xab; 32]);
        assert_eq!(identity.source(), KeySource::Kept);
    }

    #[tokio::test]
    async fn the_id_is_irohs_public_key_for_the_same_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(KEY_FILENAME), format!("{}\n", "07".repeat(32))).expect("write");
        let identity = Identity::load(dir.path(), &NodeConfig::default()).await.expect("loads");
        let expected = iroh::SecretKey::from_bytes(&[0x07; 32]).public();
        assert_eq!(identity.id().as_bytes(), expected.as_bytes());
        assert_eq!(identity.id().to_hex(), expected.to_string(), "iroh renders the same hex");
        assert_eq!(node_id_of(&SecretKeyBytes::from_bytes([0x07; 32])), identity.id());
    }

    #[tokio::test]
    async fn presentation_comes_from_the_config_or_the_hostname() {
        let dir = tempfile::tempdir().expect("tempdir");
        let named = NodeConfig {
            display_name: Some("  Greg's mini  ".to_string()),
            always_on: true,
            deep_index: false,
            relays: false,
        };
        let identity = Identity::load(dir.path(), &named).await.expect("loads");
        assert_eq!(identity.display_name(), "Greg's mini");
        assert_eq!(
            identity.capabilities(),
            NodeCapabilities {
                always_on: true,
                deep_index: false,
                relays: false,
            }
        );
        let record = identity.record(Vec::new());
        assert_eq!(record.id, identity.id());
        assert_eq!(record.check_bounds(), Ok(()));

        let unnamed = Identity::load(dir.path(), &NodeConfig::default()).await.expect("loads");
        assert!(!unnamed.display_name().is_empty());
        assert!(unnamed.display_name().chars().count() <= DISPLAY_NAME_CHARS_MAX);
        assert!(unnamed.capabilities().deep_index);
        assert!(!unnamed.capabilities().always_on);
    }

    #[tokio::test]
    async fn debug_output_never_shows_the_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hex = "07".repeat(32);
        std::fs::write(dir.path().join(KEY_FILENAME), format!("{hex}\n")).expect("write");
        let identity = Identity::load(dir.path(), &NodeConfig::default()).await.expect("loads");
        let rendered = format!("{identity:?}");
        assert!(rendered.contains("<redacted>"), "the secret is redacted: {rendered}");
        assert!(!rendered.contains(&hex), "the secret never prints: {rendered}");
        assert!(rendered.contains(&identity.display_name()));
    }

    #[test]
    fn the_factory_refuses_bad_display_names_and_unknown_fields() {
        let empty: toml::Table = toml::from_str("display_name = \"  \"").expect("toml");
        assert!(NodeFactory.build(&empty).is_err());
        let long: toml::Table =
            toml::from_str(&format!("display_name = \"{}\"", "n".repeat(DISPLAY_NAME_CHARS_MAX + 1)))
                .expect("toml");
        assert!(NodeFactory.build(&long).is_err());
        let unknown: toml::Table = toml::from_str("hostname = \"x\"").expect("toml");
        assert!(NodeFactory.build(&unknown).is_err());
        let fine: toml::Table = toml::from_str("always_on = true").expect("toml");
        assert!(NodeFactory.build(&fine).is_ok());
        assert_eq!(NodeFactory.name(), "node");
    }

    #[test]
    fn key_files_round_trip_through_render_and_parse() {
        let secret = SecretKeyBytes::from_bytes(std::array::from_fn(|i| u8::try_from(i * 7 % 256).expect("fits")));
        let rendered = render_key_file(&secret);
        let parsed = parse_key_file(rendered.as_bytes()).expect("parses");
        assert_eq!(parsed.as_bytes(), secret.as_bytes());
        assert!(parse_key_file(&[b'0'; KEY_FILE_BYTES_MAX + 1]).is_none(), "oversized files are not read");
    }
}
