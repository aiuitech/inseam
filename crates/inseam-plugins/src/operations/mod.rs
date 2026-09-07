//! The `operations` provider (`design/node-api.md`): serves the typed
//! operation messages by consuming `store`, `connections`, `finder`,
//! `sweep`, and — for the owner's grant operations — `oauth`. Transports
//! (CLI, FFI, HTTP) consume this seam and stay logic-free. Boundary
//! enforcement is the [`OperationRequest`] guard on dispatch:
//! access-control listeners deny, and denial is monotonic.
//!
//! The ladder's rungs live in [`ladder`], shared with the routing handler
//! so a routed request is served by the same code as a local one. When
//! the network entries are mounted, `query` also fans out through the
//! `routing` seam and merges what came back ([`merge`]), and the ladder
//! reads sources other nodes steward through the same seam; the owner's
//! network operations are [`network`].
//!
//! The plugin operations (`plugins`, `install_plugin`) reach the kernel
//! through the `composition` service: this provider runs inside the tree
//! and cannot hold the kernel, so it submits edits the distribution
//! applies (`design/composition.md`).

mod install;
pub(crate) mod ladder;
mod merge;
mod network;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{HostId, Timestamp};
use inseam_kernel::store::{
    CatalogRow, CatalogSelection, IndexStore, SearchIndexRepair,
    SearchIndexRepairOutcome, StoredSource, VectorScope,
};
use inseam_kernel::substrate::{
    ApplyCx, CompositionEdit, CompositionEditor, Entry, EventBus, Facts, Inject, Manifest,
    Plugin, PluginError, SubstrateError, Verdict, COMPOSITION, STORE,
};
use inseam_seams::connection::{
    resolve_default, Connections, Registration as ConnectionRegistration, CONNECTIONS,
};
use inseam_seams::finder::{Finder, QueryTrace, FINDER};
use inseam_seams::node::NODE;
use inseam_seams::oauth::{
    AuthorizationCallback, AuthorizationStarted, Grant, GrantId, OAuth, OAUTH,
};
use inseam_seams::operations::{
    AuthorizeGrantRequest, AwaitAuthorizationRequest, CatalogFilter, CatalogRequest,
    CatalogResponse, CatalogSourceView, ExpandRequest, ExpandResponse, ExpelRequest,
    FanOutSummary, FetchBytesRequest, FetchBytesResponse, FetchRequest, FetchResponse,
    GrantView, HostView, IndexRequest, InstallPluginRequest, JoinRequest, NetworkView,
    OperationRequest, Operations, PluginView, QueryMeta, QueryRequest, QueryResponse,
    QueryResult, RepairOutcome, RepairReport, RepairRequest, RevokeGrantRequest, ScanRequest,
    ScanResponse, Settings, StatusReport, OPERATIONS,
};
use inseam_seams::roster::{Invitation, ROSTER};
use inseam_seams::routing::{FanOutReply, Routing, ROUTING};
use inseam_seams::sync::SYNC;
use crate::settings::{SettingsDocument, WriteMode};
use inseam_seams::sweep::{IndexMonitor, IndexReport, Sweep, SweepRequest, SWEEP};
use inseam_seams::dates::ymd;
use inseam_seams::SeamError;

use ladder::Reader;
use network::NetworkOperations;

/// Catalog entries one listing may return; counts still cover everything.
const CATALOG_LIMIT_MAX: u32 = 10_000;

pub struct OperationsPlugin;

pub struct OperationsFactory;

impl inseam_kernel::substrate::PluginFactory for OperationsFactory {
    fn name(&self) -> &str {
        "operations"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(OperationsPlugin))
    }
}

