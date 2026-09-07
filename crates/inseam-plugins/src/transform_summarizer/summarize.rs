//! The summarizer: the one mandatory transform. The Finder serves summaries
//! in every response so an AI client can decide whether to keep digging, so
//! every indexed source must have one (`design/indexing.md`). Profiles
//! configure the length, not the existence, and the ladder runs: verbatim
//! when the text already fits the length (no call to make), LLM when
//! available and budgeted, extractive for text otherwise, envelope-derived
//! for everything else.
//!
//! The summary is written to be found: it answers what someone would be
//! looking for when this source is the right one, in a searcher's words.
//! Beside it ride the keywords — the terms that name the source — for the
//! full-text side of the index; the same LLM call produces both.

use inseam_kernel::address::Envelope;
use inseam_seams::dates::ymd;
use inseam_seams::extract;
use inseam_seams::text::{collapse_ws, truncate_chars};
use inseam_seams::transforms::GrantedLlm;

/// How a summary came to be. Recorded on the summary fragment's mimetype as
/// the `via` parameter, so provenance travels with the fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    /// The text fit the target length, so it is its own summary.
    Verbatim,
    Llm,
    Extractive,
    Envelope,
}

impl SummaryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verbatim => "verbatim",
            Self::Llm => "llm",
            Self::Extractive => "extractive",
            Self::Envelope => "envelope",
        }
    }
}

/// The dials a summary is made to.
#[derive(Debug, Clone, Copy)]
pub struct SummaryShape {
    /// Target summary length in characters.
    pub target_chars: usize,
    /// Characters of source text an LLM call sees; a longer text is
    /// reduced to its telling sentences first ([`extract::select`]).
    pub llm_input_chars: usize,
    /// Keywords kept beside the summary.
    pub keywords_max: usize,
}

/// A summary with the keywords made beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub text: String,
    pub kind: SummaryKind,
    /// The terms that name the source, best first; empty for a verbatim
    /// summary, which is the whole text, and for an envelope summary, which
    /// saw no text.
    pub keywords: Vec<String>,
}

/// Summarize source text. Never errors — the mandatory-summary invariant
/// wins: verbatim when the text fits, the LLM when the capability was
/// granted, extractive on any failure.
pub async fn summarize_text(
    llm: Option<&dyn GrantedLlm>,
    hint: Option<&str>,
    text: &str,
    shape: SummaryShape,
) -> Summary {
    assert!(!text.trim().is_empty(), "text summaries need text");
    // Keywords carry the terms a summary left out; a verbatim summary left
    // nothing out, and a keyword row beside the whole text only hands
    // full-text search a second, noisier copy of the same terms.
    if text.chars().count() <= shape.target_chars {
        return Summary {
            text: collapse_ws(text),
            kind: SummaryKind::Verbatim,
            keywords: Vec::new(),
        };
    }
    if let Some(llm) = llm {
        match llm_summary(llm, hint, text, shape).await {
            Ok(summary) if !summary.text.is_empty() => return summary,
            Ok(_) => tracing::debug!("llm returned an empty summary; falling back"),
            Err(e) => tracing::warn!("llm summary failed, falling back to extractive: {e}"),
        }
    }
    extractive(text, shape)
}

/// The longest first line taken as a plain-text title. Subject lines,
/// issue summaries, and page names fit well within it; a paragraph does not.
const TITLE_CHARS_MAX: usize = 160;

/// A document's title — its first line when that line is a markdown
/// heading, or a short line standing alone before a blank line, the way an
/// export names an email, a ticket, or a page — and the text after it. The
/// selection treats headings as section labels, not sentences, so the title
/// has to be carried separately: it is the one line most likely to be what
/// a searcher types.
fn title_and_body(text: &str) -> (Option<String>, &str) {
    let trimmed = text.trim_start();
    let mut lines = trimmed.lines();
    let Some(first) = lines.next() else {
        return (None, text);
    };
    let heading = first.trim_start().starts_with('#');
    let title = first.trim().trim_start_matches('#').trim();
    if title.is_empty() {
        return (None, text);
    }
    let body = &trimmed[first.len()..];
    if heading {
        return (Some(extract::strip_links(title)), body);
    }
    let stands_alone = lines.next().is_some_and(|second| second.trim().is_empty());
    if stands_alone && title.chars().count() <= TITLE_CHARS_MAX {
        (Some(extract::strip_links(title)), body)
    } else {
        (None, text)
    }
}

