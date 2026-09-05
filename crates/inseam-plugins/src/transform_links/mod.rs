//! The `transform-links` plugin: the link follower (`design/indexing.md`).
//! It claims `text/uri-list` fragments — the links the markdown transform
//! hangs off sections, or a file of URLs at the root — and turns each link
//! to content it follows into a typed fragment carrying a **content
//! reference** to the web address: an `image/png` fragment that
//! `resolves-to` from the link, with no bytes of its own. A byte-wanting
//! transform claiming that type (OCR) then receives the image through the
//! planner, and a client fetches it by the same address.
//!
//! The type comes from the URL's extension first — offline, deterministic,
//! free — and, when the web host is mounted and `probe` is on, from the
//! resource's own headers, which win when they disagree. A link whose type
//! nothing can tell, or that the follow list excludes, emits nothing: the
//! `text/uri-list` fragment itself stays in the graph.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use url::Url;

use inseam_kernel::address::{Address, ContentLength};
use inseam_kernel::fragment::{Extent, Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::connection::{Connections, CONNECTIONS};
use inseam_seams::llm::LlmLane;
use inseam_seams::transforms::{
    register_as_effect, Registration, Transform, TransformCtx, TransformKind, TransformOutput,
};

use crate::connection_web::{web_address, web_host_id};

/// Resolved links remembered per transform instance, so a URL fifty
/// documents link to is probed once per process; cleared when full.
const RESOLVED_CACHE_ENTRIES_MAX: usize = 4096;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinksConfig {
    /// Content types a link may resolve to, as essences or `type/*`
    /// patterns. Links to anything else are left as links.
    pub follow: Vec<String>,
    /// Ask the web host for the resource's headers when it is mounted, so
    /// the type comes from the server rather than the extension alone and
    /// extension-less URLs resolve at all. Off, the transform never touches
    /// the network.
    pub probe: bool,
}

impl Default for LinksConfig {
    fn default() -> Self {
        Self {
            follow: vec!["image/*".to_string()],
            probe: true,
        }
    }
}

/// The links transform's own relation kind: a link `resolves-to` the
/// content it points at.
pub fn resolves_to() -> RelationKind {
    RelationKind::new("resolves-to").expect("literal relation kind is valid")
}

pub struct LinksPlugin {
    config: LinksConfig,
}

pub struct LinksFactory;

impl PluginFactory for LinksFactory {
    fn name(&self) -> &str {
        "transform-links"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(LinksPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for LinksPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms"), Inject::required("connections")];
        Manifest {
            name: "transform-links",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let follow = self
            .config
            .follow
            .iter()
            .map(|p| MimetypePattern::parse(p))
            .collect::<Result<Vec<_>, _>>()
            .map_err(PluginError)?;
        let mut follow_names: Vec<&str> = self.config.follow.iter().map(String::as_str).collect();
        follow_names.sort_unstable();
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "links".to_string(),
                transform: Arc::new(LinksTransform {
                    follow,
                    probe: self.config.probe,
                    connections: cx.get(&CONNECTIONS)?,
                    resolved: Mutex::new(HashMap::new()),
                }),
                llm_call_budget: 0,
                llm_lane: LlmLane::Interactive,
                shape_fingerprint: format!(
                    "links-v1|follow={}|probe={}",
                    follow_names.join(","),
                    self.config.probe
                ),
            },
        )
    }
}

/// A content-type essence or a `type/*` family.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MimetypePattern {
    Essence(String),
    Family(String),
}

impl MimetypePattern {
    fn parse(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim().to_ascii_lowercase();
        match pattern.split_once('/') {
            Some((family, "*")) if !family.is_empty() && !family.contains('*') => {
                Ok(Self::Family(family.to_string()))
            }
            Some((family, subtype)) if !family.is_empty() && !subtype.is_empty() && !subtype.contains('*') => {
                Ok(Self::Essence(pattern))
            }
            _ => Err(format!("`{pattern}` is not a `type/subtype` or `type/*` follow pattern")),
        }
    }

    fn matches(&self, mimetype: &Mimetype) -> bool {
        match self {
            Self::Essence(essence) => mimetype.essence() == essence,
            Self::Family(family) => mimetype
                .essence()
                .split_once('/')
                .is_some_and(|(f, _)| f == family),
        }
    }
}

/// What a link resolved to: the typed reference to emit, or nothing.
#[derive(Debug, Clone)]
struct Resolved {
    mimetype: Mimetype,
    address: Address,
    length: Option<u64>,
}

pub(crate) struct LinksTransform {
    follow: Vec<MimetypePattern>,
    probe: bool,
    connections: Arc<dyn Connections>,
    resolved: Mutex<HashMap<String, Option<Resolved>>>,
}

