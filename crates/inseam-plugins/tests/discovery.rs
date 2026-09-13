//! Additive discovery through real guarded operations, with no model or network.
mod common;

use inseam_seams::discovery::{DiscoveryError, DiscoverySession, FindRequest, RESPONSE_CHARS_MAX};
use inseam_seams::operations::IndexRequest;
use serde_json::{Value, json};

async fn fixture(
    text: &str,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    inseam_kernel::substrate::Kernel,
) {
    let corpus = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(corpus.path().join("evidence.txt"), text).unwrap();
    let kernel = common::boot(
        data.path(),
        r#"
[[entry]]
id = "summarizer"
[entry.config]
target_chars = 24000
llm_call_budget = 0
"#,
    )
    .await;
    common::ops(&kernel)
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .unwrap();
    (corpus, data, kernel)
}

async fn find(
    session: &mut DiscoverySession,
    kernel: &inseam_kernel::substrate::Kernel,
    request: Value,
) -> Value {
    let request = FindRequest::parse(&request.to_string()).unwrap();
    let response = session
        .find(common::ops(kernel).as_ref(), request)
        .await
        .unwrap();
    let serialized = response.to_string();
    assert!(serialized.chars().count() <= RESPONSE_CHARS_MAX);
    assert_eq!(
        serde_json::from_str::<Value>(&serialized).unwrap(),
        response
    );
    response
}

fn candidate(response: &Value) -> &Value {
    response["results"][0]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["address"].as_str().unwrap().ends_with("evidence.txt"))
        .unwrap()
}

