//! The golden-check schema (`<name>.checks.toml`) shared by both plugin
//! tiers: one parser, one expectation matcher, one coverage gate. A loaded
//! plugin's checks run through the wasm bridge (`inseam plugin check`); a
//! linked transform's run through the seam (`golden_transforms`). Both tiers
//! judge the same file shape with this code, so "what counts as a test" has
//! exactly one definition.
//!
//! Checks are data, not code: an example input the harness feeds the plugin,
//! and the output shape the plugin promises for it. Nothing in a checks file
//! needs to be trusted — only run.

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// Fragments a single check may emit before the harness stops collecting —
/// a guard against a runaway plugin, not a product limit.
pub const FRAGMENTS_PER_CHECK_MAX: usize = 10_000;

/// Characters of fragment text the failure report quotes per fragment.
const DESCRIBE_TEXT_CHARS: usize = 60;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecksFile {
    #[serde(default)]
    pub check: Vec<GoldenCheck>,
}

/// One declarative check: an input the harness feeds through the real
/// bridge or seam, and the output shape the plugin promises for it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenCheck {
    pub name: String,
    /// The claimed mimetype this application arrives as.
    pub mimetype: String,
    #[serde(default = "default_true")]
    pub is_root: bool,
    /// Fragment text handed in; absent = content the node did not read.
    #[serde(default)]
    pub text: Option<String>,
    /// Fixture handed to `source-bytes`, relative to the checks file.
    #[serde(default)]
    pub bytes_file: Option<PathBuf>,
    /// What the granted LLM returns verbatim; absent = the LLM refuses,
    /// which is how a check exercises the degrade path.
    #[serde(default)]
    pub llm_returns: Option<String>,
    /// Canned network replies for a transform that fetches (`hosts` in its
    /// manifest); absent, every fetch refuses.
    #[serde(default)]
    pub fetch: Vec<CannedFetch>,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expect {
    /// Defaults to 1: a check asserts the plugin produces something unless
    /// it says otherwise (`min_fragments = 0, max_fragments = 0` asserts
    /// clean degradation).
    pub min_fragments: Option<usize>,
    pub max_fragments: Option<usize>,
    /// At least one emitted fragment's text contains this.
    pub fragment_contains: Option<String>,
    /// At least one emitted fragment carries this relation.
    pub relation: Option<String>,
    /// At least one emitted fragment's mimetype starts with this.
    pub mimetype: Option<String>,
    /// At least one keyed sprout's key or text contains this (linked
    /// transforms only — the transform WIT seam emits child fragments,
    /// never keyed sprouts).
    pub keyed_contains: Option<String>,
    pub max_keyed: Option<usize>,
}

/// What a plugin emitted for one check, in the tier-neutral shape the
/// matcher judges. Each tier converts its own output type into this.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Emitted {
    pub fragments: Vec<EmittedFragment>,
    pub keyed: Vec<EmittedKeyed>,
}

