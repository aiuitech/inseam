//! The error vocabulary service calls speak across seams. Variants are the
//! conditions consumers meaningfully branch on; everything else travels as
//! `Failed` with a message that says what failed and why.

use inseam_kernel::address::{Address, HostId};
use inseam_kernel::store::StoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SeamError {
    #[error("no source at {0} in this node's catalog")]
    UnknownSource(Address),
    #[error("{0} is not indexed as text and has no text fragments to scan")]
    NothingToScan(Address),
    #[error("{0} is binary ({1}); fetch its bytes instead of its text")]
    BinaryFetch(Address, String),
    /// A byte fetch was asked for content past the bound the operation
    /// carries in one message.
    #[error("{address} is {bytes} bytes; fetching bytes is bounded at {limit}")]
    FetchTooLarge {
        address: Address,
        bytes: u64,
        limit: u64,
    },
    #[error("bad address: {0}")]
    Address(#[from] inseam_kernel::address::AddressError),
    /// No mounted connection stewards the named host.
    #[error("no connection on this node stewards host `{0}`")]
    UnknownHost(HostId),
    /// A scope named no host and this node stewards several.
    #[error("several hosts are mounted ({}); name one", hosts_list(.0))]
    AmbiguousHost(Vec<HostId>),
    /// A credential is not granted yet — the owner must authorize it.
    #[error("not authorized: {0}")]
    Unauthorized(String),
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

fn hosts_list(hosts: &[HostId]) -> String {
    hosts
        .iter()
        .map(|h| format!("`{h}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
