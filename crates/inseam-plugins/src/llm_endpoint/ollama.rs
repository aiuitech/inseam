//! Ollama's native API, used only for **discovery**. Ollama's
//! OpenAI-compatible surface (`/v1/models`) lists installed model ids and
//! nothing else — not whether a model embeds or chats, not its vector width.
//! Its native API does: `/api/tags` enumerates what is installed and
//! `/api/show` reports each model's capabilities and, for embedders, the
//! architecture's `embedding_length`. Every call here is best-effort: any
//! failure reads as "not ollama", and the OpenAI-compatible path stands.

use serde::Deserialize;
use serde_json::{json, Value};

use inseam_seams::llm::{EmbeddingModel, ModelInfo};

/// Installed models ollama will introspect per `inseam models`; one
/// `/api/show` each, all local, so the bound is about runtime, not bytes.
const MODELS_SHOWN_MAX: usize = 200;

/// The origin an OpenAI-compatible base URL sits on, for the native API
/// beside it: `http://localhost:11434/v1` → `http://localhost:11434`.
pub(super) fn origin_of(base_url: &str) -> String {
    base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string()
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<Tag>,
}

#[derive(Debug, Deserialize)]
struct Tag {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ShowResponse {
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    model_info: serde_json::Map<String, Value>,
}

impl ShowResponse {
    fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// `<architecture>.embedding_length` — the model's hidden width, which
    /// is the vector width for an embedding model. The vision tower reports
    /// its own `vision.embedding_length`; that one is not a vector width.
    fn embedding_length(&self) -> Option<usize> {
        self.model_info
            .iter()
            .filter(|(key, _)| key.ends_with(".embedding_length"))
            .filter(|(key, _)| !key.contains(".vision."))
            .find_map(|(_, value)| value.as_u64())
            .and_then(|n| usize::try_from(n).ok())
    }

    fn context_length(&self) -> Option<u64> {
        self.model_info
            .iter()
            .filter(|(key, _)| key.ends_with(".context_length"))
            .filter(|(key, _)| !key.contains(".vision."))
            .find_map(|(_, value)| value.as_u64())
    }
}

/// Best-effort native-API client over the plugin's HTTP client.
pub(super) struct OllamaApi<'a> {
    http: &'a reqwest::Client,
    origin: String,
}

impl<'a> OllamaApi<'a> {
    pub(super) fn new(http: &'a reqwest::Client, base_url: &str) -> Self {
        Self {
            http,
            origin: origin_of(base_url),
        }
    }

    /// Installed model names, or `None` when the origin is not ollama.
    async fn tags(&self) -> Option<Vec<String>> {
        let resp = self
            .http
            .get(format!("{}/api/tags", self.origin))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let tags: TagsResponse = resp.json().await.ok()?;
        Some(tags.models.into_iter().map(|t| t.name).collect())
    }

    async fn show(&self, model: &str) -> Option<ShowResponse> {
        let resp = self
            .http
            .post(format!("{}/api/show", self.origin))
            .json(&json!({ "model": model }))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// What ollama knows about one embedding model: its width, if the model
    /// is installed and declares the `embedding` capability. Ollama
    /// truncates any model's vectors to a requested `dimensions`, which
    /// is only meaningful for Matryoshka-trained models — the embedder's
    /// catalog knows those; here nothing is reported reducible.
    pub(super) async fn embedding_model(&self, model: &str) -> Option<EmbeddingModelVerdict> {
        let shown = self.show(model).await?;
        if !shown.can("embedding") {
            return Some(EmbeddingModelVerdict::NotAnEmbedder);
        }
        let dimensions = shown.embedding_length()?;
        Some(EmbeddingModelVerdict::Embedder(EmbeddingModel {
            dimensions,
            reducible: false,
        }))
    }

    /// The installed catalog, introspected: embedding models when
    /// `embeddings`, chat models otherwise (tool-capable ones flagged the
    /// way OpenRouter's catalog flags them, so one filter serves both).
    /// `None` when the origin is not ollama.
    pub(super) async fn models(&self, embeddings: bool) -> Option<Vec<ModelInfo>> {
        let names = self.tags().await?;
        let mut out = Vec::with_capacity(names.len().min(MODELS_SHOWN_MAX));
        for name in names.into_iter().take(MODELS_SHOWN_MAX) {
            let Some(shown) = self.show(&name).await else {
                continue;
            };
            let wanted = if embeddings {
                shown.can("embedding")
            } else {
                shown.can("completion")
            };
            if !wanted {
                continue;
            }
            let supported_parameters = if shown.can("tools") {
                Some(vec!["tools".to_string()])
            } else {
                Some(Vec::new())
            };
            out.push(ModelInfo {
                id: name,
                name: None,
                context_length: shown.context_length(),
                pricing: None,
                supported_parameters,
                embedding_dimensions: if embeddings {
                    shown.embedding_length()
                } else {
                    None
                },
            });
        }
        Some(out)
    }
}

/// What `/api/show` said about a model asked to embed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EmbeddingModelVerdict {
    Embedder(EmbeddingModel),
    /// Installed, but a chat/vision model: embedding with it is a config
    /// error worth naming, not a silent width mismatch later.
    NotAnEmbedder,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_strips_the_openai_path() {
        assert_eq!(
            origin_of("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            origin_of("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
        assert_eq!(
            origin_of("http://localhost:11434"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn show_reads_the_text_towers_width_not_the_vision_towers() {
        let shown: ShowResponse = serde_json::from_value(json!({
            "capabilities": ["completion", "vision", "thinking"],
            "model_info": {
                "qwen35.context_length": 262144,
                "qwen35.embedding_length": 4096,
                "qwen35.vision.embedding_length": 1152
            }
        }))
        .expect("deserializes");
        assert_eq!(shown.embedding_length(), Some(4096));
        assert_eq!(shown.context_length(), Some(262144));
        assert!(shown.can("thinking"));
        assert!(!shown.can("embedding"));
    }

    #[test]
    fn show_of_an_embedder_reports_its_width() {
        let shown: ShowResponse = serde_json::from_value(json!({
            "capabilities": ["embedding"],
            "model_info": { "bert.context_length": 512, "bert.embedding_length": 384 }
        }))
        .expect("deserializes");
        assert!(shown.can("embedding"));
        assert_eq!(shown.embedding_length(), Some(384));
    }
}
