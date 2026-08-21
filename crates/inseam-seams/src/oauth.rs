//! The `oauth` seam: the node's OAuth 2.0 grants as a service
//! (`design/connections.md`). Many remote hosts are reached through the
//! same protocol — an authorization-code grant that yields a refreshable
//! token — so the flow, the token storage, and the refresh live once,
//! behind this seam, and every host connection (Gmail, Calendar, Slack, a
//! loaded plugin for some REST API) *consumes* a grant by id instead of
//! re-implementing the dance. A grant is configured in the composition
//! (provider endpoints, scopes, which environment variables hold the
//! client's own credentials); the owner authorizes it once through the
//! browser; connections then ask the handle for a live access token.
//!
//! The secret material never crosses the seam except as an [`AccessToken`]
//! the consumer needs to call its host: client secrets stay in the
//! environment, refresh tokens stay in the provider's credential files.
//! That is also the attenuation point for loaded plugins — a bridge can hand
//! a component a handle that signs requests without ever revealing the
//! token.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use inseam_kernel::address::Timestamp;
use inseam_kernel::substrate::ServiceKey;

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

/// Names one configured grant — what a host connection's config points at
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
    },
}

/// One configured grant: a handle consumers hold for the life of their
/// fiber and ask for tokens when they call their host.
#[async_trait::async_trait]
pub trait Grant: Send + Sync {
    fn id(&self) -> &GrantId;

    /// The scopes the owner declared for this grant in the composition.
    fn scopes(&self) -> &[String];

    async fn state(&self) -> GrantState;

    /// A currently valid access token, refreshed first when it is about to
    /// expire. `SeamError::Unauthorized` until the owner has authorized.
    async fn access_token(&self) -> Result<AccessToken, SeamError>;

    /// Start the owner's authorization: the returned handle carries the URL
    /// to send them to, and completes when their browser comes back.
    async fn authorize(&self) -> Result<Box<dyn PendingAuthorization>, SeamError>;

    /// Forget the stored tokens; the grant is `Unauthorized` afterwards.
    async fn revoke(&self) -> Result<(), SeamError>;
}

/// An authorization in flight: the owner is (about to be) in the browser.
#[async_trait::async_trait]
pub trait PendingAuthorization: Send {
    /// The provider's authorization URL, with this attempt's parameters.
    fn url(&self) -> &str;

    /// Wait — bounded by the provider's configured timeout — for the
    /// browser to come back, exchange the code, and store the tokens.
    async fn complete(self: Box<Self>) -> Result<(), SeamError>;
}

/// The seam: the grants this node has configured.
pub trait OAuth: Send + Sync {
    /// Every configured grant, ordered by id.
    fn grants(&self) -> Vec<Arc<dyn Grant>>;

    /// The grant a consumer's config names.
    fn grant(&self, id: &GrantId) -> Option<Arc<dyn Grant>> {
        self.grants().into_iter().find(|g| g.id() == id)
    }
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
        assert_eq!(GrantId::new("slack_work-2").expect("valid").as_str(), "slack_work-2");
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
}
