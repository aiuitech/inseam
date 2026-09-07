//! Planning one source's subtree: read its content, apply the registered
//! transforms recursively — every claimant of a fragment concurrently, the
//! emitted fragments in turn until nothing claims the output — and describe
//! the result as a [`SubtreePlan`] the store lands in one transaction.
//!
//! Planning writes nothing to the store: it is the parallel half of the
//! sweep (`design/indexing.md`), and many planners run at once because
//! transforms — LLM calls above all — are where indexing spends its time.
//! The one store access is a read: before an LLM transform is applied, the
//! planner asks the digest-keyed transform cache whether this input under
//! this transform has an output already ([`super::cache`]). Outputs the
//! model produced ride the plan back to the sweep, which files them when
//! the subtree lands.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use futures_util::future::join_all;
use tokio::sync::Semaphore;

use inseam_kernel::address::{Address, ContentDigest, ContentLength, Envelope};
use inseam_kernel::fragment::{Extent, Mimetype, NewFragment, Sprout};
use inseam_kernel::store::{IndexStore, InventoryEntry};
use inseam_kernel::subtree::{PlanNode, PlannedFragment, PlannedKeyed, Shape, SubtreePlan};
use inseam_seams::SeamError;
use inseam_seams::connection::{Connection, Connections, EnumeratedSource};
use inseam_seams::text::{count_lines, is_indexable_text};
use inseam_seams::transforms::{
    Anchor, DecomposeBudget, GrantedLlm, KeyedSprout, Registration, TransformCtx, TransformOutput,
    participating, prune, shape_stamp,
};

use super::cache::{ObservedLlm, cache_key, caches_output, decode_output, encode_output};
use super::grant::Grantor;

/// The sweep's decomposition dials (shape tier), as the planner enforces
/// them over every transform's output.
#[derive(Debug, Clone, Copy)]
pub(super) struct PlanLimits {
    pub(super) max_depth: usize,
    pub(super) max_fragments_per_source: usize,
    pub(super) max_content_bytes: u64,
    /// The crawl depth: content references followed in a chain from the
    /// source before the planner stops applying transforms to them.
    pub(super) max_reference_hops: u32,
}

/// What planning one source produced besides the plan: the counts the
/// run report tallies.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlanStats {
    pub(super) llm_summaries: usize,
    pub(super) extractive_summaries: usize,
    pub(super) envelope_summaries: usize,
    /// Transform applications answered from the transform cache.
    pub(super) transforms_reused: usize,
}

pub(super) struct Planned {
    pub(super) plan: SubtreePlan,
    pub(super) stats: PlanStats,
    /// Transform outputs the model produced this plan, keyed for the
    /// cache; the sweep files them when the subtree lands.
    pub(super) cache_entries: Vec<(String, String)>,
}

/// One transform's output for one work item and where it came from.
struct Applied<'a> {
    registration: &'a Arc<Registration>,
    output: TransformOutput,
    provenance: Provenance,
}

enum Provenance {
    /// Applied now; `Some` when the model produced it and it should be filed.
    Applied(Option<(String, String)>),
    /// Taken from the transform cache.
    Reused,
}

/// What the cache said about one claimant of a work item.
enum CacheLookup {
    Hit(TransformOutput),
    /// Not cached; `Some` when the output can be filed under this key.
    Miss(Option<String>),
}

/// Where a source's content comes from when it is planned: read through the
/// host's connection (a file), or composed by the sweep before planning (a
/// folder, whose text is the listing of its landed children —
/// `design/indexing.md`, folders). Composed content is always text.
#[derive(Debug, Clone)]
pub(super) enum PlanContent {
    Host,
    Composed(String),
}

/// One source to plan, with its content's origin.
#[derive(Debug, Clone)]
pub(super) struct PlanInput {
    pub(super) source: EnumeratedSource,
    pub(super) content: PlanContent,
}

impl PlanInput {
    pub(super) fn from_host(source: EnumeratedSource) -> Self {
        Self {
            source,
            content: PlanContent::Host,
        }
    }
}

