//! Thin wrapper over the conformance harness, kept for in-repo workflows:
//!
//! ```sh
//! cargo run -p inseam-wasm-host --example inspect -- plugins/ocr/ocr.wasm
//! ```
//!
//! The installed CLI exposes the same harness as `inseam plugin check`,
//! which is what the authoring skill and install-time admission use.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let artifact = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: inspect <path-to.wasm>"))?;
    let report = inseam_wasm_host::check_artifact(&artifact).await;
    print!("{}", report.render());
    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}
