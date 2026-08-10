//! The reconciling sweep (`design/index-maintenance.md`), end-to-end through
//! the operations layer: vanished sources are removed, orphaned entities are
//! collected, profile shape changes re-index while query-time changes don't,
//! catalog-only rows converge to deep-indexed, embedding changes re-embed in
//! place, and scope shrinkage never evicts.

use std::path::Path;
use std::time::{Duration, SystemTime};

use inseam::fragment::{Mimetype, NewFragment, RelationKind};
use inseam::ops::{Node, QueryRequest};
use inseam::profile::{EmbeddingProvider, IndexProfile};

fn offline_profile() -> IndexProfile {
    let mut profile = IndexProfile::default();
    profile.embedding.provider = EmbeddingProvider::Hashed;
    profile.embedding.model = "hashed".into();
    profile.embedding.dimensions = 64;
    profile.entities.enabled = false;
    profile
}

async fn open(data: &Path, profile: IndexProfile) -> Node {
    Node::open(data, profile, None).await.expect("opens")
}

async fn hits(node: &Node, text: &str) -> usize {
    node.query(QueryRequest {
        text: text.into(),
        limit: 10,
    })
    .await
    .expect("queries")
    .results
    .len()
}

#[tokio::test]
async fn vanished_sources_are_removed_and_their_entities_collected() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let doomed = corpus.path().join("doomed.md");
    std::fs::write(&doomed, "# Doomed\n\nnotes about xylophones\n").expect("writes");
    std::fs::write(corpus.path().join("keeper.md"), "# Keeper\n\nnotes about kazoos\n")
        .expect("writes");

    let node = open(data.path(), offline_profile()).await;
    node.index_dir(corpus.path(), false).await.expect("indexes");
    assert_eq!(hits(&node, "xylophones").await, 1);

    // Wire an entity whose only mention lives in the doomed source, as the
    // entity transform would have (it needs an LLM, so we plant it directly).
    let store = node.store();
    let doomed_source = store
        .source_by_address(&"inseam://fs-test/dummy".parse().expect("parses"))
        .expect("ok");
    assert!(doomed_source.is_none(), "sanity: bogus address is absent");
    let sources = store.sources_of_host(node.host_id()).expect("ok");
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
    let report = node.index_dir(corpus.path(), false).await.expect("sweeps");
    assert_eq!(report.removed, 1, "vanished source reconciled: {report}");
    assert_eq!(report.entities_removed, 1, "orphaned entity collected");
    assert_eq!(report.unchanged, 1, "the keeper was untouched");

    assert_eq!(hits(&node, "xylophones").await, 0, "no stale results");
    assert_eq!(hits(&node, "kazoos").await, 1);

    // A file that reappears is simply new — no tombstone in the way.
    std::fs::write(&doomed, "# Doomed\n\nnotes about xylophones\n").expect("writes");
    let report = node.index_dir(corpus.path(), false).await.expect("sweeps");
    assert_eq!(report.indexed, 1);
    assert_eq!(hits(&node, "xylophones").await, 1);
}

#[tokio::test]
async fn shape_profile_changes_reindex_and_query_time_changes_do_not() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("a.md"), "# A\n\nalpha\n").expect("writes");
    std::fs::write(corpus.path().join("b.md"), "# B\n\nbeta\n").expect("writes");

    let node = open(data.path(), offline_profile()).await;
    node.index_dir(corpus.path(), false).await.expect("indexes");

    // Shape tier: summary length changes what subtrees look like.
    let mut reshaped = offline_profile();
    reshaped.summary.target_chars = 120;
    let node = open(data.path(), reshaped).await;
    let report = node.index_dir(corpus.path(), false).await.expect("sweeps");
    assert_eq!(report.indexed, 2, "stamp mismatch re-indexes: {report}");
    assert_eq!(report.unchanged, 0);

    // Query-time tier: finder tuning must not dirty anything.
    let mut retuned = offline_profile();
    retuned.summary.target_chars = 120;
    retuned.finder.damping = 0.8;
    retuned.finder.seed_k = 20;
    let node = open(data.path(), retuned).await;
    let report = node.index_dir(corpus.path(), false).await.expect("sweeps");
    assert_eq!(report.unchanged, 2, "query-time tuning is free: {report}");
    assert_eq!(report.indexed, 0);

    // Run-metering tier: budgets bound runs, they don't define output.
    let mut remetered = offline_profile();
    remetered.summary.target_chars = 120;
    remetered.summary.llm_call_budget = 3;
    let node = open(data.path(), remetered).await;
    let report = node.index_dir(corpus.path(), false).await.expect("sweeps");
    assert_eq!(report.unchanged, 2, "metering changes are free: {report}");
}

