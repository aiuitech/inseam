//! Behavior tests for the routing plugin over the fake network: nodes A,
//! B, and C, where A stewards a filesystem host through the real
//! filesystem connection and the others reach it directly or by relay.

use std::sync::Arc;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Timestamp};
use inseam_kernel::fragment::{Extent, FragmentId, Mimetype};
use inseam_kernel::store::StoredFragment;
use inseam_seams::SeamError;
use inseam_seams::connection::{Connection, EnumeratedSource};
use inseam_seams::routing::{HOPS_MAX, Location, Routing};
use inseam_seams::transport::Transport;

use super::RoutingConfig;
use super::fake::{
    FakeNetwork, NodeSpec, RosterRecords, StubFinder, TestNode, TestSource, host_id, node_id,
};
use super::protocol::{
    RouteBody, RouteReply, RouteRequest, RouteResponse, VISITED_MAX, decode_response,
    encode_request, route_protocol,
};
use crate::connection_fs::{FsHost, WalkConfig};

const FILE_TEXT: &str = "first line\nsecond line\nthird line\n";

/// A's host: a real filesystem connection over a temp directory holding
/// one file, plus the file's address and bytes.
struct StewardedFile {
    host: HostId,
    connection: Arc<FsHost>,
    address: Address,
    _dir: tempfile::TempDir,
}

fn stewarded_file() -> StewardedFile {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, FILE_TEXT).expect("writes");
    let host = host_id("fs-a");
    let connection = Arc::new(FsHost::new(host.clone(), WalkConfig::standard()));
    let address = connection.address_for(&path).expect("addressable");
    StewardedFile {
        host,
        connection,
        address,
        _dir: dir,
    }
}

/// A connection that answers `describe` — the filesystem host describes
/// only by enumeration, so a describing host stands in for a service one.
struct DescribingConnection {
    envelope: Envelope,
}

#[async_trait::async_trait]
impl Connection for DescribingConnection {
    async fn enumerate(&self, _root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        Ok(Vec::new())
    }
    fn locator_prefix(&self, _root: &str) -> Option<String> {
        None
    }
    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        Err(SeamError::UnknownSource(address.clone()))
    }
    async fn read_lines(&self, address: &Address, _s: u64, _e: u64) -> Result<String, SeamError> {
        Err(SeamError::UnknownSource(address.clone()))
    }
    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        Err(SeamError::UnknownSource(address.clone()))
    }
    async fn describe(&self, _address: &Address) -> Result<Envelope, SeamError> {
        Ok(self.envelope.clone())
    }
}

/// A and B on one network, A stewarding the file, B able to dial A.
async fn a_and_b() -> (TestNode, TestNode, StewardedFile, Arc<FakeNetwork>) {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.link(b.id, a.id);
    (a, b, file, network)
}

async fn send(from: &TestNode, to: &TestNode, request: &RouteRequest) -> RouteReply {
    let body = encode_request(request).expect("encodes");
    let bytes = from
        .transport
        .request(
            &inseam_seams::transport::PeerAddress {
                id: to.id,
                endpoints: Vec::new(),
                invitation: None,
            },
            &route_protocol(),
            body,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("exchanges");
    decode_response(&bytes).expect("decodes")
}

fn error_kind(reply: &RouteReply) -> (String, String) {
    match reply {
        RouteReply::Json(RouteResponse::Error { kind, message }) => (kind.clone(), message.clone()),
        other => panic!("expected an error reply, got {other:?}"),
    }
}

#[tokio::test]
async fn locate_answers_local_remote_and_unknown() {
    let (a, b, file, _network) = a_and_b().await;
    assert!(matches!(
        a.service.locate(&file.host).await.expect("locates"),
        Location::Local(_)
    ));
    match b.service.locate(&file.host).await.expect("locates") {
        Location::Remote(stewards) => {
            assert_eq!(stewards.len(), 1);
            assert_eq!(stewards[0].node, a.id);
        }
        other => panic!("expected Remote, got {other:?}"),
    }
    assert!(matches!(
        b.service
            .locate(&host_id("fs-nobody"))
            .await
            .expect("locates"),
        Location::Unknown
    ));
}

#[tokio::test]
async fn a_stale_claim_by_this_node_is_not_a_remote_steward() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    a.claim(&host_id("fs-gone"));
    assert!(matches!(
        a.service
            .locate(&host_id("fs-gone"))
            .await
            .expect("locates"),
        Location::Unknown
    ));
}

