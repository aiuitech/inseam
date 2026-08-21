//! The `operations` provider (`design/node-api.md`): serves the typed
//! operation messages by consuming `store`, `connection`, `finder`, and
//! `sweep`. Transports (CLI, FFI, future HTTP/MCP) consume this seam and
//! stay logic-free. Boundary enforcement is the [`OperationRequest`] guard
//! on dispatch: access-control listeners deny, and denial is monotonic.

use std::sync::Arc;

use inseam_kernel::address::Address;
use inseam_kernel::store::{IndexStore, StoredFragment, StoredSource};
use inseam_kernel::substrate::{
    ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError, Verdict, STORE,
};
use inseam_seams::connection::{Connection, CONNECTION};
use inseam_seams::finder::{Finder, RankedFragment, FINDER};
use inseam_seams::operations::{
    EnvelopeView, ExpandRequest, ExpandResponse, FetchRequest, FetchResponse, FragmentHint,
    FragmentView, IndexRequest, OperationRequest, Operations, QueryRequest, QueryResponse,
    QueryResult, RelationView, ScanRequest, ScanResponse, StatusReport, OPERATIONS,
};
use inseam_seams::sweep::{IndexReport, Sweep, SweepRequest, SWEEP};
use inseam_seams::dates::ymd;
use inseam_seams::text::{is_indexable_text, preview, slice_lines};
use inseam_seams::SeamError;

/// Characters of fragment text shown in hints and expand views.
const PREVIEW_CHARS: usize = 280;

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
            Inject::required("connection"),
            Inject::required("finder"),
            Inject::required("sweep"),
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
            connection: cx.get(&CONNECTION)?,
            finder: cx.get(&FINDER)?,
            sweep: cx.get(&SWEEP)?,
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
    connection: Arc<dyn Connection>,
    finder: Arc<dyn Finder>,
    sweep: Arc<dyn Sweep>,
    bus: EventBus,
}

impl OperationsService {
    async fn source_at(&self, address: &Address) -> Result<StoredSource, SeamError> {
        self.store
            .source_by_address(address).await?
            .ok_or_else(|| SeamError::UnknownSource(address.clone()))
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
                .connection
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
        let text = self.connection.read_text(&source.address).await?;
        Ok(FetchResponse {
            address: source.address,
            content_type: source.envelope.content_type.to_string(),
            text,
        })
    }

    async fn index(&self, request: IndexRequest) -> Result<IndexReport, SeamError> {
        // Owner operation: not boundary-guarded (local transports only).
        self.sweep
            .sweep(&SweepRequest {
                root: request.root,
                rebuild: request.rebuild,
            })
            .await
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
            entities: stats.entities,
            search_rows,
            embedding_model: identity.as_ref().map(|(m, _)| m.clone()),
            embedding_dimensions: identity.map(|(_, d)| d).unwrap_or(0),
            reembed_pending: self.store.reembed_pending(),
        })
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
