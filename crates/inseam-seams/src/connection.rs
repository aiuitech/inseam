//! The `connections` seam: the registry of configured edges from this node
//! to the hosts it stewards (`design/connections.md`). A node stewards many
//! hosts at once — the local filesystem, a mailbox, a chat workspace — so
//! the seam is a registry, not a single binding: every connection plugin,
//! linked or loaded, registers one [`Registration`] per host it reaches as a
//! fiber effect, and consumers (the sweep, operations) resolve a connection
//! by the host an address or a scope names. Tier is provenance, not shape:
//! the filesystem connection and a loaded Gmail connection are
//! indistinguishable to the consumer.
//!
//! A registration is exactly what the roster will publish network-wide as
//! a stewardship record — host identity, host kind, capabilities — minus the
//! credentials, which never leave the node. Change *detection* belongs here
//! too: a change feed is a declared connection capability, never a kernel
//! feature.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_kernel::address::{Address, Envelope, HostId};
use inseam_kernel::substrate::{ApplyCx, PluginError, ServiceKey};

use crate::SeamError;

pub const CONNECTIONS: ServiceKey<dyn Connections> = ServiceKey::new("connections");

/// Register a connection into the seam as a fiber effect: a connection
/// plugin's `apply` calls this once per host it stewards, and unmounting
/// the plugin unwinds the registration through the disposer the registry
/// returned. Both tiers register this way, which is what keeps them
/// indistinguishable to the sweep and to operations.
pub fn register_as_effect(
    cx: &mut ApplyCx<'_>,
    registration: Registration,
) -> Result<(), PluginError> {
    let label = format!("register connection to host {}", registration.host.id);
    let registry = cx.get(&CONNECTIONS)?;
    let disposer = registry
        .register(registration)
        .map_err(|e| PluginError(e.to_string()))?;
    cx.effect(label, disposer);
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HostKindError {
    #[error("host kind may not be empty")]
    Empty,
    #[error("host kind `{0}` may only contain lowercase ASCII letters, digits, and `-`")]
    InvalidCharacters(String),
    #[error("host kind `{0}` is longer than {HOST_KIND_CHARS_MAX} characters")]
    TooLong(String),
}

/// Longest host kind accepted; kinds are short family names (`fs`,
/// `gmail`), never descriptions.
pub const HOST_KIND_CHARS_MAX: usize = 32;

/// The locator-schema family of a host (`design/roster.md`): `fs`, `gmail`,
/// `slack`, … A validated name, not an enum — the vocabulary belongs to the
/// connection plugins, and the kind is the domain separator in host-id
/// derivation (`design/addressing.md`), so two kinds can never mint the
/// same id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostKind(String);

impl HostKind {
    pub fn new(kind: impl Into<String>) -> Result<Self, HostKindError> {
        let kind = kind.into();
        if kind.is_empty() {
            return Err(HostKindError::Empty);
        }
        if kind.len() > HOST_KIND_CHARS_MAX {
            return Err(HostKindError::TooLong(kind));
        }
        let valid = kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return Err(HostKindError::InvalidCharacters(kind));
        }
        Ok(Self(kind))
    }

    /// The local filesystem family; the one kind the first-party set ships.
    pub fn filesystem() -> Self {
        Self("fs".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostKind {
    type Error = HostKindError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<HostKind> for String {
    fn from(k: HostKind) -> String {
        k.0
    }
}

impl fmt::Display for HostKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the roster's host record carries about a host: its stable id, its
/// kind, and presentation (`design/roster.md`). Presentation lives here,
/// never in addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDescription {
    pub id: HostId,
    pub kind: HostKind,
    /// Owner-facing name ("Greg's Mac mini", "greg@example.com").
    pub display_name: String,
}

/// What an edge supports (`design/connections.md`) — the facts consumers
/// branch on, and what a stewardship record publishes. Every field is
/// explicit so a new connection states its whole contract in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The connection can list the sources under a scope. A fetch-only
    /// connection (one that serves addresses it is handed but cannot
    /// discover them) says `false` and is never swept.
    pub enumerates: bool,
    /// The connection can push change hints that schedule targeted sweeps
    /// (FSEvents, a history API). Hints are never authoritative; a full
    /// sweep still heals what a feed missed.
    pub change_feed: bool,
    /// The edge may write back to the host. Read-only edges say `false`.
    pub writable: bool,
}

impl Capabilities {
    /// The most common contract: enumerate and read, nothing more.
    pub const READ_ONLY: Self = Self {
        enumerates: true,
        change_feed: false,
        writable: false,
    };
}

/// A source found by enumeration: its address, its envelope, and the raw
/// byte size the sweep stores for change detection.
#[derive(Debug, Clone)]
pub struct EnumeratedSource {
    pub address: Address,
    pub envelope: Envelope,
    pub raw_bytes: u64,
}

/// One edge to one host: how the steward lists, reads, and scopes it.
#[async_trait::async_trait]
pub trait Connection: Send + Sync {
    /// Every enumerable source under `root` (a connection-interpreted scope:
    /// a directory path for the filesystem, a label for a mailbox).
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError>;

    /// The locator prefix that `root` covers, for reconciling vanished
    /// sources. `None` when the scope has no stable prefix (reconciliation
    /// is skipped rather than guessed).
    fn locator_prefix(&self, root: &str) -> Option<String>;

    /// Full content of a text source, lossily decoded.
    async fn read_text(&self, address: &Address) -> Result<String, SeamError>;

    /// Lines `start..=end` (1-based, inclusive) of a text source.
    async fn read_lines(&self, address: &Address, start: u64, end: u64)
    -> Result<String, SeamError>;

    /// Raw bytes of a source, for byte-wanting transforms and binary fetches.
    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError>;
}

/// One registered connection: the host it stewards, what the edge supports,
/// and the edge itself. The composition entry that registered it is the
/// identity status surfaces and the roster name.
pub struct Registration {
    pub entry_id: String,
    pub host: HostDescription,
    pub capabilities: Capabilities,
    pub connection: Arc<dyn Connection>,
}

/// The registry seam. Registration returns a disposer — the effect the
/// registering fiber accumulates — or refuses: one node holds at most one
/// connection per host, so a second registration for a host already
/// stewarded here is a composition mistake named loudly, never a silent
/// override.
pub trait Connections: Send + Sync {
    fn register(&self, registration: Registration) -> Result<Box<dyn FnOnce() + Send>, SeamError>;

    /// Every live registration, ordered by host id — deterministic
    /// regardless of activation order.
    fn snapshot(&self) -> Vec<Arc<Registration>>;

    /// The connection stewarding `host`, if this node holds one.
    fn resolve(&self, host: &HostId) -> Option<Arc<Registration>> {
        self.snapshot().into_iter().find(|r| r.host.id == *host)
    }
}

/// Pick the connection a scope names when the caller did not: the only one
/// mounted, or an error listing the choices. Explicit is the rule the
/// moment a node stewards two hosts.
pub fn resolve_default(registry: &dyn Connections) -> Result<Arc<Registration>, SeamError> {
    let mut hosts = registry.snapshot();
    match hosts.len() {
        0 => Err(SeamError::Unavailable(
            "no connection is mounted; this node stewards no host".to_string(),
        )),
        1 => Ok(hosts.remove(0)),
        _ => Err(SeamError::AmbiguousHost(
            hosts.iter().map(|r| r.host.id.clone()).collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_kind_accepts_short_lowercase_names() {
        assert_eq!(HostKind::new("gmail").expect("valid").as_str(), "gmail");
        assert_eq!(HostKind::new("fs-2").expect("valid").as_str(), "fs-2");
        assert_eq!(HostKind::filesystem().as_str(), "fs");
    }

    #[test]
    fn host_kind_rejects_empty_long_and_odd_names() {
        assert_eq!(HostKind::new(""), Err(HostKindError::Empty));
        assert!(matches!(
            HostKind::new("Gmail"),
            Err(HostKindError::InvalidCharacters(_))
        ));
        assert!(matches!(
            HostKind::new("a b"),
            Err(HostKindError::InvalidCharacters(_))
        ));
        let long = "k".repeat(HOST_KIND_CHARS_MAX + 1);
        assert!(matches!(HostKind::new(long), Err(HostKindError::TooLong(_))));
    }
}
