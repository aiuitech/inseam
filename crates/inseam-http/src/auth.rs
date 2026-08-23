use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::error::{ApiError, ConfigError};

const COOKIE_NAME: &str = "inseam_owner";
const SESSION_SECONDS: u64 = 12 * 60 * 60;
const TOKEN_BYTES_MIN: usize = 32;
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieSecurity {
    Secure,
    LocalHttp,
}

pub(crate) struct Auth {
    secret: Vec<u8>,
    token_digest: [u8; 32],
    cookie_security: CookieSecurity,
}

impl Auth {
    pub(crate) fn validate_token(token: &str) -> Result<(), ConfigError> {
        if token.len() < TOKEN_BYTES_MIN {
            return Err(ConfigError::OwnerTokenTooShort(TOKEN_BYTES_MIN));
        }
        Ok(())
    }

    pub(crate) fn new(token: &str, cookie_security: CookieSecurity) -> Result<Self, ConfigError> {
        Self::validate_token(token)?;
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        Ok(Self {
            secret: token.as_bytes().to_vec(),
            token_digest: digest,
            cookie_security,
        })
    }

    fn token_matches(&self, candidate: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        bool::from(self.token_digest.ct_eq(&digest))
    }

    fn session_cookie(&self, now_seconds: u64) -> Result<String, ApiError> {
        let expires = now_seconds
            .checked_add(SESSION_SECONDS)
            .ok_or_else(ApiError::clock)?;
        let payload = format!("v1.{expires}");
        let signature = self.signature(&payload);
        let secure = match self.cookie_security {
            CookieSecurity::Secure => "; Secure",
            CookieSecurity::LocalHttp => "",
        };
        Ok(format!(
            "{COOKIE_NAME}={payload}.{signature}; Path=/api/v1; Max-Age={SESSION_SECONDS}; HttpOnly; SameSite=Strict{secure}"
        ))
    }

    fn authenticated(&self, headers: &HeaderMap, now_seconds: u64) -> bool {
        let Some(cookie) = cookie_value(headers, COOKIE_NAME) else {
            return false;
        };
        self.verify_cookie(cookie, now_seconds)
    }

    fn verify_cookie(&self, cookie: &str, now_seconds: u64) -> bool {
        let mut pieces = cookie.split('.');
        let Some(version) = pieces.next() else {
            return false;
        };
        let Some(expires) = pieces.next() else {
            return false;
        };
        let Some(signature) = pieces.next() else {
            return false;
        };
        if pieces.next().is_some() {
            return false;
        }
        if version != "v1" {
            return false;
        }
        let Ok(expires_seconds) = expires.parse::<u64>() else {
            return false;
        };
        if expires_seconds < now_seconds {
            return false;
        }
        if expires_seconds.saturating_sub(now_seconds) > SESSION_SECONDS {
            return false;
        }
        let payload = format!("{version}.{expires}");
        self.signature_matches(&payload, signature)
    }

    fn signature(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .expect("HMAC accepts keys of every byte length");
        mac.update(payload.as_bytes());
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn signature_matches(&self, payload: &str, candidate: &str) -> bool {
        let Ok(candidate) = URL_SAFE_NO_PAD.decode(candidate) else {
            return false;
        };
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .expect("HMAC accepts keys of every byte length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&candidate).is_ok()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginRequest {
    token: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionResponse {
    authenticated: bool,
}

pub(crate) async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    if !state.auth.token_matches(&request.token) {
        return Err(ApiError::invalid_credentials());
    }
    let cookie = state.auth.session_cookie(now_seconds()?)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::header)?,
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(crate) async fn logout() -> Response {
    let cookie = format!("{COOKIE_NAME}=; Path=/api/v1; Max-Age=0; HttpOnly; SameSite=Strict");
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("fixed cookie header is valid"),
    );
    response
}

pub(crate) async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, ApiError> {
    let authenticated = state.auth.authenticated(&headers, now_seconds()?);
    Ok(Json(SessionResponse { authenticated }))
}

pub(crate) async fn require_owner(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let now = match now_seconds() {
        Ok(now) => now,
        Err(error) => return error.into_response(),
    };
    if state.auth.authenticated(request.headers(), now) {
        return next.run(request).await;
    }
    ApiError::authentication_required().into_response()
}

fn now_seconds() -> Result<u64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| ApiError::clock())
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .find_map(|pair| {
            pair.strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn signed_cookie_authenticates_until_expiry() {
        let auth = Auth::new(TOKEN, CookieSecurity::Secure).expect("valid auth");
        let header = auth.session_cookie(100).expect("cookie");
        let value = header
            .split(';')
            .next()
            .and_then(|pair| pair.strip_prefix("inseam_owner="))
            .expect("cookie value");
        assert!(auth.verify_cookie(value, 100));
        assert!(auth.verify_cookie(value, 100 + SESSION_SECONDS));
        assert!(!auth.verify_cookie(value, 101 + SESSION_SECONDS));
    }

    #[test]
    fn signed_cookie_rejects_tampering() {
        let auth = Auth::new(TOKEN, CookieSecurity::Secure).expect("valid auth");
        let header = auth.session_cookie(100).expect("cookie");
        let value = header
            .split(';')
            .next()
            .and_then(|pair| pair.strip_prefix("inseam_owner="))
            .expect("cookie value");
        let tampered = format!("{value}x");
        assert!(!auth.verify_cookie(&tampered, 100));
    }

    #[test]
    fn owner_token_has_a_fixed_minimum_length() {
        assert!(Auth::validate_token("short").is_err());
        assert!(Auth::validate_token(TOKEN).is_ok());
    }
}
