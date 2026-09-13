//! Extractive selection shared by the transforms that must say what a text
//! is about without a model reading all of it: which sentences carry the
//! most information (an LLM's input window; the offline summary) and which
//! terms name it (the full-text side of the index). No model, no network, no
//! corpus: every weight comes from the text itself, so the selection is
//! deterministic and free (`design/indexing.md`).
//!
//! Terms are weighted by frequency against how many sentences they spread
//! over (a term said often but in few places names a subject; one in every
//! sentence is furniture). Sentences score by the terms they carry, and are
//! chosen by maximal marginal relevance — the best sentence that says
//! something the chosen ones do not — after one sentence per section, so a
//! long document's tail is represented and not only its head.

use std::collections::HashMap;

use crate::text::truncate_chars;

/// Sentences considered per text; past this the tail is ignored.
const SENTENCES_MAX: usize = 4_096;
/// Sections a text is divided into for coverage; later ones join the last.
const SECTIONS_MAX: u32 = 512;
/// Shortest token that counts as a term.
const TERM_CHARS_MIN: usize = 3;
/// Longest token that counts as a term; past this it is a hash or a URL.
const TERM_CHARS_MAX: usize = 40;
/// Maximal marginal relevance: the share of a sentence's score that is its
/// own relevance; the rest is its novelty against what is already chosen.
const RELEVANCE_WEIGHT: f64 = 0.7;
/// A phrase that recurs outranks its words: bigrams carry this factor.
const PHRASE_WEIGHT: f64 = 1.2;
/// A bigram must recur to count as a phrase at all.
const PHRASE_COUNT_MIN: u32 = 2;

/// A query with its function words removed, for a full-text search that
/// ORs its terms: on a large index a term like "the" matches nearly every
/// row and the ranker scores them all for nothing, since a row matched by
/// function words alone ranks last anyway. A query made only of function
/// words is returned whole — filtering it would leave nothing to search.
pub fn strip_stopwords(query: &str) -> String {
    let kept: Vec<&str> = query
        .split_whitespace()
        .filter(|word| {
            let bare: String = word
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            !STOPWORDS.contains(&bare.as_str())
        })
        .collect();
    if kept.is_empty() {
        query.to_string()
    } else {
        kept.join(" ")
    }
}

/// Whether a lowercase word is a function word (`the`, `about`): the one
/// list every stopword decision in the node shares.
pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

