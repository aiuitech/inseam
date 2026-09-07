//! The `transport-iroh` plugin: the node↔node transport
//! (`design/connections.md`), the `transport` seam's flagship provider.
//! iroh gives QUIC dialed by the peer's public key, hole punching, and an
//! encrypted relay fallback; this plugin gives the seam its shape on top:
//! one ALPN, one request per bi-stream routed by protocol name, an
//! admission handshake on every new connection, and a session table that
//! is local knowledge only.
//!
//! **Admission.** The first stream a dialer opens is `inseam/hello/1`,
//! carrying its invitation token or nothing. The acceptor asks the
//! installed [`Admission`] policy — the roster's — and with none installed
//! refuses everyone, so a node whose roster has not come up is closed, not
//! open. A refused peer learns only that it was refused; the connection is
//! closed with an application code and never enters the table. The dialer
//! needs no such check: iroh authenticates the dialed key in the TLS
//! handshake, so the peer on the other end is the one the roster named.
//!
//! **Sessions.** QUIC is symmetric, so an accepted inbound connection
//! serves this node's outbound requests too — which is how a dialable
//! backbone reaches an outbound-only laptop: the laptop keeps a standing
//! connection open (a keep-alive at a third of the idle timeout keeps NAT
//! bindings warm) and the backbone opens streams on it. A dead session is
//! evicted and the request fails; the caller retries, so there is no
//! retry storm inside the transport.
//!
//! **Relays.** `relay = "n0"` uses iroh's public relays so NAT traversal
//! works out of the box; `"none"` is direct-only; an `https://` URL is the
//! network's own relay on its backbone node (`design/roster.md`). No
//! address-lookup service is ever configured: the roster is where a peer's
//! endpoints come from, and nothing about this node is published outside
//! the network.

mod endpoints;
mod sessions;
mod wire;

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use iroh::endpoint::{
    presets, BindOpts, Builder, Connection, ConnectionError, IdleTimeout, Incoming,
    InvalidSocketAddr, PortmapperConfig, QuicTransportConfig, RecvStream, SendStream, VarInt,
};
use iroh::{Endpoint as IrohEndpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::{Endpoint, NodeId};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::node::{SecretKeyBytes, NODE};
use inseam_seams::transport::{
    Admission, Admit, Disposer, InvitationToken, PeerAddress, ProtocolName, RequestHandler,
    SessionDirection, SessionView, Transport, TRANSPORT,
};
use inseam_seams::SeamError;

use sessions::{Session, SessionTable};
use wire::{Hello, WireError, HELLO_PROTOCOL};

pub use wire::ALPN;

pub const PLUGIN_NAME: &str = "transport-iroh";
/// `relay = "n0"`: iroh's public relays.
pub const RELAY_N0: &str = "n0";
/// `relay = "none"`: direct connections only.
pub const RELAY_NONE: &str = "none";
/// Most streams one connection serves at once; a peer opening a 65th waits
/// in QUIC flow control, and the same figure is the QUIC stream limit.
pub const STREAMS_IN_FLIGHT_MAX: u32 = 64;
/// Most admission handshakes in progress at once; beyond it, incoming
/// connections are refused at the door rather than queued.
pub const HANDSHAKES_IN_FLIGHT_MAX: usize = 64;
/// Most protocol handlers one node registers; a node speaks a handful.
pub const HANDLERS_MAX: usize = 64;
/// Least idle timeout: the keep-alive is a third of it and must be at
/// least a second.
pub const IDLE_TIMEOUT_SECS_MIN: u32 = 3;
/// How long a refusal is given to reach the peer before the connection is
/// closed under it.
const REFUSAL_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Application close codes, so a peer can tell why a connection ended.
const CLOSE_REFUSED: VarInt = VarInt::from_u32(1);
const CLOSE_PROTOCOL: VarInt = VarInt::from_u32(2);
const CLOSE_DISCONNECTED: VarInt = VarInt::from_u32(3);
const CLOSE_SUPERSEDED: VarInt = VarInt::from_u32(4);
const CLOSE_SHUTDOWN: VarInt = VarInt::from_u32(5);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransportConfig {
    /// `n0` for iroh's public relays, `none` for direct-only, or the
    /// `https://` URL of the network's own relay.
    pub relay: String,
    /// UDP port to listen on; `0` takes an ephemeral one. A backbone node
    /// with a forwarded port names it here.
    pub bind_port: u16,
    /// How long the transport's own exchanges may take: the admission
    /// handshake, and serving one inbound stream end to end.
    pub request_timeout_secs: u32,
    /// QUIC idle timeout; a keep-alive at a third of it keeps a standing
    /// connection open through NATs.
    pub idle_timeout_secs: u32,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            relay: RELAY_N0.to_string(),
            bind_port: 0,
            request_timeout_secs: 30,
            idle_timeout_secs: 120,
        }
    }
}

/// The relay choice, parsed from `relay`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaySetting {
    N0,
    None,
    /// The network's own relay.
    Own(RelayUrl),
}

impl RelaySetting {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim() {
            RELAY_N0 => Ok(Self::N0),
            RELAY_NONE => Ok(Self::None),
            url if url.starts_with("https://") => url
                .parse::<RelayUrl>()
                .map(Self::Own)
                .map_err(|e| format!("relay `{url}`: {e}")),
            other => Err(format!(
                "relay must be `{RELAY_N0}`, `{RELAY_NONE}`, or an https:// relay URL, not `{other}`"
            )),
        }
    }

    fn mode(&self) -> RelayMode {
        match self {
            Self::N0 => RelayMode::Default,
            Self::None => RelayMode::Disabled,
            Self::Own(url) => RelayMode::custom([url.clone()]),
        }
    }
}

