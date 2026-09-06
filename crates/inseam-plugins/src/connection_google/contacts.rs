//! Google Contacts (the People API) as a host: every contact is a source
//! whose locator is the person's id (`c123…`, the `people/` resource name
//! minus its prefix). The connection serves a contact card as text — names,
//! addresses, organizations, notes — so the envelope says `text/plain`. The
//! account's contacts are one flat set, so the only scope is the whole host.

use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{Address, HostId, Locator, Timestamp};
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::dates::parse_rfc3339_epoch;
use inseam_seams::text::slice_lines;
use inseam_seams::SeamError;

use super::api::{opt_str_field, rendered, str_field, GoogleApi};

/// Contacts per page — the People API's maximum.
const PAGE_SIZE: &str = "1000";

const PERSON_FIELDS: &str =
    "names,emailAddresses,phoneNumbers,organizations,biographies,urls,addresses,birthdays,metadata";

const RESOURCE_PREFIX: &str = "people/";

pub struct ContactsConnection {
    api: Arc<GoogleApi>,
    host: HostId,
    sources_max: usize,
}

impl ContactsConnection {
    pub fn new(api: Arc<GoogleApi>, host: HostId, sources_max: usize) -> Self {
        Self {
            api,
            host,
            sources_max,
        }
    }

    fn person_id<'a>(&self, address: &'a Address) -> Result<&'a str, SeamError> {
        if address.host != self.host {
            return Err(SeamError::failed(format!(
                "address {address} names host `{}`, but this connection stewards `{}`",
                address.host, self.host
            )));
        }
        let id = address.locator.as_str();
        let valid = id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            return Err(SeamError::Refused(format!("`{id}` is not a contact id")));
        }
        Ok(id)
    }

    fn source_of(&self, person: &serde_json::Value, observed: Timestamp) -> Option<EnumeratedSource> {
        let resource = opt_str_field(person, "resourceName")?;
        let id = resource.strip_prefix(RESOURCE_PREFIX).unwrap_or(&resource);
        let locator = Locator::new(id).ok()?;
        let text = render_person(person);
        let r = rendered(
            "contact",
            &text,
            None,
            updated_at(person),
            observed,
            display_name(person),
        );
        Some(EnumeratedSource {
            address: Address::new(self.host.clone(), locator),
            envelope: r.envelope,
            raw_bytes: r.raw_bytes,
        })
    }
}

/// The latest `updateTime` across the person's sources, when any says.
fn updated_at(person: &serde_json::Value) -> Option<Timestamp> {
    person
        .get("metadata")?
        .get("sources")?
        .as_array()?
        .iter()
        .filter_map(|s| parse_rfc3339_epoch(str_field(s, "updateTime")).ok())
        .max()
        .map(Timestamp)
}

fn display_name(person: &serde_json::Value) -> Option<String> {
    values_of(person, "names", "displayName")
        .into_iter()
        .next()
        .or_else(|| values_of(person, "emailAddresses", "value").into_iter().next())
}

/// The `key` field of every entry under `list`, in order, non-empty.
fn values_of(person: &serde_json::Value, list: &str, key: &str) -> Vec<String> {
    person
        .get(list)
        .and_then(|l| l.as_array())
        .map(|entries| entries.iter().filter_map(|e| opt_str_field(e, key)).collect())
        .unwrap_or_default()
}

/// The contact card as the index reads it.
pub fn render_person(person: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str("Contact: ");
    out.push_str(&display_name(person).unwrap_or_default());
    out.push('\n');
    let lines: [(&str, Vec<String>); 5] = [
        ("Email", values_of(person, "emailAddresses", "value")),
        ("Phone", values_of(person, "phoneNumbers", "value")),
        ("Organization", organizations(person)),
        ("Address", values_of(person, "addresses", "formattedValue")),
        ("URL", values_of(person, "urls", "value")),
    ];
    for (label, values) in lines {
        if !values.is_empty() {
            out.push_str(&format!("{label}: {}\n", values.join(", ")));
        }
    }
    if let Some(birthday) = person
        .get("birthdays")
        .and_then(|b| b.as_array())
        .and_then(|b| b.iter().find_map(|entry| opt_str_field(entry, "text")))
    {
        out.push_str(&format!("Birthday: {birthday}\n"));
    }
    let notes = values_of(person, "biographies", "value");
    if !notes.is_empty() {
        out.push('\n');
        out.push_str(notes.join("\n").trim_end());
        out.push('\n');
    }
    out
}

