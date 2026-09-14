//! The vocabulary (`design/vocabulary.md`) end to end on the offline base:
//! the sweep's pass mines the corpus's own words from its statistics,
//! anchors them across every source, clusters them by co-occurrence, and
//! the finder grounds a question through them, bounds hubs, filters by
//! envelope, and explains every result with an exact ledger.

mod common;

use inseam_kernel::fragment::RelationKind;
use inseam_seams::SeamError;
use inseam_seams::finder::SeedChannel;
use inseam_seams::operations::{IndexRequest, QueryRequest, VocabularyRequest};

/// Two folders of notes. `SUP-1042` and `eu-central-1` recur in a few
/// notes; `Redwood` in most; `sprocket` in every one (a hub the band
/// keeps out); `zebra-mode` in exactly one (below the band).
fn write_corpus(dir: &std::path::Path) {
    let notes: [(&str, &str); 6] = [
        (
            "slack/incident.md",
            "# Incident\n\nTicket SUP-1042 tracks the eu-central-1 outage on Redwood. sprocket\n",
        ),
        (
            "slack/followup.md",
            "# Follow-up\n\nSUP-1042 closed after the Redwood rollback in eu-central-1. sprocket\n",
        ),
        (
            "slack/lunch.md",
            "# Lunch\n\nThe team went for tacos and talked about Redwood pricing. sprocket\n",
        ),
        (
            "jira/plan.md",
            "# Plan\n\nRedwood capacity for eu-central-1 doubles next quarter. sprocket\n",
        ),
        (
            "jira/retro.md",
            "# Retro\n\nWhat went well: Redwood shipped. zebra-mode was never enabled. sprocket\n",
        ),
        (
            "jira/misc.md",
            "# Misc\n\nUnrelated note about gardening and compost. sprocket\n",
        ),
    ];
    for (path, body) in notes {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, body).expect("write");
    }
}

const OVERLAY: &str = r#"
[[entry]]
id = "sweep"
[entry.config.vocabulary]
term_df_min = 2
term_df_max_floor = 4
cluster_llm_budget = 0
"#;

async fn indexed_corpus() -> (
    inseam_kernel::substrate::Kernel,
    tempfile::TempDir,
    tempfile::TempDir,
    inseam_seams::sweep::IndexReport,
) {
    let corpus = tempfile::tempdir().expect("tempdir");
    write_corpus(corpus.path());
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(data.path(), OVERLAY).await;
    let ops = common::ops(&kernel);
    let report = ops
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("sweeps");
    (kernel, corpus, data, report)
}

#[tokio::test]
async fn the_pass_mines_the_band_and_anchors_every_source() {
    let (kernel, _corpus, _data, report) = indexed_corpus().await;
    let vocabulary = report.vocabulary.as_ref().expect("the pass ran");
    assert!(vocabulary.skipped.is_none(), "{vocabulary}");
    assert_eq!(vocabulary.sources_walked, 6, "{vocabulary}");
    assert!(vocabulary.rows_planted > 0, "{vocabulary}");
    assert!(vocabulary.matches_counted > 0, "{vocabulary}");
    assert_eq!(
        vocabulary.anchors_added, 0,
        "a mined row stores no edges: {vocabulary}"
    );

    let ops = common::ops(&kernel);
    let listing = ops
        .vocabulary(VocabularyRequest {
            limit: 100,
            ..VocabularyRequest::default()
        })
        .await
        .expect("lists");
    let by_spelling: std::collections::HashMap<String, u32> = listing
        .rows
        .iter()
        .map(|row| (row.spelling.to_lowercase(), row.document_frequency))
        .collect();
    assert_eq!(by_spelling.get("sup-1042"), Some(&2), "{by_spelling:?}");
    assert_eq!(by_spelling.get("eu-central-1"), Some(&3), "{by_spelling:?}");
    assert!(
        !by_spelling.contains_key("sprocket"),
        "a word in every source is over the band: {by_spelling:?}"
    );
    assert!(
        !by_spelling.contains_key("zebra-mode"),
        "a word in one source is under the band: {by_spelling:?}"
    );
    let identifier = listing
        .rows
        .iter()
        .find(|row| row.spelling.eq_ignore_ascii_case("sup-1042"))
        .expect("the ticket is a row");
    assert_eq!(
        identifier.kind,
        inseam_kernel::store::VocabularyKind::Identifier
    );
    assert_eq!(identifier.key, "identifier:sup-1042");

    let shown = ops
        .vocabulary(VocabularyRequest {
            show: Some("SUP-1042".into()),
            ..VocabularyRequest::default()
        })
        .await
        .expect("shows")
        .shown
        .expect("the row exists");
    assert_eq!(shown.sources.len(), 2, "{shown:?}");
    assert!(shown.row.cluster.is_some(), "every row gets a cluster");
}

