//! Google Calendar as a host: every event is a source whose locator is
//! `<calendar id>/<event id>`. The connection serves an event as text —
//! what, when, where, who, then the description — so the envelope says
//! `text/plain`. The scope is a calendar id (`primary`), or empty for every
//! calendar in the account's list; cancelled events are not sources.

use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{Address, HostId, Locator, Timestamp};
use inseam_seams::SeamError;
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::text::slice_lines;

use super::api::{GoogleApi, opt_str_field, rendered, split_locator, str_field, timestamp_field};

/// Events per page — the Calendar API's maximum.
const PAGE_SIZE: &str = "2500";

const EVENT_FIELDS: &str = "nextPageToken,items(id,status,summary,description,location,start,end,updated,created,attendees,organizer,htmlLink,hangoutLink)";

pub struct CalendarConnection {
    api: Arc<GoogleApi>,
    host: HostId,
    sources_max: usize,
}

impl CalendarConnection {
    pub fn new(api: Arc<GoogleApi>, host: HostId, sources_max: usize) -> Self {
        Self {
            api,
            host,
            sources_max,
        }
    }

    /// The calendars a scope names: the one given, or every calendar the
    /// account lists.
    async fn calendars_for(&self, root: &str) -> Result<Vec<String>, SeamError> {
        let root = root.trim();
        if !root.is_empty() {
            return Ok(vec![root.to_string()]);
        }
        let listed = self
            .api
            .list_pages(
                self.api.api_url(
                    &["calendar", "v3", "users", "me", "calendarList"],
                    &[("fields", "nextPageToken,items(id)")],
                ),
                "items",
                self.sources_max,
            )
            .await?;
        Ok(listed
            .iter()
            .filter_map(|c| opt_str_field(c, "id"))
            .collect())
    }

    fn source_of(
        &self,
        calendar: &str,
        event: &serde_json::Value,
        observed: Timestamp,
    ) -> Option<EnumeratedSource> {
        if str_field(event, "status") == "cancelled" {
            return None;
        }
        let id = opt_str_field(event, "id")?;
        let locator = Locator::new(format!("{calendar}/{id}")).ok()?;
        let text = render_event(event);
        let r = rendered(
            "event",
            &text,
            timestamp_field(event, "created"),
            timestamp_field(event, "updated"),
            observed,
            opt_str_field(event, "summary"),
        );
        Some(EnumeratedSource {
            address: Address::new(self.host.clone(), locator),
            envelope: r.envelope,
            raw_bytes: r.raw_bytes,
        })
    }

    fn event_of<'a>(&self, address: &'a Address) -> Result<(&'a str, &'a str), SeamError> {
        if address.host != self.host {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, but this connection stewards `{}`",
                address.host, self.host
            )));
        }
        split_locator(address.locator.as_str()).ok_or_else(|| {
            SeamError::Refused(format!(
                "`{}` is not a `<calendar>/<event>` locator",
                address.locator.as_str()
            ))
        })
    }
}

/// When an event starts or ends, as the API spells it: a `dateTime` for a
/// timed event, a `date` for an all-day one.
fn when(event: &serde_json::Value, edge: &str) -> String {
    let Some(at) = event.get(edge) else {
        return String::new();
    };
    opt_str_field(at, "dateTime")
        .or_else(|| opt_str_field(at, "date"))
        .unwrap_or_default()
}

/// One attendee or organizer as `Name <email>`, whichever parts exist.
fn person(value: &serde_json::Value) -> Option<String> {
    let email = opt_str_field(value, "email");
    let name = opt_str_field(value, "displayName");
    match (name, email) {
        (Some(name), Some(email)) => Some(format!("{name} <{email}>")),
        (None, Some(email)) => Some(email),
        (Some(name), None) => Some(name),
        (None, None) => None,
    }
}

fn people(event: &serde_json::Value, key: &str) -> Vec<String> {
    event
        .get(key)
        .and_then(|a| a.as_array())
        .map(|list| list.iter().filter_map(person).collect())
        .unwrap_or_default()
}

/// The event as the index reads it.
pub fn render_event(event: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str("Event: ");
    out.push_str(str_field(event, "summary"));
    out.push('\n');
    let start = when(event, "start");
    let end = when(event, "end");
    if !start.is_empty() {
        out.push_str(&format!("When: {start} – {end}\n"));
    }
    if let Some(location) = opt_str_field(event, "location") {
        out.push_str(&format!("Where: {location}\n"));
    }
    if let Some(organizer) = event.get("organizer").and_then(person) {
        out.push_str(&format!("Organizer: {organizer}\n"));
    }
    let attendees = people(event, "attendees");
    if !attendees.is_empty() {
        out.push_str(&format!("Attendees: {}\n", attendees.join(", ")));
    }
    for link in ["htmlLink", "hangoutLink"] {
        if let Some(url) = opt_str_field(event, link) {
            out.push_str(&format!("Link: {url}\n"));
        }
    }
    if let Some(description) = opt_str_field(event, "description") {
        out.push('\n');
        out.push_str(description.trim_end());
        out.push('\n');
    }
    out
}

