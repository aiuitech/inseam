//! The wire form of `inseam/route/1`: one routed request in, one reply
//! out, over a single transport exchange. Requests are JSON: the hop
//! budget, the nodes already crossed, and the body naming the rung. A
//! reply is framed by its first byte — `0` for a JSON [`RouteResponse`],
//! `1` for the raw bytes of a successful `ReadBytes`. Bytes ride raw
//! because base64 inside JSON would inflate a maximal fetch by a third and
//! push it past the transport's message bound
//! (`inseam_seams::transport::MESSAGE_BYTES_MAX`).
//!
//! Bounds are checked on both sides of the wire: a request is checked
//! before it is sent and again when it is received, and a malformed one
//! is refused by name rather than served.

use inseam_kernel::address::{Address, Envelope, HostId};
use inseam_kernel::network::NodeId;
use serde::{Deserialize, Serialize};

use inseam_seams::SeamError;
use inseam_seams::operations::{ExpandResponse, QueryResult, ScanResponse};
use inseam_seams::routing::HOPS_MAX;
use inseam_seams::text::check_line_range;
use inseam_seams::transport::ProtocolName;

use crate::operations::ladder::QUERY_LIMIT_MAX;

/// The protocol name the handler registers under and every request names.
pub const ROUTE_PROTOCOL: &str = "inseam/route/1";
/// Most nodes a request's `visited` list may name: the requester plus one
/// per hop it may still take.
pub const VISITED_MAX: usize = HOPS_MAX as usize + 1;
const _: () = assert!(
    VISITED_MAX >= 2,
    "a visited list holds the requester and at least one relay"
);
/// Most bytes a routed request may occupy: an address, a query, and the
/// visited list are small; anything larger is not a request.
pub const REQUEST_BYTES_MAX: usize = 64 * 1024;
/// The kind a relay answers with when nobody it tried could serve the
/// host — the one error kind a requester treats as "try the next
/// candidate" rather than as the host's answer.
pub const ERROR_KIND_UNREACHABLE: &str = "unreachable";

const TAG_JSON: u8 = 0;
const TAG_BYTES: u8 = 1;

pub fn route_protocol() -> ProtocolName {
    ProtocolName::new(ROUTE_PROTOCOL).expect("the literal protocol name is valid")
}

/// One routed request: how many more nodes it may cross, which nodes it
/// has crossed (the requester first), and what it asks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRequest {
    pub hops_remaining: u32,
    pub visited: Vec<NodeId>,
    pub body: RouteBody,
}

impl RouteRequest {
    /// The bounds every request must satisfy, checked before send and
    /// after receive: a hop budget within [`HOPS_MAX`], a visited list
    /// within [`VISITED_MAX`] with no node twice, and a body whose own
    /// arguments are well-formed.
    pub fn check_bounds(&self) -> Result<(), SeamError> {
        if self.hops_remaining > HOPS_MAX {
            return Err(malformed(format!(
                "hops_remaining {} exceeds {HOPS_MAX}",
                self.hops_remaining
            )));
        }
        if self.visited.is_empty() {
            return Err(malformed("visited names no requester".to_string()));
        }
        if self.visited.len() > VISITED_MAX {
            return Err(malformed(format!(
                "visited names {} nodes; the bound is {VISITED_MAX}",
                self.visited.len()
            )));
        }
        let mut seen: Vec<NodeId> = Vec::with_capacity(self.visited.len());
        for node in &self.visited {
            if seen.contains(node) {
                return Err(malformed(format!(
                    "visited names node {} twice",
                    node.short()
                )));
            }
            seen.push(*node);
        }
        self.body.check_bounds()
    }
}

/// What a routed request asks of the steward: a rung of the ladder, or
/// a query of the serving node's own index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RouteBody {
    ReadText {
        address: Address,
    },
    ReadLines {
        address: Address,
        start: u64,
        end: u64,
    },
    ReadBytes {
        address: Address,
    },
    Describe {
        address: Address,
    },
    Expand {
        address: Address,
    },
    Scan {
        address: Address,
        start: u64,
        end: u64,
    },
    /// Answered from the serving node's index and never forwarded: a
    /// fan-out reaches every target directly, and a relayed query would
    /// count one index twice.
    Query {
        text: String,
        limit: u32,
    },
}

