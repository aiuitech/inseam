//! The batch lane of the `llm` provider: chat calls for a `:batch` model are
//! parked and submitted together through the endpoint's batch API
//! (OpenRouter's `/api/beta/batches`), which answers asynchronously at a
//! discount. The point is the job size: an indexing run parks thousands of
//! planners on their summary calls, and one job carries them all.
//!
//! A background leader task — never a caller's own task, which a cancelled
//! planner would take down with it — collects arrivals until a job is full,
//! or arrivals have gone quiet, or the oldest has waited long enough, then
//! hands the job to its own task and keeps collecting. Jobs run
//! concurrently up to a bound; each result is delivered to its caller by the
//! stable id it was sent under, and one failed item fails only its caller.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use inseam_seams::llm::ChatMessage;
use inseam_seams::SeamError;

use super::{ChatResponse, Transport, Usage};

/// Leader ticks before the leader gives up on a queue that never drains:
/// a day of 250 ms ticks, well past any job's completion window.
const LEAD_TICKS_MAX: u32 = 345_600;

/// The batch lane's dials. Built from config by the plugin; tests build a
/// fast one against a fake endpoint.
#[derive(Debug, Clone)]
pub(crate) struct ChatBatching {
    /// The batch-API collection URL (`POST` creates, `GET <url>/<id>` polls).
    pub batches_url: String,
    /// Requests per job, the upper bound.
    pub requests_max: usize,
    /// Serialized request bytes per job, the upper bound.
    pub bytes_max: usize,
    /// No arrival for this long submits what is pending.
    pub quiescence: Duration,
    /// The oldest pending request waits at most this long, however steady
    /// the trickle of arrivals.
    pub age_max: Duration,
    /// The leader's clock.
    pub tick: Duration,
    /// Status polls: this cadence for the first `poll_steady_after` polls,
    /// then `poll_steady`, for at most `poll_max` polls.
    pub poll_initial: Duration,
    pub poll_steady: Duration,
    pub poll_steady_after: u32,
    pub poll_max: u32,
    /// Jobs in flight at once; the leader waits for a slot beyond it.
    pub jobs_in_flight_max: usize,
    /// Parked calls the lane holds; beyond it a call is refused.
    pub queue_max: usize,
    /// How long one job-creation upload may take.
    pub create_timeout: Duration,
}

/// One parked call: its request body (model already the job's base model),
/// its size, and the caller waiting for the answer.
pub(crate) struct PendingBatchChat {
    pub model: String,
    pub body: Value,
    pub bytes: usize,
    pub response: oneshot::Sender<Result<ChatMessage, SeamError>>,
}

#[derive(Default)]
struct BatcherState {
    pending: VecDeque<PendingBatchChat>,
    pending_bytes: usize,
    last_arrival: Option<Instant>,
    oldest_arrival: Option<Instant>,
    leading: bool,
}

/// What the leader found on a tick.
enum Tick {
    Job(Vec<PendingBatchChat>),
    Wait,
    Retire,
}

pub(crate) struct ChatBatcher {
    transport: Arc<Transport>,
    tuning: ChatBatching,
    state: Mutex<BatcherState>,
    jobs: Arc<Semaphore>,
}

impl ChatBatcher {
    pub(crate) fn new(transport: Arc<Transport>, tuning: ChatBatching) -> Self {
        assert!(tuning.requests_max > 0);
        assert!(tuning.bytes_max > 0);
        assert!(tuning.jobs_in_flight_max > 0);
        assert!(tuning.queue_max >= tuning.requests_max);
        Self {
            transport,
            jobs: Arc::new(Semaphore::new(tuning.jobs_in_flight_max)),
            tuning,
            state: Mutex::new(BatcherState::default()),
        }
    }

