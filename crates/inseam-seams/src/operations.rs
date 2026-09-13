//! The `operations` seam: typed, transport-neutral request/response messages
//! (`design/node-api.md`). Transport plugins (CLI, HTTP, MCP, FFI) consume
//! this seam and stay logic-free. `query -> expand`/`scan` -> `fetch` is the
//! incremental-discovery ladder; `index`, `repair`, and `status` are owner
//! operations.
//!
//! Boundary enforcement is a guard on dispatch: providers check
//! [`OperationRequest`] before serving scoped operations, and denial is
//! monotonic — no listener can force-allow what another denied
//! (`design/access-control.md`).

use std::sync::Arc;

use inseam_kernel::address::{Address, ContentDigest, ContentLength, HostId};
use inseam_kernel::fragment::{Extent, FragmentId, Relation};
use inseam_kernel::network::{HostRecord, NodeId, NodeRecord};
use inseam_kernel::store::VectorScope;
use inseam_kernel::substrate::{FiberState, FiberView, Guard, SecretNeed, ServiceKey};
use serde::{Deserialize, Serialize};

use crate::SeamError;
use crate::call_capture::{CaptureStatus, PhoneNumber};
use crate::finder::QueryFilters;
use inseam_kernel::store::{VocabularyCounts, VocabularyKind};
use crate::connection::{Capabilities, HostKind};
use crate::finder::QueryTrace;
use crate::llm::LlmLane;
use crate::oauth::{AuthorizationCallback, AuthorizationStarted, GrantId, GrantState, Redirect};
use crate::roster::Invitation;
use crate::sweep::{DeepBudget, IndexMonitor, IndexReport};

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
    /// The bytes of a source or of a referenced fragment, with their
    /// content type — the rung for content that is not text: an image, a
    /// PDF, a linked file. Bounded at [`FETCH_BYTES_MAX`] per message.
    async fn fetch_bytes(
        &self,
        request: FetchBytesRequest,
    ) -> Result<FetchBytesResponse, SeamError>;
    /// Owner operation: reconcile the index over a scope of one host.
    async fn index(&self, request: IndexRequest) -> Result<IndexReport, SeamError>;
    /// The same operation with a process-local progress and control monitor.
    /// Transports without a live process use [`Operations::index`].
    async fn index_monitored(
        &self,
        request: IndexRequest,
        monitor: Arc<dyn IndexMonitor>,
    ) -> Result<IndexReport, SeamError> {
        let _ = monitor;
        self.index(request).await
    }
    /// Owner operation: the hosts this node stewards, with what each
    /// connection supports — the local view of the stewardship records the
    /// roster will publish.
    async fn hosts(&self) -> Result<Vec<HostView>, SeamError>;
    /// Owner operation: index and catalog statistics.
    async fn status(&self) -> Result<StatusReport, SeamError>;
    /// Owner operation: converge the derived search index without fetching
    /// sources or calling transforms and embedding providers.
    async fn repair(&self, request: RepairRequest) -> Result<RepairReport, SeamError>;
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
    async fn authorize_grant(
        &self,
        request: AuthorizeGrantRequest,
    ) -> Result<AuthorizationStarted, SeamError>;
    /// Owner operation: wait for a started authorization to finish, bounded
    /// by the oauth provider's timeout; the grant as it stands afterwards.
    async fn await_authorization(
        &self,
        request: AwaitAuthorizationRequest,
    ) -> Result<GrantView, SeamError>;
    /// Owner operation: the browser came back to a transport-served
    /// redirect with these parameters; exchange and store.
    async fn complete_authorization(
        &self,
        callback: AuthorizationCallback,
    ) -> Result<GrantView, SeamError>;
    /// Owner operation: forget a grant's tokens; hosts behind it withdraw.
    async fn revoke_grant(&self, request: RevokeGrantRequest) -> Result<GrantView, SeamError>;

    /// Owner: where call capture stands on this node — the number to
    /// merge, the owner's own number, the last call placed.
    async fn call_capture_status(&self) -> Result<CaptureStatus, SeamError>;
    /// Owner: begin verifying the owner's phone; the node calls it and
    /// speaks a code.
    async fn set_capture_number(
        &self,
        request: SetCaptureNumberRequest,
    ) -> Result<CaptureStatus, SeamError>;
    /// Owner: finish verification with the spoken code.
    async fn verify_capture_number(
        &self,
        request: VerifyCaptureNumberRequest,
    ) -> Result<CaptureStatus, SeamError>;
    /// Owner: place the capture call to the verified number now.
    async fn start_call_capture(&self) -> Result<CaptureStatus, SeamError>;
    /// Owner operation: every composition entry as the kernel runs it —
    /// what `inseam plugins` prints, for every transport.
    async fn plugins(&self) -> Result<Vec<PluginView>, SeamError>;
    /// Owner operation: install a loaded plugin from its files and mount it
    /// into the running node now — no restart. The files land under the
    /// node's data directory, the composition gains the entry, and the
    /// kernel reconciles; an entry that fails to activate (admission, a
    /// bad manifest) is rolled back and the failure is the error.
    async fn install_plugin(&self, request: InstallPluginRequest) -> Result<PluginView, SeamError>;
    /// Owner operation: the first-party settings document — every
    /// first-party entry's enable switch and complete config, defaults
    /// applied — as the running node's composition projects it.
    async fn settings(&self) -> Result<Settings, SeamError>;
    /// Owner operation: replace the first-party settings with a complete
    /// document and apply it to the running node now — the overlay is
    /// rewritten, changed entries restart, and an entry that fails to come
    /// back rolls the whole write back. The reply is the document as it
    /// stands afterwards.
    async fn configure(&self, settings: Settings) -> Result<Settings, SeamError>;

    // The network operations are required, not defaulted: a provider on a
    // node without the network entries mounted answers each one with
    // `SeamError::Unavailable` naming the missing entry, so a transport
    // never has to guess whether "no network" means unmounted or broken.

    /// Owner operation: the network as this node sees it — every roster
    /// node with what the last sync learned about it, every host with its
    /// stewards, and the size of the replicated log.
    async fn network(&self) -> Result<NetworkView, SeamError>;
    /// Owner operation: mint an invitation for another node to join
    /// through this one; the owner carries its text form across.
    async fn invite(&self) -> Result<Invitation, SeamError>;
    /// Owner operation: join the network an invitation names — dial the
    /// inviter, present the token, sync once — and report the network as
    /// it looks afterwards.
    async fn join(&self, request: JoinRequest) -> Result<NetworkView, SeamError>;
    /// Owner operation: expel a node — every node stops admitting it and
    /// drops its logs; this node disconnects it now.
    async fn expel(&self, request: ExpelRequest) -> Result<NetworkView, SeamError>;
    /// Owner operation: one sync round with every dialable node now, and
    /// the network as it looks afterwards.
    async fn sync_now(&self) -> Result<NetworkView, SeamError>;
}

