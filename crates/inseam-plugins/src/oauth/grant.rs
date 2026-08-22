//! The stateful half of one grant: a [`GrantHandle`] owning its credential
//! file, its in-memory tokens, and the token-endpoint calls that mint and
//! refresh them. Every decision about the protocol itself is in `flow.rs`;
//! this file does I/O — the network (token endpoint), the clock (expiry),
//! and the file (persistence) — each injected so tests run against a fake
//! endpoint and a fixed clock. The authorization dance that produces the
//! first tokens lives one level up (`attempt.rs`, driven by the service),
//! because it spans transports; the handle only begins it (the URL) and
//! finishes it (the exchange).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use inseam_kernel::address::Timestamp;
use inseam_kernel::substrate::EventBus;
use inseam_seams::oauth::{
    AccessToken, Grant, GrantChanged, GrantId, GrantSpec, GrantState,
};
use inseam_seams::SeamError;

use super::flow::{
    self, authorization_url, parse_token_response, AuthorizationParams, StoredTokens,
    TOKENS_VERSION,
};

/// Access tokens are refreshed this many seconds before they expire, so a
/// token handed to a consumer is good for at least this long.
pub const EXPIRY_SKEW_SECS: i64 = 60;

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
    /// Where [`GrantChanged`] is announced.
    pub bus: EventBus,
}

/// The client's own credentials, read from the environment the spec
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

impl ClientCredentials {
    /// Read the client's credentials from the environment the spec names.
    pub fn from_environment(spec: &GrantSpec) -> Self {
        let present = |env: &str| std::env::var(env).ok().filter(|v| !v.trim().is_empty());
        let Some(id) = present(&spec.client_id_env) else {
            return Self::Missing {
                env: spec.client_id_env.clone(),
            };
        };
        let secret = match &spec.client_secret_env {
            None => None,
            Some(env) => match present(env) {
                Some(secret) => Some(secret),
                None => return Self::Missing { env: env.clone() },
            },
        };
        Self::Ready { id, secret }
    }

    fn ready(&self, grant: &GrantId) -> Result<(&str, Option<&str>), SeamError> {
        match self {
            Self::Ready { id, secret } => Ok((id.as_str(), secret.as_deref())),
            Self::Missing { env } => Err(SeamError::Unavailable(format!(
                "grant `{grant}` needs {env} in the environment (the OAuth client id or secret)"
            ))),
        }
    }
}

/// What [`GrantHandle::begin`] hands the service for one attempt: the URL
/// to send the owner to, and the PKCE verifier the exchange needs back.
pub struct Begun {
    pub url: String,
    pub state: String,
    pub verifier: String,
}

#[derive(Clone)]
pub struct GrantHandle {
    inner: Arc<GrantInner>,
}

struct GrantInner {
    spec: GrantSpec,
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
        spec: GrantSpec,
        client: ClientCredentials,
        path: PathBuf,
        settings: Arc<Settings>,
    ) -> Self {
        let tokens = read_tokens(&path).await;
        Self {
            inner: Arc::new(GrantInner {
                spec,
                client,
                path,
                settings,
                tokens: Mutex::new(tokens),
            }),
        }
    }

    /// The first half of an authorization: fresh `state` and PKCE material,
    /// and the provider URL carrying them. Refused while the client
    /// credentials are missing — there is nothing to sign in as.
    pub fn begin(&self, redirect_uri: &str) -> Result<Begun, SeamError> {
        let inner = &self.inner;
        let (client_id, _) = inner.client.ready(&inner.spec.id)?;
        let state = flow::random_token();
        let verifier = flow::random_token();
        let url = authorization_url(&AuthorizationParams {
            authorization_url: &inner.spec.authorization_url,
            client_id,
            redirect_uri,
            scopes: &inner.spec.scopes,
            state: &state,
            code_challenge: &flow::pkce_challenge(&verifier),
            extra: &inner.spec.authorization_params,
        })?;
        Ok(Begun {
            url,
            state,
            verifier,
        })
    }

    /// The second half: exchange the code the browser brought back, persist
    /// the tokens, and announce the grant authorized.
    pub async fn finish(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<(), SeamError> {
        let inner = &self.inner;
        let (client_id, client_secret) = inner.client.ready(&inner.spec.id)?;
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ];
        if let Some(secret) = client_secret {
            fields.push(("client_secret", secret));
        }
        let tokens = inner.exchange(&fields, None).await?;
        write_tokens(&inner.path, &tokens).await?;
        let state = authorized_state(&tokens);
        *inner.tokens.lock().await = Some(tokens);
        inner.announce(state);
        Ok(())
    }
}

fn authorized_state(tokens: &StoredTokens) -> GrantState {
    GrantState::Authorized {
        expires_at: tokens.expires_at.map(Timestamp),
        scopes: tokens.scopes.clone(),
        account: tokens.account.clone(),
    }
}

#[async_trait::async_trait]
impl Grant for GrantHandle {
    fn spec(&self) -> &GrantSpec {
        &self.inner.spec
    }

    async fn state(&self) -> GrantState {
        if let ClientCredentials::Missing { env } = &self.inner.client {
            return GrantState::MissingSecret { env: env.clone() };
        }
        match self.inner.tokens.lock().await.as_ref() {
            None => GrantState::Unauthorized,
            Some(tokens) => authorized_state(tokens),
        }
    }

    async fn access_token(&self) -> Result<AccessToken, SeamError> {
        let inner = &self.inner;
        let (client_id, client_secret) = inner.client.ready(&inner.spec.id)?;
        let mut guard = inner.tokens.lock().await;
        let Some(tokens) = guard.as_ref() else {
            return Err(SeamError::Unauthorized(format!(
                "grant `{}` has not been authorized; run `inseam authorize {}`",
                inner.spec.id, inner.spec.id
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
                inner.spec.id, inner.spec.id
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
        let refreshed = inner.exchange(&fields, guard.as_ref()).await?;
        write_tokens(&inner.path, &refreshed).await?;
        let token = AccessToken::new(refreshed.access_token.clone());
        *guard = Some(refreshed);
        Ok(token)
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
        let was_authorized = guard.take().is_some();
        drop(guard);
        if was_authorized {
            self.inner.announce(GrantState::Unauthorized);
        }
        Ok(())
    }
}

impl GrantInner {
    /// One token-endpoint call: form in, tokens out.
    async fn exchange(
        &self,
        fields: &[(&str, &str)],
        previous: Option<&StoredTokens>,
    ) -> Result<StoredTokens, SeamError> {
        let response = self
            .settings
            .http
            .post(&self.spec.token_url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(flow::form_body(fields))
            .send()
            .await
            .map_err(|e| SeamError::failed(format!("token endpoint {}: {e}", self.spec.token_url)))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.map_err(|e| {
            SeamError::failed(format!(
                "token endpoint {} answered {status} with a non-JSON body: {e}",
                self.spec.token_url
            ))
        })?;
        let now = self.settings.clock.now().0;
        parse_token_response(&body, now, previous, &self.spec.scopes)
    }

    fn announce(&self, state: GrantState) {
        self.settings.bus.emit(&GrantChanged {
            grant: self.spec.id.clone(),
            state,
        });
    }
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
pub(super) async fn write_tokens(path: &Path, tokens: &StoredTokens) -> Result<(), SeamError> {
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
