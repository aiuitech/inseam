//! The `oauth` plugin: the seam's provider — every OAuth 2.0 grant this node
//! holds (`design/connections.md`). Grants arrive two ways and live in one
//! registry: configured on this entry in the composition, or registered by
//! a connection plugin that knows its provider (the Google connection
//! brings Google's endpoints and scopes). The flow is the authorization-code
//! grant with PKCE (RFC 6749, 7636): the owner is sent to the provider and
//! the browser comes back either to a loopback listener this plugin runs
//! (RFC 8252 — `inseam authorize`, the native app) or to a redirect URI a
//! remote transport serves and hands back (`inseam serve`'s callback route
//! for the web console). The code is exchanged for tokens kept in a private
//! credential file under the node's data directory; access tokens refresh
//! themselves ahead of expiry; consumers only ever ask the handle for a live
//! token, and learn of authorizations through the `GrantChanged` event.
//!
//! The client's own credentials (client id, optional secret) are
//! environment variables the grant names, never composition values — the
//! one secrets rule the node has (`design/composition.md`). A grant whose
//! variables are unset is held in a `MissingSecret` state so the other
//! grants keep working and status surfaces can ask for exactly what is
//! missing; `Plugin::secrets()` declares every configured variable with its
//! reason (a registering plugin declares its own).

mod attempt;
mod flow;
mod grant;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use inseam_kernel::address::Timestamp;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, SecretNeed,
};
use inseam_seams::oauth::{
    AuthorizationCallback, AuthorizationStarted, Grant, GrantDisposer, GrantId, GrantSpec,
    OAuth, Redirect, OAUTH,
};
use inseam_seams::SeamError;

use attempt::{Attempt, ATTEMPTS_MAX};
use grant::{ClientCredentials, GrantHandle, Settings};

pub use grant::{Clock, SystemClock};

/// Subdirectory of the data dir holding credential files when the config
/// names none.
pub const CREDENTIALS_DIRNAME: &str = "oauth";

/// Grants one entry may configure; a node authorizes a handful of
/// providers, never hundreds. Registered grants count against the same
/// ceiling.
pub const GRANTS_MAX: usize = 64;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuthConfig {
    /// Loopback port the authorization redirect lands on; register
    /// `http://127.0.0.1:<callback_port>/callback` with each provider. `0`
    /// lets the OS pick a port per attempt — only for providers that accept
    /// any loopback port (Google's desktop clients do).
    pub callback_port: u16,
    /// How long an authorization may take: the listener, the waiter, and a
    /// remote transport's pending attempt all give up after this.
    pub authorization_timeout_secs: u64,
    /// Where credential files live; `<data-dir>/oauth` when unset. Owner-
    /// private (0700 / 0600), never synced, never in the store.
    pub credentials_dir: Option<PathBuf>,
    /// Generic grants the owner defines here; connection plugins register
    /// their own.
    pub grants: Vec<GrantSpec>,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            callback_port: 47781,
            authorization_timeout_secs: 300,
            credentials_dir: None,
            grants: Vec::new(),
        }
    }
}

/// One provider account the node may be authorized against — the
/// composition's name for a [`GrantSpec`].
pub type GrantConfig = GrantSpec;

pub struct OAuthPlugin {
    config: OAuthConfig,
}

impl OAuthPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: OAuthConfig = parse_config(config)?;
        validate(&config)?;
        Ok(Self { config })
    }
}

