//! Fragments: the unit of the semantic graph (`design/indexing.md`). A
//! fragment is a piece of understanding derived from a source — a markdown
//! section, a chunk, a summary, an entity — carrying a mimetype, an extent,
//! and typed relations to other fragments. Fragments are index-local: they
//! have no addresses and never sync.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FragmentError {
    #[error("`{0}` is not a type/subtype mimetype")]
    BadMimetype(String),
    #[error("`{0}` is not a relation kind")]
    BadRelationKind(String),
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

    /// inseam-defined type for the mandatory summary fragment.
    pub fn summary() -> Self {
        Self::parse("text/x-inseam-summary").expect("literal mimetype is valid")
    }

    /// inseam-defined type for deduplicated entity fragments.
    pub fn entity() -> Self {
        Self::parse("text/x-inseam-entity").expect("literal mimetype is valid")
    }

    pub fn is_summary(&self) -> bool {
        self.essence == "text/x-inseam-summary"
    }

    pub fn is_entity(&self) -> bool {
        self.essence == "text/x-inseam-entity"
    }

    /// inseam-defined types are derived understanding, not source content;
    /// structural transforms must never decompose them.
    pub fn is_inseam_defined(&self) -> bool {
        self.essence.starts_with("text/x-inseam-")
    }

    /// Whether content of this type is worth reading and indexing as text:
    /// all of `text/*` plus the structured-text application types.
    pub fn is_indexable_text(&self) -> bool {
        if self.is_text() {
            return true;
        }
        matches!(
            self.essence(),
            "application/json"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/xml"
                | "application/javascript"
                | "application/x-sh"
                | "application/sql"
                | "image/svg+xml"
        )
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

/// Typed edge kinds between fragments. Relation kinds are first-class: the
/// Finder conducts relevance along them with per-kind weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelationKind {
    /// Structural decomposition: parent contains child.
    Contains,
    /// A link found inside a fragment, pointing at a URL fragment.
    LinksTo,
    /// Derived understanding (a summary) pointing back at what it derives from.
    DerivedFrom,
    /// A fragment references an entity.
    Mentions,
    /// A transcript fragment transcribing its media parent.
    Transcribes,
}

impl RelationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::LinksTo => "links-to",
            Self::DerivedFrom => "derived-from",
            Self::Mentions => "mentions",
            Self::Transcribes => "transcribes",
        }
    }

    /// Build the stored relation for a parent -> child production, keeping the
    /// semantic direction of each kind: `contains`/`links-to`/`mentions` read
    /// parent -> child, while `derived-from`/`transcribes` read child -> parent
    /// (the summary derives from its parent, the transcript transcribes it).
    pub fn edge(self, parent: FragmentId, child: FragmentId) -> Relation {
        match self {
            Self::Contains | Self::LinksTo | Self::Mentions => Relation {
                from: parent,
                kind: self,
                to: child,
            },
            Self::DerivedFrom | Self::Transcribes => Relation {
                from: child,
                kind: self,
                to: parent,
            },
        }
    }
}

impl fmt::Display for RelationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RelationKind {
    type Err = FragmentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "contains" => Ok(Self::Contains),
            "links-to" => Ok(Self::LinksTo),
            "derived-from" => Ok(Self::DerivedFrom),
            "mentions" => Ok(Self::Mentions),
            "transcribes" => Ok(Self::Transcribes),
            other => Err(FragmentError::BadRelationKind(other.to_string())),
        }
    }
}

/// A typed edge between two stored fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Relation {
    pub from: FragmentId,
    pub kind: RelationKind,
    pub to: FragmentId,
}

/// A fragment that has not been stored yet — what transforms emit.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFragment {
    pub mimetype: Mimetype,
    pub text: Option<String>,
    pub extent: Option<Extent>,
}

/// A new fragment plus how it hangs off its parent and its own descendants.
/// Transforms return trees of these; the indexer persists them.
#[derive(Debug, Clone, PartialEq)]
pub struct Sprout {
    pub fragment: NewFragment,
    /// Kind of the edge between the parent fragment and this one.
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
        assert!(m.is_entity());
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
        for kind in [
            RelationKind::Contains,
            RelationKind::LinksTo,
            RelationKind::DerivedFrom,
            RelationKind::Mentions,
            RelationKind::Transcribes,
        ] {
            assert_eq!(kind.as_str().parse::<RelationKind>(), Ok(kind));
        }
    }

    #[test]
    fn derived_from_edge_points_child_to_parent() {
        let parent = FragmentId(1);
        let child = FragmentId(2);
        let r = RelationKind::DerivedFrom.edge(parent, child);
        assert_eq!((r.from, r.to), (child, parent));
    }

    #[test]
    fn contains_edge_points_parent_to_child() {
        let r = RelationKind::Contains.edge(FragmentId(1), FragmentId(2));
        assert_eq!((r.from, r.to), (FragmentId(1), FragmentId(2)));
    }

    #[test]
    fn sprout_count_includes_descendants() {
        let leaf = |t: &str| {
            Sprout::leaf(
                NewFragment {
                    mimetype: Mimetype::text_plain(),
                    text: Some(t.to_string()),
                    extent: None,
                },
                RelationKind::Contains,
            )
        };
        let mut root = leaf("root");
        root.children.push(leaf("a"));
        root.children.push(leaf("b"));
        root.children[0].children.push(leaf("a1"));
        assert_eq!(root.count(), 4);
    }
}
