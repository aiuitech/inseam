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
use crate::oauth::{AuthorizationCallback, AuthorizationStarted, GrantId, GrantState, Redirect};
use crate::sweep::{DeepBudget, IndexReport};
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
    /// Owner operation: the catalog as this node holds it — every source it
    /// knows about, deep-indexed or still waiting on budget.
    async fn catalog(&self, request: CatalogRequest) -> Result<CatalogResponse, SeamError>;
    /// Owner operation: the OAuth grants this node holds and where each
    /// stands — what a "connect an account" surface lists.
    async fn grants(&self) -> Result<Vec<GrantView>, SeamError>;
    /// Owner operation: start the browser authorization of a grant; the
    /// transport sends the owner to the returned URL. A local transport
    /// asks for the loopback redirect and then waits with
    /// [`Operations::await_authorization`]; a remote one serves the redirect
    /// itself and delivers it with [`Operations::complete_authorization`].
    async fn authorize_grant(&self, request: AuthorizeGrantRequest) -> Result<AuthorizationStarted, SeamError>;
    /// Owner operation: wait for a started authorization to finish, bounded
    /// by the oauth provider's timeout; the grant as it stands afterwards.
    async fn await_authorization(&self, request: AwaitAuthorizationRequest) -> Result<GrantView, SeamError>;
    /// Owner operation: the browser came back to a transport-served
    /// redirect with these parameters; exchange and store.
    async fn complete_authorization(&self, callback: AuthorizationCallback) -> Result<GrantView, SeamError>;
    /// Owner operation: forget a grant's tokens; hosts behind it withdraw.
    async fn revoke_grant(&self, request: RevokeGrantRequest) -> Result<GrantView, SeamError>;
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
    /// This run's deep budget; `None` takes the composition's. `catalog_only`
    /// is the ingest run: every source enters the catalog, none is
    /// deep-indexed until a later run has budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_budget: Option<DeepBudget>,
}

/// Which cataloged sources a listing shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFilter {
    #[default]
    All,
    /// Sources whose fragment subtree is built and searchable.
    Indexed,
    /// Cataloged sources with no subtree yet: catalog-only rows, sources
    /// past the cutoff, and ones a prior run left interrupted.
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogRequest {
    /// Restrict to one host; `None` lists every host this node knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostId>,
    #[serde(default)]
    pub filter: CatalogFilter,
    /// Entries to return; the counts always cover the whole selection.
    #[serde(default = "default_catalog_limit")]
    pub limit: u32,
}

fn default_catalog_limit() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogResponse {
    /// Counts over the host selection, regardless of `filter` and `limit`.
    pub sources: u64,
    pub indexed: u64,
    pub pending: u64,
    /// The first `limit` entries matching the filter, ordered by address.
    pub entries: Vec<CatalogSourceView>,
}

/// One cataloged source as owner surfaces list it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSourceView {
    pub address: Address,
    pub indexed: bool,
    pub content_type: String,
    /// The source's size on its host as enumeration reported it.
    pub raw_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
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

/// One grant as owner surfaces show it: what it is for, where it stands,
/// and which environment variables unlock it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantView {
    pub id: GrantId,
    /// The provider's host ("accounts.google.com"), for presentation.
    pub provider: String,
    /// The scopes declared for the grant.
    pub scopes: Vec<String>,
    pub client_id_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env: Option<String>,
    pub state: GrantState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizeGrantRequest {
    pub grant: GrantId,
    pub redirect: Redirect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwaitAuthorizationRequest {
    /// The `state` the authorization started with.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeGrantRequest {
    pub grant: GrantId,
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
    /// Bytes the store occupies on the node's disk (database plus its
    /// write-ahead log).
    pub store_bytes: u64,
    /// Bytes of source content the catalog covers, summed from enumeration's
    /// raw sizes — what the hosts hold, not what the node stores.
    pub content_bytes: u64,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: usize,
    pub reembed_pending: bool,
}
