//! The stateful half of the OAuth plugin: one [`GrantHandle`] per configured
//! grant, owning its credential file, its in-memory tokens, and the
//! loopback authorization flow. Every decision about the protocol itself is
//! in `flow.rs`; this file does I/O — the network (token endpoint), the
//! clock (expiry), and the file (persistence) — each injected so tests run
//! against a fake endpoint and a fixed clock.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use inseam_kernel::address::Timestamp;
use inseam_seams::oauth::{AccessToken, Grant, GrantId, GrantState, PendingAuthorization};
use inseam_seams::SeamError;

use super::flow::{
    self, authorization_url, http_response, parse_callback, parse_token_response,
    AuthorizationParams, StoredTokens, REQUEST_HEAD_BYTES_MAX, TOKENS_VERSION,
};
use super::GrantConfig;

/// Access tokens are refreshed this many seconds before they expire, so a
/// token handed to a consumer is good for at least this long.
pub const EXPIRY_SKEW_SECS: i64 = 60;

/// Loopback connections one authorization attempt will look at before
/// giving up: browsers fetch favicons and prefetch, so the callback is
/// rarely the first connection, but it is never the fortieth.
pub const CALLBACK_CONNECTIONS_MAX: u32 = 32;

/// The clock the grant reads expiry against; injected so tests can move it.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::from(SystemTime::now())
    }
}

/// What every grant of one plugin instance shares.
pub struct Settings {
    pub callback_port: u16,
    pub authorization_timeout: Duration,
    pub http: reqwest::Client,
    pub clock: Arc<dyn Clock>,
}

/// The client's own credentials, read from the environment the config
/// names — or the name of what is missing, so the grant can say so.
pub enum ClientCredentials {
    Ready {
        id: String,
        secret: Option<String>,
    },
    Missing {
        env: String,
    },
}

pub struct GrantHandle {
    inner: Arc<GrantInner>,
}

struct GrantInner {
    config: GrantConfig,
    client: ClientCredentials,
    path: PathBuf,
    settings: Arc<Settings>,
    tokens: Mutex<Option<StoredTokens>>,
}

impl GrantHandle {
    /// Build the handle and load whatever the credential file holds. A
    /// file that does not parse is reported and treated as absent — the
    /// owner re-authorizes; nothing is migrated.
    pub async fn load(
        config: GrantConfig,
        client: ClientCredentials,
        path: PathBuf,
        settings: Arc<Settings>,
    ) -> Self {
        let tokens = read_tokens(&path).await;
        Self {
            inner: Arc::new(GrantInner {
                config,
                client,
                path,
                settings,
                tokens: Mutex::new(tokens),
            }),
        }
    }
}

#[async_trait::async_trait]
impl Grant for GrantHandle {
    fn id(&self) -> &GrantId {
        &self.inner.config.id
    }

    fn scopes(&self) -> &[String] {
        &self.inner.config.scopes
    }

    async fn state(&self) -> GrantState {
        if let ClientCredentials::Missing { env } = &self.inner.client {
            return GrantState::MissingSecret { env: env.clone() };
        }
        match self.inner.tokens.lock().await.as_ref() {
            None => GrantState::Unauthorized,
            Some(tokens) => GrantState::Authorized {
                expires_at: tokens.expires_at.map(Timestamp),
                scopes: tokens.scopes.clone(),
            },
        }
    }