#[tokio::test]
async fn a_peer_reads_a_stewarded_file_through_its_steward() {
    let (a, b, file, network) = a_and_b().await;
    let text = b.service.read_text(&file.address).await.expect("reads");
    assert_eq!(text, FILE_TEXT);
    let lines = b
        .service
        .read_lines(&file.address, 2, 3)
        .await
        .expect("reads lines");
    assert_eq!(lines, "second line\nthird line");
    let bytes = b
        .service
        .read_bytes(&file.address)
        .await
        .expect("reads bytes");
    assert_eq!(bytes, FILE_TEXT.as_bytes());
    assert_eq!(
        network.requests_to(a.id),
        3,
        "each read is one exchange with the steward"
    );
    // Served locally on A without touching the network.
    let local = a.service.read_text(&file.address).await.expect("reads");
    assert_eq!(local, FILE_TEXT);
    assert_eq!(network.requests_from(a.id), 0);
}

#[tokio::test]
async fn a_bad_line_range_is_refused_before_anything_is_dialed() {
    let (_a, b, file, network) = a_and_b().await;
    assert!(matches!(
        b.service.read_lines(&file.address, 0, 4).await,
        Err(SeamError::ScanRange { start: 0, end: 4 })
    ));
    assert_eq!(network.requests_from(b.id), 0);
}

#[tokio::test]
async fn a_stewards_typed_error_comes_back_typed() {
    let (_a, b, file, _network) = a_and_b().await;
    let missing: Address = format!("inseam://{}/nowhere/missing.txt", file.host)
        .parse()
        .expect("valid");
    // Expand needs a catalog row on the steward, and there is none.
    assert!(matches!(
        b.service.expand(&missing).await,
        Err(SeamError::UnknownSource(address)) if address == missing
    ));
}

#[tokio::test]
async fn describe_is_answered_by_the_stewards_connection() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let envelope = Envelope {
        source_type: "message".to_string(),
        content_type: Mimetype::parse("text/html").expect("valid"),
        length: ContentLength::Bytes(512),
        created: Some(Timestamp(1)),
        modified: None,
        observed: Timestamp(2),
        properties: Vec::new(),
        hint: Some("Subject".to_string()),
        content_digest: None,
    };
    let host = host_id("mail-a");
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    a.steward(
        &host,
        Arc::new(DescribingConnection {
            envelope: envelope.clone(),
        }),
    );
    network.link(b.id, a.id);
    let address: Address = format!("inseam://{host}/msg-1").parse().expect("valid");
    let described = b.service.describe(&address).await.expect("describes");
    assert_eq!(described, envelope);
}