/// Parse, don't validate — but a config can be well-typed and still name
/// two grants the same or a token URL that is not one; those are refused
/// at build, before any fiber runs.
fn validate(config: &OAuthConfig) -> Result<(), PluginError> {
    if config.grants.len() > GRANTS_MAX {
        return Err(PluginError(format!(
            "config: {} grants configured; at most {GRANTS_MAX} are supported",
            config.grants.len()
        )));
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for grant in &config.grants {
        if !seen.insert(grant.id.as_str()) {
            return Err(PluginError(format!("config: grant `{}` is configured twice", grant.id)));
        }
        validate_spec(grant).map_err(|e| PluginError(format!("config: {e}")))?;
    }
    Ok(())
}

/// What makes a grant definition usable: http(s) endpoints and a named
/// client id variable. Shared by the composition door and the registry door.
pub fn validate_spec(spec: &GrantSpec) -> Result<(), SeamError> {
    for (field, value) in [
        ("authorization_url", &spec.authorization_url),
        ("token_url", &spec.token_url),
    ] {
        let url = url::Url::parse(value)
            .map_err(|e| SeamError::failed(format!("grant `{}` {field}: {e}", spec.id)))?;
        if url.scheme() != "https" && url.scheme() != "http" {
            return Err(SeamError::failed(format!(
                "grant `{}` {field} must be an http(s) URL",
                spec.id
            )));
        }
    }
    if spec.client_id_env.trim().is_empty() {
        return Err(SeamError::failed(format!(
            "grant `{}` names no client_id_env",
            spec.id
        )));
    }
    Ok(())
}

pub struct OAuthFactory;

impl inseam_kernel::substrate::PluginFactory for OAuthFactory {
    fn name(&self) -> &str {
        "oauth"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(OAuthPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for OAuthPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "oauth",
            inject: INJECT,
            provides: &["oauth"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let dir = self
            .config
            .credentials_dir
            .clone()
            .unwrap_or_else(|| cx.data_dir().join(CREDENTIALS_DIRNAME));
        let settings = Arc::new(Settings {
            callback_port: self.config.callback_port,
            authorization_timeout: Duration::from_secs(self.config.authorization_timeout_secs),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| PluginError(format!("http client: {e}")))?,
            clock: Arc::new(SystemClock),
            bus: cx.bus().clone(),
        });
        let service = Service::load(&self.config.grants, dir, settings).await;
        cx.provide(&OAUTH, Arc::new(service) as Arc<dyn OAuth>, Facts::new())?;
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretNeed> {
        self.config
            .grants
            .iter()
            .flat_map(|grant| secret_needs(grant, self.config.callback_port))
            .collect()
    }
}

/// The owner-facing needs one grant definition implies: its client id
/// variable, and its secret variable when it has one. Shared with plugins
/// that register a grant, so every grant explains itself the same way.
pub fn secret_needs(spec: &GrantSpec, callback_port: u16) -> Vec<SecretNeed> {
    let provider = spec.provider();
    let id = SecretNeed {
        env: spec.client_id_env.clone(),
        purpose: format!(
            "The OAuth client id for grant `{}` — the app identity inseam signs in to \
             {provider} as. Create one in the provider's developer console with \
             `http://127.0.0.1:{callback_port}/callback` as a redirect URI (and your node's \
             `/api/v1/oauth/callback` URL when you authorize from the web console).",
            spec.id
        ),
    };
    let secret = spec.client_secret_env.clone().map(|env| SecretNeed {
        env,
        purpose: format!(
            "The OAuth client secret issued with the client id for grant `{}` ({provider}).",
            spec.id
        ),
    });
    std::iter::once(id).chain(secret).collect()
}

/// The seam provider: the registry of grants and the authorizations in
/// flight.
pub struct Service {
    inner: Arc<ServiceInner>,
}

struct ServiceInner {
    settings: Arc<Settings>,
    credentials_dir: PathBuf,
    grants: RwLock<BTreeMap<GrantId, GrantHandle>>,
    attempts: Mutex<Vec<Arc<Attempt>>>,
}

impl Service {
    /// Load the configured grants; registered ones arrive through
    /// [`OAuth::register`].
    pub async fn load(configs: &[GrantSpec], dir: PathBuf, settings: Arc<Settings>) -> Self {
        let mut grants = BTreeMap::new();
        for spec in configs {
            let handle = load_handle(spec, &dir, &settings).await;
            grants.insert(spec.id.clone(), handle);
        }
        Self {
            inner: Arc::new(ServiceInner {
                settings,
                credentials_dir: dir,
                grants: RwLock::new(grants),
                attempts: Mutex::new(Vec::new()),
            }),
        }
    }

    fn handle(&self, id: &GrantId) -> Result<GrantHandle, SeamError> {
        let grants = self.inner.grants.read().unwrap_or_else(|e| e.into_inner());
        grants.get(id).cloned().ok_or_else(|| {
            let known: Vec<String> = grants.keys().map(ToString::to_string).collect();
            SeamError::Unavailable(format!(
                "no grant `{id}` is configured; known grants: {}",
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            ))
        })
    }

    fn attempt(&self, state: &str) -> Result<Arc<Attempt>, SeamError> {
        let attempts = self.inner.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts
            .iter()
            .find(|a| a.state == state)
            .cloned()
            .ok_or_else(|| {
                SeamError::Refused(
                    "no authorization attempt matches this state; start again".to_string(),
                )
            })
    }

    /// Make room for a new attempt: drop expired ones, refuse when the
    /// ceiling is still reached.
    fn admit(&self, attempt: Arc<Attempt>) -> Result<(), SeamError> {
        let now = self.inner.settings.clock.now();
        let mut attempts = self.inner.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.retain(|a| !a.is_expired(now));
        if attempts.len() >= ATTEMPTS_MAX {
            return Err(SeamError::Refused(format!(
                "{ATTEMPTS_MAX} authorizations are already in flight; finish or wait out one first"
            )));
        }
        attempts.push(attempt);
        Ok(())
    }
}

async fn load_handle(spec: &GrantSpec, dir: &Path, settings: &Arc<Settings>) -> GrantHandle {
    let client = ClientCredentials::from_environment(spec);
    let path = dir.join(format!("{}.json", spec.id));
    GrantHandle::load(spec.clone(), client, path, Arc::clone(settings)).await
}

#[async_trait::async_trait]
impl OAuth for Service {
    fn grants(&self) -> Vec<Arc<dyn Grant>> {
        self.inner
            .grants
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|h| Arc::new(h.clone()) as Arc<dyn Grant>)
            .collect()
    }

    async fn register(&self, spec: GrantSpec) -> Result<(Arc<dyn Grant>, GrantDisposer), SeamError> {
        validate_spec(&spec)?;
        let handle = load_handle(&spec, &self.inner.credentials_dir, &self.inner.settings).await;
        let id = spec.id.clone();
        {
            let mut grants = self.inner.grants.write().unwrap_or_else(|e| e.into_inner());
            if grants.contains_key(&id) {
                return Err(SeamError::Refused(format!(
                    "grant `{id}` already exists; a plugin cannot register a second grant under a configured or registered id"
                )));
            }
            if grants.len() >= GRANTS_MAX {
                return Err(SeamError::Refused(format!(
                    "{GRANTS_MAX} grants are held already; at most {GRANTS_MAX} are supported"
                )));
            }
            grants.insert(id.clone(), handle.clone());
        }
        // The disposer holds the registry weakly: a grant being unwound after
        // the whole service is gone (full teardown) is a no-op.
        let weak = Arc::downgrade(&self.inner);
        let disposer: GrantDisposer = Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .grants
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
            }
        });
        Ok((Arc::new(handle) as Arc<dyn Grant>, disposer))
    }

    async fn authorize(
        &self,
        grant: &GrantId,
        redirect: Redirect,
    ) -> Result<AuthorizationStarted, SeamError> {
        let handle = self.handle(grant)?;
        let (listener, redirect_uri) = match redirect {
            Redirect::Loopback => {
                let listener = bind_loopback(self.inner.settings.callback_port).await?;
                let port = listener
                    .local_addr()
                    .map_err(|e| SeamError::failed(format!("loopback listener has no address: {e}")))?
                    .port();
                (Some(listener), format!("http://127.0.0.1:{port}/callback"))
            }
            Redirect::External { redirect_uri } => {
                validate_redirect_uri(&redirect_uri)?;
                (None, redirect_uri)
            }
        };
        let begun = handle.begin(&redirect_uri)?;
        let timeout = self.inner.settings.authorization_timeout;
        let deadline = Timestamp(
            self.inner
                .settings
                .clock
                .now()
                .0
                .saturating_add(i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX)),
        );
        let attempt = Arc::new(Attempt::new(
            begun.state.clone(),
            handle,
            redirect_uri.clone(),
            begun.verifier,
            deadline,
        ));
        self.admit(Arc::clone(&attempt))?;
        if let Some(listener) = listener {
            tokio::spawn(attempt::serve_loopback(attempt, listener, timeout));
        }
        Ok(AuthorizationStarted {
            grant: grant.clone(),
            url: begun.url,
            state: begun.state,
            redirect_uri,
        })
    }

    async fn await_authorization(&self, state: &str) -> Result<GrantId, SeamError> {
        let attempt = self.attempt(state)?;
        // A second of slack past the attempt's own timeout, so the listener's
        // verdict — not the waiter's — is what the owner reads.
        let timeout = self.inner.settings.authorization_timeout + Duration::from_secs(1);
        let outcome = attempt::wait(attempt.subscribe(), timeout).await?;
        outcome.into_result(attempt.grant.id())
    }

    async fn complete_authorization(
        &self,
        callback: AuthorizationCallback,
    ) -> Result<GrantId, SeamError> {
        let Some(state) = callback.state.as_deref() else {
            return Err(SeamError::Refused(
                "authorization redirect carried no state".to_string(),
            ));
        };
        let attempt = self.attempt(state)?;
        if let Some(outcome) = attempt.outcome() {
            return outcome.into_result(attempt.grant.id());
        }
        let code = match attempt.accept(&callback) {
            Ok(code) => code,
            Err(e) => {
                attempt.resolve(attempt::Outcome::Refused(e.to_string()));
                return Err(e);
            }
        };
        attempt.finish(&code).await.into_result(attempt.grant.id())
    }
}

