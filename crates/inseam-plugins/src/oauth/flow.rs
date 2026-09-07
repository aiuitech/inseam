//! The pure half of the OAuth plugin: PKCE material, the authorization URL,
//! the loopback callback's request parsing, token-endpoint bodies and
//! responses, and the credential-file record. Nothing here touches the
//! network, a clock, or a file — `grant.rs` supplies all three — so every
//! rule of the protocol is testable as data in, data out.
//!
//! Protocol references: RFC 6749 (authorization code grant, §4.1; refresh,
//! §6), RFC 7636 (PKCE, S256), RFC 8252 (native apps: loopback redirect).

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use url::form_urlencoded;

use inseam_seams::SeamError;

/// Bytes of entropy behind the CSRF `state` and the PKCE verifier. 32 bytes
/// base64url-encode to 43 characters — exactly RFC 7636's minimum verifier
/// length, and far past any guessable `state`.
pub const RANDOM_BYTES: usize = 32;

/// Longest request head the loopback listener reads from a browser. A
/// redirect is one short line plus a few headers; anything bigger is not
/// the provider calling back.
pub const REQUEST_HEAD_BYTES_MAX: usize = 8 * 1024;

/// The credential-file format version; a bump discards old files (the
/// owner re-authorizes) rather than migrating them. Version 2 added the
/// signed-in account.
pub const TOKENS_VERSION: u32 = 2;

/// A fresh base64url token of [`RANDOM_BYTES`] from the OS CSPRNG.
pub fn random_token() -> String {
    let mut bytes = [0u8; RANDOM_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// RFC 7636 S256: `BASE64URL(SHA256(ASCII(verifier)))`.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What one authorization attempt sends the owner to.
pub struct AuthorizationParams<'a> {
    pub authorization_url: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub scopes: &'a [String],
    pub state: &'a str,
    pub code_challenge: &'a str,
    /// Provider-specific extras (`access_type=offline`, `prompt=consent`).
    pub extra: &'a BTreeMap<String, String>,
}

/// The authorization request URL (RFC 6749 §4.1.1 with PKCE). Extras go
/// last and may not shadow the standard parameters — a config that tried
/// to would be asking for a different protocol.
pub fn authorization_url(params: &AuthorizationParams<'_>) -> Result<String, SeamError> {
    const STANDARD: [&str; 7] = [
        "response_type",
        "client_id",
        "redirect_uri",
        "scope",
        "state",
        "code_challenge",
        "code_challenge_method",
    ];
    let mut url = Url::parse(params.authorization_url)
        .map_err(|e| SeamError::failed(format!("authorization_url: {e}")))?;
    if let Some(shadowed) = params.extra.keys().find(|k| STANDARD.contains(&k.as_str())) {
        return Err(SeamError::failed(format!(
            "authorization_params may not set the standard parameter `{shadowed}`"
        )));
    }
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", params.client_id);
        query.append_pair("redirect_uri", params.redirect_uri);
        query.append_pair("scope", &params.scopes.join(" "));
        query.append_pair("state", params.state);
        query.append_pair("code_challenge", params.code_challenge);
        query.append_pair("code_challenge_method", "S256");
        for (key, value) in params.extra {
            query.append_pair(key, value);
        }
    }
    Ok(url.into())
}

/// The loopback redirect as the browser sent it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Callback {
    pub path: String,
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Parse the request head of one loopback connection. Only a `GET` with a
/// target is a callback; anything else is refused and answered 404 by the
/// listener.
pub fn parse_callback(request_head: &str) -> Result<Callback, SeamError> {
    let line = request_head.lines().next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" || target.is_empty() {
        return Err(SeamError::failed(format!(
            "not a callback request: `{line}`"
        )));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut callback = Callback {
        path: path.to_string(),
        ..Callback::default()
    };
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
            "code" => callback.code = Some(value),
            "state" => callback.state = Some(value),
            "error" => callback.error = Some(value),
            "error_description" => callback.error_description = Some(value),
            _ => {}
        }
    }
    Ok(callback)
}

