//! Test harness: boot a kernel with the offline test composition (hashed
//! embeddings, no LLM, no entity extractor) and optionally layer a patch
//! over it — exactly how a distribution boots a node.

use std::path::Path;
use std::sync::Arc;

use inseam_kernel::substrate::{Composition, Kernel};
use inseam_seams::operations::{Operations, QueryRequest, OPERATIONS};

pub const OFFLINE_BASE: &str = r#"
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
    let mut kernel = Kernel::boot(data_dir, inseam_plugins::factories(), Vec::new())
        .await
        .expect("kernel boots");
    reconcile(&mut kernel, overlay).await;
    kernel
}

pub async fn reconcile(kernel: &mut Kernel, overlay: &str) {
    let base = Composition::parse(OFFLINE_BASE, "test base").expect("base parses");
    let composition = if overlay.is_empty() {
        base
    } else {
        base.layered(Composition::parse(overlay, "test overlay").expect("overlay parses"))
            .expect("layers")
    };
    kernel.reconcile(&composition).await.expect("settles");
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
