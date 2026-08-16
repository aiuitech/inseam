//! The summarizer: the one mandatory transform. The Finder serves summaries
//! in every response so an AI client can decide whether to keep digging, so
//! every indexed source must have one (`design/indexing.md`). Profiles
//! configure the length, not the existence: LLM when available and budgeted,
//! extractive for text otherwise, envelope-derived for everything else.

use inseam_kernel::address::Envelope;
use inseam_kernel::text::{collapse_ws, truncate_chars};
use inseam_seams::transforms::GrantedLlm;

/// Characters of source text an LLM summary call sees.
const LLM_INPUT_CHARS: usize = 8_000;

/// How a summary came to be. Recorded on the summary fragment's mimetype as
/// the `via` parameter, so provenance travels with the fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    Llm,
    Extractive,
    Envelope,
}

impl SummaryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Extractive => "extractive",
            Self::Envelope => "envelope",
        }
    }
}

/// Summarize source text: LLM if the capability was granted, extractive
/// fallback on any failure. Never errors — the mandatory-summary invariant
/// wins.
pub async fn summarize_text(
    llm: Option<&dyn GrantedLlm>,
    hint: Option<&str>,
    text: &str,
    target_chars: usize,
) -> (String, SummaryKind) {
    if let Some(llm) = llm {
        match llm_summary(llm, hint, text, target_chars).await {
            Ok(summary) if !summary.is_empty() => return (summary, SummaryKind::Llm),
            Ok(_) => tracing::debug!("llm returned an empty summary; falling back"),
            Err(e) => tracing::warn!("llm summary failed, falling back to extractive: {e}"),
        }
    }
    (extractive(text, target_chars), SummaryKind::Extractive)
}

async fn llm_summary(
    llm: &dyn GrantedLlm,
    hint: Option<&str>,
    text: &str,
    target_chars: usize,
) -> Result<String, inseam_seams::SeamError> {
    let bounded: String = text.chars().take(LLM_INPUT_CHARS).collect();
    let name = hint.unwrap_or("(unnamed source)");
    let system = format!(
        "You summarize personal files for a search index. Reply with only the \
         summary text: at most {target_chars} characters, one paragraph, no preamble, \
         no markdown. Capture what the document is, the key people, projects, places, \
         dates and topics in it."
    );
    let reply = llm
        .complete(&system, &format!("File: {name}\n\n{bounded}"))
        .await?;
    Ok(truncate_chars(&collapse_ws(&reply), target_chars))
}

/// First words of the content with markdown furniture stripped — the offline
/// summary a small device's profile would produce.
pub fn extractive(text: &str, target_chars: usize) -> String {
    let mut cleaned = String::with_capacity(text.len().min(target_chars * 4));
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.chars().all(|c| matches!(c, '-' | '=' | '#' | '*' | '`' | '~'))
        {
            continue;
        }
        let line = line
            .trim_start_matches(['#', '>', '*', '-', ' '])
            .trim_end_matches(['#', ' ']);
        cleaned.push_str(&strip_links(line));
        cleaned.push(' ');
        if cleaned.chars().count() > target_chars * 2 {
            break;
        }
    }
    truncate_chars(&collapse_ws(&cleaned), target_chars)
}

/// Summary for sources whose content the index never read: derived from the
/// envelope alone.
pub fn envelope_summary(envelope: &Envelope) -> String {
    let name = envelope.hint.as_deref().unwrap_or("unnamed source");
    let modified = envelope
        .modified
        .map(|t| format!(", modified {}", t.ymd()))
        .unwrap_or_default();
    format!(
        "{name} — {} {} of {}{modified}. Content not indexed on this node; fetch to inspect.",
        envelope.length,
        envelope.source_type,
        envelope.content_type
    )
}

/// Replace `[text](url)` with `text`. Hand-rolled to keep regex out of core.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close_rel) = rest[open..].find(']') else { break };
        let close = open + close_rel;
        let after = &rest[close + 1..];
        if let Some(paren_end) = after.strip_prefix('(').and_then(|a| a.find(')')) {
            out.push_str(&rest[..open]);
            out.push_str(&rest[open + 1..close]);
            rest = &after[paren_end + 2..];
        } else {
            out.push_str(&rest[..close + 1]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Timestamp};
    use inseam_kernel::fragment::Mimetype;

    #[test]
    fn extractive_strips_markdown_and_caps_length() {
        let text = "# Kitchen Reno\n\n---\n\nBudget lives in [the sheet](https://x.com/s). \
                    Demo starts in June.\n";
        let s = extractive(text, 60);
        assert!(s.starts_with("Kitchen Reno"));
        assert!(s.contains("the sheet"));
        assert!(!s.contains("https://"));
        assert!(s.chars().count() <= 60);
    }

    #[test]
    fn extractive_of_empty_text_is_empty() {
        assert_eq!(extractive("", 100), "");
        assert_eq!(extractive("---\n\n===\n", 100), "");
    }

    #[tokio::test]
    async fn summarize_without_llm_is_extractive() {
        let (s, kind) = summarize_text(None, Some("a.md"), "Plain words here.", 100).await;
        assert_eq!(kind, SummaryKind::Extractive);
        assert_eq!(s, "Plain words here.");
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
        };
        let s = envelope_summary(&e);
        assert!(s.contains("IMG_2019.jpeg"));
        assert!(s.contains("52000 bytes"));
        assert!(s.contains("2015-01-01"));
    }

    #[test]
    fn strip_links_leaves_plain_brackets_alone() {
        assert_eq!(strip_links("a [note] here"), "a [note] here");
        assert_eq!(strip_links("[x](https://y) and [z](https://w)"), "x and z");
    }
}
