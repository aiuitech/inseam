//! Content references (`design/indexing.md`): a fragment whose content is
//! bytes at an address the index holds a reference to, not text. Two
//! test-only transforms stand in for a link follower and an OCR plugin: one
//! emits an `image/png` fragment referencing a file outside the swept
//! scope, the other claims `image/png` non-roots, wants bytes, and reports
//! what it was handed. Together they prove the planner reads referenced
//! bytes through the connection registry for non-root claimants, that the
//! reference lands in the graph and is fetchable, and that an oversized
//! target leaves a bare reference rather than a broken subtree. A third
//! transform chains a reference off a reference, which is how the crawl
//! depth (`sweep.max_reference_hops`) is shown to stop a chain.

mod common;

use std::sync::Arc;

use inseam_kernel::address::{Address, Locator};
use inseam_kernel::fragment::{Mimetype, NewFragment, RelationKind, Sprout};
use inseam_kernel::substrate::{ApplyCx, Inject, Manifest, Plugin, PluginError, PluginFactory};
use inseam_seams::SeamError;
use inseam_seams::connection::{CONNECTIONS, Connections};
use inseam_seams::llm::LlmLane;
use inseam_seams::operations::{ExpandRequest, FetchBytesRequest, FetchRequest, IndexRequest};
use inseam_seams::transforms::{
    Registration, Transform, TransformCtx, TransformKind, TransformOutput, register_as_effect,
};

const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

/// Structural: every `text/plain` root links to the configured file, the
/// way a markdown transform would emit an image a document embeds.
struct Linker {
    connections: Arc<dyn Connections>,
    target: String,
}

#[async_trait::async_trait]
impl Transform for Linker {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }
    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        is_root && mimetype.essence() == "text/plain"
    }
    async fn apply(&self, _ctx: TransformCtx<'_>) -> TransformOutput {
        let host = self
            .connections
            .snapshot()
            .pop()
            .expect("the filesystem host is mounted")
            .host
            .id
            .clone();
        let locator = Locator::new(self.target.trim_start_matches('/')).expect("valid");
        TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                mimetype: Mimetype::parse("image/png").expect("valid"),
                text: None,
                extent: None,
                content_address: Some(Address::new(host, locator)),
            },
            RelationKind::new("links-to").expect("valid"),
        )])
    }
}

/// Structural over non-root PNGs: references the same bytes again under
/// another type, the way a fetched page would link onward. One more hop.
struct Chainer {
    connections: Arc<dyn Connections>,
    target: String,
}

#[async_trait::async_trait]
impl Transform for Chainer {
    fn kind(&self) -> TransformKind {
        TransformKind::Structural
    }
    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        !is_root && mimetype.essence() == "image/png"
    }
    async fn apply(&self, _ctx: TransformCtx<'_>) -> TransformOutput {
        let host = self
            .connections
            .snapshot()
            .pop()
            .expect("the filesystem host is mounted")
            .host
            .id
            .clone();
        let locator = Locator::new(self.target.trim_start_matches('/')).expect("valid");
        TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                mimetype: Mimetype::parse("image/webp").expect("valid"),
                text: None,
                extent: None,
                content_address: Some(Address::new(host, locator)),
            },
            RelationKind::new("links-to").expect("valid"),
        )])
    }
}

/// Enrichment over non-root images: says exactly what bytes it received,
/// so the test can tell "handed the referenced bytes" from "handed
/// nothing" from "handed the root's bytes".
struct Sniffer;

#[async_trait::async_trait]
impl Transform for Sniffer {
    fn kind(&self) -> TransformKind {
        TransformKind::Enrichment
    }
    fn claims(&self, mimetype: &Mimetype, is_root: bool) -> bool {
        !is_root && mimetype.essence().starts_with("image/")
    }
    fn wants_bytes(&self) -> bool {
        true
    }
    async fn apply(&self, ctx: TransformCtx<'_>) -> TransformOutput {
        let Some(bytes) = ctx.bytes else {
            return TransformOutput::default();
        };
        let leading: Vec<String> = bytes.iter().take(4).map(|b| format!("{b:02x}")).collect();
        TransformOutput::sprouts(vec![Sprout::leaf(
            NewFragment {
                mimetype: Mimetype::parse("text/plain;via=sniff")
                    .expect("valid")
                    .with_param("of", ctx.mimetype.essence().replace('/', "-").as_str()),
                text: Some(format!(
                    "sniffed {} bytes starting {}",
                    bytes.len(),
                    leading.join("")
                )),
                extent: None,
                content_address: None,
            },
            RelationKind::new("transcribes").expect("valid"),
        )])
    }
}

