//! The `roster` plugin: the seam's provider and the only writer of this
//! node's own roster records (`design/roster.md`). It publishes the node
//! record with the transport's current endpoints, one host record and one
//! stewardship record per connection the registry holds, and withdrawals
//! for hosts that go away; it answers the typed view over the roster
//! tables the store materializes; and it is the transport's admission
//! policy — expelled peers are refused, roster peers admitted, and
//! strangers admitted only with a one-time invitation token.
//!
//! Publishing happens at apply and then from one reconciler task: the
//! connections registry announces changes synchronously, so the listener
//! only wakes the task, and a timer wakes it too so an endpoint rotation
//! the transport reports is republished without anyone asking. Admission
//! is synchronous on the transport's accept path, so it answers from an
//! in-memory view the reconciler refreshes from the store after every
//! publish and whenever [`RosterChanged`] fires.
//!
//! What was published last is remembered in memory only, so a restart
//! republishes every record once; the log compacts per key, so that costs
//! one entry per record, not growth. Open invitations are in memory too
//! and do not survive a restart.

mod invitations;
mod stewardships;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use inseam_kernel::address::{HostId, Timestamp};
use inseam_kernel::network::{
    Endpoint, HostRecord, NodeId, NodeRecord, Record, StewardshipRecord,
};
use inseam_kernel::store::IndexStore;
use inseam_kernel::substrate::{
    parse_config, ApplyCx, EventBus, Facts, Inject, Manifest, Plugin, PluginError,
    PluginFactory, STORE,
};
use inseam_seams::connection::{Connections, ConnectionsChanged, CONNECTIONS};
use inseam_seams::node::{Node, NODE};
use inseam_seams::roster::{
    HostStewards, Invitation, Roster, RosterChanged, INVITATIONS_OPEN_MAX, ROSTER,
};
use inseam_seams::transport::{
    admission_as_effect, Admission, Admit, InvitationToken, Transport, TRANSPORT,
};
use inseam_seams::SeamError;

use invitations::OpenInvitations;
use stewardships::{plan, Publication};

pub use stewardships::HOSTS_PER_NODE_MAX;

/// How often the reconciler re-reads the transport's endpoints when no
/// event woke it: a rotation is republished within this long.
pub const ENDPOINT_POLL_SECS_DEFAULT: u32 = 30;
/// Reconciler passes before the loop's assertion fires. The loop is meant
/// to run for the node's lifetime; at one pass per second this is longer
/// than any process lives, so reaching it is a bug, not a long uptime.
const RECONCILE_TICKS_MAX: u64 = 1 << 40;
/// Most peers admitted by invitation whose node records have not arrived
/// yet. Each came through a redeemed token, and the record arrives in the
/// first exchange, so the set stays small; a full set forgets the oldest.
const INVITED_PENDING_MAX: usize = INVITATIONS_OPEN_MAX;
const _: () = assert!(INVITED_PENDING_MAX > 0, "an invited peer must be remembered");

/// Fact keys the provider declares on the `roster` binding.
pub mod facts {
    /// This node's id as 64 hex characters.
    pub const ID: &str = "id";
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RosterConfig {
    /// Seconds between timer-driven reconciler passes; each republishes
    /// the node record when the transport's endpoints changed.
    pub endpoint_poll_secs: u32,
}

impl Default for RosterConfig {
    fn default() -> Self {
        Self {
            endpoint_poll_secs: ENDPOINT_POLL_SECS_DEFAULT,
        }
    }
}

pub struct RosterPlugin {
    config: RosterConfig,
}

impl RosterPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let config: RosterConfig = parse_config(config)?;
        if config.endpoint_poll_secs == 0 {
            return Err(PluginError(
                "config: endpoint_poll_secs must be at least 1; zero would spin".to_string(),
            ));
        }
        Ok(Self { config })
    }
}

pub struct RosterFactory;

