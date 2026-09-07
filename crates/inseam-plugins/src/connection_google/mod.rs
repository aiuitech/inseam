//! The Google Workspace connection: one entry that holds the OAuth grant to
//! a Google account and stewards each of its services — Gmail, Drive,
//! Calendar, Contacts, Tasks — as its own host (`design/connections.md`).
//! The plugin knows Google, so it registers the grant itself into the
//! `oauth` seam (endpoints, read-only scopes, the OpenID scopes that name
//! the account); the owner supplies only the client identity through the
//! environment variables the entry names, and authorizes once from any
//! client. The moment the grant is authorized — at apply if tokens are on
//! file, or later through the `GrantChanged` event — the plugin derives
//! each service's host id from the signed-in account and registers a
//! connection per service; revoking the grant withdraws them again. Nothing
//! here restarts: a long-running node (`inseam serve`) gains its Google
//! hosts the instant the browser comes back.

mod api;
mod calendar;
mod contacts;
mod drive;
mod gmail;
mod services;
mod tasks;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use inseam_kernel::substrate::{
    ApplyCx, Inject, Manifest, Plugin, PluginError, SecretNeed, parse_config,
};
use inseam_seams::connection::{
    CONNECTIONS, Capabilities, Connection, Connections, HostDescription, Registration,
    derive_host_id,
};
use inseam_seams::oauth::{GrantChanged, GrantId, GrantSpec, GrantState, register_as_effect};

pub use api::Bases;
pub use services::{GoogleService, IDENTITY_SCOPES, scopes_for};

pub const AUTHORIZATION_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Sources one enumeration of one service will collect per run, whatever
/// the service: the sweep's own `max_sources` bounds what is indexed; this
/// bounds what is asked of Google.
pub const SOURCES_MAX_DEFAULT: u32 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoogleConnectionConfig {
    /// The grant this entry registers and consumes; also its credential
    /// file's name. Two Google accounts are two entries with two grant ids.
    pub grant: GrantId,
    /// Environment variable holding the OAuth client id from the Google
    /// Cloud console.
    pub client_id_env: String,
    /// Environment variable holding the client secret; `None` (or, in TOML,
    /// an empty string) for a client type Google issues without one.
    pub client_secret_env: Option<String>,
    /// The services to steward, each as its own host; the grant's scopes
    /// follow.
    pub services: Vec<GoogleService>,
    /// Sources one enumeration collects per service per run.
    pub sources_max: u32,
}

impl Default for GoogleConnectionConfig {
    fn default() -> Self {
        Self {
            grant: GrantId::new("google").expect("literal grant id is valid"),
            client_id_env: "GOOGLE_CLIENT_ID".to_string(),
            client_secret_env: Some("GOOGLE_CLIENT_SECRET".to_string()),
            services: GoogleService::ALL.to_vec(),
            sources_max: SOURCES_MAX_DEFAULT,
        }
    }
}

impl GoogleConnectionConfig {
    /// The grant this entry defines: Google's endpoints, the identity scopes
    /// plus each enabled service's, and the owner's client variables.
    /// `access_type=offline` and `prompt=consent` make Google issue a
    /// refresh token, without which the connection would die in an hour.
    pub fn grant_spec(&self) -> GrantSpec {
        let mut authorization_params = BTreeMap::new();
        authorization_params.insert("access_type".to_string(), "offline".to_string());
        authorization_params.insert("prompt".to_string(), "consent".to_string());
        GrantSpec {
            id: self.grant.clone(),
            authorization_url: AUTHORIZATION_URL.to_string(),
            token_url: TOKEN_URL.to_string(),
            scopes: scopes_for(&self.services),
            client_id_env: self.client_id_env.clone(),
            client_secret_env: self
                .client_secret_env
                .clone()
                .filter(|env| !env.trim().is_empty()),
            authorization_params,
        }
    }

