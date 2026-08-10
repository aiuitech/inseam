//! inseam core: addressing, indexing, and discovery over a personal data network.
//!
//! This crate implements the node core described in `design/`: the catalog of
//! addresses + envelopes, the semantic-graph index built by transforms
//! (`design/indexing.md`), and the Finder retrieval algorithm
//! (`design/finder.md`), exposed through a transport-neutral operations layer
//! (`design/node-api.md`).

pub mod address;
pub mod agent;
pub mod dates;
pub mod embed;
pub mod finder;
pub mod fragment;
pub mod host_fs;
pub mod indexer;
pub mod llm;
pub mod ops;
pub mod profile;
pub mod store;
pub mod transform;

pub(crate) mod textutil;
