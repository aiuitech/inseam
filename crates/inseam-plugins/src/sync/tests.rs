//! Replication observed through the seam, on real kernels and stores over
//! the fake network: convergence, tombstones, transitive spread, epoch
//! supersession, the batch bound, peer choice, and the change event.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use inseam_kernel::network::{
    Endpoint, Epoch, LOG_ENTRIES_PER_BATCH_MAX, LogEntry, NodeCapabilities, NodeRecord, Record,
    Sequence, VersionVector,
};
use inseam_seams::SeamError;
use inseam_seams::roster::RosterChanged;
use inseam_seams::transport::PeerAddress;

use super::fake_transport::FakeNetwork;
use super::harness::{TestNode, address, envelope, node_id};
use super::{SyncRequest, protocol_name};

/// Join `joiner` to `inviter` with a fresh invitation.
async fn join(inviter: &TestNode, joiner: &TestNode) {
    let invitation = inviter.roster().invite().await.expect("mints");
    joiner
        .sync()
        .sync_with(&PeerAddress::from(invitation))
        .await
        .expect("joins");
}

fn node_record(
    id: inseam_kernel::network::NodeId,
    name: &str,
    endpoints: Vec<Endpoint>,
) -> NodeRecord {
    NodeRecord {
        id,
        display_name: name.to_string(),
        endpoints,
        capabilities: NodeCapabilities {
            always_on: false,
            deep_index: false,
            relays: false,
        },
    }
}

#[tokio::test]
async fn two_nodes_converge_on_each_others_catalogs() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-a"]).await;
    let b = TestNode::boot(&network, 0x0b, &["ip:10.0.0.2:1"], &["fs-b"]).await;
    let a_note = address("inseam://fs-a/notes/a.md");
    let b_note = address("inseam://fs-b/notes/b.md");
    a.store()
        .upsert_source(&a_note, &envelope(1), 10)
        .await
        .expect("catalogs");
    b.store()
        .upsert_source(&b_note, &envelope(2), 20)
        .await
        .expect("catalogs");

    join(&a, &b).await;

    let learned = b
        .store()
        .source_by_address(&a_note)
        .await
        .expect("reads")
        .expect("b learned a's note");
    assert_eq!(learned.origin, Some(a.id));
    assert_eq!(learned.envelope, envelope(1));
    let learned = a
        .store()
        .source_by_address(&b_note)
        .await
        .expect("reads")
        .expect("a learned b's note");
    assert_eq!(learned.origin, Some(b.id));
    assert_eq!(
        a.roster()
            .node(&b.id)
            .await
            .expect("reads")
            .expect("a knows b")
            .display_name,
        "node-0b"
    );
    assert_eq!(
        b.roster()
            .node(&a.id)
            .await
            .expect("reads")
            .expect("b knows a")
            .display_name,
        "node-0a"
    );
    assert_eq!(
        a.store().version_vector(&a.id).await.expect("reads"),
        b.store().version_vector(&b.id).await.expect("reads"),
        "both hold the same logs at the same positions"
    );
    let status = b.sync().status().await.expect("reads");
    assert_eq!(status.peers.len(), 1);
    assert_eq!(status.peers[0].node, a.id);
    assert!(status.peers[0].live);
    assert!(
        status.peers[0].entries_received >= 4,
        "a's node, host, claim, and note"
    );
    assert!(
        status.peers[0].entries_sent >= 4,
        "b's node, host, claim, and note"
    );
}

#[tokio::test]
async fn tombstones_propagate() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-a"]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;
    let a_note = address("inseam://fs-a/notes/a.md");
    let id = a
        .store()
        .upsert_source(&a_note, &envelope(1), 10)
        .await
        .expect("catalogs");
    join(&a, &b).await;
    assert!(
        b.store()
            .source_by_address(&a_note)
            .await
            .expect("reads")
            .is_some()
    );

    a.store().delete_source(id).await.expect("deletes");
    b.sync().sync_with(&a.address()).await.expect("syncs again");

    assert!(
        b.store()
            .source_by_address(&a_note)
            .await
            .expect("reads")
            .is_none(),
        "b forgot it"
    );
}

#[tokio::test]
async fn records_propagate_transitively_with_their_origin() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-a"]).await;
    let b = TestNode::boot(&network, 0x0b, &["ip:10.0.0.2:1"], &[]).await;
    let c = TestNode::boot(&network, 0x0c, &[], &[]).await;
    let a_note = address("inseam://fs-a/notes/a.md");
    a.store()
        .upsert_source(&a_note, &envelope(1), 10)
        .await
        .expect("catalogs");
    join(&a, &b).await;

    join(&b, &c).await;

    let learned = c
        .store()
        .source_by_address(&a_note)
        .await
        .expect("reads")
        .expect("c learned a's note");
    assert_eq!(
        learned.origin,
        Some(a.id),
        "the origin is a, not the relay b"
    );
    assert!(c.roster().node(&a.id).await.expect("reads").is_some());
    assert!(!network.has_session(a.id, c.id), "a and c never met");
    let stewards = c.roster().stewards_of(&a_note.host).await.expect("reads");
    assert_eq!(stewards.len(), 1);
    assert_eq!(stewards[0].node, a.id);
}

