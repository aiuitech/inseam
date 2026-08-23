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

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex as AsyncMutex};

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
/// Inputs per embeddings request. The sweep may hand us more or fewer
/// vector-covered rows; this is the endpoint-sized network batch.
const EMBED_BATCH: usize = 128;
/// Concurrent batch-only chat calls coalesced into one OpenRouter job.
const CHAT_BATCH_MAX: usize = 128;
/// Pending batch calls waiting behind the active OpenRouter job.
const CHAT_BATCH_QUEUE_MAX: usize = 1_024;
/// Brief collection window so concurrently planned sources share a job.
const CHAT_BATCH_COLLECTION_MS: u64 = 50;
/// OpenRouter batch status polling cadence and 24-hour hard bound.
const CHAT_BATCH_POLL_SECONDS: u64 = 5;
const CHAT_BATCH_POLL_MAX: u32 = 17_280;
/// Bounds one leader's drain loop even if callers continuously refill it.
const CHAT_BATCH_DRAIN_MAX: u32 = 1_000_000;

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
            .with(
                llm::facts::TRANSFORM_MODEL,
                self.config.transform_model.as_str(),
            )
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

/// Scheme and authority of a configured endpoint URL.
fn endpoint_origin(base_url: &str) -> &str {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return base_url.trim_end_matches('/');
    };
    let authority_length = rest.find('/').unwrap_or(rest.len());
    let origin_length = scheme.len() + 3 + authority_length;
    assert!(origin_length <= base_url.len());
    &base_url[..origin_length]
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
    batch_queue: AsyncMutex<ChatBatchQueue>,
}

#[derive(Default)]
struct ChatBatchQueue {
    pending: VecDeque<PendingBatchChat>,
    draining: bool,
}

