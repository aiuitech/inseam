//! The `embedder` provider plugin. Three modes behind one config: the
//! configured endpoint, deterministic local hashing (offline/small-device
//! fallback and the test embedder), or none (full-text only). Activating
//! declares the embedding identity — model, width, and which fragments get
//! vectors — to the store; that is what binds the search surface, and what
//! pends an in-place re-embed when the identity changed
//! (`design/index-maintenance.md`). Unloading withdraws it, so a provider
//! swap restarts consumers against the new surface.
//!
//! The width is the model's to decide. `dimensions` left unset takes the
//! model's native width; set, it is checked against what the model produces
//! — a catalog of known models ([`catalog`]) first, then what the endpoint
//! itself reports (a local ollama introspects its models), and only for a
//! model neither knows is the configured value taken on trust, with the
//! first embedding as the check. A smaller width is accepted only for
//! models trained for it, because every OpenAI-compatible server truncates
//! on request and a truncated non-Matryoshka vector is noise.

mod catalog;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use inseam_kernel::store::{EmbeddingIdentity, VectorScope};
use inseam_kernel::substrate::{
    ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, STORE, parse_config,
};
use inseam_seams::SeamError;
use inseam_seams::embedder::{self, EMBEDDER, Embedder};
use inseam_seams::llm::{EmbedRequest, EmbeddingModel, LLM, Llm};

/// Characters of input text an embedding sees; more adds cost, not recall.
const EMBED_INPUT_CHARS: usize = 6_000;
/// Width of the hashed embedder when the config leaves it to the provider.
const HASHED_DIMENSIONS_DEFAULT: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Remote embeddings through the `llm` seam's endpoint.
    Endpoint,
    /// Deterministic local bag-of-words hashing: weak but offline and free.
    Hashed,
    /// No vectors at all; discovery degrades to full-text seeding only.
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbedderConfig {
    pub provider: Provider,
    pub model: String,
    /// Vector width. Unset takes the model's native width; set, it must be
    /// what the model produces, or a smaller width the model was trained
    /// to support. For `hashed`, any width (default 256).
    pub dimensions: Option<usize>,
    /// Which fragments get vectors. `summaries` keeps the vector bulk to one
    /// bounded row per source; full-text search still covers every
    /// fragment.
    pub vectors: VectorScope,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Endpoint,
            model: "openai/text-embedding-3-small".to_string(),
            dimensions: None,
            vectors: VectorScope::All,
        }
    }
}

pub struct EmbedderPlugin {
    config: EmbedderConfig,
}

impl EmbedderPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: EmbedderConfig = parse_config(config)?;
        if config.dimensions == Some(0) {
            return Err(PluginError(
                "embedder `dimensions` must be greater than zero; leave it unset to take \
                 the model's native width"
                    .to_string(),
            ));
        }
        Ok(Self { config })
    }
}

pub struct EmbedderFactory;