async fn bind_loopback(port: u16) -> Result<TcpListener, SeamError> {
    TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        SeamError::failed(format!(
            "cannot listen on 127.0.0.1:{port} for the authorization redirect: {e}"
        ))
    })
}

/// A transport-served redirect must be an absolute http(s) URL — it is what
/// the provider compares against the client's registered URIs.
fn validate_redirect_uri(uri: &str) -> Result<(), SeamError> {
    let parsed = url::Url::parse(uri)
        .map_err(|e| SeamError::failed(format!("redirect_uri `{uri}`: {e}")))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(SeamError::failed(format!(
            "redirect_uri `{uri}` must be an http(s) URL"
        )));
    }
    if parsed.host_str().is_none() {
        return Err(SeamError::failed(format!("redirect_uri `{uri}` has no host")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    use inseam_kernel::substrate::EventBus;
    use inseam_seams::oauth::{GrantChanged, GrantState};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct FixedClock(AtomicI64);

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp(self.0.load(Ordering::Relaxed))
        }
    }

    /// A token endpoint that answers each request with the next canned
    /// body and records what it was sent.
    struct FakeTokenServer {
        url: String,
        bodies: Arc<Mutex<Vec<String>>>,
    }

    async fn fake_token_server(replies: Vec<serde_json::Value>) -> FakeTokenServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("binds");
        let url = format!("http://{}/token", listener.local_addr().expect("addr"));
        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let recorded = Arc::clone(&bodies);
        tokio::spawn(async move {
            for reply in replies {
                let (mut stream, _) = listener.accept().await.expect("accepts");
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.expect("reads");
                    raw.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length: usize = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().expect("length")))
                            .unwrap_or(0);
                        if body.len() >= length || n == 0 {
                            recorded.lock().expect("lock").push(body.to_string());
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let json = reply.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                stream.write_all(response.as_bytes()).await.expect("writes");
                stream.shutdown().await.expect("closes");
            }
        });
        FakeTokenServer { url, bodies }
    }

    fn spec(token_url: &str) -> GrantSpec {
        GrantSpec {
            id: GrantId::new("test").expect("valid"),
            authorization_url: "https://auth.example.com/authorize".to_string(),
            token_url: token_url.to_string(),
            scopes: vec!["read".to_string()],
            client_id_env: "INSEAM_TEST_OAUTH_CLIENT_ID_NEVER_SET".to_string(),
            client_secret_env: None,
            authorization_params: BTreeMap::new(),
        }
    }

    fn settings(clock: Arc<dyn Clock>, bus: EventBus) -> Arc<Settings> {
        Arc::new(Settings {
            callback_port: 0,
            authorization_timeout: Duration::from_secs(10),
            http: reqwest::Client::new(),
            clock,
            bus,
        })
    }

    /// A service holding one ready grant `test` over `dir`.
    async fn ready_service(dir: &Path, token_url: &str, clock: Arc<dyn Clock>, bus: EventBus) -> Service {
        let settings = settings(clock, bus);
        let spec = spec(token_url);
        let handle = GrantHandle::load(
            spec.clone(),
            ClientCredentials::Ready {
                id: "client-1".to_string(),
                secret: Some("s3".to_string()),
            },
            dir.join("test.json"),
            Arc::clone(&settings),
        )
        .await;
        let mut grants = BTreeMap::new();
        grants.insert(spec.id.clone(), handle);
        Service {
            inner: Arc::new(ServiceInner {
                settings,
                credentials_dir: dir.to_path_buf(),
                grants: RwLock::new(grants),
                attempts: Mutex::new(Vec::new()),
            }),
        }
    }

    fn test_grant() -> GrantId {
        GrantId::new("test").expect("valid")
    }

    fn query_pairs(url: &str) -> BTreeMap<String, String> {
        url::Url::parse(url)
            .expect("authorization url parses")
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// Simulate the browser: follow the state in the authorization URL back
    /// to the loopback listener with a code.
    async fn browser_returns(url: &str, code: &str, state_override: Option<&str>) {
        let pairs = query_pairs(url);
        let state = state_override.unwrap_or(&pairs["state"]);
        let redirect = format!("{}?code={code}&state={state}", pairs["redirect_uri"]);
        let _ = reqwest::Client::new().get(redirect).send().await;
    }

    fn fake_id_token(email: &str) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({"email": email}).to_string());
        format!("{header}.{payload}.sig")
    }

    #[tokio::test]
    async fn loopback_authorization_round_trips_and_persists_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(vec![serde_json::json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600, "token_type": "Bearer",
            "id_token": fake_id_token("greg@example.com")
        })])
        .await;
        let bus = EventBus::new();
        let seen: Arc<Mutex<Vec<GrantChanged>>> = Arc::default();
        let record = Arc::clone(&seen);
        let _sub = bus.on::<GrantChanged>(move |e| record.lock().expect("lock").push(e.clone()));
        let clock = Arc::new(FixedClock(AtomicI64::new(1_000)));
        let service = ready_service(dir.path(), &server.url, clock, bus).await;
        let grant = service.grant(&test_grant()).expect("held");
        assert_eq!(grant.state().await, GrantState::Unauthorized);
        assert!(matches!(grant.access_token().await, Err(SeamError::Unauthorized(_))));

        let started = service.authorize(&test_grant(), Redirect::Loopback).await.expect("begins");
        assert!(started.redirect_uri.starts_with("http://127.0.0.1:"));
        assert_eq!(query_pairs(&started.url)["state"], started.state);
        let url = started.url.clone();
        tokio::spawn(async move { browser_returns(&url, "code-xyz", None).await });
        let done = service.await_authorization(&started.state).await.expect("completes");
        assert_eq!(done, test_grant());

        let sent = server.bodies.lock().expect("lock").clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("grant_type=authorization_code"));
        assert!(sent[0].contains("code=code-xyz"));
        assert!(sent[0].contains("code_verifier="));
        assert!(sent[0].contains("client_secret=s3"));

        assert_eq!(grant.access_token().await.expect("token").secret(), "at-1");
        let state = grant.state().await;
        assert_eq!(
            state,
            GrantState::Authorized {
                expires_at: Some(Timestamp(4_600)),
                scopes: vec!["read".to_string()],
                account: Some("greg@example.com".to_string()),
            }
        );
        let events = seen.lock().expect("lock").clone();
        assert_eq!(events.len(), 1, "authorization is announced once");
        assert_eq!(events[0].grant, test_grant());
        assert_eq!(events[0].state, state);

        let file = dir.path().join("test.json");
        assert!(file.exists(), "tokens persisted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credential file is owner-private");
        }

        // A fresh service over the same file is authorized without a flow.
        let reloaded = ready_service(dir.path(), &server.url, Arc::new(FixedClock(AtomicI64::new(1_000))), EventBus::new()).await;
        let grant = reloaded.grant(&test_grant()).expect("held");
        assert_eq!(grant.access_token().await.expect("token").secret(), "at-1");
        assert_eq!(grant.state().await.account(), Some("greg@example.com"));
    }

    #[tokio::test]
    async fn external_redirect_completes_through_the_service() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(vec![serde_json::json!({
            "access_token": "at-ext", "refresh_token": "rt", "expires_in": 100
        })])
        .await;
        let service = ready_service(dir.path(), &server.url, Arc::new(FixedClock(AtomicI64::new(0))), EventBus::new()).await;
        let started = service
            .authorize(
                &test_grant(),
                Redirect::External {
                    redirect_uri: "https://node.example/api/v1/oauth/callback".to_string(),
                },
            )
            .await
            .expect("begins");
        let pairs = query_pairs(&started.url);
        assert_eq!(pairs["redirect_uri"], "https://node.example/api/v1/oauth/callback");

        // Nobody listens locally: the transport brings the parameters back.
        let callback = AuthorizationCallback {
            state: Some(started.state.clone()),
            code: Some("code-1".to_string()),
            ..AuthorizationCallback::default()
        };
        let done = service.complete_authorization(callback.clone()).await.expect("completes");
        assert_eq!(done, test_grant());
        let sent = server.bodies.lock().expect("lock").clone();
        assert!(sent[0].contains("redirect_uri=https%3A%2F%2Fnode.example%2Fapi%2Fv1%2Foauth%2Fcallback"));
        assert_eq!(
            service.grant(&test_grant()).expect("held").access_token().await.expect("token").secret(),
            "at-ext"
        );
        // A waiter sees the same outcome, and a replayed callback is the
        // recorded outcome, not a second exchange.
        assert_eq!(service.await_authorization(&started.state).await.expect("settled"), test_grant());
        assert_eq!(service.complete_authorization(callback).await.expect("idempotent"), test_grant());
        assert_eq!(server.bodies.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn a_redirect_with_the_wrong_state_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(Vec::new()).await;
        let service = ready_service(dir.path(), &server.url, Arc::new(FixedClock(AtomicI64::new(0))), EventBus::new()).await;
        let started = service.authorize(&test_grant(), Redirect::Loopback).await.expect("begins");
        let url = started.url.clone();
        tokio::spawn(async move { browser_returns(&url, "code", Some("forged")).await });
        assert!(matches!(
            service.await_authorization(&started.state).await,
            Err(SeamError::Refused(_))
        ));
        assert_eq!(service.grant(&test_grant()).expect("held").state().await, GrantState::Unauthorized);
        assert!(!dir.path().join("test.json").exists());

        // Unknown states and provider errors are refusals on the external
        // path too.
        assert!(matches!(
            service.complete_authorization(AuthorizationCallback { state: Some("nope".into()), ..Default::default() }).await,
            Err(SeamError::Refused(_))
        ));
        let started = service
            .authorize(&test_grant(), Redirect::External { redirect_uri: "http://127.0.0.1:9/cb".into() })
            .await
            .expect("begins");
        let denied = service
            .complete_authorization(AuthorizationCallback {
                state: Some(started.state.clone()),
                error: Some("access_denied".into()),
                ..Default::default()
            })
            .await;
        assert!(matches!(denied, Err(SeamError::Refused(_))));
        assert!(matches!(service.await_authorization(&started.state).await, Err(SeamError::Refused(_))));
    }

    #[tokio::test]
    async fn authorization_refuses_bad_redirect_uris_and_unknown_grants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = ready_service(dir.path(), "http://127.0.0.1:1/token", Arc::new(FixedClock(AtomicI64::new(0))), EventBus::new()).await;
        assert!(service
            .authorize(&test_grant(), Redirect::External { redirect_uri: "ftp://x/cb".into() })
            .await
            .is_err());
        assert!(service
            .authorize(&test_grant(), Redirect::External { redirect_uri: "/relative".into() })
            .await
            .is_err());
        assert!(matches!(
            service.authorize(&GrantId::new("nope").expect("valid"), Redirect::Loopback).await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_and_the_file_updated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(vec![serde_json::json!({
            "access_token": "at-2", "expires_in": 100
        })])
        .await;
        let stale = flow::StoredTokens {
            version: flow::TOKENS_VERSION,
            access_token: "at-1".to_string(),
            refresh_token: Some("rt-1".to_string()),
            expires_at: Some(1_030),
            scopes: vec!["read".to_string()],
            token_type: "Bearer".to_string(),
            account: Some("greg@example.com".to_string()),
        };
        std::fs::write(dir.path().join("test.json"), serde_json::to_vec(&stale).expect("json")).expect("write");
        let clock = Arc::new(FixedClock(AtomicI64::new(1_000)));
        let service = ready_service(dir.path(), &server.url, clock, EventBus::new()).await;
        let grant = service.grant(&test_grant()).expect("held");

        // 30 seconds to expiry is inside the skew: refresh.
        assert_eq!(grant.access_token().await.expect("token").secret(), "at-2");
        let sent = server.bodies.lock().expect("lock").clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("grant_type=refresh_token"));
        assert!(sent[0].contains("refresh_token=rt-1"));
        // Second call: fresh token, no second exchange.
        assert_eq!(grant.access_token().await.expect("token").secret(), "at-2");
        assert_eq!(server.bodies.lock().expect("lock").len(), 1);

        let on_disk: flow::StoredTokens =
            serde_json::from_slice(&std::fs::read(dir.path().join("test.json")).expect("read")).expect("parses");
        assert_eq!(on_disk.access_token, "at-2");
        assert_eq!(on_disk.refresh_token.as_deref(), Some("rt-1"), "old refresh token kept");
        assert_eq!(on_disk.account.as_deref(), Some("greg@example.com"), "account kept");
        assert_eq!(on_disk.expires_at, Some(1_100));
    }

    #[tokio::test]
    async fn revoke_forgets_the_tokens_and_announces_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = flow::StoredTokens {
            version: flow::TOKENS_VERSION,
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: Vec::new(),
            token_type: "Bearer".to_string(),
            account: None,
        };
        std::fs::write(dir.path().join("test.json"), serde_json::to_vec(&good).expect("json")).expect("write");
        let bus = EventBus::new();
        let seen: Arc<Mutex<Vec<GrantChanged>>> = Arc::default();
        let record = Arc::clone(&seen);
        let _sub = bus.on::<GrantChanged>(move |e| record.lock().expect("lock").push(e.clone()));
        let service = ready_service(dir.path(), "http://127.0.0.1:1/token", Arc::new(FixedClock(AtomicI64::new(0))), bus).await;
        let grant = service.grant(&test_grant()).expect("held");
        assert!(matches!(grant.state().await, GrantState::Authorized { .. }));
        grant.revoke().await.expect("revokes");
        assert_eq!(grant.state().await, GrantState::Unauthorized);
        assert!(!dir.path().join("test.json").exists());
        grant.revoke().await.expect("revoking twice is fine");
        let events = seen.lock().expect("lock").clone();
        assert_eq!(events.len(), 1, "only the first revoke changes anything");
        assert_eq!(events[0].state, GrantState::Unauthorized);
    }

    #[tokio::test]
    async fn missing_client_secret_is_reported_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = Service::load(
            &[spec("http://127.0.0.1:1/token")],
            dir.path().to_path_buf(),
            settings(Arc::new(FixedClock(AtomicI64::new(0))), EventBus::new()),
        )
        .await;
        let grant = service.grant(&test_grant()).expect("held");
        assert_eq!(
            grant.state().await,
            GrantState::MissingSecret { env: "INSEAM_TEST_OAUTH_CLIENT_ID_NEVER_SET".to_string() }
        );
        assert!(matches!(grant.access_token().await, Err(SeamError::Unavailable(_))));
        assert!(matches!(
            service.authorize(&test_grant(), Redirect::Loopback).await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn registered_grants_join_the_registry_and_leave_with_their_disposer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = Service::load(
            &[spec("http://127.0.0.1:1/token")],
            dir.path().to_path_buf(),
            settings(Arc::new(FixedClock(AtomicI64::new(0))), EventBus::new()),
        )
        .await;
        let mut google = spec("http://127.0.0.1:1/token");
        google.id = GrantId::new("google").expect("valid");
        let (grant, dispose) = service.register(google.clone()).await.expect("registers");
        assert_eq!(grant.id().as_str(), "google");
        let ids: Vec<String> = service.grants().iter().map(|g| g.id().to_string()).collect();
        assert_eq!(ids, vec!["google", "test"], "ordered by id");

        // One grant per id, whichever door it came through.
        assert!(matches!(service.register(google).await, Err(SeamError::Refused(_))));
        assert!(matches!(service.register(spec("http://127.0.0.1:1/token")).await, Err(SeamError::Refused(_))));
        let mut bad = spec("http://127.0.0.1:1/token");
        bad.id = GrantId::new("bad").expect("valid");
        bad.token_url = "ftp://nope".to_string();
        assert!(service.register(bad).await.is_err());

        dispose();
        let ids: Vec<String> = service.grants().iter().map(|g| g.id().to_string()).collect();
        assert_eq!(ids, vec!["test"]);
    }

    #[test]
    fn config_rejects_duplicate_grants_and_non_http_urls() {
        let duplicate: toml::Table = toml::from_str(
            r#"
[[grants]]
id = "g"
authorization_url = "https://a/x"
token_url = "https://a/t"
client_id_env = "E"
[[grants]]
id = "g"
authorization_url = "https://a/x"
token_url = "https://a/t"
client_id_env = "E"
"#,
        )
        .expect("toml");
        assert!(OAuthPlugin::from_config(&duplicate).is_err());

        let bad_url: toml::Table = toml::from_str(
            r#"
[[grants]]
id = "g"
authorization_url = "ftp://a/x"
token_url = "https://a/t"
client_id_env = "E"
"#,
        )
        .expect("toml");
        assert!(OAuthPlugin::from_config(&bad_url).is_err());

        let empty: toml::Table = toml::from_str("").expect("toml");
        assert!(OAuthPlugin::from_config(&empty).is_ok(), "no grants is a valid, idle provider");
    }

    #[test]
    fn secrets_name_every_client_variable_with_a_reason() {
        let config: toml::Table = toml::from_str(
            r#"
[[grants]]
id = "google"
authorization_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
client_id_env = "GOOGLE_CLIENT_ID"
client_secret_env = "GOOGLE_CLIENT_SECRET"
"#,
        )
        .expect("toml");
        let plugin = OAuthPlugin::from_config(&config).expect("builds");
        let needs = plugin.secrets();
        let envs: Vec<&str> = needs.iter().map(|n| n.env.as_str()).collect();
        assert_eq!(envs, vec!["GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"]);
        assert!(needs[0].purpose.contains("accounts.google.com"));
        assert!(needs[0].purpose.contains("47781/callback"));
    }
}