/// Everything a planner needs, shared across the run's concurrent planners.
pub(super) struct Planner {
    /// The connection of the host under sweep: where every root's content
    /// is read from.
    pub(super) connection: Arc<dyn Connection>,
    /// The registry, for fragments whose content lives at an address of its
    /// own (`NewFragment::content_address`) — possibly on another host.
    pub(super) connections: Arc<dyn Connections>,
    /// The store, read only, for the transform cache.
    pub(super) store: Arc<IndexStore>,
    pub(super) source_read_permits: Arc<Semaphore>,
    pub(super) registrations: Vec<Arc<Registration>>,
    pub(super) grantor: Arc<Grantor>,
    pub(super) sweep_shape: String,
    pub(super) limits: PlanLimits,
}

impl Planner {
    /// Build one source's plan by recursive transform application:
    /// registered claimants over the root, then over every emitted fragment,
    /// until nothing claims the output (`design/indexing.md`).
    pub(super) async fn plan(&self, input: &PlanInput) -> Result<Planned, SeamError> {
        let read = match &input.content {
            PlanContent::Host => self.read_source(&input.source).await?,
            PlanContent::Composed(text) => composed_read(&input.source, text),
        };
        let mut build = SubtreeBuild::new(&read, self.limits);
        // Every queued item is the root or a planted fragment, so the queue
        // never outgrows the fragment cap.
        let iterations_max = self.limits.max_fragments_per_source + 1;
        let mut iterations: usize = 0;
        while let Some(item) = build.queue.pop_front() {
            iterations += 1;
            assert!(
                iterations <= iterations_max,
                "work queue is bounded by the fragment cap"
            );
            // Derived understanding is never source content: transforms must
            // not re-decompose `text/x-inseam-*` fragments, whatever they claim.
            if item.mimetype.is_inseam_defined() {
                continue;
            }
            let outputs = self.apply_claimants(&read, &item).await;
            for applied in outputs {
                build.absorb(&item, applied);
            }
        }
        let stamp = expected_stamp(&self.registrations, &build.inventory, &self.sweep_shape);
        let planned = build.finish(read, stamp);
        assert!(
            planned.plan.is_well_ordered(),
            "planner emits parents before children"
        );
        Ok(planned)
    }

    /// Read what the transforms may see: the text for indexable text
    /// sources within the size cap, the raw bytes only when a byte-wanting
    /// transform claims the root. One raw read serves everything derived
    /// from content — the text, the bytes, and the envelope's content
    /// digest (`design/addressing.md`): the digest costs no extra fetch.
    async fn read_source(&self, source: &EnumeratedSource) -> Result<SourceRead, SeamError> {
        let is_texty = is_indexable_text(&source.envelope.content_type);
        let within_size = source.raw_bytes <= self.limits.max_content_bytes;
        let wants_bytes = self.registrations.iter().any(|r| {
            r.transform.wants_bytes() && r.transform.claims(&source.envelope.content_type, true)
        });
        let raw: Option<Vec<u8>> = if (is_texty || wants_bytes) && within_size {
            Some(self.read_source_bytes(&source.address).await?)
        } else {
            None
        };
        let mut envelope = source.envelope.clone();
        if envelope.content_digest.is_none() {
            // A steward-supplied digest (service metadata, same algorithm)
            // is kept; otherwise this first content read fills it in.
            envelope.content_digest = raw.as_deref().map(ContentDigest::of_bytes);
        }
        let content: Option<String> = match &raw {
            Some(bytes) if is_texty => Some(String::from_utf8_lossy(bytes).into_owned()),
            _ => None,
        };
        let bytes: Option<Vec<u8>> = if wants_bytes { raw } else { None };
        if let Some(text) = &content {
            envelope.length = ContentLength::Lines(count_lines(text));
        }
        Ok(SourceRead {
            source: source.clone(),
            envelope,
            content,
            bytes,
        })
    }

