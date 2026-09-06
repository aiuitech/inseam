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
//!
//! Chat calls for a `:batch` model ride the batch lane ([`batch`]): parked
//! and submitted together through the endpoint's batch API, for the large,
//! time-insensitive indexing run (`design/indexing.md`).

mod batch;
mod ollama;

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
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

use batch::{ChatBatcher, ChatBatching};
use ollama::{EmbeddingModelVerdict, OllamaApi};

const RETRIES: u32 = 3;
/// Inputs per embeddings request. The sweep may hand us more or fewer
/// vector-covered rows; this is the endpoint-sized network batch.
const EMBED_BATCH: usize = 128;
/// One synchronous request's deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
/// A batch job's serialized requests, the upper bound per job. A summary
/// request is under 10 KB, so the request cap is reached first on summary
/// work; this bounds the one upload when requests are larger.
const CHAT_BATCH_BYTES_MAX: usize = 64 * 1024 * 1024;
/// Arrivals quiet for this long submit what is parked: the run's planners
/// park within moments of each other, and a job waits minutes anyway.
const CHAT_BATCH_QUIESCENCE: Duration = Duration::from_secs(2);
/// The oldest parked call waits at most this long, however steady the
/// trickle of new arrivals (planners refilling as landed sources free
/// slots); by then what is parked is worth a job of its own.
const CHAT_BATCH_AGE_MAX: Duration = Duration::from_secs(120);
const CHAT_BATCH_TICK: Duration = Duration::from_millis(250);
/// Status polls: every five seconds for the first minute, then every thirty,
/// bounded at the provider's 24-hour completion window.
const CHAT_BATCH_POLL_INITIAL: Duration = Duration::from_secs(5);
const CHAT_BATCH_POLL_STEADY: Duration = Duration::from_secs(30);
const CHAT_BATCH_POLL_STEADY_AFTER: u32 = 12;
const CHAT_BATCH_POLL_MAX: u32 = 2_900;
/// Jobs in flight at once per endpoint.
const CHAT_BATCH_JOBS_IN_FLIGHT_MAX: usize = 8;
/// Parked calls the lane holds; beyond it a call is refused.
const CHAT_BATCH_QUEUE_MAX: usize = 65_536;
/// One job-creation upload's deadline: tens of megabytes on a slow uplink.
const CHAT_BATCH_CREATE_TIMEOUT: Duration = Duration::from_secs(900);
/// OpenRouter rejects a job containing more requests than this with 413.
const OPENROUTER_BATCH_REQUESTS_MAX: u32 = 5_000;

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
    /// The batch-API collection URL (`POST` creates a job, `GET <url>/<id>`
    /// polls it). Unset derives it from the endpoint: OpenRouter's
    /// `/api/beta/batches`; other endpoints have no batch lane and the
    /// `transform_batch_model` fact is not declared.
    pub batches_url: Option<String>,
    /// The model the batch lane names — declared as the
    /// `transform_batch_model` fact. Unset derives `<transform_model>:batch`,
    /// OpenRouter's batch variant of the same model.
    pub transform_batch_model: Option<String>,
    /// Requests per batch-API job, the upper bound. OpenRouter accepts at
    /// most 5,000; another compatible batch endpoint may set its own bound.
    pub batch_requests_max: NonZeroU32,
}

impl Default for LlmEndpointConfig {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            transform_model: "google/gemini-2.5-flash-lite".to_string(),
            transform_reasoning_effort: None,
            agent_model: "openai/gpt-5-mini".to_string(),
            batches_url: None,
            transform_batch_model: None,
            batch_requests_max: NonZeroU32::new(OPENROUTER_BATCH_REQUESTS_MAX)
                .expect("5000 is non-zero"),
        }
    }
}

impl LlmEndpointConfig {
    /// The environment variable the key is read from, or `None` when the
    /// endpoint is keyless.
    fn api_key_env(&self) -> Option<&str> {
        Some(self.api_key_env.trim()).filter(|env| !env.is_empty())
    }