struct TestTransformsPlugin {
    target: String,
}

struct TestTransformsFactory;

impl PluginFactory for TestTransformsFactory {
    fn name(&self) -> &str {
        "test-reference-transforms"
    }
    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        let target = config
            .get("target")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| PluginError("target is required".to_string()))?
            .to_string();
        Ok(Box::new(TestTransformsPlugin { target }))
    }
}

#[async_trait::async_trait]
impl Plugin for TestTransformsPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[
            Inject::required("transforms"),
            Inject::required("connections"),
        ];
        Manifest {
            name: "test-reference-transforms",
            inject: INJECT,
            provides: &[],
        }
    }
    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let connections = cx.get(&CONNECTIONS)?;
        register_as_effect(
            cx,
            Registration {
                entry_id: cx.entry_id().to_string(),
                name: "linker".to_string(),
                transform: Arc::new(Linker {
                    connections,
                    target: self.target.clone(),
                }),
                llm_call_budget: 0,
                llm_lane: LlmLane::Interactive,
                shape_fingerprint: "linker-v1".to_string(),
            },
        )?;
        register_as_effect(
            cx,
            Registration {
                entry_id: format!("{}-chainer", cx.entry_id()),
                name: "chainer".to_string(),
                transform: Arc::new(Chainer {
                    connections: cx.get(&CONNECTIONS)?,
                    target: self.target.clone(),
                }),
                llm_call_budget: 0,
                llm_lane: LlmLane::Interactive,
                shape_fingerprint: "chainer-v1".to_string(),
            },
        )?;
        register_as_effect(
            cx,
            Registration {
                entry_id: format!("{}-sniffer", cx.entry_id()),
                name: "sniffer".to_string(),
                transform: Arc::new(Sniffer),
                llm_call_budget: 0,
                llm_lane: LlmLane::Interactive,
                shape_fingerprint: "sniffer-v1".to_string(),
            },
        )
    }
}

/// Boot with the test transforms mounted, pointing at `target`, and index
/// `corpus`; the sweep's byte cap is `max_content_bytes`, its crawl depth
/// `max_reference_hops`.
async fn index_with_reference(
    data: &std::path::Path,
    corpus: &std::path::Path,
    target: &std::path::Path,
    max_content_bytes: u64,
    max_reference_hops: u32,
) -> inseam_kernel::substrate::Kernel {
    let overlay = format!(
        "[[entry]]\nid = \"sweep\"\n[entry.config]\nmax_content_bytes = {max_content_bytes}\n\
         max_reference_hops = {max_reference_hops}\n\n\
         [[entry]]\nid = \"references\"\nplugin = \"test-reference-transforms\"\n\
         [entry.config]\ntarget = \"{}\"\n",
        target.display()
    );
    let kernel = common::boot_with(data, &overlay, vec![Arc::new(TestTransformsFactory)]).await;
    let report = common::ops(&kernel)
        .index(IndexRequest {
            host: None,
            root: corpus.display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes");
    assert_eq!(report.indexed, 2, "the source and its folder: {report}");
    kernel
}

#[tokio::test]
async fn a_referenced_fragment_hands_its_bytes_to_non_root_claimants() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        corpus.path().join("note.txt"),
        "a note that embeds a diagram\n",
    )
    .expect("writes");
    // Outside the swept scope: the image is never a source of its own, so
    // everything the index knows about it comes through the reference.
    let target = elsewhere.path().join("diagram.png");
    std::fs::write(&target, PNG).expect("writes");
    let target = target.canonicalize().expect("canonical");

    let kernel = index_with_reference(data.path(), corpus.path(), &target, 2_000_000, 1).await;
    let ops = common::ops(&kernel);
    let note = common::address_of(&kernel, &corpus.path().join("note.txt"));
    let image = common::address_of(&kernel, &target);

    let expansion = ops
        .expand(ExpandRequest {
            address: note.clone(),
        })
        .await
        .expect("expands");
    let reference = expansion
        .fragments
        .iter()
        .find(|f| f.mimetype == "image/png")
        .expect("the linked image is a fragment of the note");
    assert_eq!(reference.content_address, Some(image.clone()));
    assert_eq!(reference.text, None, "a reference carries no text");
    let sniffed = expansion
        .fragments
        .iter()
        .find(|f| f.mimetype == "text/plain;via=sniff;of=image-png")
        .expect("the non-root claimant ran");
    assert_eq!(
        sniffed.text.as_deref(),
        Some("sniffed 12 bytes starting 89504e47"),
        "the sniffer saw the referenced bytes, not the note's"
    );
    assert!(sniffed.content_address.is_none());
    let kinds: Vec<&str> = expansion
        .relations
        .iter()
        .map(|r| r.kind.as_str())
        .collect();
    assert!(kinds.contains(&"links-to"), "{kinds:?}");
    assert!(kinds.contains(&"transcribes"), "{kinds:?}");

    // The derived text is searchable under the note.
    assert_eq!(common::hits(ops.as_ref(), "sniffed").await, 1);

    // The reference is fetchable as bytes under the fragment's mimetype,
    // though the image is no source of this node...
    let fetched = ops
        .fetch_bytes(FetchBytesRequest {
            address: image.clone(),
        })
        .await
        .expect("fetches referenced bytes");
    assert_eq!(fetched.content_type, "image/png");
    assert_eq!(fetched.bytes.0, PNG);
    // ...and only as bytes: it has no catalog row for the text rungs.
    assert!(matches!(
        ops.fetch(FetchRequest { address: image.clone() }).await,
        Err(SeamError::UnknownSource(a)) if a == image
    ));
}

