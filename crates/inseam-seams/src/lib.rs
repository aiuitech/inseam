//! The seam catalog (`design/services.md`): the typed service interfaces
//! plugins bind and consume. This crate is the system's real API surface —
//! definitions live here, apart from every provider and consumer, so
//! swapping a provider never touches either side. Each seam names its trait,
//! its well-known key, and the capability facts consumers may branch on.
//!
//! The kernel-provided `store` and `state` services are defined in
//! `inseam-kernel` itself; everything else is here.
//!
//! Beside the seams sit the two small conventions every plugin speaking them
//! must agree on — [`text`] (previews, the `scan` line arithmetic, which
//! content types are read as text) and [`dates`] (`YYYY-MM-DD` rendering and
//! parsing of the kernel's epoch timestamps) — and a third, [`listing`],
//! the text a folder source is composed from and parsed back out of. They
//! are deliberately not kernel modules: the kernel knows no file format and
//! renders nothing.

pub mod connection;
pub mod dates;
pub mod embedder;
pub mod error;
pub mod finder;
pub mod listing;
pub mod llm;
pub mod node;
pub mod oauth;
pub mod operations;
pub mod roster;
pub mod routing;
pub mod sweep;
pub mod sync;
pub mod text;
pub mod transforms;
pub mod transport;

pub use error::SeamError;
