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
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, State};
use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum::extract::Query;
use axum::response::{Html, IntoResponse, Redirect as HttpRedirect, Response};
use inseam_kernel::address::HostId;
use inseam_seams::oauth::{AuthorizationCallback, GrantId, Redirect};
use inseam_seams::operations::{
    AuthorizeGrantRequest, CatalogRequest, CatalogResponse, ExpandRequest, ExpandResponse,
    FetchBytesRequest, FetchBytesResponse, FetchRequest, FetchResponse, GrantView, HostView,
    IndexRequest, InstallPluginRequest,
    Operations, PluginView, QueryRequest, QueryResponse, RevokeGrantRequest, ScanRequest,
    ScanResponse, StatusReport,
};
use inseam_seams::llm::LlmLane;
use inseam_seams::sweep::DeepBudget;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use auth::CookieSecurity;
pub use error::ConfigError;
use auth::{Auth, login, logout, require_owner, session};
use error::ApiError;

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
                let parsed = url::Url::parse(url)
                    .map_err(|_| ConfigError::PublicUrlInvalid(url.clone()))?;
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

#[derive(Clone)]
struct AppState {
    operations: Arc<dyn Operations>,
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
    operations: Arc<dyn Operations>,
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

fn router(config: ServerConfig, operations: Arc<dyn Operations>) -> Result<Router, ConfigError> {
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
    headers.insert(
        axum::http::HeaderName::from_static("content-security-policy"),
        axum::http::HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; img-src 'self' data:; object-src 'none'; script-src 'self'; style-src 'self'; form-action 'self'",
        ),
    );
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
    Ok(Json(state.operations.grants().await?))
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
    let outcome = state.operations.complete_authorization(callback).await;
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
    Ok(Json(state.operations.plugins().await?))
}

/// Install a loaded plugin into the running node. The request is the
/// operation message verbatim — the plugin's files, base64 in JSON — so the
/// transport adds nothing but its larger body limit.
async fn install_plugin(
    State(state): State<AppState>,
    Json(request): Json<InstallPluginRequest>,
) -> Result<Json<PluginView>, ApiError> {
    Ok(Json(state.operations.install_plugin(request).await?))
}

async fn status(State(state): State<AppState>) -> Result<Json<StatusReport>, ApiError> {
    Ok(Json(state.operations.status().await?))
}

async fn catalog(
    State(state): State<AppState>,
    Json(request): Json<CatalogRequest>,
) -> Result<Json<CatalogResponse>, ApiError> {
    Ok(Json(state.operations.catalog(request).await?))
}

async fn hosts(State(state): State<AppState>) -> Result<Json<Vec<HostView>>, ApiError> {
    Ok(Json(state.operations.hosts().await?))
}

async fn query(
    State(state): State<AppState>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    Ok(Json(state.operations.query(request).await?))
}

async fn expand(
    State(state): State<AppState>,
    Json(request): Json<ExpandRequest>,
) -> Result<Json<ExpandResponse>, ApiError> {
    Ok(Json(state.operations.expand(request).await?))
}

async fn scan(
    State(state): State<AppState>,
    Json(request): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, ApiError> {
    Ok(Json(state.operations.scan(request).await?))
}

async fn fetch(
    State(state): State<AppState>,
    Json(request): Json<FetchRequest>,
) -> Result<Json<FetchResponse>, ApiError> {
    Ok(Json(state.operations.fetch(request).await?))
}

async fn fetch_bytes(
    State(state): State<AppState>,
    Json(request): Json<FetchBytesRequest>,
) -> Result<Json<FetchBytesResponse>, ApiError> {
    Ok(Json(state.operations.fetch_bytes(request).await?))
}

/// The one route that answers with a body that is not JSON: the bytes of a
/// source or referenced fragment under their own content type, so an
/// `<img>` in the console or any HTTP client reads an indexed image as a
/// plain URL. `fetch_bytes` unwrapped, nothing more — the operation decides
/// what may be served and how much.
async fn raw(
    State(state): State<AppState>,
    Query(request): Query<FetchBytesRequest>,
) -> Result<Response, ApiError> {
    let response = state.operations.fetch_bytes(request).await?;
    // A mimetype is validated as `type/subtype;k=v` at the store boundary,
    // so this fallback covers only a parameter value with header-hostile
    // bytes; the body is served either way.
    let content_type = axum::http::HeaderValue::from_str(&response.content_type)
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("application/octet-stream"));
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_static("inline"),
            ),
        ],
        response.bytes.0,
    )
        .into_response())
}

