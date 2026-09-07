//! The network's data model (`design/roster.md`, `design/address-sync.md`):
//! node identity, the three roster record kinds, and the replicated log
//! that carries them beside the catalog. This is vocabulary only — what the
//! store persists and the sync seam ships — with no transport, no dialing,
//! and no policy. Consumers that decide what the records *mean* live above
//! the kernel, on the seams.
//!
//! One replication mechanism carries everything: every record a node
//! originates enters that node's own append-only log under a monotonic
//! sequence number, and a node's whole knowledge of the network is the set
//! of logs it has seen, summarized by a [`VersionVector`]. Within one
//! origin's log, a later entry overwrites an earlier one for the same
//! [`RecordKey`]; removals are entries like any other, so "origin wins" is
//! the only merge rule there is.
//!
//! A log is named by its origin **and an epoch**. A node whose store is
//! rebuilt — the schema converged, the data directory replaced — starts a
//! new log from sequence one, and without the epoch every peer holding the
//! old log's higher sequence numbers would ignore the new log forever. A
//! higher epoch from the same origin supersedes the older one wholesale.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::address::{Address, Envelope, HostId};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NetworkError {
    #[error("`{0}` is not a node id (64 lowercase hex characters)")]
    InvalidNodeId(String),
    #[error("endpoint `{0}` is empty or longer than {ENDPOINT_CHARS_MAX} characters")]
    InvalidEndpoint(String),
    #[error("a record carries more than {ENDPOINTS_MAX} endpoints")]
    TooManyEndpoints,
    #[error("display name is longer than {DISPLAY_NAME_CHARS_MAX} characters")]
    DisplayNameTooLong,
    #[error("`{0}` is not a record key")]
    InvalidRecordKey(String),
}

/// Longest dialing hint a node record carries; a relay URL or a socket
/// address, never prose.
pub const ENDPOINT_CHARS_MAX: usize = 256;
/// Most endpoints one node record may carry.
pub const ENDPOINTS_MAX: usize = 16;
/// Longest owner-facing name for a node or a host.
pub const DISPLAY_NAME_CHARS_MAX: usize = 128;
/// Most log entries one sync exchange ships in one message; a peer far
/// behind catches up over several rounds rather than one unbounded reply.
pub const LOG_ENTRIES_PER_BATCH_MAX: usize = 2_000;

/// A node's identity: its public key (`design/roster.md`). Identity *is*
/// the key — it survives IP rotation, re-homing, and reinstalls that keep
/// the key — and it doubles as the dial target for the transport. Rendered
/// as 64 lowercase hex characters. The kernel treats it as an opaque 32
/// bytes; that it is a valid Ed25519 point is the transport's concern.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId([u8; 32]);

impl NodeId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The leading characters an owner reads to tell nodes apart in a
    /// listing; never an identifier on its own.
    pub fn short(&self) -> String {
        self.to_hex()[..12].to_string()
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for NodeId {
    type Err = NetworkError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Strict form on both paths: exactly what `to_hex` renders, so an id
        // round-trips byte-identically through its string form.
        if s.len() != 64 {
            return Err(NetworkError::InvalidNodeId(s.to_string()));
        }
        if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(NetworkError::InvalidNodeId(s.to_string()));
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| NetworkError::InvalidNodeId(s.to_string()))?;
        }
        Ok(Self(bytes))
    }
}

