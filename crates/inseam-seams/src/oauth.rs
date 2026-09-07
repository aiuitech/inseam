//! The `oauth` seam: the node's OAuth 2.0 grants as a service
//! (`design/connections.md`). Many remote hosts are reached through the
//! same protocol — an authorization-code grant that yields a refreshable
//! token — so the flow, the token storage, and the refresh live once,
//! behind this seam, and every host connection (Gmail, Calendar, Slack, a
//! loaded plugin for some REST API) *consumes* a grant by id instead of
//! re-implementing the dance.
//!
//! A grant arrives one of two ways, and the seam is a **registry** so both
//! meet in one place: the owner configures a generic one in the composition
//! (provider endpoints, scopes, which environment variables hold the
//! client's own credentials), or a connection plugin that knows its provider
//! registers the [`GrantSpec`] itself as a fiber effect — the Google
//! connection brings Google's endpoints and scopes, and the owner supplies
//! only the client identity. The owner authorizes a grant once through the
//! browser, from whichever client is at hand: a local transport (CLI, a
//! native app) takes the redirect on a loopback port; a remote transport
//! (the hosted web console) serves the redirect itself and hands the
//! browser's return back through [`OAuth::complete_authorization`].
//! Connections then ask the handle for a live access token, and learn about
//! authorizations and revocations through the [`GrantChanged`] event.
//!
//! The secret material never crosses the seam except as an [`AccessToken`]
//! the consumer needs to call its host: client secrets stay in the
//! environment, refresh tokens stay in the provider's credential files.
//! That is also the attenuation point for loaded plugins — a bridge can hand
//! a component a handle that signs requests without ever revealing the
//! token.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_kernel::address::Timestamp;
use inseam_kernel::substrate::{ApplyCx, Notify, PluginError, ServiceKey};

use crate::SeamError;

pub const OAUTH: ServiceKey<dyn OAuth> = ServiceKey::new("oauth");

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GrantIdError {
    #[error("grant id may not be empty")]
    Empty,
    #[error("grant id `{0}` may only contain lowercase ASCII letters, digits, `-` and `_`")]
    InvalidCharacters(String),
    #[error("grant id `{0}` is longer than {GRANT_ID_CHARS_MAX} characters")]
    TooLong(String),
}

/// Longest grant id accepted; ids name a provider account in a config
/// file (`google`, `slack-work`), and double as a credential file name.
pub const GRANT_ID_CHARS_MAX: usize = 64;

/// Names one grant — what a host connection's config points at
/// (`grant = "google"`). Validated so it is safe as a file name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GrantId(String);

