//! A source's planned subtree: everything one index run derived for one
//! source, described before any of it has an id. The sweep plans subtrees
//! in parallel (transforms are the slow part of indexing) and the store
//! lands each plan in one transaction (`design/indexing.md`), so a plan is
//! the hand-off shape between the two — pure data, no store handle.
//!
//! Fragments are addressed by **plan index** here: parents and keyed-sprout
//! anchors name positions in the plan, and the store maps positions to ids
//! as it inserts. A plan is parent-before-child by construction, which is
//! what lets the store insert it in one forward pass.

use crate::address::{Address, Envelope};
use crate::fragment::{FragmentKey, NewFragment, RelationKind};
use crate::store::InventoryEntry;

/// A fragment named by its position in a [`SubtreePlan`]: the root, or the
/// `n`th planned fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanNode {
    Root,
    Fragment(u32),
}

/// A fragment the plan will insert under a parent already in the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFragment {
    /// The input fragment; must precede this one in the plan.
    pub parent: PlanNode,
    /// Kind of the edge parent → this fragment.
    pub relation: RelationKind,
    pub fragment: NewFragment,
}

/// A keyed sprout with its anchors already resolved to plan positions: the
/// store gets-or-creates the index-wide fragment under `key` and relates
/// every anchor to it with `relation`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedKeyed {
    pub key: FragmentKey,
    pub fragment: NewFragment,
    pub relation: RelationKind,
    /// Never empty: a keyed sprout always anchors somewhere.
    pub anchors: Vec<PlanNode>,
}

/// The shape records a deep-indexed source carries
/// (`design/index-maintenance.md`): the stamp of the transforms that built
/// the subtree and the subtree's mimetype inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub stamp: String,
    pub inventory: Vec<InventoryEntry>,
}

/// One source's complete subtree, ready to be stored atomically.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtreePlan {
    pub address: Address,
    /// The envelope as the run saw it (length upgraded to lines when the
    /// content was read).
    pub envelope: Envelope,
    pub raw_bytes: u64,
    pub root: NewFragment,
    pub fragments: Vec<PlannedFragment>,
    pub keyed: Vec<PlannedKeyed>,
    pub shape: Shape,
}

impl SubtreePlan {
    /// Every parent and anchor names a position that precedes its user:
    /// the invariant the store's single forward insert pass relies on. The
    /// sweep asserts it when the plan is built; the store asserts it again
    /// before writing (paired assertions).
    pub fn is_well_ordered(&self) -> bool {
        let parents_ordered = self
            .fragments
            .iter()
            .enumerate()
            .all(|(index, planned)| node_precedes(planned.parent, index));
        let anchors_ordered = self.keyed.iter().all(|keyed| {
            !keyed.anchors.is_empty()
                && keyed
                    .anchors
                    .iter()
                    .all(|anchor| node_precedes(*anchor, self.fragments.len()))
        });
        parents_ordered && anchors_ordered
    }

    /// Fragments the plan inserts, root included.
    pub fn fragment_count(&self) -> usize {
        1 + self.fragments.len()
    }
}

/// Whether `node` is the root or a fragment strictly before position
/// `index` — i.e. already inserted by the time position `index` is.
fn node_precedes(node: PlanNode, index: usize) -> bool {
    match node {
        PlanNode::Root => true,
        PlanNode::Fragment(n) => usize::try_from(n).is_ok_and(|n| n < index),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{ContentLength, Timestamp};
    use crate::fragment::Mimetype;

    fn plan(fragments: Vec<PlannedFragment>, keyed: Vec<PlannedKeyed>) -> SubtreePlan {
        SubtreePlan {
            address: "inseam://fs-test/tmp/a.md".parse().expect("parses"),
            envelope: Envelope {
                source_type: "file".into(),
                content_type: Mimetype::markdown(),
                length: ContentLength::Lines(1),
                created: None,
                modified: None,
                observed: Timestamp(0),
                properties: Vec::new(),
                hint: None,
            },
            raw_bytes: 1,
            root: NewFragment {
                mimetype: Mimetype::markdown(),
                text: None,
                extent: None,
            },
            fragments,
            keyed,
            shape: Shape {
                stamp: "s".into(),
                inventory: Vec::new(),
            },
        }
    }

    fn fragment(parent: PlanNode) -> PlannedFragment {
        PlannedFragment {
            parent,
            relation: RelationKind::contains(),
            fragment: NewFragment {
                mimetype: Mimetype::text_plain(),
                text: Some("x".into()),
                extent: None,
            },
        }
    }

    #[test]
    fn parent_before_child_is_well_ordered() {
        let p = plan(vec![fragment(PlanNode::Root), fragment(PlanNode::Fragment(0))], vec![]);
        assert!(p.is_well_ordered());
        assert_eq!(p.fragment_count(), 3);
    }

    #[test]
    fn forward_or_self_parent_is_rejected() {
        assert!(!plan(vec![fragment(PlanNode::Fragment(0))], vec![]).is_well_ordered());
        assert!(!plan(vec![fragment(PlanNode::Root), fragment(PlanNode::Fragment(5))], vec![]).is_well_ordered());
    }

    #[test]
    fn keyed_anchors_must_exist_and_be_nonempty() {
        let keyed = |anchors: Vec<PlanNode>| PlannedKeyed {
            key: FragmentKey::new("entity:x:y").expect("valid"),
            fragment: NewFragment {
                mimetype: Mimetype::text_plain(),
                text: None,
                extent: None,
            },
            relation: RelationKind::new("mentions").expect("valid"),
            anchors,
        };
        assert!(plan(vec![fragment(PlanNode::Root)], vec![keyed(vec![PlanNode::Fragment(0)])]).is_well_ordered());
        assert!(!plan(vec![fragment(PlanNode::Root)], vec![keyed(vec![])]).is_well_ordered());
        assert!(!plan(vec![fragment(PlanNode::Root)], vec![keyed(vec![PlanNode::Fragment(1)])]).is_well_ordered());
    }
}