impl TryFrom<String> for NodeId {
    type Error = NetworkError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<NodeId> for String {
    fn from(id: NodeId) -> String {
        id.to_hex()
    }
}

/// Which incarnation of an origin's log an entry belongs to. Minted when a
/// store first creates its log and again whenever the store is rebuilt;
/// a higher epoch from one origin supersedes every entry of a lower one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(pub u64);

/// Position within one origin's log: monotonic, starting at one, never
/// reused within an epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sequence(pub u64);

/// A dialing hint for the transport, opaque to everything else: the
/// transport plugin renders and parses its own vocabulary (a relay URL, a
/// socket address). Bounded because every node record replicates
/// everywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Endpoint(String);

impl Endpoint {
    pub fn new(endpoint: impl Into<String>) -> Result<Self, NetworkError> {
        let endpoint = endpoint.into();
        if endpoint.is_empty() {
            return Err(NetworkError::InvalidEndpoint(endpoint));
        }
        if endpoint.len() > ENDPOINT_CHARS_MAX {
            return Err(NetworkError::InvalidEndpoint(endpoint));
        }
        Ok(Self(endpoint))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Endpoint {
    type Error = NetworkError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<Endpoint> for String {
    fn from(e: Endpoint) -> String {
        e.0
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a node advertises about itself (`design/roster.md`): the facts
/// discovery fan-out and routing branch on. Every field is explicit so a
/// node states its whole contract in one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    /// The node intends to be reachable at all times at a stable endpoint —
    /// the backbone convention. A laptop says `false`.
    pub always_on: bool,
    /// The node deep-indexes the content of the hosts it stewards, so a
    /// query fanned out to it can answer from content, not envelopes only.
    pub deep_index: bool,
    /// The node is willing to forward requests for hosts it does not
    /// steward toward nodes that do.
    pub relays: bool,
}

/// Authored by the node it describes: the node is origin and sole
/// authority (`design/roster.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub display_name: String,
    /// Dialing hints; empty for an outbound-only node (a laptop behind NAT,
    /// a phone) that participates by dialing others and is never dialed.
    pub endpoints: Vec<Endpoint>,
    pub capabilities: NodeCapabilities,
}

impl NodeRecord {
    /// Bounds a record must satisfy before it enters a log or a store —
    /// checked on the originating side before write and on the receiving
    /// side before apply.
    pub fn check_bounds(&self) -> Result<(), NetworkError> {
        if self.display_name.chars().count() > DISPLAY_NAME_CHARS_MAX {
            return Err(NetworkError::DisplayNameTooLong);
        }
        if self.endpoints.len() > ENDPOINTS_MAX {
            return Err(NetworkError::TooManyEndpoints);
        }
        Ok(())
    }
}

/// Authored by any steward of the host. Two stewards derive the same host
/// id independently, so their records collide by design and merge by
/// latest. The kind is the locator-schema family (`fs`, `gmail`), a
/// validated name whose vocabulary belongs to the connection plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub id: HostId,
    pub kind: String,
    pub display_name: String,
}

impl HostRecord {
    pub fn check_bounds(&self) -> Result<(), NetworkError> {
        if self.display_name.chars().count() > DISPLAY_NAME_CHARS_MAX {
            return Err(NetworkError::DisplayNameTooLong);
        }
        Ok(())
    }
}

/// What a steward's edge to a host supports, as the stewardship record
/// publishes it: the connection's capabilities minus its credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StewardCapabilities {
    pub enumerates: bool,
    pub change_feed: bool,
    pub writable: bool,
}

/// Authored by the steward: a (node, host) claim plus the edge's published
/// capabilities. Credentials never appear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StewardshipRecord {
    pub node: NodeId,
    pub host: HostId,
    pub capabilities: StewardCapabilities,
    /// The scopes the steward indexes on this host, as its connection
    /// interprets them; what a remote owner surface offers.
    pub roots: Vec<String>,
}

/// One record in a log: a catalog entry, a roster fact, or the withdrawal
/// of either. Withdrawals are entries like any other, which is what lets
/// removals replicate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum Record {
    /// A source the origin stewards: its address and envelope, plus the raw
    /// size the catalog keeps beside every row.
    Source {
        address: Address,
        envelope: Envelope,
        raw_bytes: u64,
    },
    /// The origin no longer sees this source on its host.
    SourceGone { address: Address },
    Node(NodeRecord),
    Host(HostRecord),
    Stewardship(StewardshipRecord),
    /// The steward withdrew from the host; the host stays known.
    StewardshipWithdrawn { node: NodeId, host: HostId },
    /// A node the owner expelled: every node stops admitting it and drops
    /// its logs. Authored by whichever node the owner ran the expulsion
    /// on — the one record kind an origin publishes about another node.
    Expulsion { node: NodeId },
}

impl Record {
    /// The key a later entry overwrites an earlier one under, within one
    /// origin's log. A source and its removal share a key, as do a
    /// stewardship and its withdrawal: the latest entry is the whole truth.
    pub fn key(&self) -> RecordKey {
        match self {
            Self::Source { address, .. } | Self::SourceGone { address } => {
                RecordKey::Source(address.clone())
            }
            Self::Node(node) => RecordKey::Node(node.id),
            Self::Host(host) => RecordKey::Host(host.id.clone()),
            Self::Stewardship(s) => RecordKey::Stewardship(s.node, s.host.clone()),
            Self::StewardshipWithdrawn { node, host } => {
                RecordKey::Stewardship(*node, host.clone())
            }
            Self::Expulsion { node } => RecordKey::Expulsion(*node),
        }
    }