    async fn read_source_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let permit = Arc::clone(&self.source_read_permits)
            .acquire_owned()
            .await
            .map_err(|_| SeamError::failed("source read limiter closed"))?;
        let result = self.connection.read_bytes(address).await;
        drop(permit);
        result
    }

    /// Apply every registration claiming `item` — concurrently, since
    /// claimants are independent of one another (an LLM summary and an
    /// entity extraction of the same fragment overlap in flight) — and
    /// return the outputs in registration order, which keeps budgets and
    /// planting deterministic. A claimant whose output the transform cache
    /// holds for this input is answered from there without being applied.
    async fn apply_claimants<'a>(&'a self, read: &SourceRead, item: &WorkItem) -> Vec<Applied<'a>> {
        let claimants: Vec<&Arc<Registration>> = self
            .registrations
            .iter()
            .filter(|r| r.transform.claims(&item.mimetype, item.is_root))
            .collect();
        let wants_bytes = claimants.iter().any(|r| r.transform.wants_bytes());
        let referenced: Option<Vec<u8>> = if wants_bytes && !item.is_root {
            self.read_referenced_bytes(item).await
        } else {
            None
        };
        // The root's bytes come from the source read; a referenced
        // fragment's from its own address. Either way only byte-wanting
        // claimants see them.
        let item_bytes: Option<&[u8]> = if item.is_root {
            read.bytes.as_deref()
        } else {
            referenced.as_deref()
        };
        let reference_hops_left = self
            .limits
            .max_reference_hops
            .saturating_sub(item.reference_hops);
        let lookups = self.lookup_cached(&claimants, item, item_bytes).await;
        assert_eq!(lookups.len(), claimants.len());
        let applications = claimants
            .iter()
            .zip(lookups)
            .map(|(registration, lookup)| async move {
                match lookup {
                    CacheLookup::Hit(output) => Applied {
                        registration,
                        output,
                        provenance: Provenance::Reused,
                    },
                    CacheLookup::Miss(key) => {
                        let ctx = TransformCtx {
                            address: &read.source.address,
                            envelope: &read.envelope,
                            mimetype: &item.mimetype,
                            is_root: item.is_root,
                            text: item.text.as_deref(),
                            bytes: if registration.transform.wants_bytes() {
                                item_bytes
                            } else {
                                None
                            },
                            reference_hops_left,
                            llm: None,
                        };
                        self.apply_one(registration, ctx, key).await
                    }
                }
            });
        let outputs = join_all(applications).await;
        assert_eq!(outputs.len(), claimants.len());
        outputs
    }

    /// Apply one transform under an observed LLM grant. The output is
    /// filed under `key` only when the model produced it
    /// (`super::cache`); a fallback stays a fallback for this run alone.
    async fn apply_one<'a>(
        &self,
        registration: &'a Arc<Registration>,
        mut ctx: TransformCtx<'_>,
        key: Option<String>,
    ) -> Applied<'a> {
        let observed: Option<Arc<ObservedLlm>> =
            self.grantor.grant(registration).map(ObservedLlm::new);
        ctx.llm = observed
            .as_ref()
            .map(|llm| Arc::clone(llm) as Arc<dyn GrantedLlm>);
        let output = registration.transform.apply(ctx).await;
        let filed = match (key, &observed) {
            (Some(key), Some(llm)) if llm.output_is_the_models() => {
                encode_output(&output).map(|encoded| (key, encoded))
            }
            _ => None,
        };
        Applied {
            registration,
            output,
            provenance: Provenance::Applied(filed),
        }
    }

    /// Ask the transform cache about every cacheable claimant of `item`.
    /// One read per work item, chunked by the store; a read failure is a
    /// miss for everyone, logged, because the cache only saves work.
    async fn lookup_cached(
        &self,
        claimants: &[&Arc<Registration>],
        item: &WorkItem,
        item_bytes: Option<&[u8]>,
    ) -> Vec<CacheLookup> {
        let digest = item_digest(item, item_bytes);
        let keys: Vec<Option<String>> = claimants
            .iter()
            .map(|registration| {
                digest
                    .as_ref()
                    .filter(|_| caches_output(registration))
                    .map(|digest| cache_key(digest, &item.mimetype, item.is_root, registration))
            })
            .collect();
        let wanted: Vec<String> = keys.iter().flatten().cloned().collect();
        let mut hits = if wanted.is_empty() {
            std::collections::HashMap::new()
        } else {
            match self.store.cached_transform_outputs(&wanted).await {
                Ok(hits) => hits,
                Err(e) => {
                    tracing::warn!("transform cache read failed; applying transforms: {e}");
                    std::collections::HashMap::new()
                }
            }
        };
        keys.into_iter()
            .map(|key| match key {
                Some(key) => match hits
                    .remove(&key)
                    .and_then(|encoded| decode_output(&encoded))
                {
                    Some(output) => CacheLookup::Hit(output),
                    None => CacheLookup::Miss(Some(key)),
                },
                None => CacheLookup::Miss(None),
            })
            .collect()
    }

    /// The bytes a referenced fragment names, read once per work item
    /// through the connection stewarding the address's host and bounded by
    /// the content cap. Reads are best-effort like everything in planning:
    /// an unmounted host, a failed read, or an oversized target leaves the
    /// claimants without bytes and the fragment stays a bare reference
    /// (still fetchable by a client later), logged rather than fatal.
    async fn read_referenced_bytes(&self, item: &WorkItem) -> Option<Vec<u8>> {
        assert!(!item.is_root, "the root's bytes come from the source read");
        let address = item.content_address.as_ref()?;
        let Some(steward) = self.connections.resolve(&address.host) else {
            tracing::warn!(%address, "referenced content: no connection stewards its host");
            return None;
        };
        let bytes = match steward.connection.read_bytes(address).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%address, %error, "referenced content: read failed");
                return None;
            }
        };
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > self.limits.max_content_bytes {
            tracing::warn!(
                %address,
                bytes = size,
                cap = self.limits.max_content_bytes,
                "referenced content: over the content cap, bytes withheld"
            );
            return None;
        }
        Some(bytes)
    }
}

