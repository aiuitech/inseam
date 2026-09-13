//! The `finder` seam: query → ranked sources with summaries and hints
//! (`design/finder.md`). The default provider does RRF seeding + relevance
//! propagation; an LLM re-ranker on a big node is a provider swap, not a
//! core change.

use std::collections::BTreeMap;

use inseam_kernel::address::{Address, Timestamp};
use inseam_kernel::fragment::{FragmentId, Relation};
use inseam_kernel::store::{ClusterId, RowKind, StoredFragment, StoredSource};
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// Local result score evidence, before any cross-node merge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<SourceEvidence>,
    /// Grounding the query against the vocabulary: exact rows the question
    /// names outright and clusters its vector lands on
    /// (`design/vocabulary.md`, retrieval).
    #[serde(default)]
    pub grounding_ms: u64,
    /// Vocabulary rows the query spelled out exactly — the fourth seed list.
    #[serde(default)]
    pub exact_hits: u32,
    /// Clusters whose vector cleared the query cosine floor.
    #[serde(default)]
    pub clusters_matched: u32,
    /// Rows and aliases the matched clusters contributed — the fifth list.
    #[serde(default)]
    pub cluster_hits: u32,
    /// Vertices the hub bound kept out of the walk, highest degree first;
    /// the ledger names them under `explain`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hubs_excluded: Vec<ExcludedHub>,
    /// Sources the request's filters removed before the rollup.
    #[serde(default)]
    pub filtered_sources: u32,
}

/// A vertex the degree bound kept out of the walk's slice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcludedHub {
    pub fragment: FragmentId,
    pub degree: u32,
    /// Filled under `explain`: what the hub is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RowKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One of the seed lists a query runs before fusion. Every list is a
/// query-time dial: enabled, a fusion weight, and a rank gate
/// (`design/vocabulary.md`, observability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedChannel {
    /// BM25 over prose rows: content, summaries, hints.
    Prose,
    /// BM25 over lexical rows: keywords, terms, identifiers, entities.
    Lexical,
    /// Nearest neighbours by the query's vector.
    Vector,
    /// Vocabulary rows the question spells exactly.
    Exact,
    /// Rows and aliases of the clusters the query vector lands on.
    Cluster,
}

impl SeedChannel {
    pub const ALL: [Self; 5] = [
        Self::Prose,
        Self::Lexical,
        Self::Vector,
        Self::Exact,
        Self::Cluster,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prose => "prose",
            Self::Lexical => "lexical",
            Self::Vector => "vector",
            Self::Exact => "exact",
            Self::Cluster => "cluster",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "prose" => Some(Self::Prose),
            "lexical" => Some(Self::Lexical),
            "vector" => Some(Self::Vector),
            "exact" => Some(Self::Exact),
            "cluster" => Some(Self::Cluster),
            _ => None,
        }
    }

    /// Position in [`Self::ALL`]: the index every per-channel array uses.
    pub fn index(self) -> usize {
        match self {
            Self::Prose => 0,
            Self::Lexical => 1,
            Self::Vector => 2,
            Self::Exact => 3,
            Self::Cluster => 4,
        }
    }
}

impl std::fmt::Display for SeedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The exact decomposition of one result's score (`design/vocabulary.md`,
/// observability). Fusion is a sum over channels and the walk is linear
/// in its restart vector, so `channels` sums to the source's raw score,
/// `walk_by_row_kind` splits the walk share by what carried it, and
/// `rows` names the vocabulary rows and structure that did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    /// One line per channel with non-zero mass: seed mass and the walk
    /// mass that channel's seeds induced, over the source's rollup.
    pub channels: Vec<ChannelLine>,
    /// The walk mass that arrived through neighbours of each row kind, on
    /// the walk's last iteration.
    pub walk_by_row_kind: BTreeMap<RowKind, f64>,
    /// The neighbours that carried the most mass into the source's scoring
    /// fragments, best first, with their document frequency when they are
    /// vocabulary rows.
    pub rows: Vec<CarryingRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelLine {
    pub channel: SeedChannel,
    pub seed: f64,
    pub walk: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CarryingRow {
    pub fragment: FragmentId,
    pub kind: RowKind,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document_frequency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<ClusterId>,
    /// Walk mass this neighbour handed the source's fragments.
    pub mass: f64,
}

/// One query as the finder receives it: the text and limit every client
/// sends, plus the per-request dials the benchmark harness sweeps and the
/// diagnosis a client may ask for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FinderRequest {
    pub text: String,
    pub limit: usize,
    /// Query-time finder settings for this request alone, as `key=value`
    /// pairs over the finder's own configuration keys (`seed_lists.cluster.
    /// weight=0.3`). Restricted to the query-time tier; never stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<String>,
    /// Compute and attach the ledger to every result's evidence.
    #[serde(default)]
    pub explain: bool,
    #[serde(default)]
    pub filters: QueryFilters,
}

impl FinderRequest {
    pub fn new(text: &str, limit: usize) -> Self {
        Self {
            text: text.to_string(),
            limit,
            ..Self::default()
        }
    }
}

/// Constraints on which sources may rank: applied to the candidate set
/// before the rollup, the way boundary properties are
/// (`design/vocabulary.md`, facets). Each field narrows; empty is no
/// constraint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryFilters {
    /// The source's host id, exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The source's envelope `source_type` (`email`, `file`, `directory`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    /// Facet values the source's root must be anchored to, normalized
    /// (`facet:container:slack:#incidents`, an author's entity row).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_after: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_before: Option<Timestamp>,
}

impl QueryFilters {
    pub fn is_empty(&self) -> bool {
        self.host.is_none()
            && self.source_type.is_none()
            && self.facets.is_empty()
            && self.modified_after.is_none()
            && self.modified_before.is_none()
    }
}

/// The three fragments that contribute to a source's rollup score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceEvidence {
    pub address: Address,
    pub score_raw: f64,
    pub normalization: f64,
    pub fragments: Vec<FragmentEvidence>,
    /// The exact decomposition of `score_raw`, under `explain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<Ledger>,
}

/// Measured contributions, with one-based ranks in each seed list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FragmentEvidence {
    pub fragment: FragmentId,
    pub prose_rank: Option<u32>,
    pub lexical_rank: Option<u32>,
    pub vector_rank: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_rank: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_rank: Option<u32>,
    pub seed: f64,
    pub graph: f64,
    pub weight: f64,
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
    /// Run discovery for a request: grounding, hybrid seed, graph boost,
    /// source rollup — with the request's own dials, filters, and ledger.
    async fn discover(&self, request: &FinderRequest) -> Result<Discovery, SeamError>;

    /// Run discovery for a query with the finder's configured dials.
    async fn query(&self, text: &str, limit: usize) -> Result<Discovery, SeamError> {
        self.discover(&FinderRequest::new(text, limit)).await
    }

    /// One source's subtree of the semantic graph and its cross-links.
    async fn expand(&self, source: &StoredSource) -> Result<Expansion, SeamError>;
}
