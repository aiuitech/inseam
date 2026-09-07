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
            .filter_map(|c| c.bytes_file.clone())
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
