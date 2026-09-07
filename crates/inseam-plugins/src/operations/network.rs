//! The owner's network operations (`design/node-api.md`, `design/roster.md`)
//! and the seams they stand on. Every one is optional to mount: a node
//! composed without the network entries still runs the ladder, and each
//! network operation then answers `Unavailable` naming the entry it
//! lacks, so a transport shows the owner what to mount rather than a
//! generic failure.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use inseam_kernel::address::{HostId, Timestamp};
use inseam_kernel::network::{NodeId, NodeRecord};
use inseam_kernel::store::{IndexStore, LogCount};
use inseam_seams::dates::ymd;
use inseam_seams::node::Node;
use inseam_seams::operations::{
    ExpelRequest, JoinRequest, LogSummary, NetworkHostView, NetworkNodeView, NetworkView,
};
use inseam_seams::roster::{HostStewards, Invitation, Roster};
use inseam_seams::routing::Routing;
use inseam_seams::sync::{PeerSyncView, SyncStatus, Synchronizer};
use inseam_seams::transport::PeerAddress;
use inseam_seams::SeamError;

/// The network seams as the operations plugin injected them: any may be
/// absent on a node composed without them.
pub(crate) struct NetworkOperations {
    pub routing: Option<Arc<dyn Routing>>,
    pub roster: Option<Arc<dyn Roster>>,
    pub sync: Option<Arc<dyn Synchronizer>>,
    pub node: Option<Arc<dyn Node>>,
}

impl NetworkOperations {
    /// A node with no network entries mounted.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self {
            routing: None,
            roster: None,
            sync: None,
            node: None,
        }
    }

    pub(crate) fn roster(&self) -> Result<&Arc<dyn Roster>, SeamError> {
        self.roster.as_ref().ok_or_else(|| unavailable("roster"))
    }

    pub(crate) fn sync(&self) -> Result<&Arc<dyn Synchronizer>, SeamError> {
        self.sync.as_ref().ok_or_else(|| unavailable("sync"))
    }

    pub(crate) fn node(&self) -> Result<&Arc<dyn Node>, SeamError> {
        self.node.as_ref().ok_or_else(|| unavailable("node"))
    }

    /// The network as this node sees it: the roster's durable facts beside
    /// what the last sync learned by trying.
    pub(crate) async fn network(&self, store: &IndexStore) -> Result<NetworkView, SeamError> {
        let roster = self.roster()?;
        let sync = self.sync()?;
        let node = self.node()?;
        let nodes = roster.nodes().await?;
        let hosts = roster.hosts().await?;
        let status = sync.status().await?;
        let counts = store.log_counts(&node.id()).await?;
        Ok(network_view(roster.local(), node.id(), nodes, hosts, &status, &counts))
    }

    pub(crate) async fn invite(&self) -> Result<Invitation, SeamError> {
        self.roster()?.invite().await
    }

    /// Join through an invitation: parse it, refuse one already expired
    /// by name, dial the inviter with the token, and report the network
    /// as the first sync left it. `now` is the caller's clock so the
    /// expiry check is testable without one.
    pub(crate) async fn join(
        &self,
        store: &IndexStore,
        request: JoinRequest,
        now: Timestamp,
    ) -> Result<NetworkView, SeamError> {
        let sync = self.sync()?;
        let invitation: Invitation = request
            .invitation
            .parse()
            .map_err(|error| SeamError::Refused(format!("invitation: {error}")))?;
        if !invitation.is_open_at(now) {
            return Err(SeamError::Refused(format!(
                "invitation from node {} expired on {}; ask for a fresh one",
                invitation.node.short(),
                ymd(invitation.expires)
            )));
        }
        let peer = PeerAddress::from(invitation);
        let view = sync.sync_with(&peer).await?;
        tracing::info!(
            peer = %peer.id.short(),
            received = view.entries_received,
            sent = view.entries_sent,
            "joined through an invitation"
        );
        self.network(store).await
    }

    pub(crate) async fn expel(&self, store: &IndexStore, request: ExpelRequest) -> Result<NetworkView, SeamError> {
        self.roster()?.expel(&request.node).await?;
        self.network(store).await
    }

    pub(crate) async fn sync_now(&self, store: &IndexStore) -> Result<NetworkView, SeamError> {
        self.sync()?.sync_now().await?;
        self.network(store).await
    }
}

fn unavailable(entry: &str) -> SeamError {
    SeamError::Unavailable(format!(
        "this node has no network: the `{entry}` entry is not mounted"
    ))
}

