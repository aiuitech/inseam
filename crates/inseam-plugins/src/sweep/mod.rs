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
//! `indexed` marks they complete. Folders run through the same pipeline
//! after every file, deepest level first, their content composed from the
//! children that just landed ([`folders`]).

pub mod ignore;

mod cache;
mod embed;
mod folders;
mod grant;
mod plan;
mod vocabulary;

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use inseam_kernel::address::Timestamp;
use inseam_kernel::store::{
    CatalogEntry, CatalogMark, IndexStore, KeyedFragment, SearchRole, SourceCompletion,
    SubtreeWritten,
};
use inseam_kernel::substrate::{
    ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError, STORE, parse_config,
};
use inseam_seams::SeamError;
use inseam_seams::connection::{
    CONNECTIONS, Connections, EnumeratedSource, Registration as ConnectionRegistration,
};
use inseam_seams::dates::parse_ymd_epoch;
use inseam_seams::embedder::{EMBEDDER, Embedder};
use inseam_seams::llm::{self, LLM, Llm, LlmLane};
use inseam_seams::sweep::{
    DeepBudget, IndexControl, IndexPhase, IndexProgress, IndexReport, SWEEP, Sweep, SweepRequest,
};
use inseam_seams::transforms::{Registration, TRANSFORMS, Transforms};

use embed::{EmbedStage, PendingRow, RowBuffer};
use grant::{Grantor, RunMeters};
use ignore::{IgnoreRule, IgnoreSet};
use plan::{PlanContent, PlanInput, PlanLimits, Planned, Planner, expected_stamp};
pub use vocabulary::VocabularyConfig;
use vocabulary::{LLM_CONSUMER, VocabularyPass};

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
/// A paused app run polls four times per second for at most 24 hours. iOS
/// cannot keep a foreground run alive longer than that in practice, and a
/// fixed ceiling keeps a forgotten pause from retaining pipeline memory.
const PAUSE_POLLS_MAX: u32 = 24 * 60 * 60 * 4;
const PAUSE_POLL_MS: u64 = 250;

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
    /// Source content reads allowed at once. Batch planners park after the
    /// read, so this bounds file descriptors without shrinking LLM jobs.
    /// Run-metering tier.
    pub source_reads_in_flight_max: NonZeroUsize,
    /// Fragment cap per source (shape tier).
    pub max_fragments_per_source: usize,
    /// Decomposition depth cap (shape tier).
    pub max_depth: usize,
    /// Sources larger than this are cataloged but not content-indexed
    /// (shape tier).
    pub max_content_bytes: u64,
    /// How many content references a chain may follow away from a source
    /// (shape tier): the crawl depth. `1` follows what a source itself
    /// links to and stops there; `2` also follows what that content links
    /// to; `0` stores references without ever reading them. Beyond the
    /// cap a reference is still a fragment — expandable, fetchable — but
    /// no transform is applied to it and no bytes are read for it.
    pub max_reference_hops: u32,
    /// `YYYY-MM-DD`; sources last modified before this are cataloged but not
    /// indexed. Tightening never evicts what a looser scope already built.
    pub modified_after: Option<String>,
    /// Host-agnostic ignore rules over addresses and envelopes
    /// (`design/ignore.md`). Unlike the cutoff, ignoring is membership, not
    /// scope: an ignored source is not cataloged, and one that was indexed
    /// before a rule covered it is removed like a vanished source.
    pub ignore: Vec<IgnoreRule>,
    /// The vocabulary pass (`design/vocabulary.md`): mining, matching,
    /// clustering, and grounding after every file has landed. Its own
    /// digest, never the shape stamp.
    pub vocabulary: VocabularyConfig,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            max_sources: 0,
            concurrency: NonZeroUsize::new(8).expect("8 is non-zero"),
            batch_concurrency: NonZeroUsize::new(4_096).expect("4096 is non-zero"),
            source_reads_in_flight_max: NonZeroUsize::new(128).expect("128 is non-zero"),
            max_fragments_per_source: 400,
            max_depth: 6,
            max_content_bytes: 2_000_000,
            max_reference_hops: 1,
            modified_after: None,
            ignore: Vec::new(),
            vocabulary: VocabularyConfig::default(),
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
            "sweep-v1|depth={}|fragments={}|content_bytes={}|hops={}",
            self.max_depth,
            self.max_fragments_per_source,
            self.max_content_bytes,
            self.max_reference_hops
        )
    }

    fn limits(&self) -> PlanLimits {
        PlanLimits {
            max_depth: self.max_depth,
            max_fragments_per_source: self.max_fragments_per_source,
            max_content_bytes: self.max_content_bytes,
            max_reference_hops: self.max_reference_hops,
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
        let ignore =
            IgnoreSet::compile(&self.config.ignore).map_err(|e| PluginError(e.to_string()))?; // and on a bad glob
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
        let mut report = IndexReport::default();
        let Some(prepared) = self.prepare_run(request, &mut report).await? else {
            return Ok(report);
        };
        let finalization = self.execute_run(request, prepared, &mut report).await?;
        self.finalize_run(request, finalization, &mut report)
            .await?;
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

/// What the folder pass carries from the file pass: the registry snapshot
/// and sweep shape the stamps are checked against, the run's dials, and how
/// many sources the run has already chosen to deep-index — folders count
/// against the same budget.
struct FolderRun<'a> {
    registrations: &'a [Arc<Registration>],
    sweep_shape: &'a str,
    dials: RunDials,
    deep_count: u32,
}

/// One level's folder decisions: catalog rows and the folders to plan with
/// their composed content.
struct FolderDecisions<'a> {
    catalog: Vec<CatalogEntry<'a>>,
    deep: Vec<PlanInput>,
}

