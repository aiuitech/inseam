//! The default `llm` provider: an OpenAI-compatible client against a
//! configured base URL. Which endpoint it speaks to is configuration —
//! OpenRouter is only the default; OpenAI, Ollama, vLLM are config values,
//! not code (`design/plugins.md`, settled). This is the only plugin that
//! talks to the network for LLM work; everything above takes plain data.
//!
//! The models consumers should use ride on the seam as capability facts
//! (`transform_model`, `agent_model`), so consumers never hardcode one.
//!
//! Embeddings travel as base64 (`encoding_format: "base64"`): one packed
//! little-endian `f32` array per input instead of thousands of decimal
//! literals — less than half the bytes, and a `memcpy`-grade decode instead
//! of float parsing. A server that ignores the parameter still answers in
//! JSON floats, and both shapes are accepted ([`EmbeddingVector`]).

mod ollama;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, SecretNeed,
};
use inseam_seams::llm::{
    self, ChatMessage, ChatRequest, EmbedRequest, EmbeddingModel, Llm, ModelInfo, VisionRequest,
    LLM,
};
use inseam_seams::text::truncate_chars;
use inseam_seams::SeamError;

use ollama::{EmbeddingModelVerdict, OllamaApi};

const RETRIES: u32 = 3;
/// Inputs per embeddings request; keeps request bodies comfortably bounded.
const EMBED_BATCH: usize = 64;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmEndpointConfig {
    pub base_url: String,
    /// Name of the environment variable holding the API key. The key itself
    /// never lives in the composition or the store. Empty means the
    /// endpoint needs no key (a local server such as ollama).
    pub api_key_env: String,
    /// Model for transform-grade calls (summaries, entities, OCR); cheap and
    /// fast wins here. Declared as a capability fact.
    pub transform_model: String,
    /// Reasoning effort for transform-grade calls, as the endpoint spells it
    /// (`none`, `low`, …). Set `none` for a thinking model: left to itself
    /// it spends the reply on thought and returns no summary. Absent means
    /// the parameter is not sent — endpoints reject values their model does
    /// not know, so this is never guessed. Declared as a fact.
    pub transform_reasoning_effort: Option<String>,
    /// Model for agent-grade tool-calling loops. Declared as a fact.
    pub agent_model: String,
}

impl Default for LlmEndpointConfig {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            transform_model: "google/gemini-2.5-flash-lite".to_string(),
            transform_reasoning_effort: None,
            agent_model: "openai/gpt-5-mini".to_string(),
        }
    }
}

impl LlmEndpointConfig {
    /// The environment variable the key is read from, or `None` when the
    /// endpoint is keyless.
    fn api_key_env(&self) -> Option<&str> {
        Some(self.api_key_env.trim()).filter(|env| !env.is_empty())
    }
}

pub struct LlmEndpoint {
    config: LlmEndpointConfig,
}

impl LlmEndpoint {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        Ok(Self {
            config: parse_config(config)?,
        })
    }
}

pub struct LlmEndpointFactory;

