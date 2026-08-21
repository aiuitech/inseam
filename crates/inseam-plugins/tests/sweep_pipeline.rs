//! The sweep's pipeline (`design/indexing.md`): sources are planned
//! concurrently but landed in enumeration order, so the index a run builds
//! does not depend on `concurrency` — same fragments, same ids, same rows —
//! and every deep-indexed source is marked indexed only once its search
//! rows are in place.

mod common;

use inseam_kernel::fragment::FragmentId;
use inseam_seams::connection::CONNECTION;
use inseam_seams::operations::IndexRequest;

fn write_corpus(dir: &std::path::Path) {
    for d in 0..3 {
        std::fs::create_dir_all(dir.join(format!("d{d}"))).expect("mkdir");
        for f in 0..12 {
            let mut body = format!("# Doc {d}-{f}\n\nintro about widget{f} and gadget{d}\n");
            for s in 0..4 {
                body.push_str(&format!(
                    "\n## Section {s}\n\nsprocket{s} text mentioning https://example.com/{d}/{f}/{s} here\n"
                ));
            }
            std::fs::write(dir.join(format!("d{d}/doc{f}.md")), body).expect("write");
        }
        std::fs::write(
            dir.join(format!("d{d}/notes.txt")),
            "plain notes\n\nabout flanges and grommets\n".repeat(40),
        )
        .expect("write");
    }
}

/// Every fragment of the store as `(id, source locator, mimetype, text)`,
/// in id order: the whole graph's observable identity.
async fn graph_signature(kernel: &inseam_kernel::substrate::Kernel) -> Vec<(i64, String, String, Option<String>)> {
    let store = kernel.store();
    let host = kernel
        .facts(&CONNECTION)
        .and_then(|f| f.str("host"))
        .expect("host fact")
        .to_string();
    let host_id = inseam_kernel::address::HostId::new(host).expect("valid");
    let mut out = Vec::new();
    for (sid, locator) in store.sources_of_host(&host_id).await.expect("ok") {
        let stored = store.source(sid).await.expect("ok").expect("present");
        let meta = store.index_meta(&stored.address).await.expect("ok").expect("present");
        assert!(meta.indexed, "{locator} is marked indexed once its rows landed");
        let relative = locator.rsplit('/').take(2).collect::<Vec<_>>().join("/");
        for f in store.fragments_of(sid).await.expect("ok") {
            out.push((f.id.0, relative.clone(), f.mimetype.to_string(), f.text.clone()));
        }
    }
    out.sort();
    out
}

async fn index_with(concurrency: usize, corpus: &std::path::Path) -> (inseam_kernel::substrate::Kernel, tempfile::TempDir) {
    let data = tempfile::tempdir().expect("tempdir");
    let overlay = format!(
        "[[entry]]\nid = \"sweep\"\n[entry.config]\nconcurrency = {concurrency}\n"
    );
    let kernel = common::boot(data.path(), &overlay).await;
    let ops = common::ops(&kernel);
    let report = ops
        .index(IndexRequest {
            root: corpus.display().to_string(),
            rebuild: false,
        })
        .await
        .expect("sweeps");
    assert_eq!(report.indexed, 39, "{report}");
    assert_eq!(report.extractive_summaries, 39, "{report}");
    assert!(report.embedded > 39, "every text fragment embedded: {report}");
    (kernel, data)
}

#[tokio::test]
async fn concurrency_does_not_change_the_index() {
    let corpus = tempfile::tempdir().expect("tempdir");
    write_corpus(corpus.path());

    let (sequential, _keep_a) = index_with(1, corpus.path()).await;
    let (concurrent, _keep_b) = index_with(8, corpus.path()).await;

    let a = graph_signature(&sequential).await;
    let b = graph_signature(&concurrent).await;
    assert!(!a.is_empty());
    assert_eq!(a, b, "same fragments under the same ids regardless of concurrency");
    assert_eq!(
        sequential.store().search_rows_count().await.expect("ok"),
        concurrent.store().search_rows_count().await.expect("ok"),
    );
    assert_eq!(
        common::hits(common::ops(&sequential).as_ref(), "sprocket2").await,
        common::hits(common::ops(&concurrent).as_ref(), "sprocket2").await,
    );
}

#[tokio::test]
async fn search_rows_reference_landed_fragments_only() {
    let corpus = tempfile::tempdir().expect("tempdir");
    write_corpus(corpus.path());
    let (kernel, _keep) = index_with(4, corpus.path()).await;
    let store = kernel.store();
    let ids: Vec<FragmentId> = graph_signature(&kernel)
        .await
        .into_iter()
        .map(|(id, _, _, _)| FragmentId(id))
        .collect();
    let rows = store.search_rows_count().await.expect("ok");
    let texted = store
        .fragments(&ids)
        .await
        .expect("ok")
        .into_iter()
        .filter(|f| f.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
        .count();
    assert_eq!(rows, texted, "one search row per text-bearing fragment");
}
