//! The `oauth` plugin: the seam's provider — every OAuth 2.0 grant this node
//! holds, configured once in the composition and consumed by any host
//! connection that names one (`design/connections.md`). The flow is the
//! authorization-code grant with PKCE and a loopback redirect (RFC 6749,
//! 7636, 8252): `inseam authorize <grant>` prints the provider's URL, the
//! owner signs in, the browser lands on `127.0.0.1:<callback_port>/callback`,
//! and the code is exchanged for tokens kept in a private credential file
//! under the node's data directory. Access tokens refresh themselves ahead
//! of expiry; consumers only ever ask the handle for a live token.
//!
//! The client's own credentials (client id, optional secret) are
//! environment variables the grant names, never composition values — the
//! one secrets rule the node has (`design/composition.md`). A grant whose
//! variables are unset is mounted in a `MissingSecret` state so the other
//! grants keep working and status surfaces can ask for exactly what is
//! missing; `Plugin::secrets()` declares every variable with its reason.

mod flow;
mod grant;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, SecretNeed,
};
use inseam_seams::oauth::{Grant, GrantId, OAuth, OAUTH};

pub use grant::{Clock, SystemClock};

/// Subdirectory of the data dir holding credential files when the config
/// names none.
pub const CREDENTIALS_DIRNAME: &str = "oauth";

/// Grants one entry may configure; a node authorizes a handful of
/// providers, never hundreds.
pub const GRANTS_MAX: usize = 64;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuthConfig {
    /// Loopback port the authorization redirect lands on; register
    /// `http://127.0.0.1:<callback_port>/callback` with each provider. `0`
    /// lets the OS pick a port per attempt — only for providers that accept
    /// any loopback port (Google's desktop clients do).
    pub callback_port: u16,
    /// How long `inseam authorize` waits for the browser to come back.
    pub authorization_timeout_secs: u64,
    /// Where credential files live; `<data-dir>/oauth` when unset. Owner-
    /// private (0700 / 0600), never synced, never in the store.
    pub credentials_dir: Option<PathBuf>,
    pub grants: Vec<GrantConfig>,
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

/// One provider account the node may be authorized against.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    /// What consumers name (`grant = "google"`); also the credential file's
    /// name.
    pub id: GrantId,
    pub authorization_url: String,
    pub token_url: String,
    /// The scopes the owner is willing to grant; consumers check the ones
    /// they need against this list at apply time.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Environment variable holding the OAuth client id.
    pub client_id_env: String,
    /// Environment variable holding the client secret; absent for public
    /// (PKCE-only) clients.
    #[serde(default)]
    pub client_secret_env: Option<String>,
    /// Extra parameters on the authorization request — Google needs
    /// `access_type = "offline"` and `prompt = "consent"` to issue a
    /// refresh token.
    #[serde(default)]
    pub authorization_params: BTreeMap<String, String>,
}

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
        for (field, value) in [
            ("authorization_url", &grant.authorization_url),
            ("token_url", &grant.token_url),
        ] {
            let url = url::Url::parse(value)
                .map_err(|e| PluginError(format!("config: grant `{}` {field}: {e}", grant.id)))?;
            if url.scheme() != "https" && url.scheme() != "http" {
                return Err(PluginError(format!(
                    "config: grant `{}` {field} must be an http(s) URL",
                    grant.id
                )));
            }
        }
        if grant.client_id_env.trim().is_empty() {
            return Err(PluginError(format!(
                "config: grant `{}` names no client_id_env",
                grant.id
            )));
        }
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
        let settings = Arc::new(grant::Settings {
            callback_port: self.config.callback_port,
            authorization_timeout: Duration::from_secs(self.config.authorization_timeout_secs),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|e| PluginError(format!("http client: {e}")))?,
            clock: Arc::new(SystemClock),
        });
        let service = Service::load(&self.config.grants, &dir, settings).await;
        cx.provide(&OAUTH, Arc::new(service) as Arc<dyn OAuth>, Facts::new())?;
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretNeed> {
        self.config
            .grants
            .iter()
            .flat_map(|grant| {
                let provider = provider_host(&grant.authorization_url);
                let id = [(
                    grant.client_id_env.clone(),
                    format!(
                        "The OAuth client id for grant `{}` — the app identity inseam signs \
                         in to {provider} as. Create one in the provider's developer console \
                         with `http://127.0.0.1:{}/callback` as a redirect URI.",
                        grant.id, self.config.callback_port
                    ),
                )];
                let secret = grant.client_secret_env.clone().map(|env| {
                    (
                        env,
                        format!(
                            "The OAuth client secret issued with the client id for grant `{}` \
                             ({provider}).",
                            grant.id
                        ),
                    )
                });
                id.into_iter().chain(secret)
            })
            .map(|(env, purpose)| SecretNeed { env, purpose })
            .collect()
    }
}