fn organizations(person: &serde_json::Value) -> Vec<String> {
    person
        .get("organizations")
        .and_then(|o| o.as_array())
        .map(|orgs| {
            orgs.iter()
                .filter_map(|o| {
                    let name = opt_str_field(o, "name");
                    let title = opt_str_field(o, "title");
                    match (title, name) {
                        (Some(title), Some(name)) => Some(format!("{title} at {name}")),
                        (None, Some(name)) => Some(name),
                        (Some(title), None) => Some(title),
                        (None, None) => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl Connection for ContactsConnection {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let root = root.trim();
        if !root.is_empty() && root != "connections" {
            return Err(SeamError::Refused(format!(
                "contacts have no `{root}` scope; sweep the whole host (an empty root)"
            )));
        }
        let observed = Timestamp::from(SystemTime::now());
        let people = self
            .api
            .list_pages(
                self.api.people_url(
                    &["v1", "people", "me", "connections"],
                    &[("personFields", PERSON_FIELDS), ("pageSize", PAGE_SIZE)],
                ),
                "connections",
                self.sources_max,
            )
            .await?;
        Ok(people
            .iter()
            .filter_map(|p| self.source_of(p, observed))
            .collect())
    }

    fn locator_prefix(&self, _root: &str) -> Option<String> {
        Some(String::new())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let id = self.person_id(address)?;
        let person = self
            .api
            .get_json(
                self.api
                    .people_url(&["v1", "people", id], &[("personFields", PERSON_FIELDS)]),
            )
            .await?;
        Ok(render_person(&person))
    }

    async fn read_lines(&self, address: &Address, start: u64, end: u64) -> Result<String, SeamError> {
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

    fn person(id: &str) -> serde_json::Value {
        serde_json::json!({
            "resourceName": format!("people/{id}"),
            "names": [{"displayName": "Ada Lovelace"}],
            "emailAddresses": [{"value": "ada@example.com"}],
            "phoneNumbers": [{"value": "+1 555 0100"}],
            "organizations": [{"name": "Analytical Engines", "title": "Engineer"}],
            "biographies": [{"value": "First programmer."}],
            "metadata": {"sources": [{"updateTime": "2025-01-02T03:04:05Z"}, {"updateTime": "2024-01-01T00:00:00Z"}]}
        })
    }

    fn router() -> axum::Router {
        axum::Router::new()
            .route(
                "/v1/people/me/connections",
                get(|| async { axum::Json(serde_json::json!({"connections": [person("c1"), person("c2")]})) }),
            )
            .route(
                "/v1/people/{id}",
                get(|Path(id): Path<String>| async move { axum::Json(person(&id)) }),
            )
    }

    fn host() -> HostId {
        HostId::new("google-contacts-test").expect("valid")
    }

    #[tokio::test]
    async fn enumerates_contacts_with_names_and_update_times() {
        let (api, _server) = fake_google(router()).await;
        let contacts = ContactsConnection::new(Arc::new(api), host(), 100);
        let sources = contacts.enumerate("").await.expect("enumerates");
        let ids: Vec<&str> = sources.iter().map(|s| s.address.locator.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c2"]);
        assert_eq!(sources[0].envelope.hint.as_deref(), Some("Ada Lovelace"));
        assert_eq!(sources[0].envelope.modified, Some(Timestamp(1_735_787_045)), "the latest source wins");
        assert_eq!(sources[0].envelope.source_type, "contact");
        assert!(matches!(contacts.enumerate("starred").await, Err(SeamError::Refused(_))));
        assert_eq!(contacts.locator_prefix(""), Some(String::new()));
    }

    #[tokio::test]
    async fn reads_a_contact_card() {
        let (api, _server) = fake_google(router()).await;
        let contacts = ContactsConnection::new(Arc::new(api), host(), 100);
        let address: Address = "inseam://google-contacts-test/c1".parse().expect("address");
        assert_eq!(
            contacts.read_text(&address).await.expect("reads"),
            "Contact: Ada Lovelace\nEmail: ada@example.com\nPhone: +1 555 0100\nOrganization: Engineer at Analytical Engines\n\nFirst programmer.\n"
        );
    }
}
