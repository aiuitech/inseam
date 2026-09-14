//! Bounded benchmark sessions sharing one node, with a durable file per answer.
use crate::agent::{AgentEvent, run_agent};
use anyhow::{Context, bail};
use inseam_seams::llm::Llm;
use inseam_seams::operations::{Operations, QueryRequest};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinSet;

const QUESTIONS_MAX: usize = 500;
const INPUT_BYTES_MAX: u64 = 2_000_000;
const WORKERS_MAX: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Question {
    question_id: String,
    question: String,
}

pub async fn run(
    operations: Arc<dyn Operations>,
    llm: Arc<dyn Llm>,
    model: String,
    input: &Path,
    output: &Path,
    turns: usize,
    reasoning_effort: Option<String>,
) -> anyhow::Result<()> {
    let questions = read_questions(input)?;
    if !(1..=64).contains(&turns) {
        bail!("turns must be between 1 and 64");
    }
    std::fs::create_dir_all(output)?;
    let mut pending = JoinSet::new();
    let mut failed = 0_u32;
    for question in questions {
        if pending.len() == WORKERS_MAX {
            failed += collect(&mut pending).await?;
        }
        let operations = Arc::clone(&operations);
        let llm = Arc::clone(&llm);
        let model = model.clone();
        let reasoning_effort = reasoning_effort.clone();
        let output = output.to_path_buf();
        pending.spawn(async move {
            let id = question.question_id.clone();
            let result = answer(
                operations,
                llm,
                &model,
                question,
                &output,
                turns,
                reasoning_effort.as_deref(),
            )
            .await;
            if let Err(error) = &result {
                write_record(
                    &output.join(format!("{id}.error.json")),
                    &json!({"error":error.to_string()}),
                )?;
            }
            println!(
                "{id}: {}",
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                }
            );
            Ok::<bool, anyhow::Error>(result.is_ok())
        });
    }
    for _ in 0..WORKERS_MAX {
        if pending.is_empty() {
            break;
        }
        failed += collect(&mut pending).await?;
    }
    assert!(pending.is_empty());
    write_record(
        &output.join("batch-usage.json"),
        &json!({"provider_cost_usd":llm.spent(),
        "workers_max":WORKERS_MAX,"failed":failed,"note":"Shared provider total, not per-question cost."}),
    )?;
    if failed > 0 {
        bail!("{failed} questions failed; completed answers are preserved");
    }
    Ok(())
}

async fn collect(pending: &mut JoinSet<anyhow::Result<bool>>) -> anyhow::Result<u32> {
    let success = pending
        .join_next()
        .await
        .context("missing pending answer")???;
    Ok(u32::from(!success))
}

fn read_questions(path: &Path) -> anyhow::Result<Vec<Question>> {
    if std::fs::metadata(path)?.len() > INPUT_BYTES_MAX {
        bail!("question input exceeds two megabytes");
    }
    let text = std::fs::read_to_string(path)?;
    let mut questions = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for line in text.lines().take(QUESTIONS_MAX + 1) {
        let question: Question = serde_json::from_str(line)?;
        let id = &question.question_id;
        if id.is_empty() {
            bail!("question ID is empty");
        }
        if id.len() > 80 {
            bail!("question ID exceeds 80 bytes");
        }
        if !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            bail!("question ID must contain only letters, digits, and underscores");
        }
        if !ids.insert(id.clone()) {
            bail!("duplicate question ID");
        }
        if question.question.chars().count() > 2000 {
            bail!("question exceeds Finder's 2000-character bound");
        }
        questions.push(question);
    }
    if questions.is_empty() {
        bail!("question input is empty");
    }
    if questions.len() > QUESTIONS_MAX {
        bail!("question input exceeds 500 questions");
    }
    Ok(questions)
}

async fn answer(
    operations: Arc<dyn Operations>,
    llm: Arc<dyn Llm>,
    model: &str,
    question: Question,
    output: &Path,
    turns: usize,
    reasoning_effort: Option<&str>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let retrieval = operations
        .query(QueryRequest { text: question.question.clone(), limit: 50 })
        .await?;
    let retrieval_seconds = started.elapsed().as_secs_f64();
    let mut events: Vec<Value> = Vec::new();
    let outcome = run_agent(
        operations.as_ref(),
        llm.as_ref(),
        model,
        &question.question,
        turns,
        reasoning_effort,
        |event| {
            assert!(events.len() < 2 * (1 + 8 * 64));
            events.push(match event {
                AgentEvent::ToolCall { name, arguments } => {
                    json!({"kind":"call","name":name,"arguments":arguments})
                }
                AgentEvent::ToolResult { name, brief } => {
                    json!({"kind":"result","name":name,"brief":brief})
                }
            });
        },
    )
    .await?;
    let record = json!({"question_id":question.question_id,"question":question.question,
        "answer":outcome.answer,"turns":outcome.turns,"tool_calls":outcome.tool_calls,
        "retrieval":retrieval,"retrieval_duration_seconds":retrieval_seconds,
        "answer_duration_seconds":started.elapsed().as_secs_f64()-retrieval_seconds,
        "duration_seconds":started.elapsed().as_secs_f64(),"events":events});
    write_record(
        &output.join(format!("{}.json", question.question_id)),
        &record,
    )
}

fn write_record(path: &Path, record: &Value) -> anyhow::Result<()> {
    let text = serde_json::to_vec(record)?;
    if text.len() > 16_000_000 {
        bail!("answer record exceeds 16 megabytes");
    }
    let temporary: PathBuf = path.with_extension("pending");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}