struct EnumeratedRun {
    steward: Arc<ConnectionRegistration>,
    sources: Vec<EnumeratedSource>,
    folders: Vec<EnumeratedSource>,
    files: Vec<EnumeratedSource>,
}

struct PreparedRun {
    steward: Arc<ConnectionRegistration>,
    sources: Vec<EnumeratedSource>,
    folders: Vec<EnumeratedSource>,
    registrations: Vec<Arc<Registration>>,
    sweep_shape: String,
    dials: RunDials,
    indexed_before: u64,
    batch_jobs_before: u64,
    deep: Vec<EnumeratedSource>,
    deep_count: u32,
}

struct Finalization {
    grantor: Arc<Grantor>,
    batch_jobs_before: u64,
}

impl SweepService {
    async fn prepare_run(
        &self,
        request: &SweepRequest,
        report: &mut IndexReport,
    ) -> Result<Option<PreparedRun>, SeamError> {
        let cutoff = self
            .config
            .cutoff()
            .map_err(|error| SeamError::failed(error.to_string()))?;
        let Some(run) = self.enumerate_run(request, report).await? else {
            return Ok(None);
        };
        if self
            .stop_at_phase(request, IndexPhase::Cataloging, report)
            .await
        {
            return Ok(None);
        }
        let registrations = self.stamped_registrations();
        let sweep_shape = self.config.shape_fingerprint();
        let dials = self.run_dials(request);
        let indexed_before = self.store.catalog_counts(None).await?.indexed;
        let batch_jobs_before = self.llm.as_ref().map_or(0, |llm| llm.batch_jobs());
        let decisions = self
            .decide(
                &run.files,
                cutoff,
                &registrations,
                &sweep_shape,
                dials,
                report,
            )
            .await?;
        for chunk in decisions.catalog.chunks(CATALOG_CHUNK) {
            self.store.catalog_sources(chunk).await?;
        }
        if self
            .stop_at_phase(request, IndexPhase::Indexing, report)
            .await
        {
            return Ok(None);
        }
        let deep_count = u32::try_from(decisions.deep.len()).map_err(|_| {
            SeamError::failed("more than u32::MAX sources chosen for deep indexing")
        })?;
        Ok(Some(PreparedRun {
            steward: run.steward,
            sources: run.sources,
            folders: run.folders,
            registrations,
            sweep_shape,
            dials,
            indexed_before,
            batch_jobs_before,
            deep: decisions.deep,
            deep_count,
        }))
    }

    async fn enumerate_run(
        &self,
        request: &SweepRequest,
        report: &mut IndexReport,
    ) -> Result<Option<EnumeratedRun>, SeamError> {
        if self
            .stop_at_phase(request, IndexPhase::Preparing, report)
            .await
        {
            return Ok(None);
        }
        // Connections come and go without changing the registry binding,
        // so resolve the steward for each run rather than at plugin apply.
        let steward = self.steward_of(&request.host)?;
        // A pending migration blocks search. Resolve it before enumeration
        // so even a zero-change run leaves the index queryable.
        if self.store.reembed_pending() {
            self.reembed(report).await?;
        }
        if self
            .stop_at_phase(request, IndexPhase::Enumerating, report)
            .await
        {
            return Ok(None);
        }
        let enumerated = steward.connection.enumerate(&request.root).await?;
        Ok(Some(self.admit_enumerated(steward, enumerated, report)))
    }