/// A form-encoded token-endpoint body (RFC 6749 §4.1.3 / §6).
pub fn form_body(fields: &[(&str, &str)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// One minimal HTTP/1.1 response for the browser tab that landed on the
/// loopback listener.
pub fn http_response(status: u16, reason: &str, title: &str, body: &str) -> Vec<u8> {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui;margin:3rem\"><h1>{title}</h1><p>{body}</p></body></html>"
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    )
    .into_bytes()
}

/// What a credential file holds: the record the token endpoint gave us,
/// with the expiry resolved to an absolute instant. Refresh tokens are the
/// long-lived secret; access tokens are replaced in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokens {
    pub version: u32,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix-epoch seconds; `None` when the provider gave no `expires_in`.
    pub expires_at: Option<i64>,
    pub scopes: Vec<String>,
    pub token_type: String,
    /// The signed-in account (the OpenID Connect `email` claim) when the
    /// provider issued an `id_token`: the principal a connection derives its
    /// host identity from. Carried across refreshes, which rarely re-issue
    /// the id token.
    #[serde(default)]
    pub account: Option<String>,
}

/// Interpret a token-endpoint response body (RFC 6749 §5.1 / §5.2).
/// `previous` is the record a refresh replaces: its refresh token survives
/// a response that omits one, and so does its account; `declared_scopes`
/// stands in when the provider does not echo `scope`.
pub fn parse_token_response(
    body: &serde_json::Value,
    now_epoch: i64,
    previous: Option<&StoredTokens>,
    declared_scopes: &[String],
) -> Result<StoredTokens, SeamError> {
    if let Some(error) = body.get("error").and_then(|e| e.as_str()) {
        let description = body
            .get("error_description")
            .and_then(|d| d.as_str())
            .unwrap_or("no description");
        return Err(SeamError::Refused(format!(
            "token endpoint answered `{error}`: {description}"
        )));
    }
    let access_token = body
        .get("access_token")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| SeamError::failed("token response carries no access_token"))?;
    let token_type = body
        .get("token_type")
        .and_then(|t| t.as_str())
        .unwrap_or("Bearer");
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(SeamError::failed(format!(
            "token type `{token_type}` is not bearer; only bearer tokens are supported"
        )));
    }
    let expires_at = body
        .get("expires_in")
        .and_then(|e| {
            e.as_i64()
                .or_else(|| e.as_str().and_then(|s| s.parse().ok()))
        })
        .map(|seconds| now_epoch.saturating_add(seconds));
    let refresh_token = body
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .or_else(|| previous.and_then(|p| p.refresh_token.clone()));
    let scopes = match body.get("scope").and_then(|s| s.as_str()) {
        Some(scope) if !scope.trim().is_empty() => {
            scope.split_whitespace().map(str::to_string).collect()
        }
        _ => declared_scopes.to_vec(),
    };
    let account = body
        .get("id_token")
        .and_then(|t| t.as_str())
        .and_then(id_token_email)
        .or_else(|| previous.and_then(|p| p.account.clone()));
    Ok(StoredTokens {
        version: TOKENS_VERSION,
        access_token: access_token.to_string(),
        refresh_token,
        expires_at,
        scopes,
        token_type: "Bearer".to_string(),
        account,
    })
}