/// The host part of a provider URL, for owner-facing prose.
fn provider_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// The seam provider: the configured grants, loaded once at apply.
struct Service {
    grants: Vec<Arc<dyn Grant>>,
}

impl Service {
    async fn load(
        configs: &[GrantConfig],
        dir: &std::path::Path,
        settings: Arc<grant::Settings>,
    ) -> Self {
        let mut grants: Vec<Arc<dyn Grant>> = Vec::with_capacity(configs.len());
        for config in configs {
            let client = client_credentials(config);
            let path = dir.join(format!("{}.json", config.id));
            let handle =
                grant::GrantHandle::load(config.clone(), client, path, Arc::clone(&settings)).await;
            grants.push(Arc::new(handle));
        }
        grants.sort_by(|a, b| a.id().cmp(b.id()));
        Self { grants }
    }
}

/// Read the client's credentials from the environment the grant names.
fn client_credentials(config: &GrantConfig) -> grant::ClientCredentials {
    let present = |env: &str| std::env::var(env).ok().filter(|v| !v.trim().is_empty());
    let Some(id) = present(&config.client_id_env) else {
        return grant::ClientCredentials::Missing {
            env: config.client_id_env.clone(),
        };
    };
    let secret = match &config.client_secret_env {
        None => None,
        Some(env) => match present(env) {
            Some(secret) => Some(secret),
            None => return grant::ClientCredentials::Missing { env: env.clone() },
        },
    };
    grant::ClientCredentials::Ready { id, secret }
}

