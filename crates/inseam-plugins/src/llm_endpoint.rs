//! The default `llm` provider: an OpenAI-compatible client against a
//! configured base URL. Which endpoint it speaks to is configuration —
//! OpenRouter is only the default; OpenAI, Ollama, vLLM are config values,
//! not code (`design/plugins.md`, settled). This is the only plugin that
//! talks to the network for LLM work; everything above takes plain data.
//!
//! The models consumers should use ride on the seam as capability facts
//! (`transform_model`, `agent_model`), so consumers never hardcode one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, SecretNeed,
};
use inseam_kernel::text::truncate_chars;
use inseam_seams::llm::{self, ChatMessage, ChatRequest, Llm, ModelInfo, LLM};
use inseam_seams::SeamError;

const RETRIES: u32 = 3;
/// Inputs per embeddings request; keeps request bodies comfortably bounded.
const EMBED_BATCH: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmEndpointConfig {
    pub base_url: String,
    /// Name of the environment variable holding the API key. The key itself
    /// never lives in the composition or the store.
    pub api_key_env: String,
    /// Model for transform-grade calls (summaries, entities, OCR); cheap and
    /// fast wins here. Declared as a capability fact.
    pub transform_model: String,
    /// Model for agent-grade tool-calling loops. Declared as a fact.
    pub agent_model: String,
}

impl Default for LlmEndpointConfig {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            transform_model: "google/gemini-2.5-flash-lite".to_string(),
            agent_model: "openai/gpt-5-mini".to_string(),
        }
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
        // No key, no provider: the fiber fails loudly and consumers that
        // declared `llm` optional keep running without it.
        let key = ApiKey::from_env(&self.config.api_key_env).map_err(|e| PluginError(e))?;
        let client = LlmClient::new(key, &self.config.base_url);
        let facts = Facts::new()
            .with(llm::facts::TRANSFORM_MODEL, self.config.transform_model.as_str())
            .with(llm::facts::AGENT_MODEL, self.config.agent_model.as_str());
        cx.provide(&LLM, Arc::new(client) as Arc<dyn Llm>, facts)?;
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretNeed> {
        vec![SecretNeed {
            env: self.config.api_key_env.clone(),
            purpose: format!(
                "An API key for {} unlocks the language model that powers \
                 search embeddings, summaries, and entity extraction — \
                 indexing and search stay paused without it.",
                endpoint_host(&self.config.base_url)
            ),
        }]
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
    without_scheme.split('/').next().expect("split yields at least one element")
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
    key: ApiKey,
    base_url: String,
    /// Dollars spent across all calls this process, from response usage.
    spent: Mutex<f64>,
}

impl LlmClient {
    pub fn new(key: ApiKey, base_url: impl Into<String>) -> Self {
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
                .bearer_auth(&self.key.0)
                .header("HTTP-Referer", "https://github.com/aiui/inseam")
                .header("X-Title", "inseam");
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

    async fn embed(&self, model: &str, inputs: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
        let mut out = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(EMBED_BATCH) {
            let body = json!({ "model": model, "input": batch });
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
            out.extend(data.into_iter().map(|d| d.embedding));
        }
        Ok(out)
    }

    /// Vision call via OpenAI-compatible content parts: the prompt plus one
    /// image as a data URL.
    async fn describe_image(
        &self,
        model: &str,
        prompt: &str,
        mimetype: &str,
        image: &[u8],
    ) -> Result<String, SeamError> {
        let data_url = format!("data:{mimetype};base64,{}", base64_encode(image));
        let body = json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }],
            "usage": { "include": true }
        });
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
        // `/models` is common to OpenAI-compatible servers;
        // `/embeddings/models` and pricing fields are OpenRouter's — other
        // endpoints may 404 or omit them.
        let path = if embeddings { "/embeddings/models" } else { "/models" };
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
    embedding: Vec<f32>,
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

/// Standard base64, inlined to keep the dependency tree lean.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
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
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
