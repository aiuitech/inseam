//! The `sweep` provider: the reconciling sweep, the index's only maintenance
//! mechanism (`design/index-maintenance.md`). Every run reconciles the
//! catalog against reality (what the connection enumerates) and against the
//! composition (what the mounted transforms would build): unchanged sources
//! are skipped, changed/interrupted/shape-stale ones have their subtree
//! rebuilt, vanished ones are removed, unanchored keyed fragments collected, and a
//! pending embedding migration performed before anything else.
//!
//! Plugin churn needs no hooks here: mounting or unmounting a transform
//! changes the registry snapshot, which changes the expected shape stamp of
//! exactly the sources whose mimetype inventory intersects the change —
//! dirtiness stays discovered, never triggered.
//!
//! The run is a pipeline: dirty sources are **planned** concurrently
//! ([`plan`] — transforms are the slow part, so `concurrency` of them run at
//! once, each claimant of a fragment in flight together; `batch_concurrency`
//! of them when the LLM calls ride the batch lane, so one batch-API job
//! fills before it is submitted), each plan is
//! **landed** in one store transaction in enumeration order (deterministic
//! ids), and its search rows flow to the **embedding stage** ([`embed`]),
//! which embeds batches concurrently and lands them in order with the
//! `indexed` marks they complete.

pub mod ignore;

mod embed;
mod grant;
mod plan;

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use inseam_kernel::address::Timestamp;
use inseam_kernel::store::{
    CatalogEntry, CatalogMark, IndexStore, KeyedFragment, SourceCompletion, SubtreeWritten,
};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError, STORE,
};
use inseam_seams::connection::{Connections, EnumeratedSource, Registration as ConnectionRegistration, CONNECTIONS};
use inseam_seams::dates::parse_ymd_epoch;
use inseam_seams::embedder::{Embedder, EMBEDDER};
use inseam_seams::llm::{self, Llm, LlmLane, LLM};
use inseam_seams::sweep::{DeepBudget, IndexReport, Sweep, SweepRequest, SWEEP};
use inseam_seams::transforms::{Registration, Transforms, TRANSFORMS};
use inseam_seams::SeamError;

use embed::{EmbedStage, PendingRow, RowBuffer};
use grant::{Grantor, RunMeters};
use ignore::{IgnoreRule, IgnoreSet};
use plan::{expected_stamp, PlanLimits, Planned, Planner};