    /// The batch-API collection URL: configured, derived for OpenRouter, or
    /// none.
    fn batches_url(&self) -> Option<String> {
        if let Some(url) = self.batches_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            return Some(url.trim_end_matches('/').to_string());
        }
        if endpoint_host(&self.base_url) == "openrouter.ai" {
            return Some(format!("{}/api/beta/batches", endpoint_origin(&self.base_url)));
        }
        None
    }

    /// The batch lane's model: configured, or the transform model's `:batch`
    /// variant (itself, when it already is one).
    fn transform_batch_model(&self) -> String {
        if let Some(model) = self.transform_batch_model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            return model.to_string();
        }
        match batch_base_model(&self.transform_model) {
            Some(_) => self.transform_model.clone(),
            None => format!("{}:batch", self.transform_model),
        }
    }

    /// The batch lane's dials, when the endpoint has one.
    fn batching(&self) -> Option<ChatBatching> {
        let batches_url = self.batches_url()?;
        if endpoint_host(&self.base_url) == "openrouter.ai" {
            assert!(self.batch_requests_max.get() <= OPENROUTER_BATCH_REQUESTS_MAX);
        }
        let requests_max = usize::try_from(self.batch_requests_max.get()).expect("u32 fits usize");
        Some(ChatBatching {
            batches_url,
            requests_max,
            bytes_max: CHAT_BATCH_BYTES_MAX,
            quiescence: CHAT_BATCH_QUIESCENCE,
            age_max: CHAT_BATCH_AGE_MAX,
            tick: CHAT_BATCH_TICK,
            poll_initial: CHAT_BATCH_POLL_INITIAL,
            poll_steady: CHAT_BATCH_POLL_STEADY,
            poll_steady_after: CHAT_BATCH_POLL_STEADY_AFTER,
            poll_max: CHAT_BATCH_POLL_MAX,
            jobs_in_flight_max: CHAT_BATCH_JOBS_IN_FLIGHT_MAX,
            queue_max: CHAT_BATCH_QUEUE_MAX.max(requests_max),
            create_timeout: CHAT_BATCH_CREATE_TIMEOUT,
        })
    }
}

pub struct LlmEndpoint {
    config: LlmEndpointConfig,
}

impl LlmEndpoint {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: LlmEndpointConfig = parse_config(config)?;
        validate_batch_requests(&config).map_err(PluginError)?;
        Ok(Self { config })
    }
}