impl fmt::Display for RelaySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::N0 => f.write_str(RELAY_N0),
            Self::None => f.write_str(RELAY_NONE),
            Self::Own(url) => write!(f, "{url}"),
        }
    }
}

/// Which sockets the endpoint binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindTarget {
    /// Every interface, IPv4 and IPv6, on the port (`0` for ephemeral).
    AllInterfaces { port: u16 },
    /// IPv4 loopback on an ephemeral port, with no router probing: two
    /// endpoints in one test process.
    Loopback,
}

/// The config after parse-don't-validate: what [`IrohTransport::bind`]
/// takes.
#[derive(Debug, Clone)]
pub struct Settings {
    pub relay: RelaySetting,
    pub bind: BindTarget,
    pub request_timeout: Duration,
    pub idle_timeout: Duration,
}

impl TryFrom<&TransportConfig> for Settings {
    type Error = String;

    fn try_from(config: &TransportConfig) -> Result<Self, String> {
        let relay = RelaySetting::parse(&config.relay)?;
        if config.request_timeout_secs == 0 {
            return Err("request_timeout_secs must be at least 1".to_string());
        }
        if config.idle_timeout_secs < IDLE_TIMEOUT_SECS_MIN {
            return Err(format!(
                "idle_timeout_secs must be at least {IDLE_TIMEOUT_SECS_MIN}, so the keep-alive at a \
                 third of it is at least a second"
            ));
        }
        Ok(Self {
            relay,
            bind: BindTarget::AllInterfaces {
                port: config.bind_port,
            },
            request_timeout: Duration::from_secs(u64::from(config.request_timeout_secs)),
            idle_timeout: Duration::from_secs(u64::from(config.idle_timeout_secs)),
        })
    }
}

pub struct IrohTransportPlugin {
    settings: Settings,
}

pub struct IrohTransportFactory;

impl PluginFactory for IrohTransportFactory {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        let config: TransportConfig = parse_config(config)?;
        let settings = Settings::try_from(&config).map_err(|e| PluginError(format!("config: {e}")))?;
        Ok(Box::new(IrohTransportPlugin { settings }))
    }
}

#[async_trait::async_trait]
impl Plugin for IrohTransportPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("node")];
        Manifest {
            name: PLUGIN_NAME,
            inject: INJECT,
            provides: &["transport"],
        }
    }

    /// Binds the sockets and returns: relay and network readiness are
    /// learned in the background, never awaited at boot.
    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let node = cx.get(&NODE)?;
        let transport = IrohTransport::bind(node.secret_key(), &self.settings)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        // The node plugin derived its id through the same key type this
        // endpoint authenticates with; the two must agree or every peer
        // would refuse us as an impostor of ourselves.
        assert_eq!(transport.local_id(), node.id(), "the endpoint's key is the node's key");
        tracing::info!(
            id = %transport.local_id().short(),
            relay = %self.settings.relay,
            "iroh transport bound"
        );
        let facts = Facts::new()
            .with("relay", self.settings.relay.to_string())
            .with("id", transport.local_id().to_hex());
        let shutdown = transport.clone();
        cx.effect("close the iroh endpoint", move || shutdown.shutdown_in_background());
        cx.provide(&TRANSPORT, Arc::new(transport) as Arc<dyn Transport>, facts)
    }
}

/// The seam provider: the bound endpoint, its accept loop, the handler
/// registry, the admission policy, and the session table.
#[derive(Clone)]
pub struct IrohTransport {
    inner: Arc<Shared>,
}

struct Shared {
    endpoint: IrohEndpoint,
    local_id: NodeId,
    request_timeout: Duration,
    handlers: RwLock<HashMap<ProtocolName, Arc<dyn RequestHandler>>>,
    admission: RwLock<Option<Arc<dyn Admission>>>,
    sessions: Mutex<SessionTable>,
    handshakes: Arc<Semaphore>,
    accept_loop: Mutex<Option<JoinHandle<()>>>,
}

impl IrohTransport {
    /// Bind the endpoint for `secret` and start accepting. Returns once the
    /// sockets are bound; the relay, if any, is contacted in the background.
    pub async fn bind(secret: SecretKeyBytes, settings: &Settings) -> Result<Self, SeamError> {
        let endpoint = bind_endpoint(secret, settings).await?;
        let local_id = node_id_of(endpoint.id());
        let inner = Arc::new(Shared {
            endpoint,
            local_id,
            request_timeout: settings.request_timeout,
            handlers: RwLock::new(HashMap::new()),
            admission: RwLock::new(None),
            sessions: Mutex::new(SessionTable::default()),
            handshakes: Arc::new(Semaphore::new(HANDSHAKES_IN_FLIGHT_MAX)),
            accept_loop: Mutex::new(None),
        });
        let handle = tokio::spawn(accept_loop(Arc::clone(&inner)));
        *inner.accept_loop.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(Self { inner })
    }

    /// Stop accepting, close every session, and close the endpoint,
    /// waiting for the closes to be delivered.
    pub async fn close(&self) {
        self.inner.stop_accepting();
        self.inner.close_all_sessions(CLOSE_SHUTDOWN, b"shutdown");
        self.inner.endpoint.close().await;
    }

    /// The effect's undo: synchronous, so the endpoint's graceful close is
    /// handed to the runtime when there is one and left to the socket drop
    /// otherwise.
    fn shutdown_in_background(self) {
        self.inner.stop_accepting();
        self.inner.close_all_sessions(CLOSE_SHUTDOWN, b"shutdown");
        let endpoint = self.inner.endpoint.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move { endpoint.close().await });
            }
            Err(_) => drop(endpoint),
        }
    }
}

