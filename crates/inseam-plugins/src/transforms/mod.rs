//! The `transforms` seam provider (the registry) and the first-party
//! transform plugins: markdown, chunker, summarizer, entity extractor. Each
//! transform is its own plugin — registering into the seam exactly the way a
//! community WASM transform does, which is the dog-food that proves the
//! contract (`design/plugins.md`).

pub mod chunk;
pub mod entities;
pub mod markdown;
pub mod summarize;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError,
};
use inseam_seams::transforms::{
    Registration, Transform, TransformCtx, TransformKind, TransformOutput, Transforms,
    TRANSFORMS,
};

use summarize::SummaryKind;

// ---------------------------------------------------------------------------
// The registry provider
// ---------------------------------------------------------------------------

pub struct TransformsRegistry;

pub struct TransformsRegistryFactory;

impl inseam_kernel::substrate::PluginFactory for TransformsRegistryFactory {
    fn name(&self) -> &str {
        "transforms"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(TransformsRegistry))
    }
}

#[async_trait::async_trait]
impl Plugin for TransformsRegistry {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "transforms",
            inject: INJECT,
            provides: &["transforms"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        cx.provide(
            &TRANSFORMS,
            Arc::new(Registry::default()) as Arc<dyn Transforms>,
            Facts::new(),
        )?;
        Ok(())
    }
}

#[derive(Default)]
struct Registry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    entries: RwLock<Vec<(u64, Arc<Registration>)>>,
    next: AtomicU64,
}

impl Transforms for Registry {
    fn register(&self, registration: Registration) -> Box<dyn FnOnce() + Send> {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        self.inner
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, Arc::new(registration)));
        // The disposer holds the registry weakly: a transform being unwound
        // after the whole registry is gone (full teardown, reverse order)
        // must be a no-op, not a resurrection.
        let weak = Arc::downgrade(&self.inner);
        Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .entries
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(i, _)| *i != id);
            }
        })
    }

    fn snapshot(&self) -> Vec<Arc<Registration>> {
        let mut out: Vec<Arc<Registration>> = self
            .inner
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, r)| Arc::clone(r))
            .collect();
        out.sort_by(|a, b| {
            (a.transform.kind(), a.entry_id.as_str())
                .cmp(&(b.transform.kind(), b.entry_id.as_str()))
        });
        out
    }
}

// ---------------------------------------------------------------------------
// Transform plugin scaffolding
// ---------------------------------------------------------------------------

/// Register a transform into the seam as a fiber effect: unmounting the
/// plugin unwinds the registration, and the next sweep discovers the shape
/// divergence on its own — no lifecycle hooks into the index.
fn register(
    cx: &mut ApplyCx<'_>,
    name: &str,
    transform: Arc<dyn Transform>,
    llm_call_budget: usize,
    shape_fingerprint: String,
) -> Result<(), PluginError> {
    let registry = cx.get(&TRANSFORMS)?;
    let disposer = registry.register(Registration {
        entry_id: cx.entry_id().to_string(),
        name: name.to_string(),
        transform,
        llm_call_budget,
        shape_fingerprint,
    });
    cx.effect(format!("register transform {name}"), disposer);
    Ok(())
}

static TRANSFORM_INJECT: &[Inject] = &[Inject::required("transforms")];

macro_rules! transform_plugin {
    ($plugin:ident, $factory:ident, $name:literal) => {
        pub struct $factory;

        impl inseam_kernel::substrate::PluginFactory for $factory {
            fn name(&self) -> &str {
                $name
            }

            fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
                Ok(Box::new($plugin {
                    config: parse_config(config)?,
                }))
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Markdown (structural)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkdownConfig {}

pub struct MarkdownPlugin {
    #[allow(dead_code)]
    config: MarkdownConfig,
}

transform_plugin!(MarkdownPlugin, MarkdownFactory, "transform-markdown");

#[async_trait::async_trait]
impl Plugin for MarkdownPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "transform-markdown",
            inject: TRANSFORM_INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register(cx, "markdown", Arc::new(MarkdownTransform), 0, "markdown-v1".to_string())
    }
}

struct MarkdownTransform;

#[async_trait::async_trait]
impl Transform for MarkdownTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined() && mimetype.essence() == "text/markdown"
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        TransformOutput::sprouts(ctx.text.map(markdown::decompose).unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Chunker (structural fallback for structureless text)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkerConfig {
    /// Aim for chunks around this many characters.
    pub target_chars: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self { target_chars: 1_600 }
    }
}

pub struct ChunkerPlugin {
    config: ChunkerConfig,
}

transform_plugin!(ChunkerPlugin, ChunkerFactory, "transform-chunker");

#[async_trait::async_trait]
impl Plugin for ChunkerPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "transform-chunker",
            inject: TRANSFORM_INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register(
            cx,
            "chunker",
            Arc::new(ChunkerTransform {
                target_chars: self.config.target_chars,
            }),
            0,
            format!("chunker-v1|target={}", self.config.target_chars),
        )
    }
}