    /// Park one call. The receiver resolves when its job completes; the
    /// first arrival on an idle lane starts the leader.
    pub(crate) fn submit(
        self: &Arc<Self>,
        model: &str,
        body: Value,
    ) -> Result<oneshot::Receiver<Result<ChatMessage, SeamError>>, SeamError> {
        assert!(!model.is_empty());
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| SeamError::failed(format!("unserializable request: {e}")))?
            .len();
        let (sender, receiver) = oneshot::channel();
        let mut state = self.lock();
        if state.pending.len() >= self.tuning.queue_max {
            return Err(SeamError::failed("the llm batch lane queue is full"));
        }
        let now = Instant::now();
        state.pending.push_back(PendingBatchChat {
            model: model.to_string(),
            body,
            bytes,
            response: sender,
        });
        state.pending_bytes += bytes;
        state.last_arrival = Some(now);
        state.oldest_arrival.get_or_insert(now);
        if !state.leading {
            state.leading = true;
            tokio::spawn(Arc::clone(self).lead());
        }
        Ok(receiver)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BatcherState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The leader: tick, and on each tick either hand a ready job to its own
    /// task, keep waiting, or retire once nothing is pending.
    async fn lead(self: Arc<Self>) {
        for _tick in 0..LEAD_TICKS_MAX {
            tokio::time::sleep(self.tuning.tick).await;
            match self.tick() {
                Tick::Retire => return,
                Tick::Wait => {}
                Tick::Job(job) => {
                    // Provably infallible: the semaphore is never closed.
                    #[allow(clippy::expect_used)]
                    let permit = Arc::clone(&self.jobs)
                        .acquire_owned()
                        .await
                        .expect("the job semaphore is never closed");
                    tokio::spawn(Arc::clone(&self).run_job(job, permit));
                }
            }
        }
        self.fail_pending("the llm batch lane leader reached its tick limit");
    }

    /// One leader tick under the lock: take a job when one is ready.
    fn tick(&self) -> Tick {
        let mut state = self.lock();
        let Some(oldest) = state.oldest_arrival else {
            assert!(state.pending.is_empty());
            state.leading = false;
            return Tick::Retire;
        };
        assert!(!state.pending.is_empty());
        let now = Instant::now();
        let full = state.pending.len() >= self.tuning.requests_max
            || state.pending_bytes >= self.tuning.bytes_max;
        let quiet = state
            .last_arrival
            .is_some_and(|last| now.duration_since(last) >= self.tuning.quiescence);
        let old = now.duration_since(oldest) >= self.tuning.age_max;
        if !(full || quiet || old) {
            return Tick::Wait;
        }
        let job = take_job(&mut state.pending, self.tuning.requests_max, self.tuning.bytes_max);
        state.pending_bytes = state.pending.iter().map(|p| p.bytes).sum();
        if state.pending.is_empty() {
            state.oldest_arrival = None;
            state.last_arrival = None;
        } else {
            // The next-oldest arrived no earlier than the one just taken;
            // its true arrival is not kept, so age restarts from now.
            state.oldest_arrival = Some(now);
        }
        Tick::Job(job)
    }

    fn fail_pending(&self, reason: &str) {
        let mut state = self.lock();
        let pending = std::mem::take(&mut state.pending);
        state.pending_bytes = 0;
        state.oldest_arrival = None;
        state.last_arrival = None;
        state.leading = false;
        drop(state);
        for item in pending {
            let _ = item.response.send(Err(SeamError::failed(reason)));
        }
    }

    /// One job, start to finish, on its own task: create, poll, deliver.
    async fn run_job(self: Arc<Self>, mut job: Vec<PendingBatchChat>, _permit: OwnedSemaphorePermit) {
        assert!(!job.is_empty());
        assert!(job.len() <= self.tuning.requests_max);
        let model = job[0].model.clone();
        assert!(job.iter().all(|p| p.model == model));
        let bytes: usize = job.iter().map(|p| p.bytes).sum();
        let requests: Vec<Value> = job
            .iter_mut()
            .enumerate()
            .map(|(index, pending)| {
                json!({"custom_id": custom_id(index), "body": std::mem::take(&mut pending.body)})
            })
            .collect();
        let started = Instant::now();
        let outcome = self.submit_job(&model, requests, bytes).await;
        match &outcome {
            Ok(_) => tracing::info!(
                model,
                requests = job.len(),
                elapsed_s = started.elapsed().as_secs(),
                "llm batch job completed"
            ),
            Err(error) => tracing::warn!(model, requests = job.len(), %error, "llm batch job failed"),
        }
        deliver(job, outcome);
    }

