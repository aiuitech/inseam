//! Test harness: boot a kernel with the offline test composition (hashed
//! embeddings, no LLM, no entity extractor) and optionally layer a patch
//! over it — exactly how a distribution boots a node.
#![allow(dead_code)] // shared across test binaries; each uses a subset

use std::path::Path;
use std::sync::Arc;

use inseam_kernel::address::{Address, Locator};
use inseam_kernel::substrate::{Composition, Kernel, PluginFactory};
use inseam_seams::connection::CONNECTIONS;
use inseam_seams::operations::{Operations, QueryRequest, OPERATIONS};

pub const OFFLINE_BASE: &str = r#"
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
id = "markdown"
plugin = "transform-markdown"

[[entry]]
id = "chunker"
plugin = "transform-chunker"

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
"#;

pub async fn boot(data_dir: &Path, overlay: &str) -> Kernel {
    boot_with(data_dir, overlay, Vec::new()).await
}

/// Boot with test-only plugin factories beside the first-party set — how a
/// test mounts a transform that exists nowhere but in that test.
pub async fn boot_with(data_dir: &Path, overlay: &str, extra: Vec<Arc<dyn PluginFactory>>) -> Kernel {
    let mut factories = inseam_plugins::factories();
    factories.extend(extra);
    let mut kernel = Kernel::boot(data_dir, factories, Vec::new())
        .await
        .expect("kernel boots");
    reconcile(&mut kernel, overlay).await;
    kernel
}

/// The address of a file on the booted node's filesystem host: the one
/// host the offline base mounts, with the canonical path as its locator —
/// enumeration canonicalizes, so a `/var` symlink to `/private/var` must
/// not make two addresses of one file. A file that does not exist keeps
/// its path as given.
pub fn address_of(kernel: &Kernel, path: &Path) -> Address {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = path.as_path();
    let host = kernel
        .service(&CONNECTIONS)
        .expect("connections bound")
        .snapshot()
        .pop()
        .expect("the filesystem connection registered")
        .host
        .id
        .clone();
    let locator = path
        .to_str()
        .expect("utf-8 path")
        .trim_start_matches('/')
        .to_string();
    Address::new(host, Locator::new(locator).expect("non-empty relative locator"))
}

pub async fn reconcile(kernel: &mut Kernel, overlay: &str) {
    kernel.reconcile(&layered(overlay)).await.expect("settles");
}

/// Boot a kernel whose composition is allowed to leave entries failed —
/// for tests about one entry failing alone.
pub async fn boot_unsettled(data_dir: &Path, overlay: &str) -> Kernel {
    let mut kernel = Kernel::boot(data_dir, inseam_plugins::factories(), Vec::new())
        .await
        .expect("kernel boots");
    let _ = kernel.reconcile(&layered(overlay)).await;
    kernel
}

fn layered(overlay: &str) -> Composition {
    let base = Composition::parse(OFFLINE_BASE, "test base").expect("base parses");
    if overlay.is_empty() {
        base
    } else {
        base.layered(Composition::parse(overlay, "test overlay").expect("overlay parses"))
            .expect("layers")
    }
}

pub fn ops(kernel: &Kernel) -> Arc<dyn Operations> {
    kernel.service(&OPERATIONS).expect("operations bound")
}

pub async fn hits(operations: &dyn Operations, text: &str) -> usize {
    operations
        .query(QueryRequest {
            text: text.into(),
            limit: 10,
        })
        .await
        .expect("queries")
        .results
        .len()
}
