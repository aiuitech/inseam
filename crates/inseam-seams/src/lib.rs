//! The seam catalog (`design/services.md`): the typed service interfaces
//! plugins bind and consume. This crate is the system's real API surface —
//! definitions live here, apart from every provider and consumer, so
//! swapping a provider never touches either side. Each seam names its trait,
//! its well-known key, and the capability facts consumers may branch on.
//!
//! The kernel-provided `store` and `state` services are defined in
//! `inseam-kernel` itself; everything else is here.

pub mod connection;
pub mod embedder;
pub mod error;
pub mod finder;
pub mod llm;
pub mod operations;
pub mod sweep;
pub mod transforms;

pub use error::SeamError;
