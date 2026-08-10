//! The operations layer (`design/node-api.md`): typed, transport-neutral
//! request/response messages, JSON-serializable by construction. Every way of
//! reaching a node — CLI, the agent demo, a future HTTP or MCP adapter — is a
//! thin skin over [`Node`]'s operation methods.
//!
//! `query -> expand`/`scan` -> `fetch` is the incremental-discovery ladder:
//! each rung costs more context than the last, and a client climbs only where
//! the previous rung earned it.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::address::Address;
use crate::embed::{EmbedError, Embedder};
use crate::finder::{Finder, FinderError, RankedFragment};
use crate::fragment::{FragmentId, Relation};
use crate::host_fs::{self, FsHost, FsHostError};
use crate::indexer::{IndexError, IndexReport, Indexer};
use crate::llm::LlmClient;
use crate::profile::IndexProfile;
use crate::store::{IndexStore, StoreError, StoredFragment, StoredSource};
use crate::textutil::preview;

/// Characters of fragment text shown in hints and expand views.
const PREVIEW_CHARS: usize = 280;

#[derive(Debug, Error)]
pub enum OpsError {
    #[error("no source at {0} in this node's catalog")]
    UnknownSource(Address),
    #[error("{0} is not indexed as text and has no text fragments to scan")]
    NothingToScan(Address),
    #[error("{0} is binary ({1}); fetching binary content over the JSON surface is not supported yet")]
    BinaryFetch(Address, String),
    #[error("bad address: {0}")]
    Address(#[from] crate::address::AddressError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Finder(#[from] FinderError),
    #[error(transparent)]
    Host(#[from] FsHostError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

// ---------------------------------------------------------------------------
// Operation messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    pub text: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResponse {
    pub results: Vec<QueryResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub address: Address,
    /// 1.0 for the query's top result.
    pub score: f64,
    pub summary: Option<String>,
    pub envelope: EnvelopeView,
    pub hints: Vec<FragmentHint>,
}

/// Envelope fields rendered for clients: dates as `YYYY-MM-DD`, length with
/// its unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeView {
    pub source_type: String,
    pub content_type: String,
    pub length: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentHint {
    pub fragment: FragmentId,
    pub mimetype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extent: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandRequest {
    pub address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandResponse {
    pub address: Address,
    pub summary: Option<String>,
    pub fragments: Vec<FragmentView>,
    pub relations: Vec<RelationView>,
    /// Fragments outside this source that its relations reach — entities and,
    /// through them, the rest of the graph.
    pub neighbors: Vec<FragmentView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentView {
    pub id: FragmentId,
    pub mimetype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Address of the fragment's own source, present on neighbors so a client
    /// can hop to them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationView {
    pub from: FragmentId,
    pub kind: String,
    pub to: FragmentId,
}

impl From<&Relation> for RelationView {
    fn from(r: &Relation) -> Self {
        Self {
            from: r.from,
            kind: r.kind.as_str().to_string(),
            to: r.to,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    pub address: Address,
    /// 1-based inclusive line range.
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResponse {
    pub address: Address,
    pub mimetype: String,
    pub start: u64,
    pub end: u64,
    pub text: String,
    /// Set when the source is media and the scan was served from a descendant
    /// text fragment instead (`design/finder.md`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_from_fragment: Option<FragmentId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchRequest {
    pub address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResponse {
    pub address: Address,
    pub content_type: String,
    pub text: String,
}

// ---------------------------------------------------------------------------
// The node
// ---------------------------------------------------------------------------

/// A running inseam instance, holding its store, its filesystem host
/// connection, and its profile. Operation methods are the boundary surface;
/// `index_dir` is an owner operation.
pub struct Node {
    store: IndexStore,
    fs: FsHost,
    profile: IndexProfile,
    embedder: Embedder,
    llm: Option<Arc<LlmClient>>,
}

impl Node {
    /// Open the node's store under `data_dir` with the given profile. The
    /// endpoint client is optional: without it, LLM transforms fall back
    /// and only the `endpoint` embedding provider refuses to run.
    pub async fn open(
        data_dir: &Path,
        profile: IndexProfile,
        llm: Option<Arc<LlmClient>>,
    ) -> Result<Self, OpsError> {
        let embedder = Embedder::from_profile(&profile.embedding, llm.clone())?;
        let dims = embedder.dimensions().unwrap_or(0);
        let store = IndexStore::open(data_dir, dims, &profile.embedding.model).await?;
        Ok(Self {
            store,
            fs: FsHost::local(),
            profile,
            embedder,
            llm,
        })
    }

    pub fn store(&self) -> &IndexStore {
        &self.store
    }

    /// The host this node's filesystem connection stewards.
    pub fn host_id(&self) -> &crate::address::HostId {
        self.fs.id()
    }

    pub fn profile(&self) -> &IndexProfile {
        &self.profile
    }

    pub fn llm(&self) -> Option<&Arc<LlmClient>> {
        self.llm.as_ref()
    }

    fn finder(&self) -> Finder<'_> {
        Finder {
            store: &self.store,
            embedder: &self.embedder,
            config: &self.profile.finder,
        }
    }

    /// Owner operation: index a directory of the local filesystem host.
    pub async fn index_dir(&self, dir: &Path, rebuild: bool) -> Result<IndexReport, OpsError> {
        let indexer = Indexer {
            store: &self.store,
            fs: &self.fs,
            profile: &self.profile,
            embedder: &self.embedder,
            llm: self.llm.as_deref(),
        };
        Ok(indexer.index_dir(dir, rebuild).await?)
    }

    /// Boundary operation `query`: ranked addresses + envelopes, each with
    /// score, summary, and fragment hints.
    pub async fn query(&self, request: QueryRequest) -> Result<QueryResponse, OpsError> {
        let limit = request.limit.clamp(1, 50);
        let ranked = self.finder().query(&request.text, limit).await?;
        let results = ranked
            .into_iter()
            .map(|r| QueryResult {
                address: r.source.address.clone(),
                score: round3(r.score),
                summary: r.summary,
                envelope: envelope_view(&r.source),
                hints: r.hints.iter().map(hint_view).collect(),
            })
            .collect();
        Ok(QueryResponse { results })
    }

    /// Boundary operation `expand`: one source's fragments and relations from
    /// this node's index, for navigating structure instead of re-searching.
    pub fn expand(&self, request: ExpandRequest) -> Result<ExpandResponse, OpsError> {
        let source = self.source_at(&request.address)?;
        let expansion = self.finder().expand(&source)?;
        let mut sources_cache: std::collections::HashMap<_, Address> = Default::default();
        let neighbors = expansion
            .neighbors
            .iter()
            .map(|f| {
                let address = f.source.and_then(|sid| {
                    if let Some(a) = sources_cache.get(&sid) {
                        return Some(a.clone());
                    }
                    let a = self.store.source(sid).ok().flatten().map(|s| s.address);
                    if let Some(a) = &a {
                        sources_cache.insert(sid, a.clone());
                    }
                    a
                });
                fragment_view(f, address)
            })
            .collect();
        Ok(ExpandResponse {
            address: source.address.clone(),
            summary: self.store.summary_of(source.id)?,
            fragments: expansion
                .fragments
                .iter()
                .map(|f| fragment_view(f, None))
                .collect(),
            relations: expansion.relations.iter().map(RelationView::from).collect(),
            neighbors,
        })
    }

    /// Boundary operation `scan`: read a line range of a source through the
    /// fetch path. Media sources redirect to a descendant text fragment.
    pub async fn scan(&self, request: ScanRequest) -> Result<ScanResponse, OpsError> {
        let source = self.source_at(&request.address)?;
        let (start, end) = (request.start.max(1), request.end.max(request.start));
        if host_fs::is_texty(&source.envelope.content_type) {
            let text = self.fs.read_lines(&source.address, start, end)?;
            return Ok(ScanResponse {
                address: source.address,
                mimetype: source.envelope.content_type.to_string(),
                start,
                end,
                text,
                served_from_fragment: None,
            });
        }
        // Scanning media means reading lines of its text descendants — the
        // transcript case. Pick the largest text fragment as the stand-in.
        let fragments = self.store.fragments_of(source.id)?;
        let best = fragments
            .iter()
            .filter(|f| !f.mimetype.is_summary())
            .filter_map(|f| f.text.as_ref().map(|t| (f, t)))
            .max_by_key(|(_, t)| t.len());
        let Some((fragment, text)) = best else {
            return Err(OpsError::NothingToScan(source.address));
        };
        let sliced = host_fs::slice_lines(text, start, end)?;
        Ok(ScanResponse {
            address: source.address,
            mimetype: fragment.mimetype.to_string(),
            start,
            end,
            text: sliced,
            served_from_fragment: Some(fragment.id),
        })
    }

    /// Boundary operation `fetch`: a source's full content by address.
    pub async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, OpsError> {
        let source = self.source_at(&request.address)?;
        if !host_fs::is_texty(&source.envelope.content_type) {
            return Err(OpsError::BinaryFetch(
                source.address,
                source.envelope.content_type.to_string(),
            ));
        }
        let text = self.fs.read_text(&source.address)?;
        Ok(FetchResponse {
            address: source.address,
            content_type: source.envelope.content_type.to_string(),
            text,
        })
    }

    fn source_at(&self, address: &Address) -> Result<StoredSource, OpsError> {
        self.store
            .source_by_address(address)?
            .ok_or_else(|| OpsError::UnknownSource(address.clone()))
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("host", self.fs.id())
            .finish_non_exhaustive()
    }
}

fn envelope_view(source: &StoredSource) -> EnvelopeView {
    let e = &source.envelope;
    EnvelopeView {
        source_type: e.source_type.clone(),
        content_type: e.content_type.to_string(),
        length: e.length.to_string(),
        created: e.created.map(|t| t.ymd()),
        modified: e.modified.map(|t| t.ymd()),
        title: e.hint.clone(),
    }
}

fn hint_view(ranked: &RankedFragment) -> FragmentHint {
    let f = &ranked.fragment;
    FragmentHint {
        fragment: f.id,
        mimetype: f.mimetype.to_string(),
        extent: f.extent.map(|e| e.to_string()),
        text: f
            .text
            .as_deref()
            .map(|t| preview(t, PREVIEW_CHARS))
            .unwrap_or_default(),
    }
}

fn fragment_view(f: &StoredFragment, source: Option<Address>) -> FragmentView {
    FragmentView {
        id: f.id,
        mimetype: f.mimetype.to_string(),
        extent: f.extent.map(|e| e.to_string()),
        text: f.text.as_deref().map(|t| preview(t, PREVIEW_CHARS)),
        source,
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