fn validate_batch_requests(config: &LlmEndpointConfig) -> Result<(), String> {
    if endpoint_host(&config.base_url) != "openrouter.ai" {
        return Ok(());
    }
    if config.batch_requests_max.get() <= OPENROUTER_BATCH_REQUESTS_MAX {
        return Ok(());
    }
    Err(format!(
        "llm.batch_requests_max must not exceed {OPENROUTER_BATCH_REQUESTS_MAX} for OpenRouter"
    ))
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
        let batching = self.config.batching();
        let client = LlmClient::with_batching(key, &self.config.base_url, batching.clone());
        let mut facts = Facts::new()
            .with(
                llm::facts::TRANSFORM_MODEL,
                self.config.transform_model.as_str(),
            )
            .with(llm::facts::AGENT_MODEL, self.config.agent_model.as_str());
        if let Some(effort) = self.config.transform_reasoning_effort.as_deref() {
            facts = facts.with(llm::facts::TRANSFORM_REASONING_EFFORT, effort);
        }
        if batching.is_some() {
            facts = facts.with(
                llm::facts::TRANSFORM_BATCH_MODEL,
                self.config.transform_batch_model().as_str(),
            );
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

/// The wire: one HTTP client, the key, the base URL, and the running
/// tallies. Shared by the synchronous paths and the batch lane's tasks.
pub(crate) struct Transport {
    http: reqwest::Client,
    /// `None` for a keyless endpoint: no `Authorization` header is sent.
    key: Option<ApiKey>,
    base_url: String,
    /// Dollars spent across all calls this process, from response usage.
    spent: Mutex<f64>,
    /// Batch-API jobs created this process.
    batch_jobs: AtomicU64,
}

impl Transport {
    fn record_cost(&self, usage: Option<&Usage>) {
        if let Some(cost) = usage.and_then(|u| u.cost) {
            *self.spent.lock().unwrap_or_else(|e| e.into_inner()) += cost;
        }
    }

    fn record_batch_job(&self) {
        self.batch_jobs.fetch_add(1, Ordering::Relaxed);
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, SeamError> {
        let url = format!("{}{path}", self.base_url);
        self.request_url_json(method, &url, path, body, REQUEST_TIMEOUT).await
    }

    /// One request with bounded retries on transport errors, 429, and 5xx.
    async fn request_url_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: &str,
        operation: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<T, SeamError> {
        let mut last_err = None;
        for attempt in 0..RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
            let mut req = self
                .http
                .request(method.clone(), url)
                .timeout(timeout)
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

pub struct LlmClient {
    transport: Arc<Transport>,
    /// The batch lane, when the endpoint has a batch API.
    batcher: Option<Arc<ChatBatcher>>,
}

impl LlmClient {
    pub fn new(key: Option<ApiKey>, base_url: impl Into<String>) -> Self {
        Self::with_batching(key, base_url, None)
    }

    pub(crate) fn with_batching(
        key: Option<ApiKey>,
        base_url: impl Into<String>,
        batching: Option<ChatBatching>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with static config builds");
        let transport = Arc::new(Transport {
            http,
            key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            spent: Mutex::new(0.0),
            batch_jobs: AtomicU64::new(0),
        });
        let batcher = batching.map(|tuning| Arc::new(ChatBatcher::new(Arc::clone(&transport), tuning)));
        Self { transport, batcher }
    }

    fn base_url(&self) -> &str {
        &self.transport.base_url
    }

    fn record_cost(&self, usage: Option<&Usage>) {
        self.transport.record_cost(usage);
    }

    /// Provider-specific reasoning wire shape. OpenRouter takes a nested
    /// object and can omit the unused trace; other compatible endpoints
    /// (including ollama) retain the top-level `reasoning_effort` field.
    fn chat_body(&self, request: &ChatRequest) -> Result<Value, SeamError> {
        let mut body = serde_json::to_value(request)
            .map_err(|e| SeamError::failed(format!("unserializable request: {e}")))?;
        apply_reasoning_wire(
            &mut body,
            self.base_url(),
            request.reasoning_effort.as_deref(),
        );
        Ok(body)
    }

    /// The batch lane: park the call under the job's base model and wait
    /// for its job to answer.
    async fn chat_via_batch(
        &self,
        batcher: &Arc<ChatBatcher>,
        request: &ChatRequest,
        base_model: &str,
    ) -> Result<ChatMessage, SeamError> {
        let mut body = self.chat_body(request)?;
        body["model"] = json!(base_model);
        let receiver = batcher.submit(base_model, body)?;
        receiver
            .await
            .map_err(|_| SeamError::failed("the llm batch lane stopped before replying"))?
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, SeamError> {
        self.transport.request_json(method, path, body).await
    }
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmClient")
            .field("base_url", &self.transport.base_url)
            .field("batch_lane", &self.batcher.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Llm for LlmClient {
    async fn chat(&self, request: &ChatRequest) -> Result<ChatMessage, SeamError> {
        // A `:batch` model on an endpoint without a batch lane goes out as
        // named: the endpoint decides whether it knows the variant.
        if let (Some(batcher), Some(base_model)) = (&self.batcher, batch_base_model(&request.model)) {
            return self.chat_via_batch(batcher, request, base_model).await;
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
        match OllamaApi::new(&self.transport.http, self.base_url())
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
        apply_reasoning_wire(&mut body, self.base_url(), request.reasoning_effort);
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
        if let Some(models) = OllamaApi::new(&self.transport.http, self.base_url())
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
        *self.transport.spent.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn batch_jobs(&self) -> u64 {
        self.transport.batch_jobs.load(Ordering::Relaxed)
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

#[derive(Debug, Deserialize)]
pub(crate) struct ChatResponse {
    #[serde(default)]
    pub(crate) choices: Vec<Choice>,
    #[serde(default)]
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    pub(crate) message: ChatMessage,
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
pub(crate) struct Usage {
    #[serde(default)]
    pub(crate) cost: Option<f64>,
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

    #[test]
    fn the_batch_lane_is_derived_for_openrouter_and_absent_elsewhere() {
        let openrouter = LlmEndpointConfig::default();
        assert_eq!(openrouter.batch_requests_max.get(), 5_000);
        assert_eq!(
            openrouter.batches_url().as_deref(),
            Some("https://openrouter.ai/api/beta/batches")
        );
        assert_eq!(openrouter.transform_batch_model(), "google/gemini-2.5-flash-lite:batch");
        assert!(openrouter.batching().is_some());

        let ollama = LlmEndpointConfig {
            base_url: "http://localhost:11434/v1".to_string(),
            api_key_env: String::new(),
            ..LlmEndpointConfig::default()
        };
        assert_eq!(ollama.batches_url(), None);
        assert!(ollama.batching().is_none());

        let gateway = LlmEndpointConfig {
            base_url: "https://gateway.example/v1".to_string(),
            batches_url: Some("https://gateway.example/batches/".to_string()),
            transform_model: "google/gemini-2.5-flash-lite:batch".to_string(),
            ..LlmEndpointConfig::default()
        };
        assert_eq!(gateway.batches_url().as_deref(), Some("https://gateway.example/batches"));
        assert_eq!(gateway.transform_batch_model(), "google/gemini-2.5-flash-lite:batch");
    }

    #[test]
    fn openrouter_rejects_batch_jobs_above_its_request_limit_at_boot() {
        let mut config = toml::Table::new();
        config.insert("batch_requests_max".into(), toml::Value::Integer(5_001));

        let error = LlmEndpoint::from_config(&config).err().unwrap();

        assert_eq!(
            error.to_string(),
            "llm.batch_requests_max must not exceed 5000 for OpenRouter"
        );
    }

    #[test]
    fn another_batch_endpoint_may_set_its_own_request_limit() {
        let mut config = toml::Table::new();
        config.insert(
            "base_url".into(),
            toml::Value::String("https://gateway.example/v1".into()),
        );
        config.insert(
            "batches_url".into(),
            toml::Value::String("https://gateway.example/batches".into()),
        );
        config.insert("batch_requests_max".into(), toml::Value::Integer(10_000));

        let plugin = LlmEndpoint::from_config(&config).unwrap();

        assert_eq!(plugin.config.batch_requests_max.get(), 10_000);
    }

    #[test]
    fn batch_base_model_strips_only_a_real_suffix() {
        assert_eq!(batch_base_model("google/gemini-2.5-flash-lite:batch"), Some("google/gemini-2.5-flash-lite"));
        assert_eq!(batch_base_model("google/gemini-2.5-flash-lite"), None);
        assert_eq!(batch_base_model(":batch"), None);
    }
}
