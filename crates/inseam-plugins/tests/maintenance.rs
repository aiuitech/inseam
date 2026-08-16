//! The reconciling sweep (`design/index-maintenance.md`), end-to-end through
//! the composed kernel: vanished sources are removed, orphaned entities are
//! collected, composition shape changes re-index while query-time changes
//! don't, **plugin churn dirties only what a plugin's claims touch**,
//! catalog-only rows converge to deep-indexed, embedding changes re-embed in
//! place, and scope shrinkage never evicts.

mod common;

use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind};
use inseam_seams::operations::{IndexRequest, QueryRequest};

async fn index(
    ops: &dyn inseam_seams::operations::Operations,
    root: &std::path::Path,
) -> inseam_seams::sweep::IndexReport {
    ops.index(IndexRequest {
        root: root.display().to_string(),
        rebuild: false,
    })
    .await
    .expect("sweeps")
}

#[tokio::test]
async fn vanished_sources_are_removed_and_their_entities_collected() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let doomed = corpus.path().join("doomed.md");
    std::fs::write(&doomed, "# Doomed\n\nnotes about xylophones\n").expect("writes");
    std::fs::write(corpus.path().join("keeper.md"), "# Keeper\n\nnotes about kazoos\n")
        .expect("writes");

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    index(ops.as_ref(), corpus.path()).await;
    assert_eq!(common::hits(ops.as_ref(), "xylophones").await, 1);

    // Wire an entity whose only mention lives in the doomed source, as the
    // entity transform would have (it needs an LLM, so we plant it directly
    // through the kernel store).
    let store = kernel.store();
    let host = kernel
        .facts("connection")
        .and_then(|f| f.str("host"))
        .expect("connection declares its host")
        .to_string();
    let host_id = inseam_kernel::address::HostId::new(host).expect("valid");
    let sources = store.sources_of_host(&host_id).expect("ok");
    let (doomed_sid, _) = sources
        .iter()
        .find(|(_, locator)| locator.ends_with("doomed.md"))
        .cloned()
        .expect("doomed source cataloged");
    let root = store
        .source(doomed_sid)
        .expect("ok")
        .expect("present")
        .root_fragment
        .expect("indexed sources have roots");
    let entity = store
        .insert_fragment(
            None,
            &NewFragment {
                mimetype: Mimetype::entity().with_param("kind", "instrument"),
                text: Some("Xylophone".into()),
                extent: None,
            },
        )
        .expect("inserts");
    store
        .register_entity("instrument:xylophone", entity)
        .expect("registers");
    store
        .insert_relation(&RelationKind::Mentions.edge(root, entity))
        .expect("relates");

    std::fs::remove_file(&doomed).expect("removes");
    let report = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(report.removed, 1, "vanished source reconciled: {report}");
    assert_eq!(report.entities_removed, 1, "orphaned entity collected");
    assert_eq!(report.unchanged, 1, "the keeper was untouched");

    assert_eq!(common::hits(ops.as_ref(), "xylophones").await, 0, "no stale results");
    assert_eq!(common::hits(ops.as_ref(), "kazoos").await, 1);

    // A file that reappears is simply new — no tombstone in the way.
    std::fs::write(&doomed, "# Doomed\n\nnotes about xylophones\n").expect("writes");
    let report = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(report.indexed, 1);
    assert_eq!(common::hits(ops.as_ref(), "xylophones").await, 1);
}

#[tokio::test]
async fn shape_config_changes_reindex_and_query_time_changes_do_not() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("a.md"), "# A\n\nalpha\n").expect("writes");
    std::fs::write(corpus.path().join("b.md"), "# B\n\nbeta\n").expect("writes");

    let mut kernel = common::boot(data.path(), "").await;
    index(common::ops(&kernel).as_ref(), corpus.path()).await;

    // Shape tier: summary length changes what subtrees look like. The
    // reconciler restarts the summarizer fiber; the next sweep discovers the
    // stamp divergence — no lifecycle hook told it anything.
    common::reconcile(
        &mut kernel,
        "[[entry]]\nid = \"summarizer\"\n[entry.config]\ntarget_chars = 120",
    )
    .await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.indexed, 2, "stamp mismatch re-indexes: {report}");
    assert_eq!(report.unchanged, 0);

    // Query-time tier: finder tuning must not dirty anything.
    common::reconcile(
        &mut kernel,
        r#"
        [[entry]]
        id = "summarizer"
        [entry.config]
        target_chars = 120

        [[entry]]
        id = "finder"
        [entry.config]
        damping = 0.8
        seed_k = 20
        "#,
    )
    .await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.unchanged, 2, "query-time tuning is free: {report}");
    assert_eq!(report.indexed, 0);

    // Run-metering tier: budgets bound runs, they don't define output.
    common::reconcile(
        &mut kernel,
        r#"
        [[entry]]
        id = "summarizer"
        [entry.config]
        target_chars = 120
        llm_call_budget = 3
        "#,
    )
    .await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.unchanged, 2, "metering changes are free: {report}");
}