struct ChunkerTransform {
    target_chars: usize,
}

#[async_trait::async_trait]
impl Transform for ChunkerTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root
            && !mimetype.is_inseam_defined()
            && mimetype.essence() != "text/markdown"
            && mimetype.is_indexable_text()
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        TransformOutput::sprouts(
            ctx.text
                .map(|t| chunk::chunk_with_target(ctx.mimetype, t, self.target_chars))
                .unwrap_or_default(),
        )
    }
}

// ---------------------------------------------------------------------------
// Summarizer (enrichment; the one mandatory transform)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummarizerConfig {
    /// Target summary length in characters. Config sets the length, not the
    /// existence: every indexed source gets a summary.
    pub target_chars: usize,
    /// LLM summaries per index run (run-metering tier — not in the shape
    /// stamp); beyond it the summarizer falls back to extractive summaries
    /// so the mandatory-summary invariant still holds.
    pub llm_call_budget: usize,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            target_chars: 400,
            llm_call_budget: 500,
        }
    }
}

pub struct SummarizerPlugin {
    config: SummarizerConfig,
}

transform_plugin!(SummarizerPlugin, SummarizerFactory, "transform-summarizer");

#[async_trait::async_trait]
impl Plugin for SummarizerPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "transform-summarizer",
            inject: TRANSFORM_INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register(
            cx,
            "summarizer",
            Arc::new(SummarizerTransform {
                target_chars: self.config.target_chars,
            }),
            self.config.llm_call_budget,
            format!("summarizer-v1|target_chars={}", self.config.target_chars),
        )
    }
}

pub(crate) struct SummarizerTransform {
    pub target_chars: usize,
}

#[async_trait::async_trait]
impl Transform for SummarizerTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Enrichment
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined()
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let hint = ctx.envelope.hint.as_deref();
        let (text, kind) = match ctx.text.filter(|t| !t.trim().is_empty()) {
            Some(content) => {
                summarize::summarize_text(ctx.llm.as_deref(), hint, content, self.target_chars).await
            }
            None => (
                summarize::envelope_summary(ctx.envelope),
                SummaryKind::Envelope,
            ),
        };
        if text.is_empty() {
            return TransformOutput::default();
        }
        TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                // The `via` param records provenance: llm, extractive, or
                // envelope. The index report counts by it.
                mimetype: Mimetype::summary().with_param("via", kind.as_str()),
                text: Some(text),
                extent: None,
            },
            RelationKind::DerivedFrom,
        )])
    }
}

// ---------------------------------------------------------------------------
// Entity extractor (enrichment)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EntityExtractorConfig {
    /// Cap on entities taken from a single source (shape tier).
    pub max_per_source: usize,
    /// Extraction LLM calls per index run (run-metering tier).
    pub llm_call_budget: usize,
}

impl Default for EntityExtractorConfig {
    fn default() -> Self {
        Self {
            max_per_source: 12,
            llm_call_budget: 500,
        }
    }
}

pub struct EntityExtractorPlugin {
    config: EntityExtractorConfig,
}

transform_plugin!(EntityExtractorPlugin, EntityExtractorFactory, "transform-entities");

#[async_trait::async_trait]
impl Plugin for EntityExtractorPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "transform-entities",
            inject: TRANSFORM_INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register(
            cx,
            "entity-extractor",
            Arc::new(EntityExtractorTransform {
                max_per_source: self.config.max_per_source,
            }),
            self.config.llm_call_budget,
            format!("entities-v1|max={}", self.config.max_per_source),
        )
    }
}

struct EntityExtractorTransform {
    max_per_source: usize,
}

