use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use inseam_seams::SeamError;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("INSEAM_OWNER_TOKEN must contain at least {0} bytes")]
    OwnerTokenTooShort(usize),
    #[error("index root id may not be empty")]
    IndexRootIdEmpty,
    #[error("index root id `{0}` is longer than 64 characters")]
    IndexRootIdTooLong(String),
    #[error("index root id `{0}` may only contain lowercase ASCII letters, digits, and `-`")]
    IndexRootIdInvalid(String),
    #[error("index root id `{0}` is configured more than once")]
    DuplicateIndexRoot(String),
    #[error("{0} index roots configured; at most 64 are allowed")]
    TooManyIndexRoots(usize),
    #[error("index root `{id}` at `{}` cannot be read: {source}", path.display())]
    IndexRoot {
        id: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("index root `{id}` at `{}` is not a directory", path.display())]
    IndexRootNotDirectory { id: String, path: PathBuf },
    #[error("web build is missing `{}`", .0.display())]
    WebIndexMissing(PathBuf),
    #[error("INSEAM_PUBLIC_URL `{0}` must be an absolute http(s) origin such as https://node.example")]
    PublicUrlInvalid(String),
    #[error("cannot listen on {bind}: {source}")]
    Bind {
        bind: SocketAddr,
        source: std::io::Error,
    },
    #[error("HTTP server failed: {0}")]
    Serve(std::io::Error),
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorView,
}

#[derive(Debug, Serialize)]
struct ErrorView {
    code: &'static str,
    message: String,
}

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub(crate) fn route_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "route_not_found",
            "the requested API route does not exist",
        )
    }

    pub(crate) fn authentication_required() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "sign in to manage this node",
        )
    }

    pub(crate) fn invalid_credentials() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "the owner token is not valid",
        )
    }

    pub(crate) fn unknown_index_root(root: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "unknown_index_root",
            format!("`{root}` is neither an approved index root id nor a folder configured on a host"),
        )
    }

    pub(crate) fn clock() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "clock_unavailable",
            "the system clock is earlier than the Unix epoch",
        )
    }

    pub(crate) fn header(error: axum::http::header::InvalidHeaderValue) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session_failed",
            format!("could not create the owner session: {error}"),
        )
    }

    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl From<SeamError> for ApiError {
    fn from(error: SeamError) -> Self {
        let (status, code) = match &error {
            SeamError::UnknownSource(_) | SeamError::UnknownHost(_) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            SeamError::Address(_)
            | SeamError::AmbiguousHost(_)
            | SeamError::ScanRange { .. }
            | SeamError::ScanBeyondEnd { .. } => (StatusCode::BAD_REQUEST, "invalid_request"),
            SeamError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            SeamError::Refused(_) => (StatusCode::FORBIDDEN, "refused"),
            SeamError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            SeamError::NothingToScan(_) | SeamError::BinaryFetch(_, _) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "unsupported_source")
            }
            SeamError::FetchTooLarge { .. } => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            SeamError::Store(_) | SeamError::Failed(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "operation_failed")
            }
        };
        Self::new(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorView {
                code: self.code,
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}
