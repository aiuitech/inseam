//! The `transform-hints` plugin: one LLM call per root that plants the
//! small pieces a searcher's question is likely to land on, and the edges
//! that tie sources together (`design/indexing.md`). Under the source it
//! emits `text/x-inseam-hint` fragments — a synopsis, the questions the
//! document answers in a colleague's plain words (cues), and the facts that
//! tell it apart from its near-duplicates (discriminators) — each a
//! full-text row and, under the `summaries` vector scope, a vector. Across
//! the index it emits **keyed** fragments: glossary terms (an internal word
//! with its plain gloss), exact identifiers, and entities, one fragment per
//! term however many sources use it, each anchored by a `mentions` edge, so
//! two documents that share a codename are one hop apart. Useless without
//! the granted LLM handle; emits nothing when it is withheld. Golden checks
//! live beside it in `hints.checks.toml`.

pub(crate) mod extract;

use std::sync::Arc;

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory, parse_config,
};
use inseam_seams::llm::LlmLane;
use inseam_seams::transforms::{
    Anchor, KeyedSprout, Registration, Transform, TransformCtx, TransformKind, TransformOutput,
    register_as_effect,
};

use crate::transform_entities::extract::entity_mimetype;
use extract::{HintKind, HintLimits, Hints};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HintsConfig {
    /// Characters of source text one call sees; a longer text is reduced
    /// to its telling sentences first (shape tier).
    pub llm_input_chars: usize,
    /// Caps on each hint list (shape tier).
    pub cues_max: usize,
    pub glossary_max: usize,
    pub identifiers_max: usize,
    pub discriminators_max: usize,
    pub entities_max: usize,
    /// Extraction calls per index run (run-metering tier).
    pub llm_call_budget: usize,
    /// The lane calls ride (run-metering tier).
    pub llm_lane: LlmLane,
}

impl Default for HintsConfig {
    fn default() -> Self {
        Self {
            llm_input_chars: 8_000,
            cues_max: 6,
            glossary_max: 12,
            identifiers_max: 12,
            discriminators_max: 6,
            entities_max: 12,
            llm_call_budget: 500,
            llm_lane: LlmLane::Interactive,
        }
    }
}

impl HintsConfig {
    fn limits(&self) -> HintLimits {
        HintLimits {
            llm_input_chars: self.llm_input_chars,
            cues_max: self.cues_max,
            glossary_max: self.glossary_max,
            identifiers_max: self.identifiers_max,
            discriminators_max: self.discriminators_max,
            entities_max: self.entities_max,
        }
    }
}

pub struct HintsPlugin {
    config: HintsConfig,
}

pub struct HintsFactory;

impl PluginFactory for HintsFactory {
    fn name(&self) -> &str {
        "transform-hints"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(HintsPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for HintsPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-hints",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let limits = self.config.limits();
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "hints".to_string(),
                transform: Arc::new(HintsTransform { limits }),
                llm_call_budget: self.config.llm_call_budget,
                llm_lane: self.config.llm_lane,
                shape_fingerprint: format!(
                    "hints-v1|input={}|cues={}|glossary={}|identifiers={}|discriminators={}|entities={}",
                    limits.llm_input_chars,
                    limits.cues_max,
                    limits.glossary_max,
                    limits.identifiers_max,
                    limits.discriminators_max,
                    limits.entities_max
                ),
            },
        )
    }
}

pub(crate) struct HintsTransform {
    pub(crate) limits: HintLimits,
}

#[async_trait::async_trait]
impl Transform for HintsTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Enrichment
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined()
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let (Some(llm), Some(text)) = (
            ctx.llm.as_deref(),
            ctx.text.filter(|t| !t.trim().is_empty()),
        ) else {
            return TransformOutput::default();
        };
        let hint = ctx.envelope.hint.as_deref();
        let hints = extract::extract(llm, hint, text, self.limits)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("hint extraction failed, continuing without: {e}");
                Hints::default()
            });
        TransformOutput {
            sprouts: hint_sprouts(&hints),
            keyed: keyed_sprouts(hints),
        }
    }
}

