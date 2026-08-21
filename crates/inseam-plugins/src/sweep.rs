//! The `sweep` provider: the reconciling sweep, the index's only maintenance
//! mechanism (`design/index-maintenance.md`). Every run reconciles the
//! catalog against reality (what the connection enumerates) and against the
//! composition (what the mounted transforms would build): unchanged sources
//! are skipped, changed/interrupted/shape-stale ones have their subtree
//! rebuilt, vanished ones are removed, orphaned entities collected, and a
//! pending embedding migration performed before anything else.
//!
//! Plugin churn needs no hooks here: mounting or unmounting a transform
//! changes the registry snapshot, which changes the expected shape stamp of
//! exactly the sources whose mimetype inventory intersects the change —
//! dirtiness stays discovered, never triggered.
//!
//! Transform applications recurse: a transform claiming an emitted mimetype
//! (the loaded tier's normal shape) is applied to the emitted fragment in
//! the same rebuild, so chains resolve in one pass.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::Deserialize;

use inseam_kernel::address::ContentLength;
use inseam_kernel::dates::parse_ymd_epoch;
use inseam_kernel::address::Timestamp;
use inseam_kernel::fragment::{Extent, FragmentId, Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::ignore::{IgnoreRule, IgnoreSet};
use inseam_kernel::store::{IndexStore, InventoryEntry, SearchRow, SourceId};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError, Verdict, STORE,
};
use inseam_kernel::text::count_lines;
use inseam_seams::connection::{Connection, EnumeratedSource, CONNECTION};
use inseam_seams::embedder::{Embedder, EMBEDDER};
use inseam_seams::llm::{self, ChatMessage, ChatRequest, Llm, LlmCall, LLM};
use inseam_seams::sweep::{IndexReport, Sweep, SweepRequest, SWEEP};
use inseam_seams::transforms::{
    participating, shape_stamp, DecomposeBudget, ExtractedEntity, GrantedLlm, Registration,
    TransformCtx, Transforms, TRANSFORMS,
};
use inseam_seams::SeamError;

/// Search rows buffered before an embed+write flush.
const FLUSH_AT: usize = 128;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SweepConfig {
    /// Sources to deep-index per run; the rest still enter the catalog.
    /// 0 means unlimited. (Run-metering tier: bounds a run, not the shape.)
    pub max_sources: usize,
    /// Fragment cap per source (shape tier).
    pub max_fragments_per_source: usize,
    /// Decomposition depth cap (shape tier).
    pub max_depth: usize,
    /// Sources larger than this are cataloged but not content-indexed
    /// (shape tier).
    pub max_content_bytes: u64,
    /// `YYYY-MM-DD`; sources last modified before this are cataloged but not
    /// indexed. Tightening never evicts what a looser scope already built.
    pub modified_after: Option<String>,
    /// Host-agnostic ignore rules over addresses and envelopes
    /// (`design/ignore.md`). Unlike the cutoff, ignoring is membership, not
    /// scope: an ignored source is not cataloged, and one that was indexed
    /// before a rule covered it is removed like a vanished source.
    pub ignore: Vec<IgnoreRule>,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            max_sources: 0,
            max_fragments_per_source: 400,
            max_depth: 6,
            max_content_bytes: 2_000_000,
            modified_after: None,
            ignore: Vec::new(),
        }
    }
}

impl SweepConfig {
    /// The sweep's own contribution to every shape stamp: the decomposition
    /// dials that change what a subtree looks like. Run-metering fields stay
    /// out so tuning them never re-indexes.
    fn shape_fingerprint(&self) -> String {
        format!(
            "sweep-v1|depth={}|fragments={}|content_bytes={}",
            self.max_depth, self.max_fragments_per_source, self.max_content_bytes
        )
    }

    fn cutoff(&self) -> Result<Option<Timestamp>, PluginError> {
        self.modified_after
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| parse_ymd_epoch(s).map(Timestamp))
            .transpose()
            .map_err(|e| PluginError(format!("modified_after: {e}")))
    }
}

pub struct SweepPlugin {
    config: SweepConfig,
}

impl SweepPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        Ok(Self {
            config: parse_config(config)?,
        })
    }
}

pub struct SweepFactory;