impl GrantId {
    pub fn new(id: impl Into<String>) -> Result<Self, GrantIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(GrantIdError::Empty);
        }
        if id.len() > GRANT_ID_CHARS_MAX {
            return Err(GrantIdError::TooLong(id));
        }
        let valid = id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !valid {
            return Err(GrantIdError::InvalidCharacters(id));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GrantId {
    type Error = GrantIdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<GrantId> for String {
    fn from(g: GrantId) -> String {
        g.0
    }
}

impl fmt::Display for GrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything that defines one grant before any token exists: the
/// provider's endpoints, the scopes the owner is willing to grant, and which
/// environment variables hold the client's own identity. The composition
/// form (`[[entry.config.grants]]` on the `oauth` entry) and a plugin's
/// registration are this same record — the seam does not care which door a
/// grant came through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSpec {
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

impl GrantSpec {
    /// The host part of the authorization URL, for owner-facing prose
    /// ("accounts.google.com").
    pub fn provider(&self) -> String {
        url_host(&self.authorization_url).unwrap_or_else(|| self.authorization_url.clone())
    }
}

fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// A bearer token for one call. `Debug` redacts it so it never lands in a
/// log by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn secret(&self) -> &str {
        &self.0
    }

    /// The `Authorization` header value.
    pub fn authorization_header(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

/// Where a grant stands, for status surfaces and for consumers deciding
/// whether they can work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GrantState {
    /// The client's own credentials are absent from the environment; names
    /// the variable so an owner surface can ask for it.
    MissingSecret { env: String },
    /// Configured, but the owner has not authorized it yet.
    Unauthorized,
    /// Tokens are on file and usable.
    Authorized {
        /// When the current access token expires; `None` when the provider
        /// did not say (it is then refreshed only on a rejected call).
        expires_at: Option<Timestamp>,
        /// The scopes the provider actually granted (the declared scopes
        /// when the provider did not echo them).
        scopes: Vec<String>,
        /// The account the owner signed in as, when the provider said
        /// (OpenID Connect `email`): the principal a connection derives its
        /// host identity from (`design/addressing.md`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
}

impl GrantState {
    /// The signed-in account, when authorized and known.
    pub fn account(&self) -> Option<&str> {
        match self {
            Self::Authorized { account, .. } => account.as_deref(),
            Self::MissingSecret { .. } | Self::Unauthorized => None,
        }
    }
}

/// One grant: a handle consumers hold for the life of their fiber and ask
/// for tokens when they call their host.
#[async_trait::async_trait]
pub trait Grant: Send + Sync {
    fn id(&self) -> &GrantId {
        &self.spec().id
    }

    /// What the grant was defined as: endpoints, declared scopes, client
    /// variables.
    fn spec(&self) -> &GrantSpec;

    /// The scopes the owner declared for this grant.
    fn scopes(&self) -> &[String] {
        &self.spec().scopes
    }

    async fn state(&self) -> GrantState;

    /// A currently valid access token, refreshed first when it is about to
    /// expire. `SeamError::Unauthorized` until the owner has authorized.
    async fn access_token(&self) -> Result<AccessToken, SeamError>;

    /// Forget the stored tokens; the grant is `Unauthorized` afterwards and
    /// [`GrantChanged`] says so.
    async fn revoke(&self) -> Result<(), SeamError>;
}

/// Where the provider should send the browser back (RFC 6749 §3.1.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Redirect {
    /// The provider listens on `127.0.0.1:<callback_port>/callback` itself
    /// (RFC 8252): for transports running where the owner's browser runs.
    Loopback,
    /// The transport serves `redirect_uri` and delivers what lands there
    /// through [`OAuth::complete_authorization`]: for a node the owner
    /// reaches remotely, whose loopback is not the browser's.
    External { redirect_uri: String },
}

/// An authorization the owner is about to carry out in the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationStarted {
    pub grant: GrantId,
    /// The provider's authorization URL, with this attempt's parameters:
    /// what the transport sends the owner to.
    pub url: String,
    /// This attempt's CSRF token — how the transport later names the
    /// attempt to wait on or complete.
    pub state: String,
    /// Where the browser will come back, as sent to the provider.
    pub redirect_uri: String,
}

/// What the browser brought back to an [`Redirect::External`] URI: the
/// query parameters of RFC 6749 §4.1.2, as the transport received them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuthorizationCallback {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}

/// Fired when a grant's state changes hands: after the owner authorizes it
/// (through any transport) and after it is revoked. Connections that
/// steward hosts behind a grant register and unregister on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantChanged {
    pub grant: GrantId,
    pub state: GrantState,
}

impl Notify for GrantChanged {}

/// The undo a grant registration returns; calling it withdraws the grant.
pub type GrantDisposer = Box<dyn FnOnce() + Send>;

/// The seam: the grants this node holds — configured on the `oauth` entry
/// or registered by plugins — and the owner's authorization of them.
#[async_trait::async_trait]
pub trait OAuth: Send + Sync {
    /// Every grant, ordered by id.
    fn grants(&self) -> Vec<Arc<dyn Grant>>;

    /// The grant a consumer's config names.
    fn grant(&self, id: &GrantId) -> Option<Arc<dyn Grant>> {
        self.grants().into_iter().find(|g| g.id() == id)
    }

    /// Register a grant a plugin defines (a connection that knows its
    /// provider). The handle is live at once — tokens already on file are
    /// loaded — and the disposer is the registering fiber's effect. One
    /// grant per id: a second registration for a known id is refused, named.
    async fn register(&self, spec: GrantSpec)
    -> Result<(Arc<dyn Grant>, GrantDisposer), SeamError>;

    /// Start the owner's authorization of a grant: the returned URL is what
    /// to send them to. For [`Redirect::Loopback`] the provider waits for
    /// the browser itself; for [`Redirect::External`] the transport delivers
    /// the return through [`OAuth::complete_authorization`]. Either way,
    /// [`OAuth::await_authorization`] waits for the outcome.
    async fn authorize(
        &self,
        grant: &GrantId,
        redirect: Redirect,
    ) -> Result<AuthorizationStarted, SeamError>;

