//! The roster plugin observed through the seam, on real kernels over the
//! fake network: what it publishes, what it withdraws, and whom it lets in.

use inseam_kernel::network::{LOG_ENTRIES_PER_BATCH_MAX, Record, VersionVector};
use inseam_seams::SeamError;
use inseam_seams::transport::{InvitationToken, PeerAddress};

use crate::sync::fake_transport::FakeNetwork;
use crate::sync::harness::{TestNode, endpoint};

/// The latest entry per key in a node's store, as a peer with an empty
/// vector would receive it.
async fn whole_log(node: &TestNode) -> Vec<Record> {
    node.store()
        .log_after(
            &node.id,
            &VersionVector::default(),
            LOG_ENTRIES_PER_BATCH_MAX,
        )
        .await
        .expect("lists")
        .into_iter()
        .map(|entry| entry.record)
        .collect()
}

#[tokio::test]
async fn publishes_node_host_and_stewardship_at_apply() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-a"]).await;
    let roster = a.roster();

    let local = roster.local();
    assert_eq!(local.id, a.id);
    assert_eq!(local.display_name, "node-0a");
    assert_eq!(local.endpoints, vec![endpoint("ip:10.0.0.1:1")]);
    assert_eq!(roster.nodes().await.expect("reads"), vec![local]);

    let hosts = roster.hosts().await.expect("reads");
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].host.id.as_str(), "fs-a");
    assert_eq!(hosts[0].host.kind, "fs");
    assert_eq!(hosts[0].host.display_name, "fs-a display");
    assert_eq!(hosts[0].stewards.len(), 1);
    assert_eq!(hosts[0].stewards[0].node, a.id);
    assert_eq!(hosts[0].stewards[0].roots, vec!["notes"]);
    assert!(hosts[0].stewards[0].capabilities.enumerates);

    let log = whole_log(&a).await;
    assert_eq!(log.len(), 3, "one node record, one host, one stewardship");
    assert!(roster.is_admitted(&a.id).await.expect("reads"));
}

#[tokio::test]
async fn a_vanished_registration_publishes_a_withdrawal() {
    let network = FakeNetwork::new();
    let mut a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-a"]).await;
    let changed = a.expect_roster_change();

    a.steward(&[]).await;
    changed.wait().await;

    let roster = a.roster();
    let hosts = roster.hosts().await.expect("reads");
    assert_eq!(hosts.len(), 1, "the host stays known");
    assert!(hosts[0].stewards.is_empty(), "nobody stewards it anymore");
    let log = whole_log(&a).await;
    let withdrawn = log
        .iter()
        .filter(|record| matches!(record, Record::StewardshipWithdrawn { .. }))
        .count();
    assert_eq!(withdrawn, 1);
    assert!(
        !log.iter()
            .any(|record| matches!(record, Record::Stewardship(_))),
        "the withdrawal compacted the claim away"
    );
}

#[tokio::test]
async fn an_unchanged_registration_is_not_republished() {
    let network = FakeNetwork::new();
    let mut a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-a"]).await;
    let before = a
        .store()
        .version_vector(&a.id)
        .await
        .expect("reads")
        .position_of(&a.id)
        .expect("the local log has entries")
        .1;
    let changed = a.expect_roster_change();

    a.steward(&["fs-a", "fs-b"]).await;
    changed.wait().await;

    let after = a
        .store()
        .version_vector(&a.id)
        .await
        .expect("reads")
        .position_of(&a.id)
        .expect("the local log has entries")
        .1;
    assert_eq!(
        after.0,
        before.0 + 2,
        "only the new host's record and claim were appended"
    );
    let log = whole_log(&a).await;
    let stewardships = log
        .iter()
        .filter(|record| matches!(record, Record::Stewardship(_)))
        .count();
    assert_eq!(stewardships, 2);
}

#[tokio::test]
async fn invite_then_redeem_admits_once() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let roster = a.roster();
    let peer = crate::sync::harness::node_id(0x0b);

    let invitation = roster.invite().await.expect("mints");
    assert_eq!(invitation.node, a.id);
    assert_eq!(invitation.endpoints, vec![endpoint("ip:10.0.0.1:1")]);
    assert!(roster.redeem(&peer, &invitation.token));
    assert!(
        !roster.redeem(&peer, &invitation.token),
        "spent on first redeem"
    );
    let forged = InvitationToken::new("forged").expect("valid");
    assert!(!roster.redeem(&peer, &forged));
}

