//! The `sync` plugin: the seam's provider — catalog and roster
//! replication over the transport (`design/address-sync.md`). It serves
//! `inseam/sync/1` for peers that dial in and drives exchanges with the
//! peers it can reach: one round with every dialable roster node on a
//! timer, one exchange with one named peer on request (which is how a
//! join presents its invitation). Everything replicates through the store's
//! per-origin logs and version vectors; this plugin only moves suffixes and
//! announces [`RosterChanged`] when an applied entry could have changed
//! the roster.
//!
//! [`RosterChanged`]: inseam_seams::roster::RosterChanged

mod exchange;
mod peers;
mod protocol;
mod views;

#[cfg(test)]
pub(crate) mod fake_transport;
#[cfg(test)]
pub(crate) mod harness;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use inseam_kernel::address::Timestamp;
use inseam_kernel::network::NodeId;
use inseam_kernel::store::IndexStore;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError,
    PluginFactory, STORE,
};
use inseam_seams::roster::{Roster, ROSTER};
use inseam_seams::sync::{PeerSyncView, SyncStatus, Synchronizer, PEERS_PER_ROUND_MAX, SYNC};
use inseam_seams::transport::{register_as_effect, PeerAddress, Transport, TRANSPORT};
use inseam_seams::SeamError;

use exchange::{exchange, Handler};
use peers::choose_peers;
use views::PeerLedgers;

pub use exchange::ROUNDS_MAX;
pub use protocol::{protocol_name, SyncRequest, SyncResponse, PROTOCOL};
pub use views::PEER_VIEWS_MAX;

/// Exchanges one round runs at once; the rest wait their turn. Eight keeps
/// a round short on a backbone with tens of peers without opening a
/// session storm on a laptop.
pub const CONCURRENT_PEERS_MAX: usize = 8;
const _: () = assert!(CONCURRENT_PEERS_MAX > 0, "a round must run at least one exchange");
const _: () = assert!(
    CONCURRENT_PEERS_MAX <= PEERS_PER_ROUND_MAX,
    "more concurrency than peers in a round is idle capacity"
);
/// Timer rounds before the loop's assertion fires. The loop is meant to
/// run for the node's lifetime; at one round per second this is longer
/// than any process lives, so reaching it is a bug, not a long uptime.
const SYNC_TICKS_MAX: u64 = 1 << 40;

pub const INTERVAL_SECS_DEFAULT: u32 = 60;
pub const INITIAL_DELAY_SECS_DEFAULT: u32 = 2;
pub const REQUEST_TIMEOUT_SECS_DEFAULT: u32 = 30;

pub mod facts {
    pub const INTERVAL_SECS: &str = "interval_secs";
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Seconds between timer rounds.
    pub interval_secs: u32,
    /// Seconds after mount before the first timer round, so the transport
    /// and the roster settle first.
    pub initial_delay_secs: u32,
    /// Most peers one timer round exchanges with; clamped to the seam's
    /// [`PEERS_PER_ROUND_MAX`].
    pub peers_per_round_max: u32,
    /// How long one request may take before the peer is given up on.
    pub request_timeout_secs: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            interval_secs: INTERVAL_SECS_DEFAULT,
            initial_delay_secs: INITIAL_DELAY_SECS_DEFAULT,
            peers_per_round_max: u32::try_from(PEERS_PER_ROUND_MAX).expect("the bound fits u32"),
            request_timeout_secs: REQUEST_TIMEOUT_SECS_DEFAULT,
        }
    }
}

impl SyncConfig {
    /// The configured per-round bound as the seam allows it.
    fn peers_per_round(&self) -> usize {
        let configured = usize::try_from(self.peers_per_round_max).expect("u32 fits usize");
        configured.clamp(1, PEERS_PER_ROUND_MAX)
    }
}

pub struct SyncPlugin {
    config: SyncConfig,
}

impl SyncPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: SyncConfig = parse_config(config)?;
        if config.interval_secs == 0 {
            return Err(PluginError(
                "config: interval_secs must be at least 1; zero would spin".to_string(),
            ));
        }
        if config.request_timeout_secs == 0 {
            return Err(PluginError(
                "config: request_timeout_secs must be at least 1; zero refuses every request"
                    .to_string(),
            ));
        }
        if config.peers_per_round_max == 0 {
            return Err(PluginError(
                "config: peers_per_round_max must be at least 1; zero syncs with nobody"
                    .to_string(),
            ));
        }
        Ok(Self { config })
    }
}

pub struct SyncFactory;

