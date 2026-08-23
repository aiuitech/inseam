//! HTTP/JSON owner transport for an inseam node (`design/node-api.md`).
//! The transport authenticates an owner session, bounds the wire, translates
//! JSON to the existing operation messages, and serves an optional Vite
//! build. It contains no node logic and never calls the store or plugins
//! directly. API responses are not cacheable; every response carries a
//! restrictive browser security policy.

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
use inseam_kernel::address::HostId;
use inseam_seams::operations::{
    ExpandRequest, ExpandResponse, FetchRequest, FetchResponse, HostView, IndexRequest, Operations,
    QueryRequest, QueryResponse, ScanRequest, ScanResponse, StatusReport,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use auth::CookieSecurity;
use auth::{Auth, login, logout, require_owner, session};
use error::{ApiError, ConfigError};

const BODY_BYTES_MAX: usize = 64 * 1024;
const REQUESTS_IN_FLIGHT_MAX: usize = 64;
const REQUEST_TIMEOUT_SECS: u64 = 60;
const INDEX_ROOTS_MAX: usize = 64;

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
}

impl ServerConfig {
    pub fn validate(self) -> Result<Self, ConfigError> {
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
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct OwnerInfo {
    version: &'static str,
    index_roots: Vec<IndexRoot>,
}

#[derive(Debug, Deserialize)]
struct HttpIndexRequest {
    #[serde(default)]
    host: Option<HostId>,
    root: String,
    #[serde(default)]
    rebuild: bool,
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
    let state = AppState {
        operations,
        auth: Arc::new(Auth::new(&config.owner_token, config.cookie_security)?),
        index_roots: config.index_roots.into(),
    };
    let owner = owner_router(&state);
    let api = Router::new()
        .route("/health", get(health))
        .route("/session", get(session).post(login).delete(logout))
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
        .route("/hosts", get(hosts))
        .route("/query", post(query))
        .route("/expand", post(expand))
        .route("/scan", post(scan))
        .route("/fetch", post(fetch))
        .route("/index", post(index))
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
    })
}

async fn status(State(state): State<AppState>) -> Result<Json<StatusReport>, ApiError> {
    Ok(Json(state.operations.status().await?))
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
    use inseam_seams::operations::{
        ExpandResponse, FetchResponse, IndexRequest, QueryResponse, ScanResponse,
    };
    use inseam_seams::sweep::IndexReport;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[derive(Default)]
    struct StubOperations {
        indexed: Mutex<Option<IndexRequest>>,
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

        async fn status(&self) -> Result<StatusReport, SeamError> {
            Ok(StatusReport {
                sources: 3,
                indexed_sources: 2,
                fragments: 7,
                relations: 1,
                keyed_fragments: 0,
                search_rows: 7,
                embedding_model: None,
                embedding_dimensions: 0,
                reembed_pending: false,
            })
        }
    }

    fn unused() -> SeamError {
        SeamError::Unavailable("unused by this transport test".to_string())
    }

    fn test_router(operations: Arc<StubOperations>, index_roots: Vec<IndexRoot>) -> Router {
        router(
            ServerConfig {
                bind: "127.0.0.1:0".parse().expect("valid bind"),
                owner_token: TOKEN.to_string(),
                cookie_security: CookieSecurity::LocalHttp,
                index_roots,
                web_dir: None,
            },
            operations,
        )
        .expect("valid router")
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
    async fn unknown_api_routes_do_not_fall_back_to_the_web_client() {
        let web_dir = tempfile::tempdir().expect("temporary web directory");
        std::fs::write(web_dir.path().join("index.html"), "web client").expect("web index");
        let app = router(
            ServerConfig {
                bind: "127.0.0.1:0".parse().expect("valid bind"),
                owner_token: TOKEN.to_string(),
                cookie_security: CookieSecurity::LocalHttp,
                index_roots: Vec::new(),
                web_dir: Some(web_dir.path().to_path_buf()),
            },
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
}