struct PendingBatchChat {
    request: ChatRequest,
    response: oneshot::Sender<Result<ChatMessage, SeamError>>,
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
            batch_queue: AsyncMutex::new(ChatBatchQueue::default()),
        }
    }

    fn record_cost(&self, usage: Option<&Usage>) {
        if let Some(cost) = usage.and_then(|u| u.cost) {
            *self.spent.lock().unwrap_or_else(|e| e.into_inner()) += cost;
        }
    }

    /// Provider-specific reasoning wire shape. OpenRouter takes a nested
    /// object and can omit the unused trace; other compatible endpoints
    /// (including ollama) retain the top-level `reasoning_effort` field.
    fn chat_body(&self, request: &ChatRequest) -> Result<Value, SeamError> {
        let mut body = serde_json::to_value(request)
            .map_err(|e| SeamError::failed(format!("unserializable request: {e}")))?;
        apply_reasoning_wire(
            &mut body,
            &self.base_url,
            request.reasoning_effort.as_deref(),
        );
        Ok(body)
    }

    async fn chat_via_batch(&self, request: &ChatRequest) -> Result<ChatMessage, SeamError> {
        let (sender, receiver) = oneshot::channel();
        let leads_drain = {
            let mut queue = self.batch_queue.lock().await;
            if queue.pending.len() >= CHAT_BATCH_QUEUE_MAX {
                return Err(SeamError::failed("OpenRouter chat batch queue is full"));
            }
            queue.pending.push_back(PendingBatchChat {
                request: request.clone(),
                response: sender,
            });
            let leads = !queue.draining;
            queue.draining = true;
            leads
        };
        if leads_drain {
            self.drain_chat_batches().await;
        }
        receiver
            .await
            .map_err(|_| SeamError::failed("OpenRouter chat batch stopped before replying"))?
    }

    async fn drain_chat_batches(&self) {
        for _batch_index in 0..CHAT_BATCH_DRAIN_MAX {
            tokio::time::sleep(Duration::from_millis(CHAT_BATCH_COLLECTION_MS)).await;
            let pending = self.take_chat_batch().await;
            if pending.is_empty() {
                return;
            }
            let outcomes = self.submit_chat_batch(&pending).await;
            deliver_chat_batch(pending, outcomes);
        }
        self.fail_pending_chat_batches("OpenRouter chat batch drain limit reached")
            .await;
    }

    async fn take_chat_batch(&self) -> Vec<PendingBatchChat> {
        let mut queue = self.batch_queue.lock().await;
        let Some(first) = queue.pending.front() else {
            queue.draining = false;
            return Vec::new();
        };
        let model = first.request.model.clone();
        let count = queue
            .pending
            .iter()
            .take(CHAT_BATCH_MAX)
            .take_while(|pending| pending.request.model == model)
            .count();
        assert!(count > 0);
        queue.pending.drain(..count).collect()
    }

    async fn fail_pending_chat_batches(&self, reason: &str) {
        let mut queue = self.batch_queue.lock().await;
        let pending = std::mem::take(&mut queue.pending);
        queue.draining = false;
        drop(queue);
        for item in pending {
            let _ = item.response.send(Err(SeamError::failed(reason)));
        }
    }

    async fn submit_chat_batch(
        &self,
        pending: &[PendingBatchChat],
    ) -> Result<Vec<ChatMessage>, SeamError> {
        assert!(!pending.is_empty());
        assert!(pending.len() <= CHAT_BATCH_MAX);
        let model = batch_base_model(&pending[0].request.model)
            .expect("batch dispatch only receives :batch models");
        let requests = self.chat_batch_requests(pending, model)?;
        let body = json!({
            "endpoint": "/v1/chat/completions",
            "model": model,
            "requests": requests,
        });
        let url = format!("{}/api/beta/batches", endpoint_origin(&self.base_url));
        let created: OpenRouterBatch = self
            .request_url_json(
                reqwest::Method::POST,
                &url,
                "creating OpenRouter batch",
                Some(&body),
            )
            .await?;
        let completed = self.poll_chat_batch(&url, &created.id).await?;
        self.record_cost(completed.usage.as_ref());
        self.chat_batch_messages(completed, pending.len())
    }

    fn chat_batch_requests(
        &self,
        pending: &[PendingBatchChat],
        model: &str,
    ) -> Result<Vec<Value>, SeamError> {
        pending
            .iter()
            .enumerate()
            .map(|(index, pending)| {
                let mut body = self.chat_body(&pending.request)?;
                body["model"] = json!(model);
                Ok(json!({"custom_id": format!("inseam-{index}"), "body": body}))
            })
            .collect()
    }

    async fn poll_chat_batch(
        &self,
        collection_url: &str,
        batch_id: &str,
    ) -> Result<OpenRouterBatch, SeamError> {
        let url = format!("{collection_url}/{batch_id}");
        for _poll_index in 0..CHAT_BATCH_POLL_MAX {
            tokio::time::sleep(Duration::from_secs(CHAT_BATCH_POLL_SECONDS)).await;
            let batch: OpenRouterBatch = self
                .request_url_json(reqwest::Method::GET, &url, "polling OpenRouter batch", None)
                .await?;
            match batch.status.as_str() {
                "completed" => return Ok(batch),
                "failed" | "cancelled" | "expired" => {
                    return Err(SeamError::failed(format!(
                        "OpenRouter batch {}: {}",
                        batch.status,
                        batch.error.unwrap_or(Value::Null)
                    )));
                }
                "validating" | "in_progress" | "finalizing" => {}
                status => {
                    return Err(SeamError::failed(format!(
                        "OpenRouter batch returned unknown status `{status}`"
                    )));
                }
            }
        }
        Err(SeamError::failed(
            "OpenRouter batch exceeded its 24-hour poll limit",
        ))
    }

    fn chat_batch_messages(
        &self,
        batch: OpenRouterBatch,
        expected_count: usize,
    ) -> Result<Vec<ChatMessage>, SeamError> {
        let results = batch
            .results
            .ok_or_else(|| SeamError::failed("completed OpenRouter batch has no results"))?;
        if results.len() != expected_count {
            return Err(SeamError::failed(format!(
                "OpenRouter batch returned {} of {expected_count} results",
                results.len()
            )));
        }
        reorder_chat_batch_results(results, expected_count)
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, SeamError> {
        let url = format!("{}{path}", self.base_url);
        self.request_url_json(method, &url, path, body).await
    }

    async fn request_url_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: &str,
        operation: &str,
        body: Option<&Value>,
    ) -> Result<T, SeamError> {
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
                "llm endpoint returned {} for {operation}: {}",
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
        if endpoint_host(&self.base_url) == "openrouter.ai" {
            if batch_base_model(&request.model).is_some() {
                return self.chat_via_batch(request).await;
            }
        }
        let body = self.chat_body(request)?;
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
        apply_reasoning_wire(&mut body, &self.base_url, request.reasoning_effort);
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

/// Translate the transform's semantic reasoning effort to the endpoint's
/// spelling. Reasoning traces are never consumed by transforms, so asking
/// OpenRouter to exclude them saves response bytes without changing effort.
fn apply_reasoning_wire(body: &mut Value, base_url: &str, effort: Option<&str>) {
    let Some(effort) = effort else {
        return;
    };
    if endpoint_host(base_url) == "openrouter.ai" {
        let object = body
            .as_object_mut()
            .expect("a serialized chat request is an object");
        object.remove("reasoning_effort");
        object.insert(
            "reasoning".to_string(),
            json!({"effort": effort, "exclude": true}),
        );
    } else {
        body["reasoning_effort"] = json!(effort);
    }
}

fn batch_base_model(model: &str) -> Option<&str> {
    model.strip_suffix(":batch").filter(|base| !base.is_empty())
}

fn deliver_chat_batch(
    pending: Vec<PendingBatchChat>,
    outcomes: Result<Vec<ChatMessage>, SeamError>,
) {
    match outcomes {
        Ok(messages) => {
            assert_eq!(messages.len(), pending.len());
            for (item, message) in pending.into_iter().zip(messages) {
                let _ = item.response.send(Ok(message));
            }
        }
        Err(error) => {
            let reason = error.to_string();
            for item in pending {
                let _ = item.response.send(Err(SeamError::failed(reason.clone())));
            }
        }
    }
}

fn reorder_chat_batch_results(
    results: Vec<OpenRouterBatchResult>,
    expected_count: usize,
) -> Result<Vec<ChatMessage>, SeamError> {
    let mut ordered: Vec<Option<ChatMessage>> = (0..expected_count).map(|_| None).collect();
    for result in results {
        let index = batch_result_index(&result.custom_id, expected_count)?;
        if ordered[index].is_some() {
            return Err(SeamError::failed(format!(
                "OpenRouter batch repeated `{}`",
                result.custom_id
            )));
        }
        let response = result.response.ok_or_else(|| {
            SeamError::failed(format!(
                "OpenRouter batch `{}` failed: {}",
                result.custom_id,
                result.error.unwrap_or(Value::Null)
            ))
        })?;
        if !(200..300).contains(&response.status_code) {
            return Err(SeamError::failed(format!(
                "OpenRouter batch `{}` returned status {}",
                result.custom_id, response.status_code
            )));
        }
        ordered[index] = response
            .body
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message);
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, message)| {
            message.ok_or_else(|| {
                SeamError::failed(format!("OpenRouter batch omitted inseam-{index}"))
            })
        })
        .collect()
}