impl RouteBody {
    /// The host the request must land on; a query lands wherever it is
    /// sent.
    pub fn host(&self) -> Option<&HostId> {
        self.address().map(|address| &address.host)
    }

    pub fn address(&self) -> Option<&Address> {
        match self {
            Self::ReadText { address }
            | Self::ReadLines { address, .. }
            | Self::ReadBytes { address }
            | Self::Describe { address }
            | Self::Expand { address }
            | Self::Scan { address, .. } => Some(address),
            Self::Query { .. } => None,
        }
    }

    /// The operation's name, for logs and refusals.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ReadText { .. } => "read_text",
            Self::ReadLines { .. } => "read_lines",
            Self::ReadBytes { .. } => "read_bytes",
            Self::Describe { .. } => "describe",
            Self::Expand { .. } => "expand",
            Self::Scan { .. } => "scan",
            Self::Query { .. } => "query",
        }
    }

    fn check_bounds(&self) -> Result<(), SeamError> {
        match self {
            Self::ReadLines { start, end, .. } | Self::Scan { start, end, .. } => {
                check_line_range(*start, *end)
            }
            Self::Query { text, limit } => {
                if text.is_empty() {
                    return Err(malformed("query text is empty".to_string()));
                }
                if *limit == 0 {
                    return Err(malformed("query limit is zero".to_string()));
                }
                let bound = u32::try_from(QUERY_LIMIT_MAX).expect("the query bound fits u32");
                if *limit > bound {
                    return Err(malformed(format!("query limit {limit} exceeds {bound}")));
                }
                Ok(())
            }
            Self::ReadText { .. }
            | Self::ReadBytes { .. }
            | Self::Describe { .. }
            | Self::Expand { .. } => Ok(()),
        }
    }
}

/// The JSON reply to every request but a successful `ReadBytes`. Tagged
/// adjacently so a plain string and a struct serialize the same way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum RouteResponse {
    Text(String),
    Envelope(Envelope),
    Expand(ExpandResponse),
    Scan(ScanResponse),
    Query(Vec<QueryResult>),
    /// The steward's typed refusal, or a relay's failure to reach one.
    /// `kind` is the `SeamError` variant in snake case, so a requester
    /// can rebuild the variant it needs and relay the rest verbatim.
    Error {
        kind: String,
        message: String,
    },
}

/// One reply as it crosses the wire: JSON, or the raw bytes of a fetch.
#[derive(Debug, Clone)]
pub enum RouteReply {
    Json(RouteResponse),
    Bytes(Vec<u8>),
}

impl RouteReply {
    /// Whether this reply is a relay saying "not through me": the
    /// requester tries its next candidate instead of taking it as the
    /// host's answer.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            Self::Json(RouteResponse::Error { kind, .. }) if kind == ERROR_KIND_UNREACHABLE
        )
    }
}

