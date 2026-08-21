//! The `transforms` seam: the registration door for indexing
//! (`design/indexing.md`). Transform plugins register claims + apply here as
//! effects; the sweep consumes the registry. Linked and loaded transforms
//! are indistinguishable to the consumer — tier is provenance, not shape.
//!
//! This module also owns the claims-aware **shape stamp** helpers
//! (`design/index-maintenance.md`): the stamp digests the transform
//! registrations that participated in a source's subtree, and a stored
//! mimetype inventory decides which registrations *would* participate now —
//! so mounting a video transform never dirties a markdown note.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use inseam_kernel::address::Envelope;
use inseam_kernel::fragment::{Mimetype, Sprout};
use inseam_kernel::store::InventoryEntry;
use inseam_kernel::substrate::{fnv1a, ApplyCx, PluginError, ServiceKey};
use crate::text::collapse_ws;
use crate::SeamError;

pub const TRANSFORMS: ServiceKey<dyn Transforms> = ServiceKey::new("transforms");

/// Register a transform into the seam as a fiber effect: a transform plugin's
/// `apply` calls this once per transform it ships, and unmounting the plugin
/// unwinds the registration through the disposer the registry returned. The
/// next sweep discovers the shape divergence on its own, so there are no
/// lifecycle hooks into the index. Both tiers register this way — the wasm
/// bridge included — which is what keeps them indistinguishable to the sweep.
pub fn register_as_effect(
    cx: &mut ApplyCx<'_>,
    registration: Registration,
) -> Result<(), PluginError> {
    let label = format!("register transform {}", registration.name);
    let registry = cx.get(&TRANSFORMS)?;
    let disposer = registry.register(registration);
    cx.effect(label, disposer);
    Ok(())
}

/// How a transform participates: structural transforms decompose a fragment
/// into its subtree; enrichment transforms derive understanding from it.
/// Structural transforms apply before enrichment ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransformKind {
    Structural,
    Enrichment,
}

/// The narrowed LLM capability a transform application may be granted. The
/// sweep hands it in — a transform never reaches the `llm` seam directly, so
/// model choice, budget metering, and call counting stay with the grantor.
/// Withheld (`None` in the ctx) when the node is offline or the transform's
/// budget for this run is spent.
#[async_trait::async_trait]
pub trait GrantedLlm: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String, SeamError>;
    async fn describe_image(
        &self,
        prompt: &str,
        mimetype: &str,
        image: &[u8],
    ) -> Result<String, SeamError>;
}

/// Everything a transform application may see. Capabilities are handed in,
/// never grabbed.
pub struct TransformCtx<'a> {
    pub envelope: &'a Envelope,
    /// The claimed fragment's mimetype (the envelope's content type at the
    /// root; an emitted mimetype when a chained transform re-enters).
    pub mimetype: &'a Mimetype,
    pub is_root: bool,
    /// The fragment's text; `None` for content the node did not read.
    pub text: Option<&'a str>,
    /// Raw source bytes; granted only to transforms that declare
    /// [`Transform::wants_bytes`], and only at the root.
    pub bytes: Option<&'a [u8]>,
    /// Shared (`Arc`) so a sandbox bridge can move the grant into its
    /// instance state; the grant is still per-application and metered.
    pub llm: Option<Arc<dyn GrantedLlm>>,
}

/// What a transform emits: sprouts become child fragments of the input;
/// entities are handed back for the core to deduplicate index-wide (a
/// transform cannot know fragment ids).
#[derive(Debug, Default)]
pub struct TransformOutput {
    pub sprouts: Vec<Sprout>,
    pub entities: Vec<ExtractedEntity>,
}

impl TransformOutput {
    pub fn sprouts(sprouts: Vec<Sprout>) -> Self {
        Self {
            sprouts,
            ..Self::default()
        }
    }
}

/// A transform implementation. `apply` is infallible by contract: indexing
/// is enrichment, and a transform that cannot work degrades to emitting
/// nothing rather than gating the source.
#[async_trait::async_trait]
pub trait Transform: Send + Sync {
    fn kind(&self) -> TransformKind;