#[async_trait::async_trait]
impl Plugin for OperationsPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[
            Inject::required("store"),
            Inject::required("connections"),
            Inject::required("finder"),
            Inject::required("sweep"),
            Inject::required("composition"),
            Inject::optional("oauth"),
            // The network seams are optional so a node composed without
            // them keeps every local operation; each network operation
            // then names the entry it lacks.
            Inject::optional("routing"),
            Inject::optional("roster"),
            Inject::optional("sync"),
            Inject::optional("node"),
        ];
        Manifest {
            name: "operations",
            inject: INJECT,
            provides: &["operations"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let service = OperationsService {
            store: cx.get(&STORE)?,
            connections: cx.get(&CONNECTIONS)?,
            finder: cx.get(&FINDER)?,
            sweep: cx.get(&SWEEP)?,
            oauth: cx.try_get(&OAUTH)?,
            network: NetworkOperations {
                routing: cx.try_get(&ROUTING)?,
                roster: cx.try_get(&ROSTER)?,
                sync: cx.try_get(&SYNC)?,
                node: cx.try_get(&NODE)?,
            },
            composition: cx.get(&COMPOSITION)?,
            data_dir: cx.data_dir().to_path_buf(),
            bus: cx.bus().clone(),
        };
        cx.provide(
            &OPERATIONS,
            Arc::new(service) as Arc<dyn Operations>,
            Facts::new(),
        )?;
        Ok(())
    }
}

pub struct OperationsService {
    store: Arc<IndexStore>,
    connections: Arc<dyn Connections>,
    finder: Arc<dyn Finder>,
    sweep: Arc<dyn Sweep>,
    /// Absent when no oauth entry is active: the grant operations then say
    /// so instead of pretending there are no grants.
    oauth: Option<Arc<dyn OAuth>>,
    /// The network seams, each absent on a node composed without it.
    network: NetworkOperations,
    /// The kernel's edit channel: how a plugin asks the distribution to
    /// change the composition of the node it runs in.
    composition: Arc<CompositionEditor>,
    /// Where installed plugins live: `<data-dir>/plugins/<id>/`.
    data_dir: PathBuf,
    bus: EventBus,
}

impl OperationsService {
    fn oauth(&self) -> Result<&Arc<dyn OAuth>, SeamError> {
        self.oauth.as_ref().ok_or_else(|| {
            SeamError::Unavailable("the oauth entry is not active on this node".to_string())
        })
    }

    fn grant(&self, id: &GrantId) -> Result<Arc<dyn Grant>, SeamError> {
        self.oauth()?.grant(id).ok_or_else(|| {
            SeamError::Unavailable(format!("no grant `{id}` is configured on this node"))
        })
    }

    /// How this node reads a host's content: its own connection when it
    /// stewards the host, else the routing seam toward whichever node
    /// does. A cataloged source whose host nobody here can reach is an
    /// unknown host, not a crash.
    fn reader_for(&self, host: &HostId) -> Result<Reader, SeamError> {
        if let Some(registration) = self.connections.resolve(host) {
            return Ok(Reader::Connection(Arc::clone(&registration.connection)));
        }
        match &self.network.routing {
            Some(routing) => Ok(Reader::Routing(Arc::clone(routing))),
            None => Err(SeamError::UnknownHost(host.clone())),
        }
    }

    /// Whether this node's own index answers an `expand` of the source:
    /// it stewards the host, or it deep-indexed the source itself
    /// (`design/discovery.md`: a node may index anything it can fetch).
    fn expands_locally(&self, source: &StoredSource) -> bool {
        if self.connections.resolve(&source.address.host).is_some() {
            return true;
        }
        source.root_fragment.is_some()
    }

    /// The host an index request means: the one it names, else the only
    /// one mounted.
    fn host_for_index(&self, request: &IndexRequest) -> Result<Arc<ConnectionRegistration>, SeamError> {
        let steward = match &request.host {
            Some(host) => self
                .connections
                .resolve(host)
                .ok_or_else(|| SeamError::UnknownHost(host.clone()))?,
            None => resolve_default(self.connections.as_ref())?,
        };
        // A fetch-only host has nothing to enumerate: naming it as a scope
        // is a mistake to say out loud, not an empty sweep.
        if steward.capabilities.enumerates {
            Ok(steward)
        } else {
            Err(SeamError::Refused(format!(
                "host `{}` serves fetches only and cannot be swept",
                steward.host.id
            )))
        }
    }

