//! Gmail as a host: every message is a source whose locator is the Gmail
//! message id. The connection serves a message as text — its addressing
//! headers, then the body, preferring the `text/plain` part and falling back
//! to a tag-stripped `text/html` one — so the envelope says `text/plain`:
//! what the index reads is what the connection serves. Enumeration is a
//! Gmail search (the scope is the query, empty for all mail), and each
//! listed message costs one metadata call, so the walk is bounded and
//! concurrent.

use std::sync::Arc;
use std::time::SystemTime;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::stream::{self, StreamExt};

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Locator, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::text::slice_lines;
use inseam_seams::SeamError;

use super::api::{opt_str_field, str_field, GoogleApi, FETCH_CONCURRENCY};

/// MIME parts one message walk will visit; real messages have a handful,
/// and a pathological one is cut off rather than walked forever.
pub const PARTS_MAX: u32 = 256;

/// Messages per list page — Gmail's maximum.
const PAGE_SIZE: &str = "500";

const HEADERS: [&str; 5] = ["From", "To", "Cc", "Date", "Subject"];

pub struct GmailConnection {
    api: Arc<GoogleApi>,
    host: HostId,
    sources_max: usize,
}

impl GmailConnection {
    pub fn new(api: Arc<GoogleApi>, host: HostId, sources_max: usize) -> Self {
        Self {
            api,
            host,
            sources_max,
        }
    }

    fn message_id<'a>(&self, address: &'a Address) -> Result<&'a str, SeamError> {
        if address.host != self.host {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, but this connection stewards `{}`",
                address.host, self.host
            )));
        }
        let id = address.locator.as_str();
        let valid = id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            return Err(SeamError::Refused(format!("`{id}` is not a Gmail message id")));
        }
        Ok(id)
    }

    fn message_url(&self, id: &str, format: &str) -> url::Url {
        let mut query: Vec<(&str, &str)> = vec![("format", format)];
        if format == "metadata" {
            query.extend(HEADERS.iter().map(|h| ("metadataHeaders", *h)));
        }
        self.api
            .api_url(&["gmail", "v1", "users", "me", "messages", id], &query)
    }

    /// One listed message's envelope, from its metadata.
    async fn source_of(&self, id: String, observed: Timestamp) -> Result<EnumeratedSource, SeamError> {
        let message = self.api.get_json(self.message_url(&id, "metadata")).await?;
        let locator = Locator::new(id.clone())
            .map_err(|e| SeamError::failed(format!("Gmail message id `{id}` is not addressable: {e}")))?;
        let size: u64 = message
            .get("sizeEstimate")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let internal: Option<Timestamp> = str_field(&message, "internalDate")
            .parse::<i64>()
            .ok()
            .map(|ms| Timestamp(ms.div_euclid(1000)));
        let subject = header(&message, "Subject").or_else(|| opt_str_field(&message, "snippet"));
        Ok(EnumeratedSource {
            address: Address::new(self.host.clone(), locator),
            envelope: Envelope {
                source_type: "email".to_string(),
                content_type: Mimetype::text_plain(),
                length: ContentLength::Bytes(size),
                created: internal,
                modified: internal,
                observed,
                properties: Vec::new(),
                hint: subject,
                content_digest: None,
            },
            raw_bytes: size,
        })
    }
}

/// A header value from a message payload, by case-insensitive name.
fn header(message: &serde_json::Value, name: &str) -> Option<String> {
    message
        .get("payload")?
        .get("headers")?
        .as_array()?
        .iter()
        .find(|h| str_field(h, "name").eq_ignore_ascii_case(name))
        .and_then(|h| opt_str_field(h, "value"))
}

/// The message as the index reads it: the addressing headers, a blank line,
/// the body text.
pub fn render_message(message: &serde_json::Value) -> String {
    let mut out = String::new();
    for name in HEADERS {
        if let Some(value) = header(message, name) {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(&value);
            out.push('\n');
        }
    }
    out.push('\n');
    match body_text(message) {
        Some(body) => out.push_str(body.trim_end()),
        None => out.push_str(str_field(message, "snippet")),
    }
    out.push('\n');
    out
}

