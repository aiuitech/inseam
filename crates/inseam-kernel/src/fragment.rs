//! Fragments: the unit of the semantic graph (`design/indexing.md`). A
//! fragment is a piece of understanding derived from a source — a markdown
//! section, a chunk, a summary, an entity — carrying a mimetype, an extent,
//! and typed relations to other fragments. Fragments are index-local: they
//! have no addresses and never sync.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::address::Address;

/// Longest relation kind name accepted; far above any sensible vocabulary,
/// present so the store's `kind` column has a known bound.
pub const RELATION_KIND_LEN_MAX: u32 = 64;
/// Longest key a keyed fragment may carry.
pub const FRAGMENT_KEY_LEN_MAX: u32 = 256;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FragmentError {
    #[error("`{0}` is not a type/subtype mimetype")]
    BadMimetype(String),
    #[error(
        "`{0}` is not a relation kind: lowercase ascii letters, digits and `-`, \
         starting with a letter, at most {RELATION_KIND_LEN_MAX} long"
    )]
    BadRelationKind(String),
    #[error("`{0}` is not a fragment key: non-empty, no control characters, at most {FRAGMENT_KEY_LEN_MAX} long")]
    BadFragmentKey(String),
}

/// Identifier of a stored fragment, local to one node's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FragmentId(pub i64);

impl fmt::Display for FragmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A mimetype with optional parameters, e.g. `text/x-inseam-entity;kind=person`.
/// The essence drives transform dispatch; parameters carry small refinements.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Mimetype {
    essence: String,
    params: Vec<(String, String)>,
}

impl Mimetype {
    pub fn parse(s: &str) -> Result<Self, FragmentError> {
        let mut parts = s.split(';');
        let essence = parts
            .next()
            .map(str::trim)
            .filter(|e| {
                e.split_once('/')
                    .is_some_and(|(t, s)| !t.is_empty() && !s.is_empty() && !s.contains('/'))
            })
            .ok_or_else(|| FragmentError::BadMimetype(s.to_string()))?
            .to_ascii_lowercase();
        let mut params = Vec::new();
        for p in parts {
            let (k, v) = p
                .split_once('=')
                .ok_or_else(|| FragmentError::BadMimetype(s.to_string()))?;
            params.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
        Ok(Self { essence, params })
    }

    pub fn essence(&self) -> &str {
        &self.essence
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn with_param(mut self, key: &str, value: &str) -> Self {
        self.params.push((key.to_string(), value.to_string()));
        self
    }

    pub fn is_text(&self) -> bool {
        self.essence.starts_with("text/")
    }

    pub fn markdown() -> Self {
        Self::parse("text/markdown").expect("literal mimetype is valid")
    }

    pub fn text_plain() -> Self {
        Self::parse("text/plain").expect("literal mimetype is valid")
    }

    pub fn uri_list() -> Self {
        Self::parse("text/uri-list").expect("literal mimetype is valid")
    }

    /// A folder source's type: a container whose content is what it holds,
    /// not bytes of its own (`design/indexing.md`, folders). The
    /// freedesktop spelling, so a filesystem and a Drive folder share it.
    pub fn directory() -> Self {
        Self::parse("inode/directory").expect("literal mimetype is valid")
    }

    pub fn is_directory(&self) -> bool {
        self.essence == "inode/directory"
    }

    /// The kernel-defined type for one entry of a folder's listing: the
    /// fragment that names a child source and references its address, so
    /// `expand` on a folder walks to what it holds. Inseam-defined, so no
    /// transform ever re-decomposes a child through its entry.
    pub fn directory_entry() -> Self {
        Self::parse("text/x-inseam-entry").expect("literal mimetype is valid")
    }

    pub fn is_directory_entry(&self) -> bool {
        self.essence == "text/x-inseam-entry"
    }

    /// The kernel-defined type for the mandatory summary fragment — the one
    /// derived type the store itself reads (`summary_of`).
    pub fn summary() -> Self {
        Self::parse("text/x-inseam-summary").expect("literal mimetype is valid")
    }

    pub fn is_summary(&self) -> bool {
        self.essence == "text/x-inseam-summary"
    }

    /// The kernel-defined type for the summarizer's keywords fragment: the
    /// terms that name a source, kept for the full-text side of the index
    /// beside the summary's prose (`design/indexing.md`).
    pub fn keywords() -> Self {
        Self::parse("text/x-inseam-keywords").expect("literal mimetype is valid")
    }

    pub fn is_keywords(&self) -> bool {
        self.essence == "text/x-inseam-keywords"
    }

    /// The hints plugin's per-source fragments (`text/x-inseam-hint`): a
    /// synopsis, the questions the source answers, the facts that tell it
    /// apart. Prose written to be found, so the store ranks them with
    /// summaries rather than with names and terms.
    pub fn is_hint(&self) -> bool {
        self.essence == "text/x-inseam-hint"
    }

    /// `text/x-inseam-*` types are derived understanding, not source
    /// content: the sweep never re-decomposes them and loaded transforms may
    /// not emit them. Plugins mint their own under the prefix (the entity
    /// extractor's `text/x-inseam-entity`) to opt into exactly that
    /// treatment.
    pub fn is_inseam_defined(&self) -> bool {
        self.essence.starts_with("text/x-inseam-")
    }
}

impl fmt::Display for Mimetype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.essence)?;
        for (k, v) in &self.params {
            write!(f, ";{k}={v}")?;
        }
        Ok(())
    }
}

