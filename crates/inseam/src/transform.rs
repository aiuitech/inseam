//! Transforms: registered handlers that take a fragment of a mimetype they
//! claim and emit child fragments with typed relations (`design/indexing.md`).
//! Indexing is recursive transform application until nothing claims the
//! output.
//!
//! Core transforms register into the [`TransformRegistry`] exactly the way
//! plugin transforms will: a claims predicate plus an apply that receives a
//! [`TransformCtx`] and returns a [`TransformOutput`]. The indexer consults
//! the registry and mediates every capability — a transform never reaches for
//! I/O itself; the LLM handle is granted per call (and withheld when the
//! profile's budget is spent), which is the same capability story the WASM
//! sandbox will enforce mechanically. WASM-backed transforms later become
//! additional registrants behind a host-side adapter; the registry and the
//! indexer's pathway do not change.
//!
//! The core structural transforms (markdown, chunker) claim source roots and
//! emit their whole subtree in one application, so the recursion bottoms out
//! immediately; plugin transforms will re-enter by claiming emitted
//! mimetypes.

pub mod chunk;
pub mod entities;
pub mod markdown;
pub mod summarize;

use crate::address::Envelope;
use crate::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use crate::llm::LlmClient;
use crate::profile::IndexProfile;

use entities::ExtractedEntity;
use summarize::SummaryKind;

/// How a transform participates in indexing: structural transforms decompose
/// a fragment into its subtree; enrichment transforms derive understanding
/// (summaries, entities) from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformKind {
    Structural,
    Enrichment,
}

/// Everything a transform application may see. Capabilities are handed in by
/// the indexer, never grabbed: `llm` is `None` when the node is offline or
/// the transform's budget for this run is spent.
pub struct TransformCtx<'a> {
    pub envelope: &'a Envelope,
    /// The fragment's text; `None` for content the node did not read
    /// (binary, oversized, envelope-only profiles).
    pub text: Option<&'a str>,
    pub llm: Option<&'a LlmClient>,
    /// Model the LLM capability should be used with.
    pub model: &'a str,
}

/// What a transform emits, uniformly across kinds:
///
/// - `sprouts` become child fragments of the input fragment (a summary is
///   just a sprout whose relation is `derived-from`);
/// - `entities` are handed back for the core to deduplicate index-wide and
///   wire `mentions` relations — a transform cannot know fragment ids.
#[derive(Debug, Default)]
pub struct TransformOutput {
    pub sprouts: Vec<Sprout>,
    pub entities: Vec<ExtractedEntity>,
    /// LLM calls this application made against the granted capability; the
    /// indexer charges them to the transform's per-run budget. (A WASM host
    /// will count capability invocations mechanically instead.)
    pub llm_calls: usize,
}

impl TransformOutput {
    fn sprouts(sprouts: Vec<Sprout>) -> Self {
        Self {
            sprouts,
            ..Self::default()
        }
    }
}

/// The transforms shipped in core. Enum dispatch: these are the only native
/// implementations, and plugin transforms will arrive through a WASM adapter
/// variant rather than a trait object.
#[derive(Debug, Clone)]
pub enum CoreTransform {
    Markdown,
    Chunker,
    Summarizer { target_chars: usize },
    EntityExtractor { max_per_source: usize },
}

