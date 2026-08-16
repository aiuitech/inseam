//! The `finder` seam: query → ranked sources with summaries and hints
//! (`design/finder.md`). The default provider does RRF seeding + relevance
//! propagation; an LLM re-ranker on a big node is a provider swap, not a
//! core change.

use inseam_kernel::fragment::Relation;
use inseam_kernel::store::{StoredFragment, StoredSource};
use inseam_kernel::substrate::ServiceKey;

use crate::SeamError;

pub const FINDER: ServiceKey<dyn Finder> = ServiceKey::new("finder");

/// One ranked source with everything an AI client needs to decide its next
/// move: score, the mandatory summary, and fragment-level hints.
#[derive(Debug, Clone)]
pub struct RankedSource {
    pub source: StoredSource,
    /// Normalized to 1.0 for the top result of a query.
    pub score: f64,
    pub summary: Option<String>,
    pub hints: Vec<RankedFragment>,
}

/// A fragment that earned its source a place in the ranking.
#[derive(Debug, Clone)]
pub struct RankedFragment {
    pub fragment: StoredFragment,
    pub score: f64,
}

/// The result of `expand`: one source's subtree plus its cross-links.
#[derive(Debug, Clone)]
pub struct Expansion {
    pub fragments: Vec<StoredFragment>,
    pub relations: Vec<Relation>,
    /// Fragments outside the source that its relations reach (entities, and
    /// through them the rest of the graph).
    pub neighbors: Vec<StoredFragment>,
}

#[async_trait::async_trait]
pub trait Finder: Send + Sync {
    /// Run discovery for a query: hybrid seed, graph boost, source rollup.
    async fn query(&self, text: &str, limit: usize) -> Result<Vec<RankedSource>, SeamError>;

    /// One source's subtree of the semantic graph and its cross-links.
    fn expand(&self, source: &StoredSource) -> Result<Expansion, SeamError>;
}
