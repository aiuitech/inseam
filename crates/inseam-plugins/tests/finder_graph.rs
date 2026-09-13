//! The Finder's reason to exist: relevance that is relational, not lexical.
//! A meeting note that never mentions the topic must still surface when it
//! is a `mentions` hop away from everything that does (`design/finder.md`,
//! "paths not taken: flat top-k similarity").

use std::sync::Arc;

use inseam_kernel::address::{Address, ContentDigest, ContentLength, Envelope, Timestamp};
use inseam_kernel::fragment::{
    Extent, FragmentId, FragmentKey, Mimetype, NewFragment, Relation, RelationKind,
};

fn mentions() -> RelationKind {
    RelationKind::new("mentions").expect("valid kind")
}
use inseam_kernel::store::{IndexStore, SearchRole, SearchRow, SourceId};
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
        facets: Vec::new(),
        hint: Some(hint.into()),
        content_digest: None,
    }
}

fn addr(name: &str) -> Address {
    format!("inseam://fs-test/tmp/{name}")
        .parse()
        .expect("test address parses")
}

async fn open_store(dir: &std::path::Path) -> Arc<IndexStore> {
    let store = IndexStore::open(dir).await.expect("opens");
    store
        .declare_embedding(inseam_kernel::store::EmbeddingIdentity {
            model: "hashed".to_string(),
            dimensions: DIMS,
            vectors: inseam_kernel::store::VectorScope::All,
        })
        .await
        .expect("declares");
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
        .upsert_source(&a, &envelope(name), 100)
        .await
        .expect("upserts");
    let root = store
        .insert_fragment(
            sid,
            &NewFragment {
                mimetype: Mimetype::markdown(),
                text: None,
                extent: Some(Extent::lines(1, 10)),
                content_address: None,
            },
        )
        .await
        .expect("root");
    store.set_root_fragment(sid, root).await.expect("sets root");
    let section = store
        .insert_fragment(
            sid,
            &NewFragment {
                mimetype: Mimetype::markdown(),
                text: Some(body.to_string()),
                extent: Some(Extent::lines(1, 10)),
                content_address: None,
            },
        )
        .await
        .expect("section");
    store
        .insert_relation(&Relation::new(root, RelationKind::contains(), section))
        .await
        .expect("relates");
    let vector = embedder.embed(&[body]).await.expect("embeds").remove(0);
    store
        .add_search_rows(&[SearchRow {
            fragment: section,
            source: Some(sid),
            text: body.to_string(),
            vector: Some(vector),
            role: SearchRole::Content,
        }])
        .await
        .expect("adds row");
    store
        .mark_indexed(sid, Some(("test-stamp", &[])))
        .await
        .expect("marks");
    (sid, section)
}

/// Re-upsert a seeded source with a digest-bearing envelope: what a sweep's
/// content read does once it has the bytes.
async fn set_digest(store: &IndexStore, name: &str, digest: ContentDigest) {
    let mut env = envelope(name);
    env.content_digest = Some(digest);
    store
        .upsert_source(&addr(name), &env, 100)
        .await
        .expect("upserts");
}

#[tokio::test]
async fn merge_collapses_equal_digests_and_leaves_digestless_copies_apart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(dir.path()).await;
    let embedder = hashed(DIMS);

    let body = "Espresso machine descaling procedure and water hardness notes.";
    // The same file living on two hosts: equal digests, one logical result.
    seed_source(&store, embedder.as_ref(), "local-copy.md", body).await;
    seed_source(&store, embedder.as_ref(), "drive-copy.md", body).await;
    let digest = ContentDigest::of_bytes(body.as_bytes());
    set_digest(&store, "local-copy.md", digest).await;
    set_digest(&store, "drive-copy.md", digest).await;
    // Identical content with no digest: degradation is duplication.
    seed_source(&store, embedder.as_ref(), "dup-a.md", body).await;
    seed_source(&store, embedder.as_ref(), "dup-b.md", body).await;
    store.rebuild_fts().await.expect("fts");

    let finder = FinderService::new(Arc::clone(&store), embedder, FinderConfig::default());
    let results = finder
        .query("espresso descaling", 10)
        .await
        .expect("queries")
        .ranked;

    let copies: Vec<_> = results
        .iter()
        .filter(|r| r.source.envelope.content_digest == Some(digest))
        .collect();
    assert_eq!(copies.len(), 1, "equal digests collapse into one result");
    let copy_addresses = [addr("local-copy.md"), addr("drive-copy.md")];
    assert!(copy_addresses.contains(&copies[0].source.address));
    assert_eq!(
        copies[0].replicas.len(),
        1,
        "the other copy rides as a replica"
    );
    assert!(copy_addresses.contains(&copies[0].replicas[0]));
    assert_ne!(copies[0].replicas[0], copies[0].source.address);

    let digestless: Vec<_> = results
        .iter()
        .filter(|r| r.source.envelope.content_digest.is_none())
        .collect();
    assert_eq!(digestless.len(), 2, "no digest, no collapse");
    assert!(digestless.iter().all(|r| r.replicas.is_empty()));
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
                content_address: None,
            },
        )
        .await
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
            role: SearchRole::Content,
        }])
        .await
        .expect("adds entity row");
    store
        .insert_relation(&Relation::new(b_section, mentions(), entity))
        .await
        .expect("relates");
    store
        .insert_relation(&Relation::new(a_section, mentions(), entity))
        .await
        .expect("relates");
    store.rebuild_fts().await.expect("fts");

    let finder = FinderService::new(Arc::clone(&store), embedder, FinderConfig::default());
    let results = finder
        .query("kitchen renovation", 10)
        .await
        .expect("queries")
        .ranked;
    let order: Vec<SourceId> = results.iter().map(|r| r.source.id).collect();

    // B matched directly and must lead.
    assert_eq!(order.first(), Some(&b_sid), "direct match leads: {order:?}");
    // A shares zero vocabulary with the query; only the entity hop can
    // surface it. This is the case flat similarity cannot get right.
    let a_pos = order.iter().position(|s| *s == a_sid);
    assert!(a_pos.is_some(), "meeting note surfaced via mentions edges");
    // ...and it must outrank unrelated noise if that noise appears at all.
    if let Some(c_pos) = order.iter().position(|s| *s == c_sid) {
        assert!(
            a_pos.expect("present") < c_pos,
            "relational beats unrelated"
        );
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
    let results = finder
        .query("espresso descaling", 5)
        .await
        .expect("queries")
        .ranked;
    assert_eq!(results.first().map(|r| r.source.id), Some(sid));
    assert!(results[0].score > 0.0);
}