/// Catalog-only rows per transaction.
const CATALOG_CHUNK: usize = 1_000;
/// Deep-index sources this run, as a percentage of the sources already
/// indexed, at or above which the DiskANN index is dropped before landing
/// and rebuilt once at the end. Measured on a laptop, a search row inserts
/// ~36x slower through the index than without it, and a bulk build costs
/// under half an indexed insert per row: the break-even share is ~70%. The
/// dial sits below it because landing is the sweep's serial stage and the
/// rebuild runs once, after everything is in.
const VECTOR_INDEX_DEFER_PERCENT: u64 = 50;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SweepConfig {
    /// Sources to deep-index per run; the rest still enter the catalog.
    /// 0 means unlimited — the composition's spelling of [`DeepBudget`],
    /// which a request may override per run. (Run-metering tier: bounds a
    /// run, not the shape.)
    pub max_sources: u32,
    /// Sources planned at once — transform applications (LLM calls above
    /// all) in flight together. Run-metering tier: a throughput dial, never
    /// a shape one.
    pub concurrency: NonZeroUsize,
    /// Sources planned at once when transform LLM calls ride the batch lane
    /// (`design/indexing.md`). Each planner parks on its batch call, so this
    /// is how many requests one batch-API job can gather from a run before
    /// the endpoint submits it; it also bounds the parked sources' content
    /// held in memory. Run-metering tier.
    pub batch_concurrency: NonZeroUsize,
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
            concurrency: NonZeroUsize::new(8).expect("8 is non-zero"),
            batch_concurrency: NonZeroUsize::new(4_096).expect("4096 is non-zero"),
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
    /// The composition's deep budget, parsed once per run from the
    /// `max_sources` dial: `0` is unlimited, anything else a per-run cap.
    fn deep_budget(&self) -> DeepBudget {
        match NonZeroU32::new(self.max_sources) {
            None => DeepBudget::Unlimited,
            Some(limit) => DeepBudget::Sources(limit),
        }
    }

    fn shape_fingerprint(&self) -> String {
        format!(
            "sweep-v1|depth={}|fragments={}|content_bytes={}",
            self.max_depth, self.max_fragments_per_source, self.max_content_bytes
        )
    }

    fn limits(&self) -> PlanLimits {
        PlanLimits {
            max_depth: self.max_depth,
            max_fragments_per_source: self.max_fragments_per_source,
            max_content_bytes: self.max_content_bytes,
        }
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
            Inject::required("connections"),
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
            .facts(&LLM)
            .and_then(|f| f.str(llm::facts::TRANSFORM_MODEL))
            .unwrap_or_default()
            .to_string();
        let transform_reasoning_effort = cx
            .facts(&LLM)
            .and_then(|f| f.str(llm::facts::TRANSFORM_REASONING_EFFORT))
            .map(str::to_string);
        let transform_batch_model = cx
            .facts(&LLM)
            .and_then(|f| f.str(llm::facts::TRANSFORM_BATCH_MODEL))
            .map(str::to_string);
        let service = SweepService {
            store: cx.get(&STORE)?,
            connections: cx.get(&CONNECTIONS)?,
            transforms: cx.get(&TRANSFORMS)?,
            embedder: cx.get(&EMBEDDER)?,
            llm: cx.try_get(&LLM)?,
            transform_model,
            transform_reasoning_effort,
            transform_batch_model,
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
    connections: Arc<dyn Connections>,
    transforms: Arc<dyn Transforms>,
    embedder: Arc<dyn Embedder>,
    llm: Option<Arc<dyn Llm>>,
    transform_model: String,
    transform_reasoning_effort: Option<String>,
    /// The endpoint's batch-lane model, when it has a batch API.
    transform_batch_model: Option<String>,
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
        // Resolved per run, not at apply: connections come and go with
        // their own fibers, and the registry binding never changes identity
        // for it — mounting a mailbox does not restart the sweep.
        let steward = self.steward_of(&request.host)?;

        // A pending embedding migration blocks search: resolve it before the
        // sweep so even a zero-change run leaves the index queryable.
        if self.store.reembed_pending() {
            self.reembed(&mut report).await?;
        }

        let enumerated = steward.connection.enumerate(&request.root).await?;
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

        // A request's budget wins for this run only; the composition's dial
        // is the steady state every unqualified run returns to.
        let dials = RunDials {
            rebuild: request.rebuild,
            deep_budget: request.deep_budget.unwrap_or_else(|| self.config.deep_budget()),
        };
        let indexed_before = self.store.catalog_counts(None).await?.indexed;
        let batch_jobs_before = self.llm.as_ref().map_or(0, |llm| llm.batch_jobs());
        let decisions = self
            .decide(&sources, cutoff, &registrations, &sweep_shape, dials, &mut report)
            .await?;
        for chunk in decisions.catalog.chunks(CATALOG_CHUNK) {
            self.store.catalog_sources(chunk).await?;
        }

        let grantor = Arc::new(Grantor {
            llm: self.llm.clone(),
            model: self.transform_model.clone(),
            batch_model: self.transform_batch_model.clone(),
            lane_override: request.llm_lane,
            reasoning_effort: self.transform_reasoning_effort.clone(),
            bus: self.bus.clone(),
            meters: RunMeters::for_registrations(&registrations),
        });
        let planning = self.planning_concurrency(&grantor, &registrations, request.llm_lane);
        let planner = Arc::new(Planner {
            connection: Arc::clone(&steward.connection),
            registrations,
            grantor: Arc::clone(&grantor),
            sweep_shape,
            limits: self.config.limits(),
        });
        if defers_vector_index(decisions.deep.len(), indexed_before) {
            // The index comes back in `rebuild_fts` below; until then a
            // vector search builds it itself, as it does on a fresh node.
            report.vector_index_deferred = self.store.defer_search_vector_index().await?;
        }
        self.index_deep(decisions.deep, planner, planning, &mut report).await?;

        self.reconcile_vanished(&steward, &request.root, &sources, &mut report)
            .await?;
        let orphaned = self.store.gc_keyed_fragments().await?;
        report.keyed_removed = orphaned.len();
        self.store.rebuild_fts().await?;
        for (entry, calls) in grantor.meters.calls_by_entry() {
            report.llm_calls.insert(entry.to_string(), calls);
        }
        if let Some(llm) = &self.llm {
            report.spent = llm.spent();
            report.llm_batch_jobs = llm.batch_jobs().saturating_sub(batch_jobs_before);
        }
        Ok(report)
    }
}

/// The model a shape stamp names for a configured transform model: the
/// OpenRouter `:batch` variant is the same model on the batch lane, so the
/// suffix is dropped — an index built through either lane has one stamp.
fn lane_independent_model(transform_model: &str) -> &str {
    transform_model
        .strip_suffix(":batch")
        .filter(|base| !base.is_empty())
        .unwrap_or(transform_model)
}

/// Whether this run lands enough of the table that dropping the DiskANN
/// index first and rebuilding it once is faster than inserting through it.
/// A fresh node (nothing indexed yet) has no index to drop; the store
/// answers that with `false` on its own.
fn defers_vector_index(deep_sources: usize, indexed_before: u64) -> bool {
    let deep_sources = u64::try_from(deep_sources).expect("a source count fits in u64");
    if deep_sources == 0 {
        return false;
    }
    deep_sources.saturating_mul(100) >= indexed_before.saturating_mul(VECTOR_INDEX_DEFER_PERCENT)
}

/// The per-run dials a request sets: what the dirtiness pass bends to beyond
/// the composition (`design/index-maintenance.md`, run-metering tier).
#[derive(Debug, Clone, Copy)]
struct RunDials {
    /// Re-index sources even when unchanged.
    rebuild: bool,
    /// How many sources this run may deep-index; the rest are cataloged.
    deep_budget: DeepBudget,
}

/// What the dirtiness pass decided: rows that enter the catalog without a
/// subtree, and the sources to deep-index this run, both in enumeration
/// order.
struct Decisions<'a> {
    catalog: Vec<CatalogEntry<'a>>,
    deep: Vec<EnumeratedSource>,
}