impl Shared {
    fn admit(&self, peer: &NodeId, invitation: Option<&InvitationToken>) -> Admit {
        let policy = self
            .admission
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match policy {
            Some(policy) => policy.admit(peer, invitation),
            None => Admit::Refused(
                "no admission policy is installed; the transport refuses everyone until the roster installs one"
                    .to_string(),
            ),
        }
    }

    /// Serve one request on a stream: the registered handler's answer, or
    /// an error the peer can read. A handler's answer is bounded like every
    /// message; an oversized one is an error to the peer, not a broken
    /// stream.
    async fn dispatch(
        &self,
        peer: NodeId,
        protocol: &ProtocolName,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        let handler = self
            .handlers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(protocol)
            .cloned();
        let Some(handler) = handler else {
            return Err(format!(
                "node {} serves no protocol `{protocol}`",
                self.local_id.short()
            ));
        };
        let response = handler.handle(peer, body).await.map_err(|e| e.to_string())?;
        wire::check_body_bound(response.len())
            .map_err(|e| format!("the handler for `{protocol}` answered with too much: {e}"))?;
        Ok(response)
    }

    /// Record a live connection as the session with `peer` and start
    /// serving its streams, whichever side opened it.
    fn install_session(self: &Arc<Self>, peer: NodeId, connection: Connection, direction: SessionDirection) {
        let now = now();
        let session = Session {
            connection: connection.clone(),
            direction,
            since: now,
            last_used: now,
        };
        let displaced = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.local_id, peer, session);
        for old in displaced {
            old.close(CLOSE_SUPERSEDED, b"superseded");
        }
        tokio::spawn(serve_connection(Arc::clone(self), peer, connection));
    }

    fn touch(&self, peer: &NodeId) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .touch(peer, now());
    }

    fn evict(&self, peer: &NodeId, stable_id: usize) {
        let removed = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove_if_same(peer, stable_id);
        if removed.is_some() {
            tracing::info!(peer = %peer.short(), "evicted a closed session");
        }
    }

    fn stop_accepting(&self) {
        if let Some(handle) = self
            .accept_loop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
        }
    }

    fn close_all_sessions(&self, code: VarInt, reason: &[u8]) {
        let peers: Vec<NodeId> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .views()
            .into_iter()
            .map(|view| view.peer)
            .collect();
        for peer in peers {
            let removed = self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&peer);
            if let Some(connection) = removed {
                connection.close(code, reason);
            }
        }
    }
}

async fn bind_endpoint(secret: SecretKeyBytes, settings: &Settings) -> Result<IrohEndpoint, SeamError> {
    let quic = quic_transport_config(settings.idle_timeout)?;
    // `Minimal` sets only the crypto provider: no relays and no address
    // lookup come with it, so what follows is the whole network policy.
    let builder = IrohEndpoint::builder(presets::Minimal)
        .secret_key(iroh::SecretKey::from_bytes(secret.as_bytes()))
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(settings.relay.mode())
        .transport_config(quic);
    let builder = bind_target(builder, settings.bind)?;
    builder
        .bind()
        .await
        .map_err(|e| SeamError::failed(format!("cannot bind the iroh endpoint: {e}")))
}

fn bind_target(builder: Builder, bind: BindTarget) -> Result<Builder, SeamError> {
    let invalid = |e: InvalidSocketAddr| SeamError::failed(format!("bind address: {e}"));
    match bind {
        BindTarget::AllInterfaces { port: 0 } => Ok(builder),
        BindTarget::AllInterfaces { port } => {
            // IPv6 is welcome but not required: a host without it still
            // binds its IPv4 port.
            let v6 = BindOpts::default().set_is_required(false);
            builder
                .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
                .map_err(invalid)?
                .bind_addr_with_opts(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)), v6)
                .map_err(invalid)
        }
        BindTarget::Loopback => builder
            .clear_ip_transports()
            .portmapper_config(PortmapperConfig::Disabled)
            .bind_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(invalid),
    }
}

/// The QUIC parameters: the idle timeout, a keep-alive at a third of it
/// (two lost keep-alives still leave the connection alive, and NAT
/// bindings that expire in tens of seconds stay warm — that standing
/// connection is how the backbone reaches an outbound-only node), and the
/// stream ceiling. This transport opens bi-streams only, so a peer may
/// open no uni-streams at all.
fn quic_transport_config(idle_timeout: Duration) -> Result<QuicTransportConfig, SeamError> {
    let max_idle: IdleTimeout = idle_timeout
        .try_into()
        .map_err(|e| SeamError::failed(format!("idle timeout {idle_timeout:?} is not representable: {e}")))?;
    let keep_alive = idle_timeout / 3;
    assert!(keep_alive >= Duration::from_secs(1), "the settings bound the idle timeout below");
    assert!(keep_alive < idle_timeout);
    Ok(QuicTransportConfig::builder()
        .max_idle_timeout(Some(max_idle))
        .keep_alive_interval(keep_alive)
        .max_concurrent_bidi_streams(VarInt::from_u32(STREAMS_IN_FLIGHT_MAX))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .build())
}