    /// Whether this entry withdraws what its key names.
    pub fn is_tombstone(&self) -> bool {
        matches!(
            self,
            Self::SourceGone { .. } | Self::StewardshipWithdrawn { .. }
        )
    }

    /// Bounds every record kind must satisfy, checked before write and
    /// before apply.
    pub fn check_bounds(&self) -> Result<(), NetworkError> {
        match self {
            Self::Node(node) => node.check_bounds(),
            Self::Host(host) => host.check_bounds(),
            Self::Source { .. }
            | Self::SourceGone { .. }
            | Self::Stewardship(_)
            | Self::StewardshipWithdrawn { .. }
            | Self::Expulsion { .. } => Ok(()),
        }
    }
}

/// The identity of what a record is *about*, rendered as one string the
/// store indexes: `source:<address>`, `node:<id>`, `host:<id>`,
/// `stewardship:<node>/<host>`, `expulsion:<node>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RecordKey {
    Source(Address),
    Node(NodeId),
    Host(HostId),
    Stewardship(NodeId, HostId),
    Expulsion(NodeId),
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(address) => write!(f, "source:{address}"),
            Self::Node(id) => write!(f, "node:{id}"),
            Self::Host(id) => write!(f, "host:{id}"),
            Self::Stewardship(node, host) => write!(f, "stewardship:{node}/{host}"),
            Self::Expulsion(node) => write!(f, "expulsion:{node}"),
        }
    }
}

impl FromStr for RecordKey {
    type Err = NetworkError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || NetworkError::InvalidRecordKey(s.to_string());
        let (kind, rest) = s.split_once(':').ok_or_else(invalid)?;
        match kind {
            "source" => rest.parse().map(Self::Source).map_err(|_| invalid()),
            "node" => rest.parse().map(Self::Node).map_err(|_| invalid()),
            "host" => HostId::new(rest).map(Self::Host).map_err(|_| invalid()),
            "stewardship" => {
                let (node, host) = rest.split_once('/').ok_or_else(invalid)?;
                let node = node.parse().map_err(|_| invalid())?;
                let host = HostId::new(host).map_err(|_| invalid())?;
                Ok(Self::Stewardship(node, host))
            }
            "expulsion" => rest.parse().map(Self::Expulsion).map_err(|_| invalid()),
            _ => Err(invalid()),
        }
    }
}

/// One entry as it ships between nodes: where it sits in its origin's log,
/// and what it says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub origin: NodeId,
    pub epoch: Epoch,
    pub seq: Sequence,
    pub record: Record,
}

/// The highest position seen per origin — what one node tells another so
/// the other can ship exactly the suffixes it lacks. Sorted by origin, so
/// two vectors over the same knowledge are equal.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionVector(Vec<VectorEntry>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorEntry {
    pub origin: NodeId,
    pub epoch: Epoch,
    pub seq: Sequence,
}

impl VersionVector {
    pub fn new(mut entries: Vec<VectorEntry>) -> Self {
        entries.sort_by_key(|entry| entry.origin);
        entries.dedup_by(|a, b| a.origin == b.origin);
        Self(entries)
    }

    pub fn entries(&self) -> &[VectorEntry] {
        &self.0
    }

    /// The position this vector holds for `origin`, if any.
    pub fn position_of(&self, origin: &NodeId) -> Option<(Epoch, Sequence)> {
        self.0
            .iter()
            .find(|e| e.origin == *origin)
            .map(|e| (e.epoch, e.seq))
    }

