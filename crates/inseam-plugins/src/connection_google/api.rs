//! The one HTTP client every Google service connection shares: bearer
//! authentication from the grant, Google's error envelope mapped onto the
//! seam's vocabulary, the `pageToken` walk every list endpoint speaks, and
//! the envelope shape for sources the connection renders as text. Nothing
//! service-specific lives here; each service file owns its own endpoints
//! and its own rendering.

use std::sync::Arc;

use inseam_kernel::address::{ContentLength, Envelope, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_seams::SeamError;
use inseam_seams::dates::parse_rfc3339_epoch;
use inseam_seams::oauth::Grant;
use inseam_seams::text::count_lines;
use url::Url;

/// Pages one enumeration will walk before stopping: 200 pages of the
/// largest page sizes Google serves is far past any personal account, and
/// it bounds a runaway cursor.
pub const PAGES_MAX: u32 = 200;

/// Per-item requests in flight while enumerating a service that needs one
/// call per source (Gmail's metadata): enough to hide latency, well under
/// Google's per-user rate limits.
pub const FETCH_CONCURRENCY: usize = 8;

/// Where Google's APIs live. Overridable so tests point the same client at
/// a fake server.
#[derive(Debug, Clone)]
pub struct Bases {
    /// Drive, Gmail, Calendar, Tasks: `https://www.googleapis.com`.
    pub api: Url,
    /// The People API: `https://people.googleapis.com`.
    pub people: Url,
}

impl Bases {
    pub fn google() -> Self {
        Self {
            api: Url::parse("https://www.googleapis.com").expect("literal URL is valid"),
            people: Url::parse("https://people.googleapis.com").expect("literal URL is valid"),
        }
    }
}

pub struct GoogleApi {
    http: reqwest::Client,
    grant: Arc<dyn Grant>,
    bases: Bases,
}

impl GoogleApi {
    pub fn new(http: reqwest::Client, grant: Arc<dyn Grant>, bases: Bases) -> Self {
        Self { http, grant, bases }
    }

    /// A URL under the main API base: `segments` are appended as encoded
    /// path segments (so ids containing `@` or `#` are safe), `query` as
    /// pairs.
    pub fn api_url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        build_url(&self.bases.api, segments, query)
    }

    /// A URL under the People API base.
    pub fn people_url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        build_url(&self.bases.people, segments, query)
    }

    /// One authenticated GET expecting JSON.
    pub async fn get_json(&self, url: Url) -> Result<serde_json::Value, SeamError> {
        let bytes = self.get_bytes(url.clone()).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| SeamError::failed(format!("{}: response is not JSON: {e}", redact(&url))))
    }

    /// One authenticated GET returning the raw body.
    pub async fn get_bytes(&self, url: Url) -> Result<Vec<u8>, SeamError> {
        let token = self.grant.access_token().await?;
        let response = self
            .http
            .get(url.clone())
            .header(reqwest::header::AUTHORIZATION, token.authorization_header())
            .header(reqwest::header::ACCEPT, "application/json, */*")
            .send()
            .await
            .map_err(|e| SeamError::failed(format!("{}: {e}", redact(&url))))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| {
            SeamError::failed(format!("{}: reading the response: {e}", redact(&url)))
        })?;
        if status.is_success() {
            return Ok(body.to_vec());
        }
        Err(api_error(
            status.as_u16(),
            &body,
            &url,
            self.grant.id().as_str(),
        ))
    }

    /// Walk a list endpoint: `url` names the first page; every page's
    /// `items_key` array is collected until there is no `nextPageToken`,
    /// [`PAGES_MAX`] pages have been read, or `items_max` items are held.
    pub async fn list_pages(
        &self,
        url: Url,
        items_key: &str,
        items_max: usize,
    ) -> Result<Vec<serde_json::Value>, SeamError> {
        let mut items: Vec<serde_json::Value> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut pages: u32 = 0;
        while pages < PAGES_MAX {
            pages += 1;
            let mut page_url = url.clone();
            if let Some(token) = &page_token {
                page_url.query_pairs_mut().append_pair("pageToken", token);
            }
            let mut page = self.get_json(page_url).await?;
            if let Some(serde_json::Value::Array(found)) =
                page.get_mut(items_key).map(serde_json::Value::take)
            {
                items.extend(found);
            }
            if items.len() >= items_max {
                items.truncate(items_max);
                break;
            }
            page_token = page
                .get("nextPageToken")
                .and_then(|t| t.as_str())
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if page_token.is_none() {
                break;
            }
        }
        Ok(items)
    }
}