    fn validate(&self) -> Result<(), PluginError> {
        if self.services.is_empty() {
            return Err(PluginError(
                "config: `services` is empty; enable at least one of gmail, drive, calendar, contacts, tasks"
                    .to_string(),
            ));
        }
        let distinct: BTreeSet<GoogleService> = self.services.iter().copied().collect();
        if distinct.len() != self.services.len() {
            return Err(PluginError(
                "config: `services` lists a service twice".to_string(),
            ));
        }
        if self.client_id_env.trim().is_empty() {
            return Err(PluginError("config: `client_id_env` is empty".to_string()));
        }
        if self.sources_max == 0 {
            return Err(PluginError(
                "config: `sources_max` must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

pub struct GoogleConnection {
    config: GoogleConnectionConfig,
    bases: Bases,
}

impl GoogleConnection {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: GoogleConnectionConfig = parse_config(config)?;
        config.validate()?;
        Ok(Self {
            config,
            bases: Bases::google(),
        })
    }

    /// The same plugin pointed at another Google — a fake one, in tests.
    pub fn with_bases(mut self, bases: Bases) -> Self {
        self.bases = bases;
        self
    }
}

pub struct GoogleConnectionFactory;

impl inseam_kernel::substrate::PluginFactory for GoogleConnectionFactory {
    fn name(&self) -> &str {
        "connection-google"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(GoogleConnection::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for GoogleConnection {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("connections"), Inject::required("oauth")];
        Manifest {
            name: "connection-google",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let connections = cx.get(&CONNECTIONS)?;
        let grant = register_as_effect(cx, self.config.grant_spec()).await?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| PluginError(format!("http client: {e}")))?;
        let hosts = Arc::new(Hosts {
            entry_id: cx.entry_id().to_string(),
            grant_id: self.config.grant.clone(),
            services: self.config.services.clone(),
            sources_max: usize::try_from(self.config.sources_max).unwrap_or(usize::MAX),
            connections,
            api: Arc::new(api::GoogleApi::new(
                http,
                Arc::clone(&grant),
                self.bases.clone(),
            )),
            registered: Mutex::new(Registered::default()),
        });
        // Tokens already on file: steward the hosts now, loudly if that
        // fails — a second entry for the same account is a composition
        // mistake, not something to log and move past.
        if let Some(account) = grant.state().await.account() {
            hosts.register(account).map_err(PluginError)?;
        }
        // Later authorizations and revocations arrive as events; the
        // subscription and the hosts both unwind with the fiber.
        let listening = Arc::clone(&hosts);
        let subscription = cx
            .bus()
            .on::<GrantChanged>(move |event| listening.on_grant_changed(event));
        cx.keep("follow the google grant", subscription);
        cx.effect("unregister the google hosts", move || hosts.close());
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretNeed> {
        // The same prose the oauth entry would give for a configured grant;
        // the callback port there is the default, which is what the
        // console instructions in docs use.
        crate::oauth::secret_needs(
            &self.config.grant_spec(),
            crate::oauth::OAuthConfig::default().callback_port,
        )
    }
}

/// The hosts this entry stewards: registered while the grant is
/// authorized, withdrawn when it is not, closed with the fiber.
struct Hosts {
    entry_id: String,
    grant_id: GrantId,
    services: Vec<GoogleService>,
    sources_max: usize,
    connections: Arc<dyn Connections>,
    api: Arc<api::GoogleApi>,
    registered: Mutex<Registered>,
}

#[derive(Default)]
struct Registered {
    /// Set when the fiber unwinds: no registration may follow.
    closed: bool,
    /// The account the current registrations are for.
    account: Option<String>,
    disposers: Vec<Box<dyn FnOnce() + Send>>,
}

impl Hosts {
    fn on_grant_changed(&self, event: &GrantChanged) {
        if event.grant != self.grant_id {
            return;
        }
        match &event.state {
            GrantState::Authorized {
                account: Some(account),
                ..
            } => {
                if let Err(e) = self.register(account) {
                    tracing::warn!(entry = %self.entry_id, "google hosts not registered: {e}");
                }
            }
            GrantState::Authorized { account: None, .. } => {
                tracing::warn!(
                    entry = %self.entry_id,
                    "grant `{}` was authorized without naming the account; re-authorize with the identity scopes",
                    self.grant_id
                );
            }
            GrantState::Unauthorized | GrantState::MissingSecret { .. } => self.unregister(),
        }
    }

    /// Steward every enabled service for `account`. Idempotent for the
    /// same account; a different account replaces the registrations.
    fn register(&self, account: &str) -> Result<(), String> {
        let mut registered = self.registered.lock().unwrap_or_else(|e| e.into_inner());
        if registered.closed {
            return Ok(());
        }
        if registered.account.as_deref() == Some(account) {
            return Ok(());
        }
        dispose_all(&mut registered);
        for service in &self.services {
            let host = derive_host_id(&service.kind(), account);
            let registration = Registration {
                entry_id: self.entry_id.clone(),
                host: HostDescription {
                    id: host.clone(),
                    kind: service.kind(),
                    display_name: format!("{} · {account}", service.label()),
                },
                // Every Google service here is read through its API and
                // never written; change feeds (Gmail history, Drive
                // changes) are declared when the scheduling hook exists.
                capabilities: Capabilities::READ_ONLY,
                roots: Vec::new(),
                connection: self.connection_for(*service, host),
            };
            match self.connections.register(registration) {
                Ok(disposer) => registered.disposers.push(disposer),
                Err(e) => {
                    dispose_all(&mut registered);
                    return Err(format!("{}: {e}", service.label()));
                }
            }
        }
        registered.account = Some(account.to_string());
        Ok(())
    }

    fn connection_for(
        &self,
        service: GoogleService,
        host: inseam_kernel::address::HostId,
    ) -> Arc<dyn Connection> {
        let api = Arc::clone(&self.api);
        match service {
            GoogleService::Gmail => {
                Arc::new(gmail::GmailConnection::new(api, host, self.sources_max))
            }
            GoogleService::Drive => {
                Arc::new(drive::DriveConnection::new(api, host, self.sources_max))
            }
            GoogleService::Calendar => Arc::new(calendar::CalendarConnection::new(
                api,
                host,
                self.sources_max,
            )),
            GoogleService::Contacts => Arc::new(contacts::ContactsConnection::new(
                api,
                host,
                self.sources_max,
            )),
            GoogleService::Tasks => {
                Arc::new(tasks::TasksConnection::new(api, host, self.sources_max))
            }
        }
    }

    fn unregister(&self) {
        let mut registered = self.registered.lock().unwrap_or_else(|e| e.into_inner());
        dispose_all(&mut registered);
    }

    fn close(&self) {
        let mut registered = self.registered.lock().unwrap_or_else(|e| e.into_inner());
        registered.closed = true;
        dispose_all(&mut registered);
    }
}

fn dispose_all(registered: &mut Registered) {
    for dispose in registered.disposers.drain(..) {
        dispose();
    }
    registered.account = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_entry_covers_every_service_with_identity_scopes_first() {
        let config = GoogleConnectionConfig::default();
        let spec = config.grant_spec();
        assert_eq!(spec.id.as_str(), "google");
        assert_eq!(spec.authorization_url, AUTHORIZATION_URL);
        assert_eq!(spec.scopes[0], "openid");
        assert_eq!(
            spec.scopes.len(),
            IDENTITY_SCOPES.len() + GoogleService::ALL.len()
        );
        assert_eq!(spec.authorization_params["access_type"], "offline");
        assert_eq!(spec.authorization_params["prompt"], "consent");
        assert_eq!(spec.client_id_env, "GOOGLE_CLIENT_ID");
        assert_eq!(
            spec.client_secret_env.as_deref(),
            Some("GOOGLE_CLIENT_SECRET")
        );
    }

    #[test]
    fn config_parses_services_and_refuses_empty_or_duplicate_lists() {
        let some: toml::Table = toml::from_str(r#"services = ["gmail", "drive"]"#).expect("toml");
        let plugin = GoogleConnection::from_config(&some).expect("builds");
        assert_eq!(
            plugin.config.services,
            vec![GoogleService::Gmail, GoogleService::Drive]
        );
        assert_eq!(plugin.config.grant_spec().scopes.len(), 4);

        let none: toml::Table = toml::from_str("services = []").expect("toml");
        assert!(GoogleConnection::from_config(&none).is_err());
        let twice: toml::Table = toml::from_str(r#"services = ["gmail", "gmail"]"#).expect("toml");
        assert!(GoogleConnection::from_config(&twice).is_err());
        let unknown: toml::Table = toml::from_str(r#"services = ["photos"]"#).expect("toml");
        assert!(GoogleConnection::from_config(&unknown).is_err());
        let zero: toml::Table = toml::from_str("sources_max = 0").expect("toml");
        assert!(GoogleConnection::from_config(&zero).is_err());
        let stray: toml::Table = toml::from_str("color = \"blue\"").expect("toml");
        assert!(GoogleConnection::from_config(&stray).is_err());
        let public: toml::Table = toml::from_str("client_secret_env = \"\"").expect("toml");
        let plugin = GoogleConnection::from_config(&public).expect("builds");
        assert_eq!(
            plugin.config.grant_spec().client_secret_env,
            None,
            "an empty name is no secret"
        );
    }

    #[test]
    fn secrets_name_the_client_variables() {
        let plugin = GoogleConnection::from_config(&toml::Table::new()).expect("builds");
        let needs = plugin.secrets();
        let envs: Vec<&str> = needs.iter().map(|n| n.env.as_str()).collect();
        assert_eq!(envs, vec!["GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"]);
        assert!(needs[0].purpose.contains("accounts.google.com"));
    }
}