    async fn submit_job(
        &self,
        model: &str,
        requests: Vec<Value>,
        bytes: usize,
    ) -> Result<Vec<Result<ChatMessage, SeamError>>, SeamError> {
        let expected_count = requests.len();
        // OpenRouter stream-parses the upload and requires `endpoint` and
        // `model` before `requests`; serde_json's map orders keys
        // alphabetically, which satisfies it (asserted in tests).
        let body = json!({
            "endpoint": "/v1/chat/completions",
            "model": model,
            "requests": requests,
        });
        let created: OpenRouterBatch = self
            .transport
            .request_url_json(
                reqwest::Method::POST,
                &self.tuning.batches_url,
                "creating llm batch job",
                Some(&body),
                self.tuning.create_timeout,
            )
            .await?;
        self.transport.record_batch_job();
        tracing::info!(id = created.id, model, requests = expected_count, bytes, "llm batch job created");
        let completed = self.poll(&created.id).await?;
        self.transport.record_cost(completed.usage.as_ref());
        let results = completed
            .results
            .ok_or_else(|| SeamError::failed("a completed llm batch job has no results"))?;
        Ok(results_in_request_order(results, expected_count))
    }

    async fn poll(&self, batch_id: &str) -> Result<OpenRouterBatch, SeamError> {
        let url = format!("{}/{batch_id}", self.tuning.batches_url);
        for poll_index in 0..self.tuning.poll_max {
            let cadence = if poll_index < self.tuning.poll_steady_after {
                self.tuning.poll_initial
            } else {
                self.tuning.poll_steady
            };
            tokio::time::sleep(cadence).await;
            let batch: OpenRouterBatch = self
                .transport
                .request_url_json(reqwest::Method::GET, &url, "polling llm batch job", None, self.tuning.create_timeout)
                .await?;
            tracing::debug!(id = batch_id, status = batch.status, "llm batch job polled");
            match batch.status.as_str() {
                "completed" => return Ok(batch),
                "validating" | "in_progress" | "finalizing" => {}
                "failed" | "cancelled" | "expired" => {
                    return Err(SeamError::failed(format!(
                        "llm batch job {batch_id} {}: {}",
                        batch.status,
                        batch.error.unwrap_or(Value::Null)
                    )));
                }
                status => {
                    return Err(SeamError::failed(format!(
                        "llm batch job {batch_id} returned unknown status `{status}`"
                    )));
                }
            }
        }
        Err(SeamError::failed(format!(
            "llm batch job {batch_id} exceeded its poll limit"
        )))
    }
}

/// Take the job at the head of the queue: the longest same-model prefix
/// within the request and byte caps, always at least one.
fn take_job(pending: &mut VecDeque<PendingBatchChat>, requests_max: usize, bytes_max: usize) -> Vec<PendingBatchChat> {
    let model = pending.front().map(|p| p.model.clone());
    let Some(model) = model else {
        return Vec::new();
    };
    let mut count: usize = 0;
    let mut bytes: usize = 0;
    for item in pending.iter().take(requests_max) {
        if item.model != model {
            break;
        }
        if count > 0 && bytes + item.bytes > bytes_max {
            break;
        }
        count += 1;
        bytes += item.bytes;
    }
    assert!(count >= 1);
    assert!(count <= requests_max);
    pending.drain(..count).collect()
}

fn custom_id(index: usize) -> String {
    format!("inseam-{index}")
}

