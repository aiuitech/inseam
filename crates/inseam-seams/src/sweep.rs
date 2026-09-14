//! The `sweep` seam: the reconciling sweep as a service
//! (`design/index-maintenance.md`), so change feeds can schedule targeted
//! runs and owner operations can invoke it. Consumes `connections`,
//! `transforms`, `embedder`, and the kernel store.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use inseam_kernel::address::{Address, HostId};
use inseam_kernel::substrate::ServiceKey;
use serde::{Deserialize, Serialize};

use crate::SeamError;
use crate::llm::LlmLane;

pub const SWEEP: ServiceKey<dyn Sweep> = ServiceKey::new("sweep");

/// How many sources one run may deep-index. Every enumerated source enters
/// the catalog regardless; this only bounds the transform work, so it is a
/// run-metering dial (`design/index-maintenance.md`) — changing it never
/// invalidates anything. `CatalogOnly` is the "ingest now, index later"
/// run: addresses and envelopes land, no fragment is built, and every
/// catalog-only row stays dirty for a later run with budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeepBudget {
    Unlimited,
    Sources(NonZeroU32),
    CatalogOnly,
}

impl DeepBudget {
    /// Whether a run that has already chosen `deep_count` sources for deep
    /// indexing may choose one more.
    pub fn allows(self, deep_count: u32) -> bool {
        match self {
            Self::Unlimited => true,
            Self::Sources(limit) => deep_count < limit.get(),
            Self::CatalogOnly => false,
        }
    }
}

impl fmt::Display for DeepBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unlimited => write!(f, "unlimited"),
            Self::Sources(limit) => write!(f, "{limit} sources"),
            Self::CatalogOnly => write!(f, "catalog-only"),
        }
    }
}

pub struct SweepRequest {
    /// The host to sweep; its connection interprets `root`.
    pub host: HostId,
    /// Connection-interpreted scope (a directory path for the filesystem).
    pub root: String,
    /// Re-index sources even when unchanged.
    pub rebuild: bool,
    /// This run's deep budget; `None` takes the composition's
    /// (`sweep.max_sources`). A request-level override never outlives the
    /// run.
    pub deep_budget: Option<DeepBudget>,
    /// Put every LLM-using transform on this lane for the run; `None` lets
    /// each registration's own lane stand. `Some(Batch)` is the large,
    /// time-insensitive run: summaries collect into the endpoint's batch
    /// jobs instead of one request apiece.
    pub llm_lane: Option<LlmLane>,
    /// A local owner surface observing and controlling this run. Network
    /// transports leave it absent; it is process-local and never serialized.
    pub monitor: Option<Arc<dyn IndexMonitor>>,
}

#[async_trait::async_trait]
pub trait Sweep: Send + Sync {
    async fn sweep(&self, request: &SweepRequest) -> Result<IndexReport, SeamError>;
}

/// A sweep phase owner surfaces can show without learning pipeline internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPhase {
    Preparing,
    Enumerating,
    Cataloging,
    Indexing,
    Finalizing,
    Complete,
}

/// A bounded snapshot emitted at sweep checkpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexProgress {
    pub phase: IndexPhase,
    pub sources_complete: usize,
    pub sources_total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<Address>,
    pub indexed: usize,
    pub unchanged: usize,
    pub catalog_only: usize,
    pub ignored: usize,
    pub stopped: bool,
}

/// What the owner asks the sweep to do at a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexControl {
    Continue,
    Pause,
    Stop,
}