pub fn encode_request(request: &RouteRequest) -> Result<Vec<u8>, SeamError> {
    request.check_bounds()?;
    let bytes = serde_json::to_vec(request)
        .map_err(|error| SeamError::failed(format!("encoding a route request: {error}")))?;
    if bytes.len() > REQUEST_BYTES_MAX {
        return Err(malformed(format!(
            "request is {} bytes; the bound is {REQUEST_BYTES_MAX}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Parse a received request, refusing anything malformed by name before
/// any of it is acted on.
pub fn decode_request(bytes: &[u8]) -> Result<RouteRequest, SeamError> {
    if bytes.len() > REQUEST_BYTES_MAX {
        return Err(malformed(format!(
            "request is {} bytes; the bound is {REQUEST_BYTES_MAX}",
            bytes.len()
        )));
    }
    let request: RouteRequest =
        serde_json::from_slice(bytes).map_err(|error| malformed(error.to_string()))?;
    request.check_bounds()?;
    Ok(request)
}

pub fn encode_response(reply: &RouteReply) -> Result<Vec<u8>, SeamError> {
    match reply {
        RouteReply::Json(response) => {
            let json = serde_json::to_vec(response).map_err(|error| {
                SeamError::failed(format!("encoding a route response: {error}"))
            })?;
            let mut framed = Vec::with_capacity(1 + json.len());
            framed.push(TAG_JSON);
            framed.extend_from_slice(&json);
            Ok(framed)
        }
        RouteReply::Bytes(bytes) => {
            let mut framed = Vec::with_capacity(1 + bytes.len());
            framed.push(TAG_BYTES);
            framed.extend_from_slice(bytes);
            Ok(framed)
        }
    }
}

pub fn decode_response(bytes: &[u8]) -> Result<RouteReply, SeamError> {
    let Some((&tag, payload)) = bytes.split_first() else {
        return Err(SeamError::failed("route response is empty".to_string()));
    };
    match tag {
        TAG_JSON => serde_json::from_slice(payload)
            .map(RouteReply::Json)
            .map_err(|error| SeamError::failed(format!("route response is malformed: {error}"))),
        TAG_BYTES => Ok(RouteReply::Bytes(payload.to_vec())),
        other => Err(SeamError::failed(format!(
            "route response carries unknown framing tag {other}"
        ))),
    }
}

/// A steward's error as the wire carries it.
pub fn error_response(error: &SeamError) -> RouteResponse {
    RouteResponse::Error {
        kind: error_kind(error).to_string(),
        message: error.to_string(),
    }
}

/// The requester's typed error for a wire error: the variants whose data
/// the requester already holds (the address, the host) come back typed;
/// the policy kinds keep their message; everything else is a failure
/// carrying the steward's sentence.
pub fn error_from_wire(body: &RouteBody, kind: &str, message: String) -> SeamError {
    match (kind, body.address()) {
        ("unknown_source", Some(address)) => SeamError::UnknownSource(address.clone()),
        ("nothing_to_scan", Some(address)) => SeamError::NothingToScan(address.clone()),
        ("unknown_host", Some(address)) => SeamError::UnknownHost(address.host.clone()),
        ("refused", _) => SeamError::Refused(message),
        ("invalid", _) => SeamError::Invalid(message),
        ("unavailable", _) => SeamError::Unavailable(message),
        ("unauthorized", _) => SeamError::Unauthorized(message),
        _ => SeamError::Failed(message),
    }
}

/// The `SeamError` variant name in snake case.
fn error_kind(error: &SeamError) -> &'static str {
    match error {
        SeamError::UnknownSource(_) => "unknown_source",
        SeamError::NothingToScan(_) => "nothing_to_scan",
        SeamError::ScanRange { .. } => "scan_range",
        SeamError::ScanBeyondEnd { .. } => "scan_beyond_end",
        SeamError::BinaryFetch(_, _) => "binary_fetch",
        SeamError::FetchTooLarge { .. } => "fetch_too_large",
        SeamError::Address(_) => "address",
        SeamError::UnknownHost(_) => "unknown_host",
        SeamError::AmbiguousHost(_) => "ambiguous_host",
        SeamError::Unauthorized(_) => "unauthorized",
        SeamError::Refused(_) => "refused",
        SeamError::Invalid(_) => "invalid",
        SeamError::Unavailable(_) => "unavailable",
        SeamError::Unreachable { .. } => ERROR_KIND_UNREACHABLE,
        SeamError::NotAdmitted(_) => "not_admitted",
        SeamError::Store(_) => "store",
        SeamError::Failed(_) => "failed",
    }
}

fn malformed(why: String) -> SeamError {
    SeamError::Refused(format!("route request is malformed: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn address() -> Address {
        "inseam://fs-a/notes/a.md".parse().expect("valid")
    }

    fn request(body: RouteBody) -> RouteRequest {
        RouteRequest {
            hops_remaining: HOPS_MAX,
            visited: vec![node(1)],
            body,
        }
    }

    #[test]
    fn a_request_roundtrips_through_json() {
        let original = request(RouteBody::ReadLines {
            address: address(),
            start: 3,
            end: 9,
        });
        let bytes = encode_request(&original).expect("encodes");
        let back = decode_request(&bytes).expect("decodes");
        assert_eq!(back, original);
        assert!(
            std::str::from_utf8(&bytes)
                .expect("json")
                .contains("\"op\":\"read_lines\"")
        );
    }

    #[test]
    fn malformed_requests_are_refused_by_name() {
        let cases: Vec<(&str, RouteRequest)> = vec![
            (
                "hops",
                RouteRequest {
                    hops_remaining: HOPS_MAX + 1,
                    ..request(RouteBody::ReadText { address: address() })
                },
            ),
            (
                "twice",
                RouteRequest {
                    visited: vec![node(1), node(1)],
                    ..request(RouteBody::ReadText { address: address() })
                },
            ),
            (
                "bound",
                RouteRequest {
                    visited: (0..=VISITED_MAX)
                        .map(|i| node(u8::try_from(i).expect("small")))
                        .collect(),
                    ..request(RouteBody::ReadText { address: address() })
                },
            ),
            (
                "requester",
                RouteRequest {
                    visited: Vec::new(),
                    ..request(RouteBody::ReadText { address: address() })
                },
            ),
            (
                "range",
                request(RouteBody::Scan {
                    address: address(),
                    start: 0,
                    end: 1,
                }),
            ),
            (
                "limit",
                request(RouteBody::Query {
                    text: "x".to_string(),
                    limit: 0,
                }),
            ),
            (
                "empty query",
                request(RouteBody::Query {
                    text: String::new(),
                    limit: 1,
                }),
            ),
        ];
        for (name, bad) in cases {
            match encode_request(&bad) {
                Err(SeamError::Refused(_)) | Err(SeamError::ScanRange { .. }) => {}
                other => panic!("{name}: expected a refusal, got {other:?}"),
            }
        }
        assert!(matches!(
            decode_request(b"{not json"),
            Err(SeamError::Refused(_))
        ));
    }

    #[test]
    fn json_replies_carry_the_zero_tag() {
        let reply = RouteReply::Json(RouteResponse::Text("hello".to_string()));
        let bytes = encode_response(&reply).expect("encodes");
        assert_eq!(bytes[0], TAG_JSON);
        match decode_response(&bytes).expect("decodes") {
            RouteReply::Json(RouteResponse::Text(text)) => assert_eq!(text, "hello"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_raw_bytes_tag_roundtrips_a_mebibyte() {
        let body: Vec<u8> = (0..(1024u32 * 1024))
            .map(|i| u8::try_from(i % 251).expect("fits"))
            .collect();
        let bytes = encode_response(&RouteReply::Bytes(body.clone())).expect("encodes");
        assert_eq!(bytes[0], TAG_BYTES);
        assert_eq!(bytes.len(), body.len() + 1);
        match decode_response(&bytes).expect("decodes") {
            RouteReply::Bytes(back) => assert_eq!(back, body),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_framing_and_empty_replies_are_errors() {
        assert!(decode_response(&[]).is_err());
        assert!(decode_response(&[7, 1, 2]).is_err());
        assert!(decode_response(&[TAG_JSON, b'{']).is_err());
    }

    #[test]
    fn wire_errors_come_back_typed_where_the_requester_holds_the_data() {
        let body = RouteBody::ReadText { address: address() };
        let wire = error_response(&SeamError::UnknownSource(address()));
        let RouteResponse::Error { kind, message } = wire else {
            panic!("not an error");
        };
        assert_eq!(kind, "unknown_source");
        assert!(matches!(
            error_from_wire(&body, &kind, message),
            SeamError::UnknownSource(a) if a == address()
        ));
        assert!(matches!(
            error_from_wire(&body, "refused", "no".to_string()),
            SeamError::Refused(m) if m == "no"
        ));
        assert!(matches!(
            error_from_wire(&body, "scan_beyond_end", "past".to_string()),
            SeamError::Failed(m) if m == "past"
        ));
        let relayed = RouteReply::Json(error_response(&SeamError::Unreachable {
            host: address().host,
            tried: Vec::new(),
        }));
        assert!(relayed.is_unreachable());
    }
}