impl FromStr for Mimetype {
    type Err = FragmentError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Mimetype {
    type Error = FragmentError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Mimetype> for String {
    fn from(m: Mimetype) -> String {
        m.to_string()
    }
}

/// Where a fragment sits within its parent and how long it is: lines for text,
/// bytes or milliseconds otherwise. Line ranges are 1-based and inclusive —
/// exactly what `scan` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "unit", rename_all = "lowercase")]
pub enum Extent {
    Lines { start: u64, end: u64 },
    Bytes { start: u64, end: u64 },
    Millis { start: u64, end: u64 },
}

impl Extent {
    pub fn lines(start: u64, end: u64) -> Self {
        Self::Lines { start, end }
    }
}

impl fmt::Display for Extent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lines { start, end } => write!(f, "lines {start}-{end}"),
            Self::Bytes { start, end } => write!(f, "bytes {start}-{end}"),
            Self::Millis { start, end } => write!(f, "millis {start}-{end}"),
        }
    }
}

/// The kind of an edge between fragments — an open vocabulary, because
/// plugins extend the graph by vocabulary, not schema (`design/kernel.md`).
/// The kernel defines exactly two kinds, the **transform relation**: a
/// transform's input [`contains`](Self::contains) a structural child it
/// emitted, or [`derives`](Self::derives) an enrichment of itself. Every
/// other kind (`links-to`, `mentions`, `transcribes`, …) is minted by the
/// plugin that emits it; the finder weights kinds by name.
///
/// Every relation is stored and read **input → output**: `from` is the
/// fragment a transform was applied to (or anchored at), `to` is what it
/// produced. A kind's name should read in that direction ("root contains
/// section", "note mentions person").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelationKind(String);

impl RelationKind {
    /// Structural decomposition: the input contains this child.
    pub fn contains() -> Self {
        Self("contains".to_string())
    }

    /// Enrichment: the input derives this understanding (a summary).
    pub fn derives() -> Self {
        Self("derives".to_string())
    }

