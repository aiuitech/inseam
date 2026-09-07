//! The `routing` plugin (`design/network.md`, `design/discovery.md`):
//! resolves a host to the node that serves it and carries the ladder's
//! reads there, and fans a query out to the nodes whose index is worth
//! asking. It provides the `routing` seam and serves `inseam/route/1` on
//! the transport, so the same code is the requester on one node and the
//! steward — or the relay — on another.
//!
//! Addresses and stewardship are global facts (the catalog and the
//! roster); liveness is local, learned by dialing. A read of a host this
//! node stewards never touches the network. A read of anyone else's host
//! dials the roster's stewards in order, and when none answers, asks the
//! peers this node holds a session with to relay — each hop decrements a
//! budget and appends itself to the request's visited list, so a request
//! crosses at most [`HOPS_MAX`] nodes and never the same one twice.
//!
//! A query fans out directly to every deep-index node and is never
//! relayed: a relay would answer from its own index and the requester
//! would count it twice ([`protocol::RouteBody::Query`]).

pub mod protocol;
mod serve;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use inseam_kernel::address::{Address, Envelope, HostId};
use inseam_kernel::network::{NodeId, NodeRecord};
use inseam_kernel::store::IndexStore;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, STORE,
};
use inseam_seams::connection::{Connections, CONNECTIONS};
use inseam_seams::finder::{Finder, FINDER};
use inseam_seams::node::{Node, NODE};
use inseam_seams::operations::{ExpandResponse, QueryResult};
use inseam_seams::roster::{Roster, ROSTER};
use inseam_seams::routing::{
    FanOutReply, Location, Routing, FAN_OUT_NODES_MAX, FAN_OUT_TIMEOUT_DEFAULT, HOPS_MAX, ROUTING,
};
use inseam_seams::text::check_line_range;
use inseam_seams::transport::{
    register_as_effect, PeerAddress, Transport, REQUEST_TIMEOUT_DEFAULT, TRANSPORT,
};
use inseam_seams::SeamError;

use crate::operations::ladder::QUERY_LIMIT_MAX;
use protocol::{
    encode_request, error_from_wire, route_protocol, RouteBody, RouteReply, RouteRequest,
    RouteResponse,
};

/// Most roster stewards of one host dialed directly for one request; a
/// host with more stewards than this is reached through the first four
/// or through a relay.
pub const STEWARDS_TRIED_MAX: usize = 4;
const _: () = assert!(STEWARDS_TRIED_MAX >= 1, "at least one steward is always tried");
/// Most session peers asked to relay one request after every steward
/// failed; a relay fans the request no further than this either.
pub const RELAY_PEERS_MAX: usize = 4;
const _: () = assert!(RELAY_PEERS_MAX >= 1, "relaying through nobody is no relay");

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    /// How long one routed read may take end to end before the requester
    /// gives up on the candidate and tries the next.
    pub request_timeout_secs: u32,
    /// Whether queries fan out at all; `false` keeps every query local.
    pub fan_out: bool,
    /// How long a fan-out waits for each node before reporting it as a
    /// straggler and answering with what it has.
    pub fan_out_timeout_ms: u32,
    /// Most nodes one query fans out to, held to [`FAN_OUT_NODES_MAX`].
    pub fan_out_nodes_max: u32,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            request_timeout_secs: u32::try_from(REQUEST_TIMEOUT_DEFAULT.as_secs())
                .expect("the default request timeout is seconds, not years"),
            fan_out: true,
            fan_out_timeout_ms: u32::try_from(FAN_OUT_TIMEOUT_DEFAULT.as_millis())
                .expect("the default fan-out timeout is milliseconds, not days"),
            fan_out_nodes_max: u32::try_from(FAN_OUT_NODES_MAX).expect("the fan-out bound fits u32"),
        }
    }
}

/// The config as the service runs it: durations, and the fan-out count
/// clamped to the seam's bound.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    request_timeout: Duration,
    fan_out: bool,
    fan_out_timeout: Duration,
    fan_out_nodes_max: usize,
}