/// Accept connections for the endpoint's life: `accept` yields `None` once
/// the endpoint closes, which is this loop's only exit.
async fn accept_loop(shared: Arc<Shared>) {
    loop {
        let Some(incoming) = shared.endpoint.accept().await else {
            break;
        };
        match Arc::clone(&shared.handshakes).try_acquire_owned() {
            Ok(permit) => {
                tokio::spawn(admit_incoming(Arc::clone(&shared), incoming, permit));
            }
            Err(_exhausted) => {
                tracing::warn!(
                    "{HANDSHAKES_IN_FLIGHT_MAX} admission handshakes in flight; refusing a connection"
                );
                incoming.refuse();
            }
        }
    }
    tracing::debug!("accept loop ended: endpoint closed");
}

/// Finish the QUIC handshake, then run the admission handshake under the
/// request timeout; only an admitted connection becomes a session.
async fn admit_incoming(shared: Arc<Shared>, incoming: Incoming, _permit: OwnedSemaphorePermit) {
    let connection = match complete_incoming(incoming).await {
        Ok(connection) => connection,
        Err(reason) => {
            tracing::debug!("incoming connection did not complete: {reason}");
            return;
        }
    };
    let peer = node_id_of(connection.remote_id());
    let verdict = tokio::time::timeout(
        shared.request_timeout,
        admission_handshake(&shared, &connection, peer),
    )
    .await;
    match verdict {
        Ok(Ok(())) => shared.install_session(peer, connection, SessionDirection::Inbound),
        Ok(Err(HandshakeFailure::Refused)) => connection.close(CLOSE_REFUSED, b"refused"),
        Ok(Err(HandshakeFailure::Protocol(reason))) => {
            tracing::warn!(peer = %peer.short(), "closing: {reason}");
            connection.close(CLOSE_PROTOCOL, b"expected hello");
        }
        Err(_elapsed) => {
            tracing::warn!(peer = %peer.short(), "closing: no hello within {:?}", shared.request_timeout);
            connection.close(CLOSE_PROTOCOL, b"hello timed out");
        }
    }
}

async fn complete_incoming(incoming: Incoming) -> Result<Connection, String> {
    let accepting = incoming.accept().map_err(|e| e.to_string())?;
    accepting.await.map_err(|e| e.to_string())
}

enum HandshakeFailure {
    Refused,
    Protocol(String),
}

/// The acceptor's half of the hello: the first stream must speak it, and
/// the policy decides. The peer learns only that it was refused, never
/// why; the refusal is given a bounded moment to be acknowledged so it
/// arrives before the close that follows it.
async fn admission_handshake(
    shared: &Shared,
    connection: &Connection,
    peer: NodeId,
) -> Result<(), HandshakeFailure> {
    let protocol_failure = |e: String| HandshakeFailure::Protocol(e);
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| protocol_failure(e.to_string()))?;
    let (protocol, body) = wire::read_request(&mut recv)
        .await
        .map_err(|e| protocol_failure(e.to_string()))?;
    if protocol.as_str() != HELLO_PROTOCOL {
        return Err(protocol_failure(format!(
            "first stream spoke `{protocol}`, not `{HELLO_PROTOCOL}`"
        )));
    }
    let hello: Hello =
        serde_json::from_slice(&body).map_err(|e| protocol_failure(format!("hello body: {e}")))?;
    match shared.admit(&peer, hello.invitation.as_ref()) {
        Admit::Admitted => {
            wire::write_response(&mut send, &Ok(Vec::new()))
                .await
                .map_err(|e| protocol_failure(e.to_string()))?;
            tracing::info!(peer = %peer.short(), "admitted");
            Ok(())
        }
        Admit::Refused(reason) => {
            tracing::warn!(peer = %peer.short(), "refused: {reason}");
            let written = wire::write_response(&mut send, &Err("refused".to_string())).await;
            if written.is_ok() {
                let _acknowledged = tokio::time::timeout(REFUSAL_FLUSH_TIMEOUT, send.stopped()).await;
            }
            Err(HandshakeFailure::Refused)
        }
    }
}

/// Serve every stream the peer opens on this connection, each in its own
/// task so a handler's panic takes down one stream and not the session.
/// Runs for the connection's life: `accept_bi` fails once it closes, which
/// is this loop's only exit.
async fn serve_connection(shared: Arc<Shared>, peer: NodeId, connection: Connection) {
    let limiter = Arc::new(Semaphore::new(
        usize::try_from(STREAMS_IN_FLIGHT_MAX).expect("a u32 fits a usize"),
    ));
    let stable_id = connection.stable_id();
    loop {
        // The limiter is never closed, so acquiring cannot fail; it only
        // waits while the peer has the full complement of streams in flight.
        let permit = Arc::clone(&limiter)
            .acquire_owned()
            .await
            .expect("the stream limiter is never closed");
        let (send, recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(reason) => {
                tracing::debug!(peer = %peer.short(), "connection ended: {reason}");
                break;
            }
        };
        shared.touch(&peer);
        tokio::spawn(serve_stream(Arc::clone(&shared), peer, send, recv, permit));
    }
    let removed = shared
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove_if_same(&peer, stable_id);
    if removed.is_some() {
        tracing::info!(peer = %peer.short(), "session closed");
    }
}

/// One inbound stream end to end, under the request timeout: read the
/// request, dispatch it, write the answer.
async fn serve_stream(
    shared: Arc<Shared>,
    peer: NodeId,
    mut send: SendStream,
    mut recv: RecvStream,
    _permit: OwnedSemaphorePermit,
) {
    let served = tokio::time::timeout(shared.request_timeout, async {
        let (protocol, body) = wire::read_request(&mut recv).await?;
        let outcome = shared.dispatch(peer, &protocol, body).await;
        wire::write_response(&mut send, &outcome).await
    })
    .await;
    match served {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(peer = %peer.short(), "stream not served: {e}"),
        Err(_elapsed) => {
            tracing::warn!(
                peer = %peer.short(),
                "stream abandoned after {:?}",
                shared.request_timeout
            );
            let _ = send.reset(CLOSE_PROTOCOL);
        }
    }
}