#[async_trait::async_trait]
impl Connection for CalendarConnection {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let observed = Timestamp::from(SystemTime::now());
        let mut sources: Vec<EnumeratedSource> = Vec::new();
        for calendar in self.calendars_for(root).await? {
            let remaining = self.sources_max.saturating_sub(sources.len());
            if remaining == 0 {
                break;
            }
            let events = self
                .api
                .list_pages(
                    self.api.api_url(
                        &["calendar", "v3", "calendars", &calendar, "events"],
                        &[
                            ("singleEvents", "true"),
                            ("maxResults", PAGE_SIZE),
                            ("fields", EVENT_FIELDS),
                        ],
                    ),
                    "items",
                    remaining,
                )
                .await?;
            sources.extend(
                events
                    .iter()
                    .filter_map(|e| self.source_of(&calendar, e, observed)),
            );
        }
        Ok(sources)
    }

    fn locator_prefix(&self, root: &str) -> Option<String> {
        Some(root.trim().to_string())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let (calendar, event) = self.event_of(address)?;
        let event = self
            .api
            .get_json(self.api.api_url(
                &["calendar", "v3", "calendars", calendar, "events", event],
                &[],
            ))
            .await?;
        Ok(render_event(&event))
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        let text = self.read_text(address).await?;
        slice_lines(&text, start, end)
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        Ok(self.read_text(address).await?.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::extract::Path;
    use axum::routing::get;

    use super::super::api::tests::fake_google;

    fn event(id: &str, summary: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "status": "confirmed", "summary": summary, "description": "bring notes",
            "location": "Room 4", "updated": "2025-01-02T03:04:05Z", "created": "2025-01-01T00:00:00Z",
            "start": {"dateTime": "2025-01-03T09:00:00-05:00"}, "end": {"dateTime": "2025-01-03T10:00:00-05:00"},
            "organizer": {"email": "o@example.com", "displayName": "Olga"},
            "attendees": [{"email": "a@example.com"}, {"email": "b@example.com", "displayName": "Bea"}],
            "htmlLink": "https://calendar.google.com/e/1"
        })
    }

    fn router() -> axum::Router {
        axum::Router::new()
            .route(
                "/calendar/v3/users/me/calendarList",
                get(|| async { axum::Json(serde_json::json!({"items": [{"id": "primary"}, {"id": "team@group.calendar.google.com"}]})) }),
            )
            .route(
                "/calendar/v3/calendars/{calendar}/events",
                get(|Path(calendar): Path<String>| async move {
                    let items = if calendar == "primary" {
                        serde_json::json!([event("e1", "Standup"), {"id": "gone", "status": "cancelled"}])
                    } else {
                        serde_json::json!([event("e2", "Planning")])
                    };
                    axum::Json(serde_json::json!({"items": items}))
                }),
            )
            .route(
                "/calendar/v3/calendars/{calendar}/events/{event}",
                get(|Path((_, id)): Path<(String, String)>| async move { axum::Json(event(&id, "Standup")) }),
            )
    }

    fn host() -> HostId {
        HostId::new("google-calendar-test").expect("valid")
    }

    #[tokio::test]
    async fn enumerates_every_calendar_or_one_and_skips_cancelled_events() {
        let (api, _server) = fake_google(router()).await;
        let calendar = CalendarConnection::new(Arc::new(api), host(), 100);
        let all = calendar.enumerate("").await.expect("enumerates");
        let locators: Vec<&str> = all.iter().map(|s| s.address.locator.as_str()).collect();
        assert_eq!(
            locators,
            vec!["primary/e1", "team@group.calendar.google.com/e2"]
        );
        assert_eq!(all[0].envelope.hint.as_deref(), Some("Standup"));
        assert_eq!(all[0].envelope.modified, Some(Timestamp(1_735_787_045)));
        assert_eq!(all[0].envelope.source_type, "event");
        assert!(all[0].raw_bytes > 0);
        let one = calendar.enumerate("primary").await.expect("enumerates");
        assert_eq!(one.len(), 1);
        assert_eq!(
            calendar.locator_prefix("primary"),
            Some("primary".to_string())
        );
        assert_eq!(calendar.locator_prefix(""), Some(String::new()));
    }

    #[tokio::test]
    async fn reads_an_event_as_text() {
        let (api, _server) = fake_google(router()).await;
        let calendar = CalendarConnection::new(Arc::new(api), host(), 100);
        let address: Address = "inseam://google-calendar-test/primary/e1"
            .parse()
            .expect("address");
        let text = calendar.read_text(&address).await.expect("reads");
        assert_eq!(
            text,
            "Event: Standup\nWhen: 2025-01-03T09:00:00-05:00 – 2025-01-03T10:00:00-05:00\nWhere: Room 4\nOrganizer: Olga <o@example.com>\nAttendees: a@example.com, Bea <b@example.com>\nLink: https://calendar.google.com/e/1\n\nbring notes\n"
        );
        let flat: Address = "inseam://google-calendar-test/e1".parse().expect("address");
        assert!(matches!(
            calendar.read_text(&flat).await,
            Err(SeamError::Refused(_))
        ));
    }

    #[test]
    fn all_day_events_render_their_date() {
        let e = serde_json::json!({"summary": "Holiday", "start": {"date": "2025-12-25"}, "end": {"date": "2025-12-26"}});
        assert_eq!(
            render_event(&e),
            "Event: Holiday\nWhen: 2025-12-25 – 2025-12-26\n"
        );
    }
}