    /// The boundary guard: every scoped operation passes here before it is
    /// served. Local transports act as the owner today; remote requesters
    /// arrive with the boundary work (`design/access-control.md`).
    fn guard(&self, operation: &'static str) -> Result<(), SeamError> {
        match self.bus.check(&OperationRequest {
            operation,
            requester: "owner".to_string(),
        }) {
            Verdict::Allow => Ok(()),
            Verdict::Deny(reason) => Err(SeamError::Refused(reason)),
        }
    }

    async fn index_run(
        &self,
        request: IndexRequest,
        monitor: Option<Arc<dyn IndexMonitor>>,
    ) -> Result<IndexReport, SeamError> {
        let steward = self.host_for_index(&request)?;
        self.sweep
            .sweep(&SweepRequest {
                host: steward.host.id.clone(),
                root: request.root,
                rebuild: request.rebuild,
                deep_budget: request.deep_budget,
                llm_lane: request.llm_lane,
                monitor,
            })
            .await
    }
}

/// One query as a merge of this node's index with the fan-out: the local
/// finder and the routing seam run concurrently, and what the network
/// answers is merged by rank. A node without routing, or one that fans
/// out to nobody, gets its local list back untouched.
async fn query_across(
    finder: &dyn Finder,
    routing: Option<&dyn Routing>,
    text: &str,
    limit: usize,
) -> Result<(Vec<QueryResult>, QueryTrace, Vec<FanOutSummary>), SeamError> {
    let (local, replies) = tokio::join!(
        ladder::query(finder, text, limit),
        fan_out(routing, text, limit)
    );
    let (results, trace) = local?;
    let merged = merge::merge(results, replies, limit);
    Ok((merged.results, trace, merged.remote))
}

/// The fan-out's replies, or none: a fan-out that could not even start
/// (the roster failed to list nodes) is logged and treated as no
/// replies, because a network hiccup must never fail a local query.
async fn fan_out(routing: Option<&dyn Routing>, text: &str, limit: usize) -> Vec<FanOutReply> {
    let Some(routing) = routing else {
        return Vec::new();
    };
    match routing.fan_out(text, limit).await {
        Ok(replies) => replies,
        Err(error) => {
            tracing::warn!("query fan-out did not run: {error}");
            Vec::new()
        }
    }
}

#[async_trait::async_trait]
impl Operations for OperationsService {
    async fn query(&self, request: QueryRequest) -> Result<QueryResponse, SeamError> {
        self.guard("query")?;
        let started = std::time::Instant::now();
        let limit = ladder::clamp_query_limit(request.limit);
        let routing = self.network.routing.as_deref();
        let (results, trace, remote) =
            query_across(self.finder.as_ref(), routing, &request.text, limit).await?;
        let meta = QueryMeta {
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            // The clamp above bounds `limit` to 50, so this conversion
            // cannot fail.
            limit: u32::try_from(limit).unwrap_or(50),
            trace,
            remote,
        };
        Ok(QueryResponse { results, meta })
    }

    async fn expand(&self, request: ExpandRequest) -> Result<ExpandResponse, SeamError> {
        self.guard("expand")?;
        let source = ladder::source_at(&self.store, &request.address).await?;
        if self.expands_locally(&source) {
            return ladder::expand(&self.store, self.finder.as_ref(), &source).await;
        }
        match &self.network.routing {
            Some(routing) => routing.expand(&source.address).await,
            None => Err(SeamError::UnknownHost(source.address.host.clone())),
        }
    }

    async fn scan(&self, request: ScanRequest) -> Result<ScanResponse, SeamError> {
        self.guard("scan")?;
        let source = ladder::source_at(&self.store, &request.address).await?;
        let window = ladder::scan_window(request.start, request.end)?;
        let reader = self.reader_for(&source.address.host);
        ladder::scan(&self.store, source, window, reader).await
    }

    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, SeamError> {
        self.guard("fetch")?;
        let source = ladder::source_at(&self.store, &request.address).await?;
        // Refused before any read, local or over the network.
        ladder::check_text_fetch(&source)?;
        let reader = self.reader_for(&source.address.host)?;
        ladder::fetch(&reader, source).await
    }

