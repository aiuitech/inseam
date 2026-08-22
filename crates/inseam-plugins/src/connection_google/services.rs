//! The catalog of Google services one Workspace account exposes, as inseam
//! sees them: each is its own **host kind** (`design/roster.md` — the kind
//! is the locator-schema family, and a Gmail message id and a Drive file id
//! are different schemas), carries the OAuth scope that reads it, and is
//! stewarded read-only. The grant the plugin registers is the union of the
//! enabled services' scopes plus the identity scopes every Google grant
//! needs to learn which account signed in.

use serde::{Deserialize, Serialize};

use inseam_seams::connection::HostKind;

/// The OpenID Connect scopes that make the token endpoint return an
/// `id_token` naming the account — how the host ids are derived without a
/// further API call.
pub const IDENTITY_SCOPES: [&str; 2] = ["openid", "https://www.googleapis.com/auth/userinfo.email"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoogleService {
    Gmail,
    Drive,
    Calendar,
    Contacts,
    Tasks,
}

impl GoogleService {
    /// Every service, in the order status surfaces list them.
    pub const ALL: [Self; 5] = [
        Self::Gmail,
        Self::Drive,
        Self::Calendar,
        Self::Contacts,
        Self::Tasks,
    ];

    /// The host kind — the locator-schema family — this service is.
    pub fn kind(self) -> HostKind {
        let kind = match self {
            Self::Gmail => "gmail",
            Self::Drive => "google-drive",
            Self::Calendar => "google-calendar",
            Self::Contacts => "google-contacts",
            Self::Tasks => "google-tasks",
        };
        HostKind::new(kind).expect("service kinds are valid host kinds")
    }

    /// The read-only OAuth scope that unlocks the service.
    pub fn scope(self) -> &'static str {
        match self {
            Self::Gmail => "https://www.googleapis.com/auth/gmail.readonly",
            Self::Drive => "https://www.googleapis.com/auth/drive.readonly",
            Self::Calendar => "https://www.googleapis.com/auth/calendar.readonly",
            Self::Contacts => "https://www.googleapis.com/auth/contacts.readonly",
            Self::Tasks => "https://www.googleapis.com/auth/tasks.readonly",
        }
    }

    /// Owner-facing name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gmail => "Gmail",
            Self::Drive => "Google Drive",
            Self::Calendar => "Google Calendar",
            Self::Contacts => "Google Contacts",
            Self::Tasks => "Google Tasks",
        }
    }

    /// What a sweep scope (`inseam index --host <id> <root>`) means for the
    /// service, for docs and status surfaces.
    pub fn scope_help(self) -> &'static str {
        match self {
            Self::Gmail => "a Gmail search (`label:INBOX`, `newer_than:30d`); empty for all mail",
            Self::Drive => "a folder id, swept with its subfolders; empty for every file",
            Self::Calendar => "a calendar id (`primary`); empty for every calendar",
            Self::Contacts => "empty; the account's contacts",
            Self::Tasks => "a task-list id; empty for every list",
        }
    }

    /// The composition's spelling (`gmail`, `drive`, …).
    pub fn name(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::Drive => "drive",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::Tasks => "tasks",
        }
    }
}

/// The scopes a grant over `services` must declare: identity first, then
/// each service's, in catalog order, without duplicates.
pub fn scopes_for(services: &[GoogleService]) -> Vec<String> {
    let mut scopes: Vec<String> = IDENTITY_SCOPES.iter().map(|s| s.to_string()).collect();
    for service in GoogleService::ALL {
        if services.contains(&service) {
            scopes.push(service.scope().to_string());
        }
    }
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_service_has_a_distinct_kind_scope_and_name() {
        let mut kinds: Vec<String> = GoogleService::ALL.iter().map(|s| s.kind().to_string()).collect();
        let mut scopes: Vec<&str> = GoogleService::ALL.iter().map(|s| s.scope()).collect();
        let mut names: Vec<&str> = GoogleService::ALL.iter().map(|s| s.name()).collect();
        kinds.sort();
        kinds.dedup();
        assert_eq!(kinds.len(), GoogleService::ALL.len());
        scopes.sort();
        scopes.dedup();
        names.sort();
        names.dedup();
        assert_eq!(scopes.len(), GoogleService::ALL.len());
        assert_eq!(names.len(), GoogleService::ALL.len());
    }

    #[test]
    fn scopes_are_identity_then_services_in_catalog_order() {
        let scopes = scopes_for(&[GoogleService::Tasks, GoogleService::Gmail, GoogleService::Gmail]);
        assert_eq!(
            scopes,
            vec![
                "openid",
                "https://www.googleapis.com/auth/userinfo.email",
                "https://www.googleapis.com/auth/gmail.readonly",
                "https://www.googleapis.com/auth/tasks.readonly",
            ]
        );
        assert_eq!(scopes_for(&[]).len(), IDENTITY_SCOPES.len());
    }

    #[test]
    fn services_spell_themselves_in_kebab_case() {
        let json = serde_json::to_string(&GoogleService::Calendar).expect("serializes");
        assert_eq!(json, "\"calendar\"");
        let back: GoogleService = serde_json::from_str("\"drive\"").expect("parses");
        assert_eq!(back, GoogleService::Drive);
    }
}
