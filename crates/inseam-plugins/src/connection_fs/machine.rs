//! The identity material a filesystem host's id is derived from
//! (`design/addressing.md`): the machine, never its mutable hostname. The
//! platform's own machine id is the principal wherever one exists — the
//! IOPlatformUUID on macOS, `/etc/machine-id` on Linux, the registry's
//! `MachineGuid` on Windows — so two nodes on one machine mint one host
//! without coordinating. Where the platform has none (a container image
//! without systemd, a phone), the node mints a random identity once and
//! keeps it in its data directory, so the id follows the volume that *is*
//! the tenant (`design/hosted-service.md`).

use std::path::{Path, PathBuf};

use rand::RngCore;
use thiserror::Error;

/// The file a minted identity is kept in, inside the entry's own
/// subdirectory of the node's data directory.
pub const MACHINE_IDENTITY_FILENAME: &str = "machine-id";

/// Longest identity accepted from configuration or a kept file. A platform
/// id is a 32- or 36-character UUID; anything past this is a mistake, not
/// identity material.
pub const MACHINE_IDENTITY_CHARS_MAX: usize = 128;

/// Bytes of entropy in a minted identity: the same 128 bits systemd gives
/// `/etc/machine-id`, rendered as 32 hex characters.
const MINTED_IDENTITY_BYTES: usize = 16;

#[derive(Debug, Error)]
pub enum MachineIdentityError {
    #[error("`machine_id` may not be empty; leave it unset to use this machine's own id")]
    Empty,
    #[error("machine identity `{0}` is longer than {MACHINE_IDENTITY_CHARS_MAX} characters")]
    TooLong(String),
    #[error("machine identity kept at {path} is not text: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot keep a minted machine identity at {path}: {source}")]
    Unwritable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Where an identity came from — logged at registration so an operator can
/// tell a platform id from a kept or configured one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineIdentitySource {
    /// `machine_id` in the entry's config.
    Configured,
    /// The operating system's own machine id.
    Platform,
    /// A minted identity read back from the data directory.
    Kept,
    /// Minted this boot and written to the data directory.
    Minted,
}

/// The principal a filesystem host id is derived from, with its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineIdentity {
    principal: String,
    source: MachineIdentitySource,
}

impl MachineIdentity {
    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn source(&self) -> MachineIdentitySource {
        self.source
    }
}

/// The platform's own machine id, when the platform has one and it is
/// non-empty. Docker images without systemd ship no `/etc/machine-id` (or an
/// empty one), and iOS exposes none to a library; both answer `None` and
/// fall through to a kept identity.
#[cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd"
))]
pub fn platform_machine_id() -> Option<String> {
    match machine_uid::get() {
        Ok(id) => {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(error) => {
            tracing::debug!(%error, "no platform machine id; a kept identity stands in");
            None
        }
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows",
    target_os = "freebsd"
)))]
pub fn platform_machine_id() -> Option<String> {
    None
}

/// The identity to derive this host's id from, in order of precedence: the
/// configured override, the platform's machine id, then an identity minted
/// once and kept under `keep_dir`. The directory is touched only on the
/// last path — a machine that knows its own id never gains a file to drift
/// from it.
pub fn resolve(
    configured: Option<&str>,
    platform: Option<String>,
    keep_dir: &Path,
) -> Result<MachineIdentity, MachineIdentityError> {
    if let Some(configured) = configured {
        let principal = accept(configured)?;
        return Ok(MachineIdentity {
            principal,
            source: MachineIdentitySource::Configured,
        });
    }
    if let Some(platform) = platform {
        let principal = accept(&platform)?;
        return Ok(MachineIdentity {
            principal,
            source: MachineIdentitySource::Platform,
        });
    }
    let path = keep_dir.join(MACHINE_IDENTITY_FILENAME);
    match read_kept(&path)? {
        Some(principal) => Ok(MachineIdentity {
            principal,
            source: MachineIdentitySource::Kept,
        }),
        None => {
            let principal = mint();
            keep(&path, &principal)?;
            Ok(MachineIdentity {
                principal,
                source: MachineIdentitySource::Minted,
            })
        }
    }
}

/// Identity material as given, trimmed; refused when empty or absurdly long.
fn accept(raw: &str) -> Result<String, MachineIdentityError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(MachineIdentityError::Empty);
    }
    if trimmed.chars().count() > MACHINE_IDENTITY_CHARS_MAX {
        return Err(MachineIdentityError::TooLong(trimmed.to_string()));
    }
    Ok(trimmed.to_string())
}