    fn admit_enumerated(
        &self,
        steward: Arc<ConnectionRegistration>,
        enumerated: Vec<EnumeratedSource>,
        report: &mut IndexReport,
    ) -> EnumeratedRun {
        report.sources_seen = enumerated.len();
        let admitted: Vec<EnumeratedSource> = enumerated
            .into_iter()
            .filter(|source| !self.ignore.matches(&source.address, &source.envelope))
            .collect();
        let (folders, files): (Vec<EnumeratedSource>, Vec<EnumeratedSource>) =
            admitted.into_iter().partition(folders::is_folder);
        let folders = folders::retain_holding(folders, &files);
        let sources = files.iter().chain(folders.iter()).cloned().collect();
        report.ignored = report.sources_seen - files.len() - folders.len();
        assert!(report.ignored + files.len() + folders.len() == report.sources_seen);
        EnumeratedRun {
            steward,
            sources,
            folders,
            files,
        }
    }

    fn run_dials(&self, request: &SweepRequest) -> RunDials {
        RunDials {
            rebuild: request.rebuild,
            deep_budget: request
                .deep_budget
                .unwrap_or_else(|| self.config.deep_budget()),
        }
    }

    async fn execute_run(
        &self,
        request: &SweepRequest,
        run: PreparedRun,
        report: &mut IndexReport,
    ) -> Result<Finalization, SeamError> {
        let PreparedRun {
            steward,
            sources,
            folders,
            registrations,
            sweep_shape,
            dials,
            indexed_before,
            batch_jobs_before,
            deep,
            deep_count,
        } = run;
        let grantor = Arc::new(Grantor {
            llm: self.llm.clone(),
            model: self.transform_model.clone(),
            batch_model: self.transform_batch_model.clone(),
            lane_override: request.llm_lane,
            reasoning_effort: self.transform_reasoning_effort.clone(),
            bus: self.bus.clone(),
            meters: RunMeters::for_registrations(&registrations)
                .with_meter(LLM_CONSUMER, self.config.vocabulary.cluster_llm_budget),
        });
        let planning = self.planning_concurrency(&grantor, &registrations, request.llm_lane);
        let planner =
            Arc::new(self.planner_for(&steward, registrations, sweep_shape, Arc::clone(&grantor)));
        if defers_vector_index(deep.len(), indexed_before) {
            report.vector_index_deferred = self.store.defer_search_vector_index().await?;
        }
        let files = deep.into_iter().map(PlanInput::from_host).collect();
        self.index_deep(files, Arc::clone(&planner), planning, request, report)
            .await?;
        if !report.stopped {
            self.reconcile_vanished(&steward, &request.root, &sources, report)
                .await?;
            if self.config.vocabulary.enabled {
                let changed = report.indexed > 0 || report.removed > 0;
                let pass = VocabularyPass {
                    store: &self.store,
                    embedder: self.embedder.as_ref(),
                    grantor: &grantor,
                    config: &self.config.vocabulary,
                };
                let vocabulary = pass.run(changed).await?;
                tracing::info!("{vocabulary}");
                report.vocabulary = Some(vocabulary);
            }
            let folder_run = FolderRun {
                registrations: &planner.registrations,
                sweep_shape: &planner.sweep_shape,
                dials,
                deep_count,
            };
            self.index_folders(
                folders,
                folder_run,
                Arc::clone(&planner),
                planning,
                request,
                report,
            )
            .await?;
        }
        Ok(Finalization {
            grantor,
            batch_jobs_before,
        })
    }

    fn planner_for(
        &self,
        steward: &Arc<ConnectionRegistration>,
        registrations: Vec<Arc<Registration>>,
        sweep_shape: String,
        grantor: Arc<Grantor>,
    ) -> Planner {
        Planner {
            connection: Arc::clone(&steward.connection),
            connections: Arc::clone(&self.connections),
            store: Arc::clone(&self.store),
            source_read_permits: Arc::new(Semaphore::new(
                self.config.source_reads_in_flight_max.get(),
            )),
            registrations,
            grantor,
            sweep_shape,
            limits: self.config.limits(),
        }
    }

