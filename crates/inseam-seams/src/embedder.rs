//! The `embedder` seam: text → vector with a declared model identity
//! (`design/services.md`). The provider declares the identity to the store
//! when it activates, which is what binds the search surface and detects
//! pending re-embeds.

use inseam_kernel::store::VectorScope;
use inseam_kernel::substrate::ServiceKey;

use crate::SeamError;

pub const EMBEDDER: ServiceKey<dyn Embedder> = ServiceKey::new("embedder");

/// Capability-fact keys consumers may branch on.
pub mod facts {
    /// bool: works with no network (hashed bag-of-words, local model).
    pub const OFFLINE: &str = "offline";
    /// string: the embedding model identity.
    pub const MODEL: &str = "model";
    /// number: vector width; 0 means no vectors (full-text only).
    pub const DIMENSIONS: &str = "dimensions";
    /// string: which fragments get vectors (`all` | `summaries`).
    pub const VECTORS: &str = "vectors";
}

#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// Vector width, or `None` when this provider keeps no vectors.
    fn dimensions(&self) -> Option<usize>;

    /// Which fragments get vectors. Part of the declared identity: changing
    /// it re-embeds in place, like a model change.
    fn vectors(&self) -> VectorScope;

    /// Embed texts in order. Long inputs are truncated to a bounded prefix.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError>;
}