    async fn access_token(&self) -> Result<AccessToken, SeamError> {
        let inner = &self.inner;
        let (client_id, client_secret) = inner.client.ready(&inner.config.id)?;
        let mut guard = inner.tokens.lock().await;
        let Some(tokens) = guard.as_ref() else {
            return Err(SeamError::Unauthorized(format!(
                "grant `{}` has not been authorized; run `inseam authorize {}`",
                inner.config.id, inner.config.id
            )));
        };
        let now = inner.settings.clock.now().0;
        let expiring = tokens
            .expires_at
            .is_some_and(|at| now.saturating_add(EXPIRY_SKEW_SECS) >= at);
        if !expiring {
            return Ok(AccessToken::new(tokens.access_token.clone()));
        }
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            return Err(SeamError::Unauthorized(format!(
                "grant `{}` expired and the provider issued no refresh token; run `inseam authorize {}`",
                inner.config.id, inner.config.id
            )));
        };
        let mut fields = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id),
        ];
        if let Some(secret) = client_secret {
            fields.push(("client_secret", secret));
        }
        let refreshed = inner.exchange(&fields, Some(&refresh_token)).await?;
        write_tokens(&inner.path, &refreshed).await?;
        let token = AccessToken::new(refreshed.access_token.clone());
        *guard = Some(refreshed);
        Ok(token)
    }

    async fn authorize(&self) -> Result<Box<dyn PendingAuthorization>, SeamError> {
        let inner = &self.inner;
        let (client_id, _) = inner.client.ready(&inner.config.id)?;
        let listener = TcpListener::bind(("127.0.0.1", inner.settings.callback_port))
            .await
            .map_err(|e| {
                SeamError::failed(format!(
                    "cannot listen on 127.0.0.1:{} for the authorization redirect: {e}",
                    inner.settings.callback_port
                ))
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| SeamError::failed(format!("loopback listener has no address: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let state = flow::random_token();
        let verifier = flow::random_token();
        let url = authorization_url(&AuthorizationParams {
            authorization_url: &inner.config.authorization_url,
            client_id,
            redirect_uri: &redirect_uri,
            scopes: &inner.config.scopes,
            state: &state,
            code_challenge: &flow::pkce_challenge(&verifier),
            extra: &inner.config.authorization_params,
        })?;
        Ok(Box::new(PendingFlow {
            grant: Arc::clone(inner),
            listener,
            url,
            redirect_uri,
            state,
            verifier,
        }))
    }

    async fn revoke(&self) -> Result<(), SeamError> {
        let mut guard = self.inner.tokens.lock().await;
        match tokio::fs::remove_file(&self.inner.path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(SeamError::failed(format!(
                    "cannot remove {}: {e}",
                    self.inner.path.display()
                )))
            }
        }
        *guard = None;
        Ok(())
    }
}

impl ClientCredentials {
    fn ready(&self, grant: &GrantId) -> Result<(&str, Option<&str>), SeamError> {
        match self {
            Self::Ready { id, secret } => Ok((id.as_str(), secret.as_deref())),
            Self::Missing { env } => Err(SeamError::Unavailable(format!(
                "grant `{grant}` needs {env} in the environment (the OAuth client id or secret)"
            ))),
        }
    }
}

impl GrantInner {
    /// One token-endpoint call: form in, tokens out.
    async fn exchange(
        &self,
        fields: &[(&str, &str)],
        previous_refresh_token: Option<&str>,
    ) -> Result<StoredTokens, SeamError> {
        let response = self
            .settings
            .http
            .post(&self.config.token_url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(flow::form_body(fields))
            .send()
            .await
            .map_err(|e| SeamError::failed(format!("token endpoint {}: {e}", self.config.token_url)))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.map_err(|e| {
            SeamError::failed(format!(
                "token endpoint {} answered {status} with a non-JSON body: {e}",
                self.config.token_url
            ))
        })?;
        let now = self.settings.clock.now().0;
        parse_token_response(&body, now, previous_refresh_token, &self.config.scopes)
    }
}

/// An authorization attempt between `authorize` and the browser's return.
struct PendingFlow {
    grant: Arc<GrantInner>,
    listener: TcpListener,
    url: String,
    redirect_uri: String,
    state: String,
    verifier: String,
}

#[async_trait::async_trait]
impl PendingAuthorization for PendingFlow {
    fn url(&self) -> &str {
        &self.url
    }

    async fn complete(self: Box<Self>) -> Result<(), SeamError> {
        let timeout = self.grant.settings.authorization_timeout;
        let arrived = tokio::time::timeout(timeout, accept_callback(&self.listener, &self.state))
            .await
            .map_err(|_| {
                SeamError::failed(format!(
                    "no authorization redirect arrived within {} seconds",
                    timeout.as_secs()
                ))
            })?;
        let (code, mut stream) = match arrived {
            Ok(arrived) => arrived,
            Err(e) => return Err(e),
        };
        let (client_id, client_secret) = self.grant.client.ready(&self.grant.config.id)?;
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", client_id),
            ("code_verifier", self.verifier.as_str()),
        ];
        if let Some(secret) = client_secret {
            fields.push(("client_secret", secret));
        }
        let outcome = self.grant.exchange(&fields, None).await;
        // The tab is told what happened either way; the owner is looking at
        // it, not at the terminal.
        let page = match &outcome {
            Ok(_) => http_response(200, "OK", "Authorized", "inseam received the grant. You can close this tab."),
            Err(e) => http_response(502, "Bad Gateway", "Authorization failed", &e.to_string()),
        };
        let _ = stream.write_all(&page).await;
        let _ = stream.shutdown().await;
        let tokens = outcome?;
        write_tokens(&self.grant.path, &tokens).await?;
        *self.grant.tokens.lock().await = Some(tokens);
        Ok(())
    }
}

/// Wait for the browser to land on `/callback` with our `state`. Other
/// requests on the port (favicons, stray tabs) are answered 404 and
/// skipped, up to [`CALLBACK_CONNECTIONS_MAX`] of them.
async fn accept_callback(
    listener: &TcpListener,
    expected_state: &str,
) -> Result<(String, TcpStream), SeamError> {
    let mut connections: u32 = 0;
    while connections < CALLBACK_CONNECTIONS_MAX {
        connections += 1;
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| SeamError::failed(format!("loopback accept failed: {e}")))?;
        let head = read_request_head(&mut stream).await?;
        let callback = match parse_callback(&head) {
            Ok(callback) => callback,
            Err(_) => {
                answer(&mut stream, 404, "Not Found", "Not found", "").await;
                continue;
            }
        };
        if callback.path != "/callback" {
            answer(&mut stream, 404, "Not Found", "Not found", "").await;
            continue;
        }
        if callback.state.as_deref() != Some(expected_state) {
            answer(&mut stream, 400, "Bad Request", "Rejected", "state mismatch").await;
            return Err(SeamError::Refused(
                "authorization redirect carried the wrong state; possible CSRF, attempt abandoned".to_string(),
            ));
        }
        if let Some(error) = callback.error {
            let description = callback.error_description.unwrap_or_default();
            answer(&mut stream, 400, "Bad Request", "Authorization denied", &description).await;
            return Err(SeamError::Refused(format!(
                "provider answered `{error}`: {description}"
            )));
        }
        match callback.code {
            Some(code) if !code.is_empty() => return Ok((code, stream)),
            _ => {
                answer(&mut stream, 400, "Bad Request", "Rejected", "no code").await;
                return Err(SeamError::failed("authorization redirect carried no code"));
            }
        }
    }
    Err(SeamError::failed(format!(
        "{CALLBACK_CONNECTIONS_MAX} loopback connections arrived and none was the authorization redirect"
    )))
}

/// Read until the end of the request head or the size cap.
async fn read_request_head(stream: &mut TcpStream) -> Result<String, SeamError> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buffer.len() < REQUEST_HEAD_BYTES_MAX {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| SeamError::failed(format!("loopback read failed: {e}")))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Best-effort answer to a loopback tab; a browser that hung up is not an
/// error in the flow.
async fn answer(stream: &mut TcpStream, status: u16, reason: &str, title: &str, body: &str) {
    let _ = stream.write_all(&http_response(status, reason, title, body)).await;
    let _ = stream.shutdown().await;
}

async fn read_tokens(path: &Path) -> Option<StoredTokens> {
    let raw = match tokio::fs::read(path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), "cannot read credential file: {e}");
            return None;
        }
    };
    match serde_json::from_slice::<StoredTokens>(&raw) {
        Ok(tokens) if tokens.version == TOKENS_VERSION => Some(tokens),
        Ok(tokens) => {
            tracing::warn!(
                path = %path.display(),
                "credential file is version {}, expected {TOKENS_VERSION}; re-authorize",
                tokens.version
            );
            None
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), "credential file does not parse: {e}; re-authorize");
            None
        }
    }
}

/// Persist tokens: directory private to the owner, file written whole then
/// renamed into place, so a crash mid-write never leaves a half file.
async fn write_tokens(path: &Path, tokens: &StoredTokens) -> Result<(), SeamError> {
    let parent = path
        .parent()
        .ok_or_else(|| SeamError::failed(format!("{} has no parent directory", path.display())))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| SeamError::failed(format!("create {}: {e}", parent.display())))?;
    set_private(parent, 0o700).await?;
    let rendered = serde_json::to_vec_pretty(tokens)
        .map_err(|e| SeamError::failed(format!("render credential file: {e}")))?;
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, rendered)
        .await
        .map_err(|e| SeamError::failed(format!("write {}: {e}", temporary.display())))?;
    set_private(&temporary, 0o600).await?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|e| SeamError::failed(format!("rename into {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(unix)]
async fn set_private(path: &Path, mode: u32) -> Result<(), SeamError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .await
        .map_err(|e| SeamError::failed(format!("chmod {}: {e}", path.display())))
}

#[cfg(not(unix))]
async fn set_private(_path: &Path, _mode: u32) -> Result<(), SeamError> {
    Ok(())
}
