//! The `transform-summarizer` plugin: the one mandatory enrichment. The
//! finder serves summaries in every response so an AI client can decide
//! whether to keep digging, so every indexed source must have one
//! (`design/indexing.md`). Config sets the length, never the existence:
//! verbatim when the text already fits it, LLM when the granted handle
//! allows it, extractive for text otherwise, envelope-derived for
//! everything else ([`summarize`]). Beside the summary it plants the
//! source's keywords for the full-text index. Its golden checks live
//! beside it in `summarizer.checks.toml`.

mod summarize;

use std::sync::Arc;

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{
    ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory, parse_config,
};
use inseam_seams::llm::LlmLane;
use inseam_seams::transforms::{
    Registration, Transform, TransformCtx, TransformKind, TransformOutput, register_as_effect,
};

use summarize::{SummaryKind, SummaryShape};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SummarizerConfig {
    /// Target summary length in characters. Config sets the length, not the
    /// existence: every indexed source gets a summary. Text that already
    /// fits is its own summary and costs no call.
    pub target_chars: usize,
    /// Optional folder target, so a whole-document profile need not repeat
    /// thousands of characters of child summaries in every container.
    pub directory_target_chars: Option<usize>,
    /// Characters of source text one LLM summary call reads. A longer
    /// source is first reduced, without a model, to the sentences that
    /// best represent it across its sections, so the whole document —
    /// not its head — is what the model summarizes (shape tier).
    pub llm_input_chars: usize,
    /// Keywords planted beside the summary for the full-text index, at
    /// most (shape tier). The LLM names them in the same call as the
    /// summary; without one they are the text's own weightiest terms.
    pub keywords_max: usize,
    /// LLM summaries per index run (run-metering tier — not in the shape
    /// stamp); beyond it the summarizer falls back to extractive summaries
    /// so the mandatory-summary invariant still holds.
    pub llm_call_budget: usize,
    /// The lane summary calls ride. `interactive` answers each summary with
    /// its own request. `batch` parks summaries until a large batch-API job
    /// fills and submits them together at the provider's discount — minutes
    /// to hours of latency, for large, time-insensitive runs; `inseam index
    /// --batch` asks for it per run instead. Run-metering tier: a lane
    /// change never re-indexes.
    pub llm_lane: LlmLane,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            target_chars: 400,
            directory_target_chars: None,
            llm_input_chars: 8_000,
            keywords_max: 12,
            llm_call_budget: 500,
            llm_lane: LlmLane::Interactive,
        }
    }
}

pub struct SummarizerPlugin {
    config: SummarizerConfig,
}

pub struct SummarizerFactory;

impl PluginFactory for SummarizerFactory {
    fn name(&self) -> &str {
        "transform-summarizer"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(SummarizerPlugin {
            config: parse_config(config)?,
        }))
    }
}

#[async_trait::async_trait]
impl Plugin for SummarizerPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("transforms")];
        Manifest {
            name: "transform-summarizer",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "summarizer".to_string(),
                transform: Arc::new(SummarizerTransform {
                    directory_target_chars: self.config.directory_target_chars,
                    shape: SummaryShape {
                        target_chars: self.config.target_chars,
                        llm_input_chars: self.config.llm_input_chars,
                        keywords_max: self.config.keywords_max,
                    },
                }),
                llm_call_budget: self.config.llm_call_budget,
                llm_lane: self.config.llm_lane,
                shape_fingerprint: format!(
                    "summarizer-v3|target_chars={}|llm_input_chars={}|keywords_max={}",
                    self.config.target_chars, self.config.llm_input_chars, self.config.keywords_max
                ),
            },
        )
    }
}

pub(crate) struct SummarizerTransform {
    directory_target_chars: Option<usize>,
    pub(crate) shape: SummaryShape,
}