    /// Whether this transform claims a fragment of this mimetype at this
    /// position. Inseam-defined mimetypes (summaries, entities) are derived
    /// understanding and must never be claimed.
    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool;

    /// Whether applications should receive the source's raw bytes.
    fn wants_bytes(&self) -> bool {
        false
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput;
}

/// One registered transform plus what the seam mediates for it.
pub struct Registration {
    /// The composition entry that mounted it — the shape stamp's identity.
    pub entry_id: String,
    /// Human name for reports.
    pub name: String,
    pub transform: Arc<dyn Transform>,
    /// LLM calls this transform may make per index run (0 = never granted).
    pub llm_call_budget: usize,
    /// Digest input capturing everything that changes this transform's
    /// output shape: its config, and for loaded transforms the artifact
    /// version. Two mounts with equal fingerprints build equal subtrees.
    pub shape_fingerprint: String,
}

/// The registry seam. Registration returns a disposer — the effect the
/// registering fiber accumulates, so unmounting a transform plugin unwinds
/// its registration with no uninstall path anywhere.
pub trait Transforms: Send + Sync {
    fn register(&self, registration: Registration) -> Box<dyn FnOnce() + Send>;

    /// Every live registration, ordered for application: structural before
    /// enrichment, then by entry id — deterministic regardless of activation
    /// order.
    fn snapshot(&self) -> Vec<Arc<Registration>>;
}

/// The registrations that would participate in a subtree with this mimetype
/// inventory — claims intersected with what is actually present.
pub fn participating<'a>(
    registrations: &'a [Arc<Registration>],
    inventory: &[InventoryEntry],
) -> Vec<&'a Arc<Registration>> {
    registrations
        .iter()
        .filter(|r| {
            inventory.iter().any(|entry| {
                Mimetype::parse(&entry.mimetype)
                    .is_ok_and(|m| r.transform.claims(&m, entry.is_root))
            })
        })
        .collect()
}

/// The claims-aware shape stamp: a canonical digest of the participating
/// registrations (entry id + shape fingerprint, sorted). Stored per source
/// when its subtree lands; compared on later sweeps against the stamp the
/// *current* registrations would produce for the stored inventory.
pub fn shape_stamp(participating: &[&Arc<Registration>]) -> String {
    let mut parts: Vec<String> = participating
        .iter()
        .map(|r| format!("{}={}", r.entry_id, r.shape_fingerprint))
        .collect();
    parts.sort();
    format!("v2|{:016x}", fnv1a(parts.join("|").as_bytes()))
}

/// Decomposition limits, enforced by the sweep over every transform's
/// output.
#[derive(Debug, Clone, Copy)]
pub struct DecomposeBudget {
    pub max_depth: usize,
    pub max_fragments: usize,
}

/// Enforce depth and count budgets over a sprout forest, breadth-first so
/// shallow structure survives before deep detail.
pub fn prune(sprouts: Vec<Sprout>, budget: DecomposeBudget) -> Vec<Sprout> {
    fn depth_prune(mut sprouts: Vec<Sprout>, depth_left: usize) -> Vec<Sprout> {
        if depth_left == 0 {
            return Vec::new();
        }
        for s in &mut sprouts {
            s.children = depth_prune(std::mem::take(&mut s.children), depth_left - 1);
        }
        sprouts
    }

    fn count_prune(sprouts: &mut Vec<Sprout>, remaining: &mut usize) {
        sprouts.retain_mut(|s| {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            count_prune(&mut s.children, remaining);
            true
        });
    }

    let mut sprouts = depth_prune(sprouts, budget.max_depth.max(1));
    let mut remaining = budget.max_fragments;
    count_prune(&mut sprouts, &mut remaining);
    sprouts
}

// ---------------------------------------------------------------------------
// Entity vocabulary (what enrichment transforms hand back)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Person,
    Place,
    Org,
    Project,
    Date,
    Other,
}