impl CoreTransform {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Chunker => "chunker",
            Self::Summarizer { .. } => "summarizer",
            Self::EntityExtractor { .. } => "entity-extractor",
        }
    }

    pub fn kind(&self) -> TransformKind {
        match self {
            Self::Markdown | Self::Chunker => TransformKind::Structural,
            Self::Summarizer { .. } | Self::EntityExtractor { .. } => TransformKind::Enrichment,
        }
    }

    /// Whether this transform claims a fragment of this mimetype. Core
    /// structural transforms claim source roots only (they decompose whole
    /// sources); the summarizer claims every root — it is the mandatory
    /// transform.
    pub fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        if mimetype.is_inseam_defined() || !is_root {
            return false;
        }
        match self {
            Self::Markdown => mimetype.essence() == "text/markdown",
            Self::Chunker => {
                mimetype.essence() != "text/markdown" && mimetype.is_indexable_text()
            }
            Self::Summarizer { .. } => true,
            Self::EntityExtractor { .. } => true,
        }
    }

    pub async fn apply(&self, ctx: &TransformCtx<'_>) -> TransformOutput {
        match self {
            Self::Markdown => TransformOutput::sprouts(
                ctx.text.map(markdown::decompose).unwrap_or_default(),
            ),
            Self::Chunker => TransformOutput::sprouts(
                ctx.text
                    .map(|t| chunk::chunk(&ctx.envelope.content_type, t))
                    .unwrap_or_default(),
            ),
            Self::Summarizer { target_chars } => {
                let hint = ctx.envelope.hint.as_deref();
                let (text, kind) = match ctx.text.filter(|t| !t.trim().is_empty()) {
                    Some(content) => {
                        summarize::summarize_text(
                            ctx.llm.map(|c| (c, ctx.model)),
                            hint,
                            content,
                            *target_chars,
                        )
                        .await
                    }
                    None => (
                        summarize::envelope_summary(ctx.envelope),
                        SummaryKind::Envelope,
                    ),
                };
                if text.is_empty() {
                    return TransformOutput::default();
                }
                TransformOutput {
                    sprouts: vec![Sprout::leaf(
                        NewFragment {
                            // The `via` param records provenance: llm,
                            // extractive, or envelope. The index report
                            // counts by it.
                            mimetype: Mimetype::summary().with_param("via", kind.as_str()),
                            text: Some(text),
                            extent: None,
                        },
                        RelationKind::DerivedFrom,
                    )],
                    entities: Vec::new(),
                    llm_calls: usize::from(kind == SummaryKind::Llm),
                }
            }
            Self::EntityExtractor { max_per_source } => {
                let (Some(llm), Some(text)) = (ctx.llm, ctx.text.filter(|t| !t.trim().is_empty()))
                else {
                    return TransformOutput::default();
                };
                let entities = entities::extract(
                    llm,
                    ctx.model,
                    ctx.envelope.hint.as_deref(),
                    text,
                    *max_per_source,
                )
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("entity extraction failed, continuing without: {e}");
                    Vec::new()
                });
                TransformOutput {
                    sprouts: Vec::new(),
                    entities,
                    llm_calls: 1,
                }
            }
        }
    }
}

/// A registered transform plus what the core mediates for it: how many LLM
/// calls it may make per index run (0 = never granted the capability).
#[derive(Debug, Clone)]
pub struct Registered {
    pub transform: CoreTransform,
    pub llm_call_budget: usize,
}

/// The registration pathway. Core registers its transforms here from the
/// profile; plugin transforms will register through the same door.
/// Registration order is application order — structural before enrichment,
/// so enrichment sees the decomposed fragments.
#[derive(Debug, Clone)]
pub struct TransformRegistry {
    entries: Vec<Registered>,
}

impl TransformRegistry {
    pub fn from_profile(profile: &IndexProfile) -> Self {
        let mut entries = vec![
            Registered {
                transform: CoreTransform::Markdown,
                llm_call_budget: 0,
            },
            Registered {
                transform: CoreTransform::Chunker,
                llm_call_budget: 0,
            },
            Registered {
                transform: CoreTransform::Summarizer {
                    target_chars: profile.summary.target_chars,
                },
                llm_call_budget: profile.summary.llm_call_budget,
            },
        ];
        if profile.entities.enabled {
            entries.push(Registered {
                transform: CoreTransform::EntityExtractor {
                    max_per_source: profile.entities.max_per_source,
                },
                llm_call_budget: profile.entities.llm_call_budget,
            });
        }
        Self { entries }
    }

    /// The registered transforms claiming this fragment, in application order.
    pub fn claimants<'a>(
        &'a self,
        mimetype: &'a Mimetype,
        is_root: bool,
    ) -> impl Iterator<Item = &'a Registered> {
        self.entries
            .iter()
            .filter(move |r| r.transform.claims(mimetype, is_root))
    }
}

/// Decomposition limits from the index profile.
#[derive(Debug, Clone, Copy)]
pub struct DecomposeBudget {
    pub max_depth: usize,
    pub max_fragments: usize,
}

