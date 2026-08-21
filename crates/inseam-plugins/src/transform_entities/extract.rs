//! The entity extractor: pulls people, places, organizations, projects and
//! dates out as entity fragments, deduplicated per index, with `mentions`
//! relations back to every fragment that referenced them. Entities are the
//! graph's connective tissue: two unrelated sources mentioning the same
//! person end up one hop apart (`design/indexing.md`).

use serde::Deserialize;

use inseam_seams::text::collapse_ws;
use inseam_seams::transforms::{EntityKind, ExtractedEntity, GrantedLlm};
use inseam_seams::SeamError;

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
        assert_eq!(out[0].key(), "person:greg hunt");
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