/// The title, then the text's telling sentences within what the title
/// leaves of the budget.
fn title_led_selection(text: &str, budget_chars: usize) -> String {
    let (title, body) = title_and_body(text);
    match title {
        Some(title) => {
            let title_chars = title.chars().count() + 2;
            let selected = extract::select(body, budget_chars.saturating_sub(title_chars));
            if selected.is_empty() {
                title
            } else {
                format!("{title}. {selected}")
            }
        }
        None => extract::select(text, budget_chars),
    }
}

async fn llm_summary(
    llm: &dyn GrantedLlm,
    hint: Option<&str>,
    text: &str,
    shape: SummaryShape,
) -> Result<Summary, inseam_seams::SeamError> {
    let input = title_led_selection(text, shape.llm_input_chars);
    let name = hint.unwrap_or("(unnamed source)");
    let system = format!(
        "You write the entry a search index keeps for a file, so that someone looking for \
         it finds it. Reply with JSON only, no prose and no markdown, shaped \
         {{\"summary\": string, \"keywords\": [string]}}. The summary is one paragraph of at \
         most {} characters that answers: what would someone be looking for when this file \
         is the right one? Say what the file is and what it is about, name the people, \
         projects, places, and dates it concerns, and use the words a searcher would use, \
         including likely synonyms. The keywords are at most {} terms or short phrases: the \
         most distinctive words in the file that someone might search for, names and \
         technical terms included, generic words excluded.",
        shape.target_chars, shape.keywords_max
    );
    let reply = llm
        .complete(&system, &format!("File: {name}\n\n{input}"))
        .await?;
    let (summary, keywords) = parse_reply(&reply);
    let keywords = if keywords.is_empty() {
        extract::keywords(text, shape.keywords_max)
    } else {
        keywords
    };
    Ok(Summary {
        text: truncate_chars(&collapse_ws(&summary), shape.target_chars),
        kind: SummaryKind::Llm,
        keywords: keywords.into_iter().take(shape.keywords_max).collect(),
    })
}

#[derive(serde::Deserialize)]
struct Reply {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    keywords: Vec<String>,
}

/// Read the model's JSON, tolerating fences and prose around it. A reply
/// that is not JSON is taken whole as the summary, with no keywords.
pub fn parse_reply(raw: &str) -> (String, Vec<String>) {
    let object = match (raw.find('{'), raw.rfind('}')) {
        (Some(open), Some(close)) if open < close => &raw[open..=close],
        _ => return (raw.trim().to_string(), Vec::new()),
    };
    match serde_json::from_str::<Reply>(object) {
        Ok(reply) => {
            let keywords = reply
                .keywords
                .into_iter()
                .map(|k| collapse_ws(&k))
                .filter(|k| !k.is_empty())
                .collect();
            (reply.summary, keywords)
        }
        Err(_) => (raw.trim().to_string(), Vec::new()),
    }
}

/// The offline summary a small device's profile would produce: the text's
/// telling sentences, one per section first, within the target.
pub fn extractive(text: &str, shape: SummaryShape) -> Summary {
    let selected = title_led_selection(text, shape.target_chars);
    Summary {
        text: truncate_chars(&collapse_ws(&selected), shape.target_chars),
        kind: SummaryKind::Extractive,
        keywords: extract::keywords(text, shape.keywords_max),
    }
}

