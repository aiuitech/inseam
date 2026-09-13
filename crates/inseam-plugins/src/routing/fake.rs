//! In-memory fakes for the routing tests: a node identity, a roster over
//! shared records, a connections registry, a finder with canned answers,
//! and a transport over a fake network that knows who can dial whom, who
//! holds a session with whom, and which node never answers. Every fake
//! answers from memory — no clocks, no sockets, no sleeps.
//!
//! The sync plugin keeps a fake transport of its own for its exchange
//! tests; this one is deliberately separate because routing needs the
//! dial/session/black-hole distinctions that sync does not, and the two
//! plugins are authored independently. Fold them together once both are
//! settled.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use inseam_kernel::address::{Address, ContentDigest, ContentLength, Envelope, HostId, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::network::{
    Endpoint, HostRecord, NodeCapabilities, NodeId, NodeRecord, StewardCapabilities,
    StewardshipRecord,
};
use inseam_kernel::store::{IndexStore, StoredFragment, StoredSource};
use inseam_seams::SeamError;
use inseam_seams::connection::{
    Capabilities, Connection, Connections, HostDescription, HostKind, Registration,
};
use inseam_seams::finder::{Discovery, Expansion, Finder, FinderRequest, QueryTrace, RankedSource};
use inseam_seams::node::{Node, SecretKeyBytes};
use inseam_seams::roster::{HostStewards, Invitation, Roster};
use inseam_seams::text::count_lines;
use inseam_seams::transport::{
    Admission, Disposer, InvitationToken, PeerAddress, ProtocolName, RequestHandler,
    SessionDirection, SessionView, Transport,
};

use super::protocol::route_protocol;
use super::serve::RouteHandler;
use super::{Limits, RoutingConfig, RoutingService};

pub(crate) fn node_id(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 32])
}

