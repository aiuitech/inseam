//! The `finder` seam: query → ranked sources with summaries and hints
//! (`design/finder.md`). The default provider does RRF seeding + relevance
//! propagation; an LLM re-ranker on a big node is a provider swap, not a
//! core change.

use inseam_kernel::address::Address;
use inseam_kernel::fragment::Relation;
use inseam_kernel::store::{StoredFragment, StoredSource};
use inseam_kernel::substrate::ServiceKey;
use serde::{Deserialize, Serialize};

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
    /// Addresses of other copies of the same content, collapsed into this
    /// result by equal envelope content digests (`design/finder.md`). The
    /// best-scoring copy is `source`; the caller picks its replica at fetch
    /// time.
    pub replicas: Vec<Address>,
}

/// A fragment that earned its source a place in the ranking.
#[derive(Debug, Clone)]
pub struct RankedFragment {
    pub fragment: StoredFragment,
    pub score: f64,
}

/// What one query cost and what it worked through, phase by phase — the
/// numbers a client inspects when a query feels slow or thin. Milliseconds
/// are wall-clock inside the provider; counts are what each phase handed
/// to the next.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryTrace {
    /// Hybrid seed retrieval: full-text search, query embedding, and
    /// vector search, fused by rank.
    pub seeds_ms: u64,
    /// Loading the local relation graph and running the relevance walk.
    pub graph_ms: u64,
    /// Grouping fragment scores by source and dressing the results with
    /// envelopes, summaries, and hints.
    pub rollup_ms: u64,
    /// Fragments the full-text search returned.
    pub fts_hits: u32,
    /// Lexical full-text seeds (terms, names, identifiers), the second list.
    pub lexical_hits: u32,
    /// Fragments the vector search returned within the distance floor;
    /// zero when the node has no embedder.
    pub vector_hits: u32,
    /// Distinct fragments after rank fusion — the seeds of the walk.
    pub seeds: u32,
    /// Relations loaded around the seeds.
    pub relations: u32,
    /// Distinct sources holding a scored fragment, before the limit cut.
    pub candidate_sources: u32,
}

/// The result of `query`: the ranked sources and the trace of producing
/// them.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub ranked: Vec<RankedSource>,
    pub trace: QueryTrace,
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
    async fn query(&self, text: &str, limit: usize) -> Result<Discovery, SeamError>;

    /// One source's subtree of the semantic graph and its cross-links.
    async fn expand(&self, source: &StoredSource) -> Result<Expansion, SeamError>;
}