#[tokio::test]
async fn lower_lexical_weight_preserves_name_discovery_and_favors_prose() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(dir.path()).await;
    let embedder = hashed(DIMS);
    let (prose, _) = seed_source(&store, embedder.as_ref(), "prose.md", "espresso").await;
    let (names, _) = seed_source(&store, embedder.as_ref(), "names.md", "unrelated").await;
    let fragment = store
        .insert_fragment(
            names,
            &NewFragment {
                mimetype: Mimetype::keywords(),
                text: Some("espresso descaling".to_string()),
                extent: None,
                content_address: None,
            },
        )
        .await
        .expect("name fragment");
    store
        .add_search_rows(&[SearchRow {
            fragment,
            source: Some(names),
            text: "espresso descaling".to_string(),
            vector: None,
            role: SearchRole::Lexical,
        }])
        .await
        .expect("lexical row");
    store.rebuild_fts().await.expect("fts");
    let mut ratios = Vec::new();
    for lexical_weight in [1.0, 0.1] {
        let finder = FinderService::new(
            Arc::clone(&store),
            Arc::clone(&embedder),
            FinderConfig {
                lexical_weight,
                seeds: inseam_plugins::finder::SeedLists::FullText,
                ..FinderConfig::default()
            },
        );
        let result = finder.query("espresso", 5).await.expect("queries");
        assert_eq!(result.trace.lexical_hits, 1);
        let prose_score = result
            .ranked
            .iter()
            .find(|r| r.source.id == prose)
            .expect("prose remains")
            .score;
        let name_score = result
            .ranked
            .iter()
            .find(|r| r.source.id == names)
            .expect("names remain")
            .score;
        ratios.push(name_score / prose_score);
        let name_only = finder.query("descaling", 5).await.expect("name-only query");
        assert_eq!(name_only.ranked[0].source.id, names);
    }
    assert!(ratios[1] < ratios[0]);
}

#[test]
fn lexical_weight_rejects_disabling_or_invalid_weights() {
    for lexical_weight in [0.0, -0.1, 1.1, f64::NAN, f64::INFINITY] {
        let config = FinderConfig {
            lexical_weight,
            ..FinderConfig::default()
        };
        assert!(config.validate_query_bounds().is_err());
    }
    assert!(FinderConfig::default().validate_query_bounds().is_ok());
}

#[tokio::test]
async fn evidence_reconstructs_each_returned_source_score() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(dir.path()).await;
    let embedder = hashed(DIMS);
    seed_source(
        &store,
        embedder.as_ref(),
        "coffee.md",
        "Coffee espresso brewing notes.",
    )
    .await;
    seed_source(
        &store,
        embedder.as_ref(),
        "tea.md",
        "Tea brewing temperature guide.",
    )
    .await;
    store.rebuild_fts().await.expect("fts");
    let finder = FinderService::new(Arc::clone(&store), embedder, FinderConfig::default());
    let discovery = finder.query("coffee brewing", 10).await.expect("query");
    assert!(!discovery.ranked.is_empty());
    assert_eq!(discovery.trace.evidence.len(), discovery.ranked.len());
    for result in &discovery.ranked {
        let evidence = discovery
            .trace
            .evidence
            .iter()
            .find(|item| item.address == result.source.address)
            .expect("evidence");
        assert!(evidence.fragments.len() <= 3);
        let raw: f64 = evidence
            .fragments
            .iter()
            .map(|fragment| (fragment.seed + fragment.graph) * fragment.weight)
            .sum();
        assert!((raw - evidence.score_raw).abs() < 1e-12);
        assert!((raw / evidence.normalization - result.score).abs() < 1e-12);
        assert!(
            evidence
                .fragments
                .iter()
                .any(|fragment| fragment.prose_rank.is_some())
        );
    }
}

#[tokio::test]
async fn empty_retrieval_has_no_score_evidence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(dir.path()).await;
    let finder = FinderService::new(store, hashed(DIMS), FinderConfig::default());
    let discovery = finder.query("missing", 10).await.expect("query");
    assert!(discovery.ranked.is_empty());
    assert!(discovery.trace.evidence.is_empty());
}
