//! The web host and the link follower, end to end against a loopback
//! server standing in for the open web: a note's links become typed
//! references, the references are fetchable as bytes through the guarded
//! web connection, the guard refuses what the owner did not allow, and a
//! fetch-only host never becomes a sweep's scope.

mod common;

use axum::response::{IntoResponse, Redirect};
use axum::routing::get;

use inseam_kernel::address::Address;
use inseam_seams::operations::{ExpandRequest, FetchBytesRequest, FragmentView, IndexRequest};
use inseam_seams::SeamError;

use inseam_kernel::fragment::Extent;
use inseam_plugins::connection_web::{web_address, web_host_id, WebConnectionConfig, WebHost};
use inseam_seams::connection::Connection;

const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];
/// The web connection's byte cap in the test composition; `/big.png`
/// answers with more.
const CAP: usize = 64;

/// Declares its length explicitly: hyper drops the body-derived length
/// on a `HEAD` answer, and the probe reads the declared one.
async fn png() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CONTENT_LENGTH, "12"),
        ],
        PNG,
    )
}

async fn page() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], "<h1>guide</h1>")
}

async fn redirect() -> Redirect {
    Redirect::temporary("/logo.png")
}

/// An image behind an extension-less URL: only a probe can type it.
async fn mystery() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/png")], PNG)
}

async fn big() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/png")], vec![0u8; CAP + 1])
}

/// A text resource past the cap, declared as such: unfetchable whole, yet
/// its first lines are one scan away.
async fn log() -> impl IntoResponse {
    let mut body = String::new();
    for line in 1..=LOG_LINES {
        body.push_str(&format!("line {line}: something happened\n"));
    }
    assert!(body.len() > CAP);
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/plain".to_string()),
            (axum::http::header::CONTENT_LENGTH, body.len().to_string()),
        ],
        body,
    )
}

const LOG_LINES: u64 = 40;

async fn serve_fake_web() -> String {
    let router = axum::Router::new()
        .route("/log.txt", get(log))
        .route("/logo.png", get(png))
        .route("/guide.html", get(page))
        .route("/redirect", get(redirect))
        .route("/mystery", get(mystery))
        .route("/big.png", get(big));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serves");
    });
    base
}

fn overlay() -> String {
    format!(
        r#"
[[entry]]
id = "web"
plugin = "connection-web"
[entry.config]
allow_hosts = ["127.0.0.1"]
content_bytes_max = {CAP}

[[entry]]
id = "links"
plugin = "transform-links"
"#
    )
}

fn address(url: &str) -> Address {
    web_address(&url::Url::parse(url).expect("url")).expect("addressable")
}

fn reference<'a>(fragments: &'a [FragmentView], url: &str) -> Option<&'a FragmentView> {
    let wanted = address(url);
    fragments
        .iter()
        .find(|f| f.content_address.as_ref() == Some(&wanted))
}

