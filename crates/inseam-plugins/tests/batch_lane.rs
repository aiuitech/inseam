//! The batch lane end to end (`design/indexing.md`): with `--batch`, every
//! planner of a run parks on its summary call and one batch-API job carries
//! them all; without it, the same composition answers each summary with its
//! own request. The lane is a run choice, never a shape: both runs build the
//! same index and the same stamp.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::{get, post};
use serde_json::{Value, json};

use inseam_seams::llm::LlmLane;
use inseam_seams::operations::IndexRequest;

/// A fake OpenRouter: synchronous chat completions, and a batch API whose
/// jobs complete on their second poll, echoing each request's file name.
#[derive(Default)]
struct FakeOpenRouter {
    chat_calls: AtomicUsize,
    jobs: Mutex<Vec<Value>>,
    polls: Mutex<Vec<String>>,
}

fn summary_of(body: &Value) -> String {
    let user = body["messages"][1]["content"].as_str().unwrap_or("");
    let name = user
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches("File: ");
    format!("summary of {name}")
}

fn completion(content: String) -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": content}}], "usage": {"cost": 0.001}})
}

async fn chat(
    State(fake): State<Arc<FakeOpenRouter>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    fake.chat_calls.fetch_add(1, Ordering::Relaxed);
    axum::Json(completion(summary_of(&body)))
}

async fn create_batch(
    State(fake): State<Arc<FakeOpenRouter>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    let mut jobs = fake.jobs.lock().expect("lock");
    let id = format!("batch_{}", jobs.len());
    jobs.push(body);
    axum::Json(json!({"id": id, "status": "validating"}))
}

async fn poll_batch(
    State(fake): State<Arc<FakeOpenRouter>>,
    Path(id): Path<String>,
) -> axum::Json<Value> {
    let mut polls = fake.polls.lock().expect("lock");
    let polled_before = polls.iter().filter(|p| **p == id).count();
    polls.push(id.clone());
    if polled_before == 0 {
        return axum::Json(json!({"id": id, "status": "in_progress"}));
    }
    let index: usize = id
        .strip_prefix("batch_")
        .expect("our id")
        .parse()
        .expect("our id");
    let job = fake.jobs.lock().expect("lock")[index].clone();
    let results: Vec<Value> = job["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|request| {
            json!({
                "custom_id": request["custom_id"],
                "response": {"status_code": 200, "body": completion(summary_of(&request["body"]))},
                "error": null
            })
        })
        .collect();
    axum::Json(
        json!({"id": id, "status": "completed", "results": results, "usage": {"cost": 0.01}}),
    )
}

async fn serve_fake() -> (Arc<FakeOpenRouter>, String) {
    let fake = Arc::new(FakeOpenRouter::default());
    let router = axum::Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/batches", post(create_batch))
        .route("/batches/{id}", get(poll_batch))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serves");
    });
    (fake, base)
}

fn overlay(base: &str) -> String {
    format!(
        r#"
[[entry]]
id = "llm"
plugin = "llm-endpoint"
[entry.config]
base_url = "{base}/v1"
api_key_env = ""
batches_url = "{base}/batches"
transform_model = "fake/model"

[[entry]]
id = "summarizer"
plugin = "transform-summarizer"
[entry.config]
llm_call_budget = 1000
# The corpus notes are a line each; a target they exceed keeps every
# summary a model call, which is what the lane tests count.
target_chars = 20
"#
    )
}

fn write_corpus(dir: &std::path::Path, count: usize) {
    for i in 0..count {
        std::fs::write(
            dir.join(format!("note{i}.md")),
            format!("# Note {i}\n\nwords about topic {i} and more words\n"),
        )
        .expect("write");
    }
}

async fn index_lane(
    lane: Option<LlmLane>,
) -> (
    inseam_seams::sweep::IndexReport,
    Arc<FakeOpenRouter>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let (fake, base) = serve_fake().await;
    let corpus = tempfile::tempdir().expect("tempdir");
    write_corpus(corpus.path(), 24);
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(data.path(), &overlay(&base)).await;
    let report = common::ops(&kernel)
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: lane,
        })
        .await
        .expect("sweeps");
    (report, fake, corpus, data)
}

#[tokio::test]
async fn a_batch_run_parks_every_summary_into_one_job() {
    let (report, fake, _corpus, _data) = index_lane(Some(LlmLane::Batch)).await;
    // 24 notes and the folder holding them; the folder plans after the
    // notes land, so its summary is a second, one-request job.
    assert_eq!(report.indexed, 25, "{report}");
    assert_eq!(report.llm_summaries, 25, "{report}");
    assert_eq!(report.llm_batch_jobs, 2, "{report}");
    assert_eq!(
        fake.chat_calls.load(Ordering::Relaxed),
        0,
        "no synchronous calls"
    );
    let jobs = fake.jobs.lock().expect("lock");
    assert_eq!(jobs.len(), 2);
    assert_eq!(
        jobs[0]["model"], "fake/model",
        "the job names the base model"
    );
    assert_eq!(jobs[0]["requests"].as_array().expect("requests").len(), 24);
    assert_eq!(jobs[1]["requests"].as_array().expect("requests").len(), 1);
    assert!(report.spent > 0.0, "job usage is charged: {report}");
}

