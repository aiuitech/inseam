//! What this node publishes about the hosts it stewards, and how the
//! roster decides what is news (`design/roster.md`). Every registration in
//! the connections registry maps to one host record and one stewardship
//! record; the plan is the difference between that and what was published
//! last, so an unchanged host costs no log entry and a vanished one earns
//! a withdrawal. Pure: the store and the registry stay outside.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use inseam_kernel::address::HostId;
use inseam_kernel::network::{HostRecord, NodeId, StewardCapabilities, StewardshipRecord};
use inseam_seams::connection::Registration;

/// Most hosts one node stewards at once. A node holds a filesystem, a few
/// accounts, a workspace or two; a thousand registrations is a composition
/// mistake, not a bigger node.
pub const HOSTS_PER_NODE_MAX: usize = 1_024;

/// The two records one registration publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Publication {
    pub host: HostRecord,
    pub stewardship: StewardshipRecord,
}

/// What one reconcile pass must publish and withdraw.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    pub publish: Vec<Publication>,
    pub withdraw: Vec<HostId>,
}

impl Plan {
    pub(crate) fn is_empty(&self) -> bool {
        self.publish.is_empty() && self.withdraw.is_empty()
    }
}

/// The records `registration` publishes with `local` as the steward:
/// the host as the connection describes it, and the claim carrying the
/// edge's capabilities and roots — never its credentials.
pub(crate) fn publication(local: NodeId, registration: &Registration) -> Publication {
    Publication {
        host: HostRecord {
            id: registration.host.id.clone(),
            kind: registration.host.kind.as_str().to_string(),
            display_name: registration.host.display_name.clone(),
        },
        stewardship: StewardshipRecord {
            node: local,
            host: registration.host.id.clone(),
            capabilities: StewardCapabilities {
                enumerates: registration.capabilities.enumerates,
                change_feed: registration.capabilities.change_feed,
                writable: registration.capabilities.writable,
            },
            roots: registration.roots.clone(),
        },
    }
}

/// Diff the registry against what was last published: every new or
/// changed registration is published, every published host no longer
/// registered is withdrawn. Withdrawals are ordered by host id so a pass
/// is deterministic.
pub(crate) fn plan(
    local: NodeId,
    snapshot: &[Arc<Registration>],
    published: &HashMap<HostId, Publication>,
) -> Plan {
    assert!(
        snapshot.len() <= HOSTS_PER_NODE_MAX,
        "a node stewards at most {HOSTS_PER_NODE_MAX} hosts"
    );
    let mut plan = Plan::default();
    for registration in snapshot {
        let next = publication(local, registration);
        let unchanged = published
            .get(&next.host.id)
            .is_some_and(|previous| *previous == next);
        if unchanged {
            continue;
        }
        plan.publish.push(next);
    }
    let live: HashSet<&HostId> = snapshot.iter().map(|r| &r.host.id).collect();
    for host in published.keys() {
        if live.contains(host) {
            continue;
        }
        plan.withdraw.push(host.clone());
    }
    plan.withdraw.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert!(plan.publish.len() <= snapshot.len());
    assert!(plan.withdraw.len() <= published.len());
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::Address;
    use inseam_seams::SeamError;
    use inseam_seams::connection::{
        Capabilities, Connection, EnumeratedSource, HostDescription, HostKind,
    };

    struct Stub;

    #[async_trait::async_trait]
    impl Connection for Stub {
        async fn enumerate(&self, _root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
            Ok(Vec::new())
        }
        fn locator_prefix(&self, _root: &str) -> Option<String> {
            None
        }
        async fn read_text(&self, _address: &Address) -> Result<String, SeamError> {
            Ok(String::new())
        }
        async fn read_lines(&self, _a: &Address, _s: u64, _e: u64) -> Result<String, SeamError> {
            Ok(String::new())
        }
        async fn read_bytes(&self, _address: &Address) -> Result<Vec<u8>, SeamError> {
            Ok(Vec::new())
        }
    }

    fn registration(host: &str, roots: &[&str]) -> Arc<Registration> {
        Arc::new(Registration {
            entry_id: "fs".to_string(),
            host: HostDescription {
                id: HostId::new(host).expect("valid"),
                kind: HostKind::filesystem(),
                display_name: format!("{host} display"),
            },
            capabilities: Capabilities::READ_ONLY,
            roots: roots.iter().map(ToString::to_string).collect(),
            connection: Arc::new(Stub),
        })
    }

    fn local() -> NodeId {
        NodeId::from_bytes([7; 32])
    }

    #[test]
    fn a_registration_maps_to_a_host_and_a_claim_without_credentials() {
        let published = publication(local(), &registration("fs-a", &["notes"]));
        assert_eq!(published.host.id.as_str(), "fs-a");
        assert_eq!(published.host.kind, "fs");
        assert_eq!(published.host.display_name, "fs-a display");
        assert_eq!(published.stewardship.node, local());
        assert_eq!(published.stewardship.host, published.host.id);
        assert!(published.stewardship.capabilities.enumerates);
        assert!(!published.stewardship.capabilities.writable);
        assert_eq!(published.stewardship.roots, vec!["notes"]);
    }

    #[test]
    fn a_new_registration_is_published() {
        let snapshot = vec![registration("fs-a", &[])];
        let plan = plan(local(), &snapshot, &HashMap::new());
        assert_eq!(plan.publish.len(), 1);
        assert!(plan.withdraw.is_empty());
    }

    #[test]
    fn an_unchanged_registration_is_not_republished() {
        let snapshot = vec![registration("fs-a", &["notes"])];
        let mut published = HashMap::new();
        let first = publication(local(), &snapshot[0]);
        published.insert(first.host.id.clone(), first);
        assert!(plan(local(), &snapshot, &published).is_empty());
    }

    #[test]
    fn a_changed_registration_is_published_again() {
        let mut published = HashMap::new();
        let before = publication(local(), &registration("fs-a", &["notes"]));
        published.insert(before.host.id.clone(), before);
        let snapshot = vec![registration("fs-a", &["notes", "papers"])];
        let plan = plan(local(), &snapshot, &published);
        assert_eq!(plan.publish.len(), 1);
        assert_eq!(plan.publish[0].stewardship.roots, vec!["notes", "papers"]);
        assert!(plan.withdraw.is_empty());
    }

    #[test]
    fn a_vanished_registration_is_withdrawn_in_host_order() {
        let mut published = HashMap::new();
        for host in ["fs-b", "fs-a"] {
            let gone = publication(local(), &registration(host, &[]));
            published.insert(gone.host.id.clone(), gone);
        }
        let plan = plan(local(), &[], &published);
        assert!(plan.publish.is_empty());
        let withdrawn: Vec<&str> = plan.withdraw.iter().map(HostId::as_str).collect();
        assert_eq!(withdrawn, vec!["fs-a", "fs-b"]);
    }
}
