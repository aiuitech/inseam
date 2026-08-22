//! The `operations` provider (`design/node-api.md`): serves the typed
//! operation messages by consuming `store`, `connections`, `finder`,
//! `sweep`, and — for the owner's grant operations — `oauth`. Transports
//! (CLI, FFI, HTTP) consume this seam and stay logic-free. Boundary
//! enforcement is the [`OperationRequest`] guard on dispatch:
//! access-control listeners deny, and denial is monotonic.

use std::sync::Arc;

use inseam_kernel::address::{Address, HostId};
use inseam_kernel::store::{
    CatalogRow, CatalogSelection, IndexStore, StoredFragment, StoredSource,
};
use inseam_kernel::substrate::{
    ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError, Verdict, STORE,
};
use inseam_seams::connection::{
    resolve_default, Connection, Connections, Registration as ConnectionRegistration,
    CONNECTIONS,
};
use inseam_seams::finder::{Finder, RankedFragment, FINDER};
use inseam_seams::oauth::{
    AuthorizationCallback, AuthorizationStarted, Grant, GrantId, OAuth, OAUTH,
};
use inseam_seams::operations::{
    AuthorizeGrantRequest, AwaitAuthorizationRequest, CatalogFilter, CatalogRequest,
    CatalogResponse, CatalogSourceView, EnvelopeView, ExpandRequest,
    ExpandResponse, FetchRequest, FetchResponse, FragmentHint, FragmentView, GrantView,
    HostView, IndexRequest, OperationRequest, Operations, QueryRequest, QueryResponse,
    QueryResult, RelationView, RevokeGrantRequest, ScanRequest, ScanResponse, StatusReport,
    OPERATIONS,
};
use inseam_seams::sweep::{IndexReport, Sweep, SweepRequest, SWEEP};
use inseam_seams::dates::ymd;
use inseam_seams::text::{is_indexable_text, preview, slice_lines};
use inseam_seams::SeamError;

