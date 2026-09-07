//! Boot a whole node for a test — a real kernel and store in a temp dir,
//! the fake node and transport beside the real `connections`, `roster`,
//! and `sync` plugins — and the fixtures the roster and sync tests share.
//! A stub connection plugin registers whichever hosts a test names, so
//! stewardship publishing runs against the real registry.

use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Notify;

use inseam_kernel::address::{Address, ContentLength, Envelope, HostId, Timestamp};
use inseam_kernel::fragment::Mimetype;
use inseam_kernel::network::{Endpoint, NodeId};
use inseam_kernel::store::IndexStore;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Composition, EventBus, Inject, Kernel, Manifest, Plugin, PluginError,
    PluginFactory, Subscription,
};
use inseam_seams::connection::{
    self, Capabilities, Connection, EnumeratedSource, HostDescription, HostKind, Registration,
};
use inseam_seams::roster::{Roster, RosterChanged, ROSTER};
use inseam_seams::sync::{Synchronizer, SYNC};
use inseam_seams::transport::{PeerAddress, Transport, TRANSPORT};
use inseam_seams::SeamError;

use super::fake_transport::{FakeNetwork, FakeNodeFactory, FakeTransportFactory};
use crate::connections::ConnectionsRegistryFactory;
use crate::roster::RosterFactory;
use crate::sync::SyncFactory;

/// How long a test waits for an event before declaring it never came.
const EVENT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) fn node_id(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 32])
}

pub(crate) fn endpoint(text: &str) -> Endpoint {
    Endpoint::new(text).expect("test endpoint is valid")
}

pub(crate) fn address(text: &str) -> Address {
    text.parse().expect("test address parses")
}

pub(crate) fn envelope(modified: i64) -> Envelope {
    Envelope {
        source_type: "file".to_string(),
        content_type: Mimetype::text_plain(),
        length: ContentLength::Lines(3),
        created: None,
        modified: Some(Timestamp(modified)),
        observed: Timestamp(modified),
        properties: Vec::new(),
        hint: None,
        content_digest: None,
    }
}

/// The composition every test node runs: timers set an hour out so only
/// the tests drive exchanges, and one stub connection per named host.
pub(crate) fn composition(hosts: &[&str]) -> Composition {
    let mut text = String::from(
        r#"
[[entry]]
id = "connections"
plugin = "connections"

[[entry]]
id = "node"
plugin = "node"

[[entry]]
id = "transport"
plugin = "transport"
"#,
    );
    for host in hosts {
        text.push_str(&format!(
            r#"
[[entry]]
id = "host-{host}"
plugin = "stub-connection"
[entry.config]
host = "{host}"
roots = ["notes"]
"#
        ));
    }
    text.push_str(
        r#"
[[entry]]
id = "roster"
plugin = "roster"
[entry.config]
endpoint_poll_secs = 3600

[[entry]]
id = "sync"
plugin = "sync"
[entry.config]
interval_secs = 3600
initial_delay_secs = 3600
"#,
    );
    Composition::parse(&text, "test composition").expect("test composition parses")
}

pub(crate) struct TestNode {
    pub id: NodeId,
    pub kernel: Kernel,
    _dir: TempDir,
}

impl TestNode {
    /// Boot node `byte` on `network`, dialable at `endpoints` (none for an
    /// outbound-only node), stewarding `hosts`.
    pub(crate) async fn boot(
        network: &Arc<FakeNetwork>,
        byte: u8,
        endpoints: &[&str],
        hosts: &[&str],
    ) -> Self {
        Self::boot_with(network, byte, endpoints, hosts, false).await
    }

    pub(crate) async fn boot_with(
        network: &Arc<FakeNetwork>,
        byte: u8,
        endpoints: &[&str],
        hosts: &[&str],
        always_on: bool,
    ) -> Self {
        let id = node_id(byte);
        let dir = tempfile::tempdir().expect("tempdir");
        let factories: Vec<Arc<dyn PluginFactory>> = vec![
            Arc::new(ConnectionsRegistryFactory),
            Arc::new(FakeNodeFactory {
                id,
                display_name: format!("node-{byte:02x}"),
                always_on,
            }),
            Arc::new(FakeTransportFactory {
                network: Arc::clone(network),
                id,
                endpoints: endpoints.iter().map(|e| endpoint(e)).collect(),
            }),
            Arc::new(StubConnectionFactory),
            Arc::new(RosterFactory),
            Arc::new(SyncFactory),
        ];
        let mut kernel = Kernel::boot(dir.path(), factories, Vec::new())
            .await
            .expect("kernel boots");
        kernel
            .reconcile(&composition(hosts))
            .await
            .expect("test composition settles");
        for fiber in kernel.fibers() {
            assert_eq!(
                fiber.state,
                inseam_kernel::substrate::FiberState::Active,
                "fiber `{}` is active after boot",
                fiber.id
            );
        }
        Self {
            id,
            kernel,
            _dir: dir,
        }
    }