impl OAuth for Service {
    fn grants(&self) -> Vec<Arc<dyn Grant>> {
        self.grants.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Mutex;

    use inseam_kernel::address::Timestamp;
    use inseam_seams::oauth::GrantState;
    use inseam_seams::SeamError;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    fn grant_config(token_url: &str) -> GrantConfig {
        GrantConfig {
            id: GrantId::new("test").expect("valid"),
            authorization_url: "https://auth.example.com/authorize".to_string(),
            token_url: token_url.to_string(),
            scopes: vec!["read".to_string()],
            client_id_env: "X".to_string(),
            client_secret_env: None,
            authorization_params: BTreeMap::new(),
        }
    }

    async fn handle(
        dir: &std::path::Path,
        config: GrantConfig,
        clock: Arc<dyn Clock>,
    ) -> grant::GrantHandle {
        let settings = Arc::new(grant::Settings {
            callback_port: 0,
            authorization_timeout: Duration::from_secs(10),
            http: reqwest::Client::new(),
            clock,
        });
        grant::GrantHandle::load(
            config.clone(),
            grant::ClientCredentials::Ready {
                id: "client-1".to_string(),
                secret: Some("s3".to_string()),
            },
            dir.join(format!("{}.json", config.id)),
            settings,
        )
        .await
    }

    /// Simulate the browser: follow the state in the authorization URL back
    /// to the loopback listener with a code.
    async fn browser_returns(url: &str, code: &str, state_override: Option<&str>) {
        let parsed = url::Url::parse(url).expect("authorization url parses");
        let pairs: BTreeMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let state = state_override.unwrap_or(&pairs["state"]);
        let redirect = format!("{}?code={code}&state={state}", pairs["redirect_uri"]);
        let _ = reqwest::Client::new().get(redirect).send().await;
    }

    #[tokio::test]
    async fn authorize_round_trips_through_the_loopback_and_persists_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(vec![serde_json::json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600, "token_type": "Bearer"
        })])
        .await;
        let clock = Arc::new(FixedClock(AtomicI64::new(1_000)));
        let grant = handle(dir.path(), grant_config(&server.url), clock).await;
        assert_eq!(grant.state().await, GrantState::Unauthorized);
        assert!(matches!(grant.access_token().await, Err(SeamError::Unauthorized(_))));

        let pending = grant.authorize().await.expect("begins");
        let url = pending.url().to_string();
        tokio::spawn(async move { browser_returns(&url, "code-xyz", None).await });
        pending.complete().await.expect("completes");

        let sent = server.bodies.lock().expect("lock").clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("grant_type=authorization_code"));
        assert!(sent[0].contains("code=code-xyz"));
        assert!(sent[0].contains("code_verifier="));
        assert!(sent[0].contains("client_secret=s3"));

        assert_eq!(
            grant.access_token().await.expect("token").secret(),
            "at-1"
        );
        assert_eq!(
            grant.state().await,
            GrantState::Authorized {
                expires_at: Some(Timestamp(4_600)),
                scopes: vec!["read".to_string()]
            }
        );
        let file = dir.path().join("test.json");
        assert!(file.exists(), "tokens persisted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credential file is owner-private");
        }

        // A fresh handle over the same file is authorized without a flow.
        let reloaded = handle(dir.path(), grant_config(&server.url), Arc::new(FixedClock(AtomicI64::new(1_000)))).await;
        assert_eq!(reloaded.access_token().await.expect("token").secret(), "at-1");
    }

    #[tokio::test]
    async fn a_redirect_with_the_wrong_state_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(Vec::new()).await;
        let grant = handle(dir.path(), grant_config(&server.url), Arc::new(FixedClock(AtomicI64::new(0)))).await;
        let pending = grant.authorize().await.expect("begins");
        let url = pending.url().to_string();
        tokio::spawn(async move { browser_returns(&url, "code", Some("forged")).await });
        assert!(matches!(pending.complete().await, Err(SeamError::Refused(_))));
        assert_eq!(grant.state().await, GrantState::Unauthorized);
        assert!(!dir.path().join("test.json").exists());
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
        };
        std::fs::write(dir.path().join("test.json"), serde_json::to_vec(&stale).expect("json")).expect("write");
        let clock = Arc::new(FixedClock(AtomicI64::new(1_000)));
        let grant = handle(dir.path(), grant_config(&server.url), clock).await;

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
        assert_eq!(on_disk.expires_at, Some(1_100));
    }

    #[tokio::test]
    async fn revoke_forgets_the_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let server = fake_token_server(Vec::new()).await;
        let good = flow::StoredTokens {
            version: flow::TOKENS_VERSION,
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: None,
            scopes: Vec::new(),
            token_type: "Bearer".to_string(),
        };
        std::fs::write(dir.path().join("test.json"), serde_json::to_vec(&good).expect("json")).expect("write");
        let grant = handle(dir.path(), grant_config(&server.url), Arc::new(FixedClock(AtomicI64::new(0)))).await;
        assert!(matches!(grant.state().await, GrantState::Authorized { .. }));
        grant.revoke().await.expect("revokes");
        assert_eq!(grant.state().await, GrantState::Unauthorized);
        assert!(!dir.path().join("test.json").exists());
        grant.revoke().await.expect("revoking twice is fine");
    }

    #[tokio::test]
    async fn missing_client_secret_is_reported_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = Arc::new(grant::Settings {
            callback_port: 0,
            authorization_timeout: Duration::from_secs(1),
            http: reqwest::Client::new(),
            clock: Arc::new(FixedClock(AtomicI64::new(0))),
        });
        let grant = grant::GrantHandle::load(
            grant_config("http://127.0.0.1:1/token"),
            grant::ClientCredentials::Missing { env: "TEST_CLIENT_ID".to_string() },
            dir.path().join("test.json"),
            settings,
        )
        .await;
        assert_eq!(
            grant.state().await,
            GrantState::MissingSecret { env: "TEST_CLIENT_ID".to_string() }
        );
        assert!(matches!(grant.access_token().await, Err(SeamError::Unavailable(_))));
        assert!(matches!(grant.authorize().await, Err(SeamError::Unavailable(_))));
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