impl SweepService {
    /// The connection stewarding `host`, if it can be swept at all: a
    /// fetch-only edge has nothing to enumerate and is refused by name.
    fn steward_of(&self, host: &inseam_kernel::address::HostId) -> Result<Arc<ConnectionRegistration>, SeamError> {
        let steward = self
            .connections
            .resolve(host)
            .ok_or_else(|| SeamError::UnknownHost(host.clone()))?;
        if !steward.capabilities.enumerates {
            return Err(SeamError::Unavailable(format!(
                "the connection to host `{host}` (entry `{}`) cannot enumerate sources, so it cannot be swept",
                steward.entry_id
            )));
        }
        Ok(steward)
    }

    /// Registry snapshot with model-sensitive fingerprints resolved. The
    /// stamp names the model, never the lane: the batch lane serves the same
    /// model through another door, so `--batch` and a lane change never
    /// re-index a source (`design/index-maintenance.md`, run-metering tier).
    fn stamped_registrations(&self) -> Vec<Arc<Registration>> {
        let stamp_model = lane_independent_model(&self.transform_model);
        self.transforms
            .snapshot()
            .into_iter()
            .map(|r| {
                if r.llm_call_budget == 0 || stamp_model.is_empty() {
                    r
                } else {
                    Arc::new(Registration {
                        entry_id: r.entry_id.clone(),
                        name: r.name.clone(),
                        transform: Arc::clone(&r.transform),
                        llm_call_budget: r.llm_call_budget,
                        llm_lane: r.llm_lane,
                        shape_fingerprint: format!("{}|model={stamp_model}", r.shape_fingerprint),
                    })
                }
            })
            .collect()
    }