#[tokio::test]
async fn an_invitation_admits_over_the_transport_once_and_only_once() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;
    let c = TestNode::boot(&network, 0x0c, &[], &[]).await;
    let invitation = a.roster().invite().await.expect("mints");

    let joined = b
        .sync()
        .sync_with(&PeerAddress::from(invitation.clone()))
        .await
        .expect("the invited node joins");
    assert!(joined.live);
    assert!(network.has_session(a.id, b.id));
    assert!(
        a.roster().is_admitted(&b.id).await.expect("reads"),
        "b's record arrived"
    );

    let reused = c.sync().sync_with(&PeerAddress::from(invitation)).await;
    assert!(matches!(reused, Err(SeamError::NotAdmitted(id)) if id == c.id));
    assert!(!network.has_session(a.id, c.id));
    assert!(!a.roster().is_admitted(&c.id).await.expect("reads"));
}

#[tokio::test]
async fn an_unknown_peer_without_a_token_is_refused() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &[]).await;

    let refused = b.sync().sync_with(&a.address()).await;
    assert!(matches!(refused, Err(SeamError::NotAdmitted(id)) if id == b.id));
    assert!(!network.has_session(a.id, b.id));
    let status = b.sync().status().await.expect("reads");
    assert_eq!(status.peers.len(), 1);
    assert!(!status.peers[0].live);
    assert!(status.peers[0].last_error.is_some());
}

#[tokio::test]
async fn an_expelled_peer_is_refused_and_disconnected() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let b = TestNode::boot(&network, 0x0b, &["ip:10.0.0.2:1"], &[]).await;
    let invitation = a.roster().invite().await.expect("mints");
    b.sync()
        .sync_with(&PeerAddress::from(invitation))
        .await
        .expect("joins");
    assert!(network.has_session(a.id, b.id));
    let changed = a.expect_roster_change();

    a.roster().expel(&b.id).await.expect("expels");
    changed.wait().await;

    assert_eq!(network.disconnected_by(a.id), vec![b.id]);
    assert!(!network.has_session(a.id, b.id));
    assert!(!a.roster().is_admitted(&b.id).await.expect("reads"));
    assert!(
        !a.roster()
            .nodes()
            .await
            .expect("reads")
            .iter()
            .any(|n| n.id == b.id)
    );
    let again = b.sync().sync_with(&a.address()).await;
    assert!(matches!(again, Err(SeamError::NotAdmitted(id)) if id == b.id));
}

#[tokio::test]
async fn expelling_self_is_refused() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    let refused = a.roster().expel(&a.id).await;
    assert!(matches!(refused, Err(SeamError::Refused(_))));
    assert!(a.roster().is_admitted(&a.id).await.expect("reads"));
}

#[tokio::test]
async fn republish_carries_the_rotated_endpoints() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &[]).await;
    network.set_endpoints(a.id, vec![endpoint("ip:10.0.0.9:1")]);
    let changed = a.expect_roster_change();

    let record = a.roster().republish().await.expect("republishes");
    changed.wait().await;

    assert_eq!(record.endpoints, vec![endpoint("ip:10.0.0.9:1")]);
    assert_eq!(a.roster().local(), record);
    let stored = a
        .roster()
        .node(&a.id)
        .await
        .expect("reads")
        .expect("present");
    assert_eq!(stored.endpoints, record.endpoints);
    let log = whole_log(&a).await;
    let node_records = log
        .iter()
        .filter(|record| matches!(record, Record::Node(_)))
        .count();
    assert_eq!(
        node_records, 1,
        "the log compacts to the latest node record"
    );
}

#[tokio::test]
async fn hosts_join_every_steward_across_nodes() {
    let network = FakeNetwork::new();
    let a = TestNode::boot(&network, 0x0a, &["ip:10.0.0.1:1"], &["fs-shared"]).await;
    let b = TestNode::boot(&network, 0x0b, &[], &["fs-shared", "fs-b"]).await;
    let invitation = a.roster().invite().await.expect("mints");
    b.sync()
        .sync_with(&PeerAddress::from(invitation))
        .await
        .expect("joins");

    let hosts = b.roster().hosts().await.expect("reads");
    let ids: Vec<&str> = hosts.iter().map(|h| h.host.id.as_str()).collect();
    assert_eq!(ids, vec!["fs-b", "fs-shared"]);
    let shared = &hosts[1];
    let stewards: Vec<_> = shared.stewards.iter().map(|s| s.node).collect();
    assert_eq!(
        stewards,
        vec![a.id, b.id],
        "two stewards of one host, by node"
    );
    assert_eq!(
        b.roster()
            .stewards_of(&hosts[0].host.id)
            .await
            .expect("reads")
            .len(),
        1
    );
}
