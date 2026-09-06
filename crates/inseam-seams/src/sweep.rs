//! The `sweep` seam: the reconciling sweep as a service
//! (`design/index-maintenance.md`), so change feeds can schedule targeted
//! runs and owner operations can invoke it. Consumes `connections`,
//! `transforms`, `embedder`, and the kernel store.

use std::fmt;
use std::num::NonZeroU32;

use inseam_kernel::address::HostId;
use inseam_kernel::substrate::ServiceKey;
use serde::{Deserialize, Serialize};

use crate::llm::LlmLane;
use crate::SeamError;

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

#[derive(Debug, Clone)]
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
}

#[async_trait::async_trait]
pub trait Sweep: Send + Sync {
    async fn sweep(&self, request: &SweepRequest) -> Result<IndexReport, SeamError>;
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
            writeln!(f, "vector index: dropped before landing, rebuilt at the end")?;
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
            "summaries: {} llm, {} extractive, {} envelope · {} embedded · ${:.4} spent",
            self.llm_summaries,
            self.extractive_summaries,
            self.envelope_summaries,
            self.embedded,
            self.spent
        )
    }
}