fn build_url(base: &Url, segments: &[&str], query: &[(&str, &str)]) -> Url {
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .expect("API bases are hierarchical URLs");
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    url
}

/// A URL for error messages: the path names the call; the query may carry
/// a search string the owner typed and is not worth echoing.
fn redact(url: &Url) -> String {
    format!("{}{}", url.origin().ascii_serialization(), url.path())
}

/// Google's error envelope (`{"error": {"code", "message", "status"}}`)
/// onto the seam: a rejected token is `Unauthorized` (the owner
/// re-authorizes), a denial is `Refused` (a scope or a policy), rate limits
/// are `Unavailable`, and the rest failed with Google's own words.
fn api_error(status: u16, body: &[u8], url: &Url, grant: &str) -> SeamError {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(200).collect());
    let at = redact(url);
    match status {
        401 => SeamError::Unauthorized(format!(
            "Google rejected the token for grant `{grant}` ({message}); run `inseam authorize {grant}`"
        )),
        403 => SeamError::Refused(format!("{at}: Google refused the call: {message}")),
        404 => SeamError::failed(format!("{at}: not found: {message}")),
        429 => SeamError::Unavailable(format!("{at}: Google rate-limited the call: {message}")),
        _ => SeamError::failed(format!("{at}: Google answered {status}: {message}")),
    }
}

/// An RFC 3339 field of a resource as a timestamp, when present and
/// well-formed; a malformed date is dropped rather than failing the source.
pub fn timestamp_field(value: &serde_json::Value, key: &str) -> Option<Timestamp> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| parse_rfc3339_epoch(s).ok())
        .map(Timestamp)
}

/// A string field, empty when absent.
pub fn str_field<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default()
}

/// A string field, `None` when absent or empty.
pub fn opt_str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    Some(str_field(value, key))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// What the connection rendered as text for one source: the envelope
/// (`text/plain`, a line count, the rendering's own timestamps) and the raw
/// size the sweep keeps for change detection.
pub struct Rendered {
    pub envelope: Envelope,
    pub raw_bytes: u64,
}

/// The envelope of a source this connection serves as rendered text.
pub fn rendered(
    source_type: &str,
    text: &str,
    created: Option<Timestamp>,
    modified: Option<Timestamp>,
    observed: Timestamp,
    hint: Option<String>,
) -> Rendered {
    Rendered {
        envelope: Envelope {
            source_type: source_type.to_string(),
            content_type: Mimetype::text_plain(),
            length: ContentLength::Lines(count_lines(text)),
            created,
            modified,
            observed,
            properties: Vec::new(),
            hint,
            content_digest: None,
        },
        raw_bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
    }
}

