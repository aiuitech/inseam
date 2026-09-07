//! The extraction half of the hints plugin: one LLM call per source that
//! returns, as JSON, the small pieces a searcher's question is likely to
//! land on — a synopsis, the questions the document answers (cues), the
//! internal terms it uses with plain glosses (the glossary), the exact
//! identifiers it carries, the facts that tell it apart from its near
//! duplicates (discriminators), and the entities it concerns. The
//! vocabulary — mimetypes, key namespaces, the relation — is this plugin's,
//! not the kernel's (`design/indexing.md`).

use serde::Deserialize;

use inseam_kernel::fragment::{FragmentKey, Mimetype, RelationKind};
use inseam_seams::extract::select;
use inseam_seams::text::collapse_ws;
use inseam_seams::transforms::GrantedLlm;
use inseam_seams::SeamError;

use crate::transform_entities::extract::{EntityKind, ExtractedEntity};

/// The shortest and longest text a hint, term, or identifier may carry.
const ITEM_CHARS_MIN: usize = 2;
const ITEM_CHARS_MAX: usize = 120;
/// The longest synopsis kept, in characters.
const SYNOPSIS_CHARS_MAX: usize = 400;
/// The longest gloss kept beside a term.
const GLOSS_CHARS_MAX: usize = 160;

/// A hint fragment's kind, recorded as the `kind` parameter of
/// `text/x-inseam-hint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    /// What the document is and settles, in two sentences.
    Synopsis,
    /// Questions a colleague might ask that this document answers, in plain
    /// words rather than the document's own jargon.
    Cue,
    /// The facts that tell this document apart from similar ones.
    Discriminator,
}

impl HintKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Synopsis => "synopsis",
            Self::Cue => "cue",
            Self::Discriminator => "discriminator",
        }
    }
}

/// The mimetype of a hint fragment; under `text/x-inseam-` so the sweep
/// treats it as derived understanding.
pub fn hint_mimetype(kind: HintKind) -> Mimetype {
    Mimetype::parse("text/x-inseam-hint")
        .expect("literal mimetype is valid")
        .with_param("kind", kind.as_str())
}

/// The mimetype of a glossary term: an internal word the corpus uses, with
/// its plain gloss, shared by every source that uses it.
pub fn term_mimetype() -> Mimetype {
    Mimetype::parse("text/x-inseam-term").expect("literal mimetype is valid")
}

/// The mimetype of an identifier: a ticket, a PR number, a metric or config
/// name, a version, exactly as written, shared by every source that names it.
pub fn identifier_mimetype() -> Mimetype {
    Mimetype::parse("text/x-inseam-identifier").expect("literal mimetype is valid")
}

/// The relation from a fragment to a term, identifier, or entity it uses —
/// the same edge the entity extractor draws, so the graph reads one way.
pub fn mentions() -> RelationKind {
    crate::transform_entities::extract::mentions()
}

/// An internal term with the plain words for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryTerm {
    pub term: String,
    pub gloss: String,
}

impl GlossaryTerm {
    /// One fragment per term across the index, however it is capitalised.
    pub fn key(&self) -> FragmentKey {
        FragmentKey::new(format!("term:{}", normalized(&self.term)))
            .expect("a bounded term under a literal prefix is a valid key")
    }

    /// The fragment's text: the term and its gloss, both searchable.
    pub fn text(&self) -> String {
        format!("{} — {}", self.term, self.gloss)
    }
}

/// The key of an identifier fragment.
pub fn identifier_key(identifier: &str) -> FragmentKey {
    FragmentKey::new(format!("identifier:{}", normalized(identifier)))
        .expect("a bounded identifier under a literal prefix is a valid key")
}

/// Everything one call extracted, bounded and deduplicated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hints {
    pub synopsis: String,
    pub cues: Vec<String>,
    pub glossary: Vec<GlossaryTerm>,
    pub identifiers: Vec<String>,
    pub discriminators: Vec<String>,
    pub entities: Vec<ExtractedEntity>,
}

/// The caps one extraction is made to.
#[derive(Debug, Clone, Copy)]
pub struct HintLimits {
    pub llm_input_chars: usize,
    pub cues_max: usize,
    pub glossary_max: usize,
    pub identifiers_max: usize,
    pub discriminators_max: usize,
    pub entities_max: usize,
}