impl Limits {
    pub(crate) fn from_config(config: &RoutingConfig) -> Result<Self, PluginError> {
        if config.request_timeout_secs == 0 {
            return Err(PluginError(
                "routing.request_timeout_secs must be greater than zero".to_string(),
            ));
        }
        if config.fan_out_timeout_ms == 0 {
            return Err(PluginError(
                "routing.fan_out_timeout_ms must be greater than zero".to_string(),
            ));
        }
        if config.fan_out_nodes_max == 0 {
            return Err(PluginError(
                "routing.fan_out_nodes_max must be greater than zero; set fan_out = false to keep queries local"
                    .to_string(),
            ));
        }
        let fan_out_nodes_max = usize::try_from(config.fan_out_nodes_max)
            .unwrap_or(FAN_OUT_NODES_MAX)
            .min(FAN_OUT_NODES_MAX);
        assert!(fan_out_nodes_max >= 1);
        assert!(fan_out_nodes_max <= FAN_OUT_NODES_MAX);
        Ok(Self {
            request_timeout: Duration::from_secs(u64::from(config.request_timeout_secs)),
            fan_out: config.fan_out,
            fan_out_timeout: Duration::from_millis(u64::from(config.fan_out_timeout_ms)),
            fan_out_nodes_max,
        })
    }
}

pub struct RoutingPlugin {
    limits: Limits,
}

impl RoutingPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: RoutingConfig = parse_config(config)?;
        Ok(Self {
            limits: Limits::from_config(&config)?,
        })
    }
}

pub struct RoutingFactory;