/// A `<parent>/<child>` locator split once; `None` when it has no `/`.
pub fn split_locator(locator: &str) -> Option<(&str, &str)> {
    locator
        .split_once('/')
        .filter(|(parent, child)| !parent.is_empty() && !child.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use inseam_seams::oauth::{AccessToken, GrantId, GrantSpec, GrantState};

    /// A grant that is always authorized with a fixed token.
    pub struct FakeGrant {
        spec: GrantSpec,
    }

    impl FakeGrant {
        pub fn arc() -> Arc<dyn Grant> {
            Arc::new(Self {
                spec: GrantSpec {
                    id: GrantId::new("google").expect("valid"),
                    authorization_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                    token_url: "https://oauth2.googleapis.com/token".into(),
                    scopes: Vec::new(),
                    client_id_env: "X".into(),
                    client_secret_env: None,
                    authorization_params: BTreeMap::new(),
                },
            })
        }
    }

    #[async_trait::async_trait]
    impl Grant for FakeGrant {
        fn spec(&self) -> &GrantSpec {
            &self.spec
        }
        async fn state(&self) -> GrantState {
            GrantState::Authorized {
                expires_at: None,
                scopes: Vec::new(),
                account: Some("greg@example.com".into()),
            }
        }
        async fn access_token(&self) -> Result<AccessToken, SeamError> {
            Ok(AccessToken::new("fake-token"))
        }
        async fn revoke(&self) -> Result<(), SeamError> {
            Ok(())
        }
    }

    /// A fake Google: an axum router served on a loopback port, with both
    /// bases pointed at it.
    pub async fn fake_google(router: axum::Router) -> (GoogleApi, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binds");
        let base =
            Url::parse(&format!("http://{}", listener.local_addr().expect("addr"))).expect("url");
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serves");
        });
        let api = GoogleApi::new(
            reqwest::Client::new(),
            FakeGrant::arc(),
            Bases {
                api: base.clone(),
                people: base,
            },
        );
        (api, server)
    }

    #[test]
    fn urls_encode_segments_and_queries() {
        let api = GoogleApi::new(reqwest::Client::new(), FakeGrant::arc(), Bases::google());
        let url = api.api_url(
            &[
                "calendar",
                "v3",
                "calendars",
                "en.usa#holiday@group.v.calendar.google.com",
                "events",
            ],
            &[("singleEvents", "true"), ("q", "a b")],
        );
        assert_eq!(
            url.as_str(),
            "https://www.googleapis.com/calendar/v3/calendars/en.usa%23holiday@group.v.calendar.google.com/events?singleEvents=true&q=a+b"
        );
        assert_eq!(
            api.people_url(&["v1", "people", "me", "connections"], &[])
                .as_str(),
            "https://people.googleapis.com/v1/people/me/connections"
        );
    }

    #[test]
    fn errors_map_onto_the_seam_vocabulary() {
        let url = Url::parse("https://www.googleapis.com/drive/v3/files?q=secret").expect("url");
        let body = br#"{"error":{"code":403,"message":"Insufficient Permission","status":"PERMISSION_DENIED"}}"#;
        let refused = api_error(403, body, &url, "google");
        assert!(
            matches!(refused, SeamError::Refused(ref m) if m.contains("Insufficient Permission") && !m.contains("secret"))
        );
        assert!(matches!(
            api_error(401, b"", &url, "google"),
            SeamError::Unauthorized(_)
        ));
        assert!(matches!(
            api_error(429, b"", &url, "google"),
            SeamError::Unavailable(_)
        ));
        assert!(
            matches!(api_error(500, b"boom", &url, "google"), SeamError::Failed(ref m) if m.contains("boom"))
        );
    }

    #[tokio::test]
    async fn list_pages_follows_tokens_and_stops_at_the_item_ceiling() {
        use axum::extract::Query;
        use axum::routing::get;
        use std::collections::HashMap;
        let router = axum::Router::new().route(
            "/things",
            get(|Query(q): Query<HashMap<String, String>>| async move {
                let page = q.get("pageToken").map(String::as_str).unwrap_or("");
                let body = match page {
                    "" => serde_json::json!({"items": [1, 2], "nextPageToken": "p2"}),
                    "p2" => serde_json::json!({"items": [3], "nextPageToken": "p3"}),
                    _ => serde_json::json!({"items": [4, 5]}),
                };
                axum::Json(body)
            }),
        );
        let (api, _server) = fake_google(router).await;
        let all = api
            .list_pages(api.api_url(&["things"], &[]), "items", 100)
            .await
            .expect("lists");
        assert_eq!(all.len(), 5);
        let capped = api
            .list_pages(api.api_url(&["things"], &[]), "items", 3)
            .await
            .expect("lists");
        assert_eq!(capped.len(), 3);
    }

    #[tokio::test]
    async fn requests_carry_the_bearer_token_and_surface_google_errors() {
        use axum::http::HeaderMap;
        use axum::routing::get;
        let router = axum::Router::new()
            .route(
                "/echo",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    axum::Json(serde_json::json!({"auth": auth}))
                }),
            )
            .route(
                "/denied",
                get(|| async {
                    (
                        axum::http::StatusCode::FORBIDDEN,
                        axum::Json(serde_json::json!({"error": {"message": "nope"}})),
                    )
                }),
            );
        let (api, _server) = fake_google(router).await;
        let echoed = api.get_json(api.api_url(&["echo"], &[])).await.expect("ok");
        assert_eq!(echoed["auth"], "Bearer fake-token");
        assert!(matches!(
            api.get_json(api.api_url(&["denied"], &[])).await,
            Err(SeamError::Refused(_))
        ));
    }

    #[test]
    fn rendered_envelopes_count_lines_and_bytes() {
        let r = rendered(
            "event",
            "a\nb\nc",
            None,
            Some(Timestamp(5)),
            Timestamp(9),
            Some("t".into()),
        );
        assert_eq!(r.envelope.length, ContentLength::Lines(3));
        assert_eq!(r.raw_bytes, 5);
        assert_eq!(r.envelope.content_type.essence(), "text/plain");
        assert_eq!(split_locator("list/task"), Some(("list", "task")));
        assert_eq!(split_locator("flat"), None);
        assert_eq!(split_locator("/x"), None);
    }
}
