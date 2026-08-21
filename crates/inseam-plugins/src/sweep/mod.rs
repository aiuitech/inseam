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
//! once, each claimant of a fragment in flight together), each plan is
//! **landed** in one store transaction in enumeration order (deterministic
//! ids), and its search rows flow to the **embedding stage** ([`embed`]),
//! which embeds batches concurrently and lands them in order with the
//! `indexed` marks they complete.

pub mod ignore;

mod embed;
mod grant;
mod plan;

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use serde::Deserialize;

use inseam_kernel::address::Timestamp;
use inseam_kernel::store::{
    CatalogEntry, CatalogMark, IndexStore, KeyedFragment, SourceCompletion, SubtreeWritten,
};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError, STORE,
};
use inseam_seams::connection::{Connection, EnumeratedSource, CONNECTION};
use inseam_seams::dates::parse_ymd_epoch;
use inseam_seams::embedder::{Embedder, EMBEDDER};
use inseam_seams::llm::{self, Llm, LLM};
use inseam_seams::sweep::{IndexReport, Sweep, SweepRequest, SWEEP};
use inseam_seams::transforms::{Registration, Transforms, TRANSFORMS};
use inseam_seams::SeamError;

use embed::{EmbedStage, PendingRow, RowBuffer};
use grant::{Grantor, RunMeters};
use ignore::{IgnoreRule, IgnoreSet};
use plan::{expected_stamp, PlanLimits, Planned, Planner};

/// Catalog-only rows per transaction.
const CATALOG_CHUNK: usize = 1_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SweepConfig {
    /// Sources to deep-index per run; the rest still enter the catalog.
    /// 0 means unlimited. (Run-metering tier: bounds a run, not the shape.)
    pub max_sources: usize,
    /// Sources planned at once — transform applications (LLM calls above
    /// all) in flight together. Run-metering tier: a throughput dial, never
    /// a shape one.
    pub concurrency: NonZeroUsize,
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
            .facts(&LLM)
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

        let decisions = self
            .decide(&sources, cutoff, &registrations, &sweep_shape, request.rebuild, &mut report)
            .await?;
        for chunk in decisions.catalog.chunks(CATALOG_CHUNK) {
            self.store.catalog_sources(chunk).await?;
        }

        let grantor = Arc::new(Grantor {
            llm: self.llm.clone(),
            model: self.transform_model.clone(),
            bus: self.bus.clone(),
            meters: RunMeters::for_registrations(&registrations),
        });
        let planner = Arc::new(Planner {
            connection: Arc::clone(&self.connection),
            registrations,
            grantor: Arc::clone(&grantor),
            sweep_shape,
            limits: self.config.limits(),
        });
        self.index_deep(decisions.deep, planner, &mut report).await?;

        self.reconcile_vanished(&request.root, &sources, &mut report)
            .await?;
        let orphaned = self.store.gc_keyed_fragments().await?;
        report.keyed_removed = orphaned.len();
        self.store.rebuild_fts().await?;
        for (entry, calls) in grantor.meters.calls_by_entry() {
            report.llm_calls.insert(entry.to_string(), calls);
        }
        if let Some(llm) = &self.llm {
            report.spent = llm.spent();
        }
        Ok(report)
    }
}

/// What the dirtiness pass decided: rows that enter the catalog without a
/// subtree, and the sources to deep-index this run, both in enumeration
/// order.
struct Decisions<'a> {
    catalog: Vec<CatalogEntry<'a>>,
    deep: Vec<EnumeratedSource>,
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
        rebuild: bool,
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
            if !dirty && !rebuild {
                report.unchanged += 1;
                continue;
            }
            let deep_budget_left =
                self.config.max_sources == 0 || decisions.deep.len() < self.config.max_sources;
            if !deep_budget_left {
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
        report: &mut IndexReport,
    ) -> Result<(), SeamError> {
        let stage = EmbedStage::start(Arc::clone(&self.embedder), Arc::clone(&self.store));
        let mut buffer = RowBuffer::default();
        let planned = stream::iter(deep)
            .map(|source| {
                let planner = Arc::clone(&planner);
                Spawned(tokio::spawn(async move { planner.plan(&source).await }))
            })
            .buffered(self.config.concurrency.get());
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
    /// vectors are the only thing rebuilt, through the same embedding stage
    /// an index run uses.
    async fn reembed(&self, report: &mut IndexReport) -> Result<(), SeamError> {
        let targets = self.store.reembed_targets().await?;
        report.reembedded = targets.len();
        self.store.begin_reembed().await?;
        let stage = EmbedStage::start(Arc::clone(&self.embedder), Arc::clone(&self.store));
        let mut buffer = RowBuffer::default();
        for (fragment, source, text) in targets {
            buffer.push_rows([PendingRow {
                fragment,
                source,
                text,
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