impl inseam_kernel::substrate::PluginFactory for RoutingFactory {
    fn name(&self) -> &str {
        "routing"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(RoutingPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for RoutingPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[
            Inject::required("store"),
            Inject::required("node"),
            Inject::required("transport"),
            Inject::required("roster"),
            Inject::required("connections"),
            Inject::required("finder"),
        ];
        Manifest {
            name: "routing",
            inject: INJECT,
            provides: &["routing"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let service = RoutingService::new(
            cx.get(&STORE)?,
            cx.get(&NODE)?,
            cx.get(&TRANSPORT)?,
            cx.get(&ROSTER)?,
            cx.get(&CONNECTIONS)?,
            cx.get(&FINDER)?,
            self.limits,
        );
        // The handler is registered before the seam is provided, so no
        // consumer can route through a node that is not yet serving.
        register_as_effect(cx, route_protocol(), serve::RouteHandler::serving(Arc::clone(&service)))?;
        cx.provide(&ROUTING, service as Arc<dyn Routing>, Facts::new())?;
        Ok(())
    }
}

/// The provider: requester and steward in one, sharing the seams both
/// sides read.
pub struct RoutingService {
    store: Arc<IndexStore>,
    node: Arc<dyn Node>,
    transport: Arc<dyn Transport>,
    roster: Arc<dyn Roster>,
    connections: Arc<dyn Connections>,
    finder: Arc<dyn Finder>,
    limits: Limits,
}

impl RoutingService {
    pub(crate) fn new(
        store: Arc<IndexStore>,
        node: Arc<dyn Node>,
        transport: Arc<dyn Transport>,
        roster: Arc<dyn Roster>,
        connections: Arc<dyn Connections>,
        finder: Arc<dyn Finder>,
        limits: Limits,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            node,
            transport,
            roster,
            connections,
            finder,
            limits,
        })
    }

    /// Carry one host-addressed body to whichever node serves the host,
    /// and hand back a successful reply: a steward's typed error comes
    /// back as the error it names.
    async fn route(&self, body: RouteBody) -> Result<RouteReply, SeamError> {
        // Every caller below builds a body around an address; only a
        // query has none, and queries fan out instead of routing.
        let host = body
            .host()
            .cloned()
            .expect("routed bodies name a host; queries never route");
        let reply = match self.locate(&host).await? {
            Location::Local(registration) => {
                self.serve_local(Arc::clone(&registration.connection), body.clone())
                    .await?
            }
            Location::Remote(stewards) => {
                let request = RouteRequest {
                    hops_remaining: HOPS_MAX,
                    visited: vec![self.node.id()],
                    body: body.clone(),
                };
                match self.forward_to(&stewards, &request).await {
                    Ok(reply) => reply,
                    Err(tried) => return Err(SeamError::Unreachable { host, tried }),
                }
            }
            Location::Unknown => return Err(SeamError::UnknownHost(host)),
        };
        match reply {
            RouteReply::Json(RouteResponse::Error { kind, message }) => {
                Err(error_from_wire(&body, &kind, message))
            }
            served => Ok(served),
        }
    }

    /// The nodes a query fans out to: every other node advertising a deep
    /// index, those with a live session first, then the always-on ones,
    /// then the rest in roster order, held to the configured count.
    async fn fan_out_targets(&self) -> Result<Vec<NodeRecord>, SeamError> {
        let me = self.node.id();
        let live: HashSet<NodeId> = self.transport.sessions().iter().map(|s| s.peer).collect();
        let mut targets: Vec<NodeRecord> = self
            .roster
            .nodes()
            .await?
            .into_iter()
            .filter(|record| record.id != me)
            .filter(|record| record.capabilities.deep_index)
            .collect();
        // A stable sort keeps roster order within each tier.
        targets.sort_by_key(|record| (!live.contains(&record.id), !record.capabilities.always_on));
        targets.truncate(self.limits.fan_out_nodes_max);
        assert!(targets.len() <= FAN_OUT_NODES_MAX);
        Ok(targets)
    }

    /// One node's part in a fan-out: its results stamped with its id, or
    /// the reason it gave none. Bounded twice — by the timeout handed to
    /// the transport and by a timer of our own — because a fan-out must
    /// answer on time even if a transport misbehaves.
    async fn fan_out_one(&self, record: &NodeRecord, body: Vec<u8>) -> FanOutReply {
        let started = Instant::now();
        let peer = PeerAddress::from(record);
        let protocol = route_protocol();
        let exchange = self
            .transport
            .request(&peer, &protocol, body, self.limits.fan_out_timeout);
        let outcome = tokio::time::timeout(self.limits.fan_out_timeout, exchange).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut reply = FanOutReply {
            node: record.id,
            results: Vec::new(),
            elapsed_ms,
            error: None,
        };
        match outcome {
            Err(_elapsed) => {
                reply.error = Some(format!(
                    "timed out after {} ms",
                    self.limits.fan_out_timeout.as_millis()
                ));
            }
            Ok(Err(error)) => reply.error = Some(error.to_string()),
            Ok(Ok(bytes)) => match protocol::decode_response(&bytes) {
                Ok(RouteReply::Json(RouteResponse::Query(mut results))) => {
                    results.truncate(QUERY_LIMIT_MAX);
                    for result in &mut results {
                        result.via = Some(record.id);
                    }
                    reply.results = results;
                }
                Ok(RouteReply::Json(RouteResponse::Error { kind, message })) => {
                    reply.error = Some(format!("{kind}: {message}"));
                }
                Ok(_other) => reply.error = Some("answered with something other than results".to_string()),
                Err(error) => reply.error = Some(error.to_string()),
            },
        }
        reply
    }
}

#[async_trait::async_trait]
impl Routing for RoutingService {
    async fn locate(&self, host: &HostId) -> Result<Location, SeamError> {
        if let Some(registration) = self.connections.resolve(host) {
            return Ok(Location::Local(registration));
        }
        let me = self.node.id();
        // A stale claim by this node itself is not a remote steward: the
        // registry above is the truth about what this node serves.
        let stewards: Vec<_> = self
            .roster
            .stewards_of(host)
            .await?
            .into_iter()
            .filter(|steward| steward.node != me)
            .collect();
        if stewards.is_empty() {
            Ok(Location::Unknown)
        } else {
            Ok(Location::Remote(stewards))
        }
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let body = RouteBody::ReadText {
            address: address.clone(),
        };
        match self.route(body).await? {
            RouteReply::Json(RouteResponse::Text(text)) => Ok(text),
            other => Err(unexpected_reply("read_text", &other)),
        }
    }