/// One keyed sprout as emitted: its index-wide key, the relation it anchors
/// with, and its text.
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedKeyed {
    pub key: String,
    pub relation: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmittedFragment {
    pub mimetype: String,
    pub relation: String,
    pub text: Option<String>,
}

fn default_true() -> bool {
    true
}

impl ChecksFile {
    pub fn parse(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }

    /// The fixture paths the checks reference (relative to the checks
    /// file) — what an installer must fetch alongside it.
    pub fn fixture_files(&self) -> Vec<PathBuf> {
        self.check
            .iter()
            .flat_map(|c| {
                c.bytes_file
                    .clone()
                    .into_iter()
                    .chain(c.fetch.iter().filter_map(|f| f.body_file.clone()))
            })
            .collect()
    }

    /// The mandatory coverage every plugin ships, whichever tier: at least
    /// one check that *proves the claim* (a substantive expectation, not
    /// just "something came out") and at least one that *pins the degrade
    /// path* (input starved, output shape fixed). "Has a checks file" is not
    /// the bar; "demonstrates what it does and what it does without" is.
    /// Returns every unmet requirement, worded as the fix.
    pub fn required_coverage(&self) -> Result<(), Vec<String>> {
        let mut unmet = Vec::new();
        if self.check.is_empty() {
            unmet.push("the checks file declares no checks".to_string());
        }
        if !self.check.iter().any(|c| c.expect.is_substantive()) {
            unmet.push(
                "no check proves the claim: at least one check needs a substantive \
                 expectation (`fragment_contains`, `relation`, `mimetype`, or `entity`)"
                    .to_string(),
            );
        }
        if !self
            .check
            .iter()
            .any(|c| c.is_starved() && c.expect.pins_output())
        {
            unmet.push(
                "no check pins the degrade path: at least one check must withhold both \
                 `text` and `llm_returns` and set `max_fragments` (0 for \"emits nothing\", \
                 or the fallback's shape)"
                    .to_string(),
            );
        }
        if unmet.is_empty() { Ok(()) } else { Err(unmet) }
    }
}

impl GoldenCheck {
    /// A starved check withholds the two inputs a plugin can be denied at
    /// runtime — content the node did not read, and an LLM that refuses.
    pub fn is_starved(&self) -> bool {
        self.text.is_none() && self.llm_returns.is_none()
    }

    /// Resolve `bytes_file` beside the checks file. A path that climbs out
    /// of the plugin directory is refused, not resolved — fixtures travel
    /// with the checks and never reach outside them.
    pub fn fixture_path(&self, checks_path: &Path) -> Result<Option<PathBuf>, String> {
        let Some(relative) = &self.bytes_file else {
            return Ok(None);
        };
        let escapes = relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)));
        if escapes {
            return Err(format!(
                "bytes_file `{}` escapes the plugin directory",
                relative.display()
            ));
        }
        let base = checks_path.parent().unwrap_or(Path::new("."));
        Ok(Some(base.join(relative)))
    }

    /// Judge what the plugin emitted against this check's expectations:
    /// every miss, followed by what actually came out, so a failing check
    /// reads as a diff rather than a riddle.
    pub fn verdict(&self, emitted: &Emitted) -> Result<(), String> {
        let misses = self.expect.unmet(emitted);
        if misses.is_empty() {
            Ok(())
        } else {
            Err(format!("{}; got {}", misses.join("; "), emitted.describe()))
        }
    }
}

impl Expect {
    /// Asserts something about *what* came out, not just how much.
    pub fn is_substantive(&self) -> bool {
        self.fragment_contains.is_some()
            || self.relation.is_some()
            || self.mimetype.is_some()
            || self.keyed_contains.is_some()
    }

    /// Bounds the output from above — the difference between "degrades to
    /// at most this" and "anything goes".
    pub fn pins_output(&self) -> bool {
        self.max_fragments.is_some()
    }

    pub fn unmet(&self, emitted: &Emitted) -> Vec<String> {
        let fragments = &emitted.fragments;
        let mut misses = Vec::new();
        let min = self.min_fragments.unwrap_or(1);
        if fragments.len() < min {
            misses.push(format!("expected at least {min} fragment(s)"));
        }
        if let Some(max) = self.max_fragments
            && fragments.len() > max
        {
            misses.push(format!("expected at most {max} fragment(s)"));
        }
        if let Some(needle) = &self.fragment_contains
            && !fragments
                .iter()
                .any(|f| f.text.as_deref().is_some_and(|t| t.contains(needle)))
        {
            misses.push(format!("no fragment text contains {needle:?}"));
        }
        if let Some(relation) = &self.relation
            && !fragments.iter().any(|f| &f.relation == relation)
        {
            misses.push(format!("no fragment carries relation {relation:?}"));
        }
        if let Some(prefix) = &self.mimetype
            && !fragments
                .iter()
                .any(|f| f.mimetype.starts_with(prefix.as_str()))
        {
            misses.push(format!("no fragment mimetype starts with {prefix:?}"));
        }
        if let Some(needle) = &self.keyed_contains
            && !emitted.keyed.iter().any(|k| {
                k.key.contains(needle.as_str())
                    || k.text
                        .as_deref()
                        .is_some_and(|t| t.contains(needle.as_str()))
            })
        {
            misses.push(format!("no keyed sprout's key or text contains {needle:?}"));
        }
        if let Some(max) = self.max_keyed
            && emitted.keyed.len() > max
        {
            misses.push(format!("expected at most {max} keyed sprout(s)"));
        }
        misses
    }
}

