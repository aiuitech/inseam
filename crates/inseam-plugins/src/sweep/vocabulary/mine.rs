//! Mining and matching (`design/vocabulary.md`, the vocabulary pass): the
//! corpus's own words are the tokens rare in general language that recur
//! in a few of *these* documents, and they carry the marks of internal
//! naming — capitals inside a word, digits, hyphens, dots, underscores.
//! Everything here is a pure function of text: the tokenizer, the shape
//! rule, the bounded candidate counter, and the n-gram matcher the pass
//! runs over every source-content fragment.
//!
//! Counting is by the **normalized** spelling, so the frequency the band
//! reads is the frequency matching will find: `FOR` in a heading and
//! `for` in a sentence are one candidate, and a function word is never a
//! candidate however it is capitalized. A corpus of half a million
//! documents has some ten million distinct tokens, most seen once; the
//! counter keeps a bit per token seen and a count only for tokens seen
//! twice, so memory follows the recurring tokens, not the singletons.
//! Both are keyed by a 64-bit hash of the spelling: a collision merges two
//! tokens' counts, which at ten million tokens happens with a chance under
//! one in ten thousand, and the matching walk then measures every kept
//! spelling's exact frequency anyway.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher, Hasher};
use std::sync::OnceLock;

/// General English a plain word is refused for, one word per line. A
/// plain lowercase word with no mark of internal naming is a candidate
/// only when it is not everyday language: the prose full-text index
/// already finds `expensive` and `reflect` in every document that says
/// them, and a row for such a word adds seeds without adding a name. The
/// list is a heuristic, not a frequency table (`design/vocabulary.md`,
/// open questions); marked tokens never consult it.
const COMMON_WORDS: &str = include_str!("common_words.txt");

/// Whether a lowercase plain word is everyday English.
pub fn is_common_word(word: &str) -> bool {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        COMMON_WORDS
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect()
    })
    .contains(word)
}

/// Longest token kept; anything longer is a blob, not a name.
pub const TOKEN_CHARS_MAX: usize = 64;
/// Shortest plain word that may be a candidate without a shape mark.
pub const PLAIN_WORD_CHARS_MIN: usize = 4;
/// Longest phrase a derived candidate may be, in tokens.
pub const PHRASE_TOKENS_MAX: usize = 4;
/// Distinct tokens the seen-filter is sized for: ten million documents'
/// worth of singletons. Past it the filter's false-positive rate climbs,
/// which only admits more singletons to the counter, never loses a term.
pub const SEEN_TOKENS_CAPACITY: u64 = 32_000_000;
/// Bits per token in the seen-filter and hashes per token: one percent
/// false positives at capacity.
const SEEN_BITS_PER_TOKEN: u64 = 10;
const SEEN_HASHES: u64 = 7;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shape {
    /// A function word, a number, a short plain word: no candidate.
    None,
    /// Inner capitals, hyphens, dots, or a long plain word: a term.
    Term,
    /// Digits, or the code marks (`_`, `/`, a ticket-like `ABC-123`): an
    /// exact identifier.
    Identifier,
}

/// Classify a token. The marks are read from the spelling; the function
/// word test from the normalized form, so `THE` is no more a candidate
/// than `the`. Plain words qualify only when long enough and not
/// everyday English ([`is_common_word`]) — the document-frequency band
/// then thins what is left.
pub fn shape_of(spelling: &str) -> Shape {
    let has_letter = spelling.chars().any(char::is_alphabetic);
    if !has_letter {
        return Shape::None;
    }
    let lower = spelling.to_lowercase();
    if inseam_seams::extract::is_stopword(&lower) {
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
    if lower.chars().count() >= PLAIN_WORD_CHARS_MIN && !is_common_word(&lower) {
        return Shape::Term;
    }
    Shape::None
}

/// One candidate's tally across the texts walked so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally {
    pub shape: Shape,
    /// The spelling first seen with a shape mark: what the row will show.
    pub spelling: String,
    pub documents: u32,
    /// Whether this candidate was proposed by derived text (a keyword
    /// phrase) rather than mined from a token.
    pub derived: bool,
}

/// A recurring token's count and the best shape any of its spellings had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counted {
    documents: u32,
    shape: Shape,
}