/// Assemble the owner's view: nodes in roster order with their session
/// knowledge and stewarded hosts, hosts with their stewards, and the log
/// summed across origins.
fn network_view(
    local: NodeRecord,
    local_id: NodeId,
    nodes: Vec<NodeRecord>,
    hosts: Vec<HostStewards>,
    status: &SyncStatus,
    counts: &[LogCount],
) -> NetworkView {
    let peers: HashMap<NodeId, &PeerSyncView> = status.peers.iter().map(|p| (p.node, p)).collect();
    let mut hosts_by_node: BTreeMap<NodeId, Vec<HostId>> = BTreeMap::new();
    for entry in &hosts {
        for steward in &entry.stewards {
            hosts_by_node
                .entry(steward.node)
                .or_default()
                .push(entry.host.id.clone());
        }
    }
    let nodes = nodes
        .into_iter()
        .map(|record| {
            let is_local = record.id == local_id;
            let peer = peers.get(&record.id);
            NetworkNodeView {
                hosts: hosts_by_node.remove(&record.id).unwrap_or_default(),
                is_local,
                // This node is always live to itself; a peer is live only
                // as the last attempt found it.
                live: is_local || peer.is_some_and(|p| p.live),
                last_sync: peer.and_then(|p| p.last_success).map(ymd),
                last_error: peer.and_then(|p| p.last_error.clone()),
                record,
            }
        })
        .collect();
    let hosts = hosts
        .into_iter()
        .map(|entry| NetworkHostView {
            host: entry.host,
            stewards: entry.stewards.iter().map(|s| s.node).collect(),
        })
        .collect();
    let log = LogSummary {
        entries: counts.iter().map(|c| c.entries).sum(),
        origins: u32::try_from(counts.len()).unwrap_or(u32::MAX),
    };
    NetworkView {
        local,
        nodes,
        hosts,
        log,
    }
}

#[cfg(test)]
mod tests {
    use inseam_kernel::network::{
        Epoch, HostRecord, NodeCapabilities, Sequence, StewardCapabilities, StewardshipRecord,
        VectorEntry, VersionVector,
    };
    use inseam_seams::sync::PeerSyncView;
    use inseam_seams::transport::InvitationToken;

    use super::*;
    use crate::routing::fake::{FakeNode, FakeRoster, RosterRecords};

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn record(byte: u8) -> NodeRecord {
        NodeRecord {
            id: node(byte),
            display_name: format!("node-{byte}"),
            endpoints: Vec::new(),
            capabilities: NodeCapabilities {
                always_on: false,
                deep_index: true,
                relays: true,
            },
        }
    }

    fn host(id: &str) -> HostId {
        HostId::new(id).expect("valid host id")
    }

    /// A synchronizer that answers with a canned status and records what
    /// it was asked to do.
    struct FakeSynchronizer {
        status: SyncStatus,
        synced_with: std::sync::Mutex<Vec<NodeId>>,
        rounds: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl Synchronizer for FakeSynchronizer {
        async fn sync_now(&self) -> Result<SyncStatus, SeamError> {
            self.rounds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.status.clone())
        }

        async fn sync_with(&self, peer: &PeerAddress) -> Result<PeerSyncView, SeamError> {
            self.synced_with
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(peer.id);
            Ok(PeerSyncView {
                node: peer.id,
                live: true,
                last_success: Some(Timestamp(100)),
                last_error: None,
                entries_received: 3,
                entries_sent: 1,
            })
        }

        async fn status(&self) -> Result<SyncStatus, SeamError> {
            Ok(self.status.clone())
        }
    }

    async fn store() -> (Arc<IndexStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = IndexStore::open(dir.path()).await.expect("opens");
        (Arc::new(store), dir)
    }

    fn networked() -> (NetworkOperations, Arc<FakeSynchronizer>) {
        let records = RosterRecords::shared();
        records.add_node(record(1));
        records.add_node(record(2));
        records.add_host(HostRecord {
            id: host("fs-two"),
            kind: "fs".to_string(),
            display_name: "two".to_string(),
        });
        records.add_stewardship(StewardshipRecord {
            node: node(2),
            host: host("fs-two"),
            capabilities: StewardCapabilities {
                enumerates: true,
                change_feed: false,
                writable: false,
            },
            roots: Vec::new(),
        });
        let sync = Arc::new(FakeSynchronizer {
            status: SyncStatus {
                vector: VersionVector::new(vec![VectorEntry {
                    origin: node(2),
                    epoch: Epoch(1),
                    seq: Sequence(4),
                }]),
                peers: vec![PeerSyncView {
                    node: node(2),
                    live: false,
                    last_success: Some(Timestamp(1_420_070_400)),
                    last_error: Some("dial failed".to_string()),
                    entries_received: 4,
                    entries_sent: 0,
                }],
            },
            synced_with: std::sync::Mutex::new(Vec::new()),
            rounds: std::sync::atomic::AtomicU32::new(0),
        });
        let operations = NetworkOperations {
            routing: None,
            roster: Some(Arc::new(FakeRoster::new(record(1), records))),
            sync: Some(sync.clone()),
            node: Some(Arc::new(FakeNode::new(record(1)))),
        };
        (operations, sync)
    }

