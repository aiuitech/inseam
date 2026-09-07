//! The session table: which peers are live right now, in either direction
//! — local knowledge, never synced (`design/network.md`). One session per
//! peer, bounded by [`SESSIONS_MAX`] with the least recently used evicted
//! to make room. When two live connections to one peer meet — both sides
//! dialed at once — both sides keep the one dialed by the lower node id,
//! so they converge on the same connection instead of each closing the
//! other's; when the same dialer connects twice, the newer one wins, since
//! the older is the one that dialer lost track of.

use std::collections::HashMap;

use iroh::endpoint::Connection;

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::NodeId;
use inseam_seams::transport::{SessionDirection, SessionView, SESSIONS_MAX};

pub struct Session {
    pub connection: Connection,
    pub direction: SessionDirection,
    pub since: Timestamp,
    pub last_used: Timestamp,
}

impl Session {
    /// Who opened this connection: us for an outbound session, the peer for
    /// an inbound one.
    fn dialer(&self, local: NodeId, peer: NodeId) -> NodeId {
        match self.direction {
            SessionDirection::Outbound => local,
            SessionDirection::Inbound => peer,
        }
    }
}

/// Whether a new session with a peer replaces the one already held. A dead
/// one always yields. Two live ones dialed by the same node: the newer
/// wins, since the older is the one that dialer lost track of. Two live
/// ones dialed by different nodes — both sides dialed at once — the one
/// dialed by the lower node id wins, on both sides, so they converge on
/// one connection instead of each closing the other's.
fn new_session_wins(existing: &Session, new: &Session, local: NodeId, peer: NodeId) -> bool {
    if existing.connection.close_reason().is_some() {
        return true;
    }
    let existing_dialer = existing.dialer(local, peer);
    let new_dialer = new.dialer(local, peer);
    if existing_dialer == new_dialer {
        return true;
    }
    new_dialer < existing_dialer
}

#[derive(Default)]
pub struct SessionTable {
    sessions: HashMap<NodeId, Session>,
}

impl SessionTable {
    /// Record a session with `peer`. Returns every connection the table let
    /// go of — the loser of a duplicate, an evicted idle session — for the
    /// caller to close outside the lock. At most two.
    pub fn insert(&mut self, local: NodeId, peer: NodeId, session: Session) -> Vec<Connection> {
        assert!(local != peer, "a node holds no session with itself");
        let mut displaced = Vec::with_capacity(2);
        let keep_new = match self.sessions.get(&peer) {
            None => true,
            Some(existing) => new_session_wins(existing, &session, local, peer),
        };
        if !keep_new {
            displaced.push(session.connection);
            return displaced;
        }
        if let Some(previous) = self.sessions.remove(&peer) {
            displaced.push(previous.connection);
        }
        if self.sessions.len() >= SESSIONS_MAX {
            let evicted = self.evict_least_recently_used();
            displaced.extend(evicted);
        }
        self.sessions.insert(peer, session);
        assert!(self.sessions.len() <= SESSIONS_MAX, "the session bound holds");
        assert!(displaced.len() <= 2);
        displaced
    }

    fn evict_least_recently_used(&mut self) -> Option<Connection> {
        let victim = self
            .sessions
            .iter()
            .min_by_key(|(_, session)| session.last_used)
            .map(|(peer, _)| *peer)?;
        tracing::warn!(
            peer = %victim.short(),
            "{SESSIONS_MAX} sessions are open; evicting the least recently used"
        );
        self.sessions.remove(&victim).map(|s| s.connection)
    }

    pub fn get(&self, peer: &NodeId) -> Option<Connection> {
        self.sessions.get(peer).map(|s| s.connection.clone())
    }

    pub fn touch(&mut self, peer: &NodeId, now: Timestamp) {
        if let Some(session) = self.sessions.get_mut(peer) {
            session.last_used = now;
        }
    }

    pub fn remove(&mut self, peer: &NodeId) -> Option<Connection> {
        self.sessions.remove(peer).map(|s| s.connection)
    }

    /// Remove the session with `peer` only if it is still this very
    /// connection: a serve loop ending for a superseded connection must
    /// not evict the newer session that replaced it.
    pub fn remove_if_same(&mut self, peer: &NodeId, stable_id: usize) -> Option<Connection> {
        let same = self
            .sessions
            .get(peer)
            .is_some_and(|s| s.connection.stable_id() == stable_id);
        if !same {
            return None;
        }
        self.remove(peer)
    }

    pub fn views(&self) -> Vec<SessionView> {
        let mut views: Vec<SessionView> = self
            .sessions
            .iter()
            .map(|(peer, session)| SessionView {
                peer: *peer,
                direction: session.direction,
                since: session.since,
                last_used: session.last_used,
            })
            .collect();
        views.sort_by_key(|view| view.peer);
        views
    }
}