impl PluginFactory for RosterFactory {
    fn name(&self) -> &str {
        "roster"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(RosterPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for RosterPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[
            Inject::required("store"),
            Inject::required("node"),
            Inject::required("transport"),
            Inject::required("connections"),
        ];
        Manifest {
            name: "roster",
            inject: INJECT,
            provides: &["roster"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let node = cx.get(&NODE)?;
        let transport = cx.get(&TRANSPORT)?;
        let local = node.id();
        assert_eq!(
            local,
            transport.local_id(),
            "the node seam and the transport agree on this node's identity"
        );
        let inner = Arc::new(Inner {
            local,
            node,
            transport,
            store: cx.get(&STORE)?,
            connections: cx.get(&CONNECTIONS)?,
            bus: cx.bus().clone(),
            published: RwLock::new(Published::default()),
            cache: RwLock::new(AdmissionCache::default()),
            invitations: Mutex::new(OpenInvitations::new()),
            wake: Notify::new(),
        });
        // The first pass runs inline so the node's records are in the log
        // before any peer can ask, and before the reconciler exists.
        inner
            .reconcile()
            .await
            .map_err(|e| PluginError(format!("publishing this node's roster records: {e}")))?;
        let service = Arc::new(Service {
            inner: Arc::clone(&inner),
        });
        admission_as_effect(cx, Arc::clone(&service) as Arc<dyn Admission>)?;
        keep_wakers(cx, &inner);
        let poll = Duration::from_secs(u64::from(self.config.endpoint_poll_secs));
        let task = tokio::spawn(reconcile_loop(Arc::clone(&inner), poll));
        cx.effect("abort the roster reconciler", move || task.abort());
        cx.provide(
            &ROSTER,
            service as Arc<dyn Roster>,
            Facts::new().with(facts::ID, local.to_hex()),
        )?;
        Ok(())
    }
}

/// Subscribe the reconciler's wake to both events it reacts to. The
/// listeners do nothing but wake: the registry emits synchronously from
/// inside its own registration, so no store work may happen inline.
fn keep_wakers(cx: &mut ApplyCx<'_>, inner: &Arc<Inner>) {
    let on_connections = Arc::clone(inner);
    let subscription = cx
        .bus()
        .on::<ConnectionsChanged>(move |_| on_connections.wake.notify_one());
    cx.keep("wake the roster reconciler on connection changes", subscription);
    let on_roster = Arc::clone(inner);
    let subscription = cx
        .bus()
        .on::<RosterChanged>(move |_| on_roster.wake.notify_one());
    cx.keep("refresh the admission view on roster changes", subscription);
}

/// The reconciler: sleep until woken or the poll elapses, then run one
/// pass. A wake that lands mid-pass is kept as a permit, so no change is
/// lost, and a burst collapses into one pass.
async fn reconcile_loop(inner: Arc<Inner>, poll: Duration) {
    for _tick in 0..RECONCILE_TICKS_MAX {
        tokio::select! {
            () = inner.wake.notified() => {}
            () = tokio::time::sleep(poll) => {}
        }
        if let Err(e) = inner.reconcile().await {
            tracing::warn!("roster reconcile pass failed: {e}");
        }
    }
    unreachable!("the roster reconciler runs for the node's lifetime; {RECONCILE_TICKS_MAX} passes is longer than any process");
}

/// The seam provider and the admission policy, one object: the policy's
/// answers come from the same view the provider maintains.
pub struct Service {
    inner: Arc<Inner>,
}

struct Inner {
    local: NodeId,
    node: Arc<dyn Node>,
    transport: Arc<dyn Transport>,
    store: Arc<IndexStore>,
    connections: Arc<dyn Connections>,
    bus: EventBus,
    published: RwLock<Published>,
    cache: RwLock<AdmissionCache>,
    invitations: Mutex<OpenInvitations>,
    wake: Notify,
}

/// What this node last published about itself and its hosts — the
/// baseline each reconcile pass diffs against.
#[derive(Default)]
struct Published {
    /// `None` until the first node record is published, which apply does
    /// before anything else can observe the service.
    node: Option<NodeRecord>,
    hosts: HashMap<HostId, Publication>,
}

/// The in-memory answer to "who may connect": refreshed from the store,
/// plus the peers a redeemed invitation admitted whose own records have
/// not arrived yet.
#[derive(Default)]
struct AdmissionCache {
    known: HashSet<NodeId>,
    expelled: HashSet<NodeId>,
    invited: Vec<NodeId>,
}

impl AdmissionCache {
    fn remember_invited(&mut self, peer: NodeId) {
        if self.invited.contains(&peer) {
            return;
        }
        if self.invited.len() >= INVITED_PENDING_MAX {
            self.invited.remove(0);
        }
        self.invited.push(peer);
        assert!(self.invited.len() <= INVITED_PENDING_MAX);
    }

    /// Replace the store-derived sets and forget invited peers the store
    /// now knows one way or the other.
    fn replace(&mut self, known: HashSet<NodeId>, expelled: HashSet<NodeId>) {
        assert!(
            known.is_disjoint(&expelled),
            "the store lists a node as a roster member or as expelled, never both"
        );
        self.invited
            .retain(|id| !known.contains(id) && !expelled.contains(id));
        self.known = known;
        self.expelled = expelled;
    }
}

fn now() -> Timestamp {
    Timestamp::from(SystemTime::now())
}

impl Inner {
    /// One pass: republish the node record if the endpoints rotated,
    /// publish and withdraw stewardships against the registry, refresh
    /// the admission view, and announce if anything was published.
    async fn reconcile(&self) -> Result<(), SeamError> {
        let rotated = self.republish_node_if_rotated().await?;
        let stewardship_changes = self.reconcile_stewardships().await?;
        self.refresh_cache().await?;
        if rotated || stewardship_changes > 0 {
            self.bus.emit(&RosterChanged);
        }
        Ok(())
    }

    /// Publish the node record with `endpoints`, remembering it as the
    /// last published.
    async fn publish_node(&self, endpoints: Vec<Endpoint>) -> Result<NodeRecord, SeamError> {
        let record = self.node.record(endpoints);
        assert_eq!(record.id, self.local);
        self.store.publish(&Record::Node(record.clone())).await?;
        let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
        published.node = Some(record.clone());
        Ok(record)
    }

    /// True when a record was published because the transport's endpoints
    /// differ from the last published ones — or nothing was published yet.
    async fn republish_node_if_rotated(&self) -> Result<bool, SeamError> {
        let endpoints = self.transport.endpoints();
        let unchanged = {
            let published = self.published.read().unwrap_or_else(|e| e.into_inner());
            published
                .node
                .as_ref()
                .is_some_and(|record| record.endpoints == endpoints)
        };
        if unchanged {
            return Ok(false);
        }
        self.publish_node(endpoints).await?;
        Ok(true)
    }

    /// Publish what the registry holds that the log does not, withdraw
    /// what it no longer holds; returns how many records were published.
    async fn reconcile_stewardships(&self) -> Result<u32, SeamError> {
        let snapshot = self.connections.snapshot();
        let plan = {
            let published = self.published.read().unwrap_or_else(|e| e.into_inner());
            plan(self.local, &snapshot, &published.hosts)
        };
        if plan.is_empty() {
            return Ok(0);
        }
        let mut count: u32 = 0;
        for publication in &plan.publish {
            self.store
                .publish(&Record::Host(publication.host.clone()))
                .await?;
            self.store
                .publish(&Record::Stewardship(publication.stewardship.clone()))
                .await?;
            let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
            published
                .hosts
                .insert(publication.host.id.clone(), publication.clone());
            count += 2;
        }
        for host in &plan.withdraw {
            self.store
                .publish(&Record::StewardshipWithdrawn {
                    node: self.local,
                    host: host.clone(),
                })
                .await?;
            let mut published = self.published.write().unwrap_or_else(|e| e.into_inner());
            published.hosts.remove(host);
            count += 1;
        }
        Ok(count)
    }

    async fn refresh_cache(&self) -> Result<(), SeamError> {
        let nodes = self.store.roster_nodes().await?;
        let expelled = self.store.expelled().await?;
        let mut known: HashSet<NodeId> = nodes.into_iter().map(|record| record.id).collect();
        // The local record is in the store after the first pass, but the
        // node admits itself regardless: its own key is never a stranger.
        known.insert(self.local);
        let expelled: HashSet<NodeId> = expelled.into_iter().collect();
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        cache.replace(known, expelled);
        Ok(())
    }

    fn redeem(&self, peer: &NodeId, token: &InvitationToken) -> bool {
        let redeemed = {
            let mut invitations = self.invitations.lock().unwrap_or_else(|e| e.into_inner());
            invitations.redeem(token, now())
        };
        if redeemed {
            let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
            cache.remember_invited(*peer);
        }
        redeemed
    }

    /// The policy: expelled first, so an expelled peer's stale record can
    /// never readmit it; then roster members and invited peers; then a
    /// stranger with a live token, admitted as it is redeemed.
    fn admit(&self, peer: &NodeId, invitation: Option<&InvitationToken>) -> Admit {
        if *peer == self.local {
            return Admit::Refused("a node never admits its own key as a peer".to_string());
        }
        {
            let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
            if cache.expelled.contains(peer) {
                return Admit::Refused(format!("node {} was expelled", peer.short()));
            }
            if cache.known.contains(peer) {
                return Admit::Admitted;
            }
            if cache.invited.contains(peer) {
                return Admit::Admitted;
            }
        }
        match invitation {
            None => Admit::Refused(format!(
                "node {} is not in the roster and presented no invitation",
                peer.short()
            )),
            Some(token) if self.redeem(peer, token) => Admit::Admitted,
            Some(_) => Admit::Refused(format!(
                "node {} presented an invitation that is not open",
                peer.short()
            )),
        }
    }
}

impl Admission for Service {
    fn admit(&self, peer: &NodeId, invitation: Option<&InvitationToken>) -> Admit {
        self.inner.admit(peer, invitation)
    }
}

#[async_trait::async_trait]
impl Roster for Service {
    fn local(&self) -> NodeRecord {
        let published = self.inner.published.read().unwrap_or_else(|e| e.into_inner());
        published
            .node
            .clone()
            .expect("apply published the node record before providing the roster")
    }

    async fn nodes(&self) -> Result<Vec<NodeRecord>, SeamError> {
        Ok(self.inner.store.roster_nodes().await?)
    }

    async fn node(&self, id: &NodeId) -> Result<Option<NodeRecord>, SeamError> {
        let nodes = self.inner.store.roster_nodes().await?;
        Ok(nodes.into_iter().find(|record| record.id == *id))
    }

    async fn hosts(&self) -> Result<Vec<HostStewards>, SeamError> {
        let hosts = self.inner.store.roster_hosts().await?;
        let stewardships = self.inner.store.roster_stewardships().await?;
        Ok(join_hosts(hosts, stewardships))
    }

    async fn stewards_of(&self, host: &HostId) -> Result<Vec<StewardshipRecord>, SeamError> {
        Ok(self.inner.store.stewards_of(host).await?)
    }

    async fn is_admitted(&self, id: &NodeId) -> Result<bool, SeamError> {
        // The store, not the cache: this is the authoritative answer, and
        // the pair to the cache the synchronous policy answers from.
        let nodes = self.inner.store.roster_nodes().await?;
        Ok(nodes.iter().any(|record| record.id == *id))
    }

    async fn republish(&self) -> Result<NodeRecord, SeamError> {
        let endpoints = self.inner.transport.endpoints();
        let record = self.inner.publish_node(endpoints).await?;
        self.inner.refresh_cache().await?;
        self.inner.bus.emit(&RosterChanged);
        Ok(record)
    }

    async fn invite(&self) -> Result<Invitation, SeamError> {
        let (token, expires) = {
            let mut invitations = self.inner.invitations.lock().unwrap_or_else(|e| e.into_inner());
            invitations.mint(now())?
        };
        let invitation = Invitation {
            node: self.inner.local,
            endpoints: self.inner.transport.endpoints(),
            token,
            expires,
        };
        invitation
            .check_bounds()
            .map_err(|e| SeamError::failed(format!("minted invitation: {e}")))?;
        Ok(invitation)
    }

    fn redeem(&self, peer: &NodeId, token: &InvitationToken) -> bool {
        self.inner.redeem(peer, token)
    }

    async fn expel(&self, node: &NodeId) -> Result<(), SeamError> {
        if *node == self.inner.local {
            return Err(SeamError::Refused(
                "a node cannot expel itself; expel it from another node".to_string(),
            ));
        }
        self.inner
            .store
            .publish(&Record::Expulsion { node: *node })
            .await?;
        self.inner.transport.disconnect(node);
        self.inner.refresh_cache().await?;
        {
            let cache = self.inner.cache.read().unwrap_or_else(|e| e.into_inner());
            assert!(cache.expelled.contains(node), "an expulsion just published is in the view");
            assert!(!cache.known.contains(node));
        }
        self.inner.bus.emit(&RosterChanged);
        Ok(())
    }
}

/// Pair each host with the claims on it. Both listings come ordered by
/// host, so this is one merge, and a host nobody claims keeps an empty
/// steward list — unreachable but still known.
fn join_hosts(hosts: Vec<HostRecord>, stewardships: Vec<StewardshipRecord>) -> Vec<HostStewards> {
    let mut by_host: HashMap<HostId, Vec<StewardshipRecord>> = HashMap::new();
    for stewardship in stewardships {
        by_host
            .entry(stewardship.host.clone())
            .or_default()
            .push(stewardship);
    }
    hosts
        .into_iter()
        .map(|host| {
            let stewards = by_host.remove(&host.id).unwrap_or_default();
            HostStewards { host, stewards }
        })
        .collect()
}

#[cfg(test)]
mod tests;
