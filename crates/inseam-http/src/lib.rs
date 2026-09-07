//! HTTP/JSON owner transport for an inseam node (`design/node-api.md`).
//! The transport authenticates an owner session, bounds the wire, translates
//! JSON to the existing operation messages, and serves an optional Vite
//! build. It contains no node logic and never calls the store or plugins
//! directly. API responses are not cacheable; every response carries a
//! restrictive browser security policy.
//!
//! One route is deliberately unauthenticated besides `/health`:
//! `/api/v1/oauth/callback`, where a provider sends the owner's browser
//! back after they authorize a grant from the web console. The owner's
//! browser is not where the node runs, so the loopback redirect the CLI
//! uses cannot serve it; the transport serves the redirect at its public
//! URL instead and hands the parameters to the `complete_authorization`
//! operation, which trusts nothing but the unguessable `state` it issued.

mod auth;
mod error;

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use axum::extract::Query;
use axum::extract::{DefaultBodyLimit, State};
use axum::middleware;
use axum::response::{Html, IntoResponse, Redirect as HttpRedirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use inseam_kernel::address::{HostId, Timestamp};
use inseam_kernel::network::NodeId;
use inseam_seams::llm::LlmLane;
use inseam_seams::oauth::{AuthorizationCallback, GrantId, Redirect};
use inseam_seams::operations::{
    AuthorizeGrantRequest, CatalogRequest, CatalogResponse, ExpandRequest, ExpandResponse,
    ExpelRequest, FetchBytesRequest, FetchBytesResponse, FetchRequest, FetchResponse, GrantView,
    HostView, IndexRequest, InstallPluginRequest, JoinRequest, NetworkView, Operations, PluginView,
    QueryRequest, QueryResponse, RevokeGrantRequest, ScanRequest, ScanResponse, Settings,
    StatusReport,
};
use inseam_seams::sweep::DeepBudget;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use auth::CookieSecurity;
use auth::{Auth, login, logout, require_owner, session};
use error::ApiError;
pub use error::ConfigError;

const BODY_BYTES_MAX: usize = 64 * 1024;
/// The one route that carries files: a plugin upload, base64 in JSON, so
/// the wire holds four thirds of the decoded bound plus the envelope
/// (asserted against `PLUGIN_UPLOAD_BYTES_MAX` in the tests). Every other
/// route keeps the small limit.
const PLUGIN_UPLOAD_BODY_BYTES_MAX: usize = 44 * 1024 * 1024;
const REQUESTS_IN_FLIGHT_MAX: usize = 64;
const REQUEST_TIMEOUT_SECS: u64 = 60;
const INDEX_ROOTS_MAX: usize = 64;
/// Where a provider sends the owner's browser back; registered with the
/// provider as `<public url>/api/v1/oauth/callback`.
pub const OAUTH_CALLBACK_PATH: &str = "/api/v1/oauth/callback";
/// Longest a provider's error description is echoed into the console URL.
const CALLBACK_MESSAGE_CHARS_MAX: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRoot {
    pub id: String,
    #[serde(skip)]
    path: PathBuf,
}

impl IndexRoot {
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let id = id.into();
        validate_root_id(&id)?;
        let path = path.into();
        let metadata = std::fs::metadata(&path).map_err(|source| ConfigError::IndexRoot {
            id: id.clone(),
            path: path.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(ConfigError::IndexRootNotDirectory { id, path });
        }
        Ok(Self { id, path })
    }
}

pub struct ServerConfig {
    pub bind: SocketAddr,
    pub owner_token: String,
    pub cookie_security: CookieSecurity,
    pub index_roots: Vec<IndexRoot>,
    pub web_dir: Option<PathBuf>,
    /// The origin the owner reaches this node at (`https://node.example`),
    /// which is what an OAuth provider must redirect back to. `None` derives
    /// it from the bind address — right for a local server, never for one
    /// behind a TLS proxy.
    pub public_url: Option<String>,
}

impl ServerConfig {
    /// The OAuth callback URL this server answers at: the public origin plus
    /// [`OAUTH_CALLBACK_PATH`].
    pub fn oauth_callback_url(&self) -> Result<String, ConfigError> {
        let origin = match &self.public_url {
            Some(url) => {
                let parsed =
                    url::Url::parse(url).map_err(|_| ConfigError::PublicUrlInvalid(url.clone()))?;
                if parsed.scheme() != "https" && parsed.scheme() != "http" {
                    return Err(ConfigError::PublicUrlInvalid(url.clone()));
                }
                if parsed.host_str().is_none() {
                    return Err(ConfigError::PublicUrlInvalid(url.clone()));
                }
                parsed.origin().ascii_serialization()
            }
            None => {
                let scheme = match self.cookie_security {
                    CookieSecurity::Secure => "https",
                    CookieSecurity::LocalHttp => "http",
                };
                format!("{scheme}://{}", self.bind)
            }
        };
        Ok(format!("{origin}{OAUTH_CALLBACK_PATH}"))
    }

    pub fn validate(self) -> Result<Self, ConfigError> {
        self.oauth_callback_url()?;
        if self.index_roots.len() > INDEX_ROOTS_MAX {
            return Err(ConfigError::TooManyIndexRoots(self.index_roots.len()));
        }
        for (index, root) in self.index_roots.iter().enumerate() {
            assert!(index < INDEX_ROOTS_MAX);
            let duplicate = self.index_roots[..index]
                .iter()
                .any(|other| other.id == root.id);
            if duplicate {
                return Err(ConfigError::DuplicateIndexRoot(root.id.clone()));
            }
        }
        if let Some(web_dir) = &self.web_dir {
            validate_web_dir(web_dir)?;
        }
        Auth::validate_token(&self.owner_token)?;
        Ok(self)
    }
}

/// The `operations` service as the transport reaches it. A composition
/// edit that restarts a provider `operations` consumes restarts
/// `operations` too, so the service the transport was handed at boot goes
/// stale after the first settings write; the distribution that applies
/// the edit puts the fresh binding here and every request takes the
/// current one. One writer per edit, many readers per request: a lock
/// held for the length of a pointer copy.
pub struct OperationsSlot {
    current: RwLock<Arc<dyn Operations>>,
}

impl OperationsSlot {
    pub fn new(operations: Arc<dyn Operations>) -> Self {
        Self {
            current: RwLock::new(operations),
        }
    }

    pub fn get(&self) -> Arc<dyn Operations> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn replace(&self, operations: Arc<dyn Operations>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = operations;
    }
}

#[derive(Clone)]
struct AppState {
    operations: Arc<OperationsSlot>,
    auth: Arc<Auth>,
    index_roots: Arc<[IndexRoot]>,
    /// Where providers redirect the owner's browser back.
    oauth_callback_url: Arc<str>,
    /// Whether the console is served here, so the callback can return the
    /// browser to it.
    serves_console: bool,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct OwnerInfo {
    version: &'static str,
    index_roots: Vec<IndexRoot>,
    /// What to register with an OAuth provider for web-console sign-in.
    oauth_callback_url: String,
}

#[derive(Debug, Deserialize)]
struct HttpGrantRequest {
    grant: GrantId,
}

/// A minted invitation as the console shows it: the text the owner
/// carries to the joining node, and beside it what that text names, so
/// the console can say who it is from and when it lapses without parsing
/// the text itself.
#[derive(Debug, Serialize)]
struct InvitationResponse {
    /// `inseam-invite:…`, exactly what `inseam network join` takes.
    invitation: String,
    node: NodeId,
    expires: Timestamp,
}

#[derive(Debug, Deserialize)]
struct HttpIndexRequest {
    #[serde(default)]
    host: Option<HostId>,
    root: String,
    #[serde(default)]
    rebuild: bool,
    #[serde(default)]
    deep_budget: Option<DeepBudget>,
    /// `batch` puts every LLM-using transform on the endpoint's batch lane
    /// for this run.
    #[serde(default)]
    llm_lane: Option<LlmLane>,
}

pub async fn serve(
    config: ServerConfig,
    operations: Arc<OperationsSlot>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ConfigError> {
    let config = config.validate()?;
    let bind = config.bind;
    let app = router(config, operations)?;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| ConfigError::Bind { bind, source })?;
    tracing::info!(%bind, "owner HTTP server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(ConfigError::Serve)
}

fn router(config: ServerConfig, operations: Arc<OperationsSlot>) -> Result<Router, ConfigError> {
    let oauth_callback_url = config.oauth_callback_url()?;
    let state = AppState {
        operations,
        auth: Arc::new(Auth::new(&config.owner_token, config.cookie_security)?),
        index_roots: config.index_roots.into(),
        oauth_callback_url: oauth_callback_url.into(),
        serves_console: config.web_dir.is_some(),
    };
    let owner = owner_router(&state);
    let api = Router::new()
        .route("/health", get(health))
        .route("/session", get(session).post(login).delete(logout))
        .route("/oauth/callback", get(oauth_callback))
        .nest("/owner", owner)
        .fallback(api_not_found)
        .with_state(state)
        .layer(middleware::from_fn(no_store));
    let app = Router::new().nest("/api/v1", api);
    let app = static_files(app, config.web_dir.as_deref());
    Ok(app
        .layer(middleware::from_fn(security_headers))
        .layer(DefaultBodyLimit::max(BODY_BYTES_MAX))
        .layer(ConcurrencyLimitLayer::new(REQUESTS_IN_FLIGHT_MAX))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ))
        .layer(TraceLayer::new_for_http()))
}

fn owner_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/info", get(info))
        .route("/status", get(status))
        .route("/catalog", post(catalog))
        .route("/hosts", get(hosts))
        .route("/query", post(query))
        .route("/expand", post(expand))
        .route("/scan", post(scan))
        .route("/fetch", post(fetch))
        .route("/fetch_bytes", post(fetch_bytes))
        .route("/raw", get(raw))
        .route("/index", post(index))
        .route("/grants", get(grants))
        .route("/grants/authorize", post(authorize_grant))
        .route("/grants/revoke", post(revoke_grant))
        .route("/plugins", get(plugins))
        .route("/settings", get(settings).put(configure))
        .route("/network", get(network))
        .route("/network/invite", post(invite))
        .route("/network/join", post(join))
        .route("/network/expel", post(expel))
        .route("/network/sync", post(sync_now))
        .route(
            "/plugins/install",
            post(install_plugin).layer(DefaultBodyLimit::max(PLUGIN_UPLOAD_BODY_BYTES_MAX)),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), require_owner))
}

