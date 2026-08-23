//! The `transform-entities` plugin: the enrichment that pulls people,
//! places, organizations, projects and dates out of a root as **keyed
//! sprouts** — one `text/x-inseam-entity` fragment per entity across the
//! whole index, anchored by a `mentions` edge to every fragment whose text
//! names it ([`extract`]). Entities are the graph's connective tissue: two
//! unrelated sources mentioning the same person end up one hop apart
//! (`design/indexing.md`). The entity vocabulary is entirely this plugin's;
//! the kernel only knows keyed fragments and relation names. Useless without
//! the granted LLM handle, so it emits nothing when the handle is withheld.
//! Its golden checks live beside it in `entity-extractor.checks.toml`.

mod extract;

use std::sync::Arc;

use inseam_kernel::fragment::{Mimetype, NewFragment};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::llm::LlmLane;
use inseam_seams::transforms::{
    register_as_effect, Anchor, KeyedSprout, Registration, Transform, TransformCtx,
    TransformKind, TransformOutput,
};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EntityExtractorConfig {
    /// Cap on entities taken from a single source (shape tier).
    pub max_per_source: usize,
    /// Extraction LLM calls per index run (run-metering tier).
    pub llm_call_budget: usize,
    /// The lane extraction calls ride: `interactive` (one request each) or
    /// `batch` (collected into the endpoint's batch-API jobs — cheaper and
    /// slower, for large runs). Run-metering tier: a lane change never
    /// re-indexes.
    pub llm_lane: LlmLane,
}

impl Default for EntityExtractorConfig {
    fn default() -> Self {
        Self {
            max_per_source: 12,
            llm_call_budget: 500,
            llm_lane: LlmLane::Interactive,
        }
    }
}

pub struct EntityExtractorPlugin {
    config: EntityExtractorConfig,
}

pub struct EntityExtractorFactory;

impl PluginFactory for EntityExtractorFactory {
    fn name(&self) -> &str {
        "transform-entities"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(EntityExtractorPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for EntityExtractorPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-entities",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "entity-extractor".to_string(),
                transform: Arc::new(EntityExtractorTransform {
                    max_per_source: self.config.max_per_source,
                }),
                llm_call_budget: self.config.llm_call_budget,
                llm_lane: self.config.llm_lane,
                shape_fingerprint: format!("entities-v1|max={}", self.config.max_per_source),
            },
        )
    }
}

pub(crate) struct EntityExtractorTransform {
    pub(crate) max_per_source: usize,
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
        let entities = extract::extract(llm, hint, text, self.max_per_source)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("entity extraction failed, continuing without: {e}");
                Vec::new()
            });
        let keyed = entities
            .into_iter()
            .map(|entity| KeyedSprout {
                key: entity.key(),
                fragment: NewFragment {
                    mimetype: extract::entity_mimetype(entity.kind),
                    text: Some(entity.name.clone()),
                    extent: None,
                },
                relation: extract::mentions(),
                anchor: Anchor::TextContaining(entity.name),
            })
            .collect();
        TransformOutput {
            sprouts: Vec::new(),
            keyed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{ContentLength, Envelope, Timestamp};

    #[tokio::test]
    async fn without_llm_capability_emits_nothing() {
        let envelope = Envelope {
            source_type: "file".into(),
            content_type: Mimetype::markdown(),
            length: ContentLength::Bytes(100),
            created: None,
            modified: None,
            observed: Timestamp(1_700_000_100),
            properties: Vec::new(),
            hint: Some("note.md".into()),
            content_digest: None,
        };
        let m = envelope.content_type.clone();
        let out = EntityExtractorTransform { max_per_source: 5 }
            .apply(TransformCtx {
                envelope: &envelope,
                mimetype: &m,
                is_root: true,
                text: Some("Dana and the Kitchen Reno."),
                bytes: None,
                llm: None,
            })
            .await;
        assert!(out.sprouts.is_empty());
        assert!(out.keyed.is_empty());
    }
}