#[tokio::test]
async fn expand_and_scan_are_served_from_the_stewards_index() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let ranked = TestSource::text(&file.address.to_string(), FILE_TEXT)
        .catalog(&a.store)
        .await;
    a.finder.set_fragments(vec![StoredFragment {
        id: FragmentId(1),
        source: Some(ranked.source.id),
        mimetype: Mimetype::text_plain(),
        text: Some("first line".to_string()),
        extent: Some(Extent::lines(1, 1)),
        content_address: None,
    }]);
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.link(b.id, a.id);

    let expansion = b.service.expand(&file.address).await.expect("expands");
    assert_eq!(expansion.address, file.address);
    assert_eq!(expansion.fragments.len(), 1);
    assert_eq!(expansion.fragments[0].text.as_deref(), Some("first line"));
    assert_eq!(
        a.finder
            .expansions
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        b.finder
            .expansions
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    let request = RouteRequest {
        hops_remaining: HOPS_MAX,
        visited: vec![b.id],
        body: RouteBody::Scan {
            address: file.address.clone(),
            start: 2,
            end: 99,
        },
    };
    match send(&b, &a, &request).await {
        RouteReply::Json(RouteResponse::Scan(scan)) => {
            assert_eq!(scan.text, "second line\nthird line");
            assert_eq!(scan.start, 2);
            assert_eq!(scan.end, 3, "clamped to the envelope's line count");
            assert_eq!(scan.lines_total, Some(3));
            assert_eq!(scan.served_from_fragment, None);
        }
        other => panic!("expected a scan, got {other:?}"),
    }
    assert_eq!(ranked.source.address, file.address);
}

#[tokio::test]
async fn a_query_is_answered_by_the_node_it_lands_on_and_never_forwarded() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let b_result = TestSource::text("inseam://fs-b/own.md", "b's own")
        .catalog(&b.store)
        .await;
    b.finder.set_ranked(vec![b_result]);
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.session(c.id, b.id);
    network.session(b.id, a.id);
    let request = RouteRequest {
        hops_remaining: HOPS_MAX,
        visited: vec![c.id],
        body: RouteBody::Query {
            text: "anything".to_string(),
            limit: 5,
        },
    };
    match send(&c, &b, &request).await {
        RouteReply::Json(RouteResponse::Query(results)) => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].address.to_string(), "inseam://fs-b/own.md");
            assert_eq!(
                results[0].via, None,
                "the wire carries no provenance; the requester stamps it"
            );
        }
        other => panic!("expected results, got {other:?}"),
    }
    assert_eq!(
        b.finder.queries.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        a.finder.queries.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(network.requests_to(a.id), 0, "a query is never forwarded");
}

#[tokio::test]
async fn a_host_nobody_stewards_is_unknown() {
    let (_a, b, _file, network) = a_and_b().await;
    let address: Address = "inseam://fs-nobody/x.txt".parse().expect("valid");
    assert!(matches!(
        b.service.read_text(&address).await,
        Err(SeamError::UnknownHost(host)) if host == host_id("fs-nobody")
    ));
    assert_eq!(network.requests_from(b.id), 0);
}

#[tokio::test]
async fn an_unreachable_steward_names_who_was_tried() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    // No link and no session: B cannot dial A and holds no relay.
    match b.service.read_text(&file.address).await {
        Err(SeamError::Unreachable { host, tried }) => {
            assert_eq!(host, file.host);
            assert_eq!(tried, vec![a.id]);
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
}

#[tokio::test]
async fn a_relay_carries_a_request_the_requester_cannot_dial() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.session(c.id, b.id);
    network.session(b.id, a.id);
    let text = c.service.read_text(&file.address).await.expect("relayed");
    assert_eq!(text, FILE_TEXT);
    assert_eq!(network.requests_from(c.id), 2, "C tried A directly, then B");
    assert_eq!(network.requests_from(b.id), 1, "B forwarded once, to A");
    assert_eq!(network.requests_to(a.id), 2);
}

#[tokio::test]
async fn a_node_that_does_not_relay_says_so() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let mut b_spec = NodeSpec::new(2);
    b_spec.relays = false;
    let b = TestNode::build(b_spec, &records, &network, StubFinder::empty()).await;
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.session(c.id, b.id);
    network.session(b.id, a.id);
    match c.service.read_text(&file.address).await {
        Err(SeamError::Unreachable { tried, .. }) => assert_eq!(tried, vec![a.id, b.id]),
        other => panic!("expected Unreachable, got {other:?}"),
    }
    assert_eq!(
        network.requests_from(b.id),
        0,
        "a non-relay forwards nothing"
    );
}