/// The bounded candidate table. Every token with a letter is counted by
/// the hash of its normalized spelling — a bit in the seen-filter on
/// first sight, a count from the second — and the spelling itself is kept
/// only once a token has recurred with a shape mark, up to `spellings_max`
/// of them. Derived phrases enter the spelling table directly and are
/// counted by [`Self::count_phrases`].
pub struct Candidates {
    seen: SeenFilter,
    counts: HashMap<u64, Counted>,
    spellings: HashMap<u64, Tally>,
    phrases: HashMap<String, Tally>,
    counts_max: usize,
    spellings_max: usize,
    df_min: u32,
    /// The source whose tokens are being counted; a token counts once per
    /// source.
    seen_in_source: HashSet<u64>,
    hasher: BuildHasherDefault<DefaultHasher>,
    /// Tokens or spellings a full table turned away.
    pub dropped_at_cap: u64,
}

impl Candidates {
    /// `counts_max` bounds the recurring-token table, `spellings_max` the
    /// candidates that keep a spelling, `df_min` is the recurrence a
    /// spelling is kept from.
    pub fn new(counts_max: usize, spellings_max: usize, df_min: u32) -> Self {
        assert!(df_min >= 1);
        Self {
            seen: SeenFilter::new(SEEN_TOKENS_CAPACITY),
            counts: HashMap::new(),
            spellings: HashMap::new(),
            phrases: HashMap::new(),
            counts_max,
            spellings_max,
            df_min,
            seen_in_source: HashSet::new(),
            hasher: BuildHasherDefault::default(),
            dropped_at_cap: 0,
        }
    }

    fn hash(&self, normalized: &str) -> u64 {
        let mut hasher = self.hasher.build_hasher();
        hasher.write(normalized.as_bytes());
        hasher.finish()
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
        let normalized = join_normalized(&tokens);
        if self.phrases.len() + self.spellings.len() >= self.spellings_max {
            self.dropped_at_cap += 1;
            return;
        }
        self.phrases.entry(normalized).or_insert(Tally {
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
            if !token.spelling.chars().any(char::is_alphabetic) {
                continue;
            }
            let hash = self.hash(&token.normalized);
            if !self.seen_in_source.insert(hash) {
                continue;
            }
            let shape = shape_of(&token.spelling);
            self.count_one(hash, shape, token);
        }
    }

    /// First sight sets a bit; the second starts a count; a count that
    /// reaches `df_min` with a shape mark keeps the spelling.
    fn count_one(&mut self, hash: u64, shape: Shape, token: &Token) {
        if let Some(counted) = self.counts.get_mut(&hash) {
            counted.documents = counted.documents.saturating_add(1);
            counted.shape = counted.shape.max(shape);
            let counted = *counted;
            self.keep_spelling(hash, counted, token);
            return;
        }
        let first_sight = self.seen.insert(hash);
        if first_sight {
            return;
        }
        if self.counts.len() >= self.counts_max {
            self.dropped_at_cap += 1;
            return;
        }
        let counted = Counted {
            documents: 2,
            shape,
        };
        self.counts.insert(hash, counted);
        self.keep_spelling(hash, counted, token);
    }

    fn keep_spelling(&mut self, hash: u64, counted: Counted, token: &Token) {
        if counted.shape == Shape::None || counted.documents < self.df_min {
            return;
        }
        if let Some(tally) = self.spellings.get_mut(&hash) {
            tally.documents = counted.documents;
            tally.shape = counted.shape;
            if shape_of(&tally.spelling) < shape_of(&token.spelling) {
                tally.spelling = token.spelling.clone();
            }
            return;
        }
        if self.spellings.len() + self.phrases.len() >= self.spellings_max {
            self.dropped_at_cap += 1;
            return;
        }
        self.spellings.insert(
            hash,
            Tally {
                shape: counted.shape,
                spelling: token.spelling.clone(),
                documents: counted.documents,
                derived: false,
            },
        );
    }

    /// Count a source's phrase matches: each derived phrase whose tokens
    /// appear in order counts once for the source.
    pub fn count_phrases(&mut self, tokens: &[Token]) {
        if self.phrases.is_empty() {
            return;
        }
        let normalized: Vec<&str> = tokens.iter().map(|t| t.normalized.as_str()).collect();
        let mut matched: HashSet<String> = HashSet::new();
        for n in 2..=PHRASE_TOKENS_MAX {
            if normalized.len() < n {
                break;
            }
            for window in normalized.windows(n) {
                let gram = window.join(" ");
                if self.phrases.contains_key(&gram) {
                    matched.insert(gram);
                }
            }
        }
        for gram in matched {
            if let Some(tally) = self.phrases.get_mut(&gram) {
                tally.documents = tally.documents.saturating_add(1);
            }
        }
    }

    /// Distinct recurring tokens counted, phrases included.
    pub fn len(&self) -> usize {
        self.counts.len() + self.phrases.len()
    }

    /// The candidates whose document frequency lies in `[min, max]`, most
    /// frequent first: `(normalized, tally)`. A single token's normalized
    /// spelling is its lowercase spelling.
    pub fn in_band(&self, min: u32, max: u32) -> Vec<(String, &Tally)> {
        assert!(min <= max);
        let tokens = self
            .spellings
            .values()
            .map(|tally| (tally.spelling.to_lowercase(), tally));
        let phrases = self
            .phrases
            .iter()
            .map(|(normalized, tally)| (normalized.clone(), tally));
        let mut kept: Vec<(String, &Tally)> = tokens
            .chain(phrases)
            .filter(|(_, tally)| tally.documents >= min && tally.documents <= max)
            .collect();
        kept.sort_by(|a, b| b.1.documents.cmp(&a.1.documents).then(a.0.cmp(&b.0)));
        kept
    }
}

/// A bit per token seen: a Bloom filter over token hashes, so a token's
/// first sighting costs one bit and only its second a table entry. The
/// hashes are the Kirsch–Mitzenmacher combination of the token's hash
/// and a second one derived from it.
pub struct SeenFilter {
    bits: Vec<u64>,
    bit_count: u64,
}

impl SeenFilter {
    pub fn new(capacity: u64) -> Self {
        assert!(capacity >= 1);
        let bit_count = capacity.saturating_mul(SEEN_BITS_PER_TOKEN).max(64);
        let words = usize::try_from(bit_count.div_ceil(64)).expect("filter fits memory");
        Self {
            bits: vec![0; words],
            bit_count,
        }
    }

