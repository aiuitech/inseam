//! Both tiers in one composition: the linked plugin set plus the loaded
//! OCR component (`plugins/ocr`), end to end. A fake `llm` provider stands
//! in for the vision model so the test is hermetic; the OCR plugin cannot
//! tell the difference — it reaches the LLM only through the granted,
//! metered capability the bridge hands it.
//!
//! Requires the OCR artifact to be built (see `plugins/ocr/README.md`);
//! the tests skip with a notice when it is absent so `cargo test` stays
//! green on a fresh checkout.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use inseam_kernel::substrate::{
    ApplyCx, Composition, Facts, Kernel, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::llm::{
    ChatMessage, ChatRequest, EmbedRequest, Llm, ModelInfo, Role, VisionRequest, LLM,
};
use inseam_seams::operations::{ExpandRequest, IndexRequest, QueryRequest, OPERATIONS};
use inseam_seams::SeamError;
use inseam_wasm_host::WasmSchemeFactory;

const CANNED_OCR: &str = "GARAGE SALE SATURDAY 9AM — 12 ELM STREET";

fn ocr_artifact() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/ocr/ocr.wasm");
    path.exists().then(|| path.canonicalize().expect("canonicalizes"))
}

/// A fake `llm` provider: canned vision output, refusing everything else.
struct FakeLlm;

#[async_trait::async_trait]
impl Llm for FakeLlm {
    async fn chat(&self, _request: &ChatRequest) -> Result<ChatMessage, SeamError> {
        Ok(ChatMessage {
            role: Role::Assistant,
            content: Some(String::new()),
            tool_calls: None,
            tool_call_id: None,
        })
    }

    async fn embed(&self, _request: &EmbedRequest<'_>) -> Result<Vec<Vec<f32>>, SeamError> {
        Err(SeamError::Unavailable("fake llm has no embeddings".into()))
    }

    async fn describe_image(&self, request: &VisionRequest<'_>) -> Result<String, SeamError> {
        assert_eq!(
            request.mimetype, "image/png",
            "the bridge passes the real mimetype"
        );
        assert!(
            !request.image.is_empty(),
            "the bridge passes the source bytes"
        );
        Ok(CANNED_OCR.to_string())
    }

    async fn models(&self, _embeddings: bool) -> Result<Vec<ModelInfo>, SeamError> {
        Ok(Vec::new())
    }

    fn spent(&self) -> f64 {
        0.0
    }
}

struct FakeLlmPlugin;

#[async_trait::async_trait]
impl Plugin for FakeLlmPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "fake-llm",
            inject: &[],
            provides: &["llm"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        cx.provide(
            &LLM,
            Arc::new(FakeLlm) as Arc<dyn Llm>,
            Facts::new()
                .with("transform_model", "fake-vision")
                .with("agent_model", "fake-agent"),
        )?;
        Ok(())
    }
}

struct FakeLlmFactory;

impl PluginFactory for FakeLlmFactory {
    fn name(&self) -> &str {
        "fake-llm"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(FakeLlmPlugin))
    }
}

fn composition(artifact: &Path, ocr_config: &str) -> Composition {
    Composition::parse(
        &format!(
            r#"
            [[entry]]
            id = "connections"
            plugin = "connections"

            [[entry]]
            id = "fs"
            plugin = "connection-fs"

            [[entry]]
            id = "llm"
            plugin = "fake-llm"

            [[entry]]
            id = "embedder"
            plugin = "embedder"
            [entry.config]
            provider = "hashed"
            model = "hashed"
            dimensions = 64

            [[entry]]
            id = "transforms"
            plugin = "transforms"

            [[entry]]
            id = "summarizer"
            plugin = "transform-summarizer"

            [[entry]]
            id = "finder"
            plugin = "finder"

            [[entry]]
            id = "sweep"
            plugin = "sweep"

            [[entry]]
            id = "operations"
            plugin = "operations"

            [[entry]]
            id = "ocr"
            plugin = "wasm:{}"
            {ocr_config}
            "#,
            artifact.display()
        ),
        "ocr e2e",
    )
    .expect("composition parses")
}