/// The per-source hint fragments: one row per kind, cues and discriminators
/// one per line so a full-text hit lands on a whole question or fact.
fn hint_sprouts(hints: &Hints) -> Vec<Sprout> {
    let mut sprouts = Vec::new();
    if !hints.synopsis.is_empty() {
        sprouts.push(hint_sprout(HintKind::Synopsis, hints.synopsis.clone()));
    }
    if !hints.cues.is_empty() {
        sprouts.push(hint_sprout(HintKind::Cue, hints.cues.join("\n")));
    }
    if !hints.discriminators.is_empty() {
        sprouts.push(hint_sprout(
            HintKind::Discriminator,
            hints.discriminators.join("\n"),
        ));
    }
    sprouts
}

fn hint_sprout(kind: HintKind, text: String) -> Sprout {
    Sprout::leaf(
        NewFragment {
            mimetype: extract::hint_mimetype(kind),
            text: Some(text),
            extent: None,
            content_address: None,
        },
        RelationKind::derives(),
    )
}

/// The index-wide fragments: glossary terms, identifiers, and entities,
/// each anchored where the source's text names it (the root when nothing
/// does — a gloss is not always spelled as the document spells the term).
fn keyed_sprouts(hints: Hints) -> Vec<KeyedSprout> {
    let mut keyed = Vec::new();
    for term in hints.glossary {
        keyed.push(KeyedSprout {
            key: term.key(),
            fragment: NewFragment {
                mimetype: extract::term_mimetype(),
                text: Some(term.text()),
                extent: None,
                content_address: None,
            },
            relation: extract::mentions(),
            anchor: Anchor::TextContaining(term.term),
        });
    }
    for identifier in hints.identifiers {
        keyed.push(KeyedSprout {
            key: extract::identifier_key(&identifier),
            fragment: NewFragment {
                mimetype: extract::identifier_mimetype(),
                text: Some(identifier.clone()),
                extent: None,
                content_address: None,
            },
            relation: extract::mentions(),
            anchor: Anchor::TextContaining(identifier),
        });
    }
    for entity in hints.entities {
        keyed.push(KeyedSprout {
            key: entity.key(),
            fragment: NewFragment {
                mimetype: entity_mimetype(entity.kind),
                text: Some(entity.name.clone()),
                extent: None,
                content_address: None,
            },
            relation: extract::mentions(),
            anchor: Anchor::TextContaining(entity.name),
        });
    }
    keyed
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{Address, ContentLength, Envelope, Timestamp};

    fn address() -> Address {
        "inseam://fs-test/tmp/note.md"
            .parse()
            .expect("valid address")
    }

    fn envelope() -> Envelope {
        Envelope {
            source_type: "file".into(),
            content_type: Mimetype::parse("text/plain").expect("valid"),
            length: ContentLength::Bytes(100),
            created: None,
            modified: None,
            observed: Timestamp(1_700_000_100),
            properties: Vec::new(),
            facets: Vec::new(),
            hint: Some("note.txt".into()),
            content_digest: None,
        }
    }

    #[tokio::test]
    async fn without_llm_capability_emits_nothing() {
        let envelope = envelope();
        let m = envelope.content_type.clone();
        let out = HintsTransform {
            limits: HintsConfig::default().limits(),
        }
        .apply(TransformCtx {
            address: &address(),
            envelope: &envelope,
            mimetype: &m,
            is_root: true,
            text: Some("EKS bootstrap for eu-central-2."),
            bytes: None,
            reference_hops_left: 1,
            llm: None,
        })
        .await;
        assert!(out.sprouts.is_empty());
        assert!(out.keyed.is_empty());
    }

    #[test]
    fn hints_become_one_fragment_per_kind_and_one_keyed_per_term() {
        let hints = Hints {
            synopsis: "Cluster bootstrap.".into(),
            cues: vec![
                "How do we bring up Frankfurt?".into(),
                "What quotas?".into(),
            ],
            glossary: vec![extract::GlossaryTerm {
                term: "eu-central-2".into(),
                gloss: "the Frankfurt region".into(),
            }],
            identifiers: vec!["SUP-100432".into()],
            discriminators: vec![],
            entities: vec![],
        };
        let sprouts = hint_sprouts(&hints);
        assert_eq!(sprouts.len(), 2, "no discriminators, no discriminator row");
        assert!(
            sprouts[1]
                .fragment
                .text
                .as_deref()
                .expect("text")
                .contains('\n')
        );
        let keyed = keyed_sprouts(hints);
        assert_eq!(keyed.len(), 2);
        assert_eq!(keyed[0].key.as_str(), "term:eu-central-2");
        assert_eq!(keyed[1].key.as_str(), "identifier:sup-100432");
        assert!(keyed.iter().all(|k| k.relation == extract::mentions()));
    }
}
