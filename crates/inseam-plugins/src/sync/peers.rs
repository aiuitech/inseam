//! Which peers one round exchanges with (`design/network.md`): every
//! roster node this node can dial, plus every peer holding a live session
//! with it that it could not dial — the backbone reaching an outbound-only
//! laptop through the session the laptop opened. Always-on nodes go first,
//! since a round the bound cuts short should still reach the backbone.
//! Pure: the roster, the sessions, and the expulsions are arguments.

use std::collections::HashSet;

use inseam_kernel::network::{NodeId, NodeRecord};
use inseam_seams::sync::PEERS_PER_ROUND_MAX;
use inseam_seams::transport::{PeerAddress, SessionView};

/// The peers of one round, at most `limit` of them, always-on first.
pub(super) fn choose_peers(
    local: NodeId,
    nodes: &[NodeRecord],
    sessions: &[SessionView],
    expelled: &HashSet<NodeId>,
    limit: usize,
) -> Vec<PeerAddress> {
    assert!(
        limit > 0,
        "a round with no peers is a round nobody asked for"
    );
    assert!(limit <= PEERS_PER_ROUND_MAX);
    let mut chosen: Vec<(bool, PeerAddress)> = Vec::new();
    for record in nodes {
        if record.id == local || expelled.contains(&record.id) {
            continue;
        }
        if record.endpoints.is_empty() {
            continue;
        }
        chosen.push((record.capabilities.always_on, PeerAddress::from(record)));
    }
    for session in sessions {
        if session.peer == local || expelled.contains(&session.peer) {
            continue;
        }
        if chosen.iter().any(|(_, peer)| peer.id == session.peer) {
            continue;
        }
        let always_on = nodes
            .iter()
            .find(|record| record.id == session.peer)
            .is_some_and(|record| record.capabilities.always_on);
        chosen.push((
            always_on,
            PeerAddress {
                id: session.peer,
                endpoints: Vec::new(),
                invitation: None,
            },
        ));
    }
    // A stable sort keeps roster order within each group.
    chosen.sort_by_key(|(always_on, _)| !always_on);
    chosen.truncate(limit);
    assert!(chosen.len() <= limit);
    chosen.into_iter().map(|(_, peer)| peer).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::Timestamp;
    use inseam_kernel::network::{Endpoint, NodeCapabilities};
    use inseam_seams::transport::SessionDirection;

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn record(id: NodeId, endpoints: usize, always_on: bool) -> NodeRecord {
        NodeRecord {
            id,
            display_name: id.short(),
            endpoints: (0..endpoints)
                .map(|i| Endpoint::new(format!("ip:10.0.0.{i}:1")).expect("valid"))
                .collect(),
            capabilities: NodeCapabilities {
                always_on,
                deep_index: false,
                relays: false,
            },
        }
    }

    fn session(peer: NodeId) -> SessionView {
        SessionView {
            peer,
            direction: SessionDirection::Inbound,
            since: Timestamp(0),
            last_used: Timestamp(0),
        }
    }

    #[test]
    fn skips_self_the_expelled_and_the_undialable() {
        let local = node(1);
        let nodes = vec![
            record(local, 1, true),
            record(node(2), 1, false),
            record(node(3), 0, false),
            record(node(4), 1, false),
        ];
        let expelled: HashSet<NodeId> = [node(4)].into_iter().collect();
        let peers = choose_peers(local, &nodes, &[], &expelled, PEERS_PER_ROUND_MAX);
        let ids: Vec<NodeId> = peers.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![node(2)]);
        assert!(peers[0].invitation.is_none());
    }

    #[test]
    fn a_session_reaches_a_peer_with_no_endpoints_once() {
        let local = node(1);
        let nodes = vec![record(node(2), 0, false), record(node(3), 1, false)];
        let sessions = vec![session(node(2)), session(node(2)), session(node(3))];
        let peers = choose_peers(
            local,
            &nodes,
            &sessions,
            &HashSet::new(),
            PEERS_PER_ROUND_MAX,
        );
        let ids: Vec<NodeId> = peers.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![node(3), node(2)]);
        assert!(
            peers[1].endpoints.is_empty(),
            "the session, not a dial, reaches it"
        );
        assert_eq!(
            peers[0].endpoints.len(),
            1,
            "a roster peer keeps its endpoints"
        );
    }

    #[test]
    fn always_on_nodes_come_first_and_the_bound_cuts_the_rest() {
        let local = node(1);
        let nodes = vec![
            record(node(2), 1, false),
            record(node(3), 1, true),
            record(node(4), 1, false),
        ];
        let peers = choose_peers(local, &nodes, &[session(node(9))], &HashSet::new(), 2);
        let ids: Vec<NodeId> = peers.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![node(3), node(2)]);
    }

    #[test]
    fn an_expelled_session_peer_is_not_dialed_back() {
        let local = node(1);
        let expelled: HashSet<NodeId> = [node(5)].into_iter().collect();
        let peers = choose_peers(local, &[], &[session(node(5))], &expelled, 4);
        assert!(peers.is_empty());
    }
}