#[tokio::test]
async fn a_relay_refuses_once_the_hop_budget_is_spent() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.session(c.id, b.id);
    network.session(b.id, a.id);
    let spent = RouteRequest {
        hops_remaining: 0,
        visited: vec![c.id],
        body: RouteBody::ReadText {
            address: file.address.clone(),
        },
    };
    let (kind, message) = error_kind(&send(&c, &b, &spent).await);
    assert_eq!(kind, "unreachable");
    assert!(message.contains("hop limit"), "{message}");
    assert_eq!(network.requests_from(b.id), 0);
}

#[tokio::test]
async fn visited_grows_by_one_per_hop_and_a_loop_is_refused() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.session(c.id, b.id);
    // A request that already crossed B must not be relayed by B again.
    let looped = RouteRequest {
        hops_remaining: HOPS_MAX,
        visited: vec![c.id, b.id],
        body: RouteBody::ReadText {
            address: file.address.clone(),
        },
    };
    let (kind, message) = error_kind(&send(&c, &b, &looped).await);
    assert_eq!(kind, "refused");
    assert!(message.contains("loop"), "{message}");
    // A visited list at its bound with no hops left is well-formed and
    // simply unreachable; one entry more is malformed and never served.
    let full: Vec<_> = (10..10 + u8::try_from(VISITED_MAX).expect("small"))
        .map(node_id)
        .collect();
    let exhausted = RouteRequest {
        hops_remaining: 0,
        visited: full.clone(),
        body: RouteBody::ReadText {
            address: file.address.clone(),
        },
    };
    let (kind, _) = error_kind(&send(&c, &b, &exhausted).await);
    assert_eq!(kind, "unreachable");
    let mut overfull = full;
    overfull.push(node_id(99));
    let malformed = RouteRequest {
        hops_remaining: 0,
        visited: overfull,
        body: RouteBody::ReadText {
            address: file.address.clone(),
        },
    };
    assert!(matches!(
        encode_request(&malformed),
        Err(SeamError::Refused(_))
    ));
    assert_eq!(network.requests_from(b.id), 0);
}

#[tokio::test]
async fn a_relay_skips_a_steward_that_cannot_reach_the_host_either() {
    // C -> B -> D: D claims the host but has no connection; B then tries
    // its other session, A, which serves it.
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let file = stewarded_file();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    let d = TestNode::build(NodeSpec::new(4), &records, &network, StubFinder::empty()).await;
    d.claim(&file.host);
    a.steward(&file.host, file.connection.clone() as Arc<dyn Connection>);
    network.session(c.id, b.id);
    network.session(b.id, d.id);
    network.session(b.id, a.id);
    let text = c
        .service
        .read_text(&file.address)
        .await
        .expect("relayed past D");
    assert_eq!(text, FILE_TEXT);
    assert!(
        network.requests_to(d.id) >= 1,
        "D was asked and could not serve"
    );
}

#[tokio::test]
async fn fan_out_asks_deep_index_nodes_and_reports_a_node_that_never_answers() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let b_result = TestSource::text("inseam://fs-b/doc.md", "on b")
        .catalog(&b.store)
        .await;
    b.finder.set_ranked(vec![b_result]);
    let c = TestNode::build(NodeSpec::new(3), &records, &network, StubFinder::empty()).await;
    let mut shallow = NodeSpec::new(4);
    shallow.deep_index = false;
    let d = TestNode::build(shallow, &records, &network, StubFinder::empty()).await;
    network.link(c.id, a.id);
    network.link(c.id, b.id);
    network.link(c.id, d.id);
    network.black_hole(a.id);
    let replies = c.service.fan_out("doc", 5).await.expect("fans out");
    assert_eq!(
        replies.len(),
        2,
        "A and B advertise a deep index; D does not"
    );
    let to_a = replies
        .iter()
        .find(|r| r.node == a.id)
        .expect("A was asked");
    assert!(to_a.results.is_empty());
    assert!(
        to_a.error
            .as_deref()
            .is_some_and(|e| e.contains("timed out")),
        "{:?}",
        to_a.error
    );
    let to_b = replies
        .iter()
        .find(|r| r.node == b.id)
        .expect("B was asked");
    assert_eq!(to_b.error, None);
    assert_eq!(to_b.results.len(), 1);
    assert_eq!(
        to_b.results[0].via,
        Some(b.id),
        "the requester stamps provenance"
    );
    assert_eq!(network.requests_to(d.id), 0);
    assert_eq!(
        c.finder.queries.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "fan-out never queries the local index"
    );
}