    /// Parse a kind name: lowercase ascii letters, digits and `-`, starting
    /// with a letter, at most [`RELATION_KIND_LEN_MAX`] long.
    pub fn new(name: impl Into<String>) -> Result<Self, FragmentError> {
        let name = name.into();
        let len_ok = !name.is_empty() && name.len() <= RELATION_KIND_LEN_MAX as usize;
        let starts_ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase());
        let chars_ok = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if len_ok && starts_ok && chars_ok {
            Ok(Self(name))
        } else {
            Err(FragmentError::BadRelationKind(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RelationKind {
    type Err = FragmentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for RelationKind {
    type Error = FragmentError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<RelationKind> for String {
    fn from(k: RelationKind) -> String {
        k.0
    }
}

/// The index-wide identity of a keyed fragment: a fragment that belongs to
/// no single source and is deduplicated across the whole index under this
/// key (an extracted entity, for instance). Keys are plugin-namespaced by
/// convention (`entity:person:greg`) so two plugins' vocabularies never
/// collide; the kernel only bounds and stores them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FragmentKey(String);

impl FragmentKey {
    pub fn new(key: impl Into<String>) -> Result<Self, FragmentError> {
        let key = key.into();
        let len_ok = !key.is_empty() && key.len() <= FRAGMENT_KEY_LEN_MAX as usize;
        let chars_ok = !key.chars().any(char::is_control);
        if len_ok && chars_ok {
            Ok(Self(key))
        } else {
            Err(FragmentError::BadFragmentKey(key))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FragmentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for FragmentKey {
    type Error = FragmentError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<FragmentKey> for String {
    fn from(k: FragmentKey) -> String {
        k.0
    }
}

/// A typed edge between two stored fragments, read input → output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Relation {
    pub from: FragmentId,
    pub kind: RelationKind,
    pub to: FragmentId,
}

impl Relation {
    /// The edge a parent (input) fragment has to a child (output) fragment.
    pub fn new(from: FragmentId, kind: RelationKind, to: FragmentId) -> Self {
        Self { from, kind, to }
    }
}

/// A fragment that has not been stored yet — what transforms emit. It
/// serializes so a transform's whole output can be cached by input digest
/// (`design/indexing.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewFragment {
    pub mimetype: Mimetype,
    pub text: Option<String>,
    pub extent: Option<Extent>,
    /// Where this fragment's bytes live, when its content is not text the
    /// index holds: the address of an image a document links to, say. The
    /// index stores the reference, never the bytes — source data never
    /// moves (`design/addressing.md`) — and reads them through the host's
    /// connection when a byte-wanting transform claims the fragment or a
    /// client fetches it. `None` for text fragments and for the root, whose
    /// content is the source itself.
    pub content_address: Option<Address>,
}

/// A new fragment plus how it hangs off its parent and its own descendants.
/// Transforms return trees of these; the indexer persists them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sprout {
    pub fragment: NewFragment,
    /// Kind of the edge from the parent (input) fragment to this one.
    pub relation: RelationKind,
    pub children: Vec<Sprout>,
}

impl Sprout {
    pub fn leaf(fragment: NewFragment, relation: RelationKind) -> Self {
        Self {
            fragment,
            relation,
            children: Vec::new(),
        }
    }

    /// Number of fragments in this subtree, itself included.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Sprout::count).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mimetype_parses_essence_and_params() {
        let m = Mimetype::parse("Text/X-Inseam-Entity; kind=person").expect("parses");
        assert_eq!(m.essence(), "text/x-inseam-entity");
        assert_eq!(m.param("kind"), Some("person"));
        assert!(m.is_inseam_defined());
        assert_eq!(m.to_string(), "text/x-inseam-entity;kind=person");
    }

    #[test]
    fn mimetype_rejects_shapeless_input() {
        assert!(Mimetype::parse("not-a-mimetype").is_err());
        assert!(Mimetype::parse("a/b/c").is_err());
        assert!(Mimetype::parse("/x").is_err());
        assert!(Mimetype::parse("text/plain;charset").is_err());
    }

    #[test]
    fn relation_kinds_roundtrip_their_names() {
        for name in ["contains", "derives", "links-to", "mentions", "transcribes", "x9-y"] {
            let kind = name.parse::<RelationKind>().expect("valid kind");
            assert_eq!(kind.as_str(), name);
            assert_eq!(serde_json::to_string(&kind).expect("serializes"), format!("{name:?}"));
        }
        assert_eq!(RelationKind::contains().as_str(), "contains");
        assert_eq!(RelationKind::derives().as_str(), "derives");
    }

    #[test]
    fn relation_kinds_reject_shapeless_names() {
        for bad in ["", "Contains", "derived_from", "9lives", "a b", &"x".repeat(65)] {
            assert!(
                matches!(RelationKind::new(bad), Err(FragmentError::BadRelationKind(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn fragment_keys_are_bounded_and_printable() {
        assert!(FragmentKey::new("entity:person:greg hunt").is_ok());
        assert!(matches!(FragmentKey::new(""), Err(FragmentError::BadFragmentKey(_))));
        assert!(matches!(FragmentKey::new("a\nb"), Err(FragmentError::BadFragmentKey(_))));
        assert!(matches!(FragmentKey::new("k".repeat(257)), Err(FragmentError::BadFragmentKey(_))));
    }

    #[test]
    fn sprout_count_includes_descendants() {
        let leaf = |t: &str| {
            Sprout::leaf(
                NewFragment {
                    mimetype: Mimetype::text_plain(),
                    text: Some(t.to_string()),
                    extent: None,
                    content_address: None,
                },
                RelationKind::contains(),
            )
        };
        let mut root = leaf("root");
        root.children.push(leaf("a"));
        root.children.push(leaf("b"));
        root.children[0].children.push(leaf("a1"));
        assert_eq!(root.count(), 4);
    }
}
