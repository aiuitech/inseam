//! Grounding a cluster with the model (`design/vocabulary.md`): once per
//! changed cluster, the model reads the cluster's rows with excerpts of
//! where they appear and answers three things statistics cannot — which
//! rows are the same thing, what each means in plain words, and what a
//! searcher would say instead. Thousands of calls at most, never one per
//! document; with no model or no budget the pass still mines, matches,
//! anchors, and clusters, and only glosses and aliases wait.

use serde::Deserialize;

use inseam_kernel::store::{VocabularyRow, normalize_spelling};
use inseam_seams::SeamError;
use inseam_seams::text::collapse_ws;
use inseam_seams::transforms::GrantedLlm;

/// Longest gloss kept.
pub const GLOSS_CHARS_MAX: usize = 160;
/// Longest alias kept; shortest too.
pub const ALIAS_CHARS_MAX: usize = 80;
pub const ALIAS_CHARS_MIN: usize = 3;
/// Longest excerpt handed to the model per source.
pub const EXCERPT_CHARS_MAX: usize = 400;
/// Most rows one call reads.
pub const ROWS_PER_CALL_MAX: usize = 48;

/// What the model settled about one cluster, cleaned and bounded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grounding {
    /// `(survivor, loser)` by normalized spelling, both members.
    pub merges: Vec<(String, String)>,
    /// `(member, gloss)`.
    pub glosses: Vec<(String, String)>,
    /// `(member, aliases)`.
    pub aliases: Vec<(String, Vec<String>)>,
}

/// One call for one cluster.
pub async fn ground(
    llm: &dyn GrantedLlm,
    members: &[VocabularyRow],
    excerpts: &[String],
    aliases_per_row_max: usize,
) -> Result<Grounding, SeamError> {
    assert!(!members.is_empty());
    let raw = llm
        .complete(
            &system_prompt(aliases_per_row_max),
            &user_prompt(members, excerpts),
        )
        .await?;
    Ok(parse(&raw, members, aliases_per_row_max))
}

fn system_prompt(aliases_per_row_max: usize) -> String {
    format!(
        "You curate the private vocabulary of one organization's documents: product and \
         project names, codenames, region and cluster codes, ticket and metric names, \
         customers, people, and the jargon a team uses. You are given the words of one \
         topic cluster with how many documents use each, and short excerpts where they \
         appear. Reply with JSON only, no prose and no markdown, shaped \
         {{\"merges\": [{{\"keep\": string, \"drop\": string}}], \
         \"glosses\": [{{\"term\": string, \"gloss\": string}}], \
         \"aliases\": [{{\"term\": string, \"aliases\": [string]}}]}}. \
         merges: pairs of listed words that name the same thing (a spelling variant, \
         an abbreviation and its expansion, a name with and without a surname); keep the \
         fuller spelling. Only list pairs you are sure of. \
         glosses: for each listed word, one plain sentence saying what it refers to in \
         these documents, at most twenty words; skip words you cannot tell. \
         aliases: for each listed word, up to {aliases} short phrases a colleague might \
         type when looking for it without knowing the word — plain descriptions \
         (\"the 80GB accelerator\"), expansions, common misspellings — never the word \
         itself and never generic phrases that fit any topic.",
        aliases = aliases_per_row_max
    )
}

fn user_prompt(members: &[VocabularyRow], excerpts: &[String]) -> String {
    let mut out = String::from("Words:\n");
    for row in members.iter().take(ROWS_PER_CALL_MAX) {
        out.push_str(&format!(
            "- {} ({} documents, {})\n",
            row.spelling,
            row.document_frequency,
            row.kind.as_str()
        ));
    }
    if !excerpts.is_empty() {
        out.push_str("\nExcerpts:\n");
        for excerpt in excerpts {
            out.push_str("- ");
            out.push_str(&inseam_seams::text::truncate_chars(
                &collapse_ws(excerpt),
                EXCERPT_CHARS_MAX,
            ));
            out.push('\n');
        }
    }
    out
}

#[derive(Deserialize, Default)]
struct RawMerge {
    #[serde(default)]
    keep: String,
    #[serde(default)]
    drop: String,
}

#[derive(Deserialize, Default)]
struct RawGloss {
    #[serde(default)]
    term: String,
    #[serde(default)]
    gloss: String,
}