/// The first-party settings document on the wire. Its typed shape
/// (`inseam_plugins::settings::SettingsDocument`) is built from the plugin
/// config types, which live above this seam, so the seam carries it as
/// the JSON both GUIs already speak and the provider parses it into the
/// typed document at its boundary. `{ "<entry id>": { "enabled": bool,
/// "config": { … } } }` for configured entries; `{ "enabled": bool }` for
/// toggle-only ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Settings(pub serde_json::Value);

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
    /// How the query ran — for a client that wants to know why an answer
    /// took as long as it did, not for choosing among the results.
    pub meta: QueryMeta,
}

/// Timing and shape of one served query. `elapsed_ms` covers the whole
/// operation as the transport saw it; the trace breaks the provider's share
/// into phases.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryMeta {
    /// Wall-clock from dispatch to response, including result rendering.
    pub elapsed_ms: u64,
    /// The limit the node served after clamping the request.
    pub limit: u32,
    #[serde(flatten)]
    pub trace: QueryTrace,
    /// What each node the query fanned out to answered
    /// (`design/discovery.md`): its result count, or why it gave none.
    /// Empty on a node that fanned out to nobody.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote: Vec<FanOutSummary>,
}

/// One fanned-out node's part in a query, as the caller sees it: how many
/// results it contributed before the merge, how long it took, and the
/// error when it contributed none. A failed node never fails the query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanOutSummary {
    pub node: NodeId,
    pub results: u32,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    /// The node whose index produced this result when a fan-out did;
    /// `None` for the answering node's own index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<NodeId>,
}

/// Envelope fields rendered for clients: dates as `YYYY-MM-DD`, length as
/// the structured unit and value a follow-up `scan` needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeView {
    pub source_type: String,
    pub content_type: String,
    /// `{"unit": "lines", "value": n}` for text the index has read — the
    /// bound a `scan` range can reach — else `bytes`.
    pub length: ContentLength,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The envelope's content digest, when the steward has one — carried
    /// so results merged across nodes collapse by it exactly as one node's
    /// results do (`design/finder.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<ContentDigest>,
}