pub(crate) fn host_id(name: &str) -> HostId {
    HostId::new(name).expect("test host ids are valid")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ----------------------------------------------------------------------
// Node identity
// ----------------------------------------------------------------------

pub(crate) struct FakeNode {
    record: NodeRecord,
}

impl FakeNode {
    pub(crate) fn new(record: NodeRecord) -> Self {
        Self { record }
    }
}

impl Node for FakeNode {
    fn id(&self) -> NodeId {
        self.record.id
    }

    fn display_name(&self) -> String {
        self.record.display_name.clone()
    }

    fn capabilities(&self) -> NodeCapabilities {
        self.record.capabilities
    }

    fn secret_key(&self) -> SecretKeyBytes {
        SecretKeyBytes::from_bytes([0; 32])
    }
}

// ----------------------------------------------------------------------
// Roster
// ----------------------------------------------------------------------

/// The records every node's roster view reads: global facts, as the
/// design has them, shared by every fake roster in one test.
#[derive(Default)]
pub(crate) struct RosterRecords {
    nodes: Mutex<Vec<NodeRecord>>,
    hosts: Mutex<Vec<HostRecord>>,
    stewardships: Mutex<Vec<StewardshipRecord>>,
    expelled: Mutex<Vec<NodeId>>,
}

impl RosterRecords {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn add_node(&self, record: NodeRecord) {
        let mut nodes = lock(&self.nodes);
        nodes.retain(|existing| existing.id != record.id);
        nodes.push(record);
        nodes.sort_by_key(|record| record.id);
    }

    pub(crate) fn add_host(&self, record: HostRecord) {
        let mut hosts = lock(&self.hosts);
        hosts.retain(|existing| existing.id != record.id);
        hosts.push(record);
        hosts.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    }

    pub(crate) fn add_stewardship(&self, record: StewardshipRecord) {
        let mut stewardships = lock(&self.stewardships);
        stewardships
            .retain(|existing| (existing.node, &existing.host) != (record.node, &record.host));
        stewardships.push(record);
        stewardships.sort_by(|a, b| {
            a.host
                .as_str()
                .cmp(b.host.as_str())
                .then(a.node.cmp(&b.node))
        });
    }
}

pub(crate) struct FakeRoster {
    local: NodeRecord,
    records: Arc<RosterRecords>,
}

impl FakeRoster {
    pub(crate) fn new(local: NodeRecord, records: Arc<RosterRecords>) -> Self {
        Self { local, records }
    }
}

#[async_trait::async_trait]
impl Roster for FakeRoster {
    fn local(&self) -> NodeRecord {
        self.local.clone()
    }

    async fn nodes(&self) -> Result<Vec<NodeRecord>, SeamError> {
        let expelled = lock(&self.records.expelled).clone();
        Ok(lock(&self.records.nodes)
            .iter()
            .filter(|record| !expelled.contains(&record.id))
            .cloned()
            .collect())
    }

    async fn node(&self, id: &NodeId) -> Result<Option<NodeRecord>, SeamError> {
        Ok(self
            .nodes()
            .await?
            .into_iter()
            .find(|record| record.id == *id))
    }

    async fn hosts(&self) -> Result<Vec<HostStewards>, SeamError> {
        let stewardships = lock(&self.records.stewardships).clone();
        Ok(lock(&self.records.hosts)
            .iter()
            .map(|host| HostStewards {
                host: host.clone(),
                stewards: stewardships
                    .iter()
                    .filter(|s| s.host == host.id)
                    .cloned()
                    .collect(),
            })
            .collect())
    }

    async fn stewards_of(&self, host: &HostId) -> Result<Vec<StewardshipRecord>, SeamError> {
        Ok(lock(&self.records.stewardships)
            .iter()
            .filter(|s| s.host == *host)
            .cloned()
            .collect())
    }

    async fn is_admitted(&self, id: &NodeId) -> Result<bool, SeamError> {
        Ok(self.node(id).await?.is_some())
    }

    async fn republish(&self) -> Result<NodeRecord, SeamError> {
        Ok(self.local.clone())
    }

    async fn invite(&self) -> Result<Invitation, SeamError> {
        Ok(Invitation {
            node: self.local.id,
            endpoints: Vec::new(),
            token: InvitationToken::new("fake-token").expect("valid"),
            expires: Timestamp(i64::MAX),
        })
    }

    fn redeem(&self, _peer: &NodeId, _token: &InvitationToken) -> bool {
        false
    }

    async fn expel(&self, node: &NodeId) -> Result<(), SeamError> {
        lock(&self.records.expelled).push(*node);
        Ok(())
    }
}

// ----------------------------------------------------------------------
// Connections registry
// ----------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct FakeConnections {
    entries: Mutex<Vec<Arc<Registration>>>,
}

impl Connections for FakeConnections {
    fn register(&self, registration: Registration) -> Result<Disposer, SeamError> {
        lock(&self.entries).push(Arc::new(registration));
        Ok(Box::new(|| {}))
    }

    fn snapshot(&self) -> Vec<Arc<Registration>> {
        let mut out = lock(&self.entries).clone();
        out.sort_by(|a, b| a.host.id.as_str().cmp(b.host.id.as_str()));
        out
    }
}

// ----------------------------------------------------------------------
// Transport over a fake network
// ----------------------------------------------------------------------

/// Who can reach whom. A link is one-way dialability (the roster's
/// endpoints, in effect); a session is live both ways; a black hole never
/// answers, which is how a timeout looks to a requester.
#[derive(Default)]
struct NetworkState {
    handlers: HashMap<(NodeId, ProtocolName), Arc<dyn RequestHandler>>,
    links: HashSet<(NodeId, NodeId)>,
    sessions: HashSet<(NodeId, NodeId)>,
    black_holes: HashSet<NodeId>,
    /// Every request attempted, requester then target.
    requests: Vec<(NodeId, NodeId)>,
}

#[derive(Default)]
pub(crate) struct FakeNetwork {
    state: Mutex<NetworkState>,
}

impl FakeNetwork {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `from` can dial `to`.
    pub(crate) fn link(&self, from: NodeId, to: NodeId) {
        lock(&self.state).links.insert((from, to));
    }

    /// `dialer` opened a session to `accepter`; both use it.
    pub(crate) fn session(&self, dialer: NodeId, accepter: NodeId) {
        lock(&self.state).sessions.insert((dialer, accepter));
    }