#[async_trait::async_trait]
impl Transform for LinksTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }

    /// Links wherever they appear: hung off a section by the markdown
    /// transform, or a `.uri` file of them at the root.
    fn claims(&self, mimetype: &Mimetype, _is_root: bool) -> bool {
        mimetype.essence() == "text/uri-list"
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let Some(url) = ctx.text.and_then(first_link) else {
            return TransformOutput::default();
        };
        let Some(resolved) = self.resolve(&url).await else {
            return TransformOutput::default();
        };
        TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                mimetype: resolved.mimetype,
                text: None,
                extent: resolved.length.map(|n| Extent::Bytes { start: 0, end: n }),
                content_address: Some(resolved.address),
            },
            resolves_to(),
        )])
    }
}

impl LinksTransform {
    /// The typed reference for a URL, remembered per URL: the extension's
    /// verdict, corrected by the resource's headers when the web host is
    /// mounted and probing is on, then filtered by the follow list.
    async fn resolve(&self, url: &Url) -> Option<Resolved> {
        let key = url.as_str().to_string();
        if let Some(remembered) = self.remembered(&key) {
            return remembered;
        }
        let address = web_address(url).ok()?;
        let guessed = mimetype_by_extension(url);
        let probed = if self.probe {
            self.probe(&address).await
        } else {
            None
        };
        let resolved = match probed.or(guessed.map(|m| (m, None))) {
            Some((mimetype, length)) if self.follow.iter().any(|p| p.matches(&mimetype)) => {
                Some(Resolved { mimetype, address, length })
            }
            _ => None,
        };
        self.remember(key, resolved.clone());
        resolved
    }

    /// The server's own type and length, through the mounted web host.
    /// Any failure — no host mounted, refused by the guard, unreachable —
    /// leaves the extension's verdict in place, logged at debug: a link is
    /// still a link when the site is down.
    async fn probe(&self, address: &Address) -> Option<(Mimetype, Option<u64>)> {
        let host = self.connections.resolve(&web_host_id())?;
        match host.connection.describe(address).await {
            Ok(envelope) => {
                let length = match envelope.length {
                    ContentLength::Bytes(n) if n > 0 => Some(n),
                    ContentLength::Bytes(_) | ContentLength::Lines(_) => None,
                };
                Some((essence_of(&envelope.content_type), length))
            }
            Err(error) => {
                tracing::debug!(%address, %error, "link probe failed; typing by extension");
                None
            }
        }
    }

    fn remembered(&self, key: &str) -> Option<Option<Resolved>> {
        self.resolved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }

    fn remember(&self, key: String, resolved: Option<Resolved>) {
        let mut cache = self
            .resolved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= RESOLVED_CACHE_ENTRIES_MAX {
            cache.clear();
        }
        cache.insert(key, resolved);
        assert!(cache.len() <= RESOLVED_CACHE_ENTRIES_MAX);
    }
}

/// The first URL of a `text/uri-list` (RFC 2483): one URI per line,
/// `#` lines are comments. The markdown transform emits one link per
/// fragment; a file of links contributes its first.
fn first_link(text: &str) -> Option<Url> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .and_then(|line| Url::parse(line).ok())
        .filter(|url| url.scheme() == "http" || url.scheme() == "https")
}

/// The type a URL's path extension suggests, as an essence.
fn mimetype_by_extension(url: &Url) -> Option<Mimetype> {
    mime_guess::from_path(url.path())
        .first_raw()
        .and_then(|raw| Mimetype::parse(raw).ok())
}

/// A mimetype stripped to its essence: what claims dispatch on.
fn essence_of(mimetype: &Mimetype) -> Mimetype {
    Mimetype::parse(mimetype.essence()).expect("an essence parses as a mimetype")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_link_skips_comments_blank_lines_and_other_schemes() {
        let text = "# a comment\n\n  https://example.com/a.png  \nhttps://example.com/b.png\n";
        assert_eq!(
            first_link(text).expect("link").as_str(),
            "https://example.com/a.png"
        );
        assert!(first_link("mailto:greg@example.com").is_none());
        assert!(first_link("ftp://example.com/a.png").is_none());
        assert!(first_link("not a url").is_none());
        assert!(first_link("").is_none());
    }

    #[test]
    fn extensions_type_urls_and_queries_do_not_confuse_them() {
        let url = Url::parse("https://example.com/dir/photo.JPG?size=large").expect("url");
        assert_eq!(mimetype_by_extension(&url).expect("typed").essence(), "image/jpeg");
        let bare = Url::parse("https://example.com/image?id=3").expect("url");
        assert!(mimetype_by_extension(&bare).is_none());
    }

    #[test]
    fn follow_patterns_match_families_and_essences() {
        let family = MimetypePattern::parse("image/*").expect("parses");
        assert!(family.matches(&Mimetype::parse("image/png").expect("valid")));
        assert!(!family.matches(&Mimetype::parse("text/html").expect("valid")));
        let essence = MimetypePattern::parse("application/pdf").expect("parses");
        assert!(essence.matches(&Mimetype::parse("application/pdf").expect("valid")));
        assert!(!essence.matches(&Mimetype::parse("application/json").expect("valid")));
        for bad in ["image", "*/*", "image/p*", "/png", ""] {
            assert!(MimetypePattern::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
