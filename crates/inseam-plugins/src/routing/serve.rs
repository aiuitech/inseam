//! The steward's and the relay's half of `inseam/route/1`. A request for a
//! host this node stewards is served from its connection and its index
//! through the operations ladder — the same rungs a local operation
//! climbs. A request for anyone else's host is relayed, when this node
//! relays at all and the request has hops left: first to the roster's
//! stewards of the host that the request has not visited, then to the
//! peers this node holds a session with. The reply of the first candidate
//! that answers is the reply; when none does, the requester learns who was
//! tried.
//!
//! Every reply is a reply: a steward's typed error crosses the wire as an
//! error response, never as a failed exchange, so a requester can tell a
//! host's answer ("no such source") from a network's silence.

use std::sync::Arc;

use inseam_kernel::address::HostId;
use inseam_kernel::network::{NodeId, StewardshipRecord};
use inseam_seams::SeamError;
use inseam_seams::finder::FinderRequest;
use inseam_seams::connection::Connection;
use inseam_seams::text::check_line_range;
use inseam_seams::transport::{PeerAddress, RequestHandler};

use super::protocol::{
    ERROR_KIND_UNREACHABLE, RouteBody, RouteReply, RouteRequest, RouteResponse, VISITED_MAX,
    decode_request, decode_response, encode_request, encode_response, error_response,
    route_protocol,
};
use super::{RELAY_PEERS_MAX, RoutingService, STEWARDS_TRIED_MAX};
use crate::operations::ladder::{self, Reader};

/// Serves the protocol on the transport for one [`RoutingService`].
pub(crate) struct RouteHandler {
    service: Arc<RoutingService>,
}

impl RouteHandler {
    /// The handler for one service, as the transport takes it.
    pub(crate) fn serving(service: Arc<RoutingService>) -> Arc<dyn RequestHandler> {
        Arc::new(Self { service })
    }
}

#[async_trait::async_trait]
impl RequestHandler for RouteHandler {
    async fn handle(&self, peer: NodeId, body: Vec<u8>) -> Result<Vec<u8>, SeamError> {
        // A malformed request is refused by name here and never served;
        // everything past this point answers with a reply.
        let request = decode_request(&body)?;
        tracing::debug!(
            peer = %peer.short(),
            operation = request.body.name(),
            hops_remaining = request.hops_remaining,
            "route request"
        );
        let reply = self.service.serve(request).await;
        encode_response(&reply)
    }
}

impl RoutingService {
    /// Answer one well-formed request; an error on this node's side
    /// becomes an error reply so the requester always hears something.
    pub(super) async fn serve(&self, request: RouteRequest) -> RouteReply {
        match self.serve_checked(request).await {
            Ok(reply) => reply,
            Err(error) => RouteReply::Json(error_response(&error)),
        }
    }

    async fn serve_checked(&self, request: RouteRequest) -> Result<RouteReply, SeamError> {
        let me = self.node.id();
        if request.visited.contains(&me) {
            return Err(SeamError::Refused(format!(
                "route request already crossed node {} (a relay loop)",
                me.short()
            )));
        }
        let host = match &request.body {
            // A query answers from this index wherever it lands, and is
            // never forwarded: a fan-out reaches every target itself.
            RouteBody::Query { text, limit } => return self.serve_query(text, *limit).await,
            addressed => addressed
                .host()
                .cloned()
                .expect("every body but a query names a host"),
        };
        if let Some(registration) = self.connections.resolve(&host) {
            let connection = Arc::clone(&registration.connection);
            return self.serve_local(connection, request.body).await;
        }
        self.relay(&host, request).await
    }

    /// Serve a body from this node's own connection to the host and its
    /// own index. Shared by the handler (a peer asked) and the requester
    /// side of the seam (the host turned out to be local).
    pub(super) async fn serve_local(
        &self,
        connection: Arc<dyn Connection>,
        body: RouteBody,
    ) -> Result<RouteReply, SeamError> {
        let reply = match body {
            RouteBody::ReadText { address } => {
                RouteResponse::Text(connection.read_text(&address).await?)
            }
            RouteBody::ReadLines {
                address,
                start,
                end,
            } => {
                // Checked by the requester too; the steward never trusts it.
                check_line_range(start, end)?;
                RouteResponse::Text(connection.read_lines(&address, start, end).await?)
            }
            RouteBody::ReadBytes { address } => {
                let bytes = connection.read_bytes(&address).await?;
                ladder::check_bytes_read(&address, &bytes)?;
                return Ok(RouteReply::Bytes(bytes));
            }
            RouteBody::Describe { address } => {
                RouteResponse::Envelope(connection.describe(&address).await?)
            }
            RouteBody::Expand { address } => {
                let source = ladder::source_at(&self.store, &address).await?;
                RouteResponse::Expand(
                    ladder::expand(&self.store, self.finder.as_ref(), &source).await?,
                )
            }
            RouteBody::Scan {
                address,
                start,
                end,
            } => {
                let source = ladder::source_at(&self.store, &address).await?;
                let window = ladder::scan_window(start, end)?;
                let reader = Ok(Reader::Connection(connection));
                RouteResponse::Scan(ladder::scan(&self.store, source, window, reader).await?)
            }
            RouteBody::Query { text, limit } => return self.serve_query(&text, limit).await,
        };
        Ok(RouteReply::Json(reply))
    }