/// Summary for sources whose content the index never read: derived from the
/// envelope alone.
pub fn envelope_summary(envelope: &Envelope) -> String {
    let name = envelope.hint.as_deref().unwrap_or("unnamed source");
    let modified = envelope
        .modified
        .map(|t| format!(", modified {}", ymd(t)))
        .unwrap_or_default();
    format!(
        "{name} — {} {} of {}{modified}. Content not indexed on this node; fetch to inspect.",
        envelope.length, envelope.source_type, envelope.content_type
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Timestamp};
    use inseam_kernel::fragment::Mimetype;

    const SHAPE: SummaryShape = SummaryShape {
        target_chars: 60,
        llm_input_chars: 8_000,
        keywords_max: 5,
    };

    const LONG: &str = "# Kitchen Reno\n\n---\n\nBudget lives in [the sheet](https://x.com/s). \
        Demo starts in June. The kitchen reno needs a permit. The kitchen reno permit is filed \
        with the city. Counters arrive after the cabinets.\n";

    #[test]
    fn extractive_strips_markdown_and_caps_length() {
        let s = extractive(LONG, SHAPE);
        assert_eq!(s.kind, SummaryKind::Extractive);
        assert!(!s.text.contains("https://"), "{}", s.text);
        assert!(!s.text.contains('#'), "{}", s.text);
        assert!(s.text.chars().count() <= 60);
        assert!(
            s.keywords.iter().any(|k| k == "kitchen reno"),
            "{:?}",
            s.keywords
        );
    }

    #[test]
    fn extractive_leads_with_a_heading_title() {
        let s = extractive(LONG, SHAPE);
        assert!(s.text.starts_with("Kitchen Reno. "), "{}", s.text);
        assert!(s.text.chars().count() <= 60);
    }

    #[test]
    fn title_and_body_split_only_on_a_leading_heading() {
        let (title, body) = title_and_body("# A [title](https://x)\n\nBody here.");
        assert_eq!(title.as_deref(), Some("A title"));
        assert_eq!(body.trim(), "Body here.");
        let (none, same) = title_and_body("Plain first line\nBody on the next line.");
        assert!(none.is_none());
        assert_eq!(same, "Plain first line\nBody on the next line.");
    }

    #[test]
    fn title_and_body_take_a_short_line_standing_alone_as_the_title() {
        let (title, body) = title_and_body("Subject: the reno permit\n\nBody here.");
        assert_eq!(title.as_deref(), Some("Subject: the reno permit"));
        assert_eq!(body.trim(), "Body here.");
        let paragraph = format!("{}\n\nBody.", "word ".repeat(40));
        let (none, _) = title_and_body(&paragraph);
        assert!(none.is_none(), "a paragraph-long first line is not a title");
    }

    #[tokio::test]
    async fn text_within_the_target_is_its_own_summary() {
        let s = summarize_text(None, Some("a.md"), "Plain words here.", SHAPE).await;
        assert_eq!(s.kind, SummaryKind::Verbatim);
        assert_eq!(s.text, "Plain words here.");
        assert!(s.keywords.is_empty(), "a verbatim summary left nothing out");
    }

    #[tokio::test]
    async fn summarize_without_llm_is_extractive() {
        let s = summarize_text(None, Some("a.md"), LONG, SHAPE).await;
        assert_eq!(s.kind, SummaryKind::Extractive);
    }

    #[test]
    fn parse_reply_reads_fenced_json() {
        let (summary, keywords) = parse_reply(
            "```json\n{\"summary\": \"A note.\", \"keywords\": [\"reno\", \" permit \"]}\n```",
        );
        assert_eq!(summary, "A note.");
        assert_eq!(keywords, vec!["reno", "permit"]);
    }

    #[test]
    fn parse_reply_takes_prose_whole() {
        let (summary, keywords) = parse_reply("  Just a sentence. ");
        assert_eq!(summary, "Just a sentence.");
        assert!(keywords.is_empty());
    }

    #[test]
    fn envelope_summary_names_what_it_knows() {
        let e = Envelope {
            source_type: "file".into(),
            content_type: Mimetype::parse("image/jpeg").expect("valid"),
            length: ContentLength::Bytes(52_000),
            created: None,
            modified: Some(Timestamp(1_420_070_400)),
            observed: Timestamp(1_700_000_000),
            properties: Vec::new(),
            hint: Some("IMG_2019.jpeg".into()),
            content_digest: None,
        };
        let s = envelope_summary(&e);
        assert!(s.contains("IMG_2019.jpeg"));
        assert!(s.contains("52000 bytes"));
        assert!(s.contains("2015-01-01"));
    }
}