    /// Requests to `node` never come back.
    pub(crate) fn black_hole(&self, node: NodeId) {
        lock(&self.state).black_holes.insert(node);
    }

    /// Every session and link is gone: what a network looks like after
    /// everyone rotated.
    pub(crate) fn disconnect_all(&self) {
        let mut state = lock(&self.state);
        state.sessions.clear();
        state.links.clear();
    }

    pub(crate) fn requests_from(&self, node: NodeId) -> u32 {
        let count = lock(&self.state)
            .requests
            .iter()
            .filter(|(from, _)| *from == node)
            .count();
        u32::try_from(count).expect("a test sends few requests")
    }

    pub(crate) fn requests_to(&self, node: NodeId) -> u32 {
        let count = lock(&self.state)
            .requests
            .iter()
            .filter(|(_, to)| *to == node)
            .count();
        u32::try_from(count).expect("a test sends few requests")
    }

    pub(crate) fn transport_for(self: &Arc<Self>, id: NodeId) -> Arc<FakeTransport> {
        Arc::new(FakeTransport {
            id,
            network: Arc::clone(self),
        })
    }

    /// Resolve one attempted request to the handler that serves it, or the
    /// reason it cannot be served. Holds the lock only for the lookup, so
    /// a handler that dials onward never deadlocks on it.
    fn dispatch(
        &self,
        from: NodeId,
        to: NodeId,
        protocol: &ProtocolName,
    ) -> Result<Arc<dyn RequestHandler>, SeamError> {
        let mut state = lock(&self.state);
        state.requests.push((from, to));
        if state.black_holes.contains(&to) {
            return Err(SeamError::failed(format!(
                "request to {} timed out",
                to.short()
            )));
        }
        let reachable = state.links.contains(&(from, to))
            || state.sessions.contains(&(from, to))
            || state.sessions.contains(&(to, from));
        if !reachable {
            return Err(SeamError::failed(format!("cannot dial {}", to.short())));
        }
        state
            .handlers
            .get(&(to, protocol.clone()))
            .cloned()
            .ok_or_else(|| SeamError::failed(format!("{} serves no `{protocol}`", to.short())))
    }
}

pub(crate) struct FakeTransport {
    id: NodeId,
    network: Arc<FakeNetwork>,
}

#[async_trait::async_trait]
impl Transport for FakeTransport {
    fn local_id(&self) -> NodeId {
        self.id
    }

    fn endpoints(&self) -> Vec<Endpoint> {
        Vec::new()
    }

    fn register(
        &self,
        protocol: ProtocolName,
        handler: Arc<dyn RequestHandler>,
    ) -> Result<Disposer, SeamError> {
        let key = (self.id, protocol.clone());
        let mut state = lock(&self.network.state);
        if state.handlers.contains_key(&key) {
            return Err(SeamError::Refused(format!(
                "`{protocol}` is already registered"
            )));
        }
        state.handlers.insert(key.clone(), handler);
        drop(state);
        let network = Arc::clone(&self.network);
        Ok(Box::new(move || {
            lock(&network.state).handlers.remove(&key);
        }))
    }

    fn set_admission(&self, _policy: Arc<dyn Admission>) -> Result<Disposer, SeamError> {
        Ok(Box::new(|| {}))
    }

    async fn request(
        &self,
        to: &PeerAddress,
        protocol: &ProtocolName,
        body: Vec<u8>,
        _timeout: Duration,
    ) -> Result<Vec<u8>, SeamError> {
        let handler = self.network.dispatch(self.id, to.id, protocol)?;
        handler.handle(self.id, body).await
    }

    fn sessions(&self) -> Vec<SessionView> {
        lock(&self.network.state)
            .sessions
            .iter()
            .filter_map(|(dialer, accepter)| {
                if *dialer == self.id {
                    Some((*accepter, SessionDirection::Outbound))
                } else if *accepter == self.id {
                    Some((*dialer, SessionDirection::Inbound))
                } else {
                    None
                }
            })
            .map(|(peer, direction)| SessionView {
                peer,
                direction,
                since: Timestamp(0),
                last_used: Timestamp(0),
            })
            .collect()
    }

