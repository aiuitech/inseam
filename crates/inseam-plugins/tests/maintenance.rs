//! The reconciling sweep (`design/index-maintenance.md`), end-to-end through
//! the composed kernel: vanished sources are removed, orphaned entities are
//! collected, composition shape changes re-index while query-time changes
//! don't, **plugin churn dirties only what a plugin's claims touch**,
//! catalog-only rows converge to deep-indexed, embedding changes re-embed in
//! place, and scope shrinkage never evicts.

mod common;

use inseam_kernel::fragment::{FragmentKey, Mimetype, NewFragment, Relation, RelationKind};
use inseam_seams::connection::CONNECTIONS;
use inseam_seams::operations::{
    CatalogFilter, CatalogRequest, IndexRequest, QueryRequest, RepairOutcome, RepairRequest,
};
use inseam_seams::sweep::DeepBudget;

async fn index(
    ops: &dyn inseam_seams::operations::Operations,
    root: &std::path::Path,
) -> inseam_seams::sweep::IndexReport {
    ops.index(IndexRequest {
        host: None,
        root: root.display().to_string(),
        rebuild: false,
        deep_budget: None,
        llm_lane: None,
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
    let host_id = kernel
        .service(&CONNECTIONS)
        .expect("connections bound")
        .snapshot()
        .pop()
        .expect("the filesystem connection registered")
        .host
        .id
        .clone();
    let sources = store.sources_of_host(&host_id).await.expect("ok");
    let (doomed_sid, _) = sources
        .iter()
        .find(|(_, locator)| locator.ends_with("doomed.md"))
        .cloned()
        .expect("doomed source cataloged");
    let root = store
        .source(doomed_sid).await
        .expect("ok")
        .expect("present")
        .root_fragment
        .expect("indexed sources have roots");
    let entity = store
        .keyed_fragment(
            &FragmentKey::new("entity:instrument:xylophone").expect("valid key"),
            &NewFragment {
                mimetype: Mimetype::parse("text/x-inseam-entity;kind=instrument").expect("valid"),
                text: Some("Xylophone".into()),
                extent: None,
                content_address: None,
            },
        ).await
        .expect("creates")
        .id();
    let mentions = RelationKind::new("mentions").expect("valid kind");
    store
        .insert_relation(&Relation::new(root, mentions, entity)).await
        .expect("relates");

    std::fs::remove_file(&doomed).expect("removes");
    let report = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(report.removed, 1, "vanished source reconciled: {report}");
    assert_eq!(report.keyed_removed, 1, "unanchored keyed fragment collected");
    assert_eq!(report.unchanged, 1, "the keeper was untouched");
    assert_eq!(report.indexed, 1, "the folder's listing lost an entry: {report}");

    assert_eq!(common::hits(ops.as_ref(), "xylophones").await, 0, "no stale results");
    assert_eq!(common::hits(ops.as_ref(), "kazoos").await, 1);

    // A file that reappears is simply new — no tombstone in the way.
    std::fs::write(&doomed, "# Doomed\n\nnotes about xylophones\n").expect("writes");
    let report = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(report.indexed, 2, "the note and its folder: {report}");
    assert_eq!(common::hits(ops.as_ref(), "xylophones").await, 1);
}

#[tokio::test]
async fn ignore_rules_evict_covered_sources_and_readmit_them_when_lifted() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(corpus.path().join("Archive")).expect("mkdir");
    std::fs::write(corpus.path().join("Archive/old.md"), "# Old\n\nnotes about ocarinas\n")
        .expect("writes");
    std::fs::write(corpus.path().join("now.md"), "# Now\n\nnotes about banjos\n").expect("writes");

    let mut kernel = common::boot(data.path(), "").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.indexed, 4, "two notes, `Archive`, and the root: {report}");
    assert_eq!(report.ignored, 0);
    assert_eq!(common::hits(common::ops(&kernel).as_ref(), "ocarinas").await, 1);

    // Ignoring is membership: a rule that now covers an indexed source
    // removes it on the next sweep — the catalog must not keep (or sync)
    // what the owner said is not theirs to index.
    common::reconcile(
        &mut kernel,
        r#"
        [[entry]]
        id = "sweep"
        [[entry.config.ignore]]
        locator = "**/Archive/**"
        "#,
    )
    .await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    // The note leaves, and so does `Archive`: a folder none of whose files
    // are admitted is no source. The root re-indexes without the entry.
    assert_eq!(report.ignored, 2, "the archived note and its folder are kept out: {report}");
    assert_eq!(report.removed, 2, "and their index subtrees are gone");
    assert_eq!(report.unchanged, 1);
    assert_eq!(report.indexed, 1);
    assert_eq!(common::hits(common::ops(&kernel).as_ref(), "ocarinas").await, 0);
    assert_eq!(common::hits(common::ops(&kernel).as_ref(), "banjos").await, 1);

    // Lifting the rule readmits it as a new source — no tombstone, no
    // special case.
    common::reconcile(&mut kernel, "").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.ignored, 0);
    assert_eq!(report.indexed, 3, "the note, `Archive`, and the root again: {report}");
    assert_eq!(common::hits(common::ops(&kernel).as_ref(), "ocarinas").await, 1);
}