/// A source as read for planning.
struct SourceRead {
    source: EnumeratedSource,
    /// The envelope with its length upgraded to lines when text was read.
    envelope: Envelope,
    content: Option<String>,
    bytes: Option<Vec<u8>>,
}

/// The read a composed text stands in for: no host access, the digest and
/// line count taken from the text itself — so a folder whose listing did
/// not change carries the same digest the sweep's change detection
/// compares against, and hits the digest-keyed caches like any file.
fn composed_read(source: &EnumeratedSource, text: &str) -> SourceRead {
    assert!(
        source.envelope.content_type.is_directory(),
        "only folders are composed today"
    );
    let mut envelope = source.envelope.clone();
    envelope.content_digest = Some(ContentDigest::of_bytes(text.as_bytes()));
    envelope.length = ContentLength::Lines(count_lines(text));
    SourceRead {
        source: source.clone(),
        envelope,
        content: Some(text.to_string()),
        bytes: None,
    }
}

/// The content a work item presents to its claimants, digested: the text
/// when the item has text, the bytes otherwise, nothing for a bare
/// reference — which no cache can key.
fn item_digest(item: &WorkItem, item_bytes: Option<&[u8]>) -> Option<ContentDigest> {
    match (&item.text, item_bytes) {
        (Some(text), _) => Some(ContentDigest::of_bytes(text.as_bytes())),
        (None, Some(bytes)) => Some(ContentDigest::of_bytes(bytes)),
        (None, None) => None,
    }
}

/// A fragment awaiting transform application: the root, or an emitted
/// fragment re-entering as a non-root for chained claims.
struct WorkItem {
    node: PlanNode,
    mimetype: Mimetype,
    is_root: bool,
    text: Option<String>,
    /// Where the fragment's bytes live, for byte-wanting claimants of a
    /// non-root; always `None` at the root.
    content_address: Option<Address>,
    depth: usize,
    /// Content references followed to reach this fragment, itself
    /// included: the crawl depth the planner caps.
    reference_hops: u32,
}

/// The plan under construction.
struct SubtreeBuild {
    limits: PlanLimits,
    root_mimetype: Mimetype,
    fragments: Vec<PlannedFragment>,
    /// Keyed sprouts wait until the whole subtree is planted, so text
    /// anchors can see every fragment; each remembers its input.
    keyed: Vec<(PlanNode, KeyedSprout)>,
    /// Text-bearing source-content fragments: the anchor sites for keyed
    /// sprouts. Derived understanding (summaries) is searchable but never an
    /// anchor.
    texted: Vec<(PlanNode, String)>,
    inventory: Vec<InventoryEntry>,
    inventory_seen: HashSet<(String, bool)>,
    queue: VecDeque<WorkItem>,
    fragment_budget: usize,
    stats: PlanStats,
    cache_entries: Vec<(String, String)>,
}

