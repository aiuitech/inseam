//! An in-memory network for tests: every fake node's handlers, admission
//! policy, endpoints, and sessions in one shared table, so a request from
//! one node invokes another's handler exactly the way the real transport
//! would — admission asked on first contact with whatever invitation the
//! address carries, sessions remembered afterwards in both directions —
//! without a socket anywhere. The fake node beside it is a fixed keypair.
//! Both mount as plugins, so a test boots real kernels over them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::{Endpoint, NodeCapabilities, NodeId};
use inseam_kernel::substrate::{
    ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::SeamError;
use inseam_seams::node::{NODE, Node, SecretKeyBytes};
use inseam_seams::transport::{
    Admission, Admit, Disposer, PeerAddress, ProtocolName, RequestHandler, SessionDirection,
    SessionView, TRANSPORT, Transport,
};

#[derive(Default)]
struct FakeNodeState {
    endpoints: Vec<Endpoint>,
    handlers: HashMap<ProtocolName, Arc<dyn RequestHandler>>,
    admission: Option<Arc<dyn Admission>>,
    /// Live sessions from this node's view, one per peer.
    sessions: Vec<(NodeId, SessionDirection)>,
    /// Peers this node disconnected, in order — what a test asserts on.
    disconnected: Vec<NodeId>,
}

/// The shared table every fake transport on one test network reads.
#[derive(Default)]
pub(crate) struct FakeNetwork {
    nodes: Mutex<HashMap<NodeId, FakeNodeState>>,
}

impl FakeNetwork {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Put a node on the network with its dialing hints; a second join of
    /// the same id starts it over, which is what a remounted plugin does.
    pub(crate) fn join(&self, id: NodeId, endpoints: Vec<Endpoint>) {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.insert(
            id,
            FakeNodeState {
                endpoints,
                ..FakeNodeState::default()
            },
        );
    }

    /// Rotate a node's endpoints — what a NAT rebinding does to a laptop.
    pub(crate) fn set_endpoints(&self, id: NodeId, endpoints: Vec<Endpoint>) {
        self.with_state(id, |state| state.endpoints = endpoints);
    }

    pub(crate) fn disconnected_by(&self, id: NodeId) -> Vec<NodeId> {
        self.with_state(id, |state| state.disconnected.clone())
    }

    pub(crate) fn has_session(&self, a: NodeId, b: NodeId) -> bool {
        self.with_state(a, |state| state.sessions.iter().any(|(peer, _)| *peer == b))
    }

    fn with_state<R>(&self, id: NodeId, f: impl FnOnce(&mut FakeNodeState) -> R) -> R {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        let state = nodes
            .get_mut(&id)
            .unwrap_or_else(|| panic!("node {} is not on the fake network", id.short()));
        f(state)
    }

    /// The synchronous half of a request: reach the peer (a live session
    /// in either direction, else a dial at its endpoints and its admission
    /// policy's answer), then hand back the handler to invoke outside the
    /// lock.
    fn connect(
        &self,
        from: NodeId,
        to: &PeerAddress,
        protocol: &ProtocolName,
    ) -> Result<Arc<dyn RequestHandler>, SeamError> {
        let mut nodes = self.nodes.lock().unwrap_or_else(|e| e.into_inner());
        let target = nodes.get(&to.id).ok_or_else(|| {
            SeamError::failed(format!("node {} is not on the network", to.id.short()))
        })?;
        let session_open = target.sessions.iter().any(|(peer, _)| *peer == from);
        if !session_open {
            let reachable = to
                .endpoints
                .iter()
                .any(|endpoint| target.endpoints.contains(endpoint));
            if !reachable {
                return Err(SeamError::failed(format!(
                    "node {} has no session with {} and none of its endpoints reach it",
                    from.short(),
                    to.id.short()
                )));
            }
            let admitted = match &target.admission {
                None => Admit::Refused("no admission policy is set".to_string()),
                Some(policy) => policy.admit(&from, to.invitation.as_ref()),
            };
            if let Admit::Refused(reason) = admitted {
                tracing::debug!(peer = %from.short(), "fake transport refused: {reason}");
                return Err(SeamError::NotAdmitted(from));
            }
            let target = nodes.get_mut(&to.id).expect("looked up above");
            target.sessions.push((from, SessionDirection::Inbound));
            let dialer = nodes
                .get_mut(&from)
                .unwrap_or_else(|| panic!("node {} is not on the fake network", from.short()));
            dialer.sessions.push((to.id, SessionDirection::Outbound));
        }
        let target = nodes.get(&to.id).expect("looked up above");
        target.handlers.get(protocol).cloned().ok_or_else(|| {
            SeamError::Unavailable(format!(
                "node {} serves no `{protocol}` handler",
                to.id.short()
            ))
        })
    }
}

pub(crate) struct FakeTransport {
    network: Arc<FakeNetwork>,
    id: NodeId,
}

#[async_trait::async_trait]
impl Transport for FakeTransport {
    fn local_id(&self) -> NodeId {
        self.id
    }

    fn endpoints(&self) -> Vec<Endpoint> {
        self.network
            .with_state(self.id, |state| state.endpoints.clone())
    }

    fn register(
        &self,
        protocol: ProtocolName,
        handler: Arc<dyn RequestHandler>,
    ) -> Result<Disposer, SeamError> {
        let name = protocol.clone();
        self.network.with_state(self.id, |state| {
            if state.handlers.contains_key(&protocol) {
                return Err(SeamError::Refused(format!(
                    "protocol `{protocol}` already has a handler"
                )));
            }
            state.handlers.insert(protocol, handler);
            Ok(())
        })?;
        let network = Arc::clone(&self.network);
        let id = self.id;
        Ok(Box::new(move || {
            network.with_state(id, |state| {
                state.handlers.remove(&name);
            });
        }))
    }

    fn set_admission(&self, policy: Arc<dyn Admission>) -> Result<Disposer, SeamError> {
        self.network.with_state(self.id, |state| {
            if state.admission.is_some() {
                return Err(SeamError::Refused(
                    "an admission policy is already set".to_string(),
                ));
            }
            state.admission = Some(policy);
            Ok(())
        })?;
        let network = Arc::clone(&self.network);
        let id = self.id;
        Ok(Box::new(move || {
            network.with_state(id, |state| state.admission = None);
        }))
    }

    async fn request(
        &self,
        to: &PeerAddress,
        protocol: &ProtocolName,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<Vec<u8>, SeamError> {
        let handler = self.network.connect(self.id, to, protocol)?;
        tokio::time::timeout(timeout, handler.handle(self.id, body))
            .await
            .map_err(|_| SeamError::failed(format!("request to {} timed out", to.id.short())))?
    }

    fn sessions(&self) -> Vec<SessionView> {
        self.network.with_state(self.id, |state| {
            state
                .sessions
                .iter()
                .map(|(peer, direction)| SessionView {
                    peer: *peer,
                    direction: *direction,
                    since: Timestamp(0),
                    last_used: Timestamp(0),
                })
                .collect()
        })
    }

    fn disconnect(&self, peer: &NodeId) {
        let mut nodes = self.network.nodes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = nodes.get_mut(&self.id) {
            state.sessions.retain(|(id, _)| id != peer);
            state.disconnected.push(*peer);
        }
        if let Some(state) = nodes.get_mut(peer) {
            state.sessions.retain(|(id, _)| *id != self.id);
        }
    }
}

/// Mounts a [`FakeTransport`] for one node; a test builds one per node.
pub(crate) struct FakeTransportFactory {
    pub network: Arc<FakeNetwork>,
    pub id: NodeId,
    pub endpoints: Vec<Endpoint>,
}

impl PluginFactory for FakeTransportFactory {
    fn name(&self) -> &str {
        "transport"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(FakeTransportPlugin {
            network: Arc::clone(&self.network),
            id: self.id,
            endpoints: self.endpoints.clone(),
        }))
    }
}

struct FakeTransportPlugin {
    network: Arc<FakeNetwork>,
    id: NodeId,
    endpoints: Vec<Endpoint>,
}

#[async_trait::async_trait]
impl Plugin for FakeTransportPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "transport",
            inject: INJECT,
            provides: &["transport"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        self.network.join(self.id, self.endpoints.clone());
        let transport = FakeTransport {
            network: Arc::clone(&self.network),
            id: self.id,
        };
        cx.provide(
            &TRANSPORT,
            Arc::new(transport) as Arc<dyn Transport>,
            Facts::new(),
        )?;
        Ok(())
    }
}