    async fn finalize_run(
        &self,
        request: &SweepRequest,
        finalization: Finalization,
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        if self
            .checkpoint(request, IndexPhase::Finalizing, None, report)
            .await
        {
            report.stopped = true;
        }
        report.keyed_removed = self.store.gc_keyed_fragments().await?.len();
        self.store.rebuild_fts().await?;
        for (entry, calls) in finalization.grantor.meters.calls_by_entry() {
            report.llm_calls.insert(entry.to_string(), calls);
        }
        if let Some(llm) = &self.llm {
            report.spent = llm.spent();
            report.llm_batch_jobs = llm
                .batch_jobs()
                .saturating_sub(finalization.batch_jobs_before);
        }
        let _ = self
            .checkpoint(request, IndexPhase::Complete, None, report)
            .await;
        Ok(())
    }

    async fn stop_at_phase(
        &self,
        request: &SweepRequest,
        phase: IndexPhase,
        report: &mut IndexReport,
    ) -> bool {
        if !self.checkpoint(request, phase, None, report).await {
            return false;
        }
        self.mark_stopped(request, report).await;
        true
    }

    async fn mark_stopped(&self, request: &SweepRequest, report: &mut IndexReport) {
        report.stopped = true;
        let _ = self
            .checkpoint(request, IndexPhase::Complete, None, report)
            .await;
    }

    /// Publish one progress snapshot and honor the owner's command. Pausing
    /// sleeps on the async runtime rather than holding an executor thread.
    async fn checkpoint(
        &self,
        request: &SweepRequest,
        phase: IndexPhase,
        current: Option<inseam_kernel::address::Address>,
        report: &IndexReport,
    ) -> bool {
        let Some(monitor) = &request.monitor else {
            return false;
        };
        let complete = report
            .indexed
            .saturating_add(report.unchanged)
            .saturating_add(report.catalog_only)
            .saturating_add(report.skipped_cutoff)
            .saturating_add(report.ignored)
            .min(report.sources_seen);
        let progress = IndexProgress {
            phase,
            sources_complete: complete,
            sources_total: report.sources_seen,
            current,
            indexed: report.indexed,
            unchanged: report.unchanged,
            catalog_only: report.catalog_only,
            ignored: report.ignored,
            stopped: report.stopped,
        };
        for _poll_index in 0..PAUSE_POLLS_MAX {
            match monitor.update(&progress) {
                IndexControl::Continue => return false,
                IndexControl::Stop => return true,
                IndexControl::Pause => {
                    tokio::time::sleep(std::time::Duration::from_millis(PAUSE_POLL_MS)).await;
                }
            }
        }
        true
    }

    /// The connection stewarding `host`, if it can be swept at all: a
    /// fetch-only edge has nothing to enumerate and is refused by name.
    fn steward_of(
        &self,
        host: &inseam_kernel::address::HostId,
    ) -> Result<Arc<ConnectionRegistration>, SeamError> {
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
            let deep_count = u32::try_from(decisions.deep.len()).map_err(|_| {
                SeamError::failed("more than u32::MAX sources chosen for deep indexing")
            })?;
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
        deep: Vec<PlanInput>,
        planner: Arc<Planner>,
        concurrency: NonZeroUsize,
        request: &SweepRequest,
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        if deep.is_empty() {
            return Ok(());
        }
        let mut stage = EmbedStage::start(Arc::clone(&self.embedder), Arc::clone(&self.store));
        let mut buffer = RowBuffer::default();
        let planned = stream::iter(deep)
            .map(|input| {
                let planner = Arc::clone(&planner);
                Spawned(tokio::spawn(async move { planner.plan(&input).await }))
            })
            .buffered(concurrency.get());
        futures_util::pin_mut!(planned);
        while let Some(joined) = planned.next().await {
            let planned = joined??;
            let written = self.store.write_subtree(&planned.plan).await?;
            self.store
                .remember_transform_outputs(&planned.cache_entries)
                .await?;
            tracing::debug!(address = %planned.plan.address, "indexed");
            tally(report, &planned, &written);
            let current = Some(planned.plan.address.clone());
            buffer.push_rows(search_rows_of(&planned, &written));
            buffer.push_completion(SourceCompletion {
                source: written.source,
                stamp: planned.plan.shape.stamp,
                inventory: planned.plan.shape.inventory,
            });
            for batch in buffer.drain_ready() {
                stage.submit(batch).await?;
            }
            if self
                .checkpoint(request, IndexPhase::Indexing, current, report)
                .await
            {
                report.stopped = true;
                break;
            }
        }
        for batch in buffer.drain_all() {
            stage.submit(batch).await?;
        }
        let totals = stage.finish().await?;
        report.embedded += totals.embedded;
        report.embeddings_reused += totals.reused;
        Ok(())
    }