#[async_trait::async_trait]
impl Transform for SummarizerTransform {
    fn kind(&self) -> TransformKind {
        TransformKind::Enrichment
    }

    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && !mimetype.is_inseam_defined()
    }

    fn input_shape(&self, mimetype: &Mimetype, is_root: bool) -> Option<String> {
        if self.claims(mimetype, is_root) {
            if mimetype.is_directory() {
                return self
                    .directory_target_chars
                    .map(|target| format!("directory_target_chars={target}"));
            }
        }
        None
    }

    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let hint = ctx.envelope.hint.as_deref();
        let shape = if ctx.mimetype.is_directory() {
            SummaryShape {
                target_chars: self
                    .directory_target_chars
                    .unwrap_or(self.shape.target_chars),
                ..self.shape
            }
        } else {
            self.shape
        };
        let summary = match ctx.text.filter(|t| !t.trim().is_empty()) {
            Some(content) => {
                summarize::summarize_text(ctx.llm.as_deref(), hint, content, shape).await
            }
            None => summarize::Summary {
                text: summarize::envelope_summary(ctx.envelope),
                kind: SummaryKind::Envelope,
                keywords: Vec::new(),
            },
        };
        if summary.text.is_empty() {
            return TransformOutput::default();
        }
        let mut sprouts = vec![Sprout::leaf(
            NewFragment {
                // The `via` param records provenance: verbatim, llm,
                // extractive, or envelope. The index report counts by it.
                mimetype: Mimetype::summary().with_param("via", summary.kind.as_str()),
                text: Some(summary.text),
                extent: None,
                content_address: None,
            },
            RelationKind::derives(),
        )];
        if !summary.keywords.is_empty() {
            // Keywords are the model's when the summary is; otherwise the
            // text's own. They are full-text rows only, never vectors.
            let via = match summary.kind {
                SummaryKind::Llm => "llm",
                SummaryKind::Verbatim | SummaryKind::Extractive | SummaryKind::Envelope => {
                    "extractive"
                }
            };
            sprouts.push(Sprout::leaf(
                NewFragment {
                    mimetype: Mimetype::keywords().with_param("via", via),
                    text: Some(summary.keywords.join(", ")),
                    extent: None,
                    content_address: None,
                },
                RelationKind::derives(),
            ));
        }
        TransformOutput::sprouts(sprouts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{Address, ContentLength, Envelope, Timestamp};

    fn test_address() -> &'static Address {
        static ADDRESS: std::sync::OnceLock<Address> = std::sync::OnceLock::new();
        ADDRESS.get_or_init(|| {
            "inseam://fs-test/tmp/note.md"
                .parse()
                .expect("valid address")
        })
    }

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
            content_digest: None,
        }
    }

    fn ctx<'a>(
        envelope: &'a Envelope,
        mimetype: &'a Mimetype,
        text: Option<&'a str>,
    ) -> TransformCtx<'a> {
        TransformCtx {
            address: test_address(),
            envelope,
            mimetype,
            is_root: true,
            text,
            bytes: None,
            reference_hops_left: 1,
            llm: None,
        }
    }

    #[tokio::test]
    async fn without_content_derives_from_the_envelope() {
        let envelope = envelope("image/jpeg", "IMG_2019.jpeg");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform {
            shape: shape(200),
            directory_target_chars: None,
        }
        .apply(ctx(&envelope, &m, None))
        .await;
        assert_eq!(out.sprouts.len(), 1, "an envelope summary has no keywords");
        let sprout = &out.sprouts[0];
        assert_eq!(sprout.relation, RelationKind::derives());
        assert_eq!(sprout.fragment.mimetype.param("via"), Some("envelope"));
        assert!(
            sprout
                .fragment
                .text
                .as_deref()
                .expect("has text")
                .contains("IMG_2019.jpeg")
        );
    }

    fn shape(target_chars: usize) -> SummaryShape {
        SummaryShape {
            target_chars,
            llm_input_chars: 8_000,
            keywords_max: 12,
        }
    }

    #[tokio::test]
    async fn without_llm_capability_is_extractive_with_the_texts_own_keywords() {
        let envelope = envelope("text/markdown", "note.md");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform {
            shape: shape(20),
            directory_target_chars: None,
        }
        .apply(ctx(
            &envelope,
            &m,
            Some("# Reno\n\nBudget notes for the kitchen. Kitchen demo in June."),
        ))
        .await;
        assert_eq!(out.sprouts.len(), 2);
        assert_eq!(
            out.sprouts[0].fragment.mimetype.param("via"),
            Some("extractive")
        );
        let keywords = &out.sprouts[1].fragment;
        assert!(keywords.mimetype.is_keywords());
        assert_eq!(keywords.mimetype.param("via"), Some("extractive"));
        assert_eq!(out.sprouts[1].relation, RelationKind::derives());
        assert!(
            keywords
                .text
                .as_deref()
                .expect("has text")
                .contains("kitchen")
        );
    }

    #[tokio::test]
    async fn folder_target_shortens_only_folder_summaries() {
        let transform = SummarizerTransform {
            shape: shape(200),
            directory_target_chars: Some(20),
        };
        for (content_type, expected_via) in [
            ("inode/directory", "extractive"),
            ("text/plain", "verbatim"),
        ] {
            let envelope = envelope(content_type, "notes");
            let out = transform
                .apply(ctx(
                    &envelope,
                    &envelope.content_type,
                    Some("Budget notes for the kitchen. Kitchen demo in June."),
                ))
                .await;
            assert_eq!(
                out.sprouts[0].fragment.mimetype.param("via"),
                Some(expected_via)
            );
        }
    }

    #[tokio::test]
    async fn text_within_the_target_is_verbatim() {
        let envelope = envelope("text/plain", "note.txt");
        let m = envelope.content_type.clone();
        let out = SummarizerTransform {
            shape: shape(200),
            directory_target_chars: None,
        }
        .apply(ctx(&envelope, &m, Some("Budget notes for the kitchen.")))
        .await;
        assert_eq!(
            out.sprouts[0].fragment.mimetype.param("via"),
            Some("verbatim")
        );
        assert_eq!(
            out.sprouts[0].fragment.text.as_deref(),
            Some("Budget notes for the kitchen.")
        );
    }
}