fn batch_result_index(custom_id: &str, expected_count: usize) -> Result<usize, SeamError> {
    let index = custom_id
        .strip_prefix("inseam-")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| SeamError::failed(format!("invalid OpenRouter batch id `{custom_id}`")))?;
    if index >= expected_count {
        return Err(SeamError::failed(format!(
            "OpenRouter batch id `{custom_id}` is out of range"
        )));
    }
    Ok(index)
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterBatch {
    id: String,
    status: String,
    #[serde(default)]
    results: Option<Vec<OpenRouterBatchResult>>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterBatchResult {
    custom_id: String,
    #[serde(default)]
    response: Option<OpenRouterBatchItemResponse>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterBatchItemResponse {
    status_code: u16,
    body: ChatResponse,
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

    #[test]
    fn openrouter_uses_low_effort_without_returning_a_reasoning_trace() {
        let client = LlmClient::new(None, "https://openrouter.ai/api/v1");
        let request = ChatRequest::new("google/gemini-2.5-flash-lite:batch", vec![])
            .with_reasoning_effort(Some("low"));

        let body = client.chat_body(&request).unwrap();

        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["reasoning"]["exclude"], true);
    }

    #[test]
    fn ollama_keeps_its_top_level_reasoning_effort() {
        let client = LlmClient::new(None, "http://localhost:11434/v1");
        let request = ChatRequest::new("qwen3.5:9b", vec![]).with_reasoning_effort(Some("none"));

        let body = client.chat_body(&request).unwrap();

        assert_eq!(body["reasoning_effort"], "none");
        assert!(body.get("reasoning").is_none());
    }
}