    /// How many sources this run plans at once: `batch_concurrency` when
    /// granted calls ride the batch lane — every planner parks on its call,
    /// and the parked set is what fills one batch job — `concurrency`
    /// otherwise. A batch lane asked for without an endpoint batch model is
    /// said once, then served interactively.
    fn planning_concurrency(
        &self,
        grantor: &Grantor,
        registrations: &[Arc<Registration>],
        requested: Option<LlmLane>,
    ) -> NonZeroUsize {
        if grantor.plans_on_batch_lane(registrations) {
            tracing::info!(
                concurrency = self.config.batch_concurrency.get(),
                "transform llm calls ride the batch lane"
            );
            return self.config.batch_concurrency;
        }
        if requested == Some(LlmLane::Batch) && self.llm.is_some() {
            tracing::warn!(
                "the batch lane was requested but the llm endpoint declares no batch model; \
                 calls ride the interactive lane"
            );
        }
        self.config.concurrency
    }

    /// The dirtiness pass (`design/index-maintenance.md`): per source, is it
    /// past the cutoff, unchanged, past this run's deep budget, or to be
    /// rebuilt? Reads only; the writes it decides on are batched by the
    /// caller.
    async fn decide<'a>(
        &self,
        sources: &'a [EnumeratedSource],
        cutoff: Option<Timestamp>,
        registrations: &[Arc<Registration>],
        sweep_shape: &str,
        dials: RunDials,
        report: &mut IndexReport,
    ) -> Result<Decisions<'a>, SeamError> {
        let mut decisions = Decisions {
            catalog: Vec::new(),
            deep: Vec::new(),
        };
        for source in sources {
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
                            let expected = expected_stamp(registrations, &m.mimetypes, sweep_shape);
                            *stored != expected
                        }
                    };
                    !m.indexed || content_changed || shape_stale
                }
            };
            let entry = |mark: CatalogMark| CatalogEntry {
                address: &source.address,
                envelope: &source.envelope,
                raw_bytes: source.raw_bytes,
                mark,
            };

            if let (Some(cutoff), Some(modified)) = (cutoff, source.envelope.modified)
                && modified < cutoff
            {
                // Catalog newly seen out-of-horizon sources so the map is
                // complete; leave known ones alone — scope shrinkage never
                // evicts what a looser scope already built.
                if meta.is_none() {
                    decisions.catalog.push(entry(CatalogMark::Seen));
                }
                report.skipped_cutoff += 1;
                continue;
            }
            if !dirty && !dials.rebuild {
                report.unchanged += 1;
                continue;
            }
            let deep_count = u32::try_from(decisions.deep.len())
                .map_err(|_| SeamError::failed("more than u32::MAX sources chosen for deep indexing"))?;
            if !dials.deep_budget.allows(deep_count) {
                // Catalog-only: no stamp is recorded, so the source stays
                // dirty and is deep-indexed once a later run has budget.
                decisions.catalog.push(entry(CatalogMark::CatalogOnly));
                report.catalog_only += 1;
                continue;
            }
            decisions.deep.push(source.clone());
        }
        Ok(decisions)
    }

    /// The deep-index pipeline: plan `concurrency` sources at once, land each
    /// plan in enumeration order, and stream its search rows to the
    /// embedding stage. One source's failure fails the run, as before — and
    /// aborts the planners still in flight, so no LLM spend outlives the
    /// error.
    async fn index_deep(
        &self,
        deep: Vec<EnumeratedSource>,
        planner: Arc<Planner>,
        concurrency: NonZeroUsize,
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        let stage = EmbedStage::start(Arc::clone(&self.embedder), Arc::clone(&self.store));
        let mut buffer = RowBuffer::default();
        let planned = stream::iter(deep)
            .map(|source| {
                let planner = Arc::clone(&planner);
                Spawned(tokio::spawn(async move { planner.plan(&source).await }))
            })
            .buffered(concurrency.get());
        futures_util::pin_mut!(planned);
        while let Some(joined) = planned.next().await {
            let planned = joined??;
            let written = self.store.write_subtree(&planned.plan).await?;
            tracing::debug!(address = %planned.plan.address, "indexed");
            tally(report, &planned, &written);
            buffer.push_rows(search_rows_of(&planned, &written));
            buffer.push_completion(SourceCompletion {
                source: written.source,
                stamp: planned.plan.shape.stamp,
                inventory: planned.plan.shape.inventory,
            });
            for batch in buffer.drain_ready() {
                stage.submit(batch).await?;
            }
        }
        for batch in buffer.drain_all() {
            stage.submit(batch).await?;
        }
        report.embedded += stage.finish().await?;
        Ok(())
    }

    /// Remove cataloged sources under the swept scope that enumeration no
    /// longer sees. No tombstones: the index is derived, and a source that
    /// reappears is simply new.
    async fn reconcile_vanished(
        &self,
        steward: &ConnectionRegistration,
        root: &str,
        seen: &[EnumeratedSource],
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        // A scope with no stable locator prefix reconciles nothing rather
        // than guessing.
        let Some(prefix) = steward.connection.locator_prefix(root) else {
            return Ok(());
        };
        let seen: HashSet<&str> = seen.iter().map(|s| s.address.locator.as_str()).collect();
        for (sid, locator) in self.store.sources_of_host(&steward.host.id).await? {
            if !scope_covers(&prefix, &locator) || seen.contains(locator.as_str()) {
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
    /// vectors are the only thing rebuilt, through the same embedding stage
    /// an index run uses.
    async fn reembed(&self, report: &mut IndexReport) -> Result<(), SeamError> {
        let targets = self.store.reembed_targets().await?;
        report.reembedded = targets.len();
        self.store.begin_reembed().await?;
        let stage = EmbedStage::start(Arc::clone(&self.embedder), Arc::clone(&self.store));
        let mut buffer = RowBuffer::default();
        for target in targets {
            buffer.push_rows([PendingRow {
                fragment: target.fragment,
                source: target.source,
                text: target.text,
                is_summary: target.is_summary,
            }]);
            for batch in buffer.drain_ready() {
                stage.submit(batch).await?;
            }
        }
        for batch in buffer.drain_all() {
            stage.submit(batch).await?;
        }
        report.embedded += stage.finish().await?;
        self.store.finish_reembed().await?;
        tracing::info!(rows = report.reembedded, "re-embedded search index");
        Ok(())
    }
}

/// Whether a swept scope's locator prefix covers a cataloged locator: the
/// prefix itself, anything under it (`<prefix>/…`), and — for the empty
/// prefix, which a flat id space answers for its "everything" scope — every
/// locator of the host (`Connection::locator_prefix`).
fn scope_covers(prefix: &str, locator: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if locator == prefix {
        return true;
    }
    locator
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Fold one landed plan into the run report.
fn tally(report: &mut IndexReport, planned: &Planned, written: &SubtreeWritten) {
    let created_keyed = written
        .keyed
        .iter()
        .filter(|k| matches!(k, KeyedFragment::Created(_)))
        .count();
    let anchors: usize = planned.plan.keyed.iter().map(|k| k.anchors.len()).sum();
    report.indexed += 1;
    report.fragments += planned.plan.fragment_count() + created_keyed;
    report.relations += planned.plan.fragments.len() + anchors;
    report.keyed_anchored += planned.plan.keyed.len();
    report.llm_summaries += planned.stats.llm_summaries;
    report.extractive_summaries += planned.stats.extractive_summaries;
    report.envelope_summaries += planned.stats.envelope_summaries;
}

/// The search rows a landed plan contributes: every text-bearing planned
/// fragment, plus the keyed fragments this plan created (an existing keyed
/// fragment already has its row).
fn search_rows_of(planned: &Planned, written: &SubtreeWritten) -> Vec<PendingRow> {
    let fragments = planned
        .plan
        .fragments
        .iter()
        .zip(&written.fragments)
        .filter_map(|(p, id)| {
            p.fragment
                .text
                .as_ref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| PendingRow {
                    fragment: *id,
                    source: Some(written.source),
                    text: t.clone(),
                    is_summary: p.fragment.mimetype.is_summary(),
                })
        });
    let keyed = planned
        .plan
        .keyed
        .iter()
        .zip(&written.keyed)
        .filter_map(|(p, resolved)| match resolved {
            KeyedFragment::Created(id) => p
                .fragment
                .text
                .as_ref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| PendingRow {
                    fragment: *id,
                    source: None,
                    text: t.clone(),
                    is_summary: p.fragment.mimetype.is_summary(),
                }),
            KeyedFragment::Existing(_) => None,
        });
    let mut rows: Vec<PendingRow> = fragments.chain(keyed).collect();
    rows.shrink_to_fit();
    rows
}

/// A spawned task whose handle aborts it on drop — so dropping the planning
/// stream on an error cancels the planners still in flight instead of
/// letting them finish (and spend) unobserved.
struct Spawned<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Spawned<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> std::future::Future for Spawned<T> {
    type Output = Result<T, SeamError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0)
            .poll(cx)
            .map(|joined| joined.map_err(|e| SeamError::failed(format!("planner task failed: {e}"))))
    }
}