/// Answer every caller of a job: its own result on success, the job's
/// error on failure.
fn deliver(job: Vec<PendingBatchChat>, outcome: Result<Vec<Result<ChatMessage, SeamError>>, SeamError>) {
    match outcome {
        Ok(results) => {
            assert_eq!(results.len(), job.len());
            for (item, result) in job.into_iter().zip(results) {
                let _ = item.response.send(result);
            }
        }
        Err(error) => {
            let reason = error.to_string();
            for item in job {
                let _ = item.response.send(Err(SeamError::failed(reason.clone())));
            }
        }
    }
}

/// Results by the request index their stable id names; a repeated, missing,
/// or failed item is an error for that index alone.
fn results_in_request_order(
    results: Vec<OpenRouterBatchResult>,
    expected_count: usize,
) -> Vec<Result<ChatMessage, SeamError>> {
    let mut ordered: Vec<Option<Result<ChatMessage, SeamError>>> =
        (0..expected_count).map(|_| None).collect();
    for result in results {
        let Some(index) = result_index(&result.custom_id, expected_count) else {
            tracing::warn!(custom_id = result.custom_id, "llm batch job returned an unknown id");
            continue;
        };
        if ordered[index].is_some() {
            ordered[index] = Some(Err(SeamError::failed(format!(
                "llm batch job repeated `{}`",
                result.custom_id
            ))));
            continue;
        }
        ordered[index] = Some(result_message(result));
    }
    ordered
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| {
                Err(SeamError::failed(format!("llm batch job omitted `{}`", custom_id(index))))
            })
        })
        .collect()
}

fn result_message(result: OpenRouterBatchResult) -> Result<ChatMessage, SeamError> {
    let response = result.response.ok_or_else(|| {
        SeamError::failed(format!(
            "llm batch item `{}` failed: {}",
            result.custom_id,
            result.error.unwrap_or(Value::Null)
        ))
    })?;
    if !(200..300).contains(&response.status_code) {
        return Err(SeamError::failed(format!(
            "llm batch item `{}` returned status {}",
            result.custom_id, response.status_code
        )));
    }
    response
        .body
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message)
        .ok_or_else(|| SeamError::failed(format!("llm batch item `{}` has no choices", result.custom_id)))
}