impl Emitted {
    /// A one-line account of the output, for failure reports.
    pub fn describe(&self) -> String {
        let mut out = format!("{} fragment(s)", self.fragments.len());
        for f in &self.fragments {
            let text: String = match &f.text {
                None => "(no text)".to_string(),
                Some(t) => {
                    let short: String = t.chars().take(DESCRIBE_TEXT_CHARS).collect();
                    if short.len() < t.len() {
                        format!("{short:?}…")
                    } else {
                        format!("{short:?}")
                    }
                }
            };
            out.push_str(&format!(" [{} {} {text}]", f.mimetype, f.relation));
        }
        if !self.keyed.is_empty() {
            let keys: Vec<&str> = self.keyed.iter().map(|k| k.key.as_str()).collect();
            out.push_str(&format!(", {} keyed {:?}", self.keyed.len(), keys));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(mimetype: &str, relation: &str, text: Option<&str>) -> EmittedFragment {
        EmittedFragment {
            mimetype: mimetype.into(),
            relation: relation.into(),
            text: text.map(Into::into),
        }
    }

    fn emitted(fragments: Vec<EmittedFragment>) -> Emitted {
        Emitted {
            fragments,
            keyed: Vec::new(),
        }
    }

    #[test]
    fn expect_defaults_require_one_fragment() {
        let expect = Expect::default();
        assert!(
            expect
                .unmet(&emitted(vec![fragment(
                    "text/plain",
                    "contains",
                    Some("x")
                )]))
                .is_empty()
        );
        assert_eq!(expect.unmet(&emitted(Vec::new())).len(), 1);
    }

    #[test]
    fn expect_matches_contents_relation_and_mimetype() {
        let expect: Expect = toml::from_str(
            r#"
            fragment_contains = "GARAGE"
            relation = "transcribes"
            mimetype = "text/plain"
            "#,
        )
        .expect("parses");
        let hit = emitted(vec![fragment(
            "text/plain;via=ocr",
            "transcribes",
            Some("GARAGE SALE"),
        )]);
        assert!(expect.unmet(&hit).is_empty());
        let miss = emitted(vec![fragment("text/html", "contains", Some("nothing"))]);
        assert_eq!(expect.unmet(&miss).len(), 3);
    }

    #[test]
    fn expect_zero_zero_asserts_clean_degradation() {
        let expect: Expect =
            toml::from_str("min_fragments = 0\nmax_fragments = 0").expect("parses");
        assert!(expect.unmet(&emitted(Vec::new())).is_empty());
        assert!(
            !expect
                .unmet(&emitted(vec![fragment("text/plain", "contains", None)]))
                .is_empty()
        );
    }

    #[test]
    fn expect_matches_keyed_sprouts_by_key_or_text_and_caps_them() {
        let expect: Expect =
            toml::from_str("min_fragments = 0\nkeyed_contains = \"Ada\"\nmax_keyed = 1")
                .expect("parses");
        let keyed = |key: &str, text: &str| EmittedKeyed {
            key: key.into(),
            relation: "mentions".into(),
            text: Some(text.into()),
        };
        let hit = Emitted {
            fragments: Vec::new(),
            keyed: vec![keyed("entity:person:ada", "Ada")],
        };
        assert!(expect.unmet(&hit).is_empty());
        let miss = Emitted {
            fragments: Vec::new(),
            keyed: vec![
                keyed("entity:person:bob", "Bob"),
                keyed("entity:person:cy", "Cy"),
            ],
        };
        assert_eq!(expect.unmet(&miss).len(), 2);
    }

    #[test]
    fn verdict_names_misses_and_what_came_out() {
        let check: GoldenCheck = toml::from_str(
            "name = \"x\"\nmimetype = \"text/plain\"\n[expect]\nfragment_contains = \"needle\"",
        )
        .expect("parses");
        let err = check
            .verdict(&emitted(vec![fragment(
                "text/plain",
                "contains",
                Some("hay"),
            )]))
            .expect_err("misses");
        assert!(
            err.contains("no fragment text contains \"needle\""),
            "{err}"
        );
        assert!(
            err.contains("got 1 fragment(s) [text/plain contains \"hay\"]"),
            "{err}"
        );
    }

    #[test]
    fn fixture_files_lists_bytes_fixtures() {
        let file = ChecksFile::parse(
            r#"
            [[check]]
            name = "a"
            mimetype = "image/png"
            bytes_file = "fixtures/pixel.png"

            [[check]]
            name = "b"
            mimetype = "text/plain"
        "#,
        )
        .expect("parses");
        assert_eq!(
            file.fixture_files(),
            vec![PathBuf::from("fixtures/pixel.png")]
        );
    }

    #[test]
    fn fixture_path_resolves_beside_checks_and_refuses_escapes() {
        let inside: GoldenCheck = toml::from_str(
            "name = \"a\"\nmimetype = \"image/png\"\nbytes_file = \"fixtures/p.png\"",
        )
        .expect("parses");
        assert_eq!(
            inside
                .fixture_path(Path::new("/plugins/ocr/ocr.checks.toml"))
                .expect("resolves"),
            Some(PathBuf::from("/plugins/ocr/fixtures/p.png"))
        );
        let outside: GoldenCheck = toml::from_str(
            "name = \"a\"\nmimetype = \"image/png\"\nbytes_file = \"../etc/passwd\"",
        )
        .expect("parses");
        assert!(
            outside
                .fixture_path(Path::new("/plugins/ocr/ocr.checks.toml"))
                .is_err()
        );
        let none: GoldenCheck =
            toml::from_str("name = \"a\"\nmimetype = \"text/plain\"").expect("parses");
        assert_eq!(
            none.fixture_path(Path::new("x.checks.toml"))
                .expect("resolves"),
            None
        );
    }

    const COVERED: &str = r#"
        [[check]]
        name = "proves the claim"
        mimetype = "text/plain"
        text = "hello"
        [check.expect]
        relation = "contains"

        [[check]]
        name = "pins the degrade path"
        mimetype = "text/plain"
        [check.expect]
        min_fragments = 0
        max_fragments = 0
    "#;

    #[test]
    fn required_coverage_accepts_a_positive_plus_a_starved_check() {
        assert!(
            ChecksFile::parse(COVERED)
                .expect("parses")
                .required_coverage()
                .is_ok()
        );
    }

    #[test]
    fn required_coverage_rejects_an_empty_file_with_every_requirement_named() {
        let unmet = ChecksFile::parse("")
            .expect("parses")
            .required_coverage()
            .expect_err("empty");
        assert_eq!(unmet.len(), 3, "{unmet:?}");
    }

    #[test]
    fn required_coverage_rejects_vacuous_checks() {
        // "Something came out" proves nothing about the claim.
        let vacuous = r#"
            [[check]]
            name = "emits something"
            mimetype = "text/plain"
            text = "hello"

            [[check]]
            name = "starved but unpinned"
            mimetype = "text/plain"
            [check.expect]
            min_fragments = 0
        "#;
        let unmet = ChecksFile::parse(vacuous)
            .expect("parses")
            .required_coverage()
            .expect_err("vacuous");
        assert_eq!(unmet.len(), 2, "{unmet:?}");
        assert!(unmet[0].contains("proves the claim"), "{unmet:?}");
        assert!(unmet[1].contains("pins the degrade path"), "{unmet:?}");
    }

    #[test]
    fn a_starved_check_withholds_text_and_llm() {
        let file = ChecksFile::parse(COVERED).expect("parses");
        assert!(!file.check[0].is_starved());
        assert!(file.check[1].is_starved());
        let with_llm: GoldenCheck =
            toml::from_str("name = \"a\"\nmimetype = \"image/png\"\nllm_returns = \"x\"")
                .expect("parses");
        assert!(!with_llm.is_starved());
    }
}

// ---------------------------------------------------------------------------
// Connection checks: the `connections` seam's golden shape
// ---------------------------------------------------------------------------
//
// A connection plugin is not fed text and asked for fragments; it is
// configured, then asked to enumerate a scope, read a locator, or describe
// one, reaching its host through `fetch`. Its checks therefore carry a
// plugin config, canned HTTP replies (the network as data — the same move
// `llm_returns` makes for the LLM), and expectations about sources, bytes,
// or an envelope. The coverage rule is the same in spirit: one check that
// proves the claim, one starved check (no canned replies — every fetch
// refuses, the offline node) that pins what happens then.

/// Canned replies one connection check may carry; a check needing more is
/// describing a corpus, not an example.
pub const CANNED_FETCHES_PER_CHECK_MAX: usize = 64;

/// Sources a single enumeration check may return before the harness stops
/// collecting — a guard against a runaway plugin, not a product limit.
pub const SOURCES_PER_CHECK_MAX: usize = 10_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionChecksFile {
    /// The plugin's own configuration every check configures the instance
    /// with (`[entry.config.plugin]` at mount).
    #[serde(default)]
    pub config: toml::Table,
    #[serde(default)]
    pub check: Vec<ConnectionCheck>,
}

/// Which seam call a check exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionCall {
    Enumerate,
    Read,
    Describe,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionCheck {
    pub name: String,
    pub call: ConnectionCall,
    /// The scope an `enumerate` check names.
    #[serde(default)]
    pub root: String,
    /// The locator a `read` or `describe` check names.
    #[serde(default)]
    pub locator: Option<String>,
    /// The canned network: replies matched by URL. Absent (a *starved*
    /// check), every fetch refuses — the offline node.
    #[serde(default)]
    pub fetch: Vec<CannedFetch>,
    pub expect: ConnectionExpect,
}

/// One canned HTTP reply.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CannedFetch {
    /// The exact URL this reply answers.
    pub url: String,
    #[serde(default = "default_status")]
    pub status: u16,
    /// The body inline, or...
    #[serde(default)]
    pub body: Option<String>,
    /// ...as a fixture beside the checks file.
    #[serde(default)]
    pub body_file: Option<PathBuf>,
    #[serde(default)]
    pub content_type: Option<String>,
    /// The reply requires the request to be authorized (the plugin asked
    /// for the grant's bearer); an unauthorized request gets 401 instead —
    /// how a private API behaves.
    #[serde(default)]
    pub authorized: bool,
}

fn default_status() -> u16 {
    200
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionExpect {
    /// Enumerate: sources returned, bounded below (default 1 unless
    /// `error = true`).
    #[serde(default)]
    pub min_sources: Option<usize>,
    #[serde(default)]
    pub max_sources: Option<usize>,
    /// Enumerate: some source's locator contains this.
    #[serde(default)]
    pub locator_contains: Option<String>,
    /// Enumerate or describe: some source's (the described) content type
    /// starts with this.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Enumerate or describe: some source's (the described) hint contains
    /// this.
    #[serde(default)]
    pub hint_contains: Option<String>,
    /// Read: the bytes, as text, contain this.
    #[serde(default)]
    pub text_contains: Option<String>,
    /// The call must return an error (`true`) — never a trap — or must
    /// succeed (`false`, the default).
    #[serde(default)]
    pub error: Option<bool>,
}

/// What a connection call produced, tier-neutral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionOutcome {
    Sources(Vec<EmittedSource>),
    Bytes(Vec<u8>),
    Envelope(EmittedEnvelope),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedSource {
    pub locator: String,
    pub content_type: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedEnvelope {
    pub content_type: String,
    pub hint: Option<String>,
}

impl ConnectionChecksFile {
    pub fn parse(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }

    /// The fixture paths the canned replies reference (relative to the
    /// checks file) — what an installer must fetch alongside it.
    pub fn fixture_files(&self) -> Vec<PathBuf> {
        self.check
            .iter()
            .flat_map(|c| c.fetch.iter().filter_map(|f| f.body_file.clone()))
            .collect()
    }

    /// The connection tier's mandatory coverage: one check that proves the
    /// claim (a substantive expectation about sources, bytes, or an
    /// envelope) and one starved check (no canned replies) that pins what
    /// the plugin does offline (`error = true`, or `max_sources`).
    pub fn required_coverage(&self) -> Result<(), Vec<String>> {
        let mut unmet = Vec::new();
        if self.check.is_empty() {
            unmet.push("the checks file declares no checks".to_string());
        }
        if !self.check.iter().any(|c| c.expect.is_substantive()) {
            unmet.push(
                "no check proves the claim: at least one check needs a substantive \
                 expectation (`locator_contains`, `content_type`, `hint_contains`, or \
                 `text_contains`)"
                    .to_string(),
            );
        }
        if !self
            .check
            .iter()
            .any(|c| c.is_starved() && c.expect.pins_outcome())
        {
            unmet.push(
                "no check pins the offline path: at least one check must carry no \
                 `[[check.fetch]]` replies and set `error = true` or `max_sources`"
                    .to_string(),
            );
        }
        if let Some(check) = self
            .check
            .iter()
            .find(|c| c.fetch.len() > CANNED_FETCHES_PER_CHECK_MAX)
        {
            unmet.push(format!(
                "check \"{}\" carries {} canned replies; at most {CANNED_FETCHES_PER_CHECK_MAX}",
                check.name,
                check.fetch.len()
            ));
        }
        if unmet.is_empty() { Ok(()) } else { Err(unmet) }
    }
}

impl ConnectionCheck {
    /// A starved check carries no canned network: every fetch refuses.
    pub fn is_starved(&self) -> bool {
        self.fetch.is_empty()
    }

    /// The locator a read or describe check names, or the reason it must.
    pub fn locator(&self) -> Result<&str, String> {
        match (self.call, &self.locator) {
            (ConnectionCall::Enumerate, _) => Ok(""),
            (_, Some(locator)) => Ok(locator),
            (call, None) => Err(format!("a `{call:?}` check needs a `locator`").to_lowercase()),
        }
    }

    pub fn verdict(&self, outcome: &ConnectionOutcome) -> Result<(), String> {
        let misses = self.expect.unmet(self.call, outcome);
        if misses.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{}; got {}",
                misses.join("; "),
                describe_outcome(outcome)
            ))
        }
    }
}