    /// Mark a token seen; `true` when it was not seen before (as far as
    /// the filter can tell).
    pub fn insert(&mut self, hash: u64) -> bool {
        let second = hash
            .rotate_left(32)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(1);
        let mut fresh = false;
        for i in 0..SEEN_HASHES {
            let bit = hash.wrapping_add(i.wrapping_mul(second)) % self.bit_count;
            let word = usize::try_from(bit / 64).expect("bit index fits usize");
            let mask = 1_u64 << (bit % 64);
            if self.bits[word] & mask == 0 {
                fresh = true;
                self.bits[word] |= mask;
            }
        }
        fresh
    }
}

pub fn join_normalized(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|t| t.normalized.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The matcher: normalized spellings (single tokens and phrases up to
/// [`PHRASE_TOKENS_MAX`] tokens) → the row to count. Matching is by
/// n-gram lookup over a fragment's tokens, exact on word boundaries, so
/// `H200` never matches inside `H2000`. Single tokens are probed without
/// allocating; a phrase is joined only where its first token starts one.
pub struct Matcher<Id: Copy> {
    rows: HashMap<String, Id>,
    phrase_starts: HashSet<String>,
    longest: usize,
}

impl<Id: Copy> Matcher<Id> {
    pub fn new(rows: HashMap<String, Id>) -> Self {
        let mut phrase_starts = HashSet::new();
        let mut longest = 1;
        for spelling in rows.keys() {
            let mut words = spelling.split(' ');
            let first = words.next().unwrap_or("");
            let count = 1 + words.count();
            if count > 1 {
                phrase_starts.insert(first.to_string());
                longest = longest.max(count);
            }
        }
        Self {
            rows,
            phrase_starts,
            longest: longest.min(PHRASE_TOKENS_MAX),
        }
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
        let mut found: Vec<Id> = Vec::new();
        let mut seen: HashSet<Id> = HashSet::new();
        for (position, token) in tokens.iter().enumerate() {
            if let Some(id) = self.rows.get(token.normalized.as_str())
                && seen.insert(*id)
            {
                found.push(*id);
            }
            if !self.phrase_starts.contains(token.normalized.as_str()) {
                continue;
            }
            for n in 2..=self.longest {
                let Some(window) = tokens.get(position..position + n) else {
                    break;
                };
                let gram = join_normalized(window);
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
        assert_eq!(shape_of("expensive"), Shape::None, "everyday English");
        assert_eq!(shape_of("Reflect"), Shape::None);
        assert_eq!(
            shape_of("reranking"),
            Shape::Term,
            "jargon is not on the list"
        );
        assert_eq!(
            shape_of("H100"),
            Shape::Identifier,
            "marks never consult the list"
        );
        assert_eq!(shape_of("2026"), Shape::None);
        assert_eq!(shape_of("ok"), Shape::None);
    }

    #[test]
    fn a_function_word_in_capitals_is_still_a_function_word() {
        assert_eq!(shape_of("THE"), Shape::None);
        assert_eq!(shape_of("FOR"), Shape::None);
        assert_eq!(shape_of("With"), Shape::None);
        assert_eq!(shape_of("AI"), Shape::Term, "a short acronym still counts");
    }

    #[test]
    fn candidates_count_once_per_source_and_band_by_frequency() {
        let mut candidates = Candidates::new(100, 100, 2);
        for _ in 0..3 {
            candidates.begin_source();
            candidates.count(&tokenize("Redwood Redwood rollout eu-central-1"));
        }
        candidates.begin_source();
        candidates.count(&tokenize("Redwood only"));
        let band = candidates.in_band(2, 3);
        let names: Vec<&str> = band.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["eu-central-1", "rollout"]);
        assert_eq!(
            candidates.in_band(4, 4)[0],
            (
                "redwood".to_string(),
                &Tally {
                    shape: Shape::Term,
                    spelling: "Redwood".into(),
                    documents: 4,
                    derived: false
                }
            )
        );
        assert_eq!(
            candidates.len(),
            3,
            "every recurring token is counted; a singleton is a bit"
        );
    }

    /// A token's first sighting is a bit in the seen-filter and nothing
    /// more, so its shape is read from the second sighting on; the marks
    /// that matter (digits, inner capitals, hyphens) are in every spelling
    /// of a name, and a function word is out under any capitalization.
    #[test]
    fn capitalization_is_one_candidate_counted_everywhere_it_appears() {
        let mut candidates = Candidates::new(100, 100, 2);
        candidates.begin_source();
        candidates.count(&tokenize("FOR THE nov gearbox"));
        candidates.begin_source();
        candidates.count(&tokenize("NOV once more for the gearbox"));
        candidates.begin_source();
        candidates.count(&tokenize("NOV a third time"));
        let band = candidates.in_band(1, 10);
        let names: Vec<&str> = band.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["nov", "gearbox"], "{band:?}");
        let nov = band.iter().find(|(n, _)| n == "nov").expect("kept");
        assert_eq!(nov.1.documents, 3, "every casing counts");
        assert_eq!(nov.1.spelling, "NOV", "the marked spelling is shown");
    }

    #[test]
    fn derived_phrases_count_through_matching() {
        let mut candidates = Candidates::new(100, 100, 2);
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
    fn the_caps_turn_tokens_away_and_count_them() {
        let mut candidates = Candidates::new(2, 100, 2);
        candidates.begin_source();
        candidates.count(&tokenize("alpha bravo charlie delta"));
        candidates.begin_source();
        candidates.count(&tokenize("alpha bravo charlie delta"));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates.dropped_at_cap, 2);
        let mut spellings = Candidates::new(100, 1, 2);
        for _ in 0..2 {
            spellings.begin_source();
            spellings.count(&tokenize("SUP-1 SUP-2"));
        }
        assert_eq!(spellings.in_band(1, 10).len(), 1);
        assert_eq!(spellings.dropped_at_cap, 1);
    }

    #[test]
    fn the_seen_filter_reports_first_sight_once() {
        let mut filter = SeenFilter::new(1_000);
        assert!(filter.insert(7));
        assert!(!filter.insert(7));
        assert!(filter.insert(8));
        let mut fresh = 0;
        for hash in 100..1_100_u64 {
            if filter.insert(hash.wrapping_mul(0x9E37_79B9_7F4A_7C15)) {
                fresh += 1;
            }
        }
        assert!(fresh >= 950, "under a few percent false positives: {fresh}");
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
        assert_eq!(found, vec![1, 2, 3]);
        assert!(matcher.matches(&tokenize("nothing")).is_empty());
        assert!(
            matcher
                .matches(&tokenize("residency without the stamp"))
                .is_empty()
        );
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