    /// The folder pass (`folders`): deepest level first, each level decided
    /// against the catalog the levels below just landed, then planned and
    /// landed through the same pipeline as files. The level loop is bounded
    /// by the walk's depth cap.
    async fn index_folders(
        &self,
        folders: Vec<EnumeratedSource>,
        mut run: FolderRun<'_>,
        planner: Arc<Planner>,
        concurrency: NonZeroUsize,
        request: &SweepRequest,
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        for level in folders::levels(folders) {
            let decisions = self.decide_folders(&level, &mut run, report).await?;
            for chunk in decisions.catalog.chunks(CATALOG_CHUNK) {
                self.store.catalog_sources(chunk).await?;
            }
            self.index_deep(
                decisions.deep,
                Arc::clone(&planner),
                concurrency,
                request,
                report,
            )
            .await?;
            if report.stopped {
                break;
            }
        }
        Ok(())
    }

    /// The folder dirtiness pass for one level: compose each folder's
    /// listing from the catalog, compare its digest with the recorded one,
    /// and spend the run's deep budget on the dirty ones. Folders have no
    /// modified-after cutoff of their own — a directory's timestamp is not
    /// its content's recency — but a folder none of whose children is
    /// deep-indexed yet stays catalog-only, so the cutoff and the budget
    /// reach it through them.
    async fn decide_folders<'a>(
        &self,
        level: &'a [EnumeratedSource],
        run: &mut FolderRun<'_>,
        report: &mut IndexReport,
    ) -> Result<FolderDecisions<'a>, SeamError> {
        let mut decisions = FolderDecisions {
            catalog: Vec::new(),
            deep: Vec::new(),
        };
        for folder in level {
            let content = folders::compose_content(&self.store, folder).await?;
            let meta = self.store.index_meta(&folder.address).await?;
            let dirty = folders::is_dirty(
                meta.as_ref(),
                &content.digest,
                run.registrations,
                run.sweep_shape,
            );
            if !dirty && !run.dials.rebuild {
                report.unchanged += 1;
                continue;
            }
            let has_indexed_child = content.indexed_children > 0;
            if !has_indexed_child || !run.dials.deep_budget.allows(run.deep_count) {
                decisions.catalog.push(CatalogEntry {
                    address: &folder.address,
                    envelope: &folder.envelope,
                    raw_bytes: folder.raw_bytes,
                    mark: CatalogMark::CatalogOnly,
                });
                report.catalog_only += 1;
                continue;
            }
            run.deep_count = run.deep_count.checked_add(1).ok_or_else(|| {
                SeamError::failed("more than u32::MAX sources chosen for deep indexing")
            })?;
            decisions.deep.push(PlanInput {
                source: folder.clone(),
                content: PlanContent::Composed(content.text),
            });
        }
        Ok(decisions)
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
        let mut stage = EmbedStage::start(Arc::clone(&self.embedder), Arc::clone(&self.store));
        let mut buffer = RowBuffer::default();
        for target in targets {
            buffer.push_rows([PendingRow {
                fragment: target.fragment,
                source: target.source,
                text: target.text,
                role: target.role,
            }]);
            for batch in buffer.drain_ready() {
                stage.submit(batch).await?;
            }
        }
        for batch in buffer.drain_all() {
            stage.submit(batch).await?;
        }
        let totals = stage.finish().await?;
        report.embedded += totals.embedded;
        report.embeddings_reused += totals.reused;
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
    report.verbatim_summaries += planned.stats.verbatim_summaries;
    report.llm_summaries += planned.stats.llm_summaries;
    report.extractive_summaries += planned.stats.extractive_summaries;
    report.envelope_summaries += planned.stats.envelope_summaries;
    report.transforms_reused += planned.stats.transforms_reused;
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
                    role: SearchRole::of(&p.fragment.mimetype, false),
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
                    role: SearchRole::of(&p.fragment.mimetype, true),
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
        std::pin::Pin::new(&mut self.0).poll(cx).map(|joined| {
            joined.map_err(|e| SeamError::failed(format!("planner task failed: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SweepConfig, lane_independent_model, scope_covers};

    #[test]
    fn source_reads_have_a_separate_fixed_bound() {
        let config = SweepConfig::default();

        assert_eq!(config.source_reads_in_flight_max.get(), 128);
        assert!(config.source_reads_in_flight_max < config.batch_concurrency);
    }

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
        assert!(!scope_covers(
            "Users/greg/Notes",
            "Users/greg/Notes-old/a.md"
        ));
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
            assert_eq!(
                defers_vector_index(*deep, *indexed),
                *expected,
                "deep={deep} indexed={indexed}"
            );
        }
    }
}
