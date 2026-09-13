//! Exercise the agent through real offline operations and scripted model replies.
use super::*;
use inseam_kernel::substrate::{Composition, Kernel};
use inseam_seams::llm::{EmbedRequest, ModelInfo, VisionRequest};
use inseam_seams::operations::OPERATIONS;
use std::sync::atomic::{AtomicU32, Ordering};

struct ScriptedLlm {
    replies: Vec<ChatMessage>,
    calls: AtomicU32,
}

#[async_trait::async_trait]
impl Llm for ScriptedLlm {
    async fn chat(&self, _: &ChatRequest) -> Result<ChatMessage, SeamError> {
        let index = usize::try_from(self.calls.fetch_add(1, Ordering::SeqCst)).unwrap();
        assert!(index < self.replies.len());
        Ok(self.replies[index].clone())
    }
    async fn embed(&self, _: &EmbedRequest<'_>) -> Result<Vec<Vec<f32>>, SeamError> {
        unreachable!("the agent does not call the embedder directly")
    }
    async fn describe_image(&self, _: &VisionRequest<'_>) -> Result<String, SeamError> {
        unreachable!("the fixture has no images")
    }
    async fn models(&self, _: bool) -> Result<Vec<ModelInfo>, SeamError> {
        unreachable!("the model is explicit")
    }
    fn spent(&self) -> f64 {
        0.0
    }
}

async fn fixture() -> (tempfile::TempDir, Kernel) {
    let data = tempfile::tempdir().unwrap();
    let mut kernel = Kernel::boot(data.path(), inseam_plugins::factories(), Vec::new())
        .await
        .unwrap();
    let entries = [
        ("connections", "connections"),
        ("fs", "connection-fs"),
        ("transforms", "transforms"),
        ("finder", "finder"),
        ("sweep", "sweep"),
        ("operations", "operations"),
    ];
    let mut configuration = entries
        .iter()
        .map(|(id, plugin)| format!("[[entry]]\nid = \"{id}\"\nplugin = \"{plugin}\"\n"))
        .collect::<String>();
    configuration.push_str(
        r#"
[[entry]]
id = "embedder"
plugin = "embedder"
[entry.config]
provider = "hashed"
model = "hashed"
dimensions = 64
"#,
    );
    kernel
        .reconcile(&Composition::parse(&configuration, "agent test").unwrap())
        .await
        .unwrap();
    (data, kernel)
}

async fn run(replies: Vec<ChatMessage>) -> Result<AgentOutcome, AgentError> {
    let (_data, kernel) = fixture().await;
    let operations = kernel.service(&OPERATIONS).unwrap();
    let llm = ScriptedLlm {
        replies,
        calls: AtomicU32::new(0),
    };
    let outcome = run_agent(
        operations.as_ref(),
        &llm,
        "scripted",
        "fixture",
        1,
        None,
        |_| {},
    )
    .await;
    assert_eq!(llm.calls.load(Ordering::SeqCst), 2);
    outcome
}

fn answer(text: &str) -> ChatMessage {
    ChatMessage {
        role: Role::Assistant,
        content: Some(text.into()),
        tool_calls: None,
        tool_call_id: None,
    }
}

#[tokio::test]
async fn empty_review_preserves_completed_draft() {
    let result = run(vec![answer("Supported draft."), answer("  ")])
        .await
        .unwrap();
    assert_eq!(result.answer, "Supported draft.");
}

#[tokio::test]
async fn nonempty_review_replaces_draft() {
    let result = run(vec![answer("Draft."), answer("Reviewed answer.")])
        .await
        .unwrap();
    assert_eq!(result.answer, "Reviewed answer.");
}

#[tokio::test]
async fn empty_draft_and_review_report_empty_answer() {
    let result = run(vec![answer(""), answer("")]).await;
    assert!(matches!(result, Err(AgentError::EmptyAnswer(1))));
}

#[tokio::test]
async fn tool_call_preamble_is_not_a_fallback_answer() {
    let mut preamble = assistant_calls(vec![initial_call("fixture")]);
    preamble.content = Some("I will search.".into());
    let result = run(vec![preamble, answer("")]).await;
    assert!(matches!(result, Err(AgentError::EmptyAnswer(1))));
}

#[tokio::test]
async fn batch_preserves_each_question_in_a_separate_record() {
    let (_data, kernel) = fixture().await;
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("questions.jsonl");
    let questions = (0..10)
        .map(|index| format!("{{\"question_id\":\"q_{index}\",\"question\":\"fixture\"}}\n"))
        .collect::<String>();
    std::fs::write(&input, questions).unwrap();
    let llm = std::sync::Arc::new(ScriptedLlm {
        replies: vec![answer("Supported answer."); 20],
        calls: AtomicU32::new(0),
    });
    crate::agent_batch::run(
        kernel.service(&OPERATIONS).unwrap(),
        llm.clone(),
        "scripted".into(),
        &input,
        directory.path(),
        1,
        Some("low".into()),
    )
    .await
    .unwrap();
    for index in 0..10 {
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join(format!("q_{index}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(value["question_id"], format!("q_{index}"));
        assert_eq!(value["answer"], "Supported answer.");
    }
    assert_eq!(llm.calls.load(Ordering::SeqCst), 20);
}

#[tokio::test]
async fn batch_rejects_duplicate_and_unsafe_ids_before_model_calls() {
    let (_data, kernel) = fixture().await;
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("questions.jsonl");
    let llm = std::sync::Arc::new(ScriptedLlm {
        replies: Vec::new(),
        calls: AtomicU32::new(0),
    });
    for text in [
        "{\"question_id\":\"../escape\",\"question\":\"fixture\"}\n",
        "{\"question_id\":\"q\",\"question\":\"fixture\"}\n{\"question_id\":\"q\",\"question\":\"fixture\"}\n",
        "",
    ] {
        std::fs::write(&input, text).unwrap();
        let result = crate::agent_batch::run(
            kernel.service(&OPERATIONS).unwrap(),
            llm.clone(),
            "scripted".into(),
            &input,
            directory.path(),
            1,
            None,
        )
        .await;
        assert!(result.is_err());
    }
    assert_eq!(llm.calls.load(Ordering::SeqCst), 0);
}