/// Ask the model for the hints of `text`. A text past the input cap is
/// reduced to its telling sentences first, the way the summarizer does.
pub async fn extract(
    llm: &dyn GrantedLlm,
    hint: Option<&str>,
    text: &str,
    limits: HintLimits,
) -> Result<Hints, SeamError> {
    let input = if text.chars().count() <= limits.llm_input_chars {
        text.to_string()
    } else {
        select(text, limits.llm_input_chars)
    };
    let name = hint.unwrap_or("(unnamed source)");
    let system = system_prompt(limits);
    let raw = llm
        .complete(&system, &format!("File: {name}\n\n{input}"))
        .await?;
    Ok(parse_hints(&raw, limits))
}

fn system_prompt(limits: HintLimits) -> String {
    format!(
        "You index one document from a company's internal systems (chat, email, tickets, \
         wiki pages, code reviews, CRM records) so that colleagues can find it later by \
         asking in their own words. Reply with JSON only, no prose and no markdown, shaped \
         {{\"synopsis\": string, \"cues\": [string], \"glossary\": [{{\"term\": string, \
         \"gloss\": string}}], \"identifiers\": [string], \"discriminators\": [string], \
         \"entities\": [{{\"name\": string, \"kind\": \"person\"|\"org\"|\"project\"|\
         \"place\"|\"date\"}}]}}. \
         synopsis: at most two sentences saying what the document is and what it settles. \
         cues: up to {cues} questions a colleague might ask that this document answers, in \
         plain everyday words — where the document uses a codename, acronym, region code, \
         flag or metric name, say what it means instead of repeating it. \
         glossary: up to {glossary} internal terms the document uses (codenames, acronyms, \
         region codes, config flags, product and feature names) each with a plain gloss of \
         what it refers to. \
         identifiers: up to {identifiers} exact identifiers as written (ticket and PR \
         numbers, metric and config names, version strings, customer and tenant names). \
         discriminators: up to {discriminators} short facts that tell this document apart \
         from near-duplicates on the same topic: dates, regions, customers, versions, \
         which side of a decision or an outcome it records. \
         entities: up to {entities} people, teams, organizations, projects, or dates \
         central to it.",
        cues = limits.cues_max,
        glossary = limits.glossary_max,
        identifiers = limits.identifiers_max,
        discriminators = limits.discriminators_max,
        entities = limits.entities_max,
    )
}

#[derive(Deserialize, Default)]
struct RawTerm {
    #[serde(default)]
    term: String,
    #[serde(default)]
    gloss: String,
}

#[derive(Deserialize, Default)]
struct RawEntity {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize, Default)]
struct Reply {
    #[serde(default)]
    synopsis: String,
    #[serde(default)]
    cues: Vec<String>,
    #[serde(default)]
    glossary: Vec<RawTerm>,
    #[serde(default)]
    identifiers: Vec<String>,
    #[serde(default)]
    discriminators: Vec<String>,
    #[serde(default)]
    entities: Vec<RawEntity>,
}

/// Read the model's JSON, tolerating fences and prose around it. Anything
/// that is not JSON yields no hints: extraction is enrichment, not a gate.
pub fn parse_hints(raw: &str, limits: HintLimits) -> Hints {
    let object = match (raw.find('{'), raw.rfind('}')) {
        (Some(open), Some(close)) if open < close => &raw[open..=close],
        _ => return Hints::default(),
    };
    let Ok(reply) = serde_json::from_str::<Reply>(object) else {
        return Hints::default();
    };
    let synopsis: String = collapse_ws(&reply.synopsis)
        .chars()
        .take(SYNOPSIS_CHARS_MAX)
        .collect();
    let mut glossary = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw_term in reply.glossary {
        let term = collapse_ws(raw_term.term.trim());
        let gloss: String = collapse_ws(raw_term.gloss.trim())
            .chars()
            .take(GLOSS_CHARS_MAX)
            .collect();
        if !sized(&term) || gloss.is_empty() {
            continue;
        }
        let entry = GlossaryTerm { term, gloss };
        if seen.insert(entry.key()) {
            glossary.push(entry);
        }
        if glossary.len() >= limits.glossary_max {
            break;
        }
    }
    let mut entities = Vec::new();
    let mut seen_entities = std::collections::HashSet::new();
    for raw_entity in reply.entities {
        let name = collapse_ws(raw_entity.name.trim());
        if !sized(&name) {
            continue;
        }
        let kind = raw_entity.kind.parse().unwrap_or(EntityKind::Other);
        let entity = ExtractedEntity { name, kind };
        if seen_entities.insert(entity.key()) {
            entities.push(entity);
        }
        if entities.len() >= limits.entities_max {
            break;
        }
    }
    Hints {
        synopsis,
        cues: clean_list(reply.cues, limits.cues_max),
        glossary,
        identifiers: clean_identifiers(reply.identifiers, limits.identifiers_max),
        discriminators: clean_list(reply.discriminators, limits.discriminators_max),
        entities,
    }
}

