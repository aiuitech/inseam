//! What this node last learned about each peer by trying — the status the
//! seam reports. Liveness here is local knowledge: the last exchange
//! succeeded, or a session with the peer is open right now; never a synced
//! fact. Bounded, and pure over the clock and the session set.

use std::collections::HashSet;

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::NodeId;
use inseam_seams::sync::PeerSyncView;

/// Most peers remembered. A roster is tens of nodes and a join names one
/// more; past the bound the peer least recently tried is forgotten.
pub const PEER_VIEWS_MAX: usize = 1_024;

struct PeerLedger {
    node: NodeId,
    attempted: Timestamp,
    last_ok: bool,
    last_success: Option<Timestamp>,
    last_error: Option<String>,
    entries_received: u64,
    entries_sent: u64,
}

impl PeerLedger {
    fn new(node: NodeId, attempted: Timestamp) -> Self {
        Self {
            node,
            attempted,
            last_ok: false,
            last_success: None,
            last_error: None,
            entries_received: 0,
            entries_sent: 0,
        }
    }

    fn view(&self, session_open: bool) -> PeerSyncView {
        PeerSyncView {
            node: self.node,
            live: self.last_ok || session_open,
            last_success: self.last_success,
            last_error: self.last_error.clone(),
            entries_received: self.entries_received,
            entries_sent: self.entries_sent,
        }
    }
}

#[derive(Default)]
pub(super) struct PeerLedgers {
    ledgers: Vec<PeerLedger>,
}

impl PeerLedgers {
    pub(super) fn record_success(
        &mut self,
        node: NodeId,
        now: Timestamp,
        received: u64,
        sent: u64,
    ) {
        let ledger = self.ledger_mut(node, now);
        ledger.last_ok = true;
        ledger.last_success = Some(now);
        ledger.last_error = None;
        ledger.entries_received = ledger.entries_received.saturating_add(received);
        ledger.entries_sent = ledger.entries_sent.saturating_add(sent);
    }

    pub(super) fn record_error(&mut self, node: NodeId, now: Timestamp, error: String) {
        let ledger = self.ledger_mut(node, now);
        ledger.last_ok = false;
        ledger.last_error = Some(error);
    }

    pub(super) fn view(&self, node: &NodeId, session_open: bool) -> Option<PeerSyncView> {
        self.ledgers
            .iter()
            .find(|ledger| ledger.node == *node)
            .map(|ledger| ledger.view(session_open))
    }

    /// Every peer tried, ordered by node id.
    pub(super) fn views(&self, sessions: &HashSet<NodeId>) -> Vec<PeerSyncView> {
        let mut views: Vec<PeerSyncView> = self
            .ledgers
            .iter()
            .map(|ledger| ledger.view(sessions.contains(&ledger.node)))
            .collect();
        views.sort_by_key(|view| view.node);
        views
    }

    /// The ledger for `node`, created if absent; a full set drops the peer
    /// tried longest ago first.
    fn ledger_mut(&mut self, node: NodeId, now: Timestamp) -> &mut PeerLedger {
        let position = match self.ledgers.iter().position(|ledger| ledger.node == node) {
            Some(index) => index,
            None => {
                if self.ledgers.len() >= PEER_VIEWS_MAX {
                    let oldest = self
                        .ledgers
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, ledger)| ledger.attempted)
                        .map(|(index, _)| index)
                        .expect("a full set has an oldest entry");
                    self.ledgers.swap_remove(oldest);
                }
                self.ledgers.push(PeerLedger::new(node, now));
                self.ledgers.len() - 1
            }
        };
        assert!(self.ledgers.len() <= PEER_VIEWS_MAX);
        let ledger = &mut self.ledgers[position];
        ledger.attempted = now;
        ledger
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    #[test]
    fn a_success_clears_the_error_and_accumulates_counts() {
        let mut ledgers = PeerLedgers::default();
        ledgers.record_error(node(2), Timestamp(10), "refused".to_string());
        let failed = ledgers.view(&node(2), false).expect("tracked");
        assert!(!failed.live);
        assert_eq!(failed.last_error.as_deref(), Some("refused"));
        ledgers.record_success(node(2), Timestamp(20), 3, 1);
        ledgers.record_success(node(2), Timestamp(30), 2, 0);
        let view = ledgers.view(&node(2), false).expect("tracked");
        assert!(view.live);
        assert_eq!(view.last_success, Some(Timestamp(30)));
        assert_eq!(view.last_error, None);
        assert_eq!(view.entries_received, 5);
        assert_eq!(view.entries_sent, 1);
    }

    #[test]
    fn a_session_makes_a_failed_peer_live() {
        let mut ledgers = PeerLedgers::default();
        ledgers.record_error(node(2), Timestamp(10), "timed out".to_string());
        assert!(ledgers.view(&node(2), true).expect("tracked").live);
        let sessions: HashSet<NodeId> = [node(2)].into_iter().collect();
        assert!(ledgers.views(&sessions)[0].live);
    }

    #[test]
    fn views_are_ordered_by_node() {
        let mut ledgers = PeerLedgers::default();
        ledgers.record_success(node(9), Timestamp(1), 0, 0);
        ledgers.record_success(node(3), Timestamp(2), 0, 0);
        let ids: Vec<NodeId> = ledgers
            .views(&HashSet::new())
            .iter()
            .map(|v| v.node)
            .collect();
        assert_eq!(ids, vec![node(3), node(9)]);
    }

    #[test]
    fn a_full_set_forgets_the_peer_tried_longest_ago() {
        let mut ledgers = PeerLedgers::default();
        for i in 0..PEER_VIEWS_MAX {
            let byte = u8::try_from(i % 256).expect("fits");
            let mut bytes = [byte; 32];
            bytes[0] = u8::try_from(i / 256).expect("fits");
            let time = i64::try_from(i + 1).expect("fits");
            ledgers.record_success(NodeId::from_bytes(bytes), Timestamp(time), 0, 0);
        }
        let oldest = NodeId::from_bytes([0; 32]);
        assert!(ledgers.view(&oldest, false).is_some());
        ledgers.record_success(node(255), Timestamp(5_000), 0, 0);
        assert!(ledgers.view(&oldest, false).is_none());
        assert_eq!(ledgers.views(&HashSet::new()).len(), PEER_VIEWS_MAX);
    }
}
