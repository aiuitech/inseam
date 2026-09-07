//! The `transform-chunker` plugin: the structural fallback for text with no
//! semantic structure — paragraph-boundary chunks merged toward a target
//! size ([`chunk`]). It claims every indexable-text root the markdown
//! transform does not (that one takes markdown and plain text), so a
//! source the structural transforms don't understand still gets a subtree. Its golden checks live beside it in `chunker.checks.toml`.

pub(crate) mod chunk;

use std::sync::Arc;

use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::{
    ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory, parse_config,
};
use inseam_seams::llm::LlmLane;
use inseam_seams::text::is_indexable_text;
use inseam_seams::transforms::{
    Registration, Transform, TransformCtx, TransformKind, TransformOutput, register_as_effect,
};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkerConfig {
    /// Aim for chunks around this many characters.
    pub target_chars: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            target_chars: 1_600,
        }
    }
}

pub struct ChunkerPlugin {
    config: ChunkerConfig,
}

pub struct ChunkerFactory;

impl PluginFactory for ChunkerFactory {
    fn name(&self) -> &str {
        "transform-chunker"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(ChunkerPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for ChunkerPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-chunker",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "chunker".to_string(),
                transform: Arc::new(ChunkerTransform {
                    target_chars: self.config.target_chars,
                }),
                llm_call_budget: 0,
                llm_lane: LlmLane::Interactive,
                shape_fingerprint: format!("chunker-v1|target={}", self.config.target_chars),
            },
        )
    }
}

pub(crate) struct ChunkerTransform {
    pub(crate) target_chars: usize,
}

#[async_trait::async_trait]
impl Transform for ChunkerTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root
            && !mimetype.is_inseam_defined()
            && !crate::transform_markdown::decompose::claims_essence(mimetype.essence())
            && is_indexable_text(mimetype)
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        TransformOutput::sprouts(
            ctx.text
                .map(|t| chunk::chunk_with_target(ctx.mimetype, t, self.target_chars))
                .unwrap_or_default(),
        )
    }
}