impl PluginFactory for SyncFactory {
    fn name(&self) -> &str {
        "sync"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(SyncPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for SyncPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[
            Inject::required("store"),
            Inject::required("node"),
            Inject::required("transport"),
            Inject::required("roster"),
        ];
        Manifest {
            name: "sync",
            inject: INJECT,
            provides: &["sync"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let node = cx.get(&inseam_seams::node::NODE)?;
        let transport = cx.get(&TRANSPORT)?;
        let local = node.id();
        assert_eq!(
            local,
            transport.local_id(),
            "the node seam and the transport agree on this node's identity"
        );
        let inner = Arc::new(Inner {
            local,
            store: cx.get(&STORE)?,
            transport,
            roster: cx.get(&ROSTER)?,
            bus: cx.bus().clone(),
            request_timeout: Duration::from_secs(u64::from(self.config.request_timeout_secs)),
            peers_per_round_max: self.config.peers_per_round(),
            views: Mutex::new(PeerLedgers::default()),
        });
        let handler = Arc::new(Handler {
            inner: Arc::clone(&inner),
        });
        register_as_effect(cx, protocol_name(), handler)?;
        let initial_delay = Duration::from_secs(u64::from(self.config.initial_delay_secs));
        let interval = Duration::from_secs(u64::from(self.config.interval_secs));
        let task = tokio::spawn(sync_loop(Arc::clone(&inner), initial_delay, interval));
        cx.effect("abort the sync timer", move || task.abort());
        let service = Arc::new(Service { inner });
        cx.provide(
            &SYNC,
            service as Arc<dyn Synchronizer>,
            Facts::new().with(facts::INTERVAL_SECS, self.config.interval_secs),
        )?;
        Ok(())
    }
}

/// The timer: one round with every dialable peer, every `interval`. A
/// round's failures land on the peer views, so the loop itself only logs.
async fn sync_loop(inner: Arc<Inner>, initial_delay: Duration, interval: Duration) {
    tokio::time::sleep(initial_delay).await;
    for _tick in 0..SYNC_TICKS_MAX {
        if let Err(e) = inner.sync_now().await {
            tracing::warn!("sync round failed: {e}");
        }
        tokio::time::sleep(interval).await;
    }
    unreachable!("the sync timer runs for the node's lifetime; {SYNC_TICKS_MAX} rounds is longer than any process");
}

pub struct Service {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    local: NodeId,
    store: Arc<IndexStore>,
    transport: Arc<dyn Transport>,
    roster: Arc<dyn Roster>,
    bus: EventBus,
    request_timeout: Duration,
    peers_per_round_max: usize,
    views: Mutex<PeerLedgers>,
}

fn now() -> Timestamp {
    Timestamp::from(SystemTime::now())
}

impl Inner {
    fn session_peers(&self) -> HashSet<NodeId> {
        self.transport
            .sessions()
            .into_iter()
            .map(|session| session.peer)
            .collect()
    }

    /// One exchange, recorded on the peer's view whichever way it went;
    /// the error is returned as well so a join fails loudly.
    async fn sync_with(&self, peer: &PeerAddress) -> Result<PeerSyncView, SeamError> {
        if peer.id == self.local {
            return Err(SeamError::Refused(
                "a node does not sync with itself".to_string(),
            ));
        }
        let outcome = exchange(self, peer).await;
        let session_open = self.session_peers().contains(&peer.id);
        let mut views = self.views.lock().unwrap_or_else(|e| e.into_inner());
        match outcome {
            Ok(moved) => {
                views.record_success(peer.id, now(), moved.received, moved.sent);
                let view = views
                    .view(&peer.id, session_open)
                    .expect("a peer just recorded has a view");
                Ok(view)
            }
            Err(error) => {
                views.record_error(peer.id, now(), error.to_string());
                Err(error)
            }
        }
    }

    /// One round: choose the peers, run at most [`CONCURRENT_PEERS_MAX`]
    /// exchanges at once, and let every failure land on its peer's view.
    async fn sync_now(&self) -> Result<(), SeamError> {
        let nodes = self.roster.nodes().await?;
        let expelled: HashSet<NodeId> = self.store.expelled().await?.into_iter().collect();
        let sessions = self.transport.sessions();
        let peers = choose_peers(
            self.local,
            &nodes,
            &sessions,
            &expelled,
            self.peers_per_round_max,
        );
        assert!(peers.len() <= self.peers_per_round_max);
        let gate = Semaphore::new(CONCURRENT_PEERS_MAX);
        let exchanges = peers.iter().map(|peer| async {
            // The gate is never closed, so acquiring cannot fail.
            let _permit = gate.acquire().await.expect("the concurrency gate stays open");
            if let Err(e) = self.sync_with(peer).await {
                tracing::debug!(peer = %peer.id.short(), "sync exchange failed: {e}");
            }
        });
        join_all(exchanges).await;
        Ok(())
    }

    async fn status(&self) -> Result<SyncStatus, SeamError> {
        let vector = self.store.version_vector(&self.local).await?;
        let sessions = self.session_peers();
        let peers = self
            .views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .views(&sessions);
        Ok(SyncStatus { vector, peers })
    }
}

#[async_trait::async_trait]
impl Synchronizer for Service {
    async fn sync_now(&self) -> Result<SyncStatus, SeamError> {
        self.inner.sync_now().await?;
        self.inner.status().await
    }

    async fn sync_with(&self, peer: &PeerAddress) -> Result<PeerSyncView, SeamError> {
        self.inner.sync_with(peer).await
    }

    async fn status(&self) -> Result<SyncStatus, SeamError> {
        self.inner.status().await
    }
}
