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
use inseam_kernel::substrate::{FiberState, FiberView, Guard, SecretNeed, ServiceKey};
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
    /// Owner operation: every composition entry as the kernel runs it —
    /// what `inseam plugins` prints, for every transport.
    async fn plugins(&self) -> Result<Vec<PluginView>, SeamError>;
    /// Owner operation: install a loaded plugin from its files and mount it
    /// into the running node now — no restart. The files land under the
    /// node's data directory, the composition gains the entry, and the
    /// kernel reconciles; an entry that fails to activate (admission, a
    /// bad manifest) is rolled back and the failure is the error.
    async fn install_plugin(&self, request: InstallPluginRequest) -> Result<PluginView, SeamError>;
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
    Failed { reason: String },
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
    pub reembed_pending: bool,
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
}