#[tokio::test]
async fn links_become_typed_references_fetchable_through_the_web_host() {
    let base = serve_fake_web().await;
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        corpus.path().join("note.md"),
        format!(
            "# Flyers\n\n\
             The [logo]({base}/logo.png), the [guide]({base}/guide.html), \
             a [moved image]({base}/redirect), an [untyped one]({base}/mystery), \
             a [huge one]({base}/big.png), and a [private one](http://10.0.0.9/private.png).\n"
        ),
    )
    .expect("writes");

    let kernel = common::boot(data.path(), &overlay()).await;
    let ops = common::ops(&kernel);

    // The web host is mounted, fetch-only.
    let hosts = ops.hosts().await.expect("lists");
    let web = hosts
        .iter()
        .find(|h| h.id == web_host_id())
        .expect("the web host is registered");
    assert_eq!(web.kind.as_str(), "web");
    assert!(!web.capabilities.enumerates);

    // A bare scope still means the filesystem: a fetch-only host is never
    // a candidate, so mounting it made nothing ambiguous.
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
    assert_eq!(report.indexed, 1, "{report}");

    // Naming the web host as a scope is refused, not an empty sweep.
    let refused = ops
        .index(IndexRequest {
            host: Some(web_host_id()),
            root: String::new(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await;
    assert!(matches!(refused, Err(SeamError::Refused(_))), "{refused:?}");

    let note = common::address_of(&kernel, &corpus.path().join("note.md"));
    let expansion = ops.expand(ExpandRequest { address: note }).await.expect("expands");
    let fragments = &expansion.fragments;

    // Typed by extension and confirmed by the server.
    let logo = reference(fragments, &format!("{base}/logo.png")).expect("logo referenced");
    assert_eq!(logo.mimetype, "image/png");
    assert_eq!(logo.text, None);
    assert_eq!(
        logo.extent,
        Some(Extent::Bytes { start: 0, end: 12 }),
        "the probe learned the length"
    );
    // A redirect and an extension-less URL are typed by the probe alone.
    assert_eq!(reference(fragments, &format!("{base}/redirect")).expect("followed").mimetype, "image/png");
    assert_eq!(reference(fragments, &format!("{base}/mystery")).expect("probed").mimetype, "image/png");
    // Oversized content is still a reference; only its bytes are refused.
    assert_eq!(reference(fragments, &format!("{base}/big.png")).expect("referenced").mimetype, "image/png");
    // A host the guard refuses keeps the extension's verdict: the link is
    // still a link to an image, the node just may not fetch it.
    let private = reference(fragments, "http://10.0.0.9/private.png").expect("referenced offline");
    assert_eq!(private.mimetype, "image/png");
    assert_eq!(private.extent, None);
    // Content outside the follow list is not referenced.
    assert!(reference(fragments, &format!("{base}/guide.html")).is_none());
    let kinds: Vec<&str> = expansion.relations.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "resolves-to").count(), 5, "{kinds:?}");

    // Every reference is fetchable through the web host, under the cap...
    let fetched = ops
        .fetch_bytes(FetchBytesRequest {
            address: address(&format!("{base}/logo.png")),
        })
        .await
        .expect("fetches");
    assert_eq!(fetched.content_type, "image/png");
    assert_eq!(fetched.bytes.0, PNG);
    let moved = ops
        .fetch_bytes(FetchBytesRequest {
            address: address(&format!("{base}/redirect")),
        })
        .await
        .expect("follows the redirect");
    assert_eq!(moved.bytes.0, PNG);
    // ...and refused past it, or outside the allow list.
    let huge = ops
        .fetch_bytes(FetchBytesRequest {
            address: address(&format!("{base}/big.png")),
        })
        .await;
    assert!(matches!(huge, Err(SeamError::Refused(_))), "{huge:?}");
    let guarded = ops
        .fetch_bytes(FetchBytesRequest {
            address: address("http://10.0.0.9/private.png"),
        })
        .await;
    assert!(matches!(guarded, Err(SeamError::Refused(_))), "{guarded:?}");
    // Nothing references the page, so nothing serves it.
    let unknown = ops
        .fetch_bytes(FetchBytesRequest {
            address: address(&format!("{base}/guide.html")),
        })
        .await;
    assert!(matches!(unknown, Err(SeamError::UnknownSource(_))), "{unknown:?}");
}

#[tokio::test]
async fn without_the_web_host_links_are_typed_offline_and_not_fetchable() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        corpus.path().join("note.md"),
        "# Flyers\n\nSee the [flyer](https://example.com/flyer.jpg).\n",
    )
    .expect("writes");
    let kernel = common::boot(
        data.path(),
        "[[entry]]\nid = \"links\"\nplugin = \"transform-links\"\n",
    )
    .await;
    let ops = common::ops(&kernel);
    ops.index(IndexRequest {
        host: None,
        root: corpus.path().display().to_string(),
        rebuild: false,
        deep_budget: None,
        llm_lane: None,
    })
    .await
    .expect("indexes");

    let note = common::address_of(&kernel, &corpus.path().join("note.md"));
    let expansion = ops.expand(ExpandRequest { address: note }).await.expect("expands");
    let flyer = reference(&expansion.fragments, "https://example.com/flyer.jpg").expect("referenced");
    assert_eq!(flyer.mimetype, "image/jpeg");

    let unreachable = ops
        .fetch_bytes(FetchBytesRequest {
            address: address("https://example.com/flyer.jpg"),
        })
        .await;
    assert!(
        matches!(unreachable, Err(SeamError::UnknownHost(ref h)) if *h == web_host_id()),
        "{unreachable:?}"
    );
}

#[tokio::test]
async fn line_reads_stream_the_head_of_a_resource_the_cap_refuses_whole() {
    let base = serve_fake_web().await;
    let config = WebConnectionConfig {
        allow_hosts: vec!["127.0.0.1".to_string()],
        content_bytes_max: u64::try_from(CAP).expect("fits"),
        ..WebConnectionConfig::default()
    };
    let host = WebHost::new(&config).expect("configures");
    let log = address(&format!("{base}/log.txt"));

    let whole = host.read_text(&log).await;
    assert!(matches!(whole, Err(SeamError::Refused(_))), "{whole:?}");

    let head = host.read_lines(&log, 1, 2).await.expect("reads the head");
    assert_eq!(head, "line 1: something happened\nline 2: something happened");

    // Lines past what the cap can hold are still refused: what is read
    // counts, not what is declared.
    let deep = host.read_lines(&log, LOG_LINES - 1, LOG_LINES).await;
    assert!(matches!(deep, Err(SeamError::Refused(_))), "{deep:?}");

    let beyond = host.read_lines(&log, 1, 0).await;
    assert!(matches!(beyond, Err(SeamError::ScanRange { start: 1, end: 0 })));
}
