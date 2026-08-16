//! The `connection` seam: a configured edge to one host — enumerate sources,
//! extract envelopes, serve fetches (`design/connections.md`). Providers:
//! the filesystem connection (linked), service connections like Gmail or
//! Slack (community, loaded). Change *detection* belongs here too: a
//! change feed is a connection capability, never a kernel feature.

use inseam_kernel::address::{Address, Envelope, HostId};
use inseam_kernel::substrate::ServiceKey;

use crate::SeamError;

pub const CONNECTION: ServiceKey<dyn Connection> = ServiceKey::new("connection");

/// Capability-fact keys consumers may branch on.
pub mod facts {
    /// bool: the connection can push change hints (feeds targeted sweeps).
    pub const CHANGE_FEED: &str = "change_feed";
    /// string: the host id this connection stewards.
    pub const HOST: &str = "host";
}

/// A source found by enumeration: its address, its envelope, and the raw
/// byte size the sweep stores for change detection.
#[derive(Debug, Clone)]
pub struct EnumeratedSource {
    pub address: Address,
    pub envelope: Envelope,
    pub raw_bytes: u64,
}

#[async_trait::async_trait]
pub trait Connection: Send + Sync {
    /// The host this connection stewards.
    fn host(&self) -> &HostId;

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