#[derive(Deserialize, Default)]
struct RawAliases {
    #[serde(default)]
    term: String,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Reply {
    #[serde(default)]
    merges: Vec<RawMerge>,
    #[serde(default)]
    glosses: Vec<RawGloss>,
    #[serde(default)]
    aliases: Vec<RawAliases>,
}

/// Read the model's JSON, tolerating fences and prose around it, and keep
/// only what names a listed member. Anything that is not JSON grounds
/// nothing: grounding is enrichment, not a gate.
pub fn parse(raw: &str, members: &[VocabularyRow], aliases_per_row_max: usize) -> Grounding {
    let object = match (raw.find('{'), raw.rfind('}')) {
        (Some(open), Some(close)) if open < close => &raw[open..=close],
        _ => return Grounding::default(),
    };
    let Ok(reply) = serde_json::from_str::<Reply>(object) else {
        return Grounding::default();
    };
    let is_member = |spelling: &str| members.iter().any(|m| m.normalized == spelling);
    let mut grounding = Grounding::default();
    let mut dropped: Vec<String> = Vec::new();
    for merge in reply.merges {
        let keep = normalize_spelling(&merge.keep);
        let drop = normalize_spelling(&merge.drop);
        if keep == drop || !is_member(&keep) || !is_member(&drop) || dropped.contains(&drop) {
            continue;
        }
        dropped.push(drop.clone());
        grounding.merges.push((keep, drop));
    }
    for gloss in reply.glosses {
        let term = normalize_spelling(&gloss.term);
        let text: String = collapse_ws(gloss.gloss.trim())
            .chars()
            .take(GLOSS_CHARS_MAX)
            .collect();
        if !is_member(&term) || text.is_empty() || dropped.contains(&term) {
            continue;
        }
        if grounding.glosses.iter().all(|(t, _)| *t != term) {
            grounding.glosses.push((term, text));
        }
    }
    for entry in reply.aliases {
        let term = normalize_spelling(&entry.term);
        if !is_member(&term) || dropped.contains(&term) {
            continue;
        }
        let aliases = clean_aliases(&term, entry.aliases, aliases_per_row_max);
        if !aliases.is_empty() && grounding.aliases.iter().all(|(t, _)| *t != term) {
            grounding.aliases.push((term, aliases));
        }
    }
    grounding
}

/// Trim, collapse, bound, drop the word itself and duplicates, cap.
fn clean_aliases(term: &str, raw: Vec<String>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for alias in raw {
        let cleaned = collapse_ws(alias.trim());
        let normalized = normalize_spelling(&cleaned);
        let sized = (ALIAS_CHARS_MIN..=ALIAS_CHARS_MAX).contains(&cleaned.chars().count());
        if !sized || normalized == term || normalized.is_empty() {
            continue;
        }
        if out
            .iter()
            .any(|kept| normalize_spelling(kept) == normalized)
        {
            continue;
        }
        out.push(cleaned);
        if out.len() >= max {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::fragment::{FragmentId, FragmentKey};
    use inseam_kernel::store::{VocabularyKind, VocabularyOrigin};

    fn member(spelling: &str) -> VocabularyRow {
        VocabularyRow {
            fragment: FragmentId(1),
            key: FragmentKey::new(format!("term:{spelling}")).expect("valid"),
            kind: VocabularyKind::Term,
            origin: VocabularyOrigin::Mined,
            spelling: spelling.to_string(),
            normalized: normalize_spelling(spelling),
            document_frequency: 3,
            cluster: None,
            gloss: None,
        }
    }

    #[test]
    fn parse_keeps_only_members_and_bounds_everything() {
        let members = vec![member("H200"), member("h200-80"), member("Redwood")];
        let raw = r#"Sure: {"merges": [{"keep": "H200", "drop": "h200-80"}, {"keep": "H200", "drop": "unknown"}],
            "glosses": [{"term": "h200", "gloss": " the 80 GB accelerator "}, {"term": "h200-80", "gloss": "dropped"}, {"term": "zzz", "gloss": "x"}],
            "aliases": [{"term": "Redwood", "aliases": ["the inference product", "redwood", "the inference product", "ab", "x y z", "fourth", "fifth"]}]}"#;
        let grounding = parse(raw, &members, 4);
        assert_eq!(
            grounding.merges,
            vec![("h200".to_string(), "h200-80".to_string())]
        );
        assert_eq!(
            grounding.glosses,
            vec![("h200".to_string(), "the 80 GB accelerator".to_string())]
        );
        assert_eq!(grounding.aliases.len(), 1);
        assert_eq!(
            grounding.aliases[0].1,
            vec!["the inference product", "x y z", "fourth", "fifth"]
        );
        assert_eq!(parse("no json here", &members, 4), Grounding::default());
    }

    #[test]
    fn prompts_name_every_member_and_excerpt() {
        let members = vec![member("H200")];
        let prompt = user_prompt(&members, &["  spaced   excerpt ".to_string()]);
        assert!(prompt.contains("- H200 (3 documents, term)"));
        assert!(prompt.contains("- spaced excerpt"));
        assert!(system_prompt(4).contains("up to 4 short phrases"));
    }
}