/// The session with `to` if it is live, else a fresh dial. A session found
/// closed is evicted and the request fails — the caller retries, and the
/// retry dials.
async fn session_or_dial(shared: &Arc<Shared>, to: &PeerAddress) -> Result<Connection, SeamError> {
    let live = shared
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&to.id);
    let Some(connection) = live else {
        return dial(shared, to).await;
    };
    match connection.close_reason() {
        None => Ok(connection),
        Some(reason) => {
            shared.evict(&to.id, connection.stable_id());
            Err(SeamError::Unavailable(format!(
                "the session with node {} closed ({reason}); retry to redial",
                to.id.short()
            )))
        }
    }
}

/// Dial `to` from its roster endpoints and run the hello. A peer with no
/// readable endpoint cannot be dialed: there is no address lookup to fall
/// back on, by design, so this fails loudly.
async fn dial(shared: &Arc<Shared>, to: &PeerAddress) -> Result<Connection, SeamError> {
    if to.id == shared.local_id {
        return Err(SeamError::Refused("a node does not dial itself".to_string()));
    }
    let addrs = endpoints::parse(&to.endpoints);
    if addrs.is_empty() {
        return Err(SeamError::Unavailable(format!(
            "node {} has no dialable endpoint and no open session; an outbound-only node is reached \
             only through a session it opens",
            to.id.short()
        )));
    }
    let remote = public_key_of(&to.id)?;
    let connection = shared
        .endpoint
        .connect(EndpointAddr::from_parts(remote, addrs), ALPN)
        .await
        .map_err(|e| {
            SeamError::Unavailable(format!("cannot connect to node {}: {e}", to.id.short()))
        })?;
    // iroh authenticates the dialed key in the TLS handshake; a connection
    // to anyone else cannot exist.
    assert_eq!(node_id_of(connection.remote_id()), to.id, "iroh connects only to the dialed key");
    hello(&connection, to).await?;
    shared.install_session(to.id, connection.clone(), SessionDirection::Outbound);
    Ok(connection)
}

/// The dialer's half of the hello. A refusal — answered or delivered as
/// the close code — is [`SeamError::NotAdmitted`]; the connection is
/// closed either way and never becomes a session.
async fn hello(connection: &Connection, to: &PeerAddress) -> Result<(), SeamError> {
    let body = serde_json::to_vec(&Hello {
        invitation: to.invitation.clone(),
    })
    .map_err(|e| SeamError::failed(format!("render hello: {e}")))?;
    match exchange_on(connection, &wire::hello_protocol(), body).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_refusal)) => {
            connection.close(CLOSE_REFUSED, b"refused");
            Err(SeamError::NotAdmitted(to.id))
        }
        Err(e) => {
            let refused = was_refused(connection);
            connection.close(CLOSE_PROTOCOL, b"hello failed");
            if refused {
                Err(SeamError::NotAdmitted(to.id))
            } else {
                Err(SeamError::Unavailable(format!(
                    "hello to node {} failed: {e}",
                    to.id.short()
                )))
            }
        }
    }
}

/// One request/response on an open connection: a stream, two frames.
async fn exchange_on(
    connection: &Connection,
    protocol: &ProtocolName,
    body: Vec<u8>,
) -> Result<Result<Vec<u8>, String>, WireError> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| WireError::Stream(e.to_string()))?;
    wire::write_request(&mut send, protocol, &body).await?;
    wire::read_response(&mut recv).await
}

fn was_refused(connection: &Connection) -> bool {
    matches!(
        connection.close_reason(),
        Some(ConnectionError::ApplicationClosed(close)) if close.error_code == CLOSE_REFUSED
    )
}

#[async_trait::async_trait]
impl Transport for IrohTransport {
    fn local_id(&self) -> NodeId {
        self.inner.local_id
    }

    fn endpoints(&self) -> Vec<Endpoint> {
        endpoints::render(&self.inner.endpoint.addr())
    }