#[tokio::test]
async fn catalog_only_sources_converge_to_deep_indexed_across_runs() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("a.md"), "# A\n\nalpha\n").expect("writes");
    std::fs::write(corpus.path().join("b.md"), "# B\n\nbeta\n").expect("writes");

    let mut metered = offline_profile();
    metered.budget.max_sources = 1;
    let node = open(data.path(), metered).await;

    let first = node.index_dir(corpus.path(), false).await.expect("indexes");
    assert_eq!(first.indexed, 1);
    assert_eq!(first.catalog_only, 1);

    // The catalog-only row carries no stamp, so the next run finishes it.
    let second = node.index_dir(corpus.path(), false).await.expect("indexes");
    assert_eq!(second.indexed, 1, "the deferred source converges: {second}");
    assert_eq!(second.unchanged, 1);

    let third = node.index_dir(corpus.path(), false).await.expect("indexes");
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

    let node = open(data.path(), offline_profile()).await;
    node.index_dir(corpus.path(), false).await.expect("indexes");
    drop(node);

    let mut redimensioned = offline_profile();
    redimensioned.embedding.dimensions = 32;
    let node = open(data.path(), redimensioned).await;
    assert!(node.store().reembed_pending());
    let err = node
        .query(QueryRequest {
            text: "kitchen".into(),
            limit: 5,
        })
        .await
        .expect_err("search refuses until the migration runs");
    assert!(
        err.to_string().contains("re-embed"),
        "instructive error: {err}"
    );

    let report = node.index_dir(corpus.path(), false).await.expect("sweeps");
    assert!(report.reembedded > 0, "vectors rebuilt: {report}");
    assert_eq!(report.indexed, 0, "the graph was untouched — no transforms re-ran");
    assert_eq!(report.unchanged, 1);
    assert!(!node.store().reembed_pending());
    assert_eq!(hits(&node, "kitchen renovation").await, 1);
}

#[tokio::test]
async fn cutoff_catalogs_without_indexing_and_never_evicts() {
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

    let mut horizoned = offline_profile();
    horizoned.cutoff.modified_after = Some("2015-01-01".into());
    let node = open(data.path(), horizoned).await;
    let report = node.index_dir(corpus.path(), false).await.expect("indexes");
    assert_eq!(report.skipped_cutoff, 1);
    assert_eq!(report.indexed, 0);
    // Cataloged — the map is complete — but not indexed, so not findable.
    assert_eq!(node.store().sources_of_host(node.host_id()).expect("ok").len(), 1);
    assert_eq!(hits(&node, "trilobites").await, 0);

    // Loosening the horizon picks it up: it was never marked indexed.
    let node = open(data.path(), offline_profile()).await;
    let report = node.index_dir(corpus.path(), false).await.expect("indexes");
    assert_eq!(report.indexed, 1, "horizon loosened: {report}");
    assert_eq!(hits(&node, "trilobites").await, 1);

    // Tightening it again skips the source but evicts nothing.
    let mut horizoned = offline_profile();
    horizoned.cutoff.modified_after = Some("2015-01-01".into());
    let node = open(data.path(), horizoned).await;
    let report = node.index_dir(corpus.path(), false).await.expect("indexes");
    assert_eq!(report.skipped_cutoff, 1);
    assert_eq!(report.removed, 0);
    assert_eq!(
        hits(&node, "trilobites").await,
        1,
        "scope shrinkage never evicts paid-for understanding"
    );
}
