//! The conformance harness against the real OCR artifact, and the
//! install-time admission gate built on it. Skips (with a notice) when
//! `plugins/ocr/ocr.wasm` is not built, like the OCR e2e.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use inseam_kernel::substrate::{Composition, FiberState, Kernel};
use inseam_wasm_host::{check_artifact, Outcome, Phase, WasmSchemeFactory};

fn ocr_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ocr");
    dir.join("ocr.wasm")
        .exists()
        .then(|| dir.canonicalize().expect("canonicalizes"))
}

/// Copy the OCR plugin's artifact + sidecars into a sandbox so tests can
/// mutate the checks file without touching the repo.
fn stage(from: &Path, into: &Path) -> PathBuf {
    for file in ["ocr.wasm", "ocr.manifest.toml", "ocr.checks.toml"] {
        std::fs::copy(from.join(file), into.join(file)).expect("stages sidecar");
    }
    std::fs::create_dir_all(into.join("fixtures")).expect("fixture dir");
    std::fs::copy(
        from.join("fixtures/pixel.png"),
        into.join("fixtures/pixel.png"),
    )
    .expect("stages fixture");
    into.join("ocr.wasm")
}

#[tokio::test]
async fn ocr_passes_the_full_harness_including_its_golden_checks() {
    let Some(dir) = ocr_dir() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let report = check_artifact(&dir.join("ocr.wasm")).await;
    assert!(report.passed(), "harness verdict:\n{}", report.render());
    assert!(
        report.effective_claims.iter().any(|c| c == "image/png"),
        "effective claims include image/png: {:?}",
        report.effective_claims
    );
    let golden: Vec<_> = report
        .items
        .iter()
        .filter(|i| i.phase == Phase::Golden)
        .collect();
    assert_eq!(golden.len(), 2, "both golden checks ran: {}", report.render());
    assert!(report.render().contains("PASS"));
}

#[tokio::test]
async fn harness_fails_a_plugin_whose_golden_checks_lie() {
    let Some(dir) = ocr_dir() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let staged = tempfile::tempdir().expect("tempdir");
    let artifact = stage(&dir, staged.path());
    std::fs::write(
        staged.path().join("ocr.checks.toml"),
        r#"
        [[check]]
        name = "claims a transcript it cannot produce"
        mimetype = "image/png"
        bytes_file = "fixtures/pixel.png"
        llm_returns = "the llm said something else entirely"

        [check.expect]
        fragment_contains = "TEXT THAT WILL NOT APPEAR"
        "#,
    )
    .expect("writes broken checks");

    let report = check_artifact(&artifact).await;
    assert!(!report.passed());
    let failure = report.first_failure().expect("a failure is recorded");
    assert!(
        failure.contains("claims a transcript it cannot produce"),
        "the failing check is named: {failure}"
    );
    // The contract battery still passed — only the golden phase failed.
    assert!(report
        .items
        .iter()
        .filter(|i| i.phase == Phase::Contract)
        .all(|i| !matches!(i.outcome, Outcome::Fail(_))));
}

async fn kernel_with(data_dir: &Path) -> Kernel {
    Kernel::boot(
        data_dir,
        inseam_plugins::factories(),
        vec![Arc::new(WasmSchemeFactory::new(data_dir))],
    )
    .await
    .expect("boots")
}

fn wasm_composition(artifact: &Path, config: &str) -> Composition {
    Composition::parse(
        &format!(
            "[[entry]]\nid = \"transforms\"\nplugin = \"transforms\"\n\n\
             [[entry]]\nid = \"candidate\"\nplugin = \"wasm:{}\"\n{config}",
            artifact.display()
        ),
        "admission test",
    )
    .expect("parses")
}

fn candidate_state(kernel: &Kernel) -> FiberState {
    kernel
        .fibers()
        .into_iter()
        .find(|f| f.id == "candidate")
        .expect("candidate fiber present")
        .state
}

#[tokio::test]
async fn admission_refuses_a_failing_plugin_unless_overridden() {
    let Some(dir) = ocr_dir() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let staged = tempfile::tempdir().expect("tempdir");
    let artifact = stage(&dir, staged.path());
    std::fs::write(
        staged.path().join("ocr.checks.toml"),
        "[[check]]\nname = \"impossible\"\nmimetype = \"image/png\"\n\
         [check.expect]\nmin_fragments = 99\n",
    )
    .expect("writes failing checks");

    // Enforce (the default): the fiber refuses with the failing check named
    // and the override path spelled out.
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel_with(data.path()).await;
    kernel
        .reconcile(&wasm_composition(&artifact, ""))
        .await
        .expect("the rest settles");
    let FiberState::Failed(reason) = candidate_state(&kernel) else {
        panic!("expected admission to hold the fiber");
    };
    assert!(reason.contains("failed admission"), "{reason}");
    assert!(reason.contains("impossible"), "the failing check is named: {reason}");
    assert!(reason.contains("admission"), "the override path is named: {reason}");

    // warn: mounts anyway (the cached verdict is reused, not recomputed).
    kernel
        .reconcile(&wasm_composition(&artifact, "[entry.config]\nadmission = \"warn\"\n"))
        .await
        .expect("settles");
    assert_eq!(candidate_state(&kernel), FiberState::Active);
    kernel.shutdown().await;
}

#[tokio::test]
async fn admission_passes_the_real_ocr_plugin() {
    let Some(dir) = ocr_dir() else {
        eprintln!("skipping: plugins/ocr/ocr.wasm not built");
        return;
    };
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel_with(data.path()).await;
    kernel
        .reconcile(&wasm_composition(&dir.join("ocr.wasm"), ""))
        .await
        .expect("settles");
    assert_eq!(candidate_state(&kernel), FiberState::Active);
    kernel.shutdown().await;
}