/// A node with a fixed key: the id is the key bytes, so tests name nodes
/// by one byte.
pub(crate) struct FakeNode {
    pub id: NodeId,
    pub display_name: String,
    pub always_on: bool,
}

impl Node for FakeNode {
    fn id(&self) -> NodeId {
        self.id
    }

    fn display_name(&self) -> String {
        self.display_name.clone()
    }

    fn capabilities(&self) -> NodeCapabilities {
        NodeCapabilities {
            always_on: self.always_on,
            deep_index: true,
            relays: true,
        }
    }

    fn secret_key(&self) -> SecretKeyBytes {
        SecretKeyBytes::from_bytes(*self.id.as_bytes())
    }
}

pub(crate) struct FakeNodeFactory {
    pub id: NodeId,
    pub display_name: String,
    pub always_on: bool,
}

impl PluginFactory for FakeNodeFactory {
    fn name(&self) -> &str {
        "node"
    }

    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(FakeNodePlugin {
            node: Arc::new(FakeNode {
                id: self.id,
                display_name: self.display_name.clone(),
                always_on: self.always_on,
            }),
        }))
    }
}

struct FakeNodePlugin {
    node: Arc<FakeNode>,
}

#[async_trait::async_trait]
impl Plugin for FakeNodePlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[];
        Manifest {
            name: "node",
            inject: INJECT,
            provides: &["node"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        cx.provide(&NODE, Arc::clone(&self.node) as Arc<dyn Node>, Facts::new())?;
        Ok(())
    }
}
