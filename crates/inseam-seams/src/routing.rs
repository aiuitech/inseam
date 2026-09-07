//! The `routing` seam: resolving an address to the node that can serve it
//! (`design/network.md`). Addresses and reachability facts are global —
//! the catalog says what exists, the roster says who stewards it and how
//! they are dialed — while sessions are local, so routing dials candidate
//! stewards from roster endpoints and learns liveness by trying. A local
//! steward is served from its own connection without touching the
//! network; a remote one is reached directly or, when a candidate relays,
//! by way of it, under a hop limit so a forwarded request can never loop.
//!
//! The same seam carries discovery fan-out (`design/discovery.md`): a
//! query goes in parallel to the roster nodes that advertise a deep index,
//! bounded in count and time, and the caller merges what came back.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use inseam_kernel::address::{Address, Envelope, HostId};
use inseam_kernel::network::{NodeId, StewardshipRecord};
use inseam_kernel::substrate::ServiceKey;

use crate::SeamError;
use crate::connection::Registration;
use crate::operations::{ExpandResponse, QueryResult};

pub const ROUTING: ServiceKey<dyn Routing> = ServiceKey::new("routing");

/// Most nodes a forwarded request may cross, the requester's own hop
/// included. Two is the design's A→B→C; four covers a chain of relays
/// without letting a loop run long.
pub const HOPS_MAX: u32 = 4;
const _: () = assert!(
    HOPS_MAX >= 2,
    "reaching a host by way of one relay takes two hops"
);
/// Most nodes one query fans out to; more deep-index nodes than this and
/// the closest by roster order win.
pub const FAN_OUT_NODES_MAX: usize = 8;
const _: () = assert!(
    FAN_OUT_NODES_MAX > 0,
    "a fan-out to no node is just the local query"
);
/// How long a fan-out waits for the slowest node before answering with
/// what it has; a query is interactive, a straggler is reported, not
/// awaited.
pub const FAN_OUT_TIMEOUT_DEFAULT: Duration = Duration::from_secs(3);
const _: () = assert!(
    FAN_OUT_TIMEOUT_DEFAULT.as_secs() > 0,
    "a zero timeout would report every node as a straggler"
);

/// Where a host is served from, as far as this node can tell.
pub enum Location {
    /// This node stewards it: serve from the registration's connection.
    Local(Arc<Registration>),
    /// Other nodes claim it; candidates in roster order, liveness unknown
    /// until tried.
    Remote(Vec<StewardshipRecord>),
    /// Nothing in the roster claims the host.
    Unknown,
}

impl fmt::Debug for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A registration holds a live connection handle, which has no
        // `Debug`; the host it stewards is what a reader needs.
        match self {
            Self::Local(registration) => write!(f, "Local({})", registration.host.id),
            Self::Remote(stewards) => f.debug_tuple("Remote").field(stewards).finish(),
            Self::Unknown => f.write_str("Unknown"),
        }
    }
}

/// What one fanned-out node answered — its results, or why it did not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanOutReply {
    pub node: NodeId,
    pub results: Vec<QueryResult>,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[async_trait::async_trait]
pub trait Routing: Send + Sync {
    /// Where `host` is served from, read from the registry and the roster
    /// without dialing anyone.
    async fn locate(&self, host: &HostId) -> Result<Location, SeamError>;

    /// Full text of a source, from whichever steward answers.
    async fn read_text(&self, address: &Address) -> Result<String, SeamError>;

    /// Lines `start..=end` (1-based, inclusive) of a text source.
    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError>;

    /// Raw bytes of a source, bounded by the transport's message size.
    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError>;

    async fn describe(&self, address: &Address) -> Result<Envelope, SeamError>;

    /// A steward's index view of the source: `expand` as served by the
    /// node that indexed it.
    async fn expand(&self, address: &Address) -> Result<ExpandResponse, SeamError>;

    /// Fan a query out to roster nodes that advertise `deep_index`, in
    /// parallel, bounded by [`FAN_OUT_NODES_MAX`] and
    /// [`FAN_OUT_TIMEOUT_DEFAULT`]; one reply per node tried, error or
    /// results. No steward answering a fetch is
    /// [`SeamError::Unreachable`].
    async fn fan_out(&self, text: &str, limit: usize) -> Result<Vec<FanOutReply>, SeamError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::network::StewardCapabilities;

    #[test]
    fn location_debug_names_the_host_and_the_candidates() {
        assert_eq!(format!("{:?}", Location::Unknown), "Unknown");
        let remote = Location::Remote(vec![StewardshipRecord {
            node: NodeId::from_bytes([4; 32]),
            host: HostId::new("fs-mini").expect("valid"),
            capabilities: StewardCapabilities {
                enumerates: true,
                change_feed: false,
                writable: false,
            },
            roots: Vec::new(),
        }]);
        let rendered = format!("{remote:?}");
        assert!(rendered.starts_with("Remote("));
        assert!(rendered.contains("fs-mini"));
    }

    #[test]
    fn fan_out_reply_omits_an_absent_error() {
        let reply = FanOutReply {
            node: NodeId::from_bytes([5; 32]),
            results: Vec::new(),
            elapsed_ms: 12,
            error: None,
        };
        let json = serde_json::to_value(&reply).expect("serializes");
        assert!(json.get("error").is_none());
        assert_eq!(json["elapsed_ms"], 12);
        let back: FanOutReply = serde_json::from_value(json).expect("parses");
        assert_eq!(back.node, reply.node);
        assert_eq!(back.elapsed_ms, 12);
    }
}
