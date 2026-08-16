//! The `sweep` seam: the reconciling sweep as a service
//! (`design/index-maintenance.md`), so change feeds can schedule targeted
//! runs and owner operations can invoke it. Consumes `connection`,
//! `transforms`, `embedder`, and the kernel store.

use std::fmt;

use inseam_kernel::substrate::ServiceKey;

use crate::SeamError;

pub const SWEEP: ServiceKey<dyn Sweep> = ServiceKey::new("sweep");

#[derive(Debug, Clone)]
pub struct SweepRequest {
    /// Connection-interpreted scope (a directory path for the filesystem).
    pub root: String,
    /// Re-index sources even when unchanged.
    pub rebuild: bool,
}

#[async_trait::async_trait]
pub trait Sweep: Send + Sync {
    async fn sweep(&self, request: &SweepRequest) -> Result<IndexReport, SeamError>;
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct IndexReport {
    pub sources_seen: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub catalog_only: usize,
    pub skipped_cutoff: usize,
    pub fragments: usize,
    pub relations: usize,
    pub entities_seen: usize,
    /// Sources removed because enumeration no longer sees them.
    pub removed: usize,
    /// Entity fragments collected because no relation touches them anymore.
    pub entities_removed: usize,
    /// Search rows re-embedded by a pending embedding migration.
    pub reembedded: usize,
    pub llm_summaries: usize,
    pub extractive_summaries: usize,
    pub envelope_summaries: usize,
    pub embedded: usize,
    /// LLM calls per consumer entry this run, from the granted handles.
    pub llm_calls: std::collections::BTreeMap<String, usize>,
    /// Dollars reported by the endpoint across the run's calls.
    pub spent: f64,
}

impl fmt::Display for IndexReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} sources seen: {} indexed, {} unchanged, {} catalog-only, {} past cutoff",
            self.sources_seen, self.indexed, self.unchanged, self.catalog_only, self.skipped_cutoff
        )?;
        writeln!(
            f,
            "{} fragments, {} relations, {} entity mentions",
            self.fragments, self.relations, self.entities_seen
        )?;
        if self.removed + self.entities_removed + self.reembedded > 0 {
            writeln!(
                f,
                "maintenance: {} sources removed, {} entities collected, {} rows re-embedded",
                self.removed, self.entities_removed, self.reembedded
            )?;
        }
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
