//! The error vocabulary service calls speak across seams. Variants are the
//! conditions consumers meaningfully branch on; everything else travels as
//! `Failed` with a message that says what failed and why.

use inseam_kernel::address::Address;
use inseam_kernel::store::StoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SeamError {
    #[error("no source at {0} in this node's catalog")]
    UnknownSource(Address),
    #[error("{0} is not indexed as text and has no text fragments to scan")]
    NothingToScan(Address),
    #[error("{0} is binary ({1}); fetching binary content over the JSON surface is not supported yet")]
    BinaryFetch(Address, String),
    #[error("bad address: {0}")]
    Address(#[from] inseam_kernel::address::AddressError),
    /// A policy seam (budget guard, boundary filter) refused the call.
    #[error("refused: {0}")]
    Refused(String),
    /// The call needed a capability that is not granted or not mounted.
    #[error("capability unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Failed(String),
}

impl SeamError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}