fn static_files(app: Router, web_dir: Option<&Path>) -> Router {
    match web_dir {
        Some(web_dir) => app.fallback_service(
            ServeDir::new(web_dir).not_found_service(ServeFile::new(web_dir.join("index.html"))),
        ),
        None => app,
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn api_not_found() -> ApiError {
    ApiError::route_not_found()
}

async fn no_store(
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

async fn security_headers(
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // A route that set its own policy (`raw`'s sandbox) keeps it; every
    // other response gets the console's.
    headers
        .entry(axum::http::HeaderName::from_static("content-security-policy"))
        .or_insert(axum::http::HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'self'; style-src 'self'; form-action 'self'",
        ));
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("permissions-policy"),
        axum::http::HeaderValue::from_static("camera=(), geolocation=(), microphone=()"),
    );
    response
}

async fn info(State(state): State<AppState>) -> Json<OwnerInfo> {
    Json(OwnerInfo {
        version: env!("CARGO_PKG_VERSION"),
        index_roots: state.index_roots.to_vec(),
        oauth_callback_url: state.oauth_callback_url.to_string(),
    })
}

async fn grants(State(state): State<AppState>) -> Result<Json<Vec<GrantView>>, ApiError> {
    Ok(Json(state.operations.get().grants().await?))
}

/// Start an authorization whose redirect this server serves: the console
/// sends the owner to the returned URL, and the provider brings them back to
/// [`OAUTH_CALLBACK_PATH`].
async fn authorize_grant(
    State(state): State<AppState>,
    Json(request): Json<HttpGrantRequest>,
) -> Result<Json<inseam_seams::oauth::AuthorizationStarted>, ApiError> {
    let started = state
        .operations
        .get()
        .authorize_grant(AuthorizeGrantRequest {
            grant: request.grant,
            redirect: Redirect::External {
                redirect_uri: state.oauth_callback_url.to_string(),
            },
        })
        .await?;
    Ok(Json(started))
}

async fn revoke_grant(
    State(state): State<AppState>,
    Json(request): Json<HttpGrantRequest>,
) -> Result<Json<GrantView>, ApiError> {
    Ok(Json(
        state
            .operations
            .get()
            .revoke_grant(RevokeGrantRequest {
                grant: request.grant,
            })
            .await?,
    ))
}

/// The provider's redirect. Unauthenticated by necessity — the session
/// cookie is `SameSite=Strict` and a cross-site redirect never carries it —
/// and safe because the operation accepts nothing but a `state` it issued.
/// The owner is looking at this tab, so it answers with the console when
/// one is served here, or a plain page otherwise.
async fn oauth_callback(
    State(state): State<AppState>,
    Query(callback): Query<AuthorizationCallback>,
) -> Response {
    let outcome = state
        .operations
        .get()
        .complete_authorization(callback)
        .await;
    let (grant, message) = match &outcome {
        Ok(view) => (Some(view.id.to_string()), None),
        Err(error) => (None, Some(error.to_string())),
    };
    if state.serves_console {
        let mut console = url::Url::parse("http://console.invalid/").expect("literal URL is valid");
        {
            let mut query = console.query_pairs_mut();
            match (&grant, &message) {
                (Some(grant), None) => {
                    query.append_pair("authorized", grant);
                }
                (_, Some(message)) => {
                    let short: String = message.chars().take(CALLBACK_MESSAGE_CHARS_MAX).collect();
                    query.append_pair("authorization_error", &short);
                }
                (None, None) => {}
            }
        }
        let target = match console.query() {
            Some(query) => format!("/?{query}"),
            None => "/".to_string(),
        };
        return HttpRedirect::to(&target).into_response();
    }
    match (grant, message) {
        (Some(grant), _) => Html(callback_page(
            "Authorized",
            &format!("inseam received the grant `{grant}`. You can close this tab."),
        ))
        .into_response(),
        (None, message) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Html(callback_page(
                "Authorization failed",
                &message.unwrap_or_else(|| "no outcome".to_string()),
            )),
        )
            .into_response(),
    }
}