#[tokio::test]
async fn plugin_churn_dirties_only_sources_the_plugins_claims_touch() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("note.md"), "# Note\n\nmarkdown words\n").expect("writes");
    std::fs::write(corpus.path().join("plain.txt"), "plain words here\n").expect("writes");

    let mut kernel = common::boot(data.path(), "").await;
    index(common::ops(&kernel).as_ref(), corpus.path()).await;

    // Unmount the chunker: it participated only in plain.txt's subtree
    // (markdown is claimed by the markdown transform), so exactly one
    // source re-indexes. This is the claims-aware stamp at work —
    // dirtiness discovered by the sweep, never triggered by the lifecycle.
    common::reconcile(&mut kernel, "[[entry]]\nid = \"chunker\"\ndisabled = true").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(
        (report.indexed, report.unchanged),
        (1, 1),
        "only the txt source re-indexes: {report}"
    );

    // Mount it back: again only the source whose inventory intersects the
    // chunker's claims dirties.
    common::reconcile(&mut kernel, "").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(
        (report.indexed, report.unchanged),
        (1, 1),
        "remount dirties only claimed sources: {report}"
    );

    // Steady state after the churn.
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.unchanged, 2, "converged: {report}");
}

#[tokio::test]
async fn catalog_only_sources_converge_to_deep_indexed_across_runs() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("a.md"), "# A\n\nalpha\n").expect("writes");
    std::fs::write(corpus.path().join("b.md"), "# B\n\nbeta\n").expect("writes");

    let kernel = common::boot(
        data.path(),
        "[[entry]]\nid = \"sweep\"\n[entry.config]\nmax_sources = 1",
    )
    .await;
    let ops = common::ops(&kernel);

    let first = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(first.indexed, 1);
    assert_eq!(first.catalog_only, 1);

    // The catalog-only row carries no stamp, so the next run finishes it.
    let second = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(second.indexed, 1, "the deferred source converges: {second}");
    assert_eq!(second.unchanged, 1);

    let third = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(third.unchanged, 2, "steady state: {third}");
}

#[tokio::test]
async fn embedding_change_reembeds_in_place_without_reindexing() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        corpus.path().join("note.md"),
        "# Note\n\nkitchen renovation budget\n",
    )
    .expect("writes");

    let mut kernel = common::boot(data.path(), "").await;
    index(common::ops(&kernel).as_ref(), corpus.path()).await;

    // Swap the embedder's dimensions: the embedder fiber restarts, declares
    // the new identity, and the store pends an in-place re-embed.
    common::reconcile(
        &mut kernel,
        r#"
        [[entry]]
        id = "embedder"
        [entry.config]
        provider = "hashed"
        model = "hashed"
        dimensions = 32
        "#,
    )
    .await;
    let ops = common::ops(&kernel);
    assert!(kernel.store().reembed_pending());
    let err = ops
        .query(QueryRequest {
            text: "kitchen".into(),
            limit: 5,
        })
        .await
        .expect_err("search refuses until the migration runs");
    assert!(err.to_string().contains("re-embed"), "instructive error: {err}");

    let report = index(ops.as_ref(), corpus.path()).await;
    assert!(report.reembedded > 0, "vectors rebuilt: {report}");
    assert_eq!(report.indexed, 0, "the graph was untouched — no transforms re-ran");
    assert_eq!(report.unchanged, 1);
    assert!(!kernel.store().reembed_pending());
    assert_eq!(common::hits(ops.as_ref(), "kitchen renovation").await, 1);
}

#[tokio::test]
async fn cutoff_catalogs_without_indexing_and_never_evicts() {
    use std::time::{Duration, SystemTime};
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let old_file = corpus.path().join("ancient.md");
    std::fs::write(&old_file, "# Ancient\n\nnotes about trilobites\n").expect("writes");
    let ancient = SystemTime::UNIX_EPOCH + Duration::from_secs(1_262_304_000); // 2010-01-01
    std::fs::File::options()
        .write(true)
        .open(&old_file)
        .expect("opens")
        .set_times(std::fs::FileTimes::new().set_modified(ancient))
        .expect("backdates");

    let horizoned = r#"
        [[entry]]
        id = "sweep"
        [entry.config]
        modified_after = "2015-01-01"
    "#;
    let mut kernel = common::boot(data.path(), horizoned).await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.skipped_cutoff, 1);
    assert_eq!(report.indexed, 0);
    // Cataloged — the map is complete — but not indexed, so not findable.
    assert_eq!(
        common::hits(common::ops(&kernel).as_ref(), "trilobites").await,
        0
    );

    // Loosening the horizon picks it up: it was never marked indexed.
    common::reconcile(&mut kernel, "").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.indexed, 1, "horizon loosened: {report}");
    assert_eq!(
        common::hits(common::ops(&kernel).as_ref(), "trilobites").await,
        1
    );

    // Tightening it again skips the source but evicts nothing.
    common::reconcile(&mut kernel, horizoned).await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.skipped_cutoff, 1);
    assert_eq!(report.removed, 0);
    assert_eq!(
        common::hits(common::ops(&kernel).as_ref(), "trilobites").await,
        1,
        "scope shrinkage never evicts paid-for understanding"
    );
}