#[tokio::test]
async fn an_interactive_run_answers_each_summary_with_its_own_request() {
    let (report, fake, _corpus, _data) = index_lane(None).await;
    assert_eq!(report.indexed, 25, "{report}");
    assert_eq!(report.llm_summaries, 25, "{report}");
    assert_eq!(report.llm_batch_jobs, 0, "{report}");
    assert_eq!(fake.chat_calls.load(Ordering::Relaxed), 25);
    assert!(fake.jobs.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn switching_lanes_never_re_indexes() {
    let (fake, base) = serve_fake().await;
    let corpus = tempfile::tempdir().expect("tempdir");
    write_corpus(corpus.path(), 4);
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(data.path(), &overlay(&base)).await;
    let ops = common::ops(&kernel);
    let request = |lane| IndexRequest {
        host: None,
        root: corpus.path().display().to_string(),
        rebuild: false,
        deep_budget: None,
        llm_lane: lane,
    };
    let first = ops
        .index(request(Some(LlmLane::Batch)))
        .await
        .expect("sweeps");
    assert_eq!(first.indexed, 5, "{first}");
    let second = ops.index(request(None)).await.expect("sweeps");
    assert_eq!(
        second.indexed, 0,
        "the interactive run finds nothing dirty: {second}"
    );
    assert_eq!(second.unchanged, 5, "{second}");
    assert_eq!(fake.chat_calls.load(Ordering::Relaxed), 0);
}

/// The digest-keyed caches (`design/indexing.md`): a rebuild re-runs
/// decomposition but takes every LLM summary and every vector from the
/// caches, and a shape change that leaves the summarizer's identity alone
/// (an unrelated transform's config) does the same.
#[tokio::test]
async fn rebuilds_and_shape_changes_reuse_cached_summaries_and_vectors() {
    let (fake, base) = serve_fake().await;
    let corpus = tempfile::tempdir().expect("tempdir");
    // Six notes plus the folder itself, which is a source composed from
    // its children's summaries and summarized like any file.
    write_corpus(corpus.path(), 6);
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = common::boot(data.path(), &overlay(&base)).await;
    let request = |rebuild| IndexRequest {
        host: None,
        root: corpus.path().display().to_string(),
        rebuild,
        deep_budget: None,
        llm_lane: None,
    };
    let first = common::ops(&kernel)
        .index(request(false))
        .await
        .expect("sweeps");
    assert_eq!(first.llm_summaries, 7, "{first}");
    assert_eq!(first.transforms_reused, 0, "{first}");
    assert_eq!(first.embeddings_reused, 0, "{first}");
    assert!(first.embedded > 7, "{first}");
    assert_eq!(fake.chat_calls.load(Ordering::Relaxed), 7);

    let rebuilt = common::ops(&kernel)
        .index(request(true))
        .await
        .expect("sweeps");
    assert_eq!(rebuilt.indexed, 7, "{rebuilt}");
    assert_eq!(
        rebuilt.llm_summaries, 7,
        "cached summaries are still the model's: {rebuilt}"
    );
    assert_eq!(rebuilt.transforms_reused, 7, "{rebuilt}");
    assert_eq!(rebuilt.embeddings_reused, first.embedded, "{rebuilt}");
    assert_eq!(rebuilt.embedded, 0, "{rebuilt}");
    assert_eq!(
        fake.chat_calls.load(Ordering::Relaxed),
        7,
        "no new endpoint calls"
    );

    // A sweep shape change dirties every note but leaves the summarizer's
    // identity alone, so its outputs still hit; the texts are unchanged,
    // so every vector does too.
    let reshaped_overlay = format!(
        "{}\n[[entry]]\nid = \"sweep\"\n[entry.config]\nmax_depth = 3\n",
        overlay(&base)
    );
    common::reconcile(&mut kernel, &reshaped_overlay).await;
    let reshaped = common::ops(&kernel)
        .index(request(false))
        .await
        .expect("sweeps");
    assert_eq!(reshaped.indexed, 7, "{reshaped}");
    assert_eq!(reshaped.transforms_reused, 7, "{reshaped}");
    assert_eq!(reshaped.embeddings_reused, first.embedded, "{reshaped}");
    assert_eq!(reshaped.embedded, 0, "{reshaped}");
    assert_eq!(
        fake.chat_calls.load(Ordering::Relaxed),
        7,
        "no new endpoint calls"
    );

    // A summarizer config change is a new identity: the model is asked
    // again. The new target still sits under the notes' length, so every
    // summary stays a model call.
    let resummarized_overlay = overlay(&base).replace("target_chars = 20\n", "target_chars = 24\n");
    assert_ne!(resummarized_overlay, overlay(&base));
    common::reconcile(&mut kernel, &resummarized_overlay).await;
    let resummarized = common::ops(&kernel)
        .index(request(false))
        .await
        .expect("sweeps");
    assert_eq!(resummarized.indexed, 7, "{resummarized}");
    assert_eq!(resummarized.transforms_reused, 0, "{resummarized}");
    assert_eq!(fake.chat_calls.load(Ordering::Relaxed), 14);
}
