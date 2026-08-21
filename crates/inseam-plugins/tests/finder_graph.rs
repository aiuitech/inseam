//! The Finder's reason to exist: relevance that is relational, not lexical.
//! A meeting note that never mentions the topic must still surface when it
//! is a `mentions` hop away from everything that does (`design/finder.md`,
//! "paths not taken: flat top-k similarity").

use std::sync::Arc;

use inseam_kernel::address::{Address, ContentLength, Envelope, Timestamp};
use inseam_kernel::fragment::{
    Extent, FragmentId, FragmentKey, Mimetype, NewFragment, Relation, RelationKind,
};

fn mentions() -> RelationKind {
    RelationKind::new("mentions").expect("valid kind")
}
use inseam_kernel::store::{IndexStore, SearchRow, SourceId};
use inseam_plugins::embedder::hashed;
use inseam_plugins::finder::{FinderConfig, FinderService};
use inseam_seams::embedder::Embedder;
use inseam_seams::finder::Finder;

const DIMS: usize = 64;

fn envelope(hint: &str) -> Envelope {
    Envelope {
        source_type: "file".into(),
        content_type: Mimetype::markdown(),
        length: ContentLength::Lines(10),
        created: None,
        modified: Some(Timestamp(1_700_000_000)),
        observed: Timestamp(1_700_000_100),
        properties: Vec::new(),
        hint: Some(hint.into()),
    }
}

fn addr(name: &str) -> Address {
    format!("inseam://fs-test/tmp/{name}")
        .parse()
        .expect("test address parses")
}

async fn open_store(dir: &std::path::Path) -> Arc<IndexStore> {
    let store = IndexStore::open(dir).await.expect("opens");
    store.declare_embedding("hashed", DIMS).await.expect("declares");
    Arc::new(store)
}

/// Insert a one-fragment source and return (source, section).
async fn seed_source(
    store: &IndexStore,
    embedder: &dyn Embedder,
    name: &str,
    body: &str,
) -> (SourceId, FragmentId) {
    let a = addr(name);
    let sid = store
        .upsert_source(&a, &envelope(name), 100).await
        .expect("upserts");
    let root = store
        .insert_fragment(
            sid,
            &NewFragment {
                mimetype: Mimetype::markdown(),
                text: None,
                extent: Some(Extent::lines(1, 10)),
            },
        ).await
        .expect("root");
    store.set_root_fragment(sid, root).await.expect("sets root");
    let section = store
        .insert_fragment(
            sid,
            &NewFragment {
                mimetype: Mimetype::markdown(),
                text: Some(body.to_string()),
                extent: Some(Extent::lines(1, 10)),
            },
        ).await
        .expect("section");
    store
        .insert_relation(&Relation::new(root, RelationKind::contains(), section)).await
        .expect("relates");
    let vector = embedder.embed(&[body]).await.expect("embeds").remove(0);
    store
        .add_search_rows(&[SearchRow {
            fragment: section,
            source: Some(sid),
            text: body.to_string(),
            vector: Some(vector),
        }])
        .await
        .expect("adds row");
    store.mark_indexed(sid, Some(("test-stamp", &[]))).await.expect("marks");
    (sid, section)
}

#[tokio::test]
async fn relational_relevance_beats_flat_similarity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(dir.path()).await;
    let embedder = hashed(DIMS);

    // B talks about the kitchen renovation directly.
    let (b_sid, b_section) = seed_source(
        &store,
        embedder.as_ref(),
        "kitchen-plan.md",
        "Kitchen renovation plan: cabinet quotes, appliance budget, demolition schedule.",
    )
    .await;
    // A is the meeting note that never says "kitchen" or "renovation".
    let (a_sid, a_section) = seed_source(
        &store,
        embedder.as_ref(),
        "sync-dana.md",
        "Sync with Dana: timelines, permits, contractor invoices for the project.",
    )
    .await;
    // C is unrelated noise.
    let (c_sid, _) = seed_source(
        &store,
        embedder.as_ref(),
        "groceries.md",
        "Grocery list: milk, eggs, coffee beans, flour.",
    )
    .await;

    // The shared entity — a keyed fragment — is the highway between A and B.
    let entity = store
        .keyed_fragment(
            &FragmentKey::new("entity:project:kitchen reno").expect("valid key"),
            &NewFragment {
                mimetype: Mimetype::parse("text/x-inseam-entity;kind=project").expect("valid"),
                text: Some("Kitchen Reno".to_string()),
                extent: None,
            },
        ).await
        .expect("entity")
        .id();
    let entity_vec = embedder
        .embed(&["Kitchen Reno"])
        .await
        .expect("embeds")
        .remove(0);
    store
        .add_search_rows(&[SearchRow {
            fragment: entity,
            source: None,
            text: "Kitchen Reno".to_string(),
            vector: Some(entity_vec),
        }])
        .await
        .expect("adds entity row");
    store
        .insert_relation(&Relation::new(b_section, mentions(), entity)).await
        .expect("relates");
    store
        .insert_relation(&Relation::new(a_section, mentions(), entity)).await
        .expect("relates");
    store.rebuild_fts().await.expect("fts");

    let finder = FinderService::new(Arc::clone(&store), embedder, FinderConfig::default());
    let results = finder.query("kitchen renovation", 10).await.expect("queries");
    let order: Vec<SourceId> = results.iter().map(|r| r.source.id).collect();

    // B matched directly and must lead.
    assert_eq!(order.first(), Some(&b_sid), "direct match leads: {order:?}");
    // A shares zero vocabulary with the query; only the entity hop can
    // surface it. This is the case flat similarity cannot get right.
    let a_pos = order.iter().position(|s| *s == a_sid);
    assert!(a_pos.is_some(), "meeting note surfaced via mentions edges");
    // ...and it must outrank unrelated noise if that noise appears at all.
    if let Some(c_pos) = order.iter().position(|s| *s == c_sid) {
        assert!(a_pos.expect("present") < c_pos, "relational beats unrelated");
    }
}

#[tokio::test]
async fn boost_never_gates_relationless_matches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(dir.path()).await;
    let embedder = hashed(DIMS);

    // A lone source with no relations beyond its own structure still ranks.
    let (sid, _) = seed_source(
        &store,
        embedder.as_ref(),
        "solo.md",
        "Espresso machine descaling procedure and water hardness notes.",
    )
    .await;
    store.rebuild_fts().await.expect("fts");

    let finder = FinderService::new(Arc::clone(&store), embedder, FinderConfig::default());
    let results = finder.query("espresso descaling", 5).await.expect("queries");
    assert_eq!(results.first().map(|r| r.source.id), Some(sid));
    assert!(results[0].score > 0.0);
}