    async fn fetch_bytes(&self, request: FetchBytesRequest) -> Result<FetchBytesResponse, SeamError> {
        self.guard("fetch")?;
        let (content_type, known_bytes) = ladder::content_at(&self.store, &request.address).await?;
        // Refuse before reading when the catalog already knows the size;
        // otherwise the read itself is the check (paired inside the rung).
        ladder::check_bytes_bound(&request.address, known_bytes)?;
        let reader = self.reader_for(&request.address.host)?;
        ladder::fetch_bytes(&reader, request.address, content_type).await
    }

    async fn index(&self, request: IndexRequest) -> Result<IndexReport, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.index_run(request, None).await
    }

    async fn index_monitored(
        &self,
        request: IndexRequest,
        monitor: Arc<dyn IndexMonitor>,
    ) -> Result<IndexReport, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.index_run(request, Some(monitor)).await
    }

    async fn catalog(&self, request: CatalogRequest) -> Result<CatalogResponse, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let limit = request.limit.clamp(1, CATALOG_LIMIT_MAX);
        let selection = match request.filter {
            CatalogFilter::All => CatalogSelection::All,
            CatalogFilter::Indexed => CatalogSelection::Indexed,
            CatalogFilter::Pending => CatalogSelection::Pending,
        };
        let counts = self.store.catalog_counts(request.host.as_ref()).await?;
        let rows = self
            .store
            .catalog_rows(request.host.as_ref(), selection, limit)
            .await?;
        Ok(CatalogResponse {
            sources: counts.sources,
            indexed: counts.indexed,
            pending: counts.pending,
            entries: rows.iter().map(catalog_source_view).collect(),
        })
    }

    async fn hosts(&self) -> Result<Vec<HostView>, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        Ok(self
            .connections
            .snapshot()
            .iter()
            .map(|r| HostView {
                id: r.host.id.clone(),
                kind: r.host.kind.clone(),
                display_name: r.host.display_name.clone(),
                entry: r.entry_id.clone(),
                capabilities: r.capabilities,
                roots: r.roots.clone(),
            })
            .collect())
    }

    async fn grants(&self) -> Result<Vec<GrantView>, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let mut views = Vec::new();
        for grant in self.oauth()?.grants() {
            views.push(grant_view(grant.as_ref()).await);
        }
        Ok(views)
    }

    async fn authorize_grant(&self, request: AuthorizeGrantRequest) -> Result<AuthorizationStarted, SeamError> {
        self.oauth()?.authorize(&request.grant, request.redirect).await
    }

    async fn await_authorization(&self, request: AwaitAuthorizationRequest) -> Result<GrantView, SeamError> {
        let id = self.oauth()?.await_authorization(&request.state).await?;
        Ok(grant_view(self.grant(&id)?.as_ref()).await)
    }

    async fn complete_authorization(&self, callback: AuthorizationCallback) -> Result<GrantView, SeamError> {
        let id = self.oauth()?.complete_authorization(callback).await?;
        Ok(grant_view(self.grant(&id)?.as_ref()).await)
    }

    async fn revoke_grant(&self, request: RevokeGrantRequest) -> Result<GrantView, SeamError> {
        let grant = self.grant(&request.grant)?;
        grant.revoke().await?;
        Ok(grant_view(grant.as_ref()).await)
    }

    async fn plugins(&self) -> Result<Vec<PluginView>, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let snapshot = self
            .composition
            .submit(CompositionEdit::Inspect)
            .await
            .map_err(edit_error)?;
        Ok(snapshot.fibers.into_iter().map(PluginView::from).collect())
    }

    async fn settings(&self) -> Result<Settings, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let snapshot = self
            .composition
            .submit(CompositionEdit::Inspect)
            .await
            .map_err(edit_error)?;
        settings_of(&snapshot.composition)
    }

    async fn configure(&self, settings: Settings) -> Result<Settings, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let document: SettingsDocument = serde_json::from_value(settings.0)
            .map_err(|error| SeamError::Refused(format!("settings document: {error}")))?;
        document
            .validate()
            .map_err(|error| SeamError::Refused(error.to_string()))?;
        // This provider is the `operations` entry: disabling it would
        // unload the very service answering, and every transport with it.
        if !document.operations.enabled {
            return Err(SeamError::Refused(
                "the operations entry cannot be disabled from a running node".to_string(),
            ));
        }
        let patches = document
            .into_patches(WriteMode::All)
            .map_err(|error| SeamError::Refused(error.to_string()))?;
        let snapshot = self
            .composition
            .submit(CompositionEdit::Configure(patches))
            .await
            .map_err(edit_error)?;
        settings_of(&snapshot.composition)
    }

    async fn install_plugin(&self, request: InstallPluginRequest) -> Result<PluginView, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let id = request.id.clone();
        let config = request.config.clone();
        let plan = install::plan(request)?;
        // The kernel already runs an entry with this id: refuse before any
        // file is written. The distribution checks the composition file
        // again when it applies the edit (pair assertion across the channel).
        let running = self.plugins().await?;
        if running.iter().any(|plugin| plugin.id == id.as_str()) {
            return Err(SeamError::Refused(format!(
                "this node already runs an entry `{id}`; remove it from the composition first"
            )));
        }
        let directory = self.data_dir.join("plugins").join(id.as_str());
        if directory.exists() {
            return Err(SeamError::Refused(format!(
                "{} already exists; remove that directory (and any entry naming it) first",
                directory.display()
            )));
        }
        let artifact = install::write(&directory, &plan).map_err(|error| {
            SeamError::failed(format!("writing plugin files under {}: {error}", directory.display()))
        })?;
        let entry = Entry::new(id.as_str(), &format!("wasm:{}", artifact.display())).with_config(config);
        match self.composition.submit(CompositionEdit::Mount(entry)).await {
            Ok(snapshot) => snapshot
                .fibers
                .into_iter()
                .find(|fiber| fiber.id == id.as_str())
                .map(PluginView::from)
                .ok_or_else(|| {
                    SeamError::failed(format!("entry `{id}` was mounted but is missing from the snapshot"))
                }),
            Err(error) => {
                // The entry never took: the files we wrote are ours to remove.
                if let Err(remove_error) = std::fs::remove_dir_all(&directory) {
                    tracing::warn!(
                        directory = %directory.display(),
                        "could not remove the files of a plugin that failed to mount: {remove_error}"
                    );
                }
                Err(edit_error(error))
            }
        }
    }

    async fn status(&self) -> Result<StatusReport, SeamError> {
        let stats = self.store.stats().await?;
        let search_rows = self.store.search_rows_count().await.unwrap_or(0);
        let vector_index_ready = self.store.search_vector_index_ready().await.unwrap_or(false);
        let identity = self.store.embedding_identity();
        let caches = self.store.cache_counts().await?;
        Ok(StatusReport {
            sources: stats.sources,
            indexed_sources: stats.indexed_sources,
            fragments: stats.fragments,
            relations: stats.relations,
            keyed_fragments: stats.keyed_fragments,
            search_rows,
            vector_index_ready,
            store_bytes: stats.store_bytes,
            content_bytes: stats.content_bytes,
            embedding_model: identity.as_ref().map(|i| i.model.clone()),
            embedding_dimensions: identity.as_ref().map_or(0, |i| i.dimensions),
            embedding_vectors: identity.map_or(VectorScope::All, |i| i.vectors),
            reembed_pending: self.store.reembed_pending(),
            cached_embeddings: caches.embeddings,
            cached_transform_outputs: caches.transforms,
            remote_sources: stats.remote_sources,
        })
    }

    async fn repair(&self, request: RepairRequest) -> Result<RepairReport, SeamError> {
        let repair = if request.rebuild {
            SearchIndexRepair::Rebuild
        } else {
            SearchIndexRepair::Ensure
        };
        let report = self.store.repair_search_index(repair).await?;
        let outcome = match report.outcome {
            SearchIndexRepairOutcome::Empty => RepairOutcome::Empty,
            SearchIndexRepairOutcome::AlreadyReady => RepairOutcome::AlreadyReady,
            SearchIndexRepairOutcome::Built => RepairOutcome::Built,
            SearchIndexRepairOutcome::Rebuilt => RepairOutcome::Rebuilt,
        };
        Ok(RepairReport {
            search_rows: report.search_rows,
            vectors_converted: report.vectors_converted,
            outcome,
            vector_index_ready: self.store.search_vector_index_ready().await?,
        })
    }

    async fn network(&self) -> Result<NetworkView, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.network.network(&self.store).await
    }

    async fn invite(&self) -> Result<Invitation, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.network.invite().await
    }

    async fn join(&self, request: JoinRequest) -> Result<NetworkView, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let now = Timestamp::from(SystemTime::now());
        self.network.join(&self.store, request, now).await
    }

    async fn expel(&self, request: ExpelRequest) -> Result<NetworkView, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.network.expel(&self.store, request).await
    }

    async fn sync_now(&self) -> Result<NetworkView, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.network.sync_now(&self.store).await
    }
}

