//! End-to-end through the composed kernel: boot the offline composition
//! (hashed embeddings, no LLM), index a temp corpus, then walk the whole
//! incremental-discovery ladder — query, expand, scan, fetch — exactly as an
//! external client would (`design/node-api.md`).

mod common;

use std::path::Path;

use inseam_seams::operations::{
    ExpandRequest, FetchRequest, IndexRequest, QueryRequest, ScanRequest,
};

fn write_corpus(dir: &Path) {
    std::fs::write(
        dir.join("kitchen.md"),
        "# Kitchen Renovation\n\nBudget and vendor notes.\n\n## Budget\n\nCabinets 12k, \
         appliances 8k, [moodboard](https://example.com/mood).\n\n## Schedule\n\nDemolition in June, \
         drywall in July.\n",
    )
    .expect("writes");
    std::fs::write(
        dir.join("standup.md"),
        "# Standup Notes\n\nShipping the parser rewrite this week.\n",
    )
    .expect("writes");
    std::fs::write(
        dir.join("chili.txt"),
        "Chili recipe\n\nBrown the beef with cumin and smoked paprika.\nSimmer two hours.\n",
    )
    .expect("writes");
    std::fs::write(dir.join("logo.png"), [0x89, 0x50, 0x4e, 0x47, 0, 0, 0, 0]).expect("writes");
}

#[tokio::test]
async fn the_incremental_discovery_ladder_works_offline() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    write_corpus(corpus.path());

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let root = corpus.path().display().to_string();

    // --- index (owner operation) ---
    let report = ops
        .index(IndexRequest {
            host: None,
            root: root.clone(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes");
    assert_eq!(report.sources_seen, 4);
    assert_eq!(report.indexed, 4);
    assert_eq!(report.llm_summaries, 0, "no llm mounted, no llm calls");
    // Binary png gets an envelope summary; text gets extractive.
    assert_eq!(report.envelope_summaries, 1);
    assert_eq!(report.extractive_summaries, 3);
    assert!(report.fragments > 8, "sections + summaries: {report}");
    assert!(report.spent == 0.0);

    // Second run: everything unchanged, nothing rebuilt.
    let again = ops
        .index(IndexRequest {
            host: None,
            root: root.clone(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes");
    assert_eq!(again.unchanged, 4);
    assert_eq!(again.indexed, 0);

    // --- rung 1: query ---
    let response = ops
        .query(QueryRequest {
            text: "kitchen renovation budget".into(),
            limit: 5,
        })
        .await
        .expect("queries");
    assert!(!response.results.is_empty());
    let top = &response.results[0];
    assert!(
        top.address.to_string().ends_with("kitchen.md"),
        "expected kitchen.md first, got {}",
        top.address
    );
    assert_eq!(top.score, 1.0, "top result normalizes to 1.0");
    let summary = top.summary.as_deref().expect("mandatory summary");
    assert!(summary.contains("Kitchen"), "summary: {summary}");
    assert!(!top.hints.is_empty(), "hints accompany results");
    let hint_extent = top.hints[0].extent.as_deref().expect("hints carry extents");
    assert!(hint_extent.starts_with("lines "), "got {hint_extent}");

    // --- rung 2: expand ---
    let expansion = ops
        .expand(ExpandRequest {
            address: top.address.clone(),
        })
        .await
        .expect("expands");
    let mimetypes: Vec<&str> = expansion
        .fragments
        .iter()
        .map(|f| f.mimetype.as_str())
        .collect();
    assert!(mimetypes.contains(&"text/markdown"));
    assert!(
        mimetypes
            .iter()
            .any(|m| m.starts_with("text/x-inseam-summary")),
        "summary fragment present, with via provenance: {mimetypes:?}"
    );
    assert!(
        mimetypes.contains(&"text/uri-list"),
        "the moodboard link is a fragment: {mimetypes:?}"
    );
    assert!(!expansion.relations.is_empty());
    assert!(
        expansion.relations.iter().any(|r| r.kind == "contains"),
        "structural edges present"
    );
    assert!(
        expansion.relations.iter().any(|r| r.kind == "links-to"),
        "link edges present"
    );

    // --- rung 3: scan the lines a hint pointed at ---
    let budget_fragment = expansion
        .fragments
        .iter()
        .find(|f| f.text.as_deref().is_some_and(|t| t.contains("Cabinets")))
        .expect("budget section fragment");
    let extent = budget_fragment.extent.as_deref().expect("has extent");
    let (start, end) = parse_lines_extent(extent);
    let scan = ops
        .scan(ScanRequest {
            address: top.address.clone(),
            start,
            end,
        })
        .await
        .expect("scans");
    assert!(
        scan.text.contains("Cabinets 12k"),
        "scanned the right lines: {}",
        scan.text
    );
    assert!(scan.served_from_fragment.is_none(), "text scans read the source");

    // --- rung 4: fetch ---
    let fetched = ops
        .fetch(FetchRequest {
            address: top.address.clone(),
        })
        .await
        .expect("fetches");
    assert!(fetched.text.contains("# Kitchen Renovation"));

    // Binary sources refuse the JSON fetch surface for now.
    let png = response
        .results
        .iter()
        .map(|r| &r.address)
        .find(|a| a.to_string().ends_with("logo.png"));
    if let Some(png) = png {
        assert!(ops
            .fetch(FetchRequest {
                address: png.clone()
            })
            .await
            .is_err());
    }
}

#[tokio::test]
async fn rebuild_reindexes_unchanged_sources() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("a.md"), "# A\n\nalpha beta\n").expect("writes");

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let root = corpus.path().display().to_string();
    ops.index(IndexRequest {
        host: None,
        root: root.clone(),
        rebuild: false,
        deep_budget: None,
        llm_lane: None,
    })
    .await
    .expect("indexes");
    let rebuilt = ops
        .index(IndexRequest {
            host: None,
            root: root.clone(),
            rebuild: true,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("rebuilds");
    assert_eq!(rebuilt.indexed, 1);
    assert_eq!(rebuilt.unchanged, 0);

    // The index stays queryable and singular after the rebuild (no dupes).
    assert_eq!(common::hits(ops.as_ref(), "alpha").await, 1);
}

#[tokio::test]
async fn changed_sources_have_their_subtree_replaced() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let file = corpus.path().join("note.md");
    std::fs::write(&file, "# Old Title\n\nold body about ferns\n").expect("writes");

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let root = corpus.path().display().to_string();
    ops.index(IndexRequest {
        host: None,
        root: root.clone(),
        rebuild: false,
        deep_budget: None,
        llm_lane: None,
    })
    .await
    .expect("indexes");

    std::fs::write(&file, "# New Title\n\nnew body about orchids entirely\n").expect("writes");
    let report = ops
        .index(IndexRequest {
            host: None,
            root,
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes");
    assert_eq!(report.indexed, 1, "mtime/size change re-indexes");

    assert_eq!(common::hits(ops.as_ref(), "ferns").await, 0, "stale fragments gone");
    assert_eq!(common::hits(ops.as_ref(), "orchids").await, 1);
}

fn parse_lines_extent(extent: &str) -> (u64, u64) {
    let range = extent.strip_prefix("lines ").expect("lines extent");
    let (start, end) = range.split_once('-').expect("start-end");
    (
        start.parse().expect("start parses"),
        end.parse().expect("end parses"),
    )
}
