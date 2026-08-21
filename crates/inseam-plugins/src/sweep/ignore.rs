//! Ignore rules: the host-agnostic way to keep sources out of a node
//! (`design/ignore.md`). A rule is a conjunction of globs over the parts of
//! an address and its envelope that exist for every host — host id,
//! locator, source type, content type, hint, trust properties — so the same
//! vocabulary ignores a directory on a filesystem host and a correspondent
//! on a mail host. Rules are parsed from the sweep entry's config once, at
//! the boundary, into an [`IgnoreSet`]; a bad glob is a configuration error
//! that parks the sweep before any run, never a silent non-match.
//!
//! This is the sweep plugin's own vocabulary, not a kernel type: the kernel
//! knows no ignore rule, and the one consumer is the sweep. A second consumer
//! (a query-time ignore) would promote it to the `sweep` seam, not the
//! kernel.

use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use thiserror::Error;

use inseam_kernel::address::{Address, Envelope};

/// Upper bound on rules in one set. Far above any hand-written composition;
/// exists so the per-source match loop has a known ceiling.
pub const IGNORE_RULES_MAX: u32 = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IgnoreError {
    #[error("ignore rule {index} names no field; a rule with no field would ignore everything")]
    EmptyRule { index: u32 },
    #[error("ignore rule {index}, field `{field}`: glob `{glob}` is invalid: {reason}")]
    InvalidGlob {
        index: u32,
        field: &'static str,
        glob: String,
        reason: String,
    },
    #[error("{count} ignore rules exceed the limit of {IGNORE_RULES_MAX}")]
    TooManyRules { count: u32 },
}

/// One rule as written in a composition. Every field is a glob; a source is
/// ignored when **all** the fields a rule names match it, and a set ignores
/// a source when **any** of its rules does. `address` and `locator` are
/// matched path-wise (`*` stops at `/`, `**` does not); the other fields
/// are matched as plain strings (`*` matches anything).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IgnoreRule {
    /// Glob over the rendered address, `inseam://<host>/<locator>`.
    pub address: Option<String>,
    /// Glob over the host id.
    pub host: Option<String>,
    /// Glob over the locator (a path without its leading `/` on a
    /// filesystem host; opaque elsewhere).
    pub locator: Option<String>,
    /// Glob over the envelope's source type (`file`, `email`, ...).
    pub source_type: Option<String>,
    /// Glob over the content type's essence (`image/*`, `text/markdown`).
    pub content_type: Option<String>,
    /// Glob over the discovery hint (a filename, a subject line). A source
    /// without a hint never matches this field.
    pub hint: Option<String>,
    /// Glob over each trust property rendered `key:value`
    /// (`email:*@newsletters.example`); matches when any property does.
    pub property: Option<String>,
}

impl IgnoreRule {
    fn is_empty(&self) -> bool {
        self.address.is_none()
            && self.host.is_none()
            && self.locator.is_none()
            && self.source_type.is_none()
            && self.content_type.is_none()
            && self.hint.is_none()
            && self.property.is_none()
    }
}

/// Whether `*` in a glob may cross `/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlobShape {
    /// Path-like: `*` stops at `/`, `**` spans segments.
    Path,
    /// Plain text: `*` matches anything.
    Plain,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    address: Option<GlobMatcher>,
    host: Option<GlobMatcher>,
    locator: Option<GlobMatcher>,
    source_type: Option<GlobMatcher>,
    content_type: Option<GlobMatcher>,
    hint: Option<GlobMatcher>,
    property: Option<GlobMatcher>,
}

impl CompiledRule {
    /// Pure conjunction: every named field must match. A field the rule
    /// leaves unset matches anything.
    fn matches(&self, address: &Address, envelope: &Envelope) -> bool {
        let address_ok = matches_text(&self.address, &address.to_string());
        let host_ok = matches_text(&self.host, address.host.as_str());
        let locator_ok = matches_text(&self.locator, address.locator.as_str());
        let source_type_ok = matches_text(&self.source_type, &envelope.source_type);
        let content_type_ok = matches_text(&self.content_type, envelope.content_type.essence());
        let hint_ok = match (&self.hint, &envelope.hint) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(glob), Some(hint)) => glob.is_match(hint),
        };
        let property_ok = match &self.property {
            None => true,
            Some(glob) => envelope
                .properties
                .iter()
                .any(|p| glob.is_match(format!("{}:{}", p.key, p.value))),
        };
        address_ok
            && host_ok
            && locator_ok
            && source_type_ok
            && content_type_ok
            && hint_ok
            && property_ok
    }
}

fn matches_text(glob: &Option<GlobMatcher>, text: &str) -> bool {
    match glob {
        None => true,
        Some(glob) => glob.is_match(text),
    }
}