/// Enforce depth and count budgets over a sprout forest, breadth-first so
/// shallow structure survives before deep detail.
pub fn prune(sprouts: Vec<Sprout>, budget: DecomposeBudget) -> Vec<Sprout> {
    fn depth_prune(mut sprouts: Vec<Sprout>, depth_left: usize) -> Vec<Sprout> {
        if depth_left == 0 {
            return Vec::new();
        }
        for s in &mut sprouts {
            s.children = depth_prune(std::mem::take(&mut s.children), depth_left - 1);
        }
        sprouts
    }

    fn count_prune(sprouts: &mut Vec<Sprout>, remaining: &mut usize) {
        sprouts.retain_mut(|s| {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            count_prune(&mut s.children, remaining);
            true
        });
    }

    let mut sprouts = depth_prune(sprouts, budget.max_depth.max(1));
    let mut remaining = budget.max_fragments;
    count_prune(&mut sprouts, &mut remaining);
    sprouts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{ContentLength, Timestamp};
    use crate::fragment::RelationKind;

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

    fn registry() -> TransformRegistry {
        TransformRegistry::from_profile(&IndexProfile::default())
    }

    #[test]
    fn claims_route_each_root_mimetype_to_the_right_transforms() {
        let cases: &[(&str, &[&str])] = &[
            ("text/markdown", &["markdown", "summarizer", "entity-extractor"]),
            ("text/plain", &["chunker", "summarizer", "entity-extractor"]),
            ("application/json", &["chunker", "summarizer", "entity-extractor"]),
            ("image/jpeg", &["summarizer", "entity-extractor"]),
        ];
        let registry = registry();
        for (mimetype, expected) in cases {
            let m = Mimetype::parse(mimetype).expect("valid");
            let names: Vec<&str> = registry
                .claimants(&m, true)
                .map(|r| r.transform.name())
                .collect();
            assert_eq!(&names, expected, "claims for {mimetype}");
        }
    }

    #[test]
    fn nothing_claims_non_roots_or_inseam_defined_types() {
        let registry = registry();
        assert_eq!(registry.claimants(&Mimetype::markdown(), false).count(), 0);
        assert_eq!(registry.claimants(&Mimetype::summary(), true).count(), 0);
        assert_eq!(registry.claimants(&Mimetype::entity(), true).count(), 0);
    }

    #[test]
    fn disabling_entities_unregisters_the_extractor() {
        let mut profile = IndexProfile::default();
        profile.entities.enabled = false;
        let registry = TransformRegistry::from_profile(&profile);
        assert!(registry
            .claimants(&Mimetype::markdown(), true)
            .all(|r| r.transform.name() != "entity-extractor"));
    }

    #[tokio::test]
    async fn summarizer_without_content_derives_from_the_envelope() {
        let envelope = envelope("image/jpeg", "IMG_2019.jpeg");
        let ctx = TransformCtx {
            envelope: &envelope,
            text: None,
            llm: None,
            model: "unused",
        };
        let out = CoreTransform::Summarizer { target_chars: 200 }.apply(&ctx).await;
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
        let ctx = TransformCtx {
            envelope: &envelope,
            text: Some("# Reno\n\nBudget notes for the kitchen."),
            llm: None,
            model: "unused",
        };
        let out = CoreTransform::Summarizer { target_chars: 200 }.apply(&ctx).await;
        assert_eq!(out.sprouts[0].fragment.mimetype.param("via"), Some("extractive"));
    }

    #[tokio::test]
    async fn entity_extractor_without_llm_capability_emits_nothing() {
        let envelope = envelope("text/markdown", "note.md");
        let ctx = TransformCtx {
            envelope: &envelope,
            text: Some("Dana and the Kitchen Reno."),
            llm: None,
            model: "unused",
        };
        let out = CoreTransform::EntityExtractor { max_per_source: 5 }.apply(&ctx).await;
        assert!(out.sprouts.is_empty());
        assert!(out.entities.is_empty());
    }

    #[test]
    fn prune_caps_depth() {
        fn sprout(children: Vec<Sprout>) -> Sprout {
            Sprout {
                fragment: NewFragment {
                    mimetype: Mimetype::text_plain(),
                    text: Some("x".into()),
                    extent: None,
                },
                relation: RelationKind::Contains,
                children,
            }
        }
        let tree = vec![sprout(vec![sprout(vec![sprout(vec![])])])];
        let out = prune(
            tree,
            DecomposeBudget {
                max_depth: 2,
                max_fragments: 100,
            },
        );
        assert_eq!(out[0].children.len(), 1);
        assert!(out[0].children[0].children.is_empty());

        let flat = vec![sprout(vec![]), sprout(vec![]), sprout(vec![])];
        let capped = prune(
            flat,
            DecomposeBudget {
                max_depth: 3,
                max_fragments: 2,
            },
        );
        assert_eq!(capped.len(), 2);
    }
}
