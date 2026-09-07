//! The extraction half of the entity plugin: ask the granted LLM for the
//! entities central to a text and parse its reply. The vocabulary — entity
//! kinds, the dedup key, the mimetype and relation the plugin emits — is
//! defined here, because it is this plugin's, not the kernel's
//! (`design/indexing.md`).

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

use inseam_kernel::fragment::{FragmentKey, Mimetype, RelationKind};
use inseam_seams::text::collapse_ws;
use inseam_seams::transforms::GrantedLlm;
use inseam_seams::SeamError;

/// The mimetype of an entity fragment; under `text/x-inseam-` so the sweep
/// treats it as derived understanding (never re-decomposed, never claimed).
pub fn entity_mimetype(kind: EntityKind) -> Mimetype {
    Mimetype::parse("text/x-inseam-entity")
        .expect("literal mimetype is valid")
        .with_param("kind", kind.as_str())
}

/// The relation from a fragment to an entity it references.
pub fn mentions() -> RelationKind {
    RelationKind::new("mentions").expect("literal relation kind is valid")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityKind {
    Person,
    Place,
    Org,
    Project,
    Date,
    Other,
}

impl EntityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Place => "place",
            Self::Org => "org",
            Self::Project => "project",
            Self::Date => "date",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EntityKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "person" | "people" => Ok(Self::Person),
            "place" | "location" => Ok(Self::Place),
            "org" | "organization" | "organisation" | "company" => Ok(Self::Org),
            "project" => Ok(Self::Project),
            "date" | "time" => Ok(Self::Date),
            _ => Ok(Self::Other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedEntity {
    pub name: String,
    pub kind: EntityKind,
}

impl ExtractedEntity {
    /// The per-index deduplication key: one fragment per entity, however
    /// many sources mention it. Namespaced under `entity:` so no other
    /// plugin's keyed fragments collide with it.
    pub fn key(&self) -> FragmentKey {
        FragmentKey::new(format!(
            "entity:{}:{}",
            self.kind,
            collapse_ws(&self.name).to_lowercase()
        ))
        .expect("a bounded name under a literal prefix is a valid key")
    }
}

/// Characters of source text an extraction call sees.
const LLM_INPUT_CHARS: usize = 8_000;

/// Ask the model for the entities central to `text`. Returns an empty list
/// rather than erroring on parse trouble; extraction is enrichment, not a
/// gate.
pub async fn extract(
    llm: &dyn GrantedLlm,
    hint: Option<&str>,
    text: &str,
    max: usize,
) -> Result<Vec<ExtractedEntity>, SeamError> {
    let bounded: String = text.chars().take(LLM_INPUT_CHARS).collect();
    let name = hint.unwrap_or("(unnamed source)");
    let system = format!(
        "Extract up to {max} distinct named entities central to the document: \
         specific people, places, organizations, projects, or dates. Skip generic \
         terms, file formats, and incidental words. Reply with a JSON array only, \
         no prose, each item {{\"name\": string, \"kind\": \
         \"person\"|\"place\"|\"org\"|\"project\"|\"date\"}}."
    );
    let raw = llm
        .complete(&system, &format!("File: {name}\n\n{bounded}"))
        .await?;
    Ok(parse_entities(&raw, max))
}

#[derive(Deserialize)]
struct RawEntity {
    name: String,
    #[serde(default)]
    kind: String,
}

/// Parse a model reply into entities, tolerating code fences and prose
/// around the array.
pub fn parse_entities(raw: &str, max: usize) -> Vec<ExtractedEntity> {
    let start = raw.find('[');
    let end = raw.rfind(']');
    let slice = match (start, end) {
        (Some(s), Some(e)) if s < e => &raw[s..=e],
        _ => return Vec::new(),
    };
    let parsed: Vec<RawEntity> = serde_json::from_str(slice).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    parsed
        .into_iter()
        .filter_map(|r| {
            let name = collapse_ws(r.name.trim());
            let count = name.chars().count();
            if !(2..=80).contains(&count) {
                return None;
            }
            // A control character in a model's reply is not a name, and the
            // key built from it would be refused — the proof behind `key()`.
            if name.chars().any(char::is_control) {
                return None;
            }
            let kind = r.kind.parse().unwrap_or(EntityKind::Other);
            let entity = ExtractedEntity { name, kind };
            seen.insert(entity.key()).then_some(entity)
        })
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_array() {
        let out = parse_entities(
            r#"[{"name":"Greg Hunt","kind":"person"},{"name":"Kitchen Reno","kind":"project"}]"#,
            10,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, EntityKind::Person);
        assert_eq!(out[0].key().as_str(), "entity:person:greg hunt");
    }

    #[test]
    fn parses_through_code_fences_and_prose() {
        let out = parse_entities(
            "Here you go:\n```json\n[{\"name\": \"Toronto\", \"kind\": \"place\"}]\n```\n",
            10,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EntityKind::Place);
    }

    #[test]
    fn deduplicates_and_caps() {
        let out = parse_entities(
            r#"[{"name":"Greg","kind":"person"},{"name":"greg","kind":"person"},
               {"name":"Ada","kind":"person"},{"name":"Sam","kind":"person"}]"#,
            2,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "Greg");
        assert_eq!(out[1].name, "Ada");
    }

    #[test]
    fn drops_junk_names_and_unknown_kinds_default_to_other() {
        let out = parse_entities(
            r#"[{"name":"x","kind":"person"},{"name":"Widget Co","kind":"conglomerate"}]"#,
            10,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EntityKind::Other);
    }

    #[test]
    fn garbage_replies_yield_nothing() {
        assert!(parse_entities("no entities here", 5).is_empty());
        assert!(parse_entities("]) [", 5).is_empty());
    }
}
