//! Mining and matching (`design/vocabulary.md`, the vocabulary pass): the
//! corpus's own words are the tokens rare in general language that recur
//! in a few of *these* documents, and they carry the marks of internal
//! naming — capitals inside a word, digits, hyphens, dots, underscores.
//! Everything here is a pure function of text: the tokenizer, the shape
//! rule, the candidate counter, and the n-gram matcher the pass runs over
//! every source-content fragment.

use std::collections::{HashMap, HashSet};

use inseam_kernel::store::normalize_spelling;

/// Longest token kept; anything longer is a blob, not a name.
pub const TOKEN_CHARS_MAX: usize = 64;
/// Shortest plain word that may be a candidate without a shape mark.
pub const PLAIN_WORD_CHARS_MIN: usize = 4;
/// Longest phrase a derived candidate may be, in tokens.
pub const PHRASE_TOKENS_MAX: usize = 4;

/// A token as the matcher sees it: normalized (lowercase) with the
/// original spelling kept for the row's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub normalized: String,
    pub spelling: String,
}

/// Split text into tokens: runs of alphanumerics with inner `-`, `_`, `.`,
/// `/`, `'` kept (so `eu-central-1`, `stream.timebox_finalized`, and
/// `v2.3.1` survive), edge punctuation trimmed, at most
/// [`TOKEN_CHARS_MAX`] long. Tokens without a letter (bare numbers) are
/// kept as breaks for phrases but never as candidates.
pub fn tokenize(text: &str) -> Vec<Token> {
    text.split(|c: char| c.is_whitespace() || is_separator(c))
        .filter_map(|raw| {
            let trimmed = raw.trim_matches(|c: char| !c.is_alphanumeric());
            if trimmed.is_empty() || trimmed.chars().count() > TOKEN_CHARS_MAX {
                return None;
            }
            Some(Token {
                normalized: trimmed.to_lowercase(),
                spelling: trimmed.to_string(),
            })
        })
        .collect()
}

/// Characters that end a token even inside a word.
fn is_separator(c: char) -> bool {
    matches!(
        c,
        ',' | ';'
            | ':'
            | '!'
            | '?'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | '<'
            | '>'
            | '|'
            | '\\'
            | '`'
            | '*'
            | '#'
            | '@'
            | '&'
            | '='
            | '+'
            | '~'
            | '^'
            | '%'
            | '$'
    )
}

/// The shape of a token: what kind of vocabulary row it could be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Digits, or the code marks (`_`, `/`, a ticket-like `ABC-123`): an
    /// exact identifier.
    Identifier,
    /// Inner capitals, hyphens, dots, or a long plain word: a term.
    Term,
    /// A function word, a number, a short plain word: no candidate.
    None,
}

/// Classify a token's spelling. Plain lowercase words qualify only when
/// long enough and not function words — the bulk of the candidate set,
/// which the document-frequency band then thins.
pub fn shape_of(spelling: &str) -> Shape {
    let has_letter = spelling.chars().any(char::is_alphabetic);
    if !has_letter {
        return Shape::None;
    }
    let has_digit = spelling.chars().any(|c| c.is_ascii_digit());
    let has_code_mark = spelling.contains('_') || spelling.contains('/');
    if has_digit || has_code_mark {
        return Shape::Identifier;
    }
    let mut chars = spelling.chars();
    let _first = chars.next();
    let inner_upper = chars.any(char::is_uppercase);
    let has_word_mark = spelling.contains('-') || spelling.contains('.');
    if inner_upper || has_word_mark {
        return Shape::Term;
    }
    let lower = spelling.to_lowercase();
    if lower.chars().count() >= PLAIN_WORD_CHARS_MIN && !inseam_seams::extract::is_stopword(&lower)
    {
        return Shape::Term;
    }
    Shape::None
}

/// One candidate's tally across the texts walked so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally {
    pub shape: Shape,
    /// The spelling first seen: what the row will show.
    pub spelling: String,
    pub documents: u32,
    /// Whether this candidate was proposed by derived text (a keyword
    /// phrase) rather than mined from a token.
    pub derived: bool,
}

/// The bounded candidate table: normalized spelling → tally. Shape-marked
/// tokens and derived phrases always enter until the hard cap; plain words
/// enter only while the table is under the soft cap, so a corpus of
/// millions of distinct words cannot exhaust memory.
pub struct Candidates {
    tallies: HashMap<String, Tally>,
    soft_cap: usize,
    hard_cap: usize,
    /// The source whose tokens are being counted; a token counts once per
    /// source.
    seen_in_source: HashSet<String>,
    pub dropped_at_cap: u64,
}

impl Candidates {
    pub fn new(soft_cap: usize) -> Self {
        Self {
            tallies: HashMap::new(),
            soft_cap,
            hard_cap: soft_cap.saturating_mul(2),
            seen_in_source: HashSet::new(),
            dropped_at_cap: 0,
        }
    }