#[async_trait::async_trait]
impl Transform for EntityExtractorTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Enrichment
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined()
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let (Some(llm), Some(text)) = (ctx.llm.as_deref(), ctx.text.filter(|t| !t.trim().is_empty()))
        else {
            return TransformOutput::default();
        };
        let hint = ctx.envelope.hint.as_deref();
        let entities = entities::extract(llm, hint, text, self.max_per_source)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("entity extraction failed, continuing without: {e}");
                Vec::new()
            });
        TransformOutput {
            sprouts: Vec::new(),
            entities,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Envelope, Timestamp};

    fn envelope(content_type: &str, hint: &str) -> Envelope {
        Envelope {
            source_type: "file".into(),
            content_type: Mimetype::parse(content_type).expect("valid mimetype"),
            length: ContentLength::Bytes(100),
            created: None,
            modified: Some(Timestamp(1_700_000_000)),
            observed: Timestamp(1_700_000_100),
            properties: Vec::new(),
            hint: Some(hint.into()),
        }
    }

    fn ctx<'a>(
        envelope: &'a Envelope,
        mimetype: &'a Mimetype,
        text: Option<&'a str>,
    ) -> TransformCtx<'a> {
        TransformCtx {
            envelope,
            mimetype,
            is_root: true,
            text,
            bytes: None,
            llm: None,
        }
    }

    fn linked_transforms() -> Vec<(&'static str, Arc<dyn Transform>)> {
        vec![
            ("markdown", Arc::new(MarkdownTransform) as Arc<dyn Transform>),
            ("chunker", Arc::new(ChunkerTransform { target_chars: 1600 })),
            ("summarizer", Arc::new(SummarizerTransform { target_chars: 400 })),
            ("entity-extractor", Arc::new(EntityExtractorTransform { max_per_source: 12 })),
        ]
    }

    #[test]
    fn claims_route_each_root_mimetype_to_the_right_transforms() {
        let cases: &[(&str, &[&str])] = &[
            ("text/markdown", &["markdown", "summarizer", "entity-extractor"]),
            ("text/plain", &["chunker", "summarizer", "entity-extractor"]),
            ("application/json", &["chunker", "summarizer", "entity-extractor"]),
            ("image/jpeg", &["summarizer", "entity-extractor"]),
        ];
        for (mimetype, expected) in cases {
            let m = Mimetype::parse(mimetype).expect("valid");
            let names: Vec<&str> = linked_transforms()
                .iter()
                .filter(|(_, t)| t.claims(&m, true))
                .map(|(n, _)| *n)
                .collect();
            assert_eq!(&names, expected, "claims for {mimetype}");
        }
    }

    #[test]
    fn nothing_claims_non_roots_or_inseam_defined_types() {
        for (_, t) in linked_transforms() {
            assert!(!t.claims(&Mimetype::markdown(), false));
            assert!(!t.claims(&Mimetype::summary(), true));
            assert!(!t.claims(&Mimetype::entity(), true));
        }
    }

    #[tokio::test]
    async fn summarizer_without_content_derives_from_the_envelope() {
        let envelope = envelope("image/jpeg", "IMG_2019.jpeg");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform { target_chars: 200 }
            .apply(ctx(&envelope, &m, None))
            .await;
        assert_eq!(out.sprouts.len(), 1);
        let sprout = &out.sprouts[0];
        assert_eq!(sprout.relation, RelationKind::DerivedFrom);
        assert_eq!(sprout.fragment.mimetype.param("via"), Some("envelope"));
        assert!(sprout
            .fragment
            .text
            .as_deref()
            .expect("has text")
            .contains("IMG_2019.jpeg"));
    }

    #[tokio::test]
    async fn summarizer_without_llm_capability_is_extractive() {
        let envelope = envelope("text/markdown", "note.md");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform { target_chars: 200 }
            .apply(ctx(&envelope, &m, Some("# Reno\n\nBudget notes for the kitchen.")))
            .await;
        assert_eq!(out.sprouts[0].fragment.mimetype.param("via"), Some("extractive"));
    }

    #[tokio::test]
    async fn entity_extractor_without_llm_capability_emits_nothing() {
        let envelope = envelope("text/markdown", "note.md");
        let m = envelope.content_type.clone();
        let out = EntityExtractorTransform { max_per_source: 5 }
            .apply(ctx(&envelope, &m, Some("Dana and the Kitchen Reno.")))
            .await;
        assert!(out.sprouts.is_empty());
        assert!(out.entities.is_empty());
    }

    #[test]
    fn registry_snapshot_orders_structural_before_enrichment() {
        let registry = Registry::default();
        let reg = |entry: &str, t: Arc<dyn Transform>| Registration {
            entry_id: entry.to_string(),
            name: entry.to_string(),
            transform: t,
            llm_call_budget: 0,
            shape_fingerprint: "x".into(),
        };
        registry.register(reg("summarizer", Arc::new(SummarizerTransform { target_chars: 10 })));
        let dispose_md = registry.register(reg("markdown", Arc::new(MarkdownTransform)));
        registry.register(reg("chunker", Arc::new(ChunkerTransform { target_chars: 10 })));

        let order: Vec<String> = registry.snapshot().iter().map(|r| r.entry_id.clone()).collect();
        assert_eq!(order, vec!["chunker", "markdown", "summarizer"]);

        // The disposer is the whole uninstall path.
        dispose_md();
        let order: Vec<String> = registry.snapshot().iter().map(|r| r.entry_id.clone()).collect();
        assert_eq!(order, vec!["chunker", "summarizer"]);
    }
}