/// Function words that never name a subject. English only: a text in
/// another language loses nothing but the filtering.
const STOPWORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "across",
    "additionally",
    "after",
    "again",
    "against",
    "aim",
    "aims",
    "all",
    "along",
    "already",
    "also",
    "although",
    "always",
    "among",
    "amongst",
    "amount",
    "an",
    "analysis",
    "and",
    "another",
    "any",
    "anything",
    "are",
    "aren",
    "around",
    "as",
    "assessed",
    "associated",
    "at",
    "background",
    "based",
    "be",
    "because",
    "been",
    "before",
    "behind",
    "being",
    "below",
    "beside",
    "besides",
    "between",
    "both",
    "but",
    "by",
    "can",
    "can't",
    "cannot",
    "common",
    "compared",
    "conclusion",
    "conclusions",
    "conducted",
    "could",
    "couldn",
    "current",
    "data",
    "decreased",
    "determined",
    "did",
    "didn",
    "different",
    "do",
    "does",
    "doesn",
    "doing",
    "don",
    "done",
    "down",
    "during",
    "each",
    "either",
    "especially",
    "etc",
    "evaluated",
    "every",
    "everything",
    "examined",
    "few",
    "first",
    "for",
    "found",
    "from",
    "further",
    "furthermore",
    "general",
    "get",
    "given",
    "got",
    "greater",
    "had",
    "hadn",
    "has",
    "hasn",
    "have",
    "haven",
    "having",
    "he",
    "hence",
    "her",
    "here",
    "hers",
    "high",
    "higher",
    "him",
    "his",
    "how",
    "however",
    "i",
    "if",
    "important",
    "in",
    "included",
    "including",
    "increased",
    "indicated",
    "initial",
    "into",
    "investigated",
    "is",
    "isn",
    "it",
    "its",
    "itself",
    "just",
    "known",
    "large",
    "last",
    "least",
    "less",
    "let",
    "level",
    "levels",
    "like",
    "likely",
    "long",
    "low",
    "lower",
    "made",
    "major",
    "make",
    "many",
    "may",
    "me",
    "measured",
    "method",
    "methods",
    "might",
    "minor",
    "more",
    "moreover",
    "most",
    "much",
    "must",
    "my",
    "neither",
    "never",
    "new",
    "next",
    "no",
    "not",
    "nothing",
    "now",
    "number",
    "numbers",
    "numerous",
    "objective",
    "objectives",
    "observed",
    "obtained",
    "of",
    "off",
    "often",
    "on",
    "once",
    "one",
    "only",
    "onto",
    "or",
    "other",
    "our",
    "ours",
    "out",
    "over",
    "overall",
    "own",
    "part",
    "particular",
    "particularly",
    "per",
    "performed",
    "possible",
    "potential",
    "present",
    "presented",
    "previous",
    "provided",
    "purpose",
    "recent",
    "recently",
    "related",
    "report",
    "reported",
    "reports",
    "respectively",
    "result",
    "results",
    "same",
    "second",
    "seem",
    "seemed",
    "seems",
    "several",
    "shall",
    "she",
    "short",
    "should",
    "shouldn",
    "show",
    "showed",
    "shown",
    "shows",
    "significantly",
    "since",
    "small",
    "so",
    "some",
    "something",
    "sometimes",
    "still",
    "studies",
    "study",
    "such",
    "suggest",
    "suggested",
    "suggests",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "then",
    "there",
    "therefore",
    "these",
    "they",
    "thing",
    "things",
    "this",
    "those",
    "though",
    "through",
    "thus",
    "to",
    "too",
    "total",
    "toward",
    "towards",
    "two",
    "under",
    "until",
    "unto",
    "up",
    "upon",
    "us",
    "use",
    "used",
    "using",
    "various",
    "very",
    "via",
    "was",
    "wasn",
    "way",
    "we",
    "well",
    "were",
    "weren",
    "what",
    "when",
    "where",
    "whereas",
    "whether",
    "which",
    "while",
    "who",
    "whole",
    "whom",
    "why",
    "will",
    "with",
    "within",
    "without",
    "won",
    "would",
    "wouldn",
    "yet",
    "you",
    "your",
    "yours",
    "yourself",
];

/// Choose the sentences of `text` that best represent it within
/// `budget_chars`, one per section first, in document order. A text that
/// already fits is returned whole; a text none of whose sentences fits
/// yields its best sentence cut to the budget, so a text with any prose
/// never selects to nothing.
pub fn select(text: &str, budget_chars: usize) -> String {
    if text.chars().count() <= budget_chars {
        return text.trim().to_string();
    }
    let doc = analyze(text);
    if doc.sentences.is_empty() {
        return String::new();
    }
    let scores = doc.sentence_scores();
    let mut chosen = vec![false; doc.sentences.len()];
    let mut remaining = budget_chars;
    cover_sections(&doc, &scores, &mut chosen, &mut remaining);
    fill_by_marginal_relevance(&doc, &scores, &mut chosen, &mut remaining);
    assert!(remaining <= budget_chars);
    if chosen.iter().any(|c| *c) {
        return render_chosen(&doc, &chosen);
    }
    let best = scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(index, _)| index);
    truncate_chars(&doc.sentences[best].text, budget_chars)
}

/// The terms that name `text`, best first, at most `max` of them: recurring
/// phrases outrank their words, and a chosen phrase absorbs them.
pub fn keywords(text: &str, max: usize) -> Vec<String> {
    let doc = analyze(text);
    let mut ranked: Vec<(u32, f64)> = doc
        .weights
        .iter()
        .enumerate()
        .filter_map(|(id, weight)| u32::try_from(id).ok().map(|id| (id, *weight)))
        .filter(|(id, weight)| *weight > 0.0 && doc.term_is_keyword_candidate(*id))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out: Vec<String> = Vec::with_capacity(max);
    for (id, _) in ranked {
        if out.len() >= max {
            break;
        }
        let term = &doc.terms[usize::try_from(id).unwrap_or(usize::MAX)];
        let absorbed = out.iter().any(|kept| term_absorbs(kept, term));
        if !absorbed {
            out.retain(|kept| !term_absorbs(term, kept));
            out.push(term.clone());
        }
    }
    assert!(out.len() <= max);
    out
}