impl inseam_kernel::substrate::PluginFactory for SweepFactory {
    fn name(&self) -> &str {
        "sweep"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(SweepPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for SweepPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[
            Inject::required("store"),
            Inject::required("connection"),
            Inject::required("transforms"),
            Inject::required("embedder"),
            Inject::optional("llm"),
        ];
        Manifest {
            name: "sweep",
            inject: INJECT,
            provides: &["sweep"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        self.config.cutoff()?; // fail on a bad date now, not mid-index
        let ignore = IgnoreSet::compile(&self.config.ignore)
            .map_err(|e| PluginError(e.to_string()))?; // and on a bad glob
        let transform_model = cx
            .facts("llm")
            .and_then(|f| f.str(llm::facts::TRANSFORM_MODEL))
            .unwrap_or_default()
            .to_string();
        let service = SweepService {
            store: cx.get(&STORE)?,
            connection: cx.get(&CONNECTION)?,
            transforms: cx.get(&TRANSFORMS)?,
            embedder: cx.get(&EMBEDDER)?,
            llm: cx.try_get(&LLM)?,
            transform_model,
            bus: cx.bus().clone(),
            config: self.config.clone(),
            ignore,
        };
        cx.provide(&SWEEP, Arc::new(service) as Arc<dyn Sweep>, Facts::new())?;
        Ok(())
    }
}

pub struct SweepService {
    store: Arc<IndexStore>,
    connection: Arc<dyn Connection>,
    transforms: Arc<dyn Transforms>,
    embedder: Arc<dyn Embedder>,
    llm: Option<Arc<dyn Llm>>,
    transform_model: String,
    bus: EventBus,
    config: SweepConfig,
    ignore: IgnoreSet,
}

#[async_trait::async_trait]
impl Sweep for SweepService {
    async fn sweep(&self, request: &SweepRequest) -> Result<IndexReport, SeamError> {
        let cutoff = self
            .config
            .cutoff()
            .map_err(|e| SeamError::failed(e.to_string()))?;
        let mut report = IndexReport::default();

        // A pending embedding migration blocks search: resolve it before the
        // sweep so even a zero-change run leaves the index queryable.
        if self.store.reembed_pending() {
            self.reembed(&mut report).await?;
        }

        let enumerated = self.connection.enumerate(&request.root).await?;
        report.sources_seen = enumerated.len();
        // Ignored sources leave the run here, before cataloging and before
        // vanished reconciliation — so a newly ignored source that was
        // indexed earlier is removed by the same path a deleted file takes.
        let sources: Vec<EnumeratedSource> = enumerated
            .into_iter()
            .filter(|s| !self.ignore.matches(&s.address, &s.envelope))
            .collect();
        report.ignored = report.sources_seen - sources.len();
        assert!(report.ignored + sources.len() == report.sources_seen);

        // The registry snapshot, with LLM-hungry registrations' fingerprints
        // extended by the transform model (a model change reshapes their
        // output; design/index-maintenance.md shape tier).
        let registrations = self.stamped_registrations();
        let sweep_shape = self.config.shape_fingerprint();

        let mut meters: HashMap<String, RunMeter> = HashMap::new();
        let mut pending: Vec<PendingRow> = Vec::new();

        for source in &sources {
            let meta = self.store.index_meta(&source.address).await?;
            let dirty = match &meta {
                None => true,
                Some(m) => {
                    let content_changed =
                        m.modified != source.envelope.modified || m.raw_bytes != source.raw_bytes;
                    let shape_stale = match &m.shape_stamp {
                        // Catalog-only rows carry no stamp and stay dirty.
                        None => true,
                        Some(stored) => {
                            let expected = expected_stamp(&registrations, &m.mimetypes, &sweep_shape);
                            *stored != expected
                        }
                    };
                    !m.indexed || content_changed || shape_stale
                }
            };

            if let (Some(cutoff), Some(modified)) = (cutoff, source.envelope.modified)
                && modified < cutoff
            {
                // Catalog newly seen out-of-horizon sources so the map is
                // complete; leave known ones alone — scope shrinkage never
                // evicts what a looser scope already built.
                if meta.is_none() {
                    self.store
                        .upsert_source(&source.address, &source.envelope, source.raw_bytes).await?;
                }
                report.skipped_cutoff += 1;
                continue;
            }
            if !dirty && !request.rebuild {
                report.unchanged += 1;
                continue;
            }
            let deep_budget_left =
                self.config.max_sources == 0 || report.indexed < self.config.max_sources;
            if !deep_budget_left {
                // Catalog-only: no stamp is recorded, so the source stays
                // dirty and is deep-indexed once a later run has budget.
                let sid = self
                    .store
                    .upsert_source(&source.address, &source.envelope, source.raw_bytes).await?;
                self.store.mark_indexed(sid, None).await?;
                report.catalog_only += 1;
                continue;
            }
            self.index_source(
                source,
                &registrations,
                &sweep_shape,
                &mut meters,
                &mut report,
                &mut pending,
            )
            .await?;
            report.indexed += 1;
            if pending.len() >= FLUSH_AT {
                self.flush(&mut pending, &mut report).await?;
            }
        }

        self.flush(&mut pending, &mut report).await?;
        self.reconcile_vanished(&request.root, &sources, &mut report)
            .await?;
        let orphaned = self.store.gc_entities().await?;
        report.entities_removed = orphaned.len();
        self.store.rebuild_fts().await?;
        for (entry, meter) in meters {
            let calls = meter.calls.load(Ordering::Relaxed);
            if calls > 0 {
                report.llm_calls.insert(entry, calls);
            }
        }
        if let Some(llm) = &self.llm {
            report.spent = llm.spent();
        }
        Ok(report)
    }
}

/// Per-transform, per-run LLM metering shared with the granted handles.
struct RunMeter {
    calls: Arc<AtomicUsize>,
    budget: usize,
}

impl SweepService {
    /// Registry snapshot with model-sensitive fingerprints resolved.
    fn stamped_registrations(&self) -> Vec<Arc<Registration>> {
        self.transforms
            .snapshot()
            .into_iter()
            .map(|r| {
                if r.llm_call_budget == 0 || self.transform_model.is_empty() {
                    r
                } else {
                    Arc::new(Registration {
                        entry_id: r.entry_id.clone(),
                        name: r.name.clone(),
                        transform: Arc::clone(&r.transform),
                        llm_call_budget: r.llm_call_budget,
                        shape_fingerprint: format!(
                            "{}|model={}",
                            r.shape_fingerprint, self.transform_model
                        ),
                    })
                }
            })
            .collect()
    }

    /// Build one source's subtree by recursive transform application:
    /// registered claimants over the root, then over every emitted fragment,
    /// until nothing claims the output (`design/indexing.md`). The source is
    /// marked indexed — with its stamp and inventory — only once everything
    /// is stored.
    async fn index_source(
        &self,
        source: &EnumeratedSource,
        registrations: &[Arc<Registration>],
        sweep_shape: &str,
        meters: &mut HashMap<String, RunMeter>,
        report: &mut IndexReport,
        pending: &mut Vec<PendingRow>,
    ) -> Result<(), SeamError> {
        let is_texty = source.envelope.content_type.is_indexable_text();
        let within_size = source.raw_bytes <= self.config.max_content_bytes;
        let content: Option<String> = if is_texty && within_size {
            Some(self.connection.read_text(&source.address).await?)
        } else {
            None
        };
        let wants_bytes = registrations
            .iter()
            .any(|r| r.transform.wants_bytes() && r.transform.claims(&source.envelope.content_type, true));
        let bytes: Option<Vec<u8>> = if wants_bytes && within_size {
            Some(self.connection.read_bytes(&source.address).await?)
        } else {
            None
        };

        let mut envelope = source.envelope.clone();
        if let Some(text) = &content {
            envelope.length = ContentLength::Lines(count_lines(text));
        }

        let sid = self
            .store
            .upsert_source(&source.address, &envelope, source.raw_bytes).await?;
        self.store.delete_fragments_of(sid).await?;

        let root_extent = match envelope.length {
            ContentLength::Lines(n) => Extent::Lines { start: 1, end: n.max(1) },
            ContentLength::Bytes(n) => Extent::Bytes { start: 0, end: n },
        };
        let root = self.store.insert_fragment(
            Some(sid),
            &NewFragment {
                mimetype: envelope.content_type.clone(),
                text: None,
                extent: Some(root_extent),
            },
        ).await?;
        self.store.set_root_fragment(sid, root).await?;
        report.fragments += 1;

        let mut fragment_budget = self.config.max_fragments_per_source;
        let mut texted: Vec<(FragmentId, String)> = Vec::new();
        let mut entities: Vec<ExtractedEntity> = Vec::new();
        let mut inventory: Vec<InventoryEntry> = vec![InventoryEntry {
            mimetype: envelope.content_type.essence().to_string(),
            is_root: true,
        }];
        let mut inventory_seen: HashSet<(String, bool)> =
            HashSet::from([(envelope.content_type.essence().to_string(), true)]);

        let mut queue: VecDeque<WorkItem> = VecDeque::from([WorkItem {
            fragment: root,
            mimetype: envelope.content_type.clone(),
            is_root: true,
            text: content.clone(),
            depth: 0,
        }]);

        while let Some(item) = queue.pop_front() {
            // Derived understanding is never source content: transforms must
            // not re-decompose summaries or entities, whatever they claim.
            if item.mimetype.is_inseam_defined() {
                continue;
            }
            for registration in registrations {
                if !registration.transform.claims(&item.mimetype, item.is_root) {
                    continue;
                }
                let meter = meters
                    .entry(registration.entry_id.clone())
                    .or_insert_with(|| RunMeter {
                        calls: Arc::new(AtomicUsize::new(0)),
                        budget: registration.llm_call_budget,
                    });
                // Capability mediation: the LLM handle is granted only while
                // the transform's per-run budget lasts; withheld, the
                // transform falls back or emits nothing. Every call also
                // passes the seam-level LlmCall guard.
                let granted: Option<Arc<MeteredLlm>> = match &self.llm {
                    Some(llm) if meter.calls.load(Ordering::Relaxed) < meter.budget => {
                        Some(Arc::new(MeteredLlm {
                            llm: Arc::clone(llm),
                            model: self.transform_model.clone(),
                            consumer: registration.entry_id.clone(),
                            calls: Arc::clone(&meter.calls),
                            budget: meter.budget,
                            bus: self.bus.clone(),
                        }))
                    }
                    _ => None,
                };
                let ctx = TransformCtx {
                    envelope: &envelope,
                    mimetype: &item.mimetype,
                    is_root: item.is_root,
                    text: item.text.as_deref(),
                    bytes: if item.is_root && registration.transform.wants_bytes() {
                        bytes.as_deref()
                    } else {
                        None
                    },
                    llm: granted.clone().map(|g| g as Arc<dyn GrantedLlm>),
                };
                let out = registration.transform.apply(ctx).await;

                for sprout in &out.sprouts {
                    if sprout.fragment.mimetype.is_summary() {
                        match sprout.fragment.mimetype.param("via") {
                            Some("llm") => report.llm_summaries += 1,
                            Some("envelope") => report.envelope_summaries += 1,
                            _ => report.extractive_summaries += 1,
                        }
                    }
                }
                let sprouts = inseam_seams::transforms::prune(
                    out.sprouts,
                    DecomposeBudget {
                        max_depth: self.config.max_depth.saturating_sub(item.depth).max(1),
                        max_fragments: fragment_budget,
                    },
                );
                let planted: usize = sprouts.iter().map(Sprout::count).sum();
                fragment_budget = fragment_budget.saturating_sub(planted);
                self.plant(
                    sid,
                    item.fragment,
                    item.depth,
                    sprouts,
                    report,
                    pending,
                    &mut texted,
                    &mut queue,
                    &mut inventory,
                    &mut inventory_seen,
                ).await?;
                entities.extend(out.entities);
            }
        }

        if !entities.is_empty() {
            self.wire_entities(root, entities, &texted, report, pending)
                .await?;
        }

        let stamp = expected_stamp(registrations, &inventory, sweep_shape);
        self.store.mark_indexed(sid, Some((&stamp, &inventory))).await?;
        tracing::debug!(address = %source.address, "indexed");
        Ok(())
    }

    /// Persist a sprout forest under `parent`, collecting text-bearing
    /// fragments for embedding and entity attachment, extending the mimetype
    /// inventory, and enqueueing emitted fragments for chained claims.
    #[allow(clippy::too_many_arguments)]
    async fn plant(
        &self,
        sid: SourceId,
        parent: FragmentId,
        parent_depth: usize,
        sprouts: Vec<Sprout>,
        report: &mut IndexReport,
        pending: &mut Vec<PendingRow>,
        texted: &mut Vec<(FragmentId, String)>,
        queue: &mut VecDeque<WorkItem>,
        inventory: &mut Vec<InventoryEntry>,
        inventory_seen: &mut HashSet<(String, bool)>,
    ) -> Result<(), SeamError> {
        for sprout in sprouts {
            let id = self.store.insert_fragment(Some(sid), &sprout.fragment).await?;
            self.store
                .insert_relation(&sprout.relation.edge(parent, id)).await?;
            report.fragments += 1;
            report.relations += 1;
            let essence = sprout.fragment.mimetype.essence().to_string();
            if inventory_seen.insert((essence.clone(), false)) {
                inventory.push(InventoryEntry {
                    mimetype: essence,
                    is_root: false,
                });
            }
            if let Some(text) = &sprout.fragment.text
                && !text.trim().is_empty()
            {
                pending.push(PendingRow {
                    fragment: id,
                    source: Some(sid),
                    text: text.clone(),
                });
                // Derived understanding (summaries) is searchable but not a
                // mention site: entities wire to source content only.
                if !sprout.fragment.mimetype.is_inseam_defined() {
                    texted.push((id, text.clone()));
                }
            }
            // Chained transforms: emitted fragments re-enter claiming as
            // non-roots. Depth rides along so recursion stays bounded.
            if !sprout.fragment.mimetype.is_inseam_defined()
                && parent_depth + 1 < self.config.max_depth
            {
                queue.push_back(WorkItem {
                    fragment: id,
                    mimetype: sprout.fragment.mimetype.clone(),
                    is_root: false,
                    text: sprout.fragment.text.clone(),
                    depth: parent_depth + 1,
                });
            }
            // Recursion is bounded by the sprout tree the transform
            // emitted; boxing breaks the async future cycle.
            Box::pin(self.plant(
                sid,
                id,
                parent_depth + 1,
                sprout.children,
                report,
                pending,
                texted,
                queue,
                inventory,
                inventory_seen,
            ))
            .await?;
        }
        Ok(())
    }

    /// Deduplicate extracted entities through the index-wide registry and
    /// wire `mentions` relations to the fragments whose text references
    /// them. This stays sweep-side: a transform cannot know fragment ids.
    async fn wire_entities(
        &self,
        root: FragmentId,
        extracted: Vec<ExtractedEntity>,
        texted: &[(FragmentId, String)],
        report: &mut IndexReport,
        pending: &mut Vec<PendingRow>,
    ) -> Result<(), SeamError> {
        for entity in extracted {
            let key = entity.key();
            let fragment = match self.store.entity_fragment(&key).await? {
                Some(f) => f,
                None => {
                    let f = self.store.insert_fragment(
                        None,
                        &NewFragment {
                            mimetype: Mimetype::entity().with_param("kind", entity.kind.as_str()),
                            text: Some(entity.name.clone()),
                            extent: None,
                        },
                    ).await?;
                    self.store.register_entity(&key, f).await?;
                    report.fragments += 1;
                    pending.push(PendingRow {
                        fragment: f,
                        source: None,
                        text: entity.name.clone(),
                    });
                    f
                }
            };
            // Relate the entity to the fragments that actually mention it,
            // falling back to the source root.
            let needle = entity.name.to_lowercase();
            let mut mentioned = false;
            for (fid, ftext) in texted {
                if ftext.to_lowercase().contains(&needle) {
                    self.store
                        .insert_relation(&RelationKind::Mentions.edge(*fid, fragment)).await?;
                    report.relations += 1;
                    mentioned = true;
                }
            }
            if !mentioned {
                self.store
                    .insert_relation(&RelationKind::Mentions.edge(root, fragment)).await?;
                report.relations += 1;
            }
            report.entities_seen += 1;
        }
        Ok(())
    }

    /// Remove cataloged sources under the swept scope that enumeration no
    /// longer sees. No tombstones: the index is derived, and a source that
    /// reappears is simply new.
    async fn reconcile_vanished(
        &self,
        root: &str,
        seen: &[EnumeratedSource],
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        // A scope with no stable locator prefix reconciles nothing rather
        // than guessing.
        let Some(prefix) = self.connection.locator_prefix(root) else {
            return Ok(());
        };
        let seen: HashSet<&str> = seen.iter().map(|s| s.address.locator.as_str()).collect();
        let child_prefix = format!("{prefix}/");
        for (sid, locator) in self.store.sources_of_host(self.connection.host()).await? {
            let under_root = locator == prefix || locator.starts_with(&child_prefix);
            if !under_root || seen.contains(locator.as_str()) {
                continue;
            }
            self.store.delete_source(sid).await?;
            report.removed += 1;
            tracing::debug!(locator, "removed vanished source");
        }
        Ok(())
    }

    /// Re-populate the search table under the declared embedding identity:
    /// text comes from SQLite, so no transform re-runs and no LLM spend —
    /// vectors are the only thing rebuilt.
    async fn reembed(&self, report: &mut IndexReport) -> Result<(), SeamError> {
        let targets = self.store.reembed_targets().await?;
        report.reembedded = targets.len();
        self.store.begin_reembed().await?;
        let mut pending: Vec<PendingRow> = Vec::new();
        for (fragment, source, text) in targets {
            pending.push(PendingRow {
                fragment,
                source,
                text,
            });
            if pending.len() >= FLUSH_AT {
                self.flush(&mut pending, report).await?;
            }
        }
        self.flush(&mut pending, report).await?;
        self.store.finish_reembed().await?;
        tracing::info!(rows = report.reembedded, "re-embedded search index");
        Ok(())
    }

    /// Embed buffered rows and land them in the search table.
    async fn flush(
        &self,
        pending: &mut Vec<PendingRow>,
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        if pending.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(pending);
        let vectors: Vec<Option<Vec<f32>>> = if self.embedder.dimensions().is_some() {
            let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
            match self.embedder.embed(&texts).await {
                Ok(vs) => {
                    report.embedded += vs.len();
                    vs.into_iter().map(Some).collect()
                }
                Err(e) => {
                    tracing::warn!("embedding failed; rows stay text-searchable only: {e}");
                    vec![None; rows.len()]
                }
            }
        } else {
            vec![None; rows.len()]
        };
        let search_rows: Vec<SearchRow> = rows
            .into_iter()
            .zip(vectors)
            .map(|(row, vector)| SearchRow {
                fragment: row.fragment,
                source: row.source,
                text: row.text,
                vector,
            })
            .collect();
        self.store.add_search_rows(&search_rows).await?;
        Ok(())
    }
}

/// The stamp the current registrations would produce for a subtree with this
/// inventory: participating transforms + the sweep's own shape fingerprint.
fn expected_stamp(
    registrations: &[Arc<Registration>],
    inventory: &[InventoryEntry],
    sweep_shape: &str,
) -> String {
    let participants = participating(registrations, inventory);
    format!("{}+{}", shape_stamp(&participants), sweep_shape)
}

struct WorkItem {
    fragment: FragmentId,
    mimetype: Mimetype,
    is_root: bool,
    text: Option<String>,
    depth: usize,
}

struct PendingRow {
    fragment: FragmentId,
    source: Option<SourceId>,
    text: String,
}

/// The narrowed LLM capability granted to one transform for one run:
/// mechanical call counting against the per-run budget, plus the seam-level
/// [`LlmCall`] guard — a denial from any policy listener refuses the call.
struct MeteredLlm {
    llm: Arc<dyn Llm>,
    model: String,
    consumer: String,
    calls: Arc<AtomicUsize>,
    budget: usize,
    bus: EventBus,
}

impl MeteredLlm {
    fn charge(&self) -> Result<(), SeamError> {
        if self.calls.load(Ordering::Relaxed) >= self.budget {
            return Err(SeamError::Refused(format!(
                "llm budget for `{}` is spent this run",
                self.consumer
            )));
        }
        if let Verdict::Deny(reason) = self.bus.check(&LlmCall {
            consumer: self.consumer.clone(),
        }) {
            return Err(SeamError::Refused(reason));
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[async_trait::async_trait]
impl GrantedLlm for MeteredLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String, SeamError> {
        self.charge()?;
        let request = ChatRequest::new(
            self.model.clone(),
            vec![ChatMessage::system(system), ChatMessage::user(user)],
        );
        let reply = self.llm.chat(&request).await?;
        Ok(reply.content.unwrap_or_default())
    }

    async fn describe_image(
        &self,
        prompt: &str,
        mimetype: &str,
        image: &[u8],
    ) -> Result<String, SeamError> {
        self.charge()?;
        self.llm
            .describe_image(&self.model, prompt, mimetype, image)
            .await
    }
}
