//! Client-owned additive discovery over the guarded operations seam.
//! Each batch preserves successful actions, source identities, and complete JSON.

mod preview;

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{StreamExt, stream};
use inseam_kernel::address::Address;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::operations::{ExpandRequest, Operations, QueryRequest, QueryResult, ScanRequest};
use crate::text::truncate_chars;

pub const ACTIONS_MAX: usize = 8;
pub const CONCURRENCY_MAX: usize = 4;
pub const CANDIDATES_MAX: usize = 100;
pub const RESPONSE_CHARS_MAX: usize = 24_000;
const REQUEST_BYTES_MAX: usize = 32_000;
const REQUESTS_MAX: u32 = 512;
const QUERY_CHARS_MAX: usize = 2_000;
const RECORDS_MAX: usize = 16;

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("invalid find request: {0}")]
    Request(String),
    #[error("find request JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

/// A numeric reference from an earlier result or an explicit discovered address.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SourceReference {
    Candidate(u32),
    Address(Address),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub source: SourceReference,
    pub start: u64,
    pub end: u64,
    /// Resume within the returned window when a single line exceeds the budget.
    #[serde(default)]
    pub offset_chars: u32,
    /// Continuations refuse changed source windows rather than splice two versions.
    #[serde(default)]
    pub window_digest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructureRequest {
    pub source: SourceReference,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindRequest {
    #[serde(default)]
    pub queries: Vec<QueryRequest>,
    #[serde(default)]
    pub expand: Vec<StructureRequest>,
    #[serde(default)]
    pub scan: Vec<ReadRequest>,
    /// Recover retained result details that did not fit in an earlier response.
    #[serde(default)]
    pub inspect: Vec<u32>,
    /// Explicitly release candidates; full collections never evict silently.
    #[serde(default)]
    pub forget: Vec<u32>,
}

impl FindRequest {
    pub fn parse(text: &str) -> Result<Self, DiscoveryError> {
        if text.len() > REQUEST_BYTES_MAX {
            return Err(DiscoveryError::Request("exceeds 32,000 bytes".into()));
        }
        let request: Self = serde_json::from_str(text)?;
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), DiscoveryError> {
        let count = self.queries.len()
            + self.expand.len()
            + self.scan.len()
            + self.inspect.len()
            + self.forget.len();
        if !(1..=ACTIONS_MAX).contains(&count) {
            return Err(DiscoveryError::Request("requires 1..=8 actions".into()));
        }
        for query in &self.queries {
            if query.text.trim().is_empty() {
                return Err(DiscoveryError::Request("search text is empty".into()));
            }
            if query.text.chars().count() > QUERY_CHARS_MAX {
                return Err(DiscoveryError::Request(
                    "search exceeds 2,000 characters".into(),
                ));
            }
            if !(1..=25).contains(&query.limit) {
                return Err(DiscoveryError::Request(
                    "search limit must be 1..=25".into(),
                ));
            }
        }
        for read in &self.scan {
            if read.start == 0 {
                return Err(DiscoveryError::Request(
                    "scan starts at line 1 or later".into(),
                ));
            }
            if read.end < read.start {
                return Err(DiscoveryError::Request("scan end precedes start".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Candidate {
    address: Address,
    view: Value,
    version: Value,
    discoveries: Vec<u32>,
    reads: Vec<Value>,
}

#[derive(Debug, Default)]
pub struct DiscoverySession {
    candidates: BTreeMap<u32, Candidate>,
    next_id: u32,
    requests: u32,
}

enum Action {
    Query(QueryRequest),
    Expand(StructureRequest, Address),
    Scan(ReadRequest, Address),
    Inspect(u32),
    Error(String),
}

enum Outcome {
    Query(String, Vec<QueryResult>),
    Expand(StructureRequest, crate::operations::ExpandResponse),
    Scan(ReadRequest, crate::operations::ScanResponse),
    Inspect(u32),
    Error(String),
}

impl DiscoverySession {
    /// Compose normal operations, retaining state only in this caller's session.
    pub async fn find(
        &mut self,
        operations: &dyn Operations,
        request: FindRequest,
    ) -> Result<Value, DiscoveryError> {
        request.validate()?;
        if self.requests >= REQUESTS_MAX {
            return Err(DiscoveryError::Request(
                "session exceeded 512 batches".into(),
            ));
        }
        self.requests += 1;
        let forgotten = self.forget(&request.forget);
        let actions = self.actions(request);
        let quota = (RESPONSE_CHARS_MAX - 2_000) / actions.len().max(1);
        let outcomes: Vec<_> = stream::iter(actions)
            .map(|action| execute_bounded(operations, action))
            .buffered(CONCURRENCY_MAX)
            .collect()
            .await;
        let mut results = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            let result = self.complete(outcome, quota);
            assert!(encoded_chars(&result) <= quota);
            results.push(result);
        }
        let response = json!({"results": results, "forgotten": forgotten,
            "retained_count": self.candidates.len(), "capacity": CANDIDATES_MAX});
        assert!(self.candidates.len() <= CANDIDATES_MAX);
        assert!(encoded_chars(&response) <= RESPONSE_CHARS_MAX);
        Ok(response)
    }

    fn forget(&mut self, ids: &[u32]) -> Vec<u32> {
        assert!(ids.len() <= ACTIONS_MAX);
        ids.iter()
            .copied()
            .filter(|id| self.candidates.remove(id).is_some())
            .collect()
    }

    fn resolve(&self, source: &SourceReference) -> Result<Address, String> {
        match source {
            SourceReference::Address(address) => Ok(address.clone()),
            SourceReference::Candidate(id) => self
                .candidates
                .get(id)
                .map(|candidate| candidate.address.clone())
                .ok_or_else(|| format!("unknown candidate {id}; query or inspect retained IDs")),
        }
    }

    fn actions(&self, request: FindRequest) -> Vec<Action> {
        let mut actions: Vec<_> = request.queries.into_iter().map(Action::Query).collect();
        for expand in request.expand {
            actions.push(match self.resolve(&expand.source) {
                Ok(address) => Action::Expand(expand, address),
                Err(error) => Action::Error(error),
            });
        }
        for scan in request.scan {
            actions.push(match self.resolve(&scan.source) {
                Ok(address) => Action::Scan(scan, address),
                Err(error) => Action::Error(error),
            });
        }
        actions.extend(request.inspect.into_iter().map(Action::Inspect));
        assert!(actions.len() <= ACTIONS_MAX);
        actions
    }

    fn complete(&mut self, outcome: Outcome, quota: usize) -> Value {
        match outcome {
            Outcome::Query(text, rows) => self.complete_query(&text, rows, quota),
            Outcome::Scan(request, response) => self.complete_scan(request, response, quota),
            Outcome::Expand(request, response) => preview::structure(request, response, quota),
            Outcome::Inspect(id) => match self.candidates.get(&id) {
                Some(candidate) => fit_view(self.candidate_view(id, candidate), quota),
                None => error_value(format!("unknown candidate {id}")),
            },
            Outcome::Error(error) => error_value(error),
        }
    }

    fn complete_query(&mut self, text: &str, rows: Vec<QueryResult>, quota: usize) -> Value {
        assert!(rows.len() <= 25);
        let mut ids = Vec::new();
        let mut rejected = 0_u32;
        for row in rows {
            match self.retain(row, text) {
                Some(id) => ids.push(id),
                None => rejected += 1,
            }
        }
        let mut response = json!({"operation": "query", "candidate_ids": ids,
            "candidates": [], "unretained_count": rejected,
            "omitted_candidate_ids": ids});
        for id in ids {
            let candidate = &self.candidates[&id];
            let view = self.candidate_view(id, candidate);
            response["candidates"]
                .as_array_mut()
                .expect("created as array")
                .push(view);
            if encoded_chars(&response) > quota {
                response["candidates"]
                    .as_array_mut()
                    .expect("created as array")
                    .pop();
            } else {
                response["omitted_candidate_ids"]
                    .as_array_mut()
                    .expect("created as array")
                    .retain(|value| value != id);
            }
        }
        response
    }

    fn retain(&mut self, row: QueryResult, query: &str) -> Option<u32> {
        let existing = self
            .candidates
            .iter()
            .find(|(_, candidate)| candidate.address == row.address)
            .map(|(id, _)| *id);
        let version = json!({"digest":row.envelope.content_digest,
            "modified":row.envelope.modified, "length":row.envelope.length});
        let mut view = preview::candidate(&row, query);
        match existing {
            Some(id) => {
                let candidate = self.candidates.get_mut(&id).expect("located above");
                if candidate.version != version {
                    candidate.reads.clear();
                } else {
                    retain_excerpts(&mut view, &candidate.view);
                }
                candidate.version = version;
                candidate.view = view;
                if candidate.discoveries.len() < RECORDS_MAX {
                    candidate.discoveries.push(self.requests);
                }
                Some(id)
            }
            None => {
                if self.candidates.len() == CANDIDATES_MAX {
                    return None;
                }
                self.next_id += 1;
                self.candidates.insert(
                    self.next_id,
                    Candidate {
                        address: row.address,
                        view,
                        version,
                        discoveries: vec![self.requests],
                        reads: Vec::new(),
                    },
                );
                Some(self.next_id)
            }
        }
    }

    fn candidate_view(&self, id: u32, candidate: &Candidate) -> Value {
        let mut view = candidate.view.clone();
        view["id"] = json!(id);
        view["discovery_batches"] = json!(candidate.discoveries);
        view["read_windows"] = json!(candidate.reads);
        view["history_limit"] = json!(RECORDS_MAX);
        view["history_at_capacity"] = json!(
            candidate.reads.len() == RECORDS_MAX || candidate.discoveries.len() == RECORDS_MAX
        );
        view
    }

    fn complete_scan(
        &mut self,
        request: ReadRequest,
        response: crate::operations::ScanResponse,
        quota: usize,
    ) -> Value {
        let address = response.address.clone();
        let source_bytes = match response.lines_total {
            Some(total) if response.start == 1 && response.end == total => {
                Some(response.text.len())
            }
            _ => None,
        };
        let mut value = preview::scan(request, response, quota);
        if value.get("error").is_some() {
            return value;
        }
        if let Some((id, candidate)) = self
            .candidates
            .iter_mut()
            .find(|(_, candidate)| candidate.address == address)
        {
            value["candidate_id"] = json!(id);
            if candidate.reads.len() < RECORDS_MAX {
                candidate.reads.push(json!({"start":value["start"], "end":value["end"],
                    "offset_chars":value["offset_chars"], "delivered_chars":value["delivered_chars"]}));
            }
            if let Some(bytes) = source_bytes {
                candidate.view["read_text_bytes"] = json!(bytes);
            }
        }
        value
    }
}

async fn execute(operations: &dyn Operations, action: Action) -> Outcome {
    match action {
        Action::Query(request) => {
            let text = request.text.clone();
            match operations.query(request).await {
                Ok(response) => Outcome::Query(text, response.results),
                Err(error) => Outcome::Error(error.to_string()),
            }
        }
        Action::Expand(request, address) => {
            match operations.expand(ExpandRequest { address }).await {
                Ok(response) => Outcome::Expand(request, response),
                Err(error) => Outcome::Error(error.to_string()),
            }
        }
        Action::Scan(request, address) => {
            let read = ScanRequest {
                address,
                start: request.start,
                end: request.end,
            };
            match operations.scan(read).await {
                Ok(response) => Outcome::Scan(request, response),
                Err(error) => Outcome::Error(error.to_string()),
            }
        }
        Action::Inspect(id) => Outcome::Inspect(id),
        Action::Error(error) => Outcome::Error(error),
    }
}

async fn execute_bounded(operations: &dyn Operations, action: Action) -> Outcome {
    match tokio::time::timeout(Duration::from_secs(60), execute(operations, action)).await {
        Ok(outcome) => outcome,
        Err(_) => Outcome::Error("action exceeded its 60-second timeout".into()),
    }
}

fn error_value(error: String) -> Value {
    // Six-character JSON escapes must also fit the smallest action quota.
    json!({"error": truncate_chars(&error, 256)})
}

fn encoded_chars(value: &Value) -> usize {
    // A serde_json::Value contains only serializable JSON values.
    serde_json::to_string(value)
        .expect("JSON values serialize")
        .chars()
        .count()
}

fn fit_view(view: Value, quota: usize) -> Value {
    if encoded_chars(&view) <= quota {
        view
    } else {
        error_value("candidate metadata exceeds this action's budget; inspect it alone".into())
    }
}

fn retain_excerpts(view: &mut Value, previous: &Value) {
    let excerpts = view["excerpts"]
        .as_array_mut()
        .expect("preview creates excerpts array");
    let older = previous["excerpts"]
        .as_array()
        .expect("retained preview has excerpts array");
    let mut omitted = 0_u32;
    for excerpt in older {
        if excerpts.contains(excerpt) {
            continue;
        }
        if excerpts.len() < 6 {
            excerpts.push(excerpt.clone());
        } else {
            omitted += 1;
        }
    }
    view["older_excerpts_omitted"] = json!(omitted);
}