/// Whether keeping `phrase` makes `term` redundant: a phrase absorbs its
/// own words, and a term absorbs itself.
fn term_absorbs(phrase: &str, term: &str) -> bool {
    if phrase == term {
        return true;
    }
    phrase.split(' ').any(|word| word == term)
}

/// One sentence of the text, cleaned of markdown furniture.
struct Sentence {
    text: String,
    section: u32,
    /// Term ids with their weights, sorted by id; the sentence's vector.
    vector: Vec<(u32, f64)>,
    norm: f64,
}

struct Document {
    sentences: Vec<Sentence>,
    /// Term id → its spelling.
    terms: Vec<String>,
    /// Term id → its weight in this text.
    weights: Vec<f64>,
}

impl Document {
    fn sentence_scores(&self) -> Vec<f64> {
        let raw: Vec<f64> = self
            .sentences
            .iter()
            .map(|s| {
                let sum: f64 = s.vector.iter().map(|(_, w)| w).sum();
                let len = f64::from(u32::try_from(s.vector.len()).unwrap_or(u32::MAX));
                sum / (len + 1.0).sqrt()
            })
            .collect();
        let max = raw.iter().copied().fold(0.0_f64, f64::max);
        if max <= 0.0 {
            return raw;
        }
        raw.iter().map(|s| s / max).collect()
    }

    fn term_is_keyword_candidate(&self, id: u32) -> bool {
        let term = &self.terms[usize::try_from(id).unwrap_or(usize::MAX)];
        term.chars().any(char::is_alphabetic)
    }
}

/// Cosine similarity of two sparse, id-sorted vectors.
fn similarity(a: &Sentence, b: &Sentence) -> f64 {
    if a.norm == 0.0 || b.norm == 0.0 {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut i = 0;
    let mut j = 0;
    let steps_max = a.vector.len() + b.vector.len();
    for _ in 0..steps_max {
        let (Some(x), Some(y)) = (a.vector.get(i), b.vector.get(j)) else {
            break;
        };
        match x.0.cmp(&y.0) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                dot += x.1 * y.1;
                i += 1;
                j += 1;
            }
        }
    }
    dot / (a.norm * b.norm)
}

/// First pass: the best sentence of every section, in section order, while
/// the budget holds — so a long text's tail is represented before the
/// scores decide the rest.
fn cover_sections(doc: &Document, scores: &[f64], chosen: &mut [bool], remaining: &mut usize) {
    let sections_count = doc
        .sentences
        .iter()
        .map(|s| s.section)
        .max()
        .map_or(0, |m| m + 1);
    assert!(sections_count <= SECTIONS_MAX);
    for section in 0..sections_count {
        let best = doc
            .sentences
            .iter()
            .enumerate()
            .filter(|(_, s)| s.section == section)
            .filter(|(_, s)| s.text.chars().count() <= *remaining)
            .max_by(|a, b| scores[a.0].total_cmp(&scores[b.0]));
        if let Some((index, sentence)) = best {
            chosen[index] = true;
            *remaining -= sentence.text.chars().count();
        }
    }
}