#[tokio::test]
async fn a_second_sweep_that_changes_nothing_skips_the_pass() {
    let (kernel, corpus, _data, _report) = indexed_corpus().await;
    let ops = common::ops(&kernel);
    let again = ops
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("sweeps");
    assert_eq!(again.indexed, 0);
    assert_eq!(
        again.vocabulary.as_ref().and_then(|v| v.skipped.as_deref()),
        Some("unchanged")
    );
}

#[tokio::test]
async fn folder_entries_relate_to_their_children() {
    let (kernel, corpus, _data, _report) = indexed_corpus().await;
    let store = kernel.store();
    let child = common::address_of(&kernel, &corpus.path().join("slack/incident.md"));
    let source = store
        .source_by_address(&child)
        .await
        .expect("reads")
        .expect("indexed");
    let root = source.root_fragment.expect("has a root");
    let incoming: Vec<_> = store
        .relations_touching(&[root])
        .await
        .expect("reads")
        .into_iter()
        .filter(|r| r.to == root && r.kind == RelationKind::contains())
        .collect();
    assert_eq!(
        incoming.len(),
        1,
        "one folder entry points at the child's root"
    );
    let entry = store
        .fragment(incoming[0].from)
        .await
        .expect("reads")
        .expect("exists");
    assert!(entry.mimetype.is_directory_entry());
    assert_eq!(entry.content_address.as_ref(), Some(&child));
}