impl SubtreeBuild {
    fn new(read: &SourceRead, limits: PlanLimits) -> Self {
        let root_mimetype = read.envelope.content_type.clone();
        let essence = root_mimetype.essence().to_string();
        Self {
            limits,
            root_mimetype: root_mimetype.clone(),
            fragments: Vec::new(),
            keyed: Vec::new(),
            texted: Vec::new(),
            inventory: vec![InventoryEntry {
                mimetype: essence.clone(),
                is_root: true,
            }],
            inventory_seen: HashSet::from([(essence, true)]),
            queue: VecDeque::from([WorkItem {
                node: PlanNode::Root,
                mimetype: root_mimetype,
                is_root: true,
                text: read.content.clone(),
                content_address: None,
                depth: 0,
                reference_hops: 0,
            }]),
            fragment_budget: limits.max_fragments_per_source,
            stats: PlanStats::default(),
            cache_entries: Vec::new(),
        }
    }

    /// Take one transform's output for `item`: tally summaries and cache
    /// reuse, prune to the remaining budget, plant the sprout forest, and
    /// hold the keyed sprouts.
    fn absorb(&mut self, item: &WorkItem, applied: Applied<'_>) {
        let Applied {
            registration,
            output,
            provenance,
        } = applied;
        match provenance {
            Provenance::Reused => self.stats.transforms_reused += 1,
            Provenance::Applied(Some(entry)) => self.cache_entries.push(entry),
            Provenance::Applied(None) => {}
        }
        for sprout in &output.sprouts {
            if sprout.fragment.mimetype.is_summary() {
                match sprout.fragment.mimetype.param("via") {
                    Some("llm") => self.stats.llm_summaries += 1,
                    Some("envelope") => self.stats.envelope_summaries += 1,
                    _ => self.stats.extractive_summaries += 1,
                }
            }
        }
        let sprouts = prune(
            output.sprouts,
            DecomposeBudget {
                max_depth: self.limits.max_depth.saturating_sub(item.depth).max(1),
                max_fragments: self.fragment_budget,
            },
        );
        let planted: usize = sprouts.iter().map(Sprout::count).sum();
        assert!(
            planted <= self.fragment_budget,
            "prune respects the fragment budget"
        );
        self.fragment_budget -= planted;
        self.plant(item.node, item.depth, item.reference_hops, sprouts, planted);
        self.keyed
            .extend(output.keyed.into_iter().map(|k| (item.node, k)));
        tracing::trace!(transform = %registration.name, planted, "absorbed");
    }