#[tokio::test]
async fn a_malformed_ignore_rule_parks_the_sweep_entry() {
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = common::boot(data.path(), "").await;
    let base = inseam_kernel::substrate::Composition::parse(common::OFFLINE_BASE, "test base")
        .expect("base parses");
    let overlay = inseam_kernel::substrate::Composition::parse(
        r#"
        [[entry]]
        id = "sweep"
        [[entry.config.ignore]]
        locator = "docs/[z-a]"
        "#,
        "test overlay",
    )
    .expect("overlay parses");
    // A bad glob is a contained configuration error: the sweep entry fails
    // (and its dependents wait), nothing else is touched, and the reconciler
    // reports the unsettled pair — never a sweep that silently ignores nothing.
    let outcome = kernel
        .reconcile(&base.layered(overlay).expect("layers"))
        .await;
    assert!(
        matches!(outcome, Err(inseam_kernel::substrate::SubstrateError::Unsettled { .. })),
        "{outcome:?}"
    );
    let fibers = kernel.fibers();
    let sweep = fibers.iter().find(|f| f.id == "sweep").expect("sweep entry exists");
    match &sweep.state {
        inseam_kernel::substrate::FiberState::Failed(reason) => {
            assert!(reason.contains("ignore rule 0"), "names the rule: {reason}");
            assert!(reason.contains("locator"), "names the field: {reason}");
        }
        other => panic!("sweep should be parked, was {other:?}"),
    }
    let fs = fibers.iter().find(|f| f.id == "fs").expect("fs entry exists");
    assert_eq!(fs.state, inseam_kernel::substrate::FiberState::Active);
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
    assert_eq!(report.indexed, 3, "stamp mismatch re-indexes: {report}");
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
    assert_eq!(report.unchanged, 3, "query-time tuning is free: {report}");
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
    assert_eq!(report.unchanged, 3, "metering changes are free: {report}");
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
    // The folder's listing carries the same summary, so it is untouched.
    assert_eq!(
        (report.indexed, report.unchanged),
        (1, 2),
        "only the txt source re-indexes: {report}"
    );

    // Mount it back: again only the source whose inventory intersects the
    // chunker's claims dirties.
    common::reconcile(&mut kernel, "").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(
        (report.indexed, report.unchanged),
        (1, 2),
        "remount dirties only claimed sources: {report}"
    );

    // Steady state after the churn.
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.unchanged, 3, "converged: {report}");
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

    // Folders spend the same budget, after the files: one note lands, the
    // other note and the folder wait.
    let first = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(first.indexed, 1, "{first}");
    assert_eq!(first.catalog_only, 2, "{first}");

    // The catalog-only rows carry no stamp, so the next runs finish them.
    let second = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(second.indexed, 1, "the deferred note converges: {second}");
    assert_eq!(second.unchanged, 1);
    assert_eq!(second.catalog_only, 1, "the folder waits one more run: {second}");

    let third = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(third.indexed, 1, "the folder converges: {third}");
    assert_eq!(third.unchanged, 2);

    let fourth = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(fourth.unchanged, 3, "steady state: {fourth}");
}