/// The identity kept at `path`, if any. An empty file is treated as absent
/// (an interrupted first write) and is minted over; unreadable text is an
/// error, never silently a new identity.
fn read_kept(path: &Path) -> Result<Option<String>, MachineIdentityError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                accept(trimmed).map(Some)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(MachineIdentityError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write a minted identity via a sibling temporary file and rename, so a
/// crash mid-write leaves either the whole identity or none.
fn keep(path: &Path, principal: &str) -> Result<(), MachineIdentityError> {
    let unwritable = |source: std::io::Error| MachineIdentityError::Unwritable {
        path: path.to_path_buf(),
        source,
    };
    let parent = path.parent().ok_or_else(|| {
        unwritable(std::io::Error::other(
            "identity path has no parent directory",
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(unwritable)?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, format!("{principal}\n")).map_err(unwritable)?;
    std::fs::rename(&temporary, path).map_err(unwritable)?;
    Ok(())
}

fn mint() -> String {
    let mut bytes = [0u8; MINTED_IDENTITY_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    let principal: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(principal.len(), MINTED_IDENTITY_BYTES * 2);
    principal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_identity_wins_over_the_platform() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity =
            resolve(Some("  tenant-42 "), Some("platform".into()), dir.path()).expect("resolves");
        assert_eq!(identity.principal(), "tenant-42");
        assert_eq!(identity.source(), MachineIdentitySource::Configured);
        assert!(!dir.path().join(MACHINE_IDENTITY_FILENAME).exists());
    }

    #[test]
    fn the_platform_identity_is_never_kept_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = resolve(None, Some("D91D-CEDD".into()), dir.path()).expect("resolves");
        assert_eq!(identity.principal(), "D91D-CEDD");
        assert_eq!(identity.source(), MachineIdentitySource::Platform);
        assert!(!dir.path().join(MACHINE_IDENTITY_FILENAME).exists());
    }

    #[test]
    fn a_minted_identity_is_kept_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep_dir = dir.path().join("fs");
        let minted = resolve(None, None, &keep_dir).expect("mints");
        assert_eq!(minted.source(), MachineIdentitySource::Minted);
        assert_eq!(minted.principal().len(), MINTED_IDENTITY_BYTES * 2);
        assert!(minted.principal().chars().all(|c| c.is_ascii_hexdigit()));
        let kept = resolve(None, None, &keep_dir).expect("reads back");
        assert_eq!(kept.source(), MachineIdentitySource::Kept);
        assert_eq!(kept.principal(), minted.principal());
        assert!(!keep_dir.join("machine-id.tmp").exists());
    }

    #[test]
    fn an_empty_kept_file_is_minted_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MACHINE_IDENTITY_FILENAME), "\n").expect("write");
        let identity = resolve(None, None, dir.path()).expect("mints");
        assert_eq!(identity.source(), MachineIdentitySource::Minted);
    }

    #[test]
    fn rejects_an_empty_configured_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = resolve(Some("   "), None, dir.path()).expect_err("refused");
        assert!(matches!(error, MachineIdentityError::Empty));
    }

    #[test]
    fn rejects_an_overlong_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let long = "x".repeat(MACHINE_IDENTITY_CHARS_MAX + 1);
        let error = resolve(Some(&long), None, dir.path()).expect_err("refused");
        assert!(matches!(error, MachineIdentityError::TooLong(_)));
    }

    #[test]
    fn a_keep_path_under_a_file_is_an_error_not_a_fresh_id_each_boot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_where_a_dir_should_be = dir.path().join("fs");
        std::fs::write(&file_where_a_dir_should_be, "not a directory").expect("write");
        let error = resolve(None, None, &file_where_a_dir_should_be).expect_err("refused");
        assert!(
            matches!(error, MachineIdentityError::Unreadable { .. }),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_keep_dir_is_an_error_not_a_fresh_id_each_boot() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let keep_dir = dir.path().join("fs");
        std::fs::create_dir(&keep_dir).expect("mkdir");
        std::fs::set_permissions(&keep_dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let result = resolve(None, None, &keep_dir);
        std::fs::set_permissions(&keep_dir, std::fs::Permissions::from_mode(0o755))
            .expect("chmod back so the tempdir can be removed");
        let error = result.expect_err("refused");
        assert!(
            matches!(error, MachineIdentityError::Unwritable { .. }),
            "{error}"
        );
    }
}