    fn disconnect(&self, peer: &NodeId) {
        lock(&self.network.state)
            .sessions
            .retain(|(a, b)| !((*a == self.id && b == peer) || (*b == self.id && a == peer)));
    }
}

// ----------------------------------------------------------------------
// Finder with canned answers
// ----------------------------------------------------------------------

/// A finder with canned answers, settable after the node it serves has
/// opened its store, so a test can catalog a row there and then rank it.
pub(crate) struct StubFinder {
    ranked: Mutex<Vec<RankedSource>>,
    fragments: Mutex<Vec<StoredFragment>>,
    pub(crate) queries: AtomicU32,
    pub(crate) expansions: AtomicU32,
}

impl StubFinder {
    pub(crate) fn empty() -> Self {
        Self::ranked(Vec::new())
    }

    pub(crate) fn ranked(ranked: Vec<RankedSource>) -> Self {
        Self {
            ranked: Mutex::new(ranked),
            fragments: Mutex::new(Vec::new()),
            queries: AtomicU32::new(0),
            expansions: AtomicU32::new(0),
        }
    }

    pub(crate) fn set_ranked(&self, ranked: Vec<RankedSource>) {
        *lock(&self.ranked) = ranked;
    }

    pub(crate) fn set_fragments(&self, fragments: Vec<StoredFragment>) {
        *lock(&self.fragments) = fragments;
    }
}

#[async_trait::async_trait]
impl Finder for StubFinder {
    async fn discover(&self, request: &FinderRequest) -> Result<Discovery, SeamError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok(Discovery {
            ranked: lock(&self.ranked)
                .iter()
                .take(request.limit)
                .cloned()
                .collect(),
            trace: QueryTrace::default(),
        })
    }

    async fn expand(&self, source: &StoredSource) -> Result<Expansion, SeamError> {
        self.expansions.fetch_add(1, Ordering::SeqCst);
        Ok(Expansion {
            fragments: lock(&self.fragments)
                .iter()
                .filter(|f| f.source == Some(source.id))
                .cloned()
                .collect(),
            relations: Vec::new(),
            neighbors: Vec::new(),
        })
    }
}

// ----------------------------------------------------------------------
// Cataloged sources
// ----------------------------------------------------------------------

/// A source as a test catalogs it: an address, an envelope, and the raw
/// size; `catalog` writes it and reads back the row a finder would rank.
pub(crate) struct TestSource {
    address: Address,
    envelope: Envelope,
    raw_bytes: u64,
}

impl TestSource {
    /// A `text/plain` source whose envelope records the text's line count.
    pub(crate) fn text(address: &str, text: &str) -> Self {
        let address: Address = address.parse().expect("test addresses are valid");
        Self::at(
            address,
            Mimetype::text_plain(),
            ContentLength::Lines(count_lines(text)),
            text.len(),
        )
    }

    pub(crate) fn at(
        address: Address,
        content_type: Mimetype,
        length: ContentLength,
        raw_bytes: usize,
    ) -> Self {
        Self {
            address,
            envelope: Envelope {
                source_type: "file".to_string(),
                content_type,
                length,
                created: None,
                modified: None,
                observed: Timestamp(0),
                properties: Vec::new(),
                facets: Vec::new(),
                hint: None,
                content_digest: None,
            },
            raw_bytes: u64::try_from(raw_bytes).expect("test content is small"),
        }
    }

    pub(crate) fn with_digest(mut self, digest: Option<ContentDigest>) -> Self {
        self.envelope.content_digest = digest;
        self
    }

    pub(crate) async fn catalog(&self, store: &IndexStore) -> RankedSource {
        store
            .upsert_source(&self.address, &self.envelope, self.raw_bytes)
            .await
            .expect("catalogs");
        let source = store
            .source_by_address(&self.address)
            .await
            .expect("reads")
            .expect("just cataloged");
        RankedSource {
            source,
            score: 1.0,
            summary: None,
            hints: Vec::new(),
            replicas: Vec::new(),
        }
    }
}

// ----------------------------------------------------------------------
// A whole node
// ----------------------------------------------------------------------