#[cfg(test)]
mod tests {
    use super::{lane_independent_model, scope_covers};

    #[test]
    fn the_stamp_model_drops_the_batch_lane_suffix() {
        assert_eq!(
            lane_independent_model("google/gemini-2.5-flash-lite:batch"),
            "google/gemini-2.5-flash-lite"
        );
        assert_eq!(
            lane_independent_model("google/gemini-2.5-flash-lite"),
            "google/gemini-2.5-flash-lite"
        );
        assert_eq!(lane_independent_model(":batch"), ":batch");
        assert_eq!(lane_independent_model(""), "");
    }

    #[test]
    fn scope_covers_the_prefix_its_children_and_everything_for_the_empty_prefix() {
        assert!(scope_covers("Users/greg/Notes", "Users/greg/Notes"));
        assert!(scope_covers("Users/greg/Notes", "Users/greg/Notes/a.md"));
        assert!(!scope_covers("Users/greg/Notes", "Users/greg/Notes-old/a.md"));
        assert!(!scope_covers("Users/greg/Notes", "Users/greg"));
        assert!(scope_covers("", "any/locator/at/all"));
        assert!(scope_covers("", "1a2b3c"));
    }
}

#[cfg(test)]
mod defer_tests {
    use super::defers_vector_index;

    #[test]
    fn defers_only_when_the_run_lands_a_large_share() {
        // (deep sources this run, sources indexed before, defers?)
        let cases: &[(usize, u64, bool)] = &[
            (0, 0, false),
            (0, 1_000, false),
            (1, 0, true),
            (1, 1_000, false),
            (499, 1_000, false),
            (500, 1_000, true),
            (1_000, 1_000, true),
            (usize::MAX, u64::MAX, true),
        ];
        for (deep, indexed, expected) in cases {
            assert_eq!(defers_vector_index(*deep, *indexed), *expected, "deep={deep} indexed={indexed}");
        }
    }
}
