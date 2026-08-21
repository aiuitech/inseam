//! The `operations` seam: typed, transport-neutral request/response messages
//! (`design/node-api.md`). Transport plugins (CLI, HTTP, MCP, FFI) consume
//! this seam and stay logic-free. `query -> expand`/`scan` -> `fetch` is the
//! incremental-discovery ladder; `index` and `status` are owner operations.
//!
//! Boundary enforcement is a guard on dispatch: providers check
//! [`OperationRequest`] before serving scoped operations, and denial is
//! monotonic — no listener can force-allow what another denied
//! (`design/access-control.md`).

use inseam_kernel::address::{Address, HostId};
use inseam_kernel::fragment::{FragmentId, Relation};
use inseam_kernel::substrate::{Guard, ServiceKey};
use serde::{Deserialize, Serialize};

use crate::connection::{Capabilities, HostKind};
use crate::sweep::IndexReport;
use crate::SeamError;

pub const OPERATIONS: ServiceKey<dyn Operations> = ServiceKey::new("operations");

/// Guard event checked before a boundary (non-owner) operation is served.
/// Access-control plugins listen here.
#[derive(Debug, Clone)]
pub struct OperationRequest {
    pub operation: &'static str,
    /// The requesting principal; "owner" for local transports today.
    pub requester: String,
}

impl Guard for OperationRequest {}

#[async_trait::async_trait]
pub trait Operations: Send + Sync {
    async fn query(&self, request: QueryRequest) -> Result<QueryResponse, SeamError>;
    async fn expand(&self, request: ExpandRequest) -> Result<ExpandResponse, SeamError>;
    async fn scan(&self, request: ScanRequest) -> Result<ScanResponse, SeamError>;
    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, SeamError>;
    /// Owner operation: reconcile the index over a scope of one host.
    async fn index(&self, request: IndexRequest) -> Result<IndexReport, SeamError>;
    /// Owner operation: the hosts this node stewards, with what each
    /// connection supports — the local view of the stewardship records the
    /// roster will publish.
    async fn hosts(&self) -> Result<Vec<HostView>, SeamError>;
    /// Owner operation: index and catalog statistics.
    async fn status(&self) -> Result<StatusReport, SeamError>;
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
    /// Other copies of the same content (equal envelope content digests),
    /// collapsed into this result; the caller picks its replica at fetch
    /// time (`design/finder.md`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replicas: Vec<Address>,
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
    /// Fragments outside this source that its relations reach — entities
    /// and, through them, the rest of the graph.
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
    /// Address of the fragment's own source, present on neighbors so a
    /// client can hop to them.
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
    /// Set when the source is media and the scan was served from a
    /// descendant text fragment instead (`design/finder.md`).
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRequest {
    /// The host whose connection interprets `root`. `None` means the one
    /// host this node stewards — an error naming the choices when there are
    /// several, so a scope is never guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostId>,
    pub root: String,
    #[serde(default)]
    pub rebuild: bool,
}

/// One stewarded host as owner surfaces show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostView {
    pub id: HostId,
    pub kind: HostKind,
    pub display_name: String,
    /// The composition entry whose connection stewards it.
    pub entry: String,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub sources: u64,
    pub indexed_sources: u64,
    pub fragments: u64,
    pub relations: u64,
    /// Index-wide fragments deduplicated by key (entities, for instance).
    pub keyed_fragments: u64,
    pub search_rows: usize,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: usize,
    pub reembed_pending: bool,
}