/// The layered composition the kernel reports, as the settings document.
fn settings_of(composition: &inseam_kernel::substrate::Composition) -> Result<Settings, SeamError> {
    let document = SettingsDocument::from_composition(composition)
        .map_err(|error| SeamError::failed(error.to_string()))?;
    let value = serde_json::to_value(document)
        .map_err(|error| SeamError::failed(format!("settings document: {error}")))?;
    Ok(Settings(value))
}

/// A composition edit's failure in seam vocabulary: a mount the node
/// refused or rolled back is a refusal the owner acts on; a runtime that
/// does not apply edits is a missing capability; the rest failed.
fn edit_error(error: SubstrateError) -> SeamError {
    match error {
        SubstrateError::EntryExists(_)
        | SubstrateError::MountFailed { .. }
        | SubstrateError::ConfigureFailed { .. } => SeamError::Refused(error.to_string()),
        SubstrateError::EditsUnserviced | SubstrateError::EditQueueFull => {
            SeamError::Unavailable(error.to_string())
        }
        other => SeamError::failed(other.to_string()),
    }
}

async fn grant_view(grant: &dyn Grant) -> GrantView {
    let spec = grant.spec();
    GrantView {
        id: spec.id.clone(),
        provider: spec.provider(),
        scopes: spec.scopes.clone(),
        client_id_env: spec.client_id_env.clone(),
        client_secret_env: spec.client_secret_env.clone(),
        state: grant.state().await,
    }
}

