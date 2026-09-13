//! The seam catalog (`design/services.md`): the typed service interfaces
//! plugins bind and consume. This crate is the system's real API surface —
//! definitions live here, apart from every provider and consumer, so
//! swapping a provider never touches either side. Each seam names its trait,
//! its well-known key, and the capability facts consumers may branch on.
//!
//! The kernel-provided `store` and `state` services are defined in
//! `inseam-kernel` itself; everything else is here.
//!
//! Beside the seams sit the small conventions every plugin speaking them
//! must agree on — [`text`] (previews, the `scan` line arithmetic, which
//! content types are read as text), [`dates`] (`YYYY-MM-DD` rendering and
//! parsing of the kernel's epoch timestamps), [`listing`] (the text a
//! folder source is composed from and parsed back out of), [`extract`]
//! (the model-free selection of a text's telling sentences and terms), and
//! [`fetch`] (the node's guarded HTTP request, the SSRF posture every plugin
//! that contacts the network shares). They are deliberately not kernel
//! modules: the kernel knows no file format, renders nothing, and opens no
//! socket.

pub mod call_capture;
pub mod connection;
pub mod dates;
pub mod discovery;
pub mod embedder;
pub mod error;
pub mod extract;
pub mod fetch;
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