/// A minimal page for the tab the provider sent back, with the message
/// HTML-escaped — it may quote the provider.
fn callback_page(title: &str, message: &str) -> String {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui;margin:3rem\"><h1>{title}</h1><p>{escaped}</p></body></html>"
    )
}

async fn plugins(State(state): State<AppState>) -> Result<Json<Vec<PluginView>>, ApiError> {
    Ok(Json(state.operations.get().plugins().await?))
}

/// Install a loaded plugin into the running node. The request is the
/// operation message verbatim — the plugin's files, base64 in JSON — so the
/// transport adds nothing but its larger body limit.
async fn install_plugin(
    State(state): State<AppState>,
    Json(request): Json<InstallPluginRequest>,
) -> Result<Json<PluginView>, ApiError> {
    Ok(Json(state.operations.get().install_plugin(request).await?))
}

/// The first-party settings document as the node runs it.
async fn settings(State(state): State<AppState>) -> Result<Json<Settings>, ApiError> {
    Ok(Json(state.operations.get().settings().await?))
}

/// Replace the first-party settings: the complete document, applied to the
/// running node; the reply is the document afterwards.
async fn configure(
    State(state): State<AppState>,
    Json(settings): Json<Settings>,
) -> Result<Json<Settings>, ApiError> {
    Ok(Json(state.operations.get().configure(settings).await?))
}

async fn status(State(state): State<AppState>) -> Result<Json<StatusReport>, ApiError> {
    Ok(Json(state.operations.get().status().await?))
}

/// The network as this node sees it (`design/roster.md`).
async fn network(State(state): State<AppState>) -> Result<Json<NetworkView>, ApiError> {
    Ok(Json(state.operations.get().network().await?))
}

/// Mint an invitation; the reply carries its text form for the owner to
/// copy to the joining node.
async fn invite(State(state): State<AppState>) -> Result<Json<InvitationResponse>, ApiError> {
    let invitation = state.operations.get().invite().await?;
    Ok(Json(InvitationResponse {
        invitation: invitation.to_string(),
        node: invitation.node,
        expires: invitation.expires,
    }))
}

/// Join through an invitation's text; the reply is the network afterwards.
async fn join(
    State(state): State<AppState>,
    Json(request): Json<JoinRequest>,
) -> Result<Json<NetworkView>, ApiError> {
    Ok(Json(state.operations.get().join(request).await?))
}

async fn expel(
    State(state): State<AppState>,
    Json(request): Json<ExpelRequest>,
) -> Result<Json<NetworkView>, ApiError> {
    Ok(Json(state.operations.get().expel(request).await?))
}

/// One sync round with every dialable node now.
async fn sync_now(State(state): State<AppState>) -> Result<Json<NetworkView>, ApiError> {
    Ok(Json(state.operations.get().sync_now().await?))
}

async fn catalog(
    State(state): State<AppState>,
    Json(request): Json<CatalogRequest>,
) -> Result<Json<CatalogResponse>, ApiError> {
    Ok(Json(state.operations.get().catalog(request).await?))
}

async fn hosts(State(state): State<AppState>) -> Result<Json<Vec<HostView>>, ApiError> {
    Ok(Json(state.operations.get().hosts().await?))
}

async fn query(
    State(state): State<AppState>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    Ok(Json(state.operations.get().query(request).await?))
}

async fn expand(
    State(state): State<AppState>,
    Json(request): Json<ExpandRequest>,
) -> Result<Json<ExpandResponse>, ApiError> {
    Ok(Json(state.operations.get().expand(request).await?))
}

async fn scan(
    State(state): State<AppState>,
    Json(request): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    Ok(Json(state.operations.get().scan(request).await?))
}