/// Second pass: maximal marginal relevance — each round takes the sentence
/// with the best relevance-minus-redundancy that still fits, until nothing
/// fits. `redundancy[i]` is sentence i's highest similarity to a chosen one.
fn fill_by_marginal_relevance(
    doc: &Document,
    scores: &[f64],
    chosen: &mut [bool],
    remaining: &mut usize,
) {
    let mut redundancy = vec![0.0_f64; doc.sentences.len()];
    for (i, sentence) in doc.sentences.iter().enumerate() {
        if chosen[i] {
            for (j, other) in doc.sentences.iter().enumerate() {
                redundancy[j] = redundancy[j].max(similarity(sentence, other));
            }
        }
    }
    for _ in 0..doc.sentences.len() {
        let best = doc
            .sentences
            .iter()
            .enumerate()
            .filter(|(i, s)| !chosen[*i] && s.text.chars().count() <= *remaining)
            .map(|(i, _)| {
                let marginal =
                    RELEVANCE_WEIGHT * scores[i] - (1.0 - RELEVANCE_WEIGHT) * redundancy[i];
                (i, marginal)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let Some((index, _)) = best else {
            break;
        };
        chosen[index] = true;
        *remaining -= doc.sentences[index].text.chars().count();
        let picked = &doc.sentences[index];
        for (j, other) in doc.sentences.iter().enumerate() {
            redundancy[j] = redundancy[j].max(similarity(picked, other));
        }
    }
}

/// The chosen sentences in document order: a space within a section, a
/// newline between sections.
fn render_chosen(doc: &Document, chosen: &[bool]) -> String {
    let mut out = String::new();
    let mut last_section: Option<u32> = None;
    for (sentence, _) in doc.sentences.iter().zip(chosen).filter(|(_, c)| **c) {
        match last_section {
            None => {}
            Some(section) if section == sentence.section => out.push(' '),
            Some(_) => out.push('\n'),
        }
        out.push_str(&sentence.text);
        last_section = Some(sentence.section);
    }
    out
}

/// Split the text into cleaned sentences with their section, then weigh
/// its terms.
fn analyze(text: &str) -> Document {
    let raw = split_sentences(text);
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut terms: Vec<String> = Vec::new();
    let mut is_phrase: Vec<bool> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    let mut spread: Vec<u32> = Vec::new();
    let mut per_sentence: Vec<Vec<u32>> = Vec::with_capacity(raw.len());
    for (sentence_text, _) in &raw {
        let tokens = tokenize(sentence_text);
        let mut ids: Vec<u32> = Vec::with_capacity(tokens.len() * 2);
        for token in phrases_and_words(&tokens) {
            let id = intern(
                &mut index,
                &mut terms,
                &mut is_phrase,
                &mut counts,
                &mut spread,
                token,
            );
            counts[usize::try_from(id).unwrap_or(usize::MAX)] += 1;
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        for id in &ids {
            spread[usize::try_from(*id).unwrap_or(usize::MAX)] += 1;
        }
        per_sentence.push(ids);
    }
    let weights = term_weights(&counts, &spread, &is_phrase, raw.len());
    let sentences = raw
        .into_iter()
        .zip(per_sentence)
        .map(|((text, section), ids)| {
            let vector: Vec<(u32, f64)> = ids
                .iter()
                .map(|id| (*id, weights[usize::try_from(*id).unwrap_or(usize::MAX)]))
                .collect();
            let norm = vector.iter().map(|(_, w)| w * w).sum::<f64>().sqrt();
            Sentence {
                text,
                section,
                vector,
                norm,
            }
        })
        .collect();
    Document {
        sentences,
        terms,
        weights,
    }
}

fn intern(
    index: &mut HashMap<String, u32>,
    terms: &mut Vec<String>,
    is_phrase: &mut Vec<bool>,
    counts: &mut Vec<u32>,
    spread: &mut Vec<u32>,
    token: String,
) -> u32 {
    if let Some(id) = index.get(&token) {
        return *id;
    }
    let id = u32::try_from(terms.len()).unwrap_or(u32::MAX);
    is_phrase.push(token.contains(' '));
    index.insert(token.clone(), id);
    terms.push(token);
    counts.push(0);
    spread.push(0);
    id
}

/// `count × ln(1 + sentences / spread)`: said often, but in few places. A
/// phrase that does not recur weighs nothing; one that does outranks its
/// words.
fn term_weights(counts: &[u32], spread: &[u32], is_phrase: &[bool], sentences: usize) -> Vec<f64> {
    let n = f64::from(u32::try_from(sentences).unwrap_or(u32::MAX));
    counts
        .iter()
        .zip(spread)
        .zip(is_phrase)
        .map(|((count, spread), phrase)| {
            if *phrase && *count < PHRASE_COUNT_MIN {
                return 0.0;
            }
            let spread = f64::from(*spread).max(1.0);
            let base = f64::from(*count) * (1.0 + n / spread).ln();
            if *phrase { base * PHRASE_WEIGHT } else { base }
        })
        .collect()
}

/// The terms of a sentence, each followed by the bigram it starts with the
/// term adjacent to it in the text — never across a break, and never a
/// word paired with itself.
fn phrases_and_words(tokens: &[Token]) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len() * 2);
    for (i, token) in tokens.iter().enumerate() {
        let Token::Term(word) = token else {
            continue;
        };
        out.push(word.clone());
        if let Some(Token::Term(next)) = tokens.get(i + 1) {
            if next != word {
                out.push(format!("{word} {next}"));
            }
        }
    }
    out
}

/// A word of a sentence as a term, or a break: a function word, a number,
/// or a token outside the size bounds. Breaks carry no term but still
/// separate phrases, so a bigram never spans one (`risk of prostate` must
/// not yield `risk prostate`).
enum Token {
    Term(String),
    Break,
}

/// The tokens of a sentence in order: lowercase words, with function words
/// and numbers kept only as breaks between phrases.
fn tokenize(sentence: &str) -> Vec<Token> {
    sentence
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|t| t.trim_matches('\'').to_lowercase())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let sized = (TERM_CHARS_MIN..=TERM_CHARS_MAX).contains(&t.chars().count());
            let wordy = t.chars().any(char::is_alphabetic);
            if sized && wordy && !STOPWORDS.contains(&t.as_str()) {
                Token::Term(t)
            } else {
                Token::Break
            }
        })
        .collect()
}

