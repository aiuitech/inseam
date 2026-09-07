//! The `sync` seam: catalog and roster replication across node
//! connections (`design/address-sync.md`). One mechanism carries every
//! record kind: each node's own append-only log ships as suffixes the peer
//! lacks, chosen by exchanging version vectors, and origin wins per key.
//! The trait is [`Synchronizer`] — a trait can never be named `Sync`.
//!
//! The seam exposes the two moments a consumer drives — a round with every
//! dialable node, and one exchange with one named peer, which is how a
//! join presents its invitation — and the status both leave behind.
//! Liveness in the status is what the last attempt learned, not a synced
//! fact.

use serde::{Deserialize, Serialize};

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::{NodeId, VersionVector};
use inseam_kernel::substrate::ServiceKey;

use crate::transport::PeerAddress;
use crate::SeamError;

pub const SYNC: ServiceKey<dyn Synchronizer> = ServiceKey::new("sync");

/// Most peers one round exchanges with; a roster larger than this is
/// covered over successive rounds, never one unbounded fan-out.
pub const PEERS_PER_ROUND_MAX: usize = 32;

/// What this node last learned about one peer by trying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSyncView {
    pub node: NodeId,
    /// The most recent exchange succeeded, or a session with the peer is
    /// open right now.
    pub live: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Log entries taken from the peer, in all.
    pub entries_received: u64,
    /// Log entries shipped to the peer, in all.
    pub entries_sent: u64,
}

/// Where this node's knowledge stands: its own version vector and every
/// peer it has tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    pub vector: VersionVector,
    pub peers: Vec<PeerSyncView>,
}

#[async_trait::async_trait]
pub trait Synchronizer: Send + Sync {
    /// One exchange with every dialable roster node now, bounded by
    /// [`PEERS_PER_ROUND_MAX`], returning the status afterwards.
    async fn sync_now(&self) -> Result<SyncStatus, SeamError>;

    /// One exchange with one peer — how a join presents its invitation,
    /// and how an owner pokes one node.
    async fn sync_with(&self, peer: &PeerAddress) -> Result<PeerSyncView, SeamError>;

    async fn status(&self) -> Result<SyncStatus, SeamError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::network::{Epoch, Sequence, VectorEntry};

    #[test]
    fn sync_status_roundtrips_through_serde() {
        let status = SyncStatus {
            vector: VersionVector::new(vec![VectorEntry {
                origin: NodeId::from_bytes([2; 32]),
                epoch: Epoch(1),
                seq: Sequence(4),
            }]),
            peers: vec![PeerSyncView {
                node: NodeId::from_bytes([2; 32]),
                live: true,
                last_success: Some(Timestamp(100)),
                last_error: None,
                entries_received: 4,
                entries_sent: 1,
            }],
        };
        let json = serde_json::to_value(&status).expect("serializes");
        assert!(json["peers"][0].get("last_error").is_none());
        assert_eq!(json["peers"][0]["last_success"], 100);
        let back: SyncStatus = serde_json::from_value(json).expect("parses");
        assert_eq!(back, status);
    }
}
