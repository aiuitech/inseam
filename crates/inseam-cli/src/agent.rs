//! The agent demo: a live LLM driving the discovery ladder — `query`, then
//! `expand`/`scan` where a result earns it, then `fetch` only when needed
//! (`design/finder.md`). A pure consumer of the `operations` and `llm`
//! seams; the tools the model sees are the operation messages themselves.

use serde_json::json;
use thiserror::Error;

use inseam_seams::text::truncate_chars;
use inseam_seams::llm::{ChatMessage, ChatRequest, Llm, Tool, ToolCall};
use inseam_seams::operations::{
    ExpandRequest, FetchRequest, Operations, QueryRequest, QueryResponse, ScanRequest,
};
use inseam_seams::SeamError;

/// Characters of tool output returned to the model per call.
const TOOL_RESULT_CHARS: usize = 12_000;
/// Characters of each result's summary the model sees in a `query` reply.
/// A summary may be the whole document (a text that fits the summarizer's
/// target is its own summary), and ten of those ran to 66,000 characters
/// — the cap above then cut the reply after the second result, so the
/// model chose among two of ten. Every result now fits; `scan` and `fetch`
/// are the rungs for reading one.
const QUERY_SUMMARY_CHARS: usize = 500;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] SeamError),
    #[error("model kept calling tools after {0} turns and had nothing to say when asked to close; raise --turns")]
    OutOfTurns(usize),
}

#[derive(Debug)]
pub struct AgentOutcome {
    pub answer: String,
    pub turns: usize,
    pub tool_calls: usize,
    /// Dollars reported for the whole provider, this run included.
    pub spent: f64,
}

/// One step the loop took, surfaced so the CLI can narrate the ladder.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    ToolCall { name: String, arguments: String },
    ToolResult { name: String, brief: String },
}

pub async fn run_agent(
    operations: &dyn Operations,
    llm: &dyn Llm,
    model: &str,
    question: &str,
    max_turns: usize,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<AgentOutcome, AgentError> {
    let mut messages = vec![
        ChatMessage::system(system_prompt()),
        ChatMessage::user(question.to_string()),
    ];
    let tools = tool_definitions();
    let mut tool_calls_made = 0usize;

    for turn in 1..=max_turns {
        let request = ChatRequest::new(model, messages.clone()).with_tools(tools.clone());
        let reply = llm.chat(&request).await?;

        let calls = reply.tool_calls.clone().unwrap_or_default();
        if calls.is_empty() {
            let answer = reply.content.unwrap_or_default();
            return Ok(AgentOutcome {
                answer,
                turns: turn,
                tool_calls: tool_calls_made,
                spent: llm.spent(),
            });
        }

        messages.push(reply);
        for call in calls {
            tool_calls_made += 1;
            on_event(AgentEvent::ToolCall {
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            });
            let result = execute(operations, &call).await;
            on_event(AgentEvent::ToolResult {
                name: call.function.name.clone(),
                brief: brief_of(&result),
            });
            messages.push(ChatMessage::tool_result(call.id.clone(), result));
        }
    }
    // The budget is spent, but the model has read things: one closing call
    // with no tools on offer turns what it read into an answer, instead of
    // discarding the whole exchange. Only an empty close is the failure.
    messages.push(ChatMessage::user(CLOSING_PROMPT.to_string()));
    let closing = ChatRequest::new(model, messages);
    let reply = llm.chat(&closing).await?;
    let answer = reply.content.unwrap_or_default();
    if answer.trim().is_empty() {
        return Err(AgentError::OutOfTurns(max_turns));
    }
    Ok(AgentOutcome {
        answer,
        turns: max_turns,
        tool_calls: tool_calls_made,
        spent: llm.spent(),
    })
}

/// What the model is told when its tool turns run out.
const CLOSING_PROMPT: &str = "Your search budget is spent and no more tools are available. \
    Answer the question now from what you have already read, naming the sources you relied \
    on; if what you read does not answer it, say so plainly.";

/// Run one tool call against the operations seam. Errors go back to the
/// model as text — wrong addresses and bad ranges are its problem to
/// correct.
async fn execute(operations: &dyn Operations, call: &ToolCall) -> String {
    let args = &call.function.arguments;
    let outcome: Result<String, String> = match call.function.name.as_str() {
        "query" => match parse::<QueryRequest>(args) {
            Ok(r) => operations
                .query(r)
                .await
                .map(|v| to_json(&query_for_model(v)))
                .map_err(stringify),
            Err(e) => Err(e),
        },
        "expand" => match parse::<ExpandRequest>(args) {
            Ok(r) => operations.expand(r).await.map(|v| to_json(&v)).map_err(stringify),
            Err(e) => Err(e),
        },
        "scan" => match parse::<ScanRequest>(args) {
            Ok(r) => operations.scan(r).await.map(|v| to_json(&v)).map_err(stringify),
            Err(e) => Err(e),
        },
        "fetch" => match parse::<FetchRequest>(args) {
            Ok(r) => operations.fetch(r).await.map(|v| to_json(&v)).map_err(stringify),
            Err(e) => Err(e),
        },
        other => Err(format!("unknown tool `{other}`")),
    };
    let body = match outcome {
        Ok(json) => json,
        Err(e) => json!({ "error": e }).to_string(),
    };
    truncate_chars(&body, TOOL_RESULT_CHARS)
}

/// The query reply as the model should see it: every result, each summary
/// cut to an excerpt, so the ranking is what the model chooses from.
fn query_for_model(mut response: QueryResponse) -> QueryResponse {
    for result in &mut response.results {
        if let Some(summary) = result.summary.take() {
            result.summary = Some(truncate_chars(&summary, QUERY_SUMMARY_CHARS));
        }
    }
    response
}

fn parse<T: serde::de::DeserializeOwned>(args: &str) -> Result<T, String> {
    serde_json::from_str(args).map_err(|e| format!("bad arguments: {e}"))
}

fn stringify(e: SeamError) -> String {
    e.to_string()
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| json!({ "error": e.to_string() }).to_string())
}