/// The `email` claim of an OpenID Connect id token, read without verifying
/// the signature. That is deliberate and within the spec: the token arrived
/// directly from the token endpoint over TLS in the same response as the
/// access token (OpenID Connect Core §3.1.3.7, item 6), so its integrity
/// is the channel's; it is used only to name the account, never to grant
/// anything.
pub fn id_token_email(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("email")
        .and_then(|e| e.as_str())
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_the_rfc_7636_vector() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_are_long_enough_and_distinct() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 43, "32 bytes base64url without padding");
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn authorization_url_carries_every_standard_parameter_and_extras() {
        let mut extra = BTreeMap::new();
        extra.insert("access_type".to_string(), "offline".to_string());
        let url = authorization_url(&AuthorizationParams {
            authorization_url: "https://accounts.example.com/o/auth?hd=example.com",
            client_id: "client-1",
            redirect_uri: "http://127.0.0.1:47781/callback",
            scopes: &["a".to_string(), "b c".to_string()],
            state: "st",
            code_challenge: "ch",
            extra: &extra,
        })
        .expect("builds");
        let parsed = Url::parse(&url).expect("valid url");
        let pairs: BTreeMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs["hd"], "example.com", "existing query survives");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["client_id"], "client-1");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:47781/callback");
        assert_eq!(pairs["scope"], "a b c");
        assert_eq!(pairs["state"], "st");
        assert_eq!(pairs["code_challenge"], "ch");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["access_type"], "offline");
    }

    #[test]
    fn authorization_url_refuses_extras_that_shadow_standard_parameters() {
        let mut extra = BTreeMap::new();
        extra.insert("state".to_string(), "forged".to_string());
        let result = authorization_url(&AuthorizationParams {
            authorization_url: "https://accounts.example.com/o/auth",
            client_id: "c",
            redirect_uri: "http://127.0.0.1:1/callback",
            scopes: &[],
            state: "st",
            code_challenge: "ch",
            extra: &extra,
        });
        assert!(result.is_err());
    }

    #[test]
    fn parse_callback_reads_code_state_and_errors() {
        let ok = parse_callback(
            "GET /callback?code=abc%20d&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(ok.path, "/callback");
        assert_eq!(ok.code.as_deref(), Some("abc d"));
        assert_eq!(ok.state.as_deref(), Some("xyz"));
        assert_eq!(ok.error, None);

        let denied = parse_callback(
            "GET /callback?error=access_denied&error_description=nope&state=xyz HTTP/1.1\r\n",
        )
        .expect("parses");
        assert_eq!(denied.error.as_deref(), Some("access_denied"));
        assert_eq!(denied.error_description.as_deref(), Some("nope"));

        let favicon = parse_callback("GET /favicon.ico HTTP/1.1\r\n").expect("parses");
        assert_eq!(favicon.path, "/favicon.ico");
        assert_eq!(favicon.code, None);
    }

    #[test]
    fn parse_callback_refuses_non_get_requests() {
        assert!(parse_callback("POST /callback HTTP/1.1\r\n").is_err());
        assert!(parse_callback("").is_err());
    }

    #[test]
    fn token_response_resolves_expiry_and_keeps_the_old_refresh_token() {
        let body = serde_json::json!({
            "access_token": "at", "token_type": "bearer", "expires_in": 3600
        });
        let previous = StoredTokens {
            version: TOKENS_VERSION,
            access_token: "old".to_string(),
            refresh_token: Some("rt-old".to_string()),
            expires_at: None,
            scopes: Vec::new(),
            token_type: "Bearer".to_string(),
            account: Some("greg@example.com".to_string()),
        };
        let tokens = parse_token_response(&body, 1_000, Some(&previous), &["s".to_string()])
            .expect("parses");
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-old"));
        assert_eq!(tokens.expires_at, Some(4_600));
        assert_eq!(tokens.scopes, vec!["s".to_string()]);
        assert_eq!(
            tokens.account.as_deref(),
            Some("greg@example.com"),
            "account survives a refresh"
        );

        let rotated = serde_json::json!({
            "access_token": "at2", "refresh_token": "rt-new", "scope": "x y"
        });
        let tokens = parse_token_response(&rotated, 0, Some(&previous), &[]).expect("parses");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(tokens.expires_at, None);
        assert_eq!(tokens.scopes, vec!["x".to_string(), "y".to_string()]);
    }

    /// An id token whose payload carries the given claims; the signature is
    /// junk on purpose — it is not verified.
    fn fake_id_token(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn token_response_reads_the_account_from_the_id_token() {
        let body = serde_json::json!({
            "access_token": "at",
            "id_token": fake_id_token(serde_json::json!({"sub": "1", "email": "Greg@Example.com"}))
        });
        let tokens = parse_token_response(&body, 0, None, &[]).expect("parses");
        assert_eq!(tokens.account.as_deref(), Some("Greg@Example.com"));
        assert_eq!(id_token_email("not.a-token"), None);
        assert_eq!(
            id_token_email(&fake_id_token(serde_json::json!({"sub": "1"}))),
            None
        );
        let none = parse_token_response(&serde_json::json!({"access_token": "at"}), 0, None, &[])
            .expect("parses");
        assert_eq!(none.account, None);
    }

    #[test]
    fn token_response_errors_are_refusals_and_missing_tokens_fail() {
        let denied = serde_json::json!({"error": "invalid_grant", "error_description": "expired"});
        assert!(matches!(
            parse_token_response(&denied, 0, None, &[]),
            Err(SeamError::Refused(_))
        ));
        let empty = serde_json::json!({"token_type": "Bearer"});
        assert!(matches!(
            parse_token_response(&empty, 0, None, &[]),
            Err(SeamError::Failed(_))
        ));
        let mac = serde_json::json!({"access_token": "x", "token_type": "MAC"});
        assert!(parse_token_response(&mac, 0, None, &[]).is_err());
    }

    #[test]
    fn form_body_encodes_pairs() {
        assert_eq!(
            form_body(&[("grant_type", "authorization_code"), ("code", "a b&c")]),
            "grant_type=authorization_code&code=a+b%26c"
        );
    }

    #[test]
    fn http_response_is_well_formed() {
        let bytes = http_response(200, "OK", "Done", "close me");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: "));
        assert!(text.ends_with("</html>"));
    }
}