fn result_index(custom_id: &str, expected_count: usize) -> Option<usize> {
    custom_id
        .strip_prefix("inseam-")
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|index| *index < expected_count)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parked(model: &str, bytes: usize) -> PendingBatchChat {
        let (sender, _receiver) = oneshot::channel();
        PendingBatchChat {
            model: model.to_string(),
            body: Value::Null,
            bytes,
            response: sender,
        }
    }

    #[test]
    fn a_job_is_the_same_model_prefix_within_both_caps() {
        let mut pending: VecDeque<PendingBatchChat> = VecDeque::from([
            parked("a", 10),
            parked("a", 10),
            parked("b", 10),
            parked("a", 10),
        ]);
        let job = take_job(&mut pending, 10, 1_000);
        assert_eq!(job.len(), 2);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.front().map(|p| p.model.as_str()), Some("b"));

        let mut pending: VecDeque<PendingBatchChat> =
            VecDeque::from([parked("a", 60), parked("a", 60), parked("a", 60)]);
        let by_bytes = take_job(&mut pending, 10, 100);
        assert_eq!(by_bytes.len(), 1, "a second item would cross the byte cap");
        let by_count = take_job(&mut pending, 1, 1_000);
        assert_eq!(by_count.len(), 1);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn an_oversized_single_request_still_forms_a_job() {
        let mut pending: VecDeque<PendingBatchChat> = VecDeque::from([parked("a", 5_000)]);
        assert_eq!(take_job(&mut pending, 10, 100).len(), 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn results_return_in_request_order_and_fail_only_their_own_item() {
        let results: Vec<OpenRouterBatchResult> = serde_json::from_str(
            r#"[
              {"custom_id":"inseam-1","response":{"status_code":200,"body":{"choices":[{"message":{"role":"assistant","content":"second"}}]}},"error":null},
              {"custom_id":"inseam-0","response":{"status_code":200,"body":{"choices":[{"message":{"role":"assistant","content":"first"}}]}},"error":null},
              {"custom_id":"inseam-2","response":null,"error":{"message":"rate limited"}},
              {"custom_id":"inseam-9","response":null,"error":null}
            ]"#,
        )
        .unwrap();

        let messages = results_in_request_order(results, 4);

        assert_eq!(messages[0].as_ref().unwrap().content.as_deref(), Some("first"));
        assert_eq!(messages[1].as_ref().unwrap().content.as_deref(), Some("second"));
        assert!(messages[2].as_ref().is_err_and(|e| e.to_string().contains("rate limited")));
        assert!(messages[3].as_ref().is_err_and(|e| e.to_string().contains("omitted")));
    }

    /// A fake batch endpoint: records every created job, answers the first
    /// poll with `in_progress` and the second with results that echo each
    /// request's user message, marking `custom_id`s in `fail` as errors.
    struct FakeBatches {
        jobs: Mutex<Vec<Value>>,
        polls: Mutex<Vec<String>>,
        fail: Vec<String>,
    }

    impl FakeBatches {
        fn create(self: Arc<Self>, body: Value) -> Value {
            let id = format!("batch_{}", self.jobs.lock().unwrap().len());
            self.jobs.lock().unwrap().push(body);
            json!({"id": id, "status": "validating"})
        }

        fn poll(self: Arc<Self>, id: &str) -> Value {
            let mut polls = self.polls.lock().unwrap();
            let polled_before = polls.iter().filter(|p| *p == id).count();
            polls.push(id.to_string());
            if polled_before == 0 {
                return json!({"id": id, "status": "in_progress"});
            }
            let index: usize = id.strip_prefix("batch_").unwrap().parse().unwrap();
            let job = self.jobs.lock().unwrap()[index].clone();
            let results: Vec<Value> = job["requests"]
                .as_array()
                .unwrap()
                .iter()
                .map(|request| {
                    let custom_id = request["custom_id"].as_str().unwrap();
                    if self.fail.iter().any(|f| f == custom_id) {
                        return json!({"custom_id": custom_id, "response": null, "error": {"message": "boom"}});
                    }
                    let user = request["body"]["messages"][0]["content"].as_str().unwrap();
                    json!({"custom_id": custom_id, "response": {"status_code": 200, "body": {
                        "choices": [{"message": {"role": "assistant", "content": format!("echo:{user}")}}]
                    }}, "error": null})
                })
                .collect();
            json!({"id": id, "status": "completed", "results": results, "usage": {"cost": 0.5}})
        }
    }

    async fn fake_lane(fail: Vec<String>, requests_max: usize) -> (Arc<ChatBatcher>, Arc<FakeBatches>) {
        use axum::extract::{Path, State};
        use axum::routing::{get, post};
        let fake = Arc::new(FakeBatches {
            jobs: Mutex::new(Vec::new()),
            polls: Mutex::new(Vec::new()),
            fail,
        });
        let router = axum::Router::new()
            .route(
                "/batches",
                post(|State(fake): State<Arc<FakeBatches>>, axum::Json(body): axum::Json<Value>| async move {
                    axum::Json(fake.create(body))
                }),
            )
            .route(
                "/batches/{id}",
                get(|State(fake): State<Arc<FakeBatches>>, Path(id): Path<String>| async move {
                    axum::Json(fake.poll(&id))
                }),
            )
            .with_state(Arc::clone(&fake));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("binds");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serves");
        });
        let transport = Arc::new(Transport {
            http: reqwest::Client::new(),
            key: None,
            base_url: format!("{base}/v1"),
            spent: Mutex::new(0.0),
            batch_jobs: std::sync::atomic::AtomicU64::new(0),
        });
        let tuning = ChatBatching {
            batches_url: format!("{base}/batches"),
            requests_max,
            bytes_max: 1024 * 1024,
            quiescence: Duration::from_millis(200),
            age_max: Duration::from_secs(30),
            tick: Duration::from_millis(20),
            poll_initial: Duration::from_millis(20),
            poll_steady: Duration::from_millis(20),
            poll_steady_after: 2,
            poll_max: 50,
            jobs_in_flight_max: 4,
            queue_max: 1_000,
            create_timeout: Duration::from_secs(5),
        };
        (Arc::new(ChatBatcher::new(transport, tuning)), fake)
    }

    fn user_body(text: &str) -> Value {
        json!({"model": "m", "messages": [{"role": "user", "content": text}]})
    }

    fn model_body(model: &str, text: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": text}]})
    }

    #[tokio::test]
    async fn concurrent_calls_share_one_job_and_get_their_own_answers() {
        let (lane, fake) = fake_lane(vec!["inseam-3".to_string()], 100).await;
        let receivers: Vec<_> = (0..8)
            .map(|i| lane.submit("m", user_body(&format!("q{i}"))).expect("parks"))
            .collect();
        let mut answers = Vec::new();
        for receiver in receivers {
            answers.push(receiver.await.expect("the lane replies"));
        }
        assert_eq!(fake.jobs.lock().unwrap().len(), 1, "one job carries every call");
        let job = fake.jobs.lock().unwrap()[0].clone();
        assert_eq!(job["endpoint"], "/v1/chat/completions");
        assert_eq!(job["model"], "m");
        assert_eq!(job["requests"].as_array().unwrap().len(), 8);
        for (i, answer) in answers.iter().enumerate() {
            if i == 3 {
                assert!(answer.as_ref().is_err_and(|e| e.to_string().contains("boom")));
            } else {
                assert_eq!(answer.as_ref().unwrap().content.as_deref(), Some(format!("echo:q{i}").as_str()));
            }
        }
        assert_eq!(lane.transport.batch_jobs.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!((*lane.transport.spent.lock().unwrap() - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_full_queue_splits_into_jobs_and_models_never_mix() {
        let (lane, fake) = fake_lane(Vec::new(), 3).await;
        let mut receivers = Vec::new();
        for i in 0..5 {
            receivers.push(lane.submit("a", model_body("a", &format!("a{i}"))).expect("parks"));
        }
        receivers.push(lane.submit("b", model_body("b", "b0")).expect("parks"));
        for receiver in receivers {
            receiver.await.expect("replies").expect("answers");
        }
        let jobs = fake.jobs.lock().unwrap().clone();
        assert_eq!(jobs.len(), 3, "3 + 2 of model a, 1 of model b");
        assert!(jobs.iter().all(|j| j["requests"].as_array().unwrap().iter().all(|r| r["body"]["model"] == j["model"])));
    }

    #[tokio::test]
    async fn a_caller_that_gives_up_does_not_stall_the_others() {
        let (lane, fake) = fake_lane(Vec::new(), 100).await;
        let dropped = lane.submit("m", user_body("gone")).expect("parks");
        drop(dropped);
        let kept = lane.submit("m", user_body("kept")).expect("parks");
        let answer = kept.await.expect("replies").expect("answers");
        assert_eq!(answer.content.as_deref(), Some("echo:kept"));
        assert_eq!(fake.jobs.lock().unwrap()[0]["requests"].as_array().unwrap().len(), 2);
        // The leader retired; a later call starts a fresh one.
        let again = lane.submit("m", user_body("again")).expect("parks");
        assert_eq!(again.await.expect("replies").expect("answers").content.as_deref(), Some("echo:again"));
        assert_eq!(fake.jobs.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_create_body_names_endpoint_and_model_before_requests() {
        let body = json!({
            "endpoint": "/v1/chat/completions",
            "model": "google/gemini-2.5-flash-lite",
            "requests": [],
        });
        let text = serde_json::to_string(&body).unwrap();
        let endpoint_at = text.find("\"endpoint\"").unwrap();
        let model_at = text.find("\"model\"").unwrap();
        let requests_at = text.find("\"requests\"").unwrap();
        assert!(endpoint_at < requests_at);
        assert!(model_at < requests_at);
    }
}