fn brief_of(result: &str) -> String {
    if let Some(rest) = result.strip_prefix("{\"error\":") {
        return truncate_chars(rest.trim_end_matches('}'), 120);
    }
    format!("{} chars", result.chars().count())
}

fn system_prompt() -> String {
    "You are searching a personal data network through inseam, a discovery index over \
     the user's own files. Sources are named by addresses like \
     inseam://<host>/<path>.\n\
     Follow the incremental-discovery ladder, cheapest rung first:\n\
     1. `query` — search; returns ranked addresses with summaries, the source's length \
     in lines, and matching-fragment hints with their scores and line extents.\n\
     2. `expand` — one result's fragment structure and related entities; use it to \
     navigate a promising source or hop to related ones.\n\
     3. `scan` — read a specific line range of a text source. Start from a hint's extent \
     and widen around it for context; the response says how many lines the source has.\n\
     4. `fetch` — full content, only when a scan cannot answer.\n\
     Query again with different words if results look weak. Prefer scanning the exact \
     lines the hints point at over fetching. Answer as soon as the evidence answers the \
     question — one or two confirming queries is enough; do not keep searching to prove \
     nothing else exists. When you answer, cite the addresses you used and say what you \
     found in them."
        .to_string()
}

fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::function(
            "query",
            "Search the discovery index. Returns ranked sources with scores, summaries, \
             and matching-fragment hints.",
            json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What to look for; plain words work best." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 25, "description": "Max results (default 8)." }
                },
                "required": ["text"]
            }),
        ),
        Tool::function(
            "expand",
            "Get one source's fragments and relations: its sections with line extents, plus \
             related entities and the other sources they connect to.",
            json!({
                "type": "object",
                "properties": {
                    "address": { "type": "string", "description": "An inseam:// address from a query result." }
                },
                "required": ["address"]
            }),
        ),
        Tool::function(
            "scan",
            "Read lines start..end (1-based, inclusive) of a text source without fetching it \
             all. At most 2000 lines per call; the response's `end` and `lines_total` say \
             where it stopped and how much there is.",
            json!({
                "type": "object",
                "properties": {
                    "address": { "type": "string" },
                    "start": { "type": "integer", "minimum": 1 },
                    "end": { "type": "integer", "minimum": 1 }
                },
                "required": ["address", "start", "end"]
            }),
        ),
        Tool::function(
            "fetch",
            "Retrieve a text source's full content. The most expensive rung; prefer scan.",
            json!({
                "type": "object",
                "properties": {
                    "address": { "type": "string" }
                },
                "required": ["address"]
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::ContentLength;
    use inseam_seams::operations::{EnvelopeView, QueryMeta, QueryResult};

    #[test]
    fn tool_definitions_cover_the_ladder() {
        let names: Vec<String> = tool_definitions()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert_eq!(names, vec!["query", "expand", "scan", "fetch"]);
    }

    #[test]
    fn query_for_model_keeps_every_result_and_cuts_each_summary() {
        let long = "x".repeat(6_000);
        let results: Vec<QueryResult> = (0..10)
            .map(|i| QueryResult {
                address: format!("inseam://h/doc-{i}.txt").parse().expect("valid"),
                score: 1.0,
                summary: Some(long.clone()),
                envelope: EnvelopeView {
                    source_type: "file".into(),
                    content_type: "text/plain".into(),
                    length: ContentLength::Bytes(6_000),
                    created: None,
                    modified: None,
                    title: None,
                },
                hints: Vec::new(),
                replicas: Vec::new(),
            })
            .collect();
        let response = query_for_model(QueryResponse {
            results,
            meta: QueryMeta::default(),
        });
        assert_eq!(response.results.len(), 10);
        assert!(response.results.iter().all(|r| {
            r.summary.as_ref().map_or(false, |s| s.chars().count() <= QUERY_SUMMARY_CHARS)
        }));
        assert!(to_json(&response).chars().count() <= TOOL_RESULT_CHARS);
    }

    #[test]
    fn parse_rejects_malformed_arguments() {
        let r = parse::<QueryRequest>("{\"limit\": 3}");
        assert!(r.is_err(), "text is required");
        let r = parse::<QueryRequest>("{\"text\": \"reno\"}");
        assert!(r.is_ok_and(|q| q.limit == 8), "limit defaults");
    }
}