/// What a test node advertises.
pub(crate) struct NodeSpec {
    pub byte: u8,
    pub relays: bool,
    pub deep_index: bool,
    pub always_on: bool,
}

impl NodeSpec {
    pub(crate) fn new(byte: u8) -> Self {
        Self {
            byte,
            relays: true,
            deep_index: true,
            always_on: false,
        }
    }

    pub(crate) fn record(&self) -> NodeRecord {
        NodeRecord {
            id: node_id(self.byte),
            display_name: format!("node-{}", self.byte),
            endpoints: Vec::new(),
            capabilities: NodeCapabilities {
                always_on: self.always_on,
                deep_index: self.deep_index,
                relays: self.relays,
            },
        }
    }
}

/// One node of a test network: its store, its routing service serving
/// the protocol on the fake transport, and the fakes behind it.
pub(crate) struct TestNode {
    pub id: NodeId,
    pub store: Arc<IndexStore>,
    pub service: Arc<RoutingService>,
    pub finder: Arc<StubFinder>,
    pub transport: Arc<FakeTransport>,
    pub connections: Arc<FakeConnections>,
    records: Arc<RosterRecords>,
    _dir: tempfile::TempDir,
}

impl TestNode {
    pub(crate) async fn build(
        spec: NodeSpec,
        records: &Arc<RosterRecords>,
        network: &Arc<FakeNetwork>,
        finder: StubFinder,
    ) -> Self {
        Self::build_with(spec, records, network, finder, &RoutingConfig::default()).await
    }

    pub(crate) async fn build_with(
        spec: NodeSpec,
        records: &Arc<RosterRecords>,
        network: &Arc<FakeNetwork>,
        finder: StubFinder,
        config: &RoutingConfig,
    ) -> Self {
        let record = spec.record();
        records.add_node(record.clone());
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(IndexStore::open(dir.path()).await.expect("opens"));
        let finder = Arc::new(finder);
        let transport = network.transport_for(record.id);
        let connections = Arc::new(FakeConnections::default());
        let service = RoutingService::new(
            Arc::clone(&store),
            Arc::new(FakeNode::new(record.clone())),
            transport.clone() as Arc<dyn Transport>,
            Arc::new(FakeRoster::new(record.clone(), Arc::clone(records))),
            connections.clone() as Arc<dyn Connections>,
            finder.clone() as Arc<dyn Finder>,
            Limits::from_config(config).expect("test config is valid"),
        );
        // The disposer is dropped on purpose: the fake network lives as
        // long as the test, and nothing unmounts a test node.
        let _disposer = transport
            .register(
                route_protocol(),
                RouteHandler::serving(Arc::clone(&service)),
            )
            .expect("registers once");
        Self {
            id: record.id,
            store,
            service,
            finder,
            transport,
            connections,
            records: Arc::clone(records),
            _dir: dir,
        }
    }

    /// Steward `host` through `connection`: the registry learns it and
    /// the roster records publish it, as the real plugins would.
    pub(crate) fn steward(&self, host: &HostId, connection: Arc<dyn Connection>) {
        // The disposer is dropped on purpose: nothing unmounts a test node.
        let _disposer = self
            .connections
            .register(Registration {
                entry_id: format!("fs-{}", self.id.short()),
                host: HostDescription {
                    id: host.clone(),
                    kind: HostKind::filesystem(),
                    display_name: host.to_string(),
                },
                capabilities: Capabilities::READ_ONLY,
                roots: Vec::new(),
                connection,
            })
            .expect("registers");
        self.claim(host);
    }

    /// Publish a stewardship claim without a connection — how a stale or
    /// remote claim looks to this node.
    pub(crate) fn claim(&self, host: &HostId) {
        self.records.add_host(HostRecord {
            id: host.clone(),
            kind: "fs".to_string(),
            display_name: host.to_string(),
        });
        self.records.add_stewardship(StewardshipRecord {
            node: self.id,
            host: host.clone(),
            capabilities: StewardCapabilities {
                enumerates: true,
                change_feed: false,
                writable: false,
            },
            roots: Vec::new(),
        });
    }
}