#[tokio::test]
async fn an_oversized_reference_stays_a_bare_reference() {
    let corpus = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        corpus.path().join("note.txt"),
        "a note that embeds a diagram\n",
    )
    .expect("writes");
    let target = elsewhere.path().join("diagram.png");
    std::fs::write(&target, PNG).expect("writes");
    let target = target.canonicalize().expect("canonical");

    // A cap below the image's size, above the note's.
    let kernel = index_with_reference(data.path(), corpus.path(), &target, 8, 1).await;
    let ops = common::ops(&kernel);
    let note = common::address_of(&kernel, &corpus.path().join("note.txt"));
    let image = common::address_of(&kernel, &target);

    let expansion = ops
        .expand(ExpandRequest { address: note })
        .await
        .expect("expands");
    let reference = expansion
        .fragments
        .iter()
        .find(|f| f.mimetype == "image/png")
        .expect("the reference is planted regardless");
    assert_eq!(reference.content_address, Some(image));
    assert!(
        !expansion
            .fragments
            .iter()
            .any(|f| f.mimetype.starts_with("text/plain;via=sniff")),
        "bytes over the cap are withheld from claimants"
    );
}

/// The chain the chainer builds: note → png reference (hop 1) → webp
/// reference (hop 2). With one hop allowed the second reference is stored
/// but never followed; with two, it is sniffed like the first.
async fn sniffed_types(max_reference_hops: u32) -> (Vec<String>, usize) {
    let corpus = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        corpus.path().join("note.txt"),
        "a note that embeds a diagram\n",
    )
    .expect("writes");
    let target = elsewhere.path().join("diagram.png");
    std::fs::write(&target, PNG).expect("writes");
    let target = target.canonicalize().expect("canonical");
    let kernel = index_with_reference(
        data.path(),
        corpus.path(),
        &target,
        2_000_000,
        max_reference_hops,
    )
    .await;
    let ops = common::ops(&kernel);
    let note = common::address_of(&kernel, &corpus.path().join("note.txt"));
    let expansion = ops
        .expand(ExpandRequest { address: note })
        .await
        .expect("expands");
    let mut sniffed: Vec<String> = expansion
        .fragments
        .iter()
        .filter(|f| f.mimetype.starts_with("text/plain;via=sniff"))
        .map(|f| f.mimetype.clone())
        .collect();
    sniffed.sort();
    let references = expansion
        .fragments
        .iter()
        .filter(|f| f.content_address.is_some())
        .count();
    (sniffed, references)
}

#[tokio::test]
async fn the_crawl_depth_stops_a_chain_of_references_but_keeps_the_last_link() {
    let (sniffed, references) = sniffed_types(1).await;
    assert_eq!(references, 2, "the second reference is planted regardless");
    assert_eq!(
        sniffed,
        vec!["text/plain;via=sniff;of=image-png".to_string()],
        "only the first hop was followed"
    );

    let (sniffed, references) = sniffed_types(2).await;
    assert_eq!(references, 2);
    assert_eq!(
        sniffed,
        vec![
            "text/plain;via=sniff;of=image-png".to_string(),
            "text/plain;via=sniff;of=image-webp".to_string()
        ],
        "two hops follow both references"
    );

    let (sniffed, references) = sniffed_types(0).await;
    assert_eq!(
        references, 1,
        "at zero hops the first reference is stored, never followed"
    );
    assert!(sniffed.is_empty());
}