/// Sentences with their section, in document order. Headings start
/// sections when the text has any; otherwise paragraphs do. Markdown
/// furniture is stripped; headings, code fences, and rules are skipped.
fn split_sentences(text: &str) -> Vec<(String, u32)> {
    let has_headings = text.lines().any(|l| l.trim_start().starts_with('#'));
    let mut out: Vec<(String, u32)> = Vec::new();
    let mut section: u32 = 0;
    let mut in_fence = false;
    let mut after_blank = false;
    for line in text.lines() {
        if out.len() >= SENTENCES_MAX {
            break;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.is_empty() {
            after_blank = true;
            continue;
        }
        let is_heading = trimmed.starts_with('#');
        let starts_section = if has_headings {
            is_heading
        } else {
            after_blank
        };
        if starts_section && !out.is_empty() && section + 1 < SECTIONS_MAX {
            section += 1;
        }
        after_blank = false;
        // A heading labels its section; it is not a sentence of it, and
        // its brevity would let it outscore every sentence it labels.
        if is_heading
            || trimmed
                .chars()
                .all(|c| matches!(c, '-' | '=' | '#' | '*' | '`' | '~' | '_'))
        {
            continue;
        }
        let cleaned = clean_line(trimmed);
        for sentence in sentence_boundaries(&cleaned) {
            if out.len() >= SENTENCES_MAX {
                break;
            }
            out.push((sentence, section));
        }
    }
    out
}

/// Strip list, quote, and heading markers and turn links into their text.
fn clean_line(line: &str) -> String {
    let line = line
        .trim_start_matches(['#', '>', '*', '-', ' ', '+'])
        .trim_end_matches(['#', ' ']);
    strip_links(line)
}

/// Split a line at sentence terminators followed by whitespace.
fn sentence_boundaries(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut previous_terminator = false;
    for (offset, c) in line.char_indices() {
        if previous_terminator && c.is_whitespace() {
            let piece = line[start..offset].trim();
            if !piece.is_empty() {
                out.push(piece.to_string());
            }
            start = offset;
        }
        previous_terminator = matches!(c, '.' | '!' | '?');
    }
    let tail = line[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// Replace `[text](url)` with `text`. Hand-rolled to keep regex out of core.
pub fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    for _ in 0..line.len() {
        let Some(open) = rest.find('[') else { break };
        let Some(close_rel) = rest[open..].find(']') else {
            break;
        };
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

    #[test]
    fn strip_stopwords_keeps_the_terms_that_name_things() {
        assert_eq!(
            strip_stopwords("What is the name of the new metric for SRE?"),
            "name metric SRE?"
        );
        assert_eq!(strip_stopwords("what is the"), "what is the");
        assert_eq!(strip_stopwords("Kappa-style"), "Kappa-style");
    }

    const NOTE: &str = "# Kitchen renovation\n\nThe kitchen renovation starts in March. \
        Cabinets come from the supplier in Reno.\n\n## Budget\n\nThe kitchen renovation budget \
        is forty thousand. Counters are the largest line item.\n\n## Timeline\n\nDemolition in \
        March; cabinets in April. The kitchen renovation ends in May.\n";

    #[test]
    fn select_returns_a_fitting_text_whole() {
        assert_eq!(select("Short note.", 100), "Short note.");
    }

    #[test]
    fn select_covers_every_section_before_filling() {
        let picked = select(NOTE, 130);
        assert!(picked.contains("March"), "{picked}");
        assert!(
            picked.contains("budget") || picked.contains("Counters"),
            "{picked}"
        );
        assert!(
            picked.contains("May") || picked.contains("Demolition"),
            "{picked}"
        );
        assert!(picked.chars().count() <= 130);
    }

    #[test]
    fn select_keeps_document_order() {
        let picked = select(NOTE, 400);
        let march = picked.find("starts in March").expect("first section kept");
        let may = picked.find("ends in May").expect("last section kept");
        assert!(march < may);
    }

    #[test]
    fn select_cuts_the_best_sentence_when_none_fits() {
        let picked = select("Budget notes for the kitchen. Kitchen demo in June.", 12);
        assert!(!picked.is_empty());
        assert!(picked.chars().count() <= 12, "{picked}");
    }

    #[test]
    fn select_of_empty_text_is_empty() {
        assert_eq!(select("", 10), "");
        assert_eq!(select("---\n\n===\n", 1), "");
    }

    #[test]
    fn keywords_prefer_recurring_phrases_and_absorb_their_words() {
        let words = keywords(NOTE, 5);
        assert_eq!(
            words.first().map(String::as_str),
            Some("kitchen renovation"),
            "{words:?}"
        );
        assert!(!words.iter().any(|w| w == "kitchen"), "{words:?}");
        assert!(!words.iter().any(|w| w == "renovation"), "{words:?}");
        assert!(words.len() <= 5);
    }

    #[test]
    fn keywords_skip_function_words_and_numbers() {
        let words = keywords("The the the 2024 2024 2024 lathe lathe.", 5);
        assert_eq!(words, vec!["lathe"]);
    }

    #[test]
    fn phrases_never_span_a_dropped_word() {
        let text = "Risk of prostate cancer. Risk of prostate cancer. Risk of prostate cancer.";
        let words = keywords(text, 5);
        assert!(words.iter().any(|w| w == "prostate cancer"), "{words:?}");
        assert!(!words.iter().any(|w| w == "risk prostate"), "{words:?}");
    }

    #[test]
    fn phrases_never_pair_a_word_with_itself() {
        let words = keywords("vitamin vitamin vitamin vitamin", 5);
        assert_eq!(words, vec!["vitamin"]);
    }

    #[test]
    fn keywords_of_empty_text_are_none() {
        assert!(keywords("", 5).is_empty());
    }

    #[test]
    fn sentences_split_at_terminators_not_abbreviation_dots_inside_words() {
        let s = sentence_boundaries("See v1.2 now. Then stop! Really? yes");
        assert_eq!(s, vec!["See v1.2 now.", "Then stop!", "Really?", "yes"]);
    }

    #[test]
    fn strip_links_leaves_plain_brackets_alone() {
        assert_eq!(strip_links("a [note] here"), "a [note] here");
        assert_eq!(strip_links("[x](https://y) and [z](https://w)"), "x and z");
    }

    #[test]
    fn fenced_code_is_ignored() {
        let text = "Prose line one.\n\n```\nlet code = 1;\n```\n\nProse line two.";
        let picked = select(text, 15);
        assert!(!picked.contains("code"), "{picked}");
    }
}