    async fn serve_query(&self, text: &str, limit: u32) -> Result<RouteReply, SeamError> {
        let limit = ladder::clamp_query_limit(usize::try_from(limit).unwrap_or(usize::MAX));
        let (results, _trace) =
            ladder::query(self.finder.as_ref(), &FinderRequest::new(text, limit)).await?;
        Ok(super::query_results_reply(results))
    }

    /// Forward a request for a host this node does not steward, or say
    /// why not: no hops left, this node does not relay, or nobody tried
    /// answered. The reply is always an answer, so the requester moves
    /// on to its next candidate instead of waiting.
    async fn relay(&self, host: &HostId, request: RouteRequest) -> Result<RouteReply, SeamError> {
        let me = self.node.id();
        if request.hops_remaining == 0 {
            return Ok(unreachable_reply(me, host, &[], "the hop limit is reached"));
        }
        if !self.node.capabilities().relays {
            return Ok(unreachable_reply(me, host, &[], "this node does not relay"));
        }
        let mut visited = request.visited;
        visited.push(me);
        assert!(
            visited.len() <= VISITED_MAX,
            "one node per hop keeps visited within its bound"
        );
        let next = RouteRequest {
            hops_remaining: request.hops_remaining - 1,
            visited,
            body: request.body,
        };
        let stewards = self.roster.stewards_of(host).await?;
        match self.forward_to(&stewards, &next).await {
            Ok(reply) => Ok(reply),
            Err(tried) => Ok(unreachable_reply(
                me,
                host,
                &tried,
                "no steward or relay answered",
            )),
        }
    }

    /// Try the host's stewards the request has not crossed, then the
    /// peers with a live session, each bounded; the first answer wins.
    /// `Err` carries everyone tried, for the requester's error.
    pub(super) async fn forward_to(
        &self,
        stewards: &[StewardshipRecord],
        request: &RouteRequest,
    ) -> Result<RouteReply, Vec<NodeId>> {
        let me = self.node.id();
        let Ok(body) = encode_request(request) else {
            return Err(Vec::new());
        };
        let mut tried: Vec<NodeId> = Vec::with_capacity(STEWARDS_TRIED_MAX + RELAY_PEERS_MAX);
        let candidates: Vec<NodeId> = stewards
            .iter()
            .map(|steward| steward.node)
            .filter(|node| *node != me)
            .filter(|node| !request.visited.contains(node))
            .take(STEWARDS_TRIED_MAX)
            .collect();
        for peer in candidates {
            tried.push(peer);
            if let Some(reply) = self.try_peer(peer, &body).await {
                return Ok(reply);
            }
        }
        let relays: Vec<NodeId> = self
            .transport
            .sessions()
            .iter()
            .map(|session| session.peer)
            .filter(|node| *node != me)
            .filter(|node| !request.visited.contains(node))
            .filter(|node| !tried.contains(node))
            .fold(Vec::new(), |mut distinct, node| {
                if !distinct.contains(&node) {
                    distinct.push(node);
                }
                distinct
            })
            .into_iter()
            .take(RELAY_PEERS_MAX)
            .collect();
        for peer in relays {
            tried.push(peer);
            if let Some(reply) = self.try_peer(peer, &body).await {
                return Ok(reply);
            }
        }
        assert!(tried.len() <= STEWARDS_TRIED_MAX + RELAY_PEERS_MAX);
        Err(tried)
    }

    /// One exchange with one candidate: its answer, or `None` when it is
    /// not a roster node, could not be reached, answered nonsense, or
    /// said the host is unreachable through it.
    async fn try_peer(&self, peer: NodeId, body: &[u8]) -> Option<RouteReply> {
        let record = match self.roster.node(&peer).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                tracing::debug!(peer = %peer.short(), "route candidate is not in the roster");
                return None;
            }
            Err(error) => {
                tracing::warn!(peer = %peer.short(), "roster lookup failed while routing: {error}");
                return None;
            }
        };
        let exchange = self
            .transport
            .request(
                &PeerAddress::from(&record),
                &route_protocol(),
                body.to_vec(),
                self.limits.request_timeout,
            )
            .await;
        let bytes = match exchange {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::debug!(peer = %peer.short(), "route candidate did not answer: {error}");
                return None;
            }
        };
        match decode_response(&bytes) {
            Ok(reply) if reply.is_unreachable() => {
                tracing::debug!(peer = %peer.short(), "route candidate could not reach the host");
                None
            }
            Ok(reply) => Some(reply),
            Err(error) => {
                tracing::warn!(peer = %peer.short(), "route candidate answered badly: {error}");
                None
            }
        }
    }
}

/// The relay's "not through me", naming who it tried so the requester's
/// error can say so.
fn unreachable_reply(me: NodeId, host: &HostId, tried: &[NodeId], why: &str) -> RouteReply {
    let tried_list = if tried.is_empty() {
        "nobody".to_string()
    } else {
        tried
            .iter()
            .map(NodeId::short)
            .collect::<Vec<_>>()
            .join(", ")
    };
    RouteReply::Json(RouteResponse::Error {
        kind: ERROR_KIND_UNREACHABLE.to_string(),
        message: format!(
            "node {} cannot reach host `{host}`: {why} (tried {tried_list})",
            me.short()
        ),
    })
}