#[tokio::test]
async fn exact_grounding_seeds_the_question_and_the_ledger_sums_to_the_score() {
    let (kernel, _corpus, _data, _report) = indexed_corpus().await;
    let ops = common::ops(&kernel);
    let response = ops
        .query(QueryRequest {
            text: "what happened with SUP-1042".into(),
            limit: 5,
            finder: Vec::new(),
            explain: true,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    let trace = &response.meta.trace;
    assert!(trace.exact_hits >= 1, "{trace:?}");
    assert!(response.results.len() >= 2, "{response:?}");
    let top_two: Vec<String> = response
        .results
        .iter()
        .take(2)
        .map(|r| r.address.to_string())
        .collect();
    assert!(
        top_two
            .iter()
            .all(|a| a.ends_with("incident.md") || a.ends_with("followup.md")),
        "the two notes naming the ticket lead: {top_two:?}"
    );
    for evidence in &trace.evidence {
        let ledger = evidence.ledger.as_ref().expect("explain attaches a ledger");
        let total: f64 = ledger.channels.iter().map(|c| c.seed + c.walk).sum();
        assert!(
            (total - evidence.score_raw).abs() < 1e-9,
            "{} ≠ {} for {}",
            total,
            evidence.score_raw,
            evidence.address
        );
    }
    // The grounded row is a keyed fragment of no source: its seed mass
    // reaches the notes through the walk, so the ledger shows it as walk
    // mass induced by the exact channel, never as a seed rank on a note's
    // own fragments.
    let first = &trace.evidence[0];
    assert!(
        first
            .ledger
            .as_ref()
            .expect("ledger")
            .channels
            .iter()
            .any(|c| c.channel == SeedChannel::Exact && c.walk > 0.0),
        "{first:?}"
    );
    assert!(
        first
            .ledger
            .as_ref()
            .expect("ledger")
            .rows
            .iter()
            .any(|row| row.text.eq_ignore_ascii_case("sup-1042")),
        "the ticket row carried the mass: {first:?}"
    );
}

/// A note's siblings sit four hops away — section, root, the folder's
/// entry, the folder root, the sibling's entry, its root — so the default
/// two-hop slice never reaches them, and a four-hop request reaches them
/// at a fraction of the matching note's score.
#[tokio::test]
async fn folder_membership_conducts_but_keeps_siblings_far_behind() {
    let (kernel, _corpus, _data, _report) = indexed_corpus().await;
    let ops = common::ops(&kernel);
    let two_hops = ops
        .query(QueryRequest {
            text: "gardening compost".into(),
            limit: 10,
            finder: Vec::new(),
            explain: false,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    assert!(
        two_hops
            .results
            .iter()
            .all(|r| !r.address.to_string().ends_with("plan.md")),
        "two hops stop at the folder: {two_hops:?}"
    );
    let response = ops
        .query(QueryRequest {
            text: "gardening compost".into(),
            limit: 10,
            finder: vec!["graph_hops=4".into()],
            explain: false,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    let scores: Vec<(String, f64)> = response
        .results
        .iter()
        .map(|r| (r.address.to_string(), r.score))
        .collect();
    assert!(scores[0].0.ends_with("jira/misc.md"), "{scores:?}");
    let siblings: Vec<&(String, f64)> = scores
        .iter()
        .filter(|(address, _)| address.ends_with("plan.md") || address.ends_with("retro.md"))
        .collect();
    assert!(
        !siblings.is_empty(),
        "folder membership reaches the note's siblings: {scores:?}"
    );
    assert!(
        siblings.iter().all(|(_, score)| *score < 0.25),
        "siblings trail far behind the note that matched: {scores:?}"
    );
}

#[tokio::test]
async fn overrides_bend_one_request_and_reject_unknown_keys() {
    let (kernel, _corpus, _data, _report) = indexed_corpus().await;
    let ops = common::ops(&kernel);
    let without_exact = ops
        .query(QueryRequest {
            text: "SUP-1042".into(),
            limit: 5,
            finder: vec!["seed_lists.exact.enabled=false".into()],
            explain: false,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    assert_eq!(without_exact.meta.trace.exact_hits, 0);
    let bounded = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 5,
            finder: vec!["hub_degree_max=1".into()],
            explain: true,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    assert!(
        !bounded.meta.trace.hubs_excluded.is_empty(),
        "a bound of one keeps every multi-anchor row out"
    );
    let unbounded = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 5,
            finder: vec!["hub_degree_max=0".into()],
            explain: false,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    assert!(unbounded.meta.trace.hubs_excluded.is_empty());
    let refused = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 5,
            finder: vec!["no_such_dial=1".into()],
            explain: false,
            filters: Default::default(),
        })
        .await;
    assert!(matches!(refused, Err(SeamError::Invalid(_))), "{refused:?}");
}

#[tokio::test]
async fn envelope_filters_narrow_the_candidates() {
    let (kernel, _corpus, _data, _report) = indexed_corpus().await;
    let ops = common::ops(&kernel);
    let folders_only = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 10,
            finder: Vec::new(),
            explain: false,
            filters: inseam_seams::finder::QueryFilters {
                source_type: Some("directory".into()),
                ..Default::default()
            },
        })
        .await
        .expect("queries");
    assert!(!folders_only.results.is_empty());
    assert!(
        folders_only
            .results
            .iter()
            .all(|r| r.envelope.source_type == "directory"),
        "{folders_only:?}"
    );
    assert!(folders_only.meta.trace.filtered_sources > 0);
    let nobody = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 10,
            finder: Vec::new(),
            explain: false,
            filters: inseam_seams::finder::QueryFilters {
                host: Some("no-such-host".into()),
                ..Default::default()
            },
        })
        .await
        .expect("queries");
    assert!(nobody.results.is_empty());
}

#[tokio::test]
async fn clusters_form_and_carry_vectors_for_grounding() {
    let (kernel, _corpus, _data, report) = indexed_corpus().await;
    let vocabulary = report.vocabulary.as_ref().expect("the pass ran");
    assert!(vocabulary.clusters_founded > 0, "{vocabulary}");
    assert!(vocabulary.clusters_embedded > 0, "{vocabulary}");
    let ops = common::ops(&kernel);
    let clusters = ops
        .vocabulary(VocabularyRequest {
            clusters: true,
            limit: 100,
            ..VocabularyRequest::default()
        })
        .await
        .expect("lists");
    assert!(!clusters.clusters.is_empty());
    assert!(
        clusters.clusters.iter().all(|c| c.has_vector),
        "{clusters:?}"
    );
    let grounded = ops
        .query(QueryRequest {
            text: "Redwood outage".into(),
            limit: 5,
            finder: vec!["cluster_query_cosine=-1".into()],
            explain: false,
            filters: Default::default(),
        })
        .await
        .expect("queries");
    assert!(
        grounded.meta.trace.clusters_matched > 0,
        "{:?}",
        grounded.meta
    );
    assert!(grounded.meta.trace.cluster_hits > 0, "{:?}", grounded.meta);
}

#[tokio::test]
async fn facets_become_rows_anchored_from_the_root_and_filter_queries() {
    use inseam_kernel::address::{Address, ContentLength, Envelope, Facet, Timestamp};
    use inseam_kernel::fragment::{Mimetype, NewFragment};

    let (kernel, corpus, _data, _report) = indexed_corpus().await;
    let store = kernel.store();
    // A message from a flat host, as a mail connection would enumerate it:
    // no folder, so its container and author arrive as facets.
    let address: Address = "inseam://mail-test/msg-1".parse().expect("valid");
    let envelope = Envelope {
        source_type: "email".into(),
        content_type: Mimetype::text_plain(),
        length: ContentLength::Bytes(10),
        created: None,
        modified: Some(Timestamp(1_700_000_000)),
        observed: Timestamp(1_700_000_100),
        properties: Vec::new(),
        facets: vec![
            Facet::new("author", "Dana Reyes"),
            Facet::new("label", "INBOX"),
        ],
        hint: Some("Hello".into()),
        content_digest: None,
    };
    let source = store
        .upsert_source(&address, &envelope, 10)
        .await
        .expect("upserts");
    let root = store
        .insert_fragment(
            source,
            &NewFragment {
                mimetype: Mimetype::text_plain(),
                text: None,
                extent: None,
                content_address: None,
            },
        )
        .await
        .expect("inserts");
    store.set_root_fragment(source, root).await.expect("sets");
    // A changed note makes the next sweep run the pass again.
    std::fs::write(
        corpus.path().join("jira/misc.md"),
        "# Misc\n\nUnrelated note about gardening, compost, and worms. sprocket\n",
    )
    .expect("writes");
    let ops = common::ops(&kernel);
    let report = ops
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("sweeps");
    let vocabulary = report.vocabulary.as_ref().expect("the pass ran");
    // The author and the label.
    assert_eq!(vocabulary.facets_planted, 2, "{vocabulary}");
    assert_eq!(vocabulary.facet_anchors, 2, "{vocabulary}");

    let author = ops
        .vocabulary(VocabularyRequest {
            show: Some("Dana Reyes".into()),
            ..VocabularyRequest::default()
        })
        .await
        .expect("shows")
        .shown
        .expect("the author is an entity row");
    assert_eq!(author.row.key, "entity:person:dana reyes");
    assert_eq!(author.sources, vec![address.clone()]);

    let filtered = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 10,
            finder: Vec::new(),
            explain: false,
            filters: inseam_seams::finder::QueryFilters {
                facets: vec!["label:INBOX".into()],
                ..Default::default()
            },
        })
        .await
        .expect("queries");
    assert!(filtered.results.is_empty(), "no note carries the label");
    // The host is a column on every source and a filter, never a row.
    let unfiltered = ops
        .query(QueryRequest::new("Redwood", 10))
        .await
        .expect("queries");
    let host = unfiltered.results[0].address.host.as_str().to_string();
    let by_host = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 10,
            finder: Vec::new(),
            explain: false,
            filters: inseam_seams::finder::QueryFilters {
                host: Some(host),
                ..Default::default()
            },
        })
        .await
        .expect("queries");
    assert_eq!(by_host.results.len(), unfiltered.results.len());
    let elsewhere = ops
        .query(QueryRequest {
            text: "Redwood".into(),
            limit: 10,
            finder: Vec::new(),
            explain: false,
            filters: inseam_seams::finder::QueryFilters {
                host: Some("mail-test".into()),
                ..Default::default()
            },
        })
        .await
        .expect("queries");
    assert!(elsewhere.results.is_empty(), "{elsewhere:?}");
}