/// One fragment that earned its source a place in the ranking: enough to
/// choose it, and where it sits so a `scan` can widen around it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentHint {
    pub fragment: FragmentId,
    pub mimetype: String,
    /// This fragment's own score on the query's scale (1.0 is the top
    /// result's source), so a client sees which hint made the hit.
    pub score: f64,
    /// Where the fragment sits in its source: `{"unit": "lines", "start",
    /// "end"}` for text, which `scan` accepts verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extent: Option<Extent>,
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
    pub extent: Option<Extent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Where the fragment's bytes live when it holds a reference instead of
    /// text (an image a document links to); `fetch_bytes` serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_address: Option<Address>,
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

/// Most lines one `scan` serves: a range past this is clamped to it, and
/// the response's `end` says where it stopped. A client that wants more
/// scans again from there or climbs to `fetch` — the rung for the whole
/// thing (`design/finder.md`).
pub const SCAN_LINES_MAX: u64 = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRequest {
    pub address: Address,
    /// 1-based inclusive line range. `end` is clamped to the last line and
    /// to [`SCAN_LINES_MAX`] lines after `start`; a zero start, an end
    /// before its start, or a start past the last line is refused.
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResponse {
    pub address: Address,
    /// The text type of what was read: the source's, or the stand-in
    /// fragment's.
    pub mimetype: String,
    /// The lines actually served, 1-based inclusive: `end` is the request's
    /// after clamping.
    pub start: u64,
    pub end: u64,
    /// How many lines the scanned text has in all, when the index knows —
    /// the recorded line count of a text source, or the stand-in
    /// fragment's; absent for a text source the index never read as text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_total: Option<u64>,
    pub text: String,
    /// Set when the source is not text and the scan was served from a
    /// descendant text fragment instead — a transcript for a video
    /// (`design/finder.md`).
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

/// Most bytes one `fetch_bytes` response carries: content past this is
/// refused with [`SeamError::FetchTooLarge`] rather than streamed, since
/// operation messages are single JSON values (`design/node-api.md`).
pub const FETCH_BYTES_MAX: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchBytesRequest {
    pub address: Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchBytesResponse {
    pub address: Address,
    pub content_type: String,
    /// Standard base64 on the wire, like every file an operation carries.
    pub bytes: FileBytes,
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
    /// Put every LLM-using transform on this lane for the run (`batch` is
    /// the large, time-insensitive run); `None` keeps each transform's
    /// configured lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_lane: Option<LlmLane>,
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
    /// The steward's node when the row was learned from a peer's log;
    /// absent for a source this node stewards itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<NodeId>,
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
    /// The scopes configured for this host — an `index` request may name
    /// one verbatim as its `root`.
    #[serde(default)]
    pub roots: Vec<String>,
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
pub struct SetCaptureNumberRequest {
    pub number: PhoneNumber,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyCaptureNumberRequest {
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeGrantRequest {
    pub grant: GrantId,
}

// ---------------------------------------------------------------------------
// Plugins: listing and runtime installation
// ---------------------------------------------------------------------------

/// Most files one plugin upload may carry: the artifact, its manifest and
/// checks, and the fixtures the checks name. `usize` because it bounds a
/// `Vec` length.
pub const PLUGIN_FILES_MAX: usize = 64;
/// Most bytes one plugin upload may carry in total, decoded.
pub const PLUGIN_UPLOAD_BYTES_MAX: u64 = 32 * 1024 * 1024;
/// Longest a plugin id (the composition entry id and install directory).
pub const PLUGIN_ID_CHARS_MAX: usize = 64;

/// A composition entry id chosen for an installed plugin: lowercase ASCII
/// letters, digits, `-` and `_`, starting with a letter or digit, at most
/// [`PLUGIN_ID_CHARS_MAX`] characters — safe as a directory name and as an
/// entry id anywhere the composition is rendered.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginId(String);

impl PluginId {
    pub fn new(id: &str) -> Result<Self, SeamError> {
        if id.is_empty() {
            return Err(SeamError::Refused("plugin id is empty".to_string()));
        }
        if id.chars().count() > PLUGIN_ID_CHARS_MAX {
            return Err(SeamError::Refused(format!(
                "plugin id `{id}` is longer than {PLUGIN_ID_CHARS_MAX} characters"
            )));
        }
        let well_formed = id.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '_'
        });
        if !well_formed {
            return Err(SeamError::Refused(format!(
                "plugin id `{id}` may only use lowercase letters, digits, `-` and `_`"
            )));
        }
        let starts_plainly = id
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        if !starts_plainly {
            return Err(SeamError::Refused(format!(
                "plugin id `{id}` must start with a letter or digit"
            )));
        }
        Ok(Self(id.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PluginId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value).map_err(|error| error.to_string())
    }
}

impl From<PluginId> for String {
    fn from(id: PluginId) -> Self {
        id.0
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// File contents on the wire: standard base64 in JSON, so an artifact
/// travels inside the same typed message as everything else.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct FileBytes(pub Vec<u8>);

impl std::fmt::Debug for FileBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileBytes({} bytes)", self.0.len())
    }
}

impl Serialize for FileBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use base64::Engine as _;
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for FileBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use base64::Engine as _;
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// One file of a plugin directory: the artifact (`<name>.wasm`), its
/// manifest and golden checks (`<name>.manifest.toml`, `<name>.checks.toml`),
/// and any fixtures the checks name — paths relative to the plugin
/// directory, as the registry lays them out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginFile {
    pub path: String,
    pub bytes: FileBytes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallPluginRequest {
    /// The composition entry id, and the directory name under the node's
    /// `plugins/`.
    pub id: PluginId,
    pub files: Vec<PluginFile>,
    /// The entry's config (`cooldown_days`, `fuel`, `admission`…); empty
    /// takes the bridge's defaults.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub config: toml::Table,
}

/// Where a fiber stands, as owner surfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PluginState {
    Active,
    /// Waiting on services nothing provides yet (`PluginView::missing`).
    Pending,
    /// `apply` failed; contained to this entry.
    Failed {
        reason: String,
    },
}

/// One declared secret a parked plugin waits for, with the owner-facing
/// reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretNeedView {
    pub env: String,
    pub purpose: String,
}

/// One composition entry as the kernel runs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginView {
    pub id: String,
    /// The plugin ref: a linked plugin's name or `wasm:<artifact>`.
    pub plugin: String,
    pub state: PluginState,
    /// Labels of the fiber's live effects — what it owns right now.
    pub effects: Vec<String>,
    /// Services a pending fiber waits on.
    pub missing: Vec<String>,
    pub missing_secrets: Vec<SecretNeedView>,
}

impl From<FiberView> for PluginView {
    fn from(fiber: FiberView) -> Self {
        Self {
            id: fiber.id,
            plugin: fiber.plugin,
            state: match fiber.state {
                FiberState::Active => PluginState::Active,
                FiberState::Pending => PluginState::Pending,
                FiberState::Failed(reason) => PluginState::Failed { reason },
            },
            effects: fiber.effects,
            missing: fiber.missing,
            missing_secrets: fiber
                .missing_secrets
                .into_iter()
                .map(|SecretNeed { env, purpose }| SecretNeedView { env, purpose })
                .collect(),
        }
    }
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
    pub vector_index_ready: bool,
    /// Bytes the store occupies on the node's disk (database plus its
    /// write-ahead log).
    pub store_bytes: u64,
    /// Bytes of source content the catalog covers, summed from enumeration's
    /// raw sizes — what the hosts hold, not what the node stores.
    pub content_bytes: u64,
    pub embedding_model: Option<String>,
    pub embedding_dimensions: usize,
    /// Which fragments carry vectors under the bound identity.
    pub embedding_vectors: VectorScope,
    pub reembed_pending: bool,
    /// Vectors the digest-keyed embedding cache holds (`design/indexing.md`).
    #[serde(default)]
    pub cached_embeddings: u64,
    /// Transform outputs the digest-keyed transform cache holds.
    #[serde(default)]
    pub cached_transform_outputs: u64,
    /// Sources learned from peers' logs rather than stewarded here; counted
    /// within `sources`.
    #[serde(default)]
    pub remote_sources: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RepairRequest {
    #[serde(default)]
    pub rebuild: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairOutcome {
    Empty,
    AlreadyReady,
    Built,
    Rebuilt,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RepairReport {
    pub search_rows: u64,
    pub vectors_converted: u64,
    pub outcome: RepairOutcome,
    pub vector_index_ready: bool,
}

// ---------------------------------------------------------------------------
// Network: the owner's view of the roster and the replicated log
// ---------------------------------------------------------------------------

/// The network as this node sees it (`design/roster.md`): the roster's
/// durable facts, and beside each node what the last sync learned by
/// trying — never a synced "online" bit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkView {
    /// This node's own record as last published.
    pub local: NodeRecord,
    /// Every admitted node, this one included, ordered by id.
    pub nodes: Vec<NetworkNodeView>,
    /// Every known host with its stewards, ordered by host id.
    pub hosts: Vec<NetworkHostView>,
    pub log: LogSummary,
}

/// One roster node as owner surfaces show it: the synced record, and the
/// local session knowledge beside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkNodeView {
    pub record: NodeRecord,
    pub is_local: bool,
    /// A session with the node is open, or the last exchange succeeded.
    pub live: bool,
    /// When the last successful exchange happened, as `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The hosts this node stewards.
    #[serde(default)]
    pub hosts: Vec<HostId>,
}

/// One known host and the nodes claiming to steward it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkHostView {
    pub host: HostRecord,
    pub stewards: Vec<NodeId>,
}

/// How much replicated knowledge this node holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LogSummary {
    /// Log entries held across every origin.
    pub entries: u64,
    /// Origins whose logs this node holds.
    pub origins: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    /// The invitation's text form (`inseam-invite:…`), as the inviting
    /// node rendered it; parsed by the provider.
    pub invitation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpelRequest {
    pub node: NodeId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_ids_are_directory_safe() {
        let accepted = ["ocr", "my-plugin_2", "a", "0abc"];
        for id in accepted {
            assert!(PluginId::new(id).is_ok(), "{id} should be accepted");
        }
        let refused = ["", "-ocr", "Ocr", "ocr/evil", "../x", "o c r", "ocr."];
        for id in refused {
            assert!(PluginId::new(id).is_err(), "{id} should be refused");
        }
        let long = "a".repeat(PLUGIN_ID_CHARS_MAX + 1);
        assert!(PluginId::new(&long).is_err());
    }

    #[test]
    fn file_bytes_travel_as_base64() {
        let file = PluginFile {
            path: "ocr.wasm".to_string(),
            bytes: FileBytes(vec![0, 97, 115, 109]),
        };
        let json = serde_json::to_string(&file).expect("serializes");
        assert_eq!(json, r#"{"path":"ocr.wasm","bytes":"AGFzbQ=="}"#);
        let back: PluginFile = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, file);
        assert!(serde_json::from_str::<PluginFile>(r#"{"path":"x","bytes":"!!"}"#).is_err());
    }

    #[test]
    fn a_failed_fiber_reports_its_reason() {
        let view = PluginView::from(FiberView {
            id: "ocr".to_string(),
            plugin: "wasm:/p/ocr.wasm".to_string(),
            state: FiberState::Failed("admission".to_string()),
            effects: Vec::new(),
            missing: Vec::new(),
            missing_secrets: vec![SecretNeed {
                env: "KEY".to_string(),
                purpose: "why".to_string(),
            }],
        });
        assert_eq!(
            view.state,
            PluginState::Failed {
                reason: "admission".to_string()
            }
        );
        assert_eq!(view.missing_secrets[0].env, "KEY");
        let json = serde_json::to_value(&view).expect("serializes");
        assert_eq!(json["state"]["state"], "failed");
        assert_eq!(json["state"]["reason"], "admission");
    }

    #[test]
    fn network_view_roundtrips_through_serde() {
        use inseam_kernel::network::NodeCapabilities;
        let local = NodeRecord {
            id: NodeId::from_bytes([1; 32]),
            display_name: "mini".to_string(),
            endpoints: Vec::new(),
            capabilities: NodeCapabilities {
                always_on: true,
                deep_index: true,
                relays: true,
            },
        };
        let host = HostId::new("fs-mini").expect("valid");
        let view = NetworkView {
            local: local.clone(),
            nodes: vec![NetworkNodeView {
                record: local,
                is_local: true,
                live: true,
                last_sync: None,
                last_error: None,
                hosts: vec![host.clone()],
            }],
            hosts: vec![NetworkHostView {
                host: HostRecord {
                    id: host,
                    kind: "fs".to_string(),
                    display_name: "Mini".to_string(),
                },
                stewards: vec![NodeId::from_bytes([1; 32])],
            }],
            log: LogSummary {
                entries: 12,
                origins: 1,
            },
        };
        let json = serde_json::to_value(&view).expect("serializes");
        assert!(
            json["nodes"][0].get("last_sync").is_none(),
            "absent dates are not written"
        );
        assert_eq!(json["log"]["entries"], 12);
        assert_eq!(json["hosts"][0]["stewards"][0], "01".repeat(32));
        let back: NetworkView = serde_json::from_value(json).expect("parses");
        assert_eq!(back, view);
    }

    #[test]
    fn network_requests_parse_from_their_wire_form() {
        let join: JoinRequest =
            serde_json::from_str(r#"{"invitation":"inseam-invite:abc"}"#).expect("parses");
        assert_eq!(join.invitation, "inseam-invite:abc");
        let expel: ExpelRequest =
            serde_json::from_str(&format!(r#"{{"node":"{}"}}"#, "02".repeat(32))).expect("parses");
        assert_eq!(expel.node, NodeId::from_bytes([2; 32]));
        assert!(serde_json::from_str::<ExpelRequest>(r#"{"node":"short"}"#).is_err());
    }
}