impl inseam_kernel::substrate::PluginFactory for LlmEndpointFactory {
    fn name(&self) -> &str {
        "llm-endpoint"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(LlmEndpoint::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for LlmEndpoint {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "llm-endpoint",
            inject: INJECT,
            provides: &["llm"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        // A configured key that is not set is a missing provider: the fiber
        // fails loudly and consumers that declared `llm` optional keep
        // running without it. A keyless endpoint has nothing to fail on.
        let key = self
            .config
            .api_key_env()
            .map(ApiKey::from_env)
            .transpose()
            .map_err(PluginError)?;
        let client = LlmClient::new(key, &self.config.base_url);
        let mut facts = Facts::new()
            .with(llm::facts::TRANSFORM_MODEL, self.config.transform_model.as_str())
            .with(llm::facts::AGENT_MODEL, self.config.agent_model.as_str());
        if let Some(effort) = self.config.transform_reasoning_effort.as_deref() {
            facts = facts.with(llm::facts::TRANSFORM_REASONING_EFFORT, effort);
        }
        cx.provide(&LLM, Arc::new(client) as Arc<dyn Llm>, facts)?;
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretNeed> {
        self.config
            .api_key_env()
            .map(|env| SecretNeed {
                env: env.to_string(),
                purpose: format!(
                    "An API key for {} unlocks the language model that powers \
                     search embeddings, summaries, and entity extraction — \
                     indexing and search stay paused without it.",
                    endpoint_host(&self.config.base_url)
                ),
            })
            .into_iter()
            .collect()
    }
}

/// The host part of the endpoint URL, for owner-facing prose — the scheme
/// and path would only add noise to a settings screen.
fn endpoint_host(base_url: &str) -> &str {
    let without_scheme = match base_url.split_once("://") {
        Some((_, rest)) => rest,
        None => base_url,
    };
    // Provably infallible: split always yields at least one element.
    #[allow(clippy::expect_used)]
    without_scheme
        .split('/')
        .next()
        .expect("split yields at least one element")
}

/// API key newtype so the secret never lands in logs via Debug.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Read the key from the environment variable the config names.
    pub fn from_env(env: &str) -> Result<Self, String> {
        std::env::var(env)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(Self)
            .ok_or_else(|| format!("{env} is not set; add it to the environment"))
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(redacted)")
    }
}

pub struct LlmClient {
    http: reqwest::Client,
    /// `None` for a keyless endpoint: no `Authorization` header is sent.
    key: Option<ApiKey>,
    base_url: String,
    /// Dollars spent across all calls this process, from response usage.
    spent: Mutex<f64>,
}

impl LlmClient {
    pub fn new(key: Option<ApiKey>, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .expect("reqwest client with static config builds");
        Self {
            http,
            key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            spent: Mutex::new(0.0),
        }
    }

    fn record_cost(&self, usage: Option<&Usage>) {
        if let Some(cost) = usage.and_then(|u| u.cost) {
            *self.spent.lock().unwrap_or_else(|e| e.into_inner()) += cost;
        }
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, SeamError> {
        let url = format!("{}{path}", self.base_url);
        let mut last_err = None;
        for attempt in 0..RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header("HTTP-Referer", "https://github.com/aiui/inseam")
                .header("X-Title", "inseam");
            if let Some(key) = &self.key {
                req = req.bearer_auth(&key.0);
            }
            if let Some(body) = body {
                req = req.json(body);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    last_err = Some(SeamError::failed(format!("llm transport: {e}")));
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                return resp
                    .json::<T>()
                    .await
                    .map_err(|e| SeamError::failed(format!("llm response: {e}")));
            }
            let body = resp.text().await.unwrap_or_default();
            let err = SeamError::failed(format!(
                "llm endpoint returned {} for {path}: {}",
                status.as_u16(),
                truncate_chars(&body, 400)
            ));
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable {
                return Err(err);
            }
            last_err = Some(err);
        }
        Err(last_err.unwrap_or_else(|| SeamError::failed("no attempts made")))
    }
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Llm for LlmClient {
    async fn chat(&self, request: &ChatRequest) -> Result<ChatMessage, SeamError> {
        let body = serde_json::to_value(request)
            .map_err(|e| SeamError::failed(format!("unserializable request: {e}")))?;
        let resp: ChatResponse = self
            .request_json(reqwest::Method::POST, "/chat/completions", Some(&body))
            .await?;
        self.record_cost(resp.usage.as_ref());
        resp.choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| SeamError::failed("no choices in response"))
    }

    async fn embed(&self, request: &EmbedRequest<'_>) -> Result<Vec<Vec<f32>>, SeamError> {
        let mut out = Vec::with_capacity(request.inputs.len());
        for batch in request.inputs.chunks(EMBED_BATCH) {
            let mut body = json!({
                "model": request.model,
                "input": batch,
                "encoding_format": "base64",
            });
            if let Some(dimensions) = request.dimensions {
                body["dimensions"] = json!(dimensions);
            }
            let resp: EmbeddingsResponse = self
                .request_json(reqwest::Method::POST, "/embeddings", Some(&body))
                .await?;
            self.record_cost(resp.usage.as_ref());
            if resp.data.len() != batch.len() {
                return Err(SeamError::failed(format!(
                    "asked for {} embeddings, got {}",
                    batch.len(),
                    resp.data.len()
                )));
            }
            let mut data = resp.data;
            data.sort_by_key(|d| d.index);
            for datum in data {
                out.push(datum.embedding.into_vector()?);
            }
        }
        Ok(out)
    }

    async fn embedding_model(&self, model: &str) -> Result<Option<EmbeddingModel>, SeamError> {
        match OllamaApi::new(&self.http, &self.base_url)
            .embedding_model(model)
            .await
        {
            None => Ok(None),
            Some(EmbeddingModelVerdict::Embedder(info)) => Ok(Some(info)),
            Some(EmbeddingModelVerdict::NotAnEmbedder) => Err(SeamError::failed(format!(
                "`{model}` is a chat model, not an embedding model; pick one that \
                 `inseam models --embeddings` lists"
            ))),
        }
    }

    /// Vision call via OpenAI-compatible content parts: the prompt plus one
    /// image as a data URL.
    async fn describe_image(&self, request: &VisionRequest<'_>) -> Result<String, SeamError> {
        let data_url = format!(
            "data:{};base64,{}",
            request.mimetype,
            STANDARD.encode(request.image)
        );
        let mut body = json!({
            "model": request.model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": request.prompt },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }],
            "usage": { "include": true }
        });
        if let Some(effort) = request.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        let resp: ChatResponse = self
            .request_json(reqwest::Method::POST, "/chat/completions", Some(&body))
            .await?;
        self.record_cost(resp.usage.as_ref());
        resp.choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| SeamError::failed("no content in vision response"))
    }

    async fn models(&self, embeddings: bool) -> Result<Vec<ModelInfo>, SeamError> {
        // A local ollama introspects its installed models (capabilities,
        // vector widths) through its native API; `/v1/models` alone cannot
        // tell an embedder from a chat model.
        if let Some(models) = OllamaApi::new(&self.http, &self.base_url)
            .models(embeddings)
            .await
        {
            return Ok(models);
        }
        // `/models` is common to OpenAI-compatible servers;
        // `/embeddings/models` and pricing fields are OpenRouter's — other
        // endpoints may 404 or omit them.
        let path = if embeddings {
            "/embeddings/models"
        } else {
            "/models"
        };
        let resp: ModelsResponse = self.request_json(reqwest::Method::GET, path, None).await?;
        Ok(resp.data)
    }

    fn spent(&self) -> f64 {
        *self.spent.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: usize,
    embedding: EmbeddingVector,
}

/// An embedding as the wire carries it: packed base64 when the server
/// honored `encoding_format`, a JSON float array when it did not.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EmbeddingVector {
    Base64(String),
    Floats(Vec<f32>),
}

impl EmbeddingVector {
    fn into_vector(self) -> Result<Vec<f32>, SeamError> {
        match self {
            Self::Floats(v) => Ok(v),
            Self::Base64(text) => decode_f32_base64(&text),
        }
    }
}

/// Decode a base64 string of little-endian IEEE-754 `f32`s — the OpenAI
/// embeddings `base64` encoding.
fn decode_f32_base64(text: &str) -> Result<Vec<f32>, SeamError> {
    let bytes = STANDARD
        .decode(text)
        .map_err(|e| SeamError::failed(format!("embedding base64: {e}")))?;
    if bytes.len() % 4 != 0 {
        return Err(SeamError::failed(format!(
            "embedding base64 decodes to {} bytes, not a whole number of f32s",
            bytes.len()
        )));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_declare_the_configured_env_with_a_host_purpose() {
        let plugin = LlmEndpoint::from_config(&toml::Table::new()).unwrap();
        let needs = plugin.secrets();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].env, "OPENROUTER_API_KEY");
        assert!(needs[0].purpose.contains("openrouter.ai"));
        assert!(!needs[0].purpose.contains("https://"));
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let key = ApiKey::new("sk-or-v1-supersecret");
        assert_eq!(format!("{key:?}"), "ApiKey(redacted)");
    }

    #[test]
    fn a_keyless_endpoint_declares_no_secret_and_needs_no_environment() {
        let mut config = toml::Table::new();
        config.insert("api_key_env".into(), toml::Value::String(String::new()));
        config.insert(
            "base_url".into(),
            toml::Value::String("http://localhost:11434/v1".into()),
        );
        let plugin = LlmEndpoint::from_config(&config).unwrap();
        assert!(plugin.secrets().is_empty());
        assert_eq!(plugin.config.api_key_env(), None);
    }

    #[test]
    fn embedding_vectors_decode_from_base64_and_floats_alike() {
        let floats = vec![1.0f32, -0.5, 0.25];
        let bytes: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
        let encoded = STANDARD.encode(&bytes);
        let datum: EmbeddingDatum = serde_json::from_value(json!({
            "index": 0, "embedding": encoded
        }))
        .expect("deserializes");
        assert_eq!(datum.embedding.into_vector().unwrap(), floats);
        let datum: EmbeddingDatum = serde_json::from_value(json!({
            "index": 1, "embedding": [1.0, -0.5, 0.25]
        }))
        .expect("deserializes");
        assert_eq!(datum.embedding.into_vector().unwrap(), floats);
    }

    #[test]
    fn a_torn_base64_embedding_is_an_error() {
        let encoded = STANDARD.encode([0u8, 1, 2]);
        assert!(decode_f32_base64(&encoded).is_err());
    }
}