    /// Whether an entry at (`epoch`, `seq`) from `origin` is news to the
    /// holder of this vector: a newer epoch, or the known epoch past the
    /// known sequence.
    pub fn lacks(&self, origin: &NodeId, epoch: Epoch, seq: Sequence) -> bool {
        match self.position_of(origin) {
            None => true,
            Some((known_epoch, known_seq)) => {
                if epoch > known_epoch {
                    true
                } else if epoch < known_epoch {
                    false
                } else {
                    seq > known_seq
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{ContentLength, Timestamp};
    use crate::fragment::Mimetype;

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    #[test]
    fn node_id_roundtrips_through_hex() {
        let id = node(0xab);
        let hex = id.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex.parse::<NodeId>().expect("parses"), id);
        assert_eq!(id.short().len(), 12);
    }

    #[test]
    fn node_id_rejects_malformed_text() {
        for bad in ["", "abc", &"A".repeat(64), &"g".repeat(64), &"a".repeat(63)] {
            assert!(matches!(
                bad.parse::<NodeId>(),
                Err(NetworkError::InvalidNodeId(_))
            ));
        }
    }

    #[test]
    fn endpoint_is_bounded() {
        assert!(Endpoint::new("relay:https://relay.example").is_ok());
        assert!(matches!(Endpoint::new(""), Err(NetworkError::InvalidEndpoint(_))));
        let long = "x".repeat(ENDPOINT_CHARS_MAX + 1);
        assert!(matches!(Endpoint::new(long), Err(NetworkError::InvalidEndpoint(_))));
    }

    #[test]
    fn node_record_bounds_are_checked() {
        let mut record = NodeRecord {
            id: node(1),
            display_name: "laptop".to_string(),
            endpoints: Vec::new(),
            capabilities: NodeCapabilities {
                always_on: false,
                deep_index: true,
                relays: true,
            },
        };
        assert_eq!(record.check_bounds(), Ok(()));
        record.endpoints = (0..=ENDPOINTS_MAX)
            .map(|i| Endpoint::new(format!("ip:127.0.0.1:{i}")).expect("valid"))
            .collect();
        assert_eq!(record.check_bounds(), Err(NetworkError::TooManyEndpoints));
        record.endpoints.clear();
        record.display_name = "n".repeat(DISPLAY_NAME_CHARS_MAX + 1);
        assert_eq!(record.check_bounds(), Err(NetworkError::DisplayNameTooLong));
    }

    fn envelope() -> Envelope {
        Envelope {
            source_type: "file".to_string(),
            content_type: Mimetype::text_plain(),
            length: ContentLength::Lines(3),
            created: None,
            modified: None,
            observed: Timestamp(0),
            properties: Vec::new(),
            hint: None,
            content_digest: None,
        }
    }

    #[test]
    fn a_source_and_its_removal_share_a_key() {
        let address: Address = "inseam://fs-1/notes/a.md".parse().expect("valid");
        let present = Record::Source {
            address: address.clone(),
            envelope: envelope(),
            raw_bytes: 12,
        };
        let gone = Record::SourceGone {
            address: address.clone(),
        };
        assert_eq!(present.key(), gone.key());
        assert!(!present.is_tombstone());
        assert!(gone.is_tombstone());
        assert_eq!(present.key().to_string(), format!("source:{address}"));
    }

    #[test]
    fn record_keys_roundtrip_through_text() {
        let host = HostId::new("gmail-abc").expect("valid");
        let keys = vec![
            RecordKey::Source("inseam://fs-1/a/b.md".parse().expect("valid")),
            RecordKey::Node(node(2)),
            RecordKey::Host(host.clone()),
            RecordKey::Stewardship(node(3), host),
            RecordKey::Expulsion(node(4)),
        ];
        for key in keys {
            let text = key.to_string();
            assert_eq!(text.parse::<RecordKey>().expect("parses"), key, "{text}");
        }
        assert!(matches!(
            "bogus:1".parse::<RecordKey>(),
            Err(NetworkError::InvalidRecordKey(_))
        ));
    }

    #[test]
    fn record_serializes_with_a_tag() {
        let record = Record::Expulsion { node: node(5) };
        let json = serde_json::to_string(&record).expect("serializes");
        assert!(json.contains("\"record\":\"expulsion\""));
        let back: Record = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, record);
    }

    #[test]
    fn version_vector_sorts_dedups_and_answers_lacks() {
        let vector = VersionVector::new(vec![
            VectorEntry {
                origin: node(9),
                epoch: Epoch(1),
                seq: Sequence(5),
            },
            VectorEntry {
                origin: node(1),
                epoch: Epoch(2),
                seq: Sequence(3),
            },
            VectorEntry {
                origin: node(9),
                epoch: Epoch(1),
                seq: Sequence(1),
            },
        ]);
        assert_eq!(vector.entries().len(), 2);
        assert_eq!(vector.entries()[0].origin, node(1));
        assert_eq!(vector.position_of(&node(9)), Some((Epoch(1), Sequence(5))));

        assert!(vector.lacks(&node(7), Epoch(1), Sequence(1)), "unknown origin");
        assert!(vector.lacks(&node(9), Epoch(1), Sequence(6)), "past known seq");
        assert!(!vector.lacks(&node(9), Epoch(1), Sequence(5)), "already seen");
        assert!(!vector.lacks(&node(9), Epoch(1), Sequence(2)), "compacted away");
        assert!(vector.lacks(&node(9), Epoch(2), Sequence(1)), "newer epoch");
        assert!(!vector.lacks(&node(9), Epoch(0), Sequence(99)), "older epoch");
    }
}