impl EntityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Place => "place",
            Self::Org => "org",
            Self::Project => "project",
            Self::Date => "date",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EntityKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "person" | "people" => Ok(Self::Person),
            "place" | "location" => Ok(Self::Place),
            "org" | "organization" | "organisation" | "company" => Ok(Self::Org),
            "project" => Ok(Self::Project),
            "date" | "time" => Ok(Self::Date),
            _ => Ok(Self::Other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedEntity {
    pub name: String,
    pub kind: EntityKind,
}

impl ExtractedEntity {
    /// The per-index deduplication key: one fragment per entity, however
    /// many sources mention it.
    pub fn key(&self) -> String {
        format!("{}:{}", self.kind, collapse_ws(&self.name).to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::fragment::{NewFragment, RelationKind};

    struct Claimer(&'static [&'static str], bool);

    #[async_trait::async_trait]
    impl Transform for Claimer {
        fn kind(&self) -> TransformKind {
            TransformKind::Structural
        }
        fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
            (!self.1 || is_root) && self.0.contains(&mimetype.essence())
        }
        async fn apply(&self, _ctx: TransformCtx<'_>) -> TransformOutput {
            TransformOutput::default()
        }
    }

    fn registration(entry: &str, claims: &'static [&'static str], fingerprint: &str) -> Arc<Registration> {
        Arc::new(Registration {
            entry_id: entry.to_string(),
            name: entry.to_string(),
            transform: Arc::new(Claimer(claims, true)),
            llm_call_budget: 0,
            shape_fingerprint: fingerprint.to_string(),
        })
    }

    fn inventory(entries: &[(&str, bool)]) -> Vec<InventoryEntry> {
        entries
            .iter()
            .map(|(m, r)| InventoryEntry {
                mimetype: m.to_string(),
                is_root: *r,
            })
            .collect()
    }

    #[test]
    fn stamp_ignores_transforms_whose_claims_miss_the_inventory() {
        let markdown = registration("markdown", &["text/markdown"], "a");
        let video = registration("video", &["video/mp4"], "b");
        let inv = inventory(&[("text/markdown", true)]);

        let before = shape_stamp(&participating(&[markdown.clone()], &inv));
        let after = shape_stamp(&participating(
            &[markdown.clone(), video.clone()],
            &inv,
        ));
        assert_eq!(before, after, "mounting a video transform must not dirty markdown");

        let image_inv = inventory(&[("video/mp4", true)]);
        let with_video = shape_stamp(&participating(&[markdown, video], &image_inv));
        assert_ne!(before, with_video);
    }

    #[test]
    fn stamp_changes_when_a_participant_reconfigures_or_upgrades() {
        let inv = inventory(&[("text/markdown", true)]);
        let v1 = registration("markdown", &["text/markdown"], "cfg-1");
        let v2 = registration("markdown", &["text/markdown"], "cfg-2");
        assert_ne!(
            shape_stamp(&participating(&[v1], &inv)),
            shape_stamp(&participating(&[v2], &inv)),
        );
    }

    #[test]
    fn stamp_is_order_independent() {
        let a = registration("a", &["text/plain"], "x");
        let b = registration("b", &["text/plain"], "y");
        let inv = inventory(&[("text/plain", true)]);
        assert_eq!(
            shape_stamp(&participating(&[a.clone(), b.clone()], &inv)),
            shape_stamp(&participating(&[b, a], &inv)),
        );
    }

    #[test]
    fn prune_caps_depth_and_count() {
        fn sprout(children: Vec<Sprout>) -> Sprout {
            Sprout {
                fragment: NewFragment {
                    mimetype: Mimetype::text_plain(),
                    text: Some("x".into()),
                    extent: None,
                },
                relation: RelationKind::Contains,
                children,
            }
        }
        let tree = vec![sprout(vec![sprout(vec![sprout(vec![])])])];
        let out = prune(
            tree,
            DecomposeBudget {
                max_depth: 2,
                max_fragments: 100,
            },
        );
        assert_eq!(out[0].children.len(), 1);
        assert!(out[0].children[0].children.is_empty());

        let flat = vec![sprout(vec![]), sprout(vec![]), sprout(vec![])];
        let capped = prune(
            flat,
            DecomposeBudget {
                max_depth: 3,
                max_fragments: 2,
            },
        );
        assert_eq!(capped.len(), 2);
    }
}
