//! Folders are sources (`design/indexing.md`, folders): a folder's content
//! is the listing of its landed children, so folders index after files,
//! deepest first; each carries the mandatory summary and one entry fragment
//! per child referencing the child's address; and a folder re-indexes when
//! what it holds changes, not when its own timestamp does.

mod common;

use inseam_seams::operations::{ExpandRequest, FetchRequest, IndexRequest};

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
async fn a_folder_is_summarized_from_its_children_and_lists_them_as_entries() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(corpus.path().join("kitchen")).expect("mkdir");
    std::fs::write(
        corpus.path().join("kitchen/budget.md"),
        "# Budget\n\ncabinets and counters cost quadrillions\n",
    )
    .expect("writes");
    std::fs::write(
        corpus.path().join("garden.md"),
        "# Garden\n\nnotes about hydrangeas\n",
    )
    .expect("writes");

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let report = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(
        report.indexed, 4,
        "two notes, `kitchen`, and the root: {report}"
    );

    // `kitchen` is summarized from budget.md's summary, so it ranks for the
    // note's words; its one entry names the note and references it.
    let kitchen = common::address_of(&kernel, &corpus.path().join("kitchen"));
    let expanded = ops
        .expand(ExpandRequest {
            address: kitchen.clone(),
        })
        .await
        .expect("expands");
    let summary = expanded.summary.expect("the folder has a summary");
    assert!(summary.contains("quadrillions"), "{summary}");
    let entries: Vec<_> = expanded
        .fragments
        .iter()
        .filter(|f| f.mimetype.starts_with("text/x-inseam-entry"))
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].text.as_deref(),
        Some("budget.md (text/markdown)")
    );
    assert_eq!(
        entries[0].content_address,
        Some(common::address_of(
            &kernel,
            &corpus.path().join("kitchen/budget.md")
        ))
    );
    // The root's listing was composed after `kitchen` landed, so it carries
    // the folder's summary too: both folders rank for the note's words.
    assert_eq!(
        common::folder_hits(ops.as_ref(), "quadrillions").await,
        2,
        "root and kitchen"
    );

    // The root lists both the note and the folder.
    let root = common::address_of(&kernel, corpus.path());
    let expanded = ops
        .expand(ExpandRequest {
            address: root.clone(),
        })
        .await
        .expect("expands");
    let mut names: Vec<&str> = expanded
        .fragments
        .iter()
        .filter(|f| f.mimetype.starts_with("text/x-inseam-entry"))
        .filter_map(|f| f.text.as_deref())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["garden.md (text/markdown)", "kitchen (inode/directory)"]
    );

    // Fetching a folder serves its name listing from the host.
    let fetched = ops
        .fetch(FetchRequest { address: root })
        .await
        .expect("fetches");
    assert_eq!(fetched.content_type, "inode/directory");
    assert_eq!(fetched.text, "garden.md\nkitchen/\n");
}

#[tokio::test]
async fn a_folder_re_indexes_when_what_it_holds_changes() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    let note = corpus.path().join("note.md");
    std::fs::write(&note, "# Note\n\nabout ferns\n").expect("writes");

    let kernel = common::boot(data.path(), "").await;
    let ops = common::ops(&kernel);
    let first = index(ops.as_ref(), corpus.path()).await;
    assert_eq!(first.indexed, 2, "{first}");
    assert_eq!(common::folder_hits(ops.as_ref(), "ferns").await, 1);

    // Untouched: the listing digest matches, whatever the directory mtime.
    let again = index(ops.as_ref(), corpus.path()).await;
    assert_eq!((again.indexed, again.unchanged), (0, 2), "{again}");

    // A new sibling changes the listing, so the folder re-indexes with it.
    std::fs::write(corpus.path().join("other.md"), "# Other\n\nabout orchids\n").expect("writes");
    let grown = index(ops.as_ref(), corpus.path()).await;
    assert_eq!((grown.indexed, grown.unchanged), (2, 1), "{grown}");
    assert_eq!(common::folder_hits(ops.as_ref(), "orchids").await, 1);

    // A changed note changes its summary, and so the folder's listing.
    std::fs::write(&note, "# Note\n\nabout ferns and mosses\n").expect("writes");
    let edited = index(ops.as_ref(), corpus.path()).await;
    assert_eq!((edited.indexed, edited.unchanged), (2, 1), "{edited}");
    assert_eq!(common::folder_hits(ops.as_ref(), "mosses").await, 1);
}