/// Characters of fragment text shown in hints and expand views.
const PREVIEW_CHARS: usize = 280;
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
            Inject::optional("oauth"),
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

    async fn source_at(&self, address: &Address) -> Result<StoredSource, SeamError> {
        self.store
            .source_by_address(address).await?
            .ok_or_else(|| SeamError::UnknownSource(address.clone()))
    }

    /// The connection serving a host, for reads: a cataloged source whose
    /// steward has since been unmounted is an unknown host, not a crash.
    fn connection_to(&self, host: &HostId) -> Result<Arc<dyn Connection>, SeamError> {
        self.connections
            .resolve(host)
            .map(|r| Arc::clone(&r.connection))
            .ok_or_else(|| SeamError::UnknownHost(host.clone()))
    }

    /// The host an index request means: the one it names, else the only
    /// one mounted.
    fn host_for_index(&self, request: &IndexRequest) -> Result<Arc<ConnectionRegistration>, SeamError> {
        match &request.host {
            Some(host) => self
                .connections
                .resolve(host)
                .ok_or_else(|| SeamError::UnknownHost(host.clone())),
            None => resolve_default(self.connections.as_ref()),
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
}

#[async_trait::async_trait]
impl Operations for OperationsService {
    async fn query(&self, request: QueryRequest) -> Result<QueryResponse, SeamError> {
        self.guard("query")?;
        let limit = request.limit.clamp(1, 50);
        let ranked = self.finder.query(&request.text, limit).await?;
        let results = ranked
            .into_iter()
            .map(|r| QueryResult {
                address: r.source.address.clone(),
                score: round3(r.score),
                summary: r.summary,
                envelope: envelope_view(&r.source),
                hints: r.hints.iter().map(hint_view).collect(),
                replicas: r.replicas,
            })
            .collect();
        Ok(QueryResponse { results })
    }

    async fn expand(&self, request: ExpandRequest) -> Result<ExpandResponse, SeamError> {
        self.guard("expand")?;
        let source = self.source_at(&request.address).await?;
        let expansion = self.finder.expand(&source).await?;
        let mut sources_cache: std::collections::HashMap<_, Address> = Default::default();
        let mut neighbors = Vec::with_capacity(expansion.neighbors.len());
        for f in &expansion.neighbors {
            let mut address = None;
            if let Some(sid) = f.source {
                match sources_cache.get(&sid) {
                    Some(a) => address = Some(a.clone()),
                    None => {
                        address = self.store.source(sid).await.ok().flatten().map(|s| s.address);
                        if let Some(a) = &address {
                            sources_cache.insert(sid, a.clone());
                        }
                    }
                }
            }
            neighbors.push(fragment_view(f, address));
        }
        let neighbors = neighbors;
        Ok(ExpandResponse {
            address: source.address.clone(),
            summary: self.store.summary_of(source.id).await?,
            fragments: expansion
                .fragments
                .iter()
                .map(|f| fragment_view(f, None))
                .collect(),
            relations: expansion.relations.iter().map(RelationView::from).collect(),
            neighbors,
        })
    }

    async fn scan(&self, request: ScanRequest) -> Result<ScanResponse, SeamError> {
        self.guard("scan")?;
        let source = self.source_at(&request.address).await?;
        let (start, end) = (request.start.max(1), request.end.max(request.start));
        if is_indexable_text(&source.envelope.content_type) {
            let text = self
                .connection_to(&source.address.host)?
                .read_lines(&source.address, start, end)
                .await?;
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
        let fragments = self.store.fragments_of(source.id).await?;
        let best = fragments
            .iter()
            .filter(|f| !f.mimetype.is_summary())
            .filter_map(|f| f.text.as_ref().map(|t| (f, t)))
            .max_by_key(|(_, t)| t.len());
        let Some((fragment, text)) = best else {
            return Err(SeamError::NothingToScan(source.address));
        };
        let sliced =
            slice_lines(text, start, end).map_err(SeamError::failed)?;
        Ok(ScanResponse {
            address: source.address,
            mimetype: fragment.mimetype.to_string(),
            start,
            end,
            text: sliced,
            served_from_fragment: Some(fragment.id),
        })
    }

    async fn fetch(&self, request: FetchRequest) -> Result<FetchResponse, SeamError> {
        self.guard("fetch")?;
        let source = self.source_at(&request.address).await?;
        if !is_indexable_text(&source.envelope.content_type) {
            return Err(SeamError::BinaryFetch(
                source.address,
                source.envelope.content_type.to_string(),
            ));
        }
        let text = self
            .connection_to(&source.address.host)?
            .read_text(&source.address)
            .await?;
        Ok(FetchResponse {
            address: source.address,
            content_type: source.envelope.content_type.to_string(),
            text,
        })
    }

    async fn index(&self, request: IndexRequest) -> Result<IndexReport, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        let steward = self.host_for_index(&request)?;
        self.sweep
            .sweep(&SweepRequest {
                host: steward.host.id.clone(),
                root: request.root,
                rebuild: request.rebuild,
                deep_budget: request.deep_budget,
            })
            .await
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

    async fn status(&self) -> Result<StatusReport, SeamError> {
        let stats = self.store.stats().await?;
        let search_rows = self.store.search_rows_count().await.unwrap_or(0);
        let identity = self.store.embedding_identity();
        Ok(StatusReport {
            sources: stats.sources,
            indexed_sources: stats.indexed_sources,
            fragments: stats.fragments,
            relations: stats.relations,
            keyed_fragments: stats.keyed_fragments,
            search_rows,
            store_bytes: stats.store_bytes,
            content_bytes: stats.content_bytes,
            embedding_model: identity.as_ref().map(|(m, _)| m.clone()),
            embedding_dimensions: identity.map(|(_, d)| d).unwrap_or(0),
            reembed_pending: self.store.reembed_pending(),
        })
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
    }
}

fn envelope_view(source: &StoredSource) -> EnvelopeView {
    let e = &source.envelope;
    EnvelopeView {
        source_type: e.source_type.clone(),
        content_type: e.content_type.to_string(),
        length: e.length.to_string(),
        created: e.created.map(ymd),
        modified: e.modified.map(ymd),
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
