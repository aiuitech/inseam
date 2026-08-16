//! The `llm` seam: chat/completion against a configured endpoint
//! (`design/services.md`). One seam, metered *at* the seam: every call
//! passes the [`LlmCall`] guard, which is how a consumer's budget is
//! enforced without the consumer's cooperation — a budget listener denies,
//! and denial is monotonic.

use inseam_kernel::substrate::{Guard, ServiceKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SeamError;

pub const LLM: ServiceKey<dyn Llm> = ServiceKey::new("llm");

/// Capability-fact keys the default provider declares.
pub mod facts {
    /// string: model consumers should use for transform-grade calls.
    pub const TRANSFORM_MODEL: &str = "transform_model";
    /// string: model for agent-grade tool-calling loops.
    pub const AGENT_MODEL: &str = "agent_model";
}

/// Guard event dispatched before an LLM call is made on a consumer's behalf.
/// Policy plugins (the sweep's budget meter, a future spend cap) listen and
/// may deny; the metered handle refuses the call on any denial.
#[derive(Debug, Clone)]
pub struct LlmCall {
    /// Who the call is charged to (a transform's entry id, "agent", ...).
    pub consumer: String,
}

impl Guard for LlmCall {}

#[async_trait::async_trait]
pub trait Llm: Send + Sync {
    /// One chat completion; returns the assistant message.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatMessage, SeamError>;

    /// Embed texts in order; the output index matches the input index.
    async fn embed(&self, model: &str, inputs: &[&str]) -> Result<Vec<Vec<f32>>, SeamError>;

    /// Describe an image (OCR, captioning) with a vision-capable model.
    async fn describe_image(
        &self,
        model: &str,
        prompt: &str,
        mimetype: &str,
        image: &[u8],
    ) -> Result<String, SeamError>;

    /// The endpoint's model catalog (embedding models when `embeddings`).
    async fn models(&self, embeddings: bool) -> Result<Vec<ModelInfo>, SeamError>;

    /// Dollars spent through this provider so far, as reported by usage.
    fn spent(&self) -> f64;
}

// ---------------------------------------------------------------------------
// Chat vocabulary (OpenAI-compatible wire shapes)
// ---------------------------------------------------------------------------

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
    /// Ask the endpoint to report cost in usage so `spent()` stays accurate.
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
