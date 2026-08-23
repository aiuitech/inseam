//! Installing a loaded plugin into a running node (`design/composition.md`):
//! the `install_plugin` owner operation uploads the OCR plugin's files, the
//! distribution applies the composition edit while the node keeps running,
//! and the plugin is active with no restart. The reverse paths too — a
//! plugin that fails admission is rolled back, file and tree alike, and an
//! id the node already runs is refused before anything is written.
//!
//! Requires the OCR artifact to be built (see `plugins/ocr/README.md`);
//! the tests skip with a notice when it is absent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use inseam_kernel::substrate::{Composition, CompositionEdits, Kernel};
use inseam_seams::operations::{
    FileBytes, InstallPluginRequest, PluginFile, PluginId, PluginState, PluginView,
    OPERATIONS,
};
use inseam_seams::SeamError;
use inseam_wasm_host::WasmSchemeFactory;

const BASE: &str = r#"
[[entry]]
id = "connections"
plugin = "connections"

[[entry]]
id = "fs"
plugin = "connection-fs"

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
id = "finder"
plugin = "finder"

[[entry]]
id = "sweep"
plugin = "sweep"

[[entry]]
id = "operations"
plugin = "operations"
"#;

fn ocr_directory() -> Option<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ocr");
    directory
        .join("ocr.wasm")
        .exists()
        .then(|| directory.canonicalize().expect("canonicalizes"))
}

fn ocr_files() -> Vec<PluginFile> {
    let directory = ocr_directory().expect("callers checked the artifact exists");
    ["ocr.wasm", "ocr.manifest.toml", "ocr.checks.toml", "fixtures/pixel.png"]
        .into_iter()
        .map(|relative| PluginFile {
            path: relative.to_string(),
            bytes: FileBytes(std::fs::read(directory.join(relative)).expect("reads")),
        })
        .collect()
}

fn request(id: &str, files: Vec<PluginFile>) -> InstallPluginRequest {
    InstallPluginRequest {
        id: PluginId::new(id).expect("valid id"),
        files,
        config: toml::Table::new(),
    }
}

/// A booted node: kernel, the edits it hands the distribution, and the
/// paths the distribution applies edits against.
struct Node {
    kernel: Kernel,
    edits: CompositionEdits,
    base: Composition,
    overlay_path: PathBuf,
}

async fn boot(data_dir: &Path) -> Node {
    let mut kernel = Kernel::boot(
        data_dir,
        inseam_plugins::factories(),
        vec![Arc::new(WasmSchemeFactory::new(data_dir))],
    )
    .await
    .expect("boots");
    let base = Composition::parse(BASE, "test base").expect("base parses");
    kernel.reconcile(&base).await.expect("settles");
    let edits = kernel.take_composition_edits().expect("edits are taken once");
    Node {
        kernel,
        edits,
        base,
        overlay_path: data_dir.join("composition.toml"),
    }
}

/// Run one operation the way `inseam serve` would: the operation on its own
/// task, the node applying the composition edits it submits until it
/// finishes — bounded by the edits one operation submits.
async fn serve_until<T: Send + 'static>(node: &mut Node, mut task: tokio::task::JoinHandle<T>) -> T {
    let mut applied: u32 = 0;
    loop {
        tokio::select! {
            outcome = &mut task => return outcome.expect("the operation task finishes"),
            pending = node.edits.next() => {
                let pending = pending.expect("the kernel holds the submitting end");
                applied += 1;
                assert!(applied <= 4, "one operation submits a bounded number of edits");
                let outcome = node
                    .kernel
                    .apply_composition_edit(&node.base, &node.overlay_path, pending.edit())
                    .await;
                pending.reply(outcome);
            }
        }
    }
}

async fn run_install(node: &mut Node, request: InstallPluginRequest) -> Result<PluginView, SeamError> {
    let operations = node.kernel.service(&OPERATIONS).expect("operations");
    let task = tokio::spawn(async move { operations.install_plugin(request).await });
    serve_until(node, task).await
}

async fn run_plugins(node: &mut Node) -> Vec<PluginView> {
    let operations = node.kernel.service(&OPERATIONS).expect("operations");
    let task = tokio::spawn(async move { operations.plugins().await });
    serve_until(node, task).await.expect("lists")
}

#[tokio::test]
async fn an_uploaded_plugin_mounts_into_the_running_node() {
    if ocr_directory().is_none() {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    }
    let data = tempfile::tempdir().expect("tempdir");
    let mut node = boot(data.path()).await;

    let view = run_install(&mut node, request("ocr", ocr_files()))
        .await
        .expect("installs");
    assert_eq!(view.id, "ocr");
    assert_eq!(view.state, PluginState::Active, "{view:?}");
    let artifact = data.path().join("plugins/ocr/ocr.wasm");
    assert_eq!(view.plugin, format!("wasm:{}", artifact.display()));
    assert!(artifact.is_file());
    assert!(data.path().join("plugins/ocr/fixtures/pixel.png").is_file());

    // The file is the truth a fresh boot converges to.
    let overlay = Composition::load(&node.overlay_path).expect("overlay parses");
    assert_eq!(overlay.entries.len(), 1);
    assert_eq!(overlay.entries[0].id, "ocr");
    assert_eq!(
        overlay.entries[0].plugin.as_deref(),
        Some(format!("wasm:{}", artifact.display()).as_str())
    );

    // Listed alongside the linked plugins, active, with no restart.
    let listed = run_plugins(&mut node).await;
    assert!(listed.iter().any(|plugin| plugin.id == "ocr" && plugin.state == PluginState::Active));
    assert!(listed.iter().any(|plugin| plugin.id == "finder"));

    // The same id again is refused before anything is touched.
    let again = run_install(&mut node, request("ocr", ocr_files())).await;
    assert!(matches!(again, Err(SeamError::Refused(_))), "{again:?}");
}

#[tokio::test]
async fn a_plugin_that_fails_to_mount_is_rolled_back() {
    if ocr_directory().is_none() {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    }
    let data = tempfile::tempdir().expect("tempdir");
    let mut node = boot(data.path()).await;

    // A manifest for a seam the bridge does not mount: construction fails,
    // so the reconcile refuses the entry.
    let mut files = ocr_files();
    files[1].bytes = FileBytes(b"name = \"ocr\"\nversion = \"0.0.1\"\nseam = \"finder\"\n".to_vec());
    let outcome = run_install(&mut node, request("broken", files)).await;
    let Err(SeamError::Refused(reason)) = outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(reason.contains("broken"), "{reason}");
    assert!(!node.overlay_path.exists(), "the overlay never existed, so it is gone again");
    assert!(!data.path().join("plugins/broken").exists(), "the files came out with the entry");
    assert!(!node.kernel.fibers().iter().any(|fiber| fiber.id == "broken"));

    // The node is untouched: a good install afterwards still works.
    let view = run_install(&mut node, request("ocr", ocr_files()))
        .await
        .expect("installs after a rollback");
    assert_eq!(view.state, PluginState::Active, "{view:?}");
}
