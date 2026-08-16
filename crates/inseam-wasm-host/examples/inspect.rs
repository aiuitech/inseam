//! Validate a sandboxed plugin artifact before mounting it: parses the
//! manifest, compiles the component, calls its exported `claims()`, and
//! reports the effective claim set (declared ∩ exported) plus the requested
//! capabilities. This is the authoring loop's check step:
//!
//! ```sh
//! cargo run -p inseam-wasm-host --example inspect -- plugins/ocr/ocr.wasm
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use inseam_kernel::substrate::{
    ApplyCx, Composition, Facts, Kernel, Manifest, Plugin, PluginError, PluginFactory,
    ServiceKey,
};
use inseam_seams::transforms::{Registration, Transforms, TRANSFORMS};
use inseam_wasm_host::WasmSchemeFactory;

/// A minimal in-process `transforms` registry so the bridge has a seam to
/// register into; inspect then reads the registration back out.
struct ProbeRegistry {
    seen: std::sync::Mutex<Vec<ProbeView>>,
}

#[derive(Clone)]
struct ProbeView {
    name: String,
    llm_call_budget: usize,
    shape_fingerprint: String,
    wants_bytes: bool,
    claims_samples: Vec<(String, bool)>,
}

impl Transforms for ProbeRegistry {
    fn register(&self, registration: Registration) -> Box<dyn FnOnce() + Send> {
        let samples = [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "text/plain",
            "text/markdown",
            "application/pdf",
            "audio/mpeg",
            "video/mp4",
        ];
        let transform = Arc::clone(&registration.transform);
        let claims_samples = samples
            .iter()
            .flat_map(|m| {
                let mt = inseam_kernel::fragment::Mimetype::parse(m).expect("valid");
                let transform = Arc::clone(&transform);
                [(m.to_string(), true), (m.to_string(), false)]
                    .into_iter()
                    .filter(move |(_, is_root)| transform.claims(&mt, *is_root))
            })
            .collect();
        self.seen.lock().unwrap().push(ProbeView {
            name: registration.name.clone(),
            llm_call_budget: registration.llm_call_budget,
            shape_fingerprint: registration.shape_fingerprint.clone(),
            wants_bytes: registration.transform.wants_bytes(),
            claims_samples,
        });
        Box::new(|| {})
    }

    fn snapshot(&self) -> Vec<Arc<Registration>> {
        Vec::new()
    }
}

struct ProbePlugin {
    registry: Arc<ProbeRegistry>,
}

#[async_trait::async_trait]
impl Plugin for ProbePlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "probe-registry",
            inject: &[],
            provides: &["transforms"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let _: &ServiceKey<dyn Transforms> = &TRANSFORMS;
        cx.provide(
            &TRANSFORMS,
            Arc::clone(&self.registry) as Arc<dyn Transforms>,
            Facts::new(),
        )?;
        Ok(())
    }
}

struct ProbeFactory {
    registry: Arc<ProbeRegistry>,
}

impl PluginFactory for ProbeFactory {
    fn name(&self) -> &str {
        "probe-registry"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(ProbePlugin {
            registry: Arc::clone(&self.registry),
        }))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let artifact = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: inspect <path-to.wasm>"))?;
    let data_dir = tempfile::tempdir()?;

    let registry = Arc::new(ProbeRegistry {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let mut kernel = Kernel::boot(
        data_dir.path(),
        vec![Arc::new(ProbeFactory {
            registry: Arc::clone(&registry),
        })],
        vec![Arc::new(WasmSchemeFactory::new(data_dir.path()))],
    )
    .await?;

    let composition = Composition::parse(
        &format!(
            "[[entry]]\nid = \"probe\"\nplugin = \"probe-registry\"\n\n\
             [[entry]]\nid = \"candidate\"\nplugin = \"wasm:{}\"\n",
            artifact.display()
        ),
        "inspect",
    )?;
    kernel.reconcile(&composition).await?;

    let mut failed = false;
    for fiber in kernel.fibers() {
        if fiber.id == "candidate"
            && let inseam_kernel::substrate::FiberState::Failed(reason) = &fiber.state
        {
            eprintln!("MOUNT FAILED: {reason}");
            failed = true;
        }
    }
    for view in registry.seen.lock().unwrap().iter() {
        println!("plugin            {}", view.name);
        println!("shape fingerprint {}", view.shape_fingerprint);
        println!("llm call budget   {}", view.llm_call_budget);
        println!("wants bytes       {}", view.wants_bytes);
        println!("effective claims (against common mimetypes):");
        if view.claims_samples.is_empty() {
            println!("  (none — the plugin will never run; check manifest vs exported claims)");
            failed = true;
        }
        for (mimetype, is_root) in &view.claims_samples {
            println!(
                "  {mimetype}{}",
                if *is_root { " (root)" } else { " (emitted)" }
            );
        }
    }
    kernel.shutdown().await;
    if failed {
        std::process::exit(1);
    }
    println!("\nOK: the component mounts and registers cleanly.");
    Ok(())
}