#[tokio::test]
async fn catalog_only_run_catalogs_everything_and_deep_indexes_nothing() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("a.md"), "# A\n\nalpha\n").expect("writes");
    std::fs::write(corpus.path().join("b.md"), "# B\n\nbeta\n").expect("writes");

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let ingest = ops
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: Some(DeepBudget::CatalogOnly),
            llm_lane: None,
        })
        .await
        .expect("sweeps");
    assert_eq!(ingest.catalog_only, 3, "two notes and their folder: {ingest}");
    assert_eq!(ingest.indexed, 0);
    assert_eq!(ingest.fragments, 0);

    // The ingest is visible in the catalog listing: every address known,
    // none searchable yet.
    let listing = ops
        .catalog(CatalogRequest {
            host: None,
            filter: CatalogFilter::Pending,
            limit: 10,
        })
        .await
        .expect("lists");
    assert_eq!(listing.sources, 3);
    assert_eq!(listing.indexed, 0);
    assert_eq!(listing.pending, 3);
    assert_eq!(listing.entries.len(), 3);
    assert!(listing.entries.iter().all(|e| !e.indexed));
    // Files carry their byte size; the folder has no bytes of its own.
    let is_folder = |content_type: &str| content_type == "inode/directory";
    let files = listing.entries.iter().filter(|e| !is_folder(&e.content_type)).count();
    assert_eq!(files, 2);
    assert!(listing
        .entries
        .iter()
        .all(|e| (e.raw_bytes > 0) == !is_folder(&e.content_type)));
    assert_eq!(common::hits(ops.as_ref(), "alpha").await, 0);

    // The request's budget never outlives its run: an unqualified run picks
    // the composition's (unlimited) budget and finishes the job.
    let second = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(second.indexed, 3, "{second}");
    let listing = ops
        .catalog(CatalogRequest {
            host: None,
            filter: CatalogFilter::Indexed,
            limit: 1,
        })
        .await
        .expect("lists");
    assert_eq!(listing.indexed, 3);
    assert_eq!(listing.pending, 0);
    assert_eq!(listing.entries.len(), 1, "limit bounds entries, not counts");
    assert_eq!(common::hits(ops.as_ref(), "alpha").await, 1);
}

#[tokio::test]
async fn request_budget_overrides_the_composition_for_one_run() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    for name in ["a.md", "b.md", "c.md"] {
        std::fs::write(corpus.path().join(name), format!("# {name}\n\ntext\n")).expect("writes");
    }
    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let report = ops
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: Some(DeepBudget::Sources(std::num::NonZeroU32::new(2).expect("non-zero"))),
            llm_lane: None,
        })
        .await
        .expect("sweeps");
    assert_eq!(report.indexed, 2, "{report}");
    assert_eq!(report.catalog_only, 2, "the third note and the folder: {report}");
}

#[tokio::test]
async fn status_reports_store_and_content_sizes() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let body = "# Note\n\nkitchen renovation budget\n";
    std::fs::write(corpus.path().join("note.md"), body).expect("writes");
    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);

    let before = ops.status().await.expect("status");
    assert_eq!(before.content_bytes, 0);
    index(ops.as_ref(), corpus.path()).await;
    let after = ops.status().await.expect("status");
    assert_eq!(after.content_bytes, u64::try_from(body.len()).expect("fits"));
    assert!(after.store_bytes > 0, "the database file exists on disk");
    let repaired = ops
        .repair(RepairRequest { rebuild: false })
        .await
        .expect("repairs");
    assert_eq!(repaired.outcome, RepairOutcome::AlreadyReady);
    assert!(repaired.vector_index_ready);
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
    assert_eq!(report.unchanged, 2, "the note and its folder: {report}");
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
    // The folder has no cutoff of its own, but nothing under it is indexed,
    // so it waits, cataloged, like its file.
    assert_eq!(report.catalog_only, 1, "{report}");
    // Cataloged — the map is complete — but not indexed, so not findable.
    assert_eq!(
        common::hits(common::ops(&kernel).as_ref(), "trilobites").await,
        0
    );

    // Loosening the horizon picks it up: it was never marked indexed.
    common::reconcile(&mut kernel, "").await;
    let report = index(common::ops(&kernel).as_ref(), corpus.path()).await;
    assert_eq!(report.indexed, 2, "horizon loosened, folder follows: {report}");
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
