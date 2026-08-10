//! The OpenAI-compatible LLM client: embeddings for the index, chat
//! completions for the LLM transforms (summaries, entities) and the agent
//! demo. Which endpoint it speaks to is configuration (`[endpoint]` in the
//! profile) — OpenRouter is only the default. This is the only module that
//! talks to the network; everything above it takes plain data.

use std::sync::Mutex;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::profile::EndpointConfig;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_KEY_ENV: &str = "OPENROUTER_API_KEY";
const RETRIES: u32 = 3;
/// Inputs per embeddings request; keeps request bodies comfortably bounded.
const EMBED_BATCH: usize = 64;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("{env} is not set; add it to .env or the environment")]
    MissingKey { env: String },
    #[error("llm endpoint request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("llm endpoint returned {status} for {path}: {body}")]
    Api {
        status: u16,
        path: String,
        body: String,
    },
    #[error("llm endpoint response missing expected data: {0}")]
    BadResponse(String),
}

/// API key newtype so the secret never lands in logs via Debug.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// Read the key from the environment variable the endpoint config names.
    pub fn from_env(env: &str) -> Result<Self, LlmError> {
        std::env::var(env)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(Self)
            .ok_or_else(|| LlmError::MissingKey {
                env: env.to_string(),
            })
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

    /// Build the client for the profile's configured endpoint, reading the
    /// key from the environment variable the config names.
    pub fn from_config(config: &EndpointConfig) -> Result<Self, LlmError> {
        Ok(Self::new(
            ApiKey::from_env(&config.api_key_env)?,
            &config.base_url,
        ))
    }

    /// Dollars spent by this client so far, as reported by response usage.
    pub fn spent(&self) -> f64 {
        *self.spent.lock().unwrap_or_else(|e| e.into_inner())
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
    ) -> Result<T, LlmError> {
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
                    last_err = Some(LlmError::Transport(e));
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(resp.json::<T>().await?);
            }
            let body = resp.text().await.unwrap_or_default();
            let err = LlmError::Api {
                status: status.as_u16(),
                path: path.to_string(),
                body: crate::textutil::truncate_chars(&body, 400),
            };
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable {
                return Err(err);
            }
            last_err = Some(err);
        }
        Err(last_err.unwrap_or(LlmError::BadResponse("no attempts made".into())))
    }

    /// Embed texts in order. Batches requests; the output index matches the
    /// input index.
    pub async fn embed(
        &self,
        model: &str,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>, LlmError> {
        let mut out = Vec::with_capacity(inputs.len());
        for batch in inputs.chunks(EMBED_BATCH) {
            let body = serde_json::json!({ "model": model, "input": batch });
            let resp: EmbeddingsResponse = self
                .request_json(reqwest::Method::POST, "/embeddings", Some(&body))
                .await?;
            self.record_cost(resp.usage.as_ref());
            if resp.data.len() != batch.len() {
                return Err(LlmError::BadResponse(format!(
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

    /// One chat completion; returns the assistant message.
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatMessage, LlmError> {
        let body = serde_json::to_value(request)
            .map_err(|e| LlmError::BadResponse(format!("unserializable request: {e}")))?;
        let resp: ChatResponse = self
            .request_json(reqwest::Method::POST, "/chat/completions", Some(&body))
            .await?;
        self.record_cost(resp.usage.as_ref());
        resp.choices
            .into_iter()
            .next()
            .map(|c| c.message)
            .ok_or_else(|| LlmError::BadResponse("no choices in response".into()))
    }

    /// The model catalog: chat models, or embedding models when `embeddings`.
    /// `/models` is common to OpenAI-compatible servers; `/embeddings/models`
    /// and the pricing fields are OpenRouter's — other endpoints may 404 or
    /// omit them.
    pub async fn models(&self, embeddings: bool) -> Result<Vec<ModelInfo>, LlmError> {
        let path = if embeddings {
            "/embeddings/models"
        } else {
            "/models"
        };
        let resp: ModelsResponse = self.request_json(reqwest::Method::GET, path, None).await?;
        Ok(resp.data)
    }
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }

    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, as the OpenAI-style wire format sends them.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionDef,
}

impl Tool {
    pub fn function(name: &str, description: &str, parameters: Value) -> Self {
        Self {
            kind: "function",
            function: FunctionDef {
                name: name.to_string(),
                description: description.to_string(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Ask OpenRouter to report cost in usage so `spent()` stays accurate.
    pub usage: UsageInclude,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: None,
            max_tokens: None,
            usage: UsageInclude { include: true },
        }
    }

    pub fn with_tools(mut self, tools: Vec<Tool>) -> Self {
        self.tools = Some(tools);
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageInclude {
    pub include: bool,
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
pub struct Usage {
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
    #[serde(default)]
    pub supported_parameters: Option<Vec<String>>,
}

impl ModelInfo {
    pub fn supports_tools(&self) -> bool {
        self.supported_parameters
            .as_ref()
            .is_some_and(|p| p.iter().any(|s| s == "tools"))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPricing {
    /// Dollars per prompt token, as a decimal string.
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub completion: Option<String>,
}

impl ModelPricing {
    pub fn prompt_per_million(&self) -> Option<f64> {
        self.prompt.as_deref()?.parse::<f64>().ok().map(|p| p * 1e6)
    }

    pub fn completion_per_million(&self) -> Option<f64> {
        self.completion
            .as_deref()?
            .parse::<f64>()
            .ok()
            .map(|p| p * 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_debug_is_redacted() {
        let key = ApiKey::new("sk-or-v1-supersecret");
        assert_eq!(format!("{key:?}"), "ApiKey(redacted)");
    }

    #[test]
    fn chat_request_serializes_wire_shape() {
        let req = ChatRequest::new("openai/gpt-5-mini", vec![ChatMessage::user("hi")])
            .with_tools(vec![Tool::function(
                "query",
                "search",
                serde_json::json!({"type": "object"}),
            )]);
        let v = serde_json::to_value(&req).expect("serializes");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["usage"]["include"], true);
        assert!(v["messages"][0].get("tool_calls").is_none());
    }

    #[test]
    fn tool_call_arguments_deserialize_from_wire() {
        let msg: ChatMessage = serde_json::from_str(
            r#"{"role":"assistant","tool_calls":[{"id":"c1","type":"function",
                "function":{"name":"query","arguments":"{\"text\":\"reno\"}"}}]}"#,
        )
        .expect("deserializes");
        let calls = msg.tool_calls.expect("has calls");
        assert_eq!(calls[0].function.name, "query");
    }

    #[test]
    fn pricing_converts_to_per_million() {
        let p = ModelPricing {
            prompt: Some("0.00000002".to_string()),
            completion: None,
        };
        let per_m = p.prompt_per_million().expect("parses");
        assert!((per_m - 0.02).abs() < 1e-9);
    }
}