    /// Wait — bounded by the provider's configured timeout — for a started
    /// authorization to finish: tokens stored, or the attempt refused.
    async fn await_authorization(&self, state: &str) -> Result<GrantId, SeamError>;

    /// Deliver the browser's return to a transport-served redirect URI. The
    /// `state` names the attempt; the code is exchanged and the tokens
    /// stored before this returns.
    async fn complete_authorization(
        &self,
        callback: AuthorizationCallback,
    ) -> Result<GrantId, SeamError>;
}

/// Register a grant into the seam as a fiber effect: a connection plugin's
/// `apply` calls this with the provider it knows, keeps the handle, and
/// unmounting the plugin withdraws the grant through the disposer.
pub async fn register_as_effect(
    cx: &mut ApplyCx<'_>,
    spec: GrantSpec,
) -> Result<Arc<dyn Grant>, PluginError> {
    let label = format!("register oauth grant `{}`", spec.id);
    let oauth = cx.get(&OAUTH)?;
    let (grant, disposer) = oauth
        .register(spec)
        .await
        .map_err(|e| PluginError(e.to_string()))?;
    cx.effect(label, disposer);
    Ok(grant)
}

/// A consumer's guard at apply time: the grant it was given must declare
/// every scope the consumer needs, or the mismatch is named now — not as
/// a 403 from the host mid-sweep.
pub fn require_scopes(grant: &dyn Grant, needed: &[&str]) -> Result<(), SeamError> {
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|scope| !grant.scopes().iter().any(|declared| declared == scope))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SeamError::Unavailable(format!(
            "grant `{}` does not declare the scopes {}; add them to its `scopes` and authorize again",
            grant.id(),
            missing.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_id_accepts_file_safe_names() {
        assert_eq!(GrantId::new("google").expect("valid").as_str(), "google");
        assert_eq!(
            GrantId::new("slack_work-2").expect("valid").as_str(),
            "slack_work-2"
        );
    }

    #[test]
    fn grant_id_rejects_empty_long_and_path_like_names() {
        assert_eq!(GrantId::new(""), Err(GrantIdError::Empty));
        assert!(matches!(
            GrantId::new("../etc"),
            Err(GrantIdError::InvalidCharacters(_))
        ));
        assert!(matches!(
            GrantId::new("Google"),
            Err(GrantIdError::InvalidCharacters(_))
        ));
        let long = "g".repeat(GRANT_ID_CHARS_MAX + 1);
        assert!(matches!(GrantId::new(long), Err(GrantIdError::TooLong(_))));
    }

    #[test]
    fn access_token_debug_redacts() {
        let token = AccessToken::new("ya29.secret");
        assert_eq!(format!("{token:?}"), "AccessToken(<redacted>)");
        assert_eq!(token.authorization_header(), "Bearer ya29.secret");
    }

    #[test]
    fn spec_provider_is_the_authorization_host() {
        let spec = GrantSpec {
            id: GrantId::new("g").expect("valid"),
            authorization_url: "https://accounts.google.com/o/oauth2/v2/auth?x=1".to_string(),
            token_url: "https://oauth2.googleapis.com/token".to_string(),
            scopes: Vec::new(),
            client_id_env: "E".to_string(),
            client_secret_env: None,
            authorization_params: BTreeMap::new(),
        };
        assert_eq!(spec.provider(), "accounts.google.com");
        assert_eq!(
            url_host("https://user@host.example:8443/p"),
            Some("host.example".to_string())
        );
        assert_eq!(url_host("not a url"), None);
    }

    #[test]
    fn grant_state_serializes_tagged_and_exposes_the_account() {
        let state = GrantState::Authorized {
            expires_at: Some(Timestamp(10)),
            scopes: vec!["s".to_string()],
            account: Some("greg@example.com".to_string()),
        };
        let json = serde_json::to_value(&state).expect("serializes");
        assert_eq!(json["state"], "authorized");
        assert_eq!(json["account"], "greg@example.com");
        assert_eq!(state.account(), Some("greg@example.com"));
        assert_eq!(GrantState::Unauthorized.account(), None);
        let back: GrantState = serde_json::from_value(json).expect("parses");
        assert_eq!(back, state);
    }
}