    /// Register phrases proposed by derived text. They count documents
    /// only through matching, so they start at zero.
    pub fn propose_phrase(&mut self, phrase: &str) {
        let tokens = tokenize(phrase);
        let count = tokens.len();
        if !(2..=PHRASE_TOKENS_MAX).contains(&count) {
            return;
        }
        if tokens.iter().all(|t| shape_of(&t.spelling) == Shape::None) {
            return;
        }
        let normalized = normalize_spelling(&join_normalized(&tokens));
        if self.tallies.len() >= self.hard_cap {
            self.dropped_at_cap += 1;
            return;
        }
        self.tallies.entry(normalized).or_insert(Tally {
            shape: Shape::Term,
            spelling: tokens
                .iter()
                .map(|t| t.spelling.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            documents: 0,
            derived: true,
        });
    }

    /// Start counting a new source: every token counts once per source.
    pub fn begin_source(&mut self) {
        self.seen_in_source.clear();
    }

    /// Count one source's tokens.
    pub fn count(&mut self, tokens: &[Token]) {
        for token in tokens {
            let shape = shape_of(&token.spelling);
            if shape == Shape::None {
                continue;
            }
            if !self.seen_in_source.insert(token.normalized.clone()) {
                continue;
            }
            self.count_one(shape, token);
        }
    }

    fn count_one(&mut self, shape: Shape, token: &Token) {
        if let Some(tally) = self.tallies.get_mut(&token.normalized) {
            tally.documents = tally.documents.saturating_add(1);
            return;
        }
        let cap = match shape {
            Shape::Identifier => self.hard_cap,
            Shape::Term
                if token
                    .spelling
                    .chars()
                    .any(|c| c.is_uppercase() || c == '-' || c == '.') =>
            {
                self.hard_cap
            }
            Shape::Term | Shape::None => self.soft_cap,
        };
        if self.tallies.len() >= cap {
            self.dropped_at_cap += 1;
            return;
        }
        self.tallies.insert(
            token.normalized.clone(),
            Tally {
                shape,
                spelling: token.spelling.clone(),
                documents: 1,
                derived: false,
            },
        );
    }

    /// Count a source's phrase matches: each derived phrase whose tokens
    /// appear in order counts once for the source.
    pub fn count_phrases(&mut self, tokens: &[Token]) {
        let normalized: Vec<&str> = tokens.iter().map(|t| t.normalized.as_str()).collect();
        let mut matched: HashSet<String> = HashSet::new();
        for n in 2..=PHRASE_TOKENS_MAX {
            if normalized.len() < n {
                break;
            }
            for window in normalized.windows(n) {
                let gram = window.join(" ");
                let is_derived = self.tallies.get(&gram).is_some_and(|t| t.derived);
                if is_derived {
                    matched.insert(gram);
                }
            }
        }
        for gram in matched {
            if let Some(tally) = self.tallies.get_mut(&gram) {
                tally.documents = tally.documents.saturating_add(1);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.tallies.len()
    }

    /// The candidates whose document frequency lies in `[min, max]`.
    pub fn in_band(&self, min: u32, max: u32) -> Vec<(&str, &Tally)> {
        assert!(min <= max);
        let mut kept: Vec<(&str, &Tally)> = self
            .tallies
            .iter()
            .filter(|(_, tally)| tally.documents >= min && tally.documents <= max)
            .map(|(normalized, tally)| (normalized.as_str(), tally))
            .collect();
        kept.sort_by(|a, b| b.1.documents.cmp(&a.1.documents).then(a.0.cmp(b.0)));
        kept
    }
}

fn join_normalized(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|t| t.normalized.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The matcher: normalized spellings (single tokens and phrases up to
/// [`PHRASE_TOKENS_MAX`] tokens) → the row to anchor. Matching is by
/// n-gram lookup over a fragment's tokens: four probes per token, exact on
/// word boundaries, so `H200` never matches inside `H2000`.
pub struct Matcher<Id: Copy> {
    rows: HashMap<String, Id>,
    longest: usize,
}

impl<Id: Copy> Matcher<Id> {
    pub fn new(rows: HashMap<String, Id>) -> Self {
        let longest = rows
            .keys()
            .map(|spelling| spelling.split(' ').count())
            .max()
            .unwrap_or(1)
            .clamp(1, PHRASE_TOKENS_MAX);
        Self { rows, longest }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The distinct rows a text names.
    pub fn matches(&self, tokens: &[Token]) -> Vec<Id>
    where
        Id: std::hash::Hash + Eq,
    {
        if self.rows.is_empty() {
            return Vec::new();
        }
        let normalized: Vec<&str> = tokens.iter().map(|t| t.normalized.as_str()).collect();
        let mut found: Vec<Id> = Vec::new();
        let mut seen: HashSet<Id> = HashSet::new();
        for n in 1..=self.longest {
            if normalized.len() < n {
                break;
            }
            for window in normalized.windows(n) {
                let gram = if n == 1 {
                    window[0].to_string()
                } else {
                    window.join(" ")
                };
                if let Some(id) = self.rows.get(&gram)
                    && seen.insert(*id)
                {
                    found.push(*id);
                }
            }
        }
        found
    }
}

/// Split a keywords row (`a, b, c`) into its phrases.
pub fn keyword_phrases(text: &str) -> impl Iterator<Item = &str> {
    text.split([',', '\n', ';'])
        .map(str::trim)
        .filter(|phrase| !phrase.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spellings(tokens: &[Token]) -> Vec<&str> {
        tokens.iter().map(|t| t.spelling.as_str()).collect()
    }

    #[test]
    fn tokens_keep_identifiers_whole_and_trim_punctuation() {
        let tokens = tokenize(
            "Route eu-central-1 (H200) via stream.timebox_finalized; see SUP-100432, v2.3!",
        );
        assert_eq!(
            spellings(&tokens),
            vec![
                "Route",
                "eu-central-1",
                "H200",
                "via",
                "stream.timebox_finalized",
                "see",
                "SUP-100432",
                "v2.3"
            ]
        );
        assert!(tokenize("   ").is_empty());
        assert!(tokenize(&"x".repeat(65)).is_empty());
    }

    #[test]
    fn shapes_follow_the_marks_of_internal_naming() {
        assert_eq!(shape_of("SUP-100432"), Shape::Identifier);
        assert_eq!(shape_of("H200"), Shape::Identifier);
        assert_eq!(shape_of("stream.timebox_finalized"), Shape::Identifier);
        assert_eq!(shape_of("eu-central"), Shape::Term);
        assert_eq!(shape_of("KVCache"), Shape::Term);
        assert_eq!(shape_of("Redwood"), Shape::Term);
        assert_eq!(shape_of("the"), Shape::None);
        assert_eq!(shape_of("about"), Shape::None);
        assert_eq!(shape_of("2026"), Shape::None);
        assert_eq!(shape_of("ok"), Shape::None);
    }

    #[test]
    fn candidates_count_once_per_source_and_band_by_frequency() {
        let mut candidates = Candidates::new(100);
        for _ in 0..3 {
            candidates.begin_source();
            candidates.count(&tokenize("Redwood Redwood launch eu-central-1"));
        }
        candidates.begin_source();
        candidates.count(&tokenize("Redwood only"));
        let band = candidates.in_band(2, 3);
        let names: Vec<&str> = band.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["eu-central-1", "launch"]);
        assert_eq!(
            candidates.in_band(4, 4)[0],
            (
                "redwood",
                &Tally {
                    shape: Shape::Term,
                    spelling: "Redwood".into(),
                    documents: 4,
                    derived: false
                }
            )
        );
    }

    #[test]
    fn derived_phrases_count_through_matching() {
        let mut candidates = Candidates::new(100);
        candidates.propose_phrase("residency stamp");
        candidates.propose_phrase("the");
        candidates.propose_phrase("a b c d e");
        assert_eq!(candidates.len(), 1);
        candidates.begin_source();
        candidates.count_phrases(&tokenize("a stale residency stamp after heartbeat lag"));
        candidates.begin_source();
        candidates.count_phrases(&tokenize("nothing here"));
        assert_eq!(candidates.in_band(1, 1)[0].0, "residency stamp");
    }

    #[test]
    fn the_soft_cap_stops_plain_words_but_not_marked_tokens() {
        let mut candidates = Candidates::new(2);
        candidates.begin_source();
        candidates.count(&tokenize("alpha bravo charlie delta"));
        assert_eq!(candidates.len(), 2);
        candidates.count(&tokenize("SUP-1 SUP-2"));
        assert_eq!(candidates.len(), 4, "identifiers enter up to the hard cap");
        candidates.count(&tokenize("SUP-3"));
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates.dropped_at_cap, 3);
    }

    #[test]
    fn the_matcher_finds_tokens_and_phrases_on_word_boundaries() {
        let rows: HashMap<String, u32> = HashMap::from([
            ("h200".to_string(), 1),
            ("residency stamp".to_string(), 2),
            ("eu-central-1".to_string(), 3),
        ]);
        let matcher = Matcher::new(rows);
        let found = matcher.matches(&tokenize(
            "The H2000 and the H200 share a residency stamp in eu-central-1.",
        ));
        assert_eq!(found, vec![1, 3, 2]);
        assert!(matcher.matches(&tokenize("nothing")).is_empty());
        assert!(
            Matcher::<u32>::new(HashMap::new())
                .matches(&tokenize("H200"))
                .is_empty()
        );
    }

    #[test]
    fn keyword_rows_split_on_commas() {
        let phrases: Vec<&str> =
            keyword_phrases("residency stamp, EU tenant,, heartbeat lag\n").collect();
        assert_eq!(
            phrases,
            vec!["residency stamp", "EU tenant", "heartbeat lag"]
        );
    }
}
