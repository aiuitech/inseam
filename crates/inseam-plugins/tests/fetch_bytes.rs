//! The bytes rung of the discovery ladder (`design/node-api.md`): a source
//! that is not text — an image — is fetched as bytes under its content
//! type, text sources are fetched the same way when a client wants their
//! raw form, and the text `fetch` says to use it instead of refusing
//! silently.

mod common;

use inseam_seams::SeamError;
use inseam_seams::operations::{FetchBytesRequest, FetchRequest, IndexRequest};

const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

#[tokio::test]
async fn binary_sources_are_fetched_as_bytes_under_their_content_type() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("logo.png"), PNG).expect("writes");
    std::fs::write(corpus.path().join("note.md"), "# hi\n\nbytes too\n").expect("writes");

    let kernel = common::boot(data.path(), "").await;
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
        .expect("indexes");
    assert_eq!(report.indexed, 3, "two files and their folder");

    let png = common::address_of(&kernel, &corpus.path().join("logo.png"));
    let fetched = ops
        .fetch_bytes(FetchBytesRequest {
            address: png.clone(),
        })
        .await
        .expect("fetches bytes");
    assert_eq!(fetched.content_type, "image/png");
    assert_eq!(fetched.bytes.0, PNG);
    assert_eq!(fetched.address, png);

    // The text rung names the bytes rung instead of refusing silently.
    let refused = ops
        .fetch(FetchRequest {
            address: png.clone(),
        })
        .await;
    assert!(matches!(refused, Err(SeamError::BinaryFetch(a, t)) if a == png && t == "image/png"));

    // Text sources have bytes too; a client may want the raw form.
    let note = common::address_of(&kernel, &corpus.path().join("note.md"));
    let fetched = ops
        .fetch_bytes(FetchBytesRequest { address: note })
        .await
        .expect("fetches bytes");
    assert_eq!(fetched.content_type, "text/markdown");
    assert_eq!(fetched.bytes.0, b"# hi\n\nbytes too\n");

    // Nothing the index has no record of is ever read.
    let stranger = common::address_of(&kernel, &corpus.path().join("absent.png"));
    let unknown = ops
        .fetch_bytes(FetchBytesRequest {
            address: stranger.clone(),
        })
        .await;
    assert!(matches!(unknown, Err(SeamError::UnknownSource(a)) if a == stranger));
}