impl inseam_kernel::substrate::PluginFactory for EmbedderFactory {
    fn name(&self) -> &str {
        "embedder"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(EmbedderPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for EmbedderPlugin {
    fn manifest(&self) -> Manifest {
        // `llm` is optional at the manifest level because only the endpoint
        // mode needs it; that mode fails loudly at apply when it's absent.
        static INJECT: &[Inject] = &[Inject::required("store"), Inject::optional("llm")];
        Manifest {
            name: "embedder",
            inject: INJECT,
            provides: &["embedder"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let store = cx.get(&STORE)?;
        let vectors = self.config.vectors;
        let (implementation, identity): (Arc<dyn Embedder>, EmbeddingIdentity) =
            match self.config.provider {
                Provider::Endpoint => {
                    let llm = cx.try_get(&LLM)?.ok_or_else(|| {
                        PluginError(
                            "embedder provider is `endpoint` but no `llm` service is mounted; \
                             set the endpoint API key, or switch the provider to `hashed`"
                                .to_string(),
                        )
                    })?;
                    let width = resolve_endpoint_width(
                        llm.as_ref(),
                        &self.config.model,
                        self.config.dimensions,
                    )
                    .await?;
                    (
                        Arc::new(EndpointEmbedder {
                            llm,
                            model: self.config.model.clone(),
                            width,
                            vectors,
                        }),
                        EmbeddingIdentity {
                            model: self.config.model.clone(),
                            dimensions: width.dimensions,
                            vectors,
                        },
                    )
                }
                Provider::Hashed => {
                    let dimensions = self.config.dimensions.unwrap_or(HASHED_DIMENSIONS_DEFAULT);
                    (
                        Arc::new(HashedEmbedder {
                            dimensions,
                            vectors,
                        }),
                        EmbeddingIdentity {
                            model: format!("hashed-{dimensions}"),
                            dimensions,
                            vectors,
                        },
                    )
                }
                Provider::None => (
                    Arc::new(DisabledEmbedder),
                    EmbeddingIdentity {
                        model: "none".to_string(),
                        dimensions: 0,
                        vectors,
                    },
                ),
            };

        let facts = Facts::new()
            .with(
                embedder::facts::OFFLINE,
                self.config.provider != Provider::Endpoint,
            )
            .with(embedder::facts::MODEL, identity.model.as_str())
            .with(embedder::facts::DIMENSIONS, identity.dimensions as u64)
            .with(embedder::facts::VECTORS, identity.vectors.as_str());
        store
            .declare_embedding(identity)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let store_for_undo = Arc::clone(&store);
        cx.effect("declare embedding identity", move || {
            store_for_undo.withdraw_embedding();
        });
        cx.provide(&EMBEDDER, implementation, facts)?;
        Ok(())
    }
}

/// The width an endpoint embedder runs at, and whether the request must
/// ask the model for it (a reduction) or the model produces it natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EndpointWidth {
    dimensions: usize,
    /// `Some` only when `dimensions` is a reduction the model supports; sent
    /// as the request's `dimensions`.
    requested: Option<usize>,
}

/// Settle the width from the model, the endpoint, and the config — in that
/// order of authority. A config that disagrees with the model is refused
/// here, at activation, not discovered as a width mismatch mid-index.
async fn resolve_endpoint_width(
    llm: &dyn Llm,
    model: &str,
    configured: Option<usize>,
) -> Result<EndpointWidth, PluginError> {
    let known = match catalog::lookup(model) {
        Some(known) => Some(known),
        None => llm
            .embedding_model(model)
            .await
            .map_err(|e| PluginError(format!("embedder model `{model}`: {e}")))?,
    };
    settle_width(model, known, configured)
}

/// Pure decision table over what is known and what is configured.
fn settle_width(
    model: &str,
    known: Option<EmbeddingModel>,
    configured: Option<usize>,
) -> Result<EndpointWidth, PluginError> {
    match (known, configured) {
        (None, None) => Err(PluginError(format!(
            "the vector width of `{model}` is not known; set the embedder's `dimensions` to \
             the width the model produces"
        ))),
        (None, Some(dimensions)) => Ok(EndpointWidth {
            dimensions,
            requested: None,
        }),
        (Some(known), None) => Ok(EndpointWidth {
            dimensions: known.dimensions,
            requested: None,
        }),
        (Some(known), Some(dimensions)) if dimensions == known.dimensions => Ok(EndpointWidth {
            dimensions,
            requested: None,
        }),
        (Some(known), Some(dimensions)) if dimensions < known.dimensions && known.reducible => {
            Ok(EndpointWidth {
                dimensions,
                requested: Some(dimensions),
            })
        }
        (Some(known), Some(dimensions)) if dimensions < known.dimensions => {
            Err(PluginError(format!(
                "`{model}` produces {}-dimension vectors and does not support a smaller width; \
             set `dimensions = {}` or leave it unset",
                known.dimensions, known.dimensions
            )))
        }
        (Some(known), Some(dimensions)) => Err(PluginError(format!(
            "`{model}` produces {}-dimension vectors; `dimensions = {dimensions}` asks for more \
             than the model has — set `dimensions = {}` or leave it unset",
            known.dimensions, known.dimensions
        ))),
    }
}

struct EndpointEmbedder {
    llm: Arc<dyn Llm>,
    model: String,
    width: EndpointWidth,
    vectors: VectorScope,
}

#[async_trait::async_trait]
impl Embedder for EndpointEmbedder {
    fn dimensions(&self) -> Option<usize> {
        Some(self.width.dimensions)
    }

    fn vectors(&self) -> VectorScope {
        self.vectors
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
        let bounded: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(EMBED_INPUT_CHARS).collect())
            .collect();
        let inputs: Vec<&str> = bounded.iter().map(String::as_str).collect();
        let vectors = self
            .llm
            .embed(&EmbedRequest {
                model: &self.model,
                inputs: &inputs,
                dimensions: self.width.requested,
            })
            .await?;
        // The catalog and the endpoint settled the width at activation; a
        // model neither knew was taken on trust, and this is its check.
        if let Some(v) = vectors.first()
            && v.len() != self.width.dimensions
        {
            return Err(SeamError::failed(format!(
                "`{}` returned {}-dimension vectors, config says {}; set `dimensions = {}` \
                 (or leave it unset) and re-index",
                self.model,
                v.len(),
                self.width.dimensions,
                v.len()
            )));
        }
        Ok(vectors)
    }
}

struct HashedEmbedder {
    dimensions: usize,
    vectors: VectorScope,
}

#[async_trait::async_trait]
impl Embedder for HashedEmbedder {
    fn dimensions(&self) -> Option<usize> {
        Some(self.dimensions)
    }

    fn vectors(&self) -> VectorScope {
        self.vectors
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
        Ok(texts
            .iter()
            .map(|t| hashed_embedding(t, self.dimensions))
            .collect())
    }
}

struct DisabledEmbedder;

#[async_trait::async_trait]
impl Embedder for DisabledEmbedder {
    fn dimensions(&self) -> Option<usize> {
        None
    }

    fn vectors(&self) -> VectorScope {
        VectorScope::All
    }

    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
        Err(SeamError::Unavailable("embeddings are disabled".into()))
    }
}

/// Deterministic bag-of-words embedding: each token hashes to a dimension
/// and a sign, counts accumulate, the vector is L2-normalized. Shared
/// vocabulary yields real cosine similarity — weak semantics, strong
/// determinism.
fn hashed_embedding(text: &str, dimensions: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dimensions.max(1)];
    for token in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let h = inseam_kernel::substrate::fnv1a(token.to_lowercase().as_bytes());
        let dim = (h % dimensions.max(1) as u64) as usize;
        let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
        v[dim] += sign;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Test/consumer helper: a standalone hashed embedder without a kernel.
pub fn hashed(dimensions: usize) -> Arc<dyn Embedder> {
    Arc::new(HashedEmbedder {
        dimensions,
        vectors: VectorScope::All,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashed_embeddings_are_deterministic_and_normalized() {
        let e = hashed(64);
        let a = e
            .embed(&["kitchen renovation notes"])
            .await
            .expect("embeds");
        let b = e
            .embed(&["kitchen renovation notes"])
            .await
            .expect("embeds");
        assert_eq!(a, b);
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn hashed_embeddings_rank_shared_vocabulary_higher() {
        let e = hashed(64);
        let vs = e
            .embed(&[
                "kitchen renovation budget",
                "renovation budget for the kitchen remodel",
                "quarterly tax filing checklist",
            ])
            .await
            .expect("embeds");
        let cos = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        assert!(cos(&vs[0], &vs[1]) > cos(&vs[0], &vs[2]));
    }

    #[tokio::test]
    async fn disabled_embedder_refuses() {
        let e = DisabledEmbedder;
        assert!(e.embed(&["x"]).await.is_err());
        assert_eq!(e.dimensions(), None);
    }

    #[test]
    fn zero_dimensions_is_refused_at_config() {
        let mut config = toml::Table::new();
        config.insert("dimensions".into(), toml::Value::Integer(0));
        assert!(EmbedderPlugin::from_config(&config).is_err());
    }

    #[test]
    fn config_defaults_leave_width_to_the_model_and_vectors_everywhere() {
        let plugin = EmbedderPlugin::from_config(&toml::Table::new()).expect("defaults parse");
        assert_eq!(plugin.config.dimensions, None);
        assert_eq!(plugin.config.vectors, VectorScope::All);
    }

    const MINILM: EmbeddingModel = EmbeddingModel {
        dimensions: 384,
        reducible: false,
    };
    const SMALL3: EmbeddingModel = EmbeddingModel {
        dimensions: 1536,
        reducible: true,
    };

    #[test]
    fn unset_width_takes_the_models_native_width() {
        let width = settle_width("m", Some(MINILM), None).expect("settles");
        assert_eq!(width.dimensions, 384);
        assert_eq!(width.requested, None);
    }

    #[test]
    fn matching_width_is_accepted_without_a_request_parameter() {
        let width = settle_width("m", Some(MINILM), Some(384)).expect("settles");
        assert_eq!(width.requested, None);
    }

    #[test]
    fn a_reduction_is_requested_only_from_models_trained_for_it() {
        let width = settle_width("m", Some(SMALL3), Some(256)).expect("settles");
        assert_eq!(width.dimensions, 256);
        assert_eq!(width.requested, Some(256));
        let refused = settle_width("m", Some(MINILM), Some(128)).expect_err("refuses");
        assert!(refused.0.contains("does not support a smaller width"));
    }

    #[test]
    fn more_than_the_model_has_is_refused() {
        let refused = settle_width("m", Some(MINILM), Some(768)).expect_err("refuses");
        assert!(refused.0.contains("more than the model has"));
    }

    #[test]
    fn an_unknown_model_needs_an_explicit_width_and_takes_it_on_trust() {
        assert!(settle_width("m", None, None).is_err());
        let width = settle_width("m", None, Some(1024)).expect("settles");
        assert_eq!(width.dimensions, 1024);
        assert_eq!(width.requested, None);
    }
}