async fn fetch(
    State(state): State<AppState>,
    Json(request): Json<FetchRequest>,
) -> Result<Json<FetchResponse>, ApiError> {
    Ok(Json(state.operations.get().fetch(request).await?))
}

async fn fetch_bytes(
    State(state): State<AppState>,
    Json(request): Json<FetchBytesRequest>,
) -> Result<Json<FetchBytesResponse>, ApiError> {
    Ok(Json(state.operations.get().fetch_bytes(request).await?))
}

/// Content types a browser may render inline from the owner origin: raster
/// images, which carry no script. Everything else is a download — an
/// indexed HTML or SVG file rendered inline would run on the owner origin
/// with the session cookie, and the index holds whatever the hosts do.
const INLINE_CONTENT_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The one route that answers with a body that is not JSON: the bytes of a
/// source or referenced fragment under their own content type, so an
/// `<img>` in the console or any HTTP client reads an indexed image as a
/// plain URL. `fetch_bytes` unwrapped — the operation decides what may be
/// served and how much; the route decides only how a browser may treat it:
/// every response is CSP-sandboxed (no script, opaque origin), and only
/// [`INLINE_CONTENT_TYPES`] are `inline`, the rest `attachment`.
async fn raw(
    State(state): State<AppState>,
    Query(request): Query<FetchBytesRequest>,
) -> Result<Response, ApiError> {
    let response = state.operations.get().fetch_bytes(request).await?;
    Ok((raw_headers(&response.content_type), response.bytes.0).into_response())
}

fn raw_headers(content_type: &str) -> [(axum::http::HeaderName, axum::http::HeaderValue); 3] {
    // Mimetypes are lowercase `type/subtype;k=v` from the store boundary;
    // the essence is what the allow list names.
    let essence = content_type.split(';').next().unwrap_or("").trim();
    let disposition = if INLINE_CONTENT_TYPES.contains(&essence) {
        "inline"
    } else {
        "attachment"
    };
    // The fallback covers only a parameter value with header-hostile bytes;
    // the body is served either way, as a download.
    let content_type = axum::http::HeaderValue::from_str(content_type)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    [
        (axum::http::header::CONTENT_TYPE, content_type),
        (
            axum::http::header::CONTENT_DISPOSITION,
            axum::http::HeaderValue::from_static(disposition),
        ),
        (
            axum::http::HeaderName::from_static("content-security-policy"),
            axum::http::HeaderValue::from_static("sandbox"),
        ),
    ]
}

/// Reconcile the index over one scope. `root` is an approved index root's
/// id (`--index-root`), or a folder the owner configured on a host —
/// matched verbatim against what the node reports for that host — so the
/// route never accepts a path of its own.
async fn index(
    State(state): State<AppState>,
    Json(request): Json<HttpIndexRequest>,
) -> Result<Json<inseam_seams::sweep::IndexReport>, ApiError> {
    let operations = state.operations.get();
    let approved = state
        .index_roots
        .iter()
        .find(|root| root.id == request.root)
        .map(|root| root.path.to_string_lossy().into_owned());
    let root = match approved {
        Some(path) => path,
        None => configured_root(operations.as_ref(), request.host.as_ref(), &request.root).await?,
    };
    let response = operations
        .index(IndexRequest {
            host: request.host,
            root,
            rebuild: request.rebuild,
            deep_budget: request.deep_budget,
            llm_lane: request.llm_lane,
        })
        .await?;
    Ok(Json(response))
}

/// `root` exactly as some host reports it among its configured roots —
/// the named host's when one is named, any host's otherwise.
async fn configured_root(
    operations: &dyn Operations,
    host: Option<&HostId>,
    root: &str,
) -> Result<String, ApiError> {
    let hosts = operations.hosts().await?;
    let known = hosts
        .iter()
        .filter(|view| host.is_none_or(|named| *named == view.id))
        .flat_map(|view| view.roots.iter())
        .any(|configured| configured == root);
    if known {
        Ok(root.to_string())
    } else {
        Err(ApiError::unknown_index_root(root))
    }
}

fn validate_root_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty() {
        return Err(ConfigError::IndexRootIdEmpty);
    }
    if id.len() > 64 {
        return Err(ConfigError::IndexRootIdTooLong(id.to_string()));
    }
    let valid = id.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    });
    if !valid {
        return Err(ConfigError::IndexRootIdInvalid(id.to_string()));
    }
    Ok(())
}

