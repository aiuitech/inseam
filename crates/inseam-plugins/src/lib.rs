//! First-party native plugins (`design/plugins.md`): every feature of a
//! shipping inseam distribution, as providers and consumers on the seams
//! `inseam-seams` defines. Nothing here is privileged — a community plugin
//! binding the same seam is structurally identical.
//!
//! A distribution links the set it ships and registers the factories with
//! the kernel; [`factories`] returns the full first-party set.

pub mod agent;
pub mod connection_fs;
pub mod embedder;
pub mod finder;
pub mod llm_endpoint;
pub mod operations;
pub mod sweep;
pub mod transforms;

use std::sync::Arc;

use inseam_kernel::substrate::PluginFactory;

/// Every first-party plugin factory, for distributions that ship the full
/// set (the CLI, the macOS app).
pub fn factories() -> Vec<Arc<dyn PluginFactory>> {
    vec![
        Arc::new(connection_fs::FsConnectionFactory),
        Arc::new(llm_endpoint::LlmEndpointFactory),
        Arc::new(embedder::EmbedderFactory),
        Arc::new(transforms::TransformsRegistryFactory),
        Arc::new(transforms::MarkdownFactory),
        Arc::new(transforms::ChunkerFactory),
        Arc::new(transforms::SummarizerFactory),
        Arc::new(transforms::EntityExtractorFactory),
        Arc::new(finder::FinderFactory),
        Arc::new(sweep::SweepFactory),
        Arc::new(operations::OperationsFactory),
    ]
}
