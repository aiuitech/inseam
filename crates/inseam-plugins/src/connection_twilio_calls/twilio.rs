//! The Twilio REST client this connection speaks: the 2010 Voice API for
//! calls and recordings and the v2 Conversation Intelligence API for
//! transcripts, both through the node's guarded fetcher so every request
//! passes the same posture a loaded plugin's would (`inseam_seams::fetch`).
//! Authentication is an API key scoped to one (sub)account — the account
//! SID rides in the path, the key's SID and secret in Basic auth — so a
//! node never holds anything that reaches another tenant's account.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use url::Url;

use inseam_seams::SeamError;
use inseam_seams::dates::parse_rfc3339_epoch;
use inseam_seams::fetch::{FetchMethod, FetchRequest, FetchResponse, Fetcher, HostPattern};

/// Pages of sentences one transcript render will walk: 100 pages of 100
/// sentences is far past an eight-hour call.
pub const SENTENCE_PAGES_MAX: u32 = 100;
/// Sentences requested per page, Twilio's ceiling.
pub const SENTENCE_PAGE_SIZE: u32 = 100;
/// Redirect hops a Twilio media URL may take (the media redirects once to
/// its storage origin).
const REDIRECTS_MAX: u32 = 3;

/// The account the key is scoped to and the key itself.
#[derive(Clone)]
pub struct Credentials {
    pub account_sid: String,
    pub api_key_sid: String,
    pub api_key_secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("account_sid", &self.account_sid)
            .field("api_key_sid", &self.api_key_sid)
            .field("api_key_secret", &"<redacted>")
            .finish()
    }
}

/// A recording Twilio holds, as its list endpoint describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingRecord {
    pub sid: String,
    pub call_sid: String,
    pub duration_secs: u64,
    pub created_epoch: i64,
    pub channels: u32,
}

/// The call a recording belongs to: who was dialed, from where, when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRecord {
    pub sid: String,
    pub from: String,
    pub to: String,
    pub started_epoch: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptStatus {
    Queued,
    InProgress,
    Completed,
    Failed,
}

/// One transcribed sentence: which media channel spoke it, when it
/// started, and the words.
#[derive(Debug, Clone, PartialEq)]
pub struct Sentence {
    pub channel: u32,
    pub start_secs: f64,
    pub text: String,
}

pub struct TwilioClient {
    fetcher: Fetcher,
    credentials: Credentials,
    api_base: Url,
    intelligence_base: Url,
}

impl TwilioClient {
    /// `api_base` and `intelligence_base` are Twilio's origins in
    /// production and a loopback fake in tests; only their hosts are ever
    /// contactable.
    pub fn new(
        credentials: Credentials,
        api_base: Url,
        intelligence_base: Url,
        body_bytes_max: u64,
        timeout: Duration,
    ) -> Result<Self, SeamError> {
        let mut hosts = Vec::new();
        for base in [&api_base, &intelligence_base] {
            let host = base
                .host_str()
                .ok_or_else(|| SeamError::Invalid(format!("{base} names no host")))?;
            hosts.push(HostPattern::parse(host).map_err(SeamError::Invalid)?);
        }
        Ok(Self {
            fetcher: Fetcher::for_allowed_hosts_only(
                hosts,
                body_bytes_max,
                timeout,
                REDIRECTS_MAX,
                format!("inseam/{}", env!("CARGO_PKG_VERSION")),
            ),
            credentials,
            api_base,
            intelligence_base,
        })
    }

    pub fn account_sid(&self) -> &str {
        &self.credentials.account_sid
    }

    /// Place a call from `from` to `to` that runs `twiml` when answered.
    /// Returns the call SID.
    pub async fn create_call(
        &self,
        from: &str,
        to: &str,
        twiml: &str,
        ring_secs: u32,
    ) -> Result<String, SeamError> {
        let url = self.account_url(&["Calls.json"], &[]);
        let body = form(&[
            ("From", from),
            ("To", to),
            ("Twiml", twiml),
            ("Timeout", &ring_secs.to_string()),
        ]);
        let json = self.send_json(FetchMethod::Post, url, Some(body)).await?;
        string_field(&json, "sid")
    }

