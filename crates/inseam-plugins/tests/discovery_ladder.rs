//! End-to-end through the composed kernel: boot the offline composition
//! (hashed embeddings, no LLM), index a temp corpus, then walk the whole
//! incremental-discovery ladder — query, expand, scan, fetch — exactly as an
//! external client would (`design/node-api.md`).

mod common;

use std::path::Path;

use inseam_kernel::address::ContentLength;
use inseam_kernel::fragment::Extent;
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
    // Four files and the folder holding them.
    assert_eq!(report.sources_seen, 5);
    assert_eq!(report.indexed, 5);
    assert_eq!(report.llm_summaries, 0, "no llm mounted, no llm calls");
    // Binary png gets an envelope summary; text and the folder's listing
    // get extractive ones.
    assert_eq!(report.envelope_summaries, 1);
    assert_eq!(report.extractive_summaries, 4);
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
    assert_eq!(again.unchanged, 5);
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
    // What a follow-up scan needs rides on the result: the source's length
    // in lines, and each hint's line extent and score.
    let ContentLength::Lines(kitchen_lines) = top.envelope.length else {
        panic!("a text source's length is recorded in lines");
    };
    assert!(kitchen_lines >= 10, "kitchen.md is {kitchen_lines} lines");
    let hint = &top.hints[0];
    let Some(Extent::Lines { start, end }) = hint.extent else {
        panic!("hints carry line extents, got {:?}", hint.extent);
    };
    assert!(start >= 1);
    assert!(end >= start);
    assert!(end <= kitchen_lines, "hint extent lies within the source");
    assert!(hint.score > 0.0);
    assert!(hint.score <= 1.0);
    // The meta describes the query that produced these results: the served
    // limit, and counts that agree with the results.
    let meta = &response.meta;
    assert_eq!(meta.limit, 5);
    assert!(meta.trace.fts_hits > 0, "full-text search seeded the query");
    assert!(meta.trace.seeds > 0, "fusion kept the seeds");
    assert!(
        meta.trace.candidate_sources >= u32::try_from(response.results.len()).expect("fits"),
        "the limit cuts candidates, never the other way"
    );
    assert!(
        meta.elapsed_ms >= meta.trace.seeds_ms + meta.trace.graph_ms + meta.trace.rollup_ms,
        "the operation's wall-clock contains the finder's phases"
    );

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
    let Some(Extent::Lines { start, end }) = budget_fragment.extent else {
        panic!("budget section has a line extent");
    };
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
    assert_eq!(scan.start, start);
    assert_eq!(scan.end, end);
    assert_eq!(scan.lines_total, Some(kitchen_lines));
    assert_eq!(scan.mimetype, "text/markdown");

    // Widening past the end clamps, and the response says where it stopped.
    let widened = ops
        .scan(ScanRequest {
            address: top.address.clone(),
            start: 1,
            end: 10_000,
        })
        .await
        .expect("scans");
    assert_eq!(widened.start, 1);
    assert_eq!(widened.end, kitchen_lines);
    assert!(widened.text.starts_with("# Kitchen Renovation"));
    assert!(widened.text.contains("drywall in July."));

    // Bad ranges are typed client errors, never a read.
    let beyond = ops
        .scan(ScanRequest {
            address: top.address.clone(),
            start: kitchen_lines + 5,
            end: kitchen_lines + 9,
        })
        .await;
    assert!(
        matches!(beyond, Err(inseam_seams::SeamError::ScanBeyondEnd { lines_total, .. }) if lines_total == kitchen_lines),
        "{beyond:?}"
    );
    let backwards = ops
        .scan(ScanRequest {
            address: top.address.clone(),
            start: 3,
            end: 2,
        })
        .await;
    assert!(matches!(backwards, Err(inseam_seams::SeamError::ScanRange { start: 3, end: 2 })));
    let zero = ops
        .scan(ScanRequest {
            address: top.address.clone(),
            start: 0,
            end: 2,
        })
        .await;
    assert!(matches!(zero, Err(inseam_seams::SeamError::ScanRange { start: 0, end: 2 })));

    // --- rung 4: fetch ---
    let fetched = ops
        .fetch(FetchRequest {
            address: top.address.clone(),
        })
        .await
        .expect("fetches");
    assert!(fetched.text.contains("# Kitchen Renovation"));

    // Binary sources have no text rung: the error names the bytes rung
    // (`tests/fetch_bytes.rs` walks it).
    let png = response
        .results
        .iter()
        .map(|r| &r.address)
        .find(|a| a.to_string().ends_with("logo.png"));
    if let Some(png) = png {
        let refused = ops
            .fetch(FetchRequest {
                address: png.clone(),
            })
            .await;
        assert!(matches!(refused, Err(inseam_seams::SeamError::BinaryFetch(a, _)) if &a == png));
        // Nor a scan rung: an image has no text descendants to stand in.
        let unscannable = ops
            .scan(ScanRequest {
                address: png.clone(),
                start: 1,
                end: 5,
            })
            .await;
        assert!(matches!(unscannable, Err(inseam_seams::SeamError::NothingToScan(a)) if &a == png));
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
    assert_eq!(rebuilt.indexed, 2, "the note and its folder");
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
    // The note's new summary changes its folder's listing, so both re-index.
    assert_eq!(report.indexed, 2, "mtime/size change re-indexes: {report}");

    assert_eq!(common::hits(ops.as_ref(), "ferns").await, 0, "stale fragments gone");
    assert_eq!(common::hits(ops.as_ref(), "orchids").await, 1);
}