/// A compiled rule set, ready to test sources against.
#[derive(Debug, Clone, Default)]
pub struct IgnoreSet {
    rules: Vec<CompiledRule>,
}

impl IgnoreSet {
    /// Compile rules as written. Fails on an empty rule (it would ignore
    /// everything) and on any malformed glob, naming the rule and field.
    pub fn compile(rules: &[IgnoreRule]) -> Result<Self, IgnoreError> {
        let count = u32::try_from(rules.len()).unwrap_or(u32::MAX);
        if count > IGNORE_RULES_MAX {
            return Err(IgnoreError::TooManyRules { count });
        }
        let mut compiled = Vec::with_capacity(rules.len());
        for (index, rule) in (0u32..).zip(rules) {
            if rule.is_empty() {
                return Err(IgnoreError::EmptyRule { index });
            }
            compiled.push(CompiledRule {
                address: compile_glob(index, "address", &rule.address, GlobShape::Path)?,
                host: compile_glob(index, "host", &rule.host, GlobShape::Plain)?,
                locator: compile_glob(index, "locator", &rule.locator, GlobShape::Path)?,
                source_type: compile_glob(
                    index,
                    "source_type",
                    &rule.source_type,
                    GlobShape::Plain,
                )?,
                content_type: compile_glob(
                    index,
                    "content_type",
                    &rule.content_type,
                    GlobShape::Plain,
                )?,
                hint: compile_glob(index, "hint", &rule.hint, GlobShape::Plain)?,
                property: compile_glob(index, "property", &rule.property, GlobShape::Plain)?,
            });
        }
        assert_eq!(
            compiled.len(),
            rules.len(),
            "one compiled rule per written rule"
        );
        Ok(Self { rules: compiled })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether any rule ignores this source.
    pub fn matches(&self, address: &Address, envelope: &Envelope) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.matches(address, envelope))
    }
}