    /// Completed recordings, newest first, one page.
    pub async fn list_recordings(&self, page_size: u32) -> Result<Vec<RecordingRecord>, SeamError> {
        assert!(page_size >= 1);
        let url = self.account_url(
            &["Recordings.json"],
            &[("PageSize", &page_size.to_string())],
        );
        let json = self.send_json(FetchMethod::Get, url, None).await?;
        let items = json["recordings"]
            .as_array()
            .ok_or_else(|| SeamError::failed("Twilio recordings list carries no `recordings`"))?;
        items.iter().map(parse_recording).collect()
    }

    pub async fn fetch_call(&self, call_sid: &str) -> Result<CallRecord, SeamError> {
        let url = self.account_url(&["Calls", &format!("{call_sid}.json")], &[]);
        let json = self.send_json(FetchMethod::Get, url, None).await?;
        Ok(CallRecord {
            sid: string_field(&json, "sid")?,
            from: json["from"].as_str().unwrap_or_default().to_string(),
            to: json["to"].as_str().unwrap_or_default().to_string(),
            started_epoch: json["start_time"]
                .as_str()
                .and_then(|s| parse_rfc2822_epoch(s).ok()),
        })
    }

    /// The recording's audio as MP3 (32 kbit/s — a quarter of the WAV
    /// form, which is what keeps a multi-hour call under the byte cap).
    pub async fn download_recording_mp3(&self, recording_sid: &str) -> Result<Vec<u8>, SeamError> {
        let url = self.account_url(&["Recordings", &format!("{recording_sid}.mp3")], &[]);
        let response = self.send(FetchMethod::Get, url.clone(), None).await?;
        if response.status / 100 != 2 {
            return Err(SeamError::failed(format!(
                "GET {}: Twilio answered {}",
                redact(&url),
                response.status
            )));
        }
        Ok(response.body)
    }

    /// Remove a recording from Twilio. A recording already gone is a
    /// success: the point is that it is not there.
    pub async fn delete_recording(&self, recording_sid: &str) -> Result<(), SeamError> {
        let url = self.account_url(&["Recordings", &format!("{recording_sid}.json")], &[]);
        let response = self.send(FetchMethod::Delete, url.clone(), None).await?;
        match response.status {
            204 | 404 => Ok(()),
            status => Err(SeamError::failed(format!(
                "DELETE {}: Twilio answered {status}",
                redact(&url)
            ))),
        }
    }

    /// Ask Conversation Intelligence to transcribe a recording. Returns
    /// the transcript SID; status is polled with [`Self::fetch_transcript_status`].
    pub async fn create_transcript(
        &self,
        service_sid: &str,
        recording_sid: &str,
    ) -> Result<String, SeamError> {
        let url = join(&self.intelligence_base, &["v2", "Transcripts"], &[]);
        let channel = serde_json::json!({
            "media_properties": { "source_sid": recording_sid }
        })
        .to_string();
        let body = form(&[("ServiceSid", service_sid), ("Channel", &channel)]);
        let json = self.send_json(FetchMethod::Post, url, Some(body)).await?;
        string_field(&json, "sid")
    }

    pub async fn fetch_transcript_status(
        &self,
        transcript_sid: &str,
    ) -> Result<TranscriptStatus, SeamError> {
        let url = join(
            &self.intelligence_base,
            &["v2", "Transcripts", transcript_sid],
            &[],
        );
        let json = self.send_json(FetchMethod::Get, url, None).await?;
        let status = json["status"].as_str().unwrap_or_default();
        parse_transcript_status(status)
    }

    /// Every sentence of a completed transcript, in order, across pages.
    pub async fn fetch_sentences(&self, transcript_sid: &str) -> Result<Vec<Sentence>, SeamError> {
        let mut sentences = Vec::new();
        let mut next: Option<Url> = Some(join(
            &self.intelligence_base,
            &["v2", "Transcripts", transcript_sid, "Sentences"],
            &[("PageSize", &SENTENCE_PAGE_SIZE.to_string())],
        ));
        for page in 0..SENTENCE_PAGES_MAX {
            assert!(page < SENTENCE_PAGES_MAX);
            let Some(url) = next.take() else {
                break;
            };
            let json = self.send_json(FetchMethod::Get, url, None).await?;
            if let Some(items) = json["sentences"].as_array() {
                sentences.extend(items.iter().filter_map(parse_sentence));
            }
            next = json["meta"]["next_page_url"]
                .as_str()
                .and_then(|s| Url::parse(s).ok());
        }
        Ok(sentences)
    }