    fn register(
        &self,
        protocol: ProtocolName,
        handler: Arc<dyn RequestHandler>,
    ) -> Result<Disposer, SeamError> {
        if protocol.as_str() == HELLO_PROTOCOL {
            return Err(SeamError::Refused(format!(
                "`{HELLO_PROTOCOL}` is the transport's own admission handshake"
            )));
        }
        {
            let mut handlers = self.inner.handlers.write().unwrap_or_else(|e| e.into_inner());
            if handlers.contains_key(&protocol) {
                return Err(SeamError::Refused(format!(
                    "protocol `{protocol}` already has a handler"
                )));
            }
            if handlers.len() >= HANDLERS_MAX {
                return Err(SeamError::Refused(format!(
                    "{HANDLERS_MAX} protocols are registered already"
                )));
            }
            handlers.insert(protocol.clone(), handler);
        }
        // The disposer holds the transport weakly: a handler unwound after
        // the whole transport is gone (full teardown) is a no-op.
        let weak = Arc::downgrade(&self.inner);
        Ok(Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner
                    .handlers
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&protocol);
            }
        }))
    }

    fn set_admission(&self, policy: Arc<dyn Admission>) -> Result<Disposer, SeamError> {
        {
            let mut admission = self.inner.admission.write().unwrap_or_else(|e| e.into_inner());
            if admission.is_some() {
                return Err(SeamError::Refused(
                    "an admission policy is already installed; dispose it first".to_string(),
                ));
            }
            *admission = Some(policy);
        }
        let weak = Arc::downgrade(&self.inner);
        Ok(Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                *inner.admission.write().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }))
    }

    async fn request(
        &self,
        to: &PeerAddress,
        protocol: &ProtocolName,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, SeamError> {
        // Bounds first, before a single packet: the body, the protocol, the
        // deadline.
        wire::check_body_bound(body.len()).map_err(|e| SeamError::Refused(e.to_string()))?;
        if protocol.as_str() == HELLO_PROTOCOL {
            return Err(SeamError::Refused(format!(
                "`{HELLO_PROTOCOL}` is spoken by the transport itself, never requested"
            )));
        }
        if timeout.is_zero() {
            return Err(SeamError::Refused("a zero timeout would refuse every request".to_string()));
        }
        let exchange = async {
            let connection = session_or_dial(&self.inner, to).await?;
            let answer = exchange_on(&connection, protocol, body)
                .await
                .map_err(|e| self.inner.explain_stream_failure(&connection, to, e))?;
            self.inner.touch(&to.id);
            answer.map_err(|message| {
                SeamError::failed(format!(
                    "node {} answered `{protocol}` with an error: {message}",
                    to.id.short()
                ))
            })
        };
        tokio::time::timeout(timeout, exchange).await.map_err(|_elapsed| {
            SeamError::Unavailable(format!(
                "request `{protocol}` to node {} timed out after {timeout:?}",
                to.id.short()
            ))
        })?
    }

    fn sessions(&self) -> Vec<SessionView> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .views()
    }

    fn disconnect(&self, peer: &NodeId) {
        let removed = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer);
        if let Some(connection) = removed {
            connection.close(CLOSE_DISCONNECTED, b"disconnected");
            tracing::info!(peer = %peer.short(), "disconnected");
        }
    }
}

impl Shared {
    /// A stream failure mid-request is either the session dying under it —
    /// evicted, and the caller retries — or a protocol fault on a live one.
    fn explain_stream_failure(&self, connection: &Connection, to: &PeerAddress, e: WireError) -> SeamError {
        match connection.close_reason() {
            Some(reason) => {
                self.evict(&to.id, connection.stable_id());
                SeamError::Unavailable(format!(
                    "the session with node {} closed mid-request ({reason}); retry to redial",
                    to.id.short()
                ))
            }
            None => SeamError::failed(format!("exchange with node {} failed: {e}", to.id.short())),
        }
    }
}

fn now() -> Timestamp {
    Timestamp::from(SystemTime::now())
}

fn node_id_of(key: PublicKey) -> NodeId {
    NodeId::from_bytes(*key.as_bytes())
}