fn catalog_source_view(row: &CatalogRow) -> CatalogSourceView {
    CatalogSourceView {
        address: row.address.clone(),
        indexed: row.indexed,
        content_type: row.content_type.to_string(),
        raw_bytes: row.raw_bytes,
        modified: row.modified.map(ymd),
        origin: row.origin,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use inseam_kernel::address::{Address, ContentDigest, Envelope, HostId};
    use inseam_kernel::network::NodeId;
    use inseam_seams::routing::Location;

    use super::*;
    use crate::routing::fake::{StubFinder, TestSource};

    /// A routing seam that answers fan-outs with canned replies and counts
    /// how often it was asked; every other method is unused by `query`.
    struct FakeRouting {
        replies: Vec<FanOutReply>,
        fan_outs: AtomicU32,
    }

    #[async_trait::async_trait]
    impl Routing for FakeRouting {
        async fn locate(&self, _host: &HostId) -> Result<Location, SeamError> {
            Ok(Location::Unknown)
        }
        async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
            Err(SeamError::UnknownHost(address.host.clone()))
        }
        async fn read_lines(&self, address: &Address, _s: u64, _e: u64) -> Result<String, SeamError> {
            Err(SeamError::UnknownHost(address.host.clone()))
        }
        async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
            Err(SeamError::UnknownHost(address.host.clone()))
        }
        async fn describe(&self, address: &Address) -> Result<Envelope, SeamError> {
            Err(SeamError::UnknownHost(address.host.clone()))
        }
        async fn expand(&self, address: &Address) -> Result<ExpandResponse, SeamError> {
            Err(SeamError::UnknownHost(address.host.clone()))
        }
        async fn fan_out(&self, _text: &str, _limit: usize) -> Result<Vec<FanOutReply>, SeamError> {
            self.fan_outs.fetch_add(1, Ordering::SeqCst);
            Ok(self.replies.clone())
        }
    }

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    async fn finder_with(sources: &[(&str, Option<&[u8]>)]) -> (Arc<StubFinder>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(IndexStore::open(dir.path()).await.expect("opens"));
        let mut ranked = Vec::new();
        for (address, digest) in sources {
            let source = TestSource::text(address, "one line").with_digest(digest.map(ContentDigest::of_bytes));
            ranked.push(source.catalog(&store).await);
        }
        (Arc::new(StubFinder::ranked(ranked)), dir)
    }

    fn remote_result(address: &str, digest: Option<&[u8]>) -> QueryResult {
        QueryResult {
            address: address.parse().expect("valid address"),
            score: 1.0,
            summary: Some("remote".to_string()),
            envelope: inseam_seams::operations::EnvelopeView {
                source_type: "file".to_string(),
                content_type: "text/plain".to_string(),
                length: inseam_kernel::address::ContentLength::Lines(1),
                created: None,
                modified: None,
                title: None,
                content_digest: digest.map(ContentDigest::of_bytes),
            },
            hints: Vec::new(),
            replicas: Vec::new(),
            via: None,
        }
    }

    #[tokio::test]
    async fn a_query_without_routing_is_the_local_list() {
        let (finder, _dir) = finder_with(&[("inseam://fs-a/a.md", None)]).await;
        let (results, _trace, remote) = query_across(finder.as_ref(), None, "a", 8)
            .await
            .expect("queries");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].via, None);
        assert!(remote.is_empty());
    }

    #[tokio::test]
    async fn a_query_merges_the_fan_out_and_survives_a_failed_node() {
        let (finder, _dir) = finder_with(&[
            ("inseam://fs-a/shared.md", Some(b"twin")),
            ("inseam://fs-a/local.md", None),
        ])
        .await;
        let routing = FakeRouting {
            replies: vec![
                FanOutReply {
                    node: node(2),
                    results: vec![
                        remote_result("inseam://fs-a/shared.md", Some(b"twin")),
                        remote_result("inseam://drive-b/twin.md", Some(b"twin")),
                        remote_result("inseam://fs-b/remote.md", None),
                    ],
                    elapsed_ms: 12,
                    error: None,
                },
                FanOutReply {
                    node: node(3),
                    results: Vec::new(),
                    elapsed_ms: 3000,
                    error: Some("timed out after 3000 ms".to_string()),
                },
            ],
            fan_outs: AtomicU32::new(0),
        };
        let (results, _trace, remote) = query_across(finder.as_ref(), Some(&routing), "x", 8)
            .await
            .expect("queries");
        assert_eq!(routing.fan_outs.load(Ordering::SeqCst), 1);
        let addresses: Vec<String> = results.iter().map(|r| r.address.to_string()).collect();
        assert_eq!(addresses[0], "inseam://fs-a/shared.md", "ranked by both lists");
        assert_eq!(results[0].via, None, "the local copy is kept");
        assert_eq!(results[0].summary, None, "the local copy's summary, not the remote's");
        assert_eq!(
            results[0].replicas.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["inseam://drive-b/twin.md"],
            "the digest-equal remote copy collapsed into a replica"
        );
        assert!(addresses.contains(&"inseam://fs-b/remote.md".to_string()));
        assert_eq!(
            results.iter().find(|r| r.address.to_string() == "inseam://fs-b/remote.md").and_then(|r| r.via),
            Some(node(2))
        );
        assert_eq!(remote.len(), 2);
        assert_eq!(remote[1].node, node(3));
        assert_eq!(remote[1].error.as_deref(), Some("timed out after 3000 ms"));
    }
}
