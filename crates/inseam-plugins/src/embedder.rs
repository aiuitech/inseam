//! The `embedder` provider plugin. Three modes behind one config: the
//! configured endpoint, deterministic local hashing (offline/small-device
//! fallback and the test embedder), or none (full-text only). Activating
//! declares the embedding identity to the store — that is what binds the
//! search surface, and what pends an in-place re-embed when the identity
//! changed (`design/index-maintenance.md`). Unloading withdraws it, so a
//! provider swap restarts consumers against the new surface.

use std::sync::Arc;

use serde::Deserialize;

use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, STORE,
};
use inseam_seams::embedder::{self, Embedder, EMBEDDER};
use inseam_seams::llm::{Llm, LLM};
use inseam_seams::SeamError;

/// Characters of input text an embedding sees; more adds cost, not recall.
const EMBED_INPUT_CHARS: usize = 6_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Remote embeddings through the `llm` seam's endpoint.
    Endpoint,
    /// Deterministic local bag-of-words hashing: weak but offline and free.
    Hashed,
    /// No vectors at all; discovery degrades to full-text seeding only.
    None,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbedderConfig {
    pub provider: Provider,
    pub model: String,
    pub dimensions: usize,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Endpoint,
            model: "openai/text-embedding-3-small".to_string(),
            dimensions: 1536,
        }
    }
}

pub struct EmbedderPlugin {
    config: EmbedderConfig,
}

impl EmbedderPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        Ok(Self {
            config: parse_config(config)?,
        })
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
        let (implementation, identity): (Arc<dyn Embedder>, (String, usize)) =
            match self.config.provider {
                Provider::Endpoint => {
                    let llm = cx.try_get(&LLM)?.ok_or_else(|| {
                        PluginError(
                            "embedder provider is `endpoint` but no `llm` service is mounted; \
                             set the endpoint API key, or switch the provider to `hashed`"
                                .to_string(),
                        )
                    })?;
                    (
                        Arc::new(EndpointEmbedder {
                            llm,
                            model: self.config.model.clone(),
                            dimensions: self.config.dimensions,
                        }),
                        (self.config.model.clone(), self.config.dimensions),
                    )
                }
                Provider::Hashed => (
                    Arc::new(HashedEmbedder {
                        dimensions: self.config.dimensions,
                    }),
                    (format!("hashed-{}", self.config.dimensions), self.config.dimensions),
                ),
                Provider::None => (Arc::new(DisabledEmbedder), ("none".to_string(), 0)),
            };

        store
            .declare_embedding(&identity.0, identity.1)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let store_for_undo = Arc::clone(&store);
        cx.effect("declare embedding identity", move || {
            store_for_undo.withdraw_embedding();
        });

        let facts = Facts::new()
            .with(embedder::facts::OFFLINE, self.config.provider != Provider::Endpoint)
            .with(embedder::facts::MODEL, identity.0.as_str())
            .with(embedder::facts::DIMENSIONS, identity.1 as u64);
        cx.provide(&EMBEDDER, implementation, facts)?;
        Ok(())
    }
}

struct EndpointEmbedder {
    llm: Arc<dyn Llm>,
    model: String,
    dimensions: usize,
}

#[async_trait::async_trait]
impl Embedder for EndpointEmbedder {
    fn dimensions(&self) -> Option<usize> {
        Some(self.dimensions)
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
        let bounded: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(EMBED_INPUT_CHARS).collect())
            .collect();
        let refs: Vec<&str> = bounded.iter().map(String::as_str).collect();
        let vectors = self.llm.embed(&self.model, &refs).await?;
        if let Some(v) = vectors.first()
            && v.len() != self.dimensions
        {
            return Err(SeamError::failed(format!(
                "model returned {}-dimension vectors, config says {}; fix the config or re-index",
                v.len(),
                self.dimensions
            )));
        }
        Ok(vectors)
    }
}

struct HashedEmbedder {
    dimensions: usize,
}

#[async_trait::async_trait]
impl Embedder for HashedEmbedder {
    fn dimensions(&self) -> Option<usize> {
        Some(self.dimensions)
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
    Arc::new(HashedEmbedder { dimensions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashed_embeddings_are_deterministic_and_normalized() {
        let e = hashed(64);
        let a = e.embed(&["kitchen renovation notes"]).await.expect("embeds");
        let b = e.embed(&["kitchen renovation notes"]).await.expect("embeds");
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
}