impl CannedFetch {
    /// Resolve `body_file` beside the checks file, refusing a path that
    /// climbs out of the plugin directory.
    pub fn fixture_path(&self, checks_path: &Path) -> Result<Option<PathBuf>, String> {
        let Some(relative) = &self.body_file else {
            return Ok(None);
        };
        let escapes = relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)));
        if escapes {
            return Err(format!(
                "body_file `{}` escapes the plugin directory",
                relative.display()
            ));
        }
        let base = checks_path.parent().unwrap_or(Path::new("."));
        Ok(Some(base.join(relative)))
    }
}

impl ConnectionExpect {
    /// Asserts something about *what* came back, not just that the call
    /// went one way or the other.
    pub fn is_substantive(&self) -> bool {
        let expects_success = self.error != Some(true);
        expects_success
            && (self.locator_contains.is_some()
                || self.content_type.is_some()
                || self.hint_contains.is_some()
                || self.text_contains.is_some())
    }

    /// Fixes the offline outcome: an error, or a bounded source count.
    pub fn pins_outcome(&self) -> bool {
        self.error == Some(true) || self.max_sources.is_some()
    }

    pub fn unmet(&self, call: ConnectionCall, outcome: &ConnectionOutcome) -> Vec<String> {
        let mut misses = Vec::new();
        match (self.error, outcome) {
            (Some(true), ConnectionOutcome::Error(_)) => return misses,
            (Some(true), _) => {
                misses.push("expected the call to return an error".to_string());
                return misses;
            }
            (_, ConnectionOutcome::Error(_)) => {
                misses.push("expected the call to succeed".to_string());
                return misses;
            }
            (_, _) => {}
        }
        match (call, outcome) {
            (ConnectionCall::Enumerate, ConnectionOutcome::Sources(sources)) => {
                self.unmet_sources(sources, &mut misses);
            }
            (ConnectionCall::Read, ConnectionOutcome::Bytes(bytes)) => {
                if let Some(needle) = &self.text_contains {
                    let text = String::from_utf8_lossy(bytes);
                    if !text.contains(needle.as_str()) {
                        misses.push(format!("the bytes do not contain {needle:?}"));
                    }
                }
            }
            (ConnectionCall::Describe, ConnectionOutcome::Envelope(envelope)) => {
                if let Some(prefix) = &self.content_type
                    && !envelope.content_type.starts_with(prefix.as_str())
                {
                    misses.push(format!("content type does not start with {prefix:?}"));
                }
                if let Some(needle) = &self.hint_contains
                    && !envelope
                        .hint
                        .as_deref()
                        .is_some_and(|h| h.contains(needle.as_str()))
                {
                    misses.push(format!("the hint does not contain {needle:?}"));
                }
            }
            (call, other) => misses.push(format!(
                "a {call:?} call answered with {}",
                describe_outcome(other)
            )),
        }
        misses
    }