async fn index(
    State(state): State<AppState>,
    Json(request): Json<HttpIndexRequest>,
) -> Result<Json<inseam_seams::sweep::IndexReport>, ApiError> {
    let Some(root) = state
        .index_roots
        .iter()
        .find(|root| root.id == request.root)
    else {
        return Err(ApiError::unknown_index_root(&request.root));
    };
    let response = state
        .operations
        .index(IndexRequest {
            host: request.host,
            root: root.path.to_string_lossy().into_owned(),
            rebuild: request.rebuild,
            deep_budget: request.deep_budget,
            llm_lane: request.llm_lane,
        })
        .await?;
    Ok(Json(response))
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
    use inseam_seams::SeamError;
    use inseam_seams::oauth::{AuthorizationStarted, GrantState};
    use inseam_seams::operations::{
        AwaitAuthorizationRequest, ExpandResponse, FetchResponse, IndexRequest, PluginState,
        QueryResponse, RepairOutcome, RepairReport, RepairRequest, ScanResponse,
        PLUGIN_UPLOAD_BYTES_MAX,
    };
    use inseam_seams::sweep::IndexReport;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    /// A PNG signature followed by padding: enough to be recognizably not
    /// JSON on the wire.
    const PNG_BYTES: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0];

    #[derive(Default)]
    struct StubOperations {
        indexed: Mutex<Option<IndexRequest>>,
        authorized: Mutex<Option<AuthorizeGrantRequest>>,
        completed: Mutex<Option<AuthorizationCallback>>,
        installed: Mutex<Option<InstallPluginRequest>>,
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
            if request.address.locator.as_str() != "tmp/logo.png" {
                return Err(SeamError::UnknownSource(request.address));
            }
            Ok(FetchBytesResponse {
                address: request.address,
                content_type: "image/png".to_string(),
                bytes: inseam_seams::operations::FileBytes(PNG_BYTES.to_vec()),
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
            Ok(Vec::new())
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

        async fn authorize_grant(&self, request: AuthorizeGrantRequest) -> Result<AuthorizationStarted, SeamError> {
            let redirect_uri = match &request.redirect {
                Redirect::External { redirect_uri } => redirect_uri.clone(),
                Redirect::Loopback => "http://127.0.0.1:1/callback".to_string(),
            };
            let grant = request.grant.clone();
            *self.authorized.lock().unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(AuthorizationStarted {
                grant,
                url: format!("https://accounts.example/auth?redirect_uri={redirect_uri}&state=st"),
                state: "st".to_string(),
                redirect_uri,
            })
        }

        async fn await_authorization(&self, _request: AwaitAuthorizationRequest) -> Result<GrantView, SeamError> {
            Err(unused())
        }

        async fn complete_authorization(&self, callback: AuthorizationCallback) -> Result<GrantView, SeamError> {
            let ok = callback.state.as_deref() == Some("st") && callback.code.is_some();
            *self.completed.lock().unwrap_or_else(|error| error.into_inner()) = Some(callback);
            if ok {
                Ok(grant_view(GrantState::Authorized { expires_at: None, scopes: Vec::new(), account: Some("greg@example.com".into()) }))
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

        async fn install_plugin(&self, request: InstallPluginRequest) -> Result<PluginView, SeamError> {
            let id = request.id.to_string();
            *self.installed.lock().unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(PluginView {
                id,
                plugin: "wasm:/data/plugins/demo/demo.wasm".to_string(),
                state: PluginState::Active,
                effects: Vec::new(),
                missing: Vec::new(),
                missing_secrets: Vec::new(),
            })
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
        router(test_config(index_roots, None), operations).expect("valid router")
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
        let request = Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Flogo.png")
            .header(COOKIE, &cookie)
            .body(Body::empty())
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "image/png");
        assert_eq!(response.headers()[axum::http::header::CONTENT_DISPOSITION], "inline");
        let bytes = to_bytes(response.into_body(), 4096).await.expect("body");
        assert_eq!(bytes.as_ref(), PNG_BYTES);

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
        let request = Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Fnope.png")
            .header(COOKIE, &cookie)
            .body(Body::empty())
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // And no cookie, no bytes.
        let request = Request::get("/api/v1/owner/raw?address=inseam%3A%2F%2Ffs-test%2Ftmp%2Flogo.png")
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
            Arc::new(StubOperations::default()),
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
        assert_eq!(local.oauth_callback_url().expect("derives"), "http://127.0.0.1:7337/api/v1/oauth/callback");
        let mut secure = test_config(Vec::new(), None);
        secure.cookie_security = CookieSecurity::Secure;
        assert_eq!(secure.oauth_callback_url().expect("derives"), "https://127.0.0.1:7337/api/v1/oauth/callback");
        let mut public = test_config(Vec::new(), None);
        public.public_url = Some("https://node.example/some/path".to_string());
        assert_eq!(public.oauth_callback_url().expect("derives"), "https://node.example/api/v1/oauth/callback");
        public.public_url = Some("node.example".to_string());
        assert!(matches!(public.oauth_callback_url(), Err(ConfigError::PublicUrlInvalid(_))));
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
        assert_eq!(body["redirect_uri"], "http://127.0.0.1:7337/api/v1/oauth/callback");
        assert_eq!(body["state"], "st");
        let recorded = operations.authorized.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            recorded.expect("recorded").redirect,
            Redirect::External { redirect_uri: "http://127.0.0.1:7337/api/v1/oauth/callback".to_string() }
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
            Arc::clone(&operations) as Arc<dyn Operations>,
        )
        .expect("valid router");
        let request = Request::get("/api/v1/oauth/callback?code=abc&state=st")
            .body(Body::empty())
            .expect("valid request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/?authorized=google");
        let completed = operations.completed.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(completed.expect("delivered").code.as_deref(), Some("abc"));

        let request = Request::get("/api/v1/oauth/callback?error=access_denied&state=nope")
            .body(Body::empty())
            .expect("valid request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(response.headers()["location"].to_str().expect("ascii").starts_with("/?authorization_error="));

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
        let installed = operations.installed.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