/// Trim, collapse, drop empties and oversized items, keep order, cap.
fn clean_list(items: Vec<String>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let cleaned = collapse_ws(item.trim());
        if sized(&cleaned) && !out.contains(&cleaned) {
            out.push(cleaned);
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Identifiers dedupe by their normalized key, so `SUP-100432` and
/// `sup-100432` are one.
fn clean_identifiers(items: Vec<String>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let cleaned = collapse_ws(item.trim());
        if !sized(&cleaned) {
            continue;
        }
        if seen.insert(normalized(&cleaned)) {
            out.push(cleaned);
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

fn sized(item: &str) -> bool {
    let count = item.chars().count();
    (ITEM_CHARS_MIN..=ITEM_CHARS_MAX).contains(&count)
}

/// The dedup form of a term or identifier: lowercase, whitespace collapsed,
/// surrounding punctuation dropped.
fn normalized(item: &str) -> String {
    collapse_ws(item)
        .to_lowercase()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: HintLimits = HintLimits {
        llm_input_chars: 8_000,
        cues_max: 3,
        glossary_max: 3,
        identifiers_max: 3,
        discriminators_max: 3,
        entities_max: 3,
    };

    #[test]
    fn parses_every_field_through_fences() {
        let raw = "```json\n{\"synopsis\": \"Bootstrap of the Frankfurt cluster.\", \
            \"cues\": [\"How do we bring up a new GPU cluster in Frankfurt?\", \" \", \
            \"How do we bring up a new GPU cluster in Frankfurt?\"], \
            \"glossary\": [{\"term\": \"eu-central-2\", \"gloss\": \"the Frankfurt region\"}, \
            {\"term\": \"EU-CENTRAL-2\", \"gloss\": \"dup\"}, {\"term\": \"x\", \"gloss\": \"short\"}], \
            \"identifiers\": [\"SUP-100432\", \"sup-100432\", \"p4d.24xlarge\"], \
            \"discriminators\": [\"2026-03-10\", \"Frankfurt\"], \
            \"entities\": [{\"name\": \"Maria\", \"kind\": \"person\"}]}\n```";
        let hints = parse_hints(raw, LIMITS);
        assert_eq!(hints.synopsis, "Bootstrap of the Frankfurt cluster.");
        assert_eq!(hints.cues.len(), 1, "blank and duplicate cues go");
        assert_eq!(hints.glossary.len(), 1, "case-insensitive dedup, short terms go");
        assert_eq!(hints.glossary[0].key().as_str(), "term:eu-central-2");
        assert_eq!(hints.identifiers, vec!["SUP-100432", "p4d.24xlarge"]);
        assert_eq!(hints.discriminators.len(), 2);
        assert_eq!(hints.entities[0].kind, EntityKind::Person);
    }

    #[test]
    fn caps_every_list() {
        let raw = r#"{"cues": ["a1", "a2", "a3", "a4"], "identifiers": ["i1", "i2", "i3", "i4"]}"#;
        let hints = parse_hints(raw, LIMITS);
        assert_eq!(hints.cues.len(), 3);
        assert_eq!(hints.identifiers.len(), 3);
        assert!(hints.synopsis.is_empty());
    }

    #[test]
    fn garbage_yields_no_hints() {
        assert_eq!(parse_hints("no json here", LIMITS), Hints::default());
        assert_eq!(parse_hints("{not json}", LIMITS), Hints::default());
    }

    #[test]
    fn identifier_keys_normalize_case_and_edges() {
        assert_eq!(identifier_key(" SUP-100432. ").as_str(), "identifier:sup-100432");
    }
}