    fn unmet_sources(&self, sources: &[EmittedSource], misses: &mut Vec<String>) {
        let min = self.min_sources.unwrap_or(1);
        if sources.len() < min {
            misses.push(format!("expected at least {min} source(s)"));
        }
        if let Some(max) = self.max_sources
            && sources.len() > max
        {
            misses.push(format!("expected at most {max} source(s)"));
        }
        if let Some(needle) = &self.locator_contains
            && !sources.iter().any(|s| s.locator.contains(needle.as_str()))
        {
            misses.push(format!("no source locator contains {needle:?}"));
        }
        if let Some(prefix) = &self.content_type
            && !sources
                .iter()
                .any(|s| s.content_type.starts_with(prefix.as_str()))
        {
            misses.push(format!("no source's content type starts with {prefix:?}"));
        }
        if let Some(needle) = &self.hint_contains
            && !sources.iter().any(|s| {
                s.hint
                    .as_deref()
                    .is_some_and(|h| h.contains(needle.as_str()))
            })
        {
            misses.push(format!("no source's hint contains {needle:?}"));
        }
    }
}

fn describe_outcome(outcome: &ConnectionOutcome) -> String {
    match outcome {
        ConnectionOutcome::Sources(sources) => {
            let shown: Vec<String> = sources
                .iter()
                .take(5)
                .map(|s| format!("{} [{}]", s.locator, s.content_type))
                .collect();
            format!("{} source(s) [{}]", sources.len(), shown.join(", "))
        }
        ConnectionOutcome::Bytes(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            let preview: String = text.chars().take(DESCRIBE_TEXT_CHARS).collect();
            format!("{} byte(s) {preview:?}", bytes.len())
        }
        ConnectionOutcome::Envelope(envelope) => format!(
            "envelope [{}] hint {:?}",
            envelope.content_type, envelope.hint
        ),
        ConnectionOutcome::Error(reason) => format!("error {reason:?}"),
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    const CHECKS: &str = r#"
[config]
repository = "octo/hello"

[[check]]
name = "enumerates the tree"
call = "enumerate"
[[check.fetch]]
url = "https://api.example/tree"
body = "{}"
[check.expect]
locator_contains = "README"

[[check]]
name = "errors offline"
call = "enumerate"
[check.expect]
error = true
"#;

    #[test]
    fn connection_checks_parse_and_meet_coverage() {
        let file = ConnectionChecksFile::parse(CHECKS).expect("parses");
        assert_eq!(file.config["repository"].as_str(), Some("octo/hello"));
        assert!(file.required_coverage().is_ok());
        assert!(file.check[1].is_starved());
        assert!(!file.check[0].is_starved());
    }

    #[test]
    fn coverage_needs_a_substantive_and_a_starved_pinned_check() {
        let vacuous = ConnectionChecksFile::parse(
            r#"
[[check]]
name = "just runs"
call = "enumerate"
[[check.fetch]]
url = "https://api.example/tree"
[check.expect]
min_sources = 0
"#,
        )
        .expect("parses");
        let unmet = vacuous.required_coverage().expect_err("fails coverage");
        assert_eq!(unmet.len(), 2, "{unmet:?}");
    }

    #[test]
    fn verdicts_judge_sources_bytes_envelopes_and_errors() {
        let file = ConnectionChecksFile::parse(CHECKS).expect("parses");
        let sources = ConnectionOutcome::Sources(vec![EmittedSource {
            locator: "README.md".into(),
            content_type: "text/markdown".into(),
            hint: None,
        }]);
        assert!(file.check[0].verdict(&sources).is_ok());
        assert!(
            file.check[0]
                .verdict(&ConnectionOutcome::Sources(Vec::new()))
                .is_err()
        );
        assert!(
            file.check[0]
                .verdict(&ConnectionOutcome::Error("offline".into()))
                .is_err()
        );
        assert!(
            file.check[1]
                .verdict(&ConnectionOutcome::Error("offline".into()))
                .is_ok()
        );
        assert!(file.check[1].verdict(&sources).is_err());

        let read = ConnectionCheck {
            name: "reads".into(),
            call: ConnectionCall::Read,
            root: String::new(),
            locator: Some("README.md".into()),
            fetch: Vec::new(),
            expect: ConnectionExpect {
                text_contains: Some("hello".into()),
                ..ConnectionExpect::default()
            },
        };
        assert!(
            read.verdict(&ConnectionOutcome::Bytes(b"say hello".to_vec()))
                .is_ok()
        );
        assert!(
            read.verdict(&ConnectionOutcome::Bytes(b"nope".to_vec()))
                .is_err()
        );
        assert!(
            read.verdict(&sources).is_err(),
            "a read answering with sources is a miss"
        );
        let describe = ConnectionCheck {
            call: ConnectionCall::Describe,
            expect: ConnectionExpect {
                content_type: Some("text/".into()),
                ..ConnectionExpect::default()
            },
            ..read
        };
        let envelope = ConnectionOutcome::Envelope(EmittedEnvelope {
            content_type: "text/markdown".into(),
            hint: Some("README".into()),
        });
        assert!(describe.verdict(&envelope).is_ok());
    }

    #[test]
    fn canned_fixtures_stay_inside_the_plugin_directory() {
        let canned = CannedFetch {
            url: "u".into(),
            status: 200,
            body: None,
            body_file: Some(PathBuf::from("../escape.json")),
            content_type: None,
            authorized: false,
        };
        assert!(canned.fixture_path(Path::new("x/x.checks.toml")).is_err());
        let fine = CannedFetch {
            body_file: Some(PathBuf::from("fixtures/tree.json")),
            ..canned
        };
        assert_eq!(
            fine.fixture_path(Path::new("x/x.checks.toml"))
                .expect("resolves"),
            Some(PathBuf::from("x/fixtures/tree.json"))
        );
    }
}
