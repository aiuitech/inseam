//! First-party linked plugins (`design/plugins.md`): every feature of a
//! shipping inseam distribution, as providers and consumers on the seams
//! `inseam-seams` defines. Nothing here is privileged — a community plugin
//! binding the same seam is structurally identical.
//!
//! The crate is plugin-centric: **one directory per plugin**, named after
//! the plugin's composition name (`transform-markdown` lives in
//! `transform_markdown/`), holding its `mod.rs`, any pure helpers it owns,
//! and — for transforms — its golden checks (`<registration>.checks.toml`)
//! beside the source. Nothing lives outside a plugin directory but this
//! file, which declares the modules and, in [`factories`], the full
//! first-party set a distribution links.

pub mod connection_fs;
pub mod connection_google;
pub mod connection_web;
pub mod connections;
pub mod embedder;
pub mod finder;
pub mod llm_endpoint;
pub mod node;
pub mod oauth;
pub mod operations;
pub mod roster;
pub mod routing;
pub mod settings;
pub mod sweep;
pub mod sync;
pub mod transform_chunker;
pub mod transform_directory;
pub mod transform_entities;
pub mod transform_links;
pub mod transform_markdown;
pub mod transform_summarizer;
pub mod transforms;
pub mod transport_iroh;

use std::sync::Arc;

use inseam_kernel::substrate::PluginFactory;

/// Every first-party plugin factory, for distributions that ship the full
/// set (the CLI, the macOS app).
pub fn factories() -> Vec<Arc<dyn PluginFactory>> {
    vec![
        Arc::new(node::NodeFactory),
        Arc::new(transport_iroh::IrohTransportFactory),
        Arc::new(connections::ConnectionsRegistryFactory),
        Arc::new(roster::RosterFactory),
        Arc::new(sync::SyncFactory),
        Arc::new(routing::RoutingFactory),
        Arc::new(connection_fs::FsConnectionFactory),
        Arc::new(connection_google::GoogleConnectionFactory),
        Arc::new(connection_web::WebConnectionFactory),
        Arc::new(oauth::OAuthFactory),
        Arc::new(llm_endpoint::LlmEndpointFactory),
        Arc::new(embedder::EmbedderFactory),
        Arc::new(transforms::TransformsRegistryFactory),
        Arc::new(transform_markdown::MarkdownFactory),
        Arc::new(transform_directory::DirectoryFactory),
        Arc::new(transform_chunker::ChunkerFactory),
        Arc::new(transform_summarizer::SummarizerFactory),
        Arc::new(transform_entities::EntityExtractorFactory),
        Arc::new(transform_links::LinksFactory),
        Arc::new(finder::FinderFactory),
        Arc::new(sweep::SweepFactory),
        Arc::new(operations::OperationsFactory),
    ]
}