    /// Plant a sprout forest under `parent`, depth-first in emitted order:
    /// each sprout becomes a planned fragment, extends the inventory, is
    /// collected as an anchor site when it carries source text, and re-enters
    /// the queue for chained claims.
    fn plant(
        &mut self,
        parent: PlanNode,
        parent_depth: usize,
        parent_hops: u32,
        sprouts: Vec<Sprout>,
        planted: usize,
    ) {
        // Explicit stack, children pushed in reverse so they pop in order;
        // bounded by the forest size `prune` already enforced.
        let mut stack: Vec<(PlanNode, usize, u32, Sprout)> = Vec::with_capacity(planted);
        stack.extend(
            sprouts
                .into_iter()
                .rev()
                .map(|s| (parent, parent_depth, parent_hops, s)),
        );
        let mut popped: usize = 0;
        while let Some((parent, parent_depth, parent_hops, sprout)) = stack.pop() {
            popped += 1;
            assert!(popped <= planted, "planting visits each pruned sprout once");
            let Sprout {
                fragment,
                relation,
                children,
            } = sprout;
            // A fragment with a content reference is one hop further from
            // the source than its parent.
            let hops = parent_hops + u32::from(fragment.content_address.is_some());
            let node = self.push_fragment(parent, relation, fragment, parent_depth + 1, hops);
            stack.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|c| (node, parent_depth + 1, hops, c)),
            );
        }
        assert_eq!(popped, planted);
    }

    fn push_fragment(
        &mut self,
        parent: PlanNode,
        relation: inseam_kernel::fragment::RelationKind,
        fragment: NewFragment,
        depth: usize,
        reference_hops: u32,
    ) -> PlanNode {
        let index = u32::try_from(self.fragments.len()).expect("fragment cap fits u32");
        let node = PlanNode::Fragment(index);
        let essence = fragment.mimetype.essence().to_string();
        if self.inventory_seen.insert((essence.clone(), false)) {
            self.inventory.push(InventoryEntry {
                mimetype: essence,
                is_root: false,
            });
        }
        let is_derived = fragment.mimetype.is_inseam_defined();
        if let Some(text) = &fragment.text
            && !text.trim().is_empty()
            && !is_derived
        {
            self.texted.push((node, text.clone()));
        }
        // Chained transforms: emitted fragments re-enter claiming as
        // non-roots. Depth and hops ride along so recursion stays bounded:
        // a reference past the crawl depth is planted but never followed —
        // no transform sees it, no bytes are read for it.
        let within_depth = depth < self.limits.max_depth;
        let within_hops = reference_hops <= self.limits.max_reference_hops;
        if !is_derived && within_depth && within_hops {
            self.queue.push_back(WorkItem {
                node,
                mimetype: fragment.mimetype.clone(),
                is_root: false,
                text: fragment.text.clone(),
                content_address: fragment.content_address.clone(),
                depth,
                reference_hops,
            });
        }
        self.fragments.push(PlannedFragment {
            parent,
            relation,
            fragment,
        });
        node
    }

    fn finish(self, read: SourceRead, stamp: String) -> Planned {
        let keyed: Vec<PlannedKeyed> = self
            .keyed
            .into_iter()
            .map(|(input, sprout)| {
                let anchors = anchors_for(&sprout.anchor, input, &self.texted);
                assert!(!anchors.is_empty(), "every keyed sprout anchors somewhere");
                PlannedKeyed {
                    key: sprout.key,
                    fragment: sprout.fragment,
                    relation: sprout.relation,
                    anchors,
                }
            })
            .collect();
        let root_extent = match read.envelope.length {
            ContentLength::Lines(n) => Extent::Lines {
                start: 1,
                end: n.max(1),
            },
            ContentLength::Bytes(n) => Extent::Bytes { start: 0, end: n },
        };
        Planned {
            plan: SubtreePlan {
                address: read.source.address,
                envelope: read.envelope,
                raw_bytes: read.source.raw_bytes,
                root: NewFragment {
                    mimetype: self.root_mimetype,
                    text: None,
                    extent: Some(root_extent),
                    content_address: None,
                },
                fragments: self.fragments,
                keyed,
                shape: Shape {
                    stamp,
                    inventory: self.inventory,
                },
            },
            stats: self.stats,
            cache_entries: self.cache_entries,
        }
    }
}

/// The stamp the current registrations would produce for a subtree with this
/// inventory: participating transforms + the sweep's own shape fingerprint.
pub(super) fn expected_stamp(
    registrations: &[Arc<Registration>],
    inventory: &[InventoryEntry],
    sweep_shape: &str,
) -> String {
    let participants = participating(registrations, inventory);
    format!("{}+{}", shape_stamp(&participants), sweep_shape)
}

/// The fragments a keyed sprout's anchor resolves to within one source:
/// the input fragment, or every text-bearing source-content fragment whose
/// text contains the needle (case-insensitive), falling back to the input
/// when none does — so an emission is never silently dropped.
fn anchors_for(anchor: &Anchor, input: PlanNode, texted: &[(PlanNode, String)]) -> Vec<PlanNode> {
    match anchor {
        Anchor::Input => vec![input],
        Anchor::TextContaining(needle) => {
            let needle = needle.to_lowercase();
            let hits: Vec<PlanNode> = texted
                .iter()
                .filter(|(_, text)| text.to_lowercase().contains(&needle))
                .map(|(node, _)| *node)
                .collect();
            if hits.is_empty() { vec![input] } else { hits }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_anchors_fall_back_to_the_input() {
        let texted = vec![
            (PlanNode::Fragment(0), "Greg went home".to_string()),
            (PlanNode::Fragment(1), "nothing here".to_string()),
        ];
        let hits = anchors_for(
            &Anchor::TextContaining("greg".into()),
            PlanNode::Root,
            &texted,
        );
        assert_eq!(hits, vec![PlanNode::Fragment(0)]);
        let none = anchors_for(
            &Anchor::TextContaining("zed".into()),
            PlanNode::Root,
            &texted,
        );
        assert_eq!(none, vec![PlanNode::Root]);
        assert_eq!(
            anchors_for(&Anchor::Input, PlanNode::Fragment(1), &texted),
            vec![PlanNode::Fragment(1)]
        );
    }
}