#[tokio::test]
async fn fan_out_prefers_live_sessions_then_always_on_nodes_within_its_bound() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let mut backbone = NodeSpec::new(1);
    backbone.always_on = true;
    let a = TestNode::build(backbone, &records, &network, StubFinder::empty()).await;
    let b = TestNode::build(NodeSpec::new(2), &records, &network, StubFinder::empty()).await;
    let one = RoutingConfig {
        fan_out_nodes_max: 1,
        ..RoutingConfig::default()
    };
    let c = TestNode::build_with(
        NodeSpec::new(3),
        &records,
        &network,
        StubFinder::empty(),
        &one,
    )
    .await;
    network.link(c.id, a.id);
    network.session(c.id, b.id);
    let replies = c.service.fan_out("x", 5).await.expect("fans out");
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].node, b.id,
        "a live session outranks an always-on node"
    );
    network.disconnect_all();
    network.link(c.id, a.id);
    network.link(c.id, b.id);
    let replies = c.service.fan_out("x", 5).await.expect("fans out");
    assert_eq!(
        replies[0].node, a.id,
        "with no session, the always-on node comes first"
    );
}

#[tokio::test]
async fn fan_out_is_skipped_when_disabled() {
    let records = RosterRecords::shared();
    let network = FakeNetwork::shared();
    let a = TestNode::build(NodeSpec::new(1), &records, &network, StubFinder::empty()).await;
    let off = RoutingConfig {
        fan_out: false,
        ..RoutingConfig::default()
    };
    let c = TestNode::build_with(
        NodeSpec::new(3),
        &records,
        &network,
        StubFinder::empty(),
        &off,
    )
    .await;
    network.link(c.id, a.id);
    let replies = c.service.fan_out("x", 5).await.expect("fans out");
    assert!(replies.is_empty());
    assert_eq!(network.requests_from(c.id), 0);
}

#[test]
fn the_config_refuses_zero_timeouts_and_clamps_the_fan_out_count() {
    use super::Limits;
    use inseam_seams::routing::FAN_OUT_NODES_MAX;
    assert!(
        Limits::from_config(&RoutingConfig {
            request_timeout_secs: 0,
            ..RoutingConfig::default()
        })
        .is_err()
    );
    assert!(
        Limits::from_config(&RoutingConfig {
            fan_out_timeout_ms: 0,
            ..RoutingConfig::default()
        })
        .is_err()
    );
    assert!(
        Limits::from_config(&RoutingConfig {
            fan_out_nodes_max: 0,
            ..RoutingConfig::default()
        })
        .is_err()
    );
    let clamped = Limits::from_config(&RoutingConfig {
        fan_out_nodes_max: 1_000,
        ..RoutingConfig::default()
    })
    .expect("valid");
    assert_eq!(clamped.fan_out_nodes_max, FAN_OUT_NODES_MAX);
    let unknown: Result<RoutingConfig, _> = toml::from_str("fan_in = true");
    assert!(unknown.is_err(), "unknown fields are refused");
}

#[test]
fn the_default_config_matches_the_seam_defaults() {
    let config = RoutingConfig::default();
    assert_eq!(config.request_timeout_secs, 30);
    assert!(config.fan_out);
    assert_eq!(config.fan_out_timeout_ms, 3000);
    assert_eq!(config.fan_out_nodes_max, 8);
}
