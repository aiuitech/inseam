//! The inseam kernel (`design/kernel.md`): the smallest thing that makes
//! "everything is a plugin" true. Exactly two responsibilities:
//!
//! 1. **The substrate** — services, plugins/fibers, effects, events, and the
//!    composition reconciler ([`substrate`]).
//! 2. **The store** — the catalog, the semantic graph, the search surfaces,
//!    and plugin state, with no data migrations anywhere, ever ([`store`]).
//!
//! Plus the vocabulary those two speak: addresses and envelopes
//! ([`address`]), fragments and relations ([`fragment`]). The kernel knows no
//! host, no file format, no ranking algorithm, no transport — those are all
//! plugins on the seams `inseam-seams` defines.

pub mod address;
pub mod dates;
pub mod fragment;
pub mod state;
pub mod store;
pub mod substrate;
pub mod text;