fn validate_web_dir(web_dir: &Path) -> Result<(), ConfigError> {
    let index = web_dir.join("index.html");
    if !index.is_file() {
        return Err(ConfigError::WebIndexMissing(index));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use axum::body::{Body, to_bytes};
    use axum::http::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
    use axum::http::{Request, StatusCode};
    use inseam_kernel::network::{HostRecord, NodeCapabilities, NodeRecord};
    use inseam_seams::SeamError;
    use inseam_seams::oauth::{AuthorizationStarted, GrantState};
    use inseam_seams::operations::{
        AwaitAuthorizationRequest, ExpandResponse, FetchResponse, IndexRequest, LogSummary,
        NetworkHostView, NetworkNodeView, PLUGIN_UPLOAD_BYTES_MAX, PluginState, QueryMeta,
        QueryResponse, RepairOutcome, RepairReport, RepairRequest, ScanResponse,
    };
    use inseam_seams::roster::Invitation;
    use inseam_seams::sweep::IndexReport;
    use inseam_seams::transport::InvitationToken;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    /// A PNG signature followed by padding: enough to be recognizably not
    /// JSON on the wire.
    const PNG_BYTES: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0];

    #[derive(Default)]
    struct StubOperations {
        configured: Mutex<Option<Settings>>,
        indexed: Mutex<Option<IndexRequest>>,
        authorized: Mutex<Option<AuthorizeGrantRequest>>,
        completed: Mutex<Option<AuthorizationCallback>>,
        installed: Mutex<Option<InstallPluginRequest>>,
        joined: Mutex<Option<JoinRequest>>,
        expelled: Mutex<Option<ExpelRequest>>,
        sync_rounds: std::sync::atomic::AtomicU32,
    }

    fn node_record(byte: u8, name: &str) -> NodeRecord {
        NodeRecord {
            id: NodeId::from_bytes([byte; 32]),
            display_name: name.to_string(),
            endpoints: Vec::new(),
            capabilities: NodeCapabilities {
                always_on: false,
                deep_index: true,
                relays: true,
            },
        }
    }

    /// Two nodes, one host stewarded by the peer: enough shape for a
    /// transport test to see every field cross the wire.
    fn network_view() -> NetworkView {
        let host = HostId::new("fs-peer").expect("valid");
        NetworkView {
            local: node_record(1, "mini"),
            nodes: vec![
                NetworkNodeView {
                    record: node_record(1, "mini"),
                    is_local: true,
                    live: true,
                    last_sync: None,
                    last_error: None,
                    hosts: Vec::new(),
                },
                NetworkNodeView {
                    record: node_record(2, "laptop"),
                    is_local: false,
                    live: false,
                    last_sync: Some("2026-09-07".to_string()),
                    last_error: Some("dial failed".to_string()),
                    hosts: vec![host.clone()],
                },
            ],
            hosts: vec![NetworkHostView {
                host: HostRecord {
                    id: host,
                    kind: "fs".to_string(),
                    display_name: "Laptop disk".to_string(),
                },
                stewards: vec![NodeId::from_bytes([2; 32])],
            }],
            log: LogSummary {
                entries: 9,
                origins: 2,
            },
        }
    }

    fn grant_view(state: GrantState) -> GrantView {
        GrantView {
            id: GrantId::new("google").expect("valid"),
            provider: "accounts.google.com".to_string(),
            scopes: vec!["openid".to_string()],
            client_id_env: "GOOGLE_CLIENT_ID".to_string(),
            client_secret_env: None,
            state,
        }
    }

    #[async_trait]
    impl Operations for StubOperations {
        async fn query(&self, _request: QueryRequest) -> Result<QueryResponse, SeamError> {
            Ok(QueryResponse {
                results: Vec::new(),
                meta: QueryMeta::default(),
            })
        }

        async fn expand(&self, _request: ExpandRequest) -> Result<ExpandResponse, SeamError> {
            Err(unused())
        }

        async fn scan(&self, _request: ScanRequest) -> Result<ScanResponse, SeamError> {
            Err(unused())
        }

        async fn fetch(&self, _request: FetchRequest) -> Result<FetchResponse, SeamError> {
            Err(unused())
        }

        async fn fetch_bytes(
            &self,
            request: FetchBytesRequest,
        ) -> Result<FetchBytesResponse, SeamError> {
            let (content_type, bytes) = match request.address.locator.as_str() {
                "tmp/logo.png" => ("image/png", PNG_BYTES.to_vec()),
                "tmp/diagram.svg" => ("image/svg+xml", b"<svg onload=\"alert(1)\"/>".to_vec()),
                _ => return Err(SeamError::UnknownSource(request.address)),
            };
            Ok(FetchBytesResponse {
                address: request.address,
                content_type: content_type.to_string(),
                bytes: inseam_seams::operations::FileBytes(bytes),
            })
        }

        async fn index(&self, request: IndexRequest) -> Result<IndexReport, SeamError> {
            *self
                .indexed
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(IndexReport::default())
        }

        async fn hosts(&self) -> Result<Vec<HostView>, SeamError> {
            Ok(vec![HostView {
                id: HostId::new("fs-test").expect("valid"),
                kind: inseam_seams::connection::HostKind::filesystem(),
                display_name: "test".to_string(),
                entry: "fs".to_string(),
                capabilities: inseam_seams::connection::Capabilities::READ_ONLY,
                roots: vec!["/srv/notes".to_string()],
            }])
        }

        async fn catalog(&self, _request: CatalogRequest) -> Result<CatalogResponse, SeamError> {
            Ok(CatalogResponse {
                sources: 3,
                indexed: 2,
                pending: 1,
                entries: Vec::new(),
            })
        }

        async fn status(&self) -> Result<StatusReport, SeamError> {
            Ok(StatusReport {
                sources: 3,
                indexed_sources: 2,
                fragments: 7,
                relations: 1,
                keyed_fragments: 0,
                search_rows: 7,
                vector_index_ready: false,
                store_bytes: 4096,
                content_bytes: 120,
                embedding_model: None,
                embedding_dimensions: 0,
                embedding_vectors: inseam_kernel::store::VectorScope::All,
                reembed_pending: false,
                cached_embeddings: 0,
                cached_transform_outputs: 0,
                remote_sources: 0,
            })
        }

        async fn repair(&self, _request: RepairRequest) -> Result<RepairReport, SeamError> {
            Ok(RepairReport {
                search_rows: 7,
                vectors_converted: 0,
                outcome: RepairOutcome::AlreadyReady,
                vector_index_ready: true,
            })
        }

        async fn grants(&self) -> Result<Vec<GrantView>, SeamError> {
            Ok(vec![grant_view(GrantState::Unauthorized)])
        }

        async fn authorize_grant(
            &self,
            request: AuthorizeGrantRequest,
        ) -> Result<AuthorizationStarted, SeamError> {
            let redirect_uri = match &request.redirect {
                Redirect::External { redirect_uri } => redirect_uri.clone(),
                Redirect::Loopback => "http://127.0.0.1:1/callback".to_string(),
            };
            let grant = request.grant.clone();
            *self
                .authorized
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(AuthorizationStarted {
                grant,
                url: format!("https://accounts.example/auth?redirect_uri={redirect_uri}&state=st"),
                state: "st".to_string(),
                redirect_uri,
            })
        }

        async fn await_authorization(
            &self,
            _request: AwaitAuthorizationRequest,
        ) -> Result<GrantView, SeamError> {
            Err(unused())
        }

        async fn complete_authorization(
            &self,
            callback: AuthorizationCallback,
        ) -> Result<GrantView, SeamError> {
            let ok = callback.state.as_deref() == Some("st") && callback.code.is_some();
            *self
                .completed
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(callback);
            if ok {
                Ok(grant_view(GrantState::Authorized {
                    expires_at: None,
                    scopes: Vec::new(),
                    account: Some("greg@example.com".into()),
                }))
            } else {
                Err(SeamError::Refused("wrong state".to_string()))
            }
        }

        async fn revoke_grant(&self, _request: RevokeGrantRequest) -> Result<GrantView, SeamError> {
            Ok(grant_view(GrantState::Unauthorized))
        }

        async fn plugins(&self) -> Result<Vec<PluginView>, SeamError> {
            Ok(vec![PluginView {
                id: "finder".to_string(),
                plugin: "finder".to_string(),
                state: PluginState::Active,
                effects: vec!["provide finder".to_string()],
                missing: Vec::new(),
                missing_secrets: Vec::new(),
            }])
        }

        async fn install_plugin(
            &self,
            request: InstallPluginRequest,
        ) -> Result<PluginView, SeamError> {
            let id = request.id.to_string();
            *self
                .installed
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(PluginView {
                id,
                plugin: "wasm:/data/plugins/demo/demo.wasm".to_string(),
                state: PluginState::Active,
                effects: Vec::new(),
                missing: Vec::new(),
                missing_secrets: Vec::new(),
            })
        }

        async fn settings(&self) -> Result<Settings, SeamError> {
            Ok(Settings(serde_json::json!({
                "sweep": { "enabled": true, "config": { "max_depth": 6 } }
            })))
        }

        async fn configure(&self, settings: Settings) -> Result<Settings, SeamError> {
            *self
                .configured
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(settings.clone());
            Ok(settings)
        }

        async fn network(&self) -> Result<NetworkView, SeamError> {
            Ok(network_view())
        }

        async fn invite(&self) -> Result<Invitation, SeamError> {
            Ok(Invitation {
                node: NodeId::from_bytes([1; 32]),
                endpoints: Vec::new(),
                token: InvitationToken::new("one-time").expect("valid"),
                expires: Timestamp(1_800_000_000),
            })
        }

        async fn join(&self, request: JoinRequest) -> Result<NetworkView, SeamError> {
            if !request.invitation.starts_with("inseam-invite:") {
                return Err(SeamError::Refused("invitation: not one".to_string()));
            }
            *self
                .joined
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(network_view())
        }

        async fn expel(&self, request: ExpelRequest) -> Result<NetworkView, SeamError> {
            *self
                .expelled
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(network_view())
        }

        async fn sync_now(&self) -> Result<NetworkView, SeamError> {
            self.sync_rounds
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(network_view())
        }
    }

    fn unused() -> SeamError {
        SeamError::Unavailable("unused by this transport test".to_string())
    }

    fn test_config(index_roots: Vec<IndexRoot>, web_dir: Option<PathBuf>) -> ServerConfig {
        ServerConfig {
            bind: "127.0.0.1:7337".parse().expect("valid bind"),
            owner_token: TOKEN.to_string(),
            cookie_security: CookieSecurity::LocalHttp,
            index_roots,
            web_dir,
            public_url: None,
        }
    }

    fn test_router(operations: Arc<StubOperations>, index_roots: Vec<IndexRoot>) -> Router {
        router(test_config(index_roots, None), slot(operations)).expect("valid router")
    }

    fn slot(operations: Arc<StubOperations>) -> Arc<OperationsSlot> {
        Arc::new(OperationsSlot::new(operations))
    }

    #[tokio::test]
    async fn settings_are_read_and_replaced_through_the_document_routes() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        let request = Request::get("/api/v1/owner/settings")
            .header(COOKIE, cookie.clone())
            .body(Body::empty())
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(body["sweep"]["config"]["max_depth"], 6);

        let request = Request::put("/api/v1/owner/settings")
            .header(CONTENT_TYPE, "application/json")
            .header(COOKIE, cookie)
            .body(Body::from(
                r#"{"sweep":{"enabled":false,"config":{"max_depth":2}}}"#,
            ))
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let configured = operations
            .configured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let configured = configured.expect("the operation saw the document");
        assert_eq!(configured.0["sweep"]["enabled"], false);
    }

    #[tokio::test]
    async fn a_replaced_operations_service_answers_the_next_request() {
        let first = Arc::new(StubOperations::default());
        let slot = slot(Arc::clone(&first));
        let app = router(test_config(Vec::new(), None), Arc::clone(&slot)).expect("valid router");
        let cookie = login_cookie(&app).await;
        let second = Arc::new(StubOperations::default());
        slot.replace(Arc::clone(&second) as Arc<dyn Operations>);
        let request = Request::put("/api/v1/owner/settings")
            .header(CONTENT_TYPE, "application/json")
            .header(COOKIE, cookie)
            .body(Body::from(r#"{"sweep":{"enabled":true}}"#))
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            first
                .configured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
        );
        assert!(
            second
                .configured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        );
    }

    async fn login_cookie(app: &Router) -> String {
        let request = Request::post("/api/v1/session")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"token":"{TOKEN}"}}"#)))
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        response
            .headers()
            .get(SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .expect("session cookie")
            .to_string()
    }

    #[tokio::test]
    async fn owner_routes_reject_an_anonymous_request() {
        let app = test_router(Arc::new(StubOperations::default()), Vec::new());
        let request = Request::get("/api/v1/owner/status")
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_raw_route_serves_bytes_under_their_own_content_type() {
        let app = test_router(Arc::new(StubOperations::default()), Vec::new());
        let cookie = login_cookie(&app).await;
        let request =
            Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Flogo.png")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "image/png");
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_DISPOSITION],
            "inline"
        );
        assert_eq!(response.headers()["content-security-policy"], "sandbox");
        assert_eq!(
            response.headers()[axum::http::header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        assert_eq!(bytes.as_ref(), PNG_BYTES);

        // Anything that can carry script is a sandboxed download, never
        // rendered on the owner origin.
        let request =
            Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Fdiagram.svg")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "image/svg+xml");
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_DISPOSITION],
            "attachment"
        );
        assert_eq!(response.headers()["content-security-policy"], "sandbox");

        // The JSON form of the same operation carries the bytes as base64.
        let request = Request::post("/api/v1/owner/fetch_bytes")
            .header(COOKIE, &cookie)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"address":"inseam://fs-test/tmp/logo.png"}"#))
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(body["content_type"], "image/png");
        assert_eq!(body["bytes"], "iVBORw0KGgoAAA==");

        // An address the operation refuses is a JSON error, not a body.
        let request =
            Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Fnope.png")
                .header(COOKIE, &cookie)
                .body(Body::empty())
                .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // And no cookie, no bytes.
        let request =
            Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Flogo.png")
                .body(Body::empty())
                .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_api_routes_do_not_fall_back_to_the_web_client() {
        let web_dir = tempfile::tempdir().expect("temporary web directory");
        std::fs::write(web_dir.path().join("index.html"), "web client").expect("web index");
        let app = router(
            test_config(Vec::new(), Some(web_dir.path().to_path_buf())),
            slot(Arc::new(StubOperations::default())),
        )
        .expect("valid router");
        let request = Request::get("/api/v1/not-a-route")
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[axum::http::header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .expect("content type")
            .to_str()
            .expect("valid content type");
        assert_eq!(content_type, "application/json");
    }

    #[tokio::test]
    async fn login_cookie_authenticates_an_owner_request() {
        let app = test_router(Arc::new(StubOperations::default()), Vec::new());
        let cookie = login_cookie(&app).await;
        let request = Request::get("/api/v1/owner/status")
            .header(COOKIE, cookie)
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[axum::http::header::CACHE_CONTROL],
            "no-store"
        );
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(body["sources"], 3);
    }

    #[tokio::test]
    async fn index_resolves_only_a_configured_root_id() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = IndexRoot::new("documents", directory.path()).expect("valid root");
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), vec![root]);
        let cookie = login_cookie(&app).await;
        let response = index_request(&app, &cookie, "documents").await;
        assert_eq!(response, StatusCode::OK);
        {
            let indexed = operations
                .indexed
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert_eq!(
                indexed.as_ref().expect("indexed").root,
                directory.path().to_string_lossy()
            );
        }
        assert_eq!(
            index_request(&app, &cookie, "/etc").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn the_callback_url_follows_the_public_url_or_the_bind_address() {
        let local = test_config(Vec::new(), None);
        assert_eq!(
            local.oauth_callback_url().expect("derives"),
            "http://127.0.0.1:7337/api/v1/oauth/callback"
        );
        let mut secure = test_config(Vec::new(), None);
        secure.cookie_security = CookieSecurity::Secure;
        assert_eq!(
            secure.oauth_callback_url().expect("derives"),
            "https://127.0.0.1:7337/api/v1/oauth/callback"
        );
        let mut public = test_config(Vec::new(), None);
        public.public_url = Some("https://node.example/some/path".to_string());
        assert_eq!(
            public.oauth_callback_url().expect("derives"),
            "https://node.example/api/v1/oauth/callback"
        );
        public.public_url = Some("node.example".to_string());
        assert!(matches!(
            public.oauth_callback_url(),
            Err(ConfigError::PublicUrlInvalid(_))
        ));
    }

    #[tokio::test]
    async fn authorize_route_asks_for_the_server_served_redirect() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        let request = Request::post("/api/v1/owner/grants/authorize")
            .header(CONTENT_TYPE, "application/json")
            .header(COOKIE, cookie.clone())
            .body(Body::from(r#"{"grant":"google"}"#))
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(
            body["redirect_uri"],
            "http://127.0.0.1:7337/api/v1/oauth/callback"
        );
        assert_eq!(body["state"], "st");
        let recorded = operations
            .authorized
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            recorded.expect("recorded").redirect,
            Redirect::External {
                redirect_uri: "http://127.0.0.1:7337/api/v1/oauth/callback".to_string()
            }
        );

        let request = Request::get("/api/v1/owner/grants")
            .header(COOKIE, cookie)
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(body[0]["id"], "google");
        assert_eq!(body[0]["state"]["state"], "unauthorized");
    }

    #[tokio::test]
    async fn the_callback_is_unauthenticated_and_returns_the_browser_to_the_console() {
        let web_dir = tempfile::tempdir().expect("temporary web directory");
        std::fs::write(web_dir.path().join("index.html"), "web client").expect("web index");
        let operations = Arc::new(StubOperations::default());
        let app = router(
            test_config(Vec::new(), Some(web_dir.path().to_path_buf())),
            slot(Arc::clone(&operations)),
        )
        .expect("valid router");
        let request = Request::get("/api/v1/oauth/callback?code=abc&state=st")
            .body(Body::empty())
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/?authorized=google");
        let completed = operations
            .completed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(completed.expect("delivered").code.as_deref(), Some("abc"));

        let request = Request::get("/api/v1/oauth/callback?error=access_denied&state=nope")
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(
            response.headers()["location"]
                .to_str()
                .expect("ascii")
                .starts_with("/?authorization_error=")
        );

        // Without a console to return to, the tab gets a page.
        let bare = test_router(Arc::new(StubOperations::default()), Vec::new());
        let request = Request::get("/api/v1/oauth/callback?code=abc&state=st")
            .body(Body::empty())
            .expect("valid request");
        let response = bare.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        assert!(String::from_utf8_lossy(&bytes).contains("Authorized"));
    }

    async fn index_request(app: &Router, cookie: &str, root: &str) -> StatusCode {
        let request = Request::post("/api/v1/owner/index")
            .header(CONTENT_TYPE, "application/json")
            .header(COOKIE, cookie)
            .body(Body::from(format!(r#"{{"root":"{root}"}}"#)))
            .expect("valid request");
        app.clone()
            .oneshot(request)
            .await
            .expect("response")
            .status()
    }

    #[tokio::test]
    async fn a_folder_configured_on_a_host_is_an_accepted_index_root() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        assert_eq!(
            index_request(&app, &cookie, "/srv/notes").await,
            StatusCode::OK
        );
        let indexed = operations
            .indexed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(indexed.expect("the operation ran").root, "/srv/notes");
        // A path the node never reported is still refused: the route
        // matches verbatim and invents nothing.
        assert_eq!(
            index_request(&app, &cookie, "/srv").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            index_request(&app, &cookie, "/srv/notes/sub").await,
            StatusCode::BAD_REQUEST
        );
    }

    /// One owner request with an optional JSON body, answered as status
    /// and parsed body; every network route is exercised through it.
    async fn owner_json(
        app: &Router,
        cookie: Option<&str>,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(cookie) = cookie {
            builder = builder.header(COOKIE, cookie);
        }
        let request = match body {
            Some(json) => builder
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn the_network_route_serves_the_owner_view() {
        let app = test_router(Arc::new(StubOperations::default()), Vec::new());
        let cookie = login_cookie(&app).await;
        let (status, body) =
            owner_json(&app, Some(&cookie), "GET", "/api/v1/owner/network", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["local"]["display_name"], "mini");
        assert_eq!(body["nodes"][1]["last_error"], "dial failed");
        assert_eq!(body["nodes"][1]["hosts"][0], "fs-peer");
        assert_eq!(body["hosts"][0]["stewards"][0], "02".repeat(32));
        assert_eq!(body["log"]["entries"], 9);
        let (status, _) = owner_json(&app, None, "GET", "/api/v1/owner/network", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_invite_route_answers_with_the_text_the_owner_carries() {
        let app = test_router(Arc::new(StubOperations::default()), Vec::new());
        let cookie = login_cookie(&app).await;
        let (status, body) = owner_json(
            &app,
            Some(&cookie),
            "POST",
            "/api/v1/owner/network/invite",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = body["invitation"].as_str().expect("the text form");
        let parsed: Invitation = text.parse().expect("the text parses back");
        assert_eq!(parsed.node, NodeId::from_bytes([1; 32]));
        assert_eq!(body["node"], "01".repeat(32));
        assert_eq!(body["expires"], 1_800_000_000);
        assert!(
            body.get("token").is_none(),
            "the token travels only inside the text"
        );
        let (status, _) =
            owner_json(&app, None, "POST", "/api/v1/owner/network/invite", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_join_route_hands_the_invitation_to_the_operation() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        let request = r#"{"invitation":"inseam-invite:abc"}"#;
        let (status, body) = owner_json(
            &app,
            Some(&cookie),
            "POST",
            "/api/v1/owner/network/join",
            Some(request),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["nodes"].as_array().map(Vec::len), Some(2));
        let joined = operations
            .joined
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            joined.expect("the operation ran").invitation,
            "inseam-invite:abc"
        );
        // The operation's refusal is the owner's answer, not a 500.
        let (status, body) = owner_json(
            &app,
            Some(&cookie),
            "POST",
            "/api/v1/owner/network/join",
            Some(r#"{"invitation":"nope"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "refused");
        let (status, _) = owner_json(
            &app,
            None,
            "POST",
            "/api/v1/owner/network/join",
            Some(request),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_expel_route_names_the_node_to_the_operation() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        let request = format!(r#"{{"node":"{}"}}"#, "02".repeat(32));
        let (status, body) = owner_json(
            &app,
            Some(&cookie),
            "POST",
            "/api/v1/owner/network/expel",
            Some(&request),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["local"]["id"], "01".repeat(32));
        let expelled = operations
            .expelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            expelled.expect("the operation ran").node,
            NodeId::from_bytes([2; 32])
        );
        // A malformed id never reaches the operation.
        let (status, _) = owner_json(
            &app,
            Some(&cookie),
            "POST",
            "/api/v1/owner/network/expel",
            Some(r#"{"node":"short"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, _) = owner_json(
            &app,
            None,
            "POST",
            "/api/v1/owner/network/expel",
            Some(&request),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_sync_route_runs_one_round_and_answers_with_the_view() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        let (status, body) = owner_json(
            &app,
            Some(&cookie),
            "POST",
            "/api/v1/owner/network/sync",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["log"]["origins"], 2);
        assert_eq!(
            operations
                .sync_rounds
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let (status, _) = owner_json(&app, None, "POST", "/api/v1/owner/network/sync", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            operations
                .sync_rounds
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn the_upload_body_limit_covers_the_decoded_bound_in_base64() {
        let decoded = u64::try_from(PLUGIN_UPLOAD_BODY_BYTES_MAX).expect("fits") / 4 * 3;
        assert!(decoded >= PLUGIN_UPLOAD_BYTES_MAX + 64 * 1024 / 4 * 3);
        assert!(decoded < PLUGIN_UPLOAD_BYTES_MAX * 2);
    }

    #[tokio::test]
    async fn plugin_uploads_get_their_own_body_limit() {
        let operations = Arc::new(StubOperations::default());
        let app = test_router(Arc::clone(&operations), Vec::new());
        let cookie = login_cookie(&app).await;
        // Well past the 64 KiB every other route allows.
        let bytes = "A".repeat(300 * 1024);
        let body = format!(
            r#"{{"id":"demo","files":[{{"path":"demo.wasm","bytes":"{bytes}"}},{{"path":"demo.manifest.toml","bytes":"bmFtZQ=="}}]}}"#
        );
        let request = Request::post("/api/v1/owner/plugins/install")
            .header(CONTENT_TYPE, "application/json")
            .header(COOKIE, cookie.clone())
            .body(Body::from(body))
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let installed = operations
            .installed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let installed = installed.expect("the operation saw the upload");
        assert_eq!(installed.id.as_str(), "demo");
        assert_eq!(installed.files.len(), 2);
        assert_eq!(installed.files[0].bytes.0.len(), 300 * 1024 / 4 * 3);

        // The same body on an ordinary route is still too large.
        let request = Request::post("/api/v1/owner/query")
            .header(CONTENT_TYPE, "application/json")
            .header(COOKIE, cookie.clone())
            .body(Body::from(format!(r#"{{"text":"{bytes}"}}"#)))
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let request = Request::get("/api/v1/owner/plugins")
            .header(COOKIE, cookie)
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(body[0]["id"], "finder");
        assert_eq!(body[0]["state"]["state"], "active");
    }
}