    async fn read_lines(&self, address: &Address, start: u64, end: u64) -> Result<String, SeamError> {
        // Checked here and again by the steward: a bad range never
        // crosses the wire.
        check_line_range(start, end)?;
        let body = RouteBody::ReadLines {
            address: address.clone(),
            start,
            end,
        };
        match self.route(body).await? {
            RouteReply::Json(RouteResponse::Text(text)) => Ok(text),
            other => Err(unexpected_reply("read_lines", &other)),
        }
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let body = RouteBody::ReadBytes {
            address: address.clone(),
        };
        match self.route(body).await? {
            RouteReply::Bytes(bytes) => Ok(bytes),
            other => Err(unexpected_reply("read_bytes", &other)),
        }
    }

    async fn describe(&self, address: &Address) -> Result<Envelope, SeamError> {
        let body = RouteBody::Describe {
            address: address.clone(),
        };
        match self.route(body).await? {
            RouteReply::Json(RouteResponse::Envelope(envelope)) => Ok(envelope),
            other => Err(unexpected_reply("describe", &other)),
        }
    }

    async fn expand(&self, address: &Address) -> Result<ExpandResponse, SeamError> {
        let body = RouteBody::Expand {
            address: address.clone(),
        };
        match self.route(body).await? {
            RouteReply::Json(RouteResponse::Expand(expansion)) => Ok(expansion),
            other => Err(unexpected_reply("expand", &other)),
        }
    }

    async fn fan_out(&self, text: &str, limit: usize) -> Result<Vec<FanOutReply>, SeamError> {
        if !self.limits.fan_out {
            return Ok(Vec::new());
        }
        let targets = self.fan_out_targets().await?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let limit = u32::try_from(limit.clamp(1, QUERY_LIMIT_MAX)).expect("the clamped limit fits u32");
        // Hops are zero because a query is never forwarded: the target
        // answers from its own index or not at all.
        let body = encode_request(&RouteRequest {
            hops_remaining: 0,
            visited: vec![self.node.id()],
            body: RouteBody::Query {
                text: text.to_string(),
                limit,
            },
        })?;
        let calls = targets
            .iter()
            .map(|record| self.fan_out_one(record, body.clone()));
        let replies = join_all(calls).await;
        assert_eq!(replies.len(), targets.len(), "one reply per target, error or results");
        Ok(replies)
    }
}

/// A steward answered the wrong shape: a protocol mismatch, named.
fn unexpected_reply(operation: &str, reply: &RouteReply) -> SeamError {
    let shape = match reply {
        RouteReply::Bytes(_) => "raw bytes".to_string(),
        RouteReply::Json(RouteResponse::Text(_)) => "text".to_string(),
        RouteReply::Json(RouteResponse::Envelope(_)) => "an envelope".to_string(),
        RouteReply::Json(RouteResponse::Expand(_)) => "an expansion".to_string(),
        RouteReply::Json(RouteResponse::Scan(_)) => "a scan".to_string(),
        RouteReply::Json(RouteResponse::Query(_)) => "query results".to_string(),
        RouteReply::Json(RouteResponse::Error { kind, .. }) => format!("a `{kind}` error"),
    };
    SeamError::failed(format!("routed {operation} was answered with {shape}"))
}

/// Results a peer's index produced, as the operations layer renders them
/// for a fan-out reply; kept public for the handler and the tests.
pub(crate) fn query_results_reply(results: Vec<QueryResult>) -> RouteReply {
    RouteReply::Json(RouteResponse::Query(results))
}