#[tokio::test]
async fn a_newer_epoch_replaces_a_stale_copy_of_a_log() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;
    let stale_epoch = Epoch(a.store().log_epoch().0 - 1);
    let stale_note = address("inseam://fs-a/notes/stale.md");
    let stale = vec![
        LogEntry {
            origin: a.id,
            epoch: stale_epoch,
            seq: Sequence(1),
            record: Record::Node(node_record(a.id, "before-rebuild", Vec::new())),
        },
        LogEntry {
            origin: a.id,
            epoch: stale_epoch,
            seq: Sequence(2),
            record: Record::Source {
                address: stale_note.clone(),
                envelope: envelope(1),
                raw_bytes: 10,
            },
        },
    ];
    b.store()
        .apply_remote(&b.id, &stale)
        .await
        .expect("applies");
    assert!(
        b.store()
            .source_by_address(&stale_note)
            .await
            .expect("reads")
            .is_some()
    );

    join(&a, &b).await;

    assert!(
        b.store()
            .source_by_address(&stale_note)
            .await
            .expect("reads")
            .is_none(),
        "the old epoch was purged"
    );
    let current = b
        .roster()
        .node(&a.id)
        .await
        .expect("reads")
        .expect("a is known");
    assert_eq!(current.display_name, "node-0a");
    let held = b.store().version_vector(&b.id).await.expect("reads");
    assert_eq!(
        held.position_of(&a.id).map(|(epoch, _)| epoch),
        Some(a.store().log_epoch())
    );
}

#[tokio::test]
async fn a_batch_beyond_the_bound_is_refused_by_the_handler() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;
    join(&a, &b).await;
    let entries: Vec<LogEntry> = (1..=LOG_ENTRIES_PER_BATCH_MAX + 1)
        .map(|seq| LogEntry {
            origin: b.id,
            epoch: Epoch(1),
            seq: Sequence(u64::try_from(seq).expect("fits")),
            record: Record::Expulsion {
                node: node_id(0xee),
            },
        })
        .collect();
    let body = serde_json::to_vec(&SyncRequest {
        vector: VersionVector::default(),
        entries,
    })
    .expect("encodes");

    let refused = b
        .transport()
        .request(&a.address(), &protocol_name(), body, Duration::from_secs(5))
        .await;

    assert!(matches!(refused, Err(SeamError::Refused(_))), "{refused:?}");
    assert!(
        a.store().expelled().await.expect("reads").is_empty(),
        "nothing was applied"
    );
}

#[tokio::test]
async fn sync_now_skips_self_and_undialable_nodes() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;
    join(&a, &b).await;
    let c = node_id(0x0c);
    a.store()
        .apply_remote(
            &a.id,
            &[LogEntry {
                origin: c,
                epoch: Epoch(1),
                seq: Sequence(1),
                record: Record::Node(node_record(c, "unreachable", Vec::new())),
            }],
        )
        .await
        .expect("applies");
    assert!(a.roster().node(&c).await.expect("reads").is_some());

    let status = a.sync().sync_now().await.expect("rounds");

    let tried: Vec<_> = status.peers.iter().map(|p| p.node).collect();
    assert_eq!(
        tried,
        vec![b.id],
        "b through its session; not a itself, not c"
    );
    assert!(status.peers[0].live);
    assert!(status.peers[0].last_success.is_some());
}

#[tokio::test]
async fn sync_now_dials_always_on_nodes_and_records_failures() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot_with(&network, 0x0b, &["ip:10.0.0.2:1"], &[], true).await;
    let c = TestNode::boot(&network, 0x0c, &["ip:10.0.0.3:1"], &[]).await;
    // c joins b first, then a joins b: a learns c's record through b, but c
    // never hears of a, so a's round can dial c and be refused as a stranger.
    join(&b, &c).await;
    join(&a, &b).await;
    assert!(!network.has_session(a.id, c.id));

    let status = a.sync().sync_now().await.expect("rounds");

    let mut views = status.peers.clone();
    views.sort_by_key(|view| view.node);
    let tried: Vec<_> = views.iter().map(|view| view.node).collect();
    assert_eq!(tried, vec![b.id, c.id]);
    let b_view = views
        .iter()
        .find(|view| view.node == b.id)
        .expect("tried b");
    assert!(b_view.live);
    let c_view = views
        .iter()
        .find(|view| view.node == c.id)
        .expect("tried c");
    assert!(!c_view.live, "c does not know a and refused the dial");
    assert!(c_view.last_error.is_some());
    assert!(
        a.roster().node(&c.id).await.expect("reads").is_some(),
        "a still knows c through b"
    );
}

#[tokio::test]
async fn roster_changed_fires_after_applying_a_roster_record() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;
    let fired = Arc::new(AtomicU64::new(0));
    let counting = Arc::clone(&fired);
    let _subscription = b.bus().on::<RosterChanged>(move |_| {
        counting.fetch_add(1, Ordering::SeqCst);
    });

    join(&a, &b).await;
    assert_eq!(fired.load(Ordering::SeqCst), 1, "a's node record arrived");

    b.sync().sync_with(&a.address()).await.expect("syncs again");
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "nothing new applied, nothing announced"
    );

    let note = address("inseam://fs-a/notes/a.md");
    a.store()
        .upsert_source(&note, &envelope(1), 10)
        .await
        .expect("catalogs");
    b.sync().sync_with(&a.address()).await.expect("syncs again");
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "a catalog entry is not a roster change"
    );
}

#[tokio::test]
async fn sync_with_self_is_refused() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let refused = a.sync().sync_with(&a.address()).await;
    assert!(matches!(refused, Err(SeamError::Refused(_))));
    assert!(a.sync().status().await.expect("reads").peers.is_empty());
}