    fn account_url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut path: Vec<&str> = vec!["2010-04-01", "Accounts", &self.credentials.account_sid];
        path.extend_from_slice(segments);
        join(&self.api_base, &path, query)
    }

    async fn send_json(
        &self,
        method: FetchMethod,
        url: Url,
        body: Option<String>,
    ) -> Result<serde_json::Value, SeamError> {
        let response = self.send(method, url.clone(), body).await?;
        let json: serde_json::Value = serde_json::from_slice(&response.body).unwrap_or_default();
        match response.status {
            200..=299 => Ok(json),
            401 | 403 => Err(SeamError::Unauthorized(format!(
                "Twilio refused the account's API key ({}): {}",
                response.status,
                twilio_message(&json)
            ))),
            status => Err(SeamError::failed(format!(
                "{method:?} {}: Twilio answered {status}: {}",
                redact(&url),
                twilio_message(&json)
            ))),
        }
    }

    async fn send(
        &self,
        method: FetchMethod,
        url: Url,
        body: Option<String>,
    ) -> Result<FetchResponse, SeamError> {
        let mut headers = vec![
            ("Authorization".to_string(), self.basic_auth()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        if body.is_some() {
            headers.push((
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ));
        }
        let request = FetchRequest {
            method,
            url,
            headers,
            body: body.map(String::into_bytes),
        };
        self.fetcher.send(&request, &[]).await
    }

    fn basic_auth(&self) -> String {
        let pair = format!(
            "{}:{}",
            self.credentials.api_key_sid, self.credentials.api_key_secret
        );
        format!("Basic {}", STANDARD.encode(pair))
    }
}

/// A URL under `base`: `segments` appended as encoded path segments,
/// `query` as pairs.
fn join(base: &Url, segments: &[&str], query: &[(&str, &str)]) -> Url {
    let mut url = base.clone();
    {
        let mut path = url.path_segments_mut().expect("http(s) URLs have a path");
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query.iter().copied());
    }
    url
}

/// The URL without its query, for messages.
fn redact(url: &Url) -> String {
    let mut shown = url.clone();
    shown.set_query(None);
    shown.to_string()
}