fn public_key_of(id: &NodeId) -> Result<PublicKey, SeamError> {
    PublicKey::from_bytes(id.as_bytes())
        .map_err(|e| SeamError::failed(format!("node id {id} is not an Ed25519 public key: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rand::RngCore;

    use inseam_seams::transport::MESSAGE_BYTES_MAX;

    /// Generous bound on any one step; loopback exchanges take milliseconds.
    const WAIT: Duration = Duration::from_secs(10);

    fn settings() -> Settings {
        Settings {
            relay: RelaySetting::None,
            bind: BindTarget::Loopback,
            request_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(30),
        }
    }

    async fn transport() -> IrohTransport {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        IrohTransport::bind(SecretKeyBytes::from_bytes(bytes), &settings())
            .await
            .expect("binds on loopback")
    }

    /// Where to dial `t`, as a peer holding its roster record would.
    async fn address_of(t: &IrohTransport, invitation: Option<&str>) -> PeerAddress {
        let endpoints = dialable_endpoints(t).await;
        PeerAddress {
            id: t.local_id(),
            endpoints,
            invitation: invitation.map(|s| InvitationToken::new(s).expect("valid token")),
        }
    }

    /// The bound socket reaches the address watcher just after bind; wait
    /// for it under the test bound rather than assume.
    async fn dialable_endpoints(t: &IrohTransport) -> Vec<Endpoint> {
        use iroh::Watcher;
        let mut watcher = t.inner.endpoint.watch_addr();
        tokio::time::timeout(WAIT, async {
            loop {
                if watcher.get().ip_addrs().next().is_some() {
                    break;
                }
                watcher.updated().await.expect("the endpoint is alive");
            }
        })
        .await
        .expect("a bound endpoint reports its socket");
        let endpoints = t.endpoints();
        assert!(!endpoints.is_empty());
        endpoints
    }

    fn protocol(name: &str) -> ProtocolName {
        ProtocolName::new(name).expect("valid protocol name")
    }

    fn echo() -> ProtocolName {
        protocol("inseam/echo/1")
    }

    /// Answers with the body prefixed, and remembers who asked. Built
    /// shared, since the transport holds one handle and the test another.
    struct Echo {
        asked_by: Mutex<Option<NodeId>>,
        calls: AtomicU32,
    }

    impl Echo {
        fn shared() -> Arc<Self> {
            Arc::new(Self {
                asked_by: Mutex::new(None),
                calls: AtomicU32::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl RequestHandler for Echo {
        async fn handle(&self, peer: NodeId, body: Vec<u8>) -> Result<Vec<u8>, SeamError> {
            *self.asked_by.lock().expect("lock") = Some(peer);
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok([b"echo:".as_slice(), body.as_slice()].concat())
        }
    }

    struct Failing;

    #[async_trait::async_trait]
    impl RequestHandler for Failing {
        async fn handle(&self, _peer: NodeId, _body: Vec<u8>) -> Result<Vec<u8>, SeamError> {
            Err(SeamError::Refused("the handler said no".to_string()))
        }
    }

    struct AdmitAll;

    impl Admission for AdmitAll {
        fn admit(&self, _peer: &NodeId, _invitation: Option<&InvitationToken>) -> Admit {
            Admit::Admitted
        }
    }

    struct AdmitToken(InvitationToken);

    impl Admission for AdmitToken {
        fn admit(&self, _peer: &NodeId, invitation: Option<&InvitationToken>) -> Admit {
            if invitation == Some(&self.0) {
                Admit::Admitted
            } else {
                Admit::Refused("the token does not match".to_string())
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_registered_handler_answers_a_dialing_peer() {
        let a = transport().await;
        let b = transport().await;
        let _policy = a.set_admission(Arc::new(AdmitAll)).expect("policy");
        let handler = Echo::shared();
        let _handler = a.register(echo(), handler.clone()).expect("registers");

        let to = address_of(&a, None).await;
        let answer = b.request(&to, &echo(), b"hi".to_vec(), WAIT).await.expect("answered");
        assert_eq!(answer, b"echo:hi");
        assert_eq!(*handler.asked_by.lock().expect("lock"), Some(b.local_id()));

        let on_a = a.sessions();
        assert_eq!(on_a.len(), 1);
        assert_eq!(on_a[0].peer, b.local_id());
        assert_eq!(on_a[0].direction, SessionDirection::Inbound);
        let on_b = b.sessions();
        assert_eq!(on_b.len(), 1);
        assert_eq!(on_b[0].peer, a.local_id());
        assert_eq!(on_b[0].direction, SessionDirection::Outbound);

        // A second request reuses the session: still one each side.
        let again = b.request(&to, &echo(), b"again".to_vec(), WAIT).await.expect("answered");
        assert_eq!(again, b"echo:again");
        assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
        assert_eq!(a.sessions().len(), 1);
        assert_eq!(b.sessions().len(), 1);
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unregistered_protocol_is_an_error_not_a_hang() {
        let a = transport().await;
        let b = transport().await;
        let _policy = a.set_admission(Arc::new(AdmitAll)).expect("policy");
        let _failing = a.register(protocol("inseam/no/1"), Arc::new(Failing)).expect("registers");
        let to = address_of(&a, None).await;

        let unknown = tokio::time::timeout(WAIT * 2, b.request(&to, &protocol("inseam/nope/1"), Vec::new(), WAIT))
            .await
            .expect("answers before the bound");
        match unknown {
            Err(SeamError::Failed(message)) => assert!(message.contains("serves no protocol"), "{message}"),
            other => panic!("expected a failed exchange, got {other:?}"),
        }
        let refused = b.request(&to, &protocol("inseam/no/1"), Vec::new(), WAIT).await;
        match refused {
            Err(SeamError::Failed(message)) => assert!(message.contains("the handler said no"), "{message}"),
            other => panic!("expected the handler's error, got {other:?}"),
        }
        assert_eq!(a.sessions().len(), 1, "an error answer does not cost the session");
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peers_are_refused_without_a_policy_and_admitted_with_one() {
        let a = transport().await;
        let b = transport().await;
        let _handler = a.register(echo(), Echo::shared()).expect("registers");
        let to = address_of(&a, None).await;

        let refused = b.request(&to, &echo(), b"x".to_vec(), WAIT).await;
        match refused {
            Err(SeamError::NotAdmitted(id)) => assert_eq!(id, a.local_id()),
            other => panic!("expected NotAdmitted, got {other:?}"),
        }
        assert!(a.sessions().is_empty(), "a refused peer never becomes a session");
        assert!(b.sessions().is_empty(), "a refused dial is not kept");

        let _policy = a.set_admission(Arc::new(AdmitAll)).expect("policy");
        let answer = b.request(&to, &echo(), b"x".to_vec(), WAIT).await.expect("admitted now");
        assert_eq!(answer, b"echo:x");
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_invitation_closes_the_connection() {
        let a = transport().await;
        let b = transport().await;
        let token = InvitationToken::new("open-sesame").expect("valid");
        let _policy = a.set_admission(Arc::new(AdmitToken(token))).expect("policy");
        let _handler = a.register(echo(), Echo::shared()).expect("registers");

        for wrong in [Some("wrong"), None] {
            let to = address_of(&a, wrong).await;
            let refused = b.request(&to, &echo(), b"x".to_vec(), WAIT).await;
            assert!(matches!(refused, Err(SeamError::NotAdmitted(id)) if id == a.local_id()), "{refused:?}");
            assert!(a.sessions().is_empty());
            assert!(b.sessions().is_empty());
        }

        let to = address_of(&a, Some("open-sesame")).await;
        let answer = b.request(&to, &echo(), b"x".to_vec(), WAIT).await.expect("the right token admits");
        assert_eq!(answer, b"echo:x");
        assert_eq!(a.sessions().len(), 1);
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_inbound_session_serves_outbound_requests() {
        let a = transport().await;
        let b = transport().await;
        let _policy = a.set_admission(Arc::new(AdmitAll)).expect("policy");
        let _on_a = a.register(echo(), Echo::shared()).expect("registers");
        let on_b = Echo::shared();
        let _on_b = b.register(echo(), on_b.clone()).expect("registers");

        // B dials A once; from then on A reaches B over that session, with
        // no endpoint for B at all — B is the outbound-only laptop.
        let to_a = address_of(&a, None).await;
        b.request(&to_a, &echo(), b"from b".to_vec(), WAIT).await.expect("b reaches a");
        let to_b = PeerAddress {
            id: b.local_id(),
            endpoints: Vec::new(),
            invitation: None,
        };
        let answer = a.request(&to_b, &echo(), b"from a".to_vec(), WAIT).await.expect("a reaches b");
        assert_eq!(answer, b"echo:from a");
        assert_eq!(*on_b.asked_by.lock().expect("lock"), Some(a.local_id()));

        let on_a_table = a.sessions();
        assert_eq!(on_a_table.len(), 1, "no second connection was opened");
        assert_eq!(on_a_table[0].direction, SessionDirection::Inbound);
        assert_eq!(on_a_table[0].peer, b.local_id());
        assert_eq!(b.sessions().len(), 1);
        assert_eq!(b.sessions()[0].direction, SessionDirection::Outbound);

        // Without the session, the same address is undialable.
        a.disconnect(&b.local_id());
        assert!(a.sessions().is_empty());
        let unreachable = a.request(&to_b, &echo(), b"?".to_vec(), WAIT).await;
        assert!(matches!(unreachable, Err(SeamError::Unavailable(_))), "{unreachable:?}");
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_oversized_body_is_refused_before_it_is_sent() {
        let a = transport().await;
        let b = transport().await;
        let _policy = a.set_admission(Arc::new(AdmitAll)).expect("policy");
        let to = address_of(&a, None).await;
        let too_big = usize::try_from(MESSAGE_BYTES_MAX + 1).expect("fits");
        let refused = b.request(&to, &echo(), vec![0u8; too_big], WAIT).await;
        assert!(matches!(refused, Err(SeamError::Refused(_))), "{refused:?}");
        assert!(b.sessions().is_empty(), "nothing was dialed");
        assert!(a.sessions().is_empty());

        let hello = b.request(&to, &wire::hello_protocol(), Vec::new(), WAIT).await;
        assert!(matches!(hello, Err(SeamError::Refused(_))), "{hello:?}");
        let zero = b.request(&to, &echo(), Vec::new(), Duration::ZERO).await;
        assert!(matches!(zero, Err(SeamError::Refused(_))), "{zero:?}");
        a.close().await;
        b.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bound_transport_publishes_endpoints_that_parse_back() {
        let a = transport().await;
        let endpoints = dialable_endpoints(&a).await;
        assert!(endpoints.iter().all(|e| e.as_str().starts_with("ip:127.0.0.1:")), "{endpoints:?}");
        let parsed = endpoints::parse(&endpoints);
        assert_eq!(parsed.len(), endpoints.len());
        assert!(parsed.iter().all(|addr| addr.is_ip()));
        let id = a.local_id();
        assert_eq!(id.as_bytes(), a.inner.endpoint.id().as_bytes());
        a.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registrations_are_one_per_name_and_disposers_withdraw_them() {
        let a = transport().await;
        let first = a.register(echo(), Echo::shared()).expect("registers");
        assert!(matches!(a.register(echo(), Echo::shared()), Err(SeamError::Refused(_))));
        first();
        let _second = a.register(echo(), Echo::shared()).expect("registers again after disposal");
        assert!(matches!(
            a.register(wire::hello_protocol(), Echo::shared()),
            Err(SeamError::Refused(_))
        ));

        let policy = a.set_admission(Arc::new(AdmitAll)).expect("policy");
        assert!(matches!(a.set_admission(Arc::new(AdmitAll)), Err(SeamError::Refused(_))));
        policy();
        let _policy = a.set_admission(Arc::new(AdmitAll)).expect("installs again after disposal");
        a.close().await;
    }

    #[test]
    fn the_config_parses_relay_modes_and_refuses_bad_ones() {
        let defaults = Settings::try_from(&TransportConfig::default()).expect("defaults parse");
        assert_eq!(defaults.relay, RelaySetting::N0);
        assert_eq!(defaults.bind, BindTarget::AllInterfaces { port: 0 });
        assert_eq!(defaults.request_timeout, Duration::from_secs(30));
        assert_eq!(defaults.idle_timeout, Duration::from_secs(120));

        assert_eq!(RelaySetting::parse("none").expect("parses"), RelaySetting::None);
        let own = RelaySetting::parse("https://relay.example").expect("parses");
        assert!(matches!(&own, RelaySetting::Own(url) if url.as_str() == "https://relay.example/"));
        assert_eq!(own.to_string(), "https://relay.example/");
        for bad in ["", "http://relay.example", "ftp://x", "relay.example", "N0"] {
            assert!(RelaySetting::parse(bad).is_err(), "{bad} should be refused");
        }

        let zero_timeout = TransportConfig {
            request_timeout_secs: 0,
            ..TransportConfig::default()
        };
        assert!(Settings::try_from(&zero_timeout).is_err());
        let short_idle = TransportConfig {
            idle_timeout_secs: IDLE_TIMEOUT_SECS_MIN - 1,
            ..TransportConfig::default()
        };
        assert!(Settings::try_from(&short_idle).is_err());
        let least_idle = TransportConfig {
            idle_timeout_secs: IDLE_TIMEOUT_SECS_MIN,
            ..TransportConfig::default()
        };
        assert!(Settings::try_from(&least_idle).is_ok());

        let unknown: toml::Table = toml::from_str("port = 1").expect("toml");
        assert!(IrohTransportFactory.build(&unknown).is_err());
        let fine: toml::Table = toml::from_str("relay = \"none\"\nbind_port = 4433").expect("toml");
        assert!(IrohTransportFactory.build(&fine).is_ok());
        assert_eq!(IrohTransportFactory.name(), PLUGIN_NAME);
    }
}