/// The best body: the first `text/plain` part, else the first `text/html`
/// part with its tags stripped. The MIME tree is walked with an explicit,
/// bounded stack.
fn body_text(message: &serde_json::Value) -> Option<String> {
    let payload = message.get("payload")?;
    let mut plain: Option<String> = None;
    let mut html: Option<String> = None;
    let mut stack: Vec<&serde_json::Value> = vec![payload];
    let mut visited: u32 = 0;
    while let Some(part) = stack.pop() {
        if visited >= PARTS_MAX {
            break;
        }
        visited += 1;
        let mimetype = str_field(part, "mimeType").to_ascii_lowercase();
        let data = part
            .get("body")
            .and_then(|b| b.get("data"))
            .and_then(|d| d.as_str())
            .and_then(decode_base64url);
        if mimetype == "text/plain" && plain.is_none() {
            plain = data;
        } else if mimetype == "text/html" && html.is_none() {
            html = data;
        }
        if plain.is_some() {
            break;
        }
        if let Some(parts) = part.get("parts").and_then(|p| p.as_array()) {
            // Pushed in reverse so the first part is visited first.
            stack.extend(parts.iter().rev());
        }
    }
    plain.or_else(|| html.map(|h| strip_html(&h)))
}

/// Gmail encodes bodies as URL-safe base64, usually unpadded; both forms
/// are accepted.
fn decode_base64url(data: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(data.trim_end_matches('=')).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Tags removed, block elements turned into line breaks, `<style>` and
/// `<script>` bodies dropped, the handful of entities mail clients emit
/// decoded, whitespace runs collapsed per line. Not a renderer — just
/// enough to index the words.
pub fn strip_html(html: &str) -> String {
    const BLOCK_TAGS: [&str; 12] = [
        "p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote",
    ];
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skipping = false;
    let mut tag = String::new();
    for c in html.chars() {
        match (in_tag, c) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                let closing = tag.starts_with('/');
                let name: String = tag
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if BLOCK_TAGS.contains(&name.as_str()) {
                    text.push('\n');
                }
                if matches!(name.as_str(), "style" | "script") {
                    skipping = !closing;
                }
            }
            (true, c) => tag.push(c),
            (false, c) => {
                if !skipping {
                    text.push(c);
                }
            }
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait::async_trait]
impl Connection for GmailConnection {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let observed = Timestamp::from(SystemTime::now());
        let query = root.trim();
        let mut params: Vec<(&str, &str)> = vec![("maxResults", PAGE_SIZE)];
        if !query.is_empty() {
            params.push(("q", query));
        }
        let listed = self
            .api
            .list_pages(
                self.api.api_url(&["gmail", "v1", "users", "me", "messages"], &params),
                "messages",
                self.sources_max,
            )
            .await?;
        let ids: Vec<String> = listed.iter().filter_map(|m| opt_str_field(m, "id")).collect();
        // One metadata call per message, `FETCH_CONCURRENCY` in flight,
        // landed in list order so enumeration stays deterministic.
        let sources: Vec<Result<EnumeratedSource, SeamError>> = stream::iter(ids)
            .map(|id| self.source_of(id, observed))
            .buffered(FETCH_CONCURRENCY)
            .collect()
            .await;
        sources.into_iter().collect()
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        // Message ids are flat and a search is not a prefix: only all mail
        // reconciles deletions.
        root.trim().is_empty().then(String::new)
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let id = self.message_id(address)?;
        let message = self.api.get_json(self.message_url(id, "full")).await?;
        Ok(render_message(&message))
    }

    async fn read_lines(&self, address: &Address, start: u64, end: u64) -> Result<String, SeamError> {
        let text = self.read_text(address).await?;
        slice_lines(&text, start, end).map_err(SeamError::failed)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        Ok(self.read_text(address).await?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use axum::extract::{Path, Query};
    use axum::routing::get;

    use super::super::api::tests::fake_google;

    fn b64(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s)
    }

    fn router() -> axum::Router {
        axum::Router::new()
            .route(
                "/gmail/v1/users/me/messages",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let messages = if q.get("q").map(String::as_str) == Some("label:INBOX") {
                        serde_json::json!([{"id": "m1"}])
                    } else {
                        serde_json::json!([{"id": "m1"}, {"id": "m2"}])
                    };
                    axum::Json(serde_json::json!({"messages": messages}))
                }),
            )
            .route(
                "/gmail/v1/users/me/messages/{id}",
                get(|Path(id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                    let full = q.get("format").map(String::as_str) == Some("full");
                    let mut message = serde_json::json!({
                        "id": id, "sizeEstimate": 1234, "internalDate": "1735787045123", "snippet": "snip",
                        "payload": {"mimeType": "multipart/alternative", "headers": [
                            {"name": "Subject", "value": format!("Hello {id}")},
                            {"name": "From", "value": "a@example.com"},
                            {"name": "Date", "value": "Thu, 2 Jan 2025 03:04:05 +0000"}
                        ]}
                    });
                    if full {
                        message["payload"]["parts"] = serde_json::json!([
                            {"mimeType": "text/html", "body": {"data": b64("<p>Hi <b>there</b></p>")}},
                            {"mimeType": "text/plain", "body": {"data": b64("Hi there\n\nplain body")}}
                        ]);
                    }
                    axum::Json(message)
                }),
            )
    }

    fn host() -> HostId {
        HostId::new("gmail-test").expect("valid")
    }

    #[tokio::test]
    async fn enumerates_messages_with_subject_size_and_date() {
        let (api, _server) = fake_google(router()).await;
        let gmail = GmailConnection::new(Arc::new(api), host(), 100);
        let sources = gmail.enumerate("").await.expect("enumerates");
        let ids: Vec<&str> = sources.iter().map(|s| s.address.locator.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2"]);
        assert_eq!(sources[0].envelope.hint.as_deref(), Some("Hello m1"));
        assert_eq!(sources[0].envelope.modified, Some(Timestamp(1_735_787_045)));
        assert_eq!(sources[0].raw_bytes, 1234);
        assert_eq!(sources[0].envelope.source_type, "email");
        assert_eq!(gmail.enumerate("label:INBOX").await.expect("enumerates").len(), 1);
        assert_eq!(gmail.locator_prefix(""), Some(String::new()));
        assert_eq!(gmail.locator_prefix("label:INBOX"), None);
    }

    #[tokio::test]
    async fn reads_a_message_as_headers_and_the_plain_part() {
        let (api, _server) = fake_google(router()).await;
        let gmail = GmailConnection::new(Arc::new(api), host(), 100);
        let address: Address = "inseam://gmail-test/m1".parse().expect("address");
        let text = gmail.read_text(&address).await.expect("reads");
        assert_eq!(
            text,
            "From: a@example.com\nDate: Thu, 2 Jan 2025 03:04:05 +0000\nSubject: Hello m1\n\nHi there\n\nplain body\n"
        );
        assert_eq!(gmail.read_lines(&address, 3, 3).await.expect("reads"), "Subject: Hello m1");
        let bad: Address = "inseam://gmail-test/not%20an%20id".parse().expect("address");
        assert!(matches!(gmail.read_text(&bad).await, Err(SeamError::Refused(_))));
    }

    #[test]
    fn html_bodies_are_stripped_when_no_plain_part_exists() {
        let message = serde_json::json!({
            "payload": {"mimeType": "multipart/mixed", "headers": [{"name": "Subject", "value": "S"}], "parts": [
                {"mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/html", "body": {"data": b64("<html><style>p{color:red}</style><body><p>Hi&nbsp;<b>you</b></p><div>Second &amp; last</div></body></html>")}}
                ]}
            ]}
        });
        assert_eq!(render_message(&message), "Subject: S\n\nHi you\nSecond & last\n");
        let empty = serde_json::json!({"snippet": "only a snippet"});
        assert_eq!(render_message(&empty), "\nonly a snippet\n");
    }

    #[test]
    fn base64url_accepts_padded_and_unpadded_data() {
        assert_eq!(decode_base64url("aGk").as_deref(), Some("hi"));
        assert_eq!(decode_base64url("aGk=").as_deref(), Some("hi"));
        assert_eq!(decode_base64url("!!"), None);
    }
}