fn form(fields: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in fields {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn twilio_message(json: &serde_json::Value) -> String {
    json["message"]
        .as_str()
        .filter(|m| !m.is_empty())
        .unwrap_or("no message")
        .to_string()
}

fn string_field(json: &serde_json::Value, field: &str) -> Result<String, SeamError> {
    json[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| SeamError::failed(format!("Twilio answer carries no `{field}`")))
}

fn parse_recording(json: &serde_json::Value) -> Result<RecordingRecord, SeamError> {
    let created = json["date_created"].as_str().unwrap_or_default();
    Ok(RecordingRecord {
        sid: string_field(json, "sid")?,
        call_sid: json["call_sid"].as_str().unwrap_or_default().to_string(),
        duration_secs: json["duration"]
            .as_str()
            .and_then(|d| d.parse::<u64>().ok())
            .or_else(|| json["duration"].as_u64())
            .unwrap_or(0),
        created_epoch: parse_rfc2822_epoch(created).unwrap_or(0),
        channels: u32::try_from(json["channels"].as_u64().unwrap_or(1)).unwrap_or(1),
    })
}

pub(crate) fn parse_transcript_status(status: &str) -> Result<TranscriptStatus, SeamError> {
    match status {
        "queued" => Ok(TranscriptStatus::Queued),
        "in-progress" => Ok(TranscriptStatus::InProgress),
        "completed" => Ok(TranscriptStatus::Completed),
        "failed" | "canceled" | "error" => Ok(TranscriptStatus::Failed),
        other => Err(SeamError::failed(format!(
            "Twilio transcript status `{other}` is not one this connection knows"
        ))),
    }
}

fn parse_sentence(json: &serde_json::Value) -> Option<Sentence> {
    let text = json["transcript"].as_str()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Sentence {
        channel: u32::try_from(json["media_channel"].as_u64().unwrap_or(1)).unwrap_or(1),
        start_secs: json["start_time"].as_f64().unwrap_or(0.0),
        text,
    })
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Twilio's 2010 API stamps dates RFC 2822 style (`Fri, 12 Sep 2026
/// 18:05:23 +0000`). Reshaped into RFC 3339 and handed to the shared
/// parser, so there is one calendar implementation in the seams.
pub fn parse_rfc2822_epoch(s: &str) -> Result<i64, SeamError> {
    let unparseable = || SeamError::failed(format!("`{s}` is not an RFC 2822 date"));
    let rest = s.trim();
    let rest = rest.split_once(',').map_or(rest, |(_, r)| r).trim();
    let mut parts = rest.split_whitespace();
    let (day, month, year, clock, offset) = match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some(d), Some(m), Some(y), Some(c), Some(o), None) => (d, m, y, c, o),
        _ => return Err(unparseable()),
    };
    let month_index = MONTHS
        .iter()
        .position(|name| name.eq_ignore_ascii_case(month))
        .ok_or_else(unparseable)?;
    let day: u32 = day.parse().map_err(|_| unparseable())?;
    if offset.len() != 5 || !offset.starts_with(['+', '-']) {
        return Err(unparseable());
    }
    let rfc3339 = format!(
        "{year}-{:02}-{day:02}T{clock}{}:{}",
        month_index + 1,
        &offset[..3],
        &offset[3..]
    );
    parse_rfc3339_epoch(&rfc3339).map_err(|_| unparseable())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2822_dates_parse_to_epoch_seconds() {
        assert_eq!(
            parse_rfc2822_epoch("Fri, 12 Sep 2026 18:05:23 +0000").expect("parses"),
            1_789_236_323
        );
        assert_eq!(
            parse_rfc2822_epoch("Fri, 12 Sep 2026 20:05:23 +0200").expect("parses"),
            1_789_236_323
        );
        assert_eq!(
            parse_rfc2822_epoch("12 Sep 2026 18:05:23 +0000").expect("parses"),
            1_789_236_323
        );
    }

    #[test]
    fn rfc2822_dates_refuse_the_shapeless() {
        for bad in [
            "",
            "2026-09-12",
            "Fri, 12 Sept 2026 18:05:23 +0000",
            "12 Sep 2026 18:05:23",
        ] {
            assert!(parse_rfc2822_epoch(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn urls_join_segments_and_queries_under_the_base() {
        let base = Url::parse("https://api.twilio.com").expect("url");
        let url = join(
            &base,
            &["2010-04-01", "Accounts", "AC1", "Calls.json"],
            &[("PageSize", "5")],
        );
        assert_eq!(
            url.as_str(),
            "https://api.twilio.com/2010-04-01/Accounts/AC1/Calls.json?PageSize=5"
        );
        assert_eq!(
            redact(&url),
            "https://api.twilio.com/2010-04-01/Accounts/AC1/Calls.json"
        );
    }

    #[test]
    fn forms_encode_twiml_safely() {
        let body = form(&[("To", "+14155550123"), ("Twiml", "<Response/>")]);
        assert_eq!(body, "To=%2B14155550123&Twiml=%3CResponse%2F%3E");
    }

    #[test]
    fn recordings_parse_from_twilio_json() {
        let json = serde_json::json!({
            "sid": "RE1", "call_sid": "CA1", "duration": "61",
            "date_created": "Fri, 12 Sep 2026 18:05:23 +0000", "channels": 1
        });
        let record = parse_recording(&json).expect("parses");
        assert_eq!(record.sid, "RE1");
        assert_eq!(record.duration_secs, 61);
        assert_eq!(record.created_epoch, 1_789_236_323);
        assert!(parse_recording(&serde_json::json!({})).is_err());
    }

    #[test]
    fn transcript_statuses_map_and_unknown_ones_fail() {
        assert_eq!(
            parse_transcript_status("completed").expect("known"),
            TranscriptStatus::Completed
        );
        assert_eq!(
            parse_transcript_status("canceled").expect("known"),
            TranscriptStatus::Failed
        );
        assert!(parse_transcript_status("dreaming").is_err());
    }

    #[test]
    fn credentials_redact_the_secret_in_debug() {
        let shown = format!(
            "{:?}",
            Credentials {
                account_sid: "AC1".into(),
                api_key_sid: "SK1".into(),
                api_key_secret: "hunter2".into(),
            }
        );
        assert!(shown.contains("SK1"));
        assert!(!shown.contains("hunter2"));
    }
}