    /// Reconcile against a different host set, as the owner editing the
    /// composition would.
    pub(crate) async fn steward(&mut self, hosts: &[&str]) {
        self.kernel
            .reconcile(&composition(hosts))
            .await
            .expect("test composition settles");
    }

    pub(crate) fn roster(&self) -> Arc<dyn Roster> {
        self.kernel.service(&ROSTER).expect("roster bound")
    }

    pub(crate) fn sync(&self) -> Arc<dyn Synchronizer> {
        self.kernel.service(&SYNC).expect("sync bound")
    }

    pub(crate) fn transport(&self) -> Arc<dyn Transport> {
        self.kernel.service(&TRANSPORT).expect("transport bound")
    }

    pub(crate) fn store(&self) -> &Arc<IndexStore> {
        self.kernel.store()
    }

    pub(crate) fn bus(&self) -> &EventBus {
        self.kernel.bus()
    }

    /// This node as a peer dials it: id and current endpoints, no token.
    pub(crate) fn address(&self) -> PeerAddress {
        PeerAddress {
            id: self.id,
            endpoints: self.transport().endpoints(),
            invitation: None,
        }
    }

    /// Start listening for the next roster change before the action that
    /// should cause it.
    pub(crate) fn expect_roster_change(&self) -> RosterChangeWaiter {
        RosterChangeWaiter::on(self.bus())
    }
}

/// A subscription that resolves once `RosterChanged` fires after it was
/// taken — event-driven, bounded by [`EVENT_WAIT`], never a sleep.
pub(crate) struct RosterChangeWaiter {
    fired: Arc<Notify>,
    _subscription: Subscription,
}

impl RosterChangeWaiter {
    pub(crate) fn on(bus: &EventBus) -> Self {
        let fired = Arc::new(Notify::new());
        let signal = Arc::clone(&fired);
        let subscription = bus.on::<RosterChanged>(move |_| signal.notify_one());
        Self {
            fired,
            _subscription: subscription,
        }
    }

    pub(crate) async fn wait(&self) {
        tokio::time::timeout(EVENT_WAIT, self.fired.notified())
            .await
            .expect("RosterChanged fired within the wait bound");
    }
}

/// A connection that lists nothing; enough to register a host.
struct StubConnection;

#[async_trait::async_trait]
impl Connection for StubConnection {
    async fn enumerate(&self, _root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        Ok(Vec::new())
    }
    fn locator_prefix(&self, _root: &str) -> Option<String> {
        None
    }
    async fn read_text(&self, _address: &Address) -> Result<String, SeamError> {
        Ok(String::new())
    }
    async fn read_lines(&self, _a: &Address, _s: u64, _e: u64) -> Result<String, SeamError> {
        Ok(String::new())
    }
    async fn read_bytes(&self, _address: &Address) -> Result<Vec<u8>, SeamError> {
        Ok(Vec::new())
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StubConnectionConfig {
    host: String,
    #[serde(default)]
    roots: Vec<String>,
}

struct StubConnectionFactory;

impl PluginFactory for StubConnectionFactory {
    fn name(&self) -> &str {
        "stub-connection"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        let config: StubConnectionConfig = parse_config(config)?;
        Ok(Box::new(StubConnectionPlugin { config }))
    }
}

struct StubConnectionPlugin {
    config: StubConnectionConfig,
}

#[async_trait::async_trait]
impl Plugin for StubConnectionPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("connections")];
        Manifest {
            name: "stub-connection",
            inject: INJECT,
            provides: &[],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let registration = Registration {
            entry_id: cx.entry_id().to_string(),
            host: HostDescription {
                id: HostId::new(&self.config.host).map_err(|e| PluginError(e.to_string()))?,
                kind: HostKind::filesystem(),
                display_name: format!("{} display", self.config.host),
            },
            capabilities: Capabilities::READ_ONLY,
            roots: self.config.roots.clone(),
            connection: Arc::new(StubConnection),
        };
        connection::register_as_effect(cx, registration)
    }
}