async fn kernel(data_dir: &Path) -> Kernel {
    let mut factories = inseam_plugins::factories();
    factories.push(Arc::new(FakeLlmFactory));
    Kernel::boot(
        data_dir,
        factories,
        vec![Arc::new(WasmSchemeFactory::new(data_dir))],
    )
    .await
    .expect("boots")
}

// A tiny valid PNG (1x1 transparent pixel) so enumeration sees an image.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
    0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
    0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn loaded_ocr_transcribes_images_through_the_seam() {
    let Some(artifact) = ocr_artifact() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let corpus = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(corpus.path().join("flyer.png"), PNG).expect("writes");
    std::fs::write(corpus.path().join("note.md"), "# Note\n\nabout nothing\n").expect("writes");

    let mut kernel = kernel(data.path()).await;
    kernel
        .reconcile(&composition(&artifact, ""))
        .await
        .expect("settles with the loaded plugin mounted");
    let ops = kernel.service(&OPERATIONS).expect("operations");

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
    assert_eq!(report.indexed, 2);
    assert_eq!(
        report.llm_calls.get("ocr"),
        Some(&1),
        "the vision call was metered against the ocr entry: {report:?}"
    );

    // The transcription is a first-class fragment: findable...
    let found = ops
        .query(QueryRequest {
            text: "garage sale elm street".into(),
            limit: 5,
        })
        .await
        .expect("queries");
    assert!(
        found
            .results
            .first()
            .is_some_and(|r| r.address.to_string().ends_with("flyer.png")),
        "the image surfaces by its transcribed text: {found:?}"
    );

    // ...and structurally a `transcribes` child of the image root.
    let expansion = ops
        .expand(ExpandRequest {
            address: found.results[0].address.clone(),
        })
        .await
        .expect("expands");
    let ocr_fragment = expansion
        .fragments
        .iter()
        .find(|f| f.mimetype.starts_with("text/plain") && f.mimetype.contains("via=ocr"))
        .expect("ocr fragment present");
    assert!(
        ocr_fragment
            .text
            .as_deref()
            .is_some_and(|t| t.contains("GARAGE SALE")),
        "transcript text stored"
    );
    assert!(
        expansion.relations.iter().any(|r| r.kind == "transcribes"),
        "transcribes edge present: {:?}",
        expansion.relations
    );

    // Unmounting the OCR entry dirties exactly the image on the next sweep —
    // claims-aware invalidation covers the loaded tier identically.
    let mut without = composition(&artifact, "");
    if let Some(e) = without.entries.iter_mut().find(|e| e.id == "ocr") {
        e.disabled = Some(true);
    }
    kernel.reconcile(&without).await.expect("settles");
    let ops = kernel.service(&OPERATIONS).expect("operations");
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
    assert_eq!(
        (report.indexed, report.unchanged),
        (1, 1),
        "only the image re-indexed after unmounting ocr: {report}"
    );
    kernel.shutdown().await;
}

#[tokio::test]
async fn release_cooldown_holds_new_artifacts_until_consent() {
    let Some(artifact) = ocr_artifact() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(data.path()).await;

    // A 7-day cooldown: the artifact was first observed just now, so the
    // fiber must refuse to activate...
    let held = composition(&artifact, "[entry.config]\ncooldown_days = 7");
    kernel.reconcile(&held).await.expect("the rest settles");
    let fibers = kernel.fibers();
    let ocr = fibers.iter().find(|f| f.id == "ocr").expect("present");
    let inseam_kernel::substrate::FiberState::Failed(reason) = &ocr.state else {
        panic!("expected the cooldown to hold the fiber, got {:?}", ocr.state);
    };
    assert!(reason.contains("cooldown"), "{reason}");
    assert!(reason.contains("allow_new"), "the error names the consent path: {reason}");

    // ...and the explicit per-entry override is the consent moment.
    let allowed = composition(
        &artifact,
        "[entry.config]\ncooldown_days = 7\nallow_new = true",
    );
    kernel.reconcile(&allowed).await.expect("settles");
    let fibers = kernel.fibers();
    let ocr = fibers.iter().find(|f| f.id == "ocr").expect("present");
    assert_eq!(ocr.state, inseam_kernel::substrate::FiberState::Active);
    kernel.shutdown().await;
}