/// Process-local observer used by native app transports. Implementations
/// must return quickly; pause is represented as a state the sweep polls.
pub trait IndexMonitor: Send + Sync {
    fn update(&self, progress: &IndexProgress) -> IndexControl;
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IndexReport {
    pub sources_seen: usize,
    /// Enumerated sources the host-agnostic ignore rules kept out
    /// (`design/ignore.md`); they are not cataloged.
    pub ignored: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub catalog_only: usize,
    pub skipped_cutoff: usize,
    pub fragments: usize,
    pub relations: usize,
    /// Keyed sprouts anchored this run (an entity mentioned, say).
    pub keyed_anchored: usize,
    /// Sources removed because enumeration no longer sees them.
    pub removed: usize,
    /// Keyed fragments collected because no relation touches them anymore.
    pub keyed_removed: usize,
    /// Search rows re-embedded by a pending embedding migration.
    pub reembedded: usize,
    /// Sources whose text fit the summary length and became their own
    /// summary, with no call made.
    pub verbatim_summaries: usize,
    pub llm_summaries: usize,
    pub extractive_summaries: usize,
    pub envelope_summaries: usize,
    pub embedded: usize,
    /// Vectors taken from the digest-keyed embedding cache instead of the
    /// endpoint: text the index had already embedded under this model.
    pub embeddings_reused: usize,
    /// Transform outputs (LLM summaries, say) taken from the digest-keyed
    /// transform cache instead of applying the transform again.
    pub transforms_reused: usize,
    /// The DiskANN index was dropped before landing and rebuilt at the end,
    /// because this run re-landed a large share of the search table.
    pub vector_index_deferred: bool,
    /// LLM calls per consumer entry this run, from the granted handles.
    pub llm_calls: std::collections::BTreeMap<String, usize>,
    /// Dollars reported by the endpoint across the run's calls.
    pub spent: f64,
    /// Batch-API jobs the endpoint created across the run's calls (the
    /// batch lane's unit of work; zero on the interactive lane).
    pub llm_batch_jobs: u64,
    /// The owner stopped the run at a safe checkpoint. Work reported above
    /// is complete and a later sweep resumes from the remaining dirty rows.
    pub stopped: bool,
    /// What the vocabulary pass did this run (`design/vocabulary.md`);
    /// `None` when the pass is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocabulary: Option<VocabularyReport>,
}

/// The vocabulary pass's report: what it mined, planted, anchored, and
/// grounded, and what the hub bound would keep out of the walk, so the
/// first thing to do after a pass is read the top of the hub list.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
pub struct VocabularyReport {
    /// The pass found nothing to do and why (`"unchanged"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    /// Sources whose content text the pass walked.
    pub sources_walked: usize,
    /// Distinct tokens and phrases that cleared the shape rule.
    pub candidates_mined: usize,
    /// Candidates in the document-frequency band, planted or already rows.
    pub candidates_kept: usize,
    /// Phrases proposed by keywords, cues, and extracted names.
    pub candidates_derived: usize,
    pub rows_planted: usize,
    /// Row–source pairs the matching walk counted in memory; a matched
    /// row's anchors are the full-text index, never stored edges.
    pub matches_counted: usize,
    /// Relations written this pass: facet and author anchors, aliases.
    pub anchors_added: usize,
    /// Mined rows the statistics no longer name, retracted.
    pub rows_retracted: usize,
    /// Transform-planted keyed fragments adopted as vocabulary rows.
    pub rows_adopted: usize,
    pub clusters_founded: usize,
    pub clusters_joined: usize,
    pub clusters_merged: usize,
    pub clusters_embedded: usize,
    pub clusters_grounded: usize,
    pub aliases_planted: usize,
    pub glosses_written: usize,
    pub rows_merged: usize,
    pub llm_calls: usize,
    /// Facet and author rows planted from envelopes, and their anchors.
    pub facets_planted: usize,
    pub facet_anchors: usize,
    /// The most frequent rows, named with their document frequency: the
    /// first thing to read after a pass.
    pub hubs: Vec<(String, u32)>,
    pub mine_ms: u64,
    pub match_ms: u64,
    pub cluster_ms: u64,
    pub ground_ms: u64,
}

impl fmt::Display for VocabularyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(reason) = &self.skipped {
            return write!(f, "vocabulary: skipped ({reason})");
        }
        writeln!(
            f,
            "vocabulary: {} sources walked, {} candidates mined ({} kept, {} derived), {} rows planted, {} matches, {} anchors, {} retracted, {} adopted, {} facet rows ({} anchors)",
            self.sources_walked,
            self.candidates_mined,
            self.candidates_kept,
            self.candidates_derived,
            self.rows_planted,
            self.matches_counted,
            self.anchors_added,
            self.rows_retracted,
            self.rows_adopted,
            self.facets_planted,
            self.facet_anchors
        )?;
        writeln!(
            f,
            "clusters: {} founded, {} joined, {} merged, {} embedded, {} grounded ({} aliases, {} glosses, {} rows merged, {} llm calls)",
            self.clusters_founded,
            self.clusters_joined,
            self.clusters_merged,
            self.clusters_embedded,
            self.clusters_grounded,
            self.aliases_planted,
            self.glosses_written,
            self.rows_merged,
            self.llm_calls
        )?;
        write!(
            f,
            "vocabulary time: mine {} ms, match {} ms, cluster {} ms, ground {} ms",
            self.mine_ms, self.match_ms, self.cluster_ms, self.ground_ms
        )?;
        if !self.hubs.is_empty() {
            let named: Vec<String> = self
                .hubs
                .iter()
                .take(20)
                .map(|(name, degree)| format!("{name} ({degree})"))
                .collect();
            write!(f, "\nmost frequent rows: {}", named.join(", "))?;
        }
        Ok(())
    }
}

impl fmt::Display for IndexReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} sources seen: {} indexed, {} unchanged, {} catalog-only, {} past cutoff, {} ignored",
            self.sources_seen,
            self.indexed,
            self.unchanged,
            self.catalog_only,
            self.skipped_cutoff,
            self.ignored
        )?;
        writeln!(
            f,
            "{} fragments, {} relations, {} keyed fragments anchored",
            self.fragments, self.relations, self.keyed_anchored
        )?;
        if self.removed + self.keyed_removed + self.reembedded > 0 {
            writeln!(
                f,
                "maintenance: {} sources removed, {} keyed fragments collected, {} rows re-embedded",
                self.removed, self.keyed_removed, self.reembedded
            )?;
        }
        if self.vector_index_deferred {
            writeln!(
                f,
                "vector index: dropped before landing, rebuilt at the end"
            )?;
        }
        if self.llm_batch_jobs > 0 {
            writeln!(f, "llm batch lane: {} jobs", self.llm_batch_jobs)?;
        }
        writeln!(
            f,
            "reused: {} embeddings, {} transform outputs",
            self.embeddings_reused, self.transforms_reused
        )?;
        write!(
            f,
            "summaries: {} verbatim, {} llm, {} extractive, {} envelope · {} embedded · ${:.4} spent",
            self.verbatim_summaries,
            self.llm_summaries,
            self.extractive_summaries,
            self.envelope_summaries,
            self.embedded,
            self.spent
        )?;
        if let Some(vocabulary) = &self.vocabulary {
            write!(f, "\n{vocabulary}")?;
        }
        Ok(())
    }
}