#[tokio::test]
async fn subsequent_searches_preserve_candidate_ids_and_read_history() {
    let (_corpus, _data, kernel) =
        fixture("# Xylophone\nSapphire calibration uses 42 milliseconds.\n").await;
    let mut session = DiscoverySession::default();
    let first = find(
        &mut session,
        &kernel,
        json!({"queries":[{"text":"xylophone","limit":8}]}),
    )
    .await;
    let id = candidate(&first)["id"].as_u64().unwrap();
    let batch = find(
        &mut session,
        &kernel,
        json!({
        "queries":[{"text":"sapphire calibration","limit":8}],
        "scan":[{"source":id,"start":1,"end":2}], "expand":[{"source":id}]}),
    )
    .await;
    assert_eq!(candidate(&batch)["id"], id);
    assert_eq!(batch["results"][1]["operation"], "expand");
    assert_eq!(batch["results"][2]["operation"], "scan");
    assert!(
        batch["results"][2]["text"]
            .as_str()
            .unwrap()
            .contains("42 milliseconds")
    );
    let inspected = find(&mut session, &kernel, json!({"inspect":[id]})).await;
    assert_eq!(
        inspected["results"][0]["read_windows"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let later = find(
        &mut session,
        &kernel,
        json!({"queries":[{"text":"zzzzunmatched","limit":8}]}),
    )
    .await;
    assert_eq!(later["retained_count"], batch["retained_count"]);
    let forgotten = find(&mut session, &kernel, json!({"forget":[id],"inspect":[id]})).await;
    assert_eq!(forgotten["forgotten"], json!([id]));
    assert!(forgotten["results"][0]["error"].is_string());
}

#[tokio::test]
async fn relevant_indexed_excerpts_include_evidence_beyond_the_prefix() {
    let body = format!(
        "# Calibration\n{}\nSapphire zirconium finalization metric counter labels.\n",
        "Ordinary background discussion. ".repeat(90)
    );
    let (_corpus, _data, kernel) = fixture(&body).await;
    let mut session = DiscoverySession::default();
    let result = find(
        &mut session,
        &kernel,
        json!({"queries":[{"text":"sapphire zirconium finalization metric","limit":8}]}),
    )
    .await;
    let row = candidate(&result);
    assert!(row["indexed_summary_chars"].as_u64().unwrap() > 500);
    assert!(row["lines_total"].as_u64().unwrap() > 0);
    assert!(row["excerpts"].as_array().unwrap().iter().any(|excerpt| {
        excerpt["text"]
            .as_str()
            .unwrap()
            .contains("Sapphire zirconium")
    }));
    for excerpt in row["excerpts"].as_array().unwrap() {
        assert_eq!(excerpt["origin"], "indexed_summary");
        assert!(excerpt["source_lines"].is_null());
    }
}

#[tokio::test]
async fn one_failed_action_preserves_other_reads() {
    let (corpus, _data, kernel) = fixture("# Evidence\nA supported fact.\n").await;
    let address = common::address_of(&kernel, &corpus.path().join("evidence.txt"));
    let mut session = DiscoverySession::default();
    let result = find(
        &mut session,
        &kernel,
        json!({"scan":[
        {"source":9999,"start":1,"end":2},
        {"source":address,"start":1,"end":2}]}),
    )
    .await;
    assert!(result["results"][0]["error"].is_string());
    assert!(
        result["results"][1]["text"]
            .as_str()
            .unwrap()
            .contains("supported fact")
    );
}

#[tokio::test]
async fn long_unicode_lines_continue_without_losing_or_duplicating_text() {
    let body = format!("{}\n", "中\\\"🙂".repeat(9_000));
    let (corpus, _data, kernel) = fixture(&body).await;
    let address = common::address_of(&kernel, &corpus.path().join("evidence.txt"));
    let mut session = DiscoverySession::default();
    let mut request = json!({"source":address,"start":1,"end":1});
    let mut assembled = String::new();
    let mut completed = false;
    for _ in 0..64 {
        let response = find(&mut session, &kernel, json!({"scan":[request]})).await;
        let read = &response["results"][0];
        assembled.push_str(read["text"].as_str().unwrap());
        request = read["next"].clone();
        if request.is_null() {
            completed = true;
            break;
        }
        assert!(request["offset_chars"].as_u64().unwrap() > 0);
    }
    assert!(completed);
    assert_eq!(assembled, body.trim_end_matches('\n'));
}

#[tokio::test]
async fn continuation_refuses_a_changed_source_window() {
    let (corpus, _data, kernel) = fixture(&"x".repeat(30_000)).await;
    let path = corpus.path().join("evidence.txt");
    let address = common::address_of(&kernel, &path);
    let mut session = DiscoverySession::default();
    let first = find(
        &mut session,
        &kernel,
        json!({"scan":[{"source":address,"start":1,"end":1}]}),
    )
    .await;
    let next = &first["results"][0]["next"];
    assert!(!next.is_null());
    std::fs::write(path, "y".repeat(30_000)).unwrap();
    let second = find(&mut session, &kernel, json!({"scan":[next]})).await;
    assert!(second["results"][0]["error"].is_string());
    assert!(second["results"][0].get("text").is_none());
}

#[test]
fn rejects_empty_oversized_and_invalid_requests() {
    for request in [
        json!({}),
        json!({"queries":[{"text":""}]}),
        json!({"queries":[{"text":"x","limit":26}]}),
        json!({"inspect":vec![1;9]}),
        json!({"scan":[{"source":1,"start":0,"end":1}]}),
        json!({"scan":[{"source":1,"start":2,"end":1}]}),
    ] {
        assert!(matches!(
            FindRequest::parse(&request.to_string()),
            Err(DiscoveryError::Request(_))
        ));
    }
    assert!(matches!(
        FindRequest::parse("{"),
        Err(DiscoveryError::Json(_))
    ));
    assert!(matches!(
        FindRequest::parse(&" ".repeat(32_001)),
        Err(DiscoveryError::Request(_))
    ));
}

#[tokio::test]
async fn omitted_candidate_details_are_recoverable_without_requerying() {
    let (corpus, _data, kernel) = fixture("Shared prism evidence.").await;
    for index in 0..30 {
        std::fs::write(
            corpus.path().join(format!("prism-{index}.txt")),
            format!(
                "Shared prism evidence {index}. {}",
                "Detailed prism background. ".repeat(80)
            ),
        )
        .unwrap();
    }
    common::ops(&kernel)
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .unwrap();
    let mut session = DiscoverySession::default();
    let batch = find(
        &mut session,
        &kernel,
        json!({"queries":vec![json!({"text":"prism","limit":25});8]}),
    )
    .await;
    let omitted = batch["results"][0]["omitted_candidate_ids"]
        .as_array()
        .unwrap();
    assert!(!omitted.is_empty());
    let inspected = find(&mut session, &kernel, json!({"inspect":[omitted[0]]})).await;
    assert_eq!(inspected["results"][0]["id"], omitted[0]);
    assert!(inspected["results"][0]["address"].is_string());
}

#[tokio::test]
async fn collection_capacity_is_explicit_and_does_not_evict_earlier_candidates() {
    let (corpus, _data, kernel) = fixture("Calibration reference.").await;
    for index in 0..105 {
        std::fs::write(
            corpus.path().join(format!("item-{index}.txt")),
            format!("quartzidentifier{index:04} ").repeat(20),
        )
        .unwrap();
    }
    common::ops(&kernel)
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .unwrap();
    let mut session = DiscoverySession::default();
    let mut rejected = 0;
    for index in 0..105 {
        let response = find(
            &mut session,
            &kernel,
            json!({"queries":[{"text":format!("quartzidentifier{index:04}"),"limit":1}]}),
        )
        .await;
        rejected += response["results"][0]["unretained_count"].as_u64().unwrap();
        assert!(response["retained_count"].as_u64().unwrap() <= 100);
    }
    assert!(rejected > 0);
    let first = find(&mut session, &kernel, json!({"inspect":[1]})).await;
    assert_eq!(first["results"][0]["id"], 1);
    assert!(
        first["results"][0]["address"]
            .as_str()
            .unwrap()
            .ends_with("item-0.txt")
    );
}

#[tokio::test]
async fn changed_indexed_version_invalidates_old_read_coverage() {
    let (corpus, _data, kernel) = fixture("Sapphire original evidence.\n").await;
    let mut session = DiscoverySession::default();
    let first = find(
        &mut session,
        &kernel,
        json!({"queries":[{"text":"sapphire","limit":8}]}),
    )
    .await;
    let id = candidate(&first)["id"].as_u64().unwrap();
    find(
        &mut session,
        &kernel,
        json!({"scan":[{"source":id,"start":1,"end":1}]}),
    )
    .await;
    std::fs::write(
        corpus.path().join("evidence.txt"),
        "Sapphire replacement evidence differs.\n",
    )
    .unwrap();
    common::ops(&kernel)
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: true,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .unwrap();
    let updated = find(
        &mut session,
        &kernel,
        json!({"queries":[{"text":"sapphire","limit":8}]}),
    )
    .await;
    assert_eq!(candidate(&updated)["id"], id);
    assert!(
        candidate(&updated)["read_windows"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