    #[tokio::test]
    async fn every_network_operation_names_the_missing_entry_without_a_network() {
        let (store, _dir) = store().await;
        let none = NetworkOperations::none();
        let expel = ExpelRequest { node: node(2) };
        let join = JoinRequest {
            invitation: "inseam-invite:abc".to_string(),
        };
        let outcomes: Vec<(&str, Result<(), SeamError>)> = vec![
            ("network", none.network(&store).await.map(drop)),
            ("invite", none.invite().await.map(drop)),
            ("join", none.join(&store, join, Timestamp(0)).await.map(drop)),
            ("expel", none.expel(&store, expel).await.map(drop)),
            ("sync_now", none.sync_now(&store).await.map(drop)),
        ];
        for (operation, outcome) in outcomes {
            match outcome {
                Err(SeamError::Unavailable(message)) => {
                    assert!(message.contains("entry is not mounted"), "{operation}: {message}");
                }
                other => panic!("{operation} should be unavailable, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_network_view_joins_roster_facts_with_session_knowledge() {
        let (store, _dir) = store().await;
        let (operations, _sync) = networked();
        let view = operations.network(&store).await.expect("view");
        assert_eq!(view.local.id, node(1));
        assert_eq!(view.nodes.len(), 2);
        let me = &view.nodes[0];
        assert!(me.is_local);
        assert!(me.live, "a node is live to itself");
        assert!(me.hosts.is_empty());
        let peer = &view.nodes[1];
        assert!(!peer.is_local);
        assert!(!peer.live);
        assert_eq!(peer.last_sync.as_deref(), Some("2015-01-01"));
        assert_eq!(peer.last_error.as_deref(), Some("dial failed"));
        assert_eq!(peer.hosts, vec![host("fs-two")]);
        assert_eq!(view.hosts.len(), 1);
        assert_eq!(view.hosts[0].stewards, vec![node(2)]);
        assert_eq!(view.log, LogSummary::default(), "an empty store holds no log");
    }

    #[tokio::test]
    async fn join_dials_the_inviter_with_the_token_and_reports_the_network() {
        let (store, _dir) = store().await;
        let (operations, sync) = networked();
        let invitation = Invitation {
            node: node(2),
            endpoints: Vec::new(),
            token: InvitationToken::new("once").expect("valid"),
            expires: Timestamp(1_000),
        };
        let request = JoinRequest {
            invitation: invitation.to_string(),
        };
        let view = operations
            .join(&store, request, Timestamp(999))
            .await
            .expect("joins");
        assert_eq!(view.nodes.len(), 2);
        assert_eq!(
            *sync.synced_with.lock().unwrap_or_else(|e| e.into_inner()),
            vec![node(2)]
        );
    }

    #[tokio::test]
    async fn join_refuses_an_expired_or_malformed_invitation_by_name() {
        let (store, _dir) = store().await;
        let (operations, sync) = networked();
        let invitation = Invitation {
            node: node(2),
            endpoints: Vec::new(),
            token: InvitationToken::new("once").expect("valid"),
            expires: Timestamp(1_000),
        };
        let expired = JoinRequest {
            invitation: invitation.to_string(),
        };
        match operations.join(&store, expired, Timestamp(1_000)).await {
            Err(SeamError::Refused(message)) => assert!(message.contains("expired"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let malformed = JoinRequest {
            invitation: "not-an-invitation".to_string(),
        };
        match operations.join(&store, malformed, Timestamp(0)).await {
            Err(SeamError::Refused(message)) => assert!(message.contains("invitation"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            sync.synced_with.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "nothing was dialed"
        );
    }

    #[tokio::test]
    async fn sync_now_runs_a_round_and_expel_publishes_through_the_roster() {
        let (store, _dir) = store().await;
        let (operations, sync) = networked();
        operations.sync_now(&store).await.expect("syncs");
        assert_eq!(sync.rounds.load(std::sync::atomic::Ordering::SeqCst), 1);
        let request = ExpelRequest { node: node(2) };
        operations.expel(&store, request).await.expect("expels");
        let roster = operations.roster().expect("mounted");
        assert!(!roster.is_admitted(&node(2)).await.expect("answers"));
    }
}