fn compile_glob(
    index: u32,
    field: &'static str,
    glob: &Option<String>,
    shape: GlobShape,
) -> Result<Option<GlobMatcher>, IgnoreError> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let literal_separator = match shape {
        GlobShape::Path => true,
        GlobShape::Plain => false,
    };
    GlobBuilder::new(glob)
        .literal_separator(literal_separator)
        .build()
        .map(|g| Some(g.compile_matcher()))
        .map_err(|e| IgnoreError::InvalidGlob {
            index,
            field,
            glob: glob.clone(),
            reason: e.kind().to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Property, Timestamp, TrustLevel};
    use inseam_kernel::fragment::Mimetype;

    fn file(address: &str, mimetype: &str, hint: &str) -> (Address, Envelope) {
        let address: Address = address.parse().expect("test address parses");
        let envelope = Envelope {
            source_type: "file".into(),
            content_type: Mimetype::parse(mimetype).expect("valid mimetype"),
            length: ContentLength::Bytes(10),
            created: None,
            modified: None,
            observed: Timestamp(0),
            properties: Vec::new(),
            hint: Some(hint.into()),
        };
        (address, envelope)
    }

    fn email(address: &str, from: &str, subject: &str) -> (Address, Envelope) {
        let (address, mut envelope) = file(address, "message/rfc822", subject);
        envelope.source_type = "email".into();
        envelope.properties.push(Property {
            key: "email".into(),
            value: from.into(),
            trust: TrustLevel::Verified,
        });
        (address, envelope)
    }

    fn set(toml: &str) -> IgnoreSet {
        #[derive(Deserialize)]
        struct Wrapper {
            ignore: Vec<IgnoreRule>,
        }
        let wrapper: Wrapper = toml::from_str(toml).expect("test rules parse");
        IgnoreSet::compile(&wrapper.ignore).expect("test rules compile")
    }

    #[test]
    fn locator_globs_are_path_wise() {
        let rules = set(r#"
            [[ignore]]
            locator = "Users/*/Library/**"
        "#);
        let (a, e) = file(
            "inseam://fs-mba/Users/greg/Library/Caches/x.db",
            "application/octet-stream",
            "x.db",
        );
        assert!(rules.matches(&a, &e));
        let (a, e) = file(
            "inseam://fs-mba/Users/greg/Data/Library/x.md",
            "text/markdown",
            "x.md",
        );
        assert!(!rules.matches(&a, &e), "`*` must not cross `/`");
    }

    #[test]
    fn double_star_spans_segments() {
        let rules = set(r#"
            [[ignore]]
            locator = "**/node_modules/**"
        "#);
        let (a, e) = file(
            "inseam://fs-mba/w/app/node_modules/left-pad/index.js",
            "text/javascript",
            "index.js",
        );
        assert!(rules.matches(&a, &e));
        let (a, e) = file(
            "inseam://fs-mba/w/app/src/index.js",
            "text/javascript",
            "index.js",
        );
        assert!(!rules.matches(&a, &e));
    }

    #[test]
    fn address_glob_covers_host_and_locator_together() {
        let rules = set(r#"
            [[ignore]]
            address = "inseam://fs-*/tmp/**"
        "#);
        let (a, e) = file(
            "inseam://fs-mba/tmp/scratch.txt",
            "text/plain",
            "scratch.txt",
        );
        assert!(rules.matches(&a, &e));
        let (a, e) = file(
            "inseam://gmail-greg/tmp/scratch.txt",
            "text/plain",
            "scratch.txt",
        );
        assert!(!rules.matches(&a, &e));
    }

    #[test]
    fn fields_within_a_rule_are_anded() {
        let rules = set(r#"
            [[ignore]]
            host = "gmail-*"
            property = "email:*@newsletters.example"
        "#);
        let (a, e) = email(
            "inseam://gmail-greg/msg-1",
            "weekly@newsletters.example",
            "This week",
        );
        assert!(rules.matches(&a, &e));
        let (a, e) = email("inseam://gmail-greg/msg-2", "friend@example.com", "Hi");
        assert!(!rules.matches(&a, &e), "property does not match");
        let (a, e) = email(
            "inseam://imap-work/msg-3",
            "weekly@newsletters.example",
            "This week",
        );
        assert!(!rules.matches(&a, &e), "host does not match");
    }

    #[test]
    fn rules_within_a_set_are_ored() {
        let rules = set(r#"
            [[ignore]]
            content_type = "image/*"
            [[ignore]]
            hint = "*.log"
        "#);
        let (a, e) = file("inseam://fs-mba/p/photo.jpeg", "image/jpeg", "photo.jpeg");
        assert!(rules.matches(&a, &e));
        let (a, e) = file("inseam://fs-mba/p/server.log", "text/plain", "server.log");
        assert!(rules.matches(&a, &e));
        let (a, e) = file("inseam://fs-mba/p/notes.md", "text/markdown", "notes.md");
        assert!(!rules.matches(&a, &e));
    }

    #[test]
    fn source_type_glob_matches_plainly() {
        let rules = set(r#"
            [[ignore]]
            source_type = "email"
        "#);
        let (a, e) = email("inseam://gmail-greg/msg-1", "a@b.c", "x");
        assert!(rules.matches(&a, &e));
        let (a, e) = file("inseam://fs-mba/p/notes.md", "text/markdown", "notes.md");
        assert!(!rules.matches(&a, &e));
    }

    #[test]
    fn hint_rule_never_matches_a_source_without_a_hint() {
        let rules = set(r#"
            [[ignore]]
            hint = "*"
        "#);
        let (a, mut e) = file("inseam://fs-mba/p/notes.md", "text/markdown", "notes.md");
        assert!(rules.matches(&a, &e));
        e.hint = None;
        assert!(!rules.matches(&a, &e));
    }

    #[test]
    fn empty_set_ignores_nothing() {
        let rules = IgnoreSet::compile(&[]).expect("compiles");
        assert!(rules.is_empty());
        let (a, e) = file("inseam://fs-mba/p/notes.md", "text/markdown", "notes.md");
        assert!(!rules.matches(&a, &e));
    }

    #[test]
    fn rejects_a_rule_with_no_field() {
        let err = IgnoreSet::compile(&[IgnoreRule::default()]).expect_err("refused");
        assert_eq!(err, IgnoreError::EmptyRule { index: 0 });
    }

    #[test]
    fn rejects_a_malformed_glob_naming_rule_and_field() {
        let rule = IgnoreRule {
            locator: Some("docs/[".into()),
            ..IgnoreRule::default()
        };
        let err = IgnoreSet::compile(&[
            IgnoreRule {
                host: Some("*".into()),
                ..IgnoreRule::default()
            },
            rule,
        ])
        .expect_err("refused");
        assert!(matches!(
            err,
            IgnoreError::InvalidGlob {
                index: 1,
                field: "locator",
                ..
            }
        ));
    }

    #[test]
    fn rejects_more_rules_than_the_limit() {
        let rule = IgnoreRule {
            host: Some("x".into()),
            ..IgnoreRule::default()
        };
        let rules = vec![rule; usize::try_from(IGNORE_RULES_MAX + 1).expect("fits")];
        assert!(matches!(
            IgnoreSet::compile(&rules),
            Err(IgnoreError::TooManyRules { .. })
        ));
    }

    #[test]
    fn unknown_fields_are_a_parse_error() {
        let parsed: Result<IgnoreRule, _> = toml::from_str("path = \"x\"");
        assert!(parsed.is_err());
    }
}
