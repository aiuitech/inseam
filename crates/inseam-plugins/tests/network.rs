//! Two nodes end to end (`design/roster.md`, `design/address-sync.md`,
//! `design/discovery.md`): full kernels in one process, real iroh
//! transports with no relay, each with its own data directory and its own
//! filesystem host. A stewards and indexes two files; B joins through an
//! invitation and, from then on, sees A's host in its roster, A's sources
//! in its catalog, and reads, expands, and queries them through A. A
//! spent invitation admits nobody, a stranger is refused, a removed source
//! is forgotten on the next sync, and an expelled node's next sync fails.
//!
//! Real sockets are involved, so the suite waits for the transport's
//! address to be published with a short sleep between bounded attempts —
//! the one place a sleep belongs.

mod common;

use std::sync::Arc;
use std::time::Duration;

use inseam_kernel::address::Address;
use inseam_kernel::network::NodeId;
use inseam_kernel::substrate::Kernel;
use inseam_seams::operations::{
    CatalogFilter, CatalogRequest, ExpandRequest, ExpelRequest, FetchRequest, IndexRequest,
    JoinRequest, NetworkView, Operations, QueryRequest, ScanRequest,
};
use inseam_seams::roster::Invitation;
use inseam_seams::transport::InvitationToken;
use inseam_seams::SeamError;

/// Attempts a wait makes before the test fails: with the interval below,
/// five seconds — far past the roster's one-second endpoint poll.
const POLL_ATTEMPTS_MAX: u32 = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

const ALPHA_TEXT: &str = "# Alpha\n\nzebras graze the savanna at dawn\n";
const BETA_TEXT: &str = "# Beta\n\nowls hunt the meadow at night\n";

/// The network entries over the offline base, named per node so two nodes
/// on one machine derive two filesystem hosts and two display names. The
/// transport is direct-only on an ephemeral port; the roster re-reads the
/// transport's address every second; the sync timer is pushed out of the
/// test's way so every exchange here is one the test asked for.
fn overlay(name: &str) -> String {
    format!(
        r#"
[[entry]]
id = "fs"
plugin = "connection-fs"
[entry.config]
machine_id = "{name}"

[[entry]]
id = "node"
plugin = "node"
[entry.config]
display_name = "{name}"

[[entry]]
id = "transport"
plugin = "transport-iroh"
[entry.config]
relay = "none"
bind_port = 0
request_timeout_secs = 5
idle_timeout_secs = 30

[[entry]]
id = "roster"
plugin = "roster"
[entry.config]
endpoint_poll_secs = 1

[[entry]]
id = "sync"
plugin = "sync"
[entry.config]
interval_secs = 3600
initial_delay_secs = 3600

[[entry]]
id = "routing"
plugin = "routing"
[entry.config]
fan_out_timeout_ms = 3000
"#
    )
}

/// One booted node with its data directory kept alive beside it.
struct Node {
    kernel: Kernel,
    ops: Arc<dyn Operations>,
    id: NodeId,
    _data: tempfile::TempDir,
}

async fn boot(name: &str) -> Node {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(data.path(), &overlay(name)).await;
    let ops = common::ops(&kernel);
    let id = ops.network().await.expect("the network view").local.id;
    Node {
        kernel,
        ops,
        id,
        _data: data,
    }
}

/// Poll a node's network view until `settled` accepts it.
async fn network_when(
    ops: &dyn Operations,
    what: &str,
    settled: impl Fn(&NetworkView) -> bool,
) -> NetworkView {
    for _attempt in 0..POLL_ATTEMPTS_MAX {
        let view = ops.network().await.expect("the network view");
        if settled(&view) {
            return view;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("{what} did not happen within {:?}", POLL_INTERVAL * POLL_ATTEMPTS_MAX);
}

fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("alpha.md"), ALPHA_TEXT).expect("write");
    std::fs::write(dir.path().join("beta.md"), BETA_TEXT).expect("write");
    dir
}

async fn index(node: &Node, root: &std::path::Path) -> inseam_seams::sweep::IndexReport {
    node.ops
        .index(IndexRequest {
            host: None,
            root: root.display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes")
}

/// Every cataloged address on `node`, with the origin each row names.
async fn catalog(node: &Node) -> Vec<(Address, Option<NodeId>)> {
    node.ops
        .catalog(CatalogRequest {
            host: None,
            filter: CatalogFilter::All,
            limit: 100,
        })
        .await
        .expect("lists")
        .entries
        .into_iter()
        .map(|entry| (entry.address, entry.origin))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_share_a_catalog_and_serve_each_other() {
    let corpus = corpus();
    let mut a = boot("node-a").await;
    let mut b = boot("node-b").await;
    let mut c = boot("node-c").await;
    let alpha = common::address_of(&a.kernel, &corpus.path().join("alpha.md"));
    let beta = common::address_of(&a.kernel, &corpus.path().join("beta.md"));
    let a_host = alpha.host.clone();
    assert_ne!(a.id, b.id);
    assert_ne!(
        common::address_of(&b.kernel, corpus.path()).host,
        a_host,
        "two machine ids, two hosts"
    );

    let report = index(&a, corpus.path()).await;
    assert_eq!(report.indexed, 3, "two notes and the folder holding them");

    // The transport publishes its address a moment after it binds; the
    // roster republishes A's record once it sees one.
    let published = network_when(a.ops.as_ref(), "A publishing its endpoints", |view| {
        !view.local.endpoints.is_empty()
    })
    .await;
    assert_eq!(published.nodes.len(), 1, "A knows only itself");
    assert_eq!(published.hosts[0].stewards, vec![a.id]);

    let invitation = a.ops.invite().await.expect("mints");
    assert_eq!(invitation.node, a.id);
    assert!(!invitation.endpoints.is_empty());
    let text = invitation.to_string();

    // B joins: one exchange carries A's roster and catalog to B and B's
    // own records back to A.
    let joined = b
        .ops
        .join(JoinRequest {
            invitation: text.clone(),
        })
        .await
        .expect("joins");
    assert_b_sees_a(&joined, &a, &a_host);
    let on_a = a.ops.network().await.expect("view");
    assert!(on_a.nodes.iter().any(|node| node.record.id == b.id), "A admitted B");
    assert!(on_a.log.origins >= 2, "A holds B's log beside its own");

    let cataloged = catalog(&b).await;
    assert!(cataloged.contains(&(alpha.clone(), Some(a.id))), "{cataloged:?}");
    assert!(cataloged.contains(&(beta.clone(), Some(a.id))), "{cataloged:?}");
    let remote = b.ops.status().await.expect("status").remote_sources;
    assert_eq!(remote, 3, "the two notes and their folder, all from A");

    assert_b_reads_through_a(&b, &a, &alpha).await;
    assert_spent_and_forged_invitations_are_refused(&c, &a, &invitation, &text).await;
    assert_removal_reaches_b(&a, &b, corpus.path(), &alpha, &beta).await;
    assert_expulsion_cuts_b_off(&a, &b).await;

    a.kernel.shutdown().await;
    b.kernel.shutdown().await;
    c.kernel.shutdown().await;
}

fn assert_b_sees_a(view: &NetworkView, a: &Node, a_host: &inseam_kernel::address::HostId) {
    let on_b = view
        .nodes
        .iter()
        .find(|node| node.record.id == a.id)
        .expect("B lists A");
    assert!(!on_b.is_local);
    assert!(on_b.live, "the join left a session with A");
    assert!(on_b.last_sync.is_some());
    assert_eq!(on_b.last_error, None);
    assert_eq!(on_b.record.display_name, "node-a");
    assert_eq!(on_b.hosts, vec![a_host.clone()]);
    let host = view
        .hosts
        .iter()
        .find(|host| host.host.id == *a_host)
        .expect("B knows A's host");
    assert_eq!(host.stewards, vec![a.id]);
    assert_eq!(host.host.kind, "fs");
    assert!(view.log.origins >= 2, "B holds A's log beside its own");
}

/// The ladder's rungs on B, every one served by A: B stewards nothing of
/// A's and indexed none of it.
async fn assert_b_reads_through_a(b: &Node, a: &Node, alpha: &Address) {
    let fetched = b
        .ops
        .fetch(FetchRequest {
            address: alpha.clone(),
        })
        .await
        .expect("fetches through A");
    assert_eq!(fetched.text, ALPHA_TEXT);

    let scanned = b
        .ops
        .scan(ScanRequest {
            address: alpha.clone(),
            start: 1,
            end: 2,
        })
        .await
        .expect("scans through A");
    assert_eq!(scanned.start, 1);
    assert_eq!(scanned.end, 2);
    assert_eq!(scanned.text, "# Alpha\n");
    assert_eq!(scanned.lines_total, Some(3));

    let expanded = b
        .ops
        .expand(ExpandRequest {
            address: alpha.clone(),
        })
        .await
        .expect("expands from A's index");
    assert_eq!(expanded.address, *alpha);
    assert!(!expanded.fragments.is_empty(), "A's index has the fragments");
    let texts: Vec<&str> = expanded
        .fragments
        .iter()
        .filter_map(|fragment| fragment.text.as_deref())
        .collect();
    assert!(texts.iter().any(|text| text.contains("zebras")), "{texts:?}");

    let response = b
        .ops
        .query(QueryRequest {
            text: "zebras".to_string(),
            limit: 8,
        })
        .await
        .expect("queries");
    let hit = response
        .results
        .iter()
        .find(|result| result.address == *alpha)
        .expect("A's note ranks on B");
    assert_eq!(hit.via, Some(a.id), "the result names the node whose index produced it");
    assert_eq!(response.meta.remote.len(), 1, "one node was fanned out to");
    assert_eq!(response.meta.remote[0].node, a.id);
    assert_eq!(response.meta.remote[0].error, None);
    assert!(response.meta.remote[0].results >= 1);
}

/// A token admits one node once: a second joiner presenting it is
/// refused, and so is one presenting a token A never minted.
async fn assert_spent_and_forged_invitations_are_refused(
    c: &Node,
    a: &Node,
    invitation: &Invitation,
    text: &str,
) {
    let spent = c
        .ops
        .join(JoinRequest {
            invitation: text.to_string(),
        })
        .await;
    assert!(matches!(spent, Err(SeamError::NotAdmitted(node)) if node == a.id), "{spent:?}");

    let forged = Invitation {
        node: a.id,
        endpoints: invitation.endpoints.clone(),
        token: InvitationToken::new("not-a-token-a-minted").expect("valid"),
        expires: invitation.expires,
    };
    let stranger = c
        .ops
        .join(JoinRequest {
            invitation: forged.to_string(),
        })
        .await;
    assert!(matches!(stranger, Err(SeamError::NotAdmitted(node)) if node == a.id), "{stranger:?}");

    let on_a = a.ops.network().await.expect("view");
    assert!(!on_a.nodes.iter().any(|node| node.record.id == c.id), "C never entered A's roster");
    let on_c = c.ops.network().await.expect("view");
    assert_eq!(on_c.nodes.len(), 1, "C learned nothing from A");
}

/// A source A no longer sees is withdrawn from B on the next sync: the
/// removal is a log entry like any other.
async fn assert_removal_reaches_b(
    a: &Node,
    b: &Node,
    corpus: &std::path::Path,
    alpha: &Address,
    beta: &Address,
) {
    std::fs::remove_file(corpus.join("beta.md")).expect("remove");
    let report = index(a, corpus).await;
    assert_eq!(report.removed, 1);
    let synced = b.ops.sync_now().await.expect("syncs");
    let with_a = synced
        .nodes
        .iter()
        .find(|node| node.record.id == a.id)
        .expect("A is listed");
    assert!(with_a.live, "the exchange with A succeeded");
    assert_eq!(with_a.last_error, None);
    let cataloged = catalog(b).await;
    assert!(cataloged.contains(&(alpha.clone(), Some(a.id))));
    assert!(!cataloged.iter().any(|(address, _)| address == beta), "{cataloged:?}");
}

/// Expelling B disconnects it now and refuses it from then on: B's next
/// round with A fails, and B is gone from A's roster.
async fn assert_expulsion_cuts_b_off(a: &Node, b: &Node) {
    let after = a
        .ops
        .expel(ExpelRequest { node: b.id })
        .await
        .expect("expels");
    assert!(!after.nodes.iter().any(|node| node.record.id == b.id), "B left A's roster");
    assert!(after.hosts.iter().all(|host| !host.stewards.contains(&b.id)), "B stewards nothing A knows");

    let synced = b.ops.sync_now().await.expect("the round runs; failures land on the peer");
    let with_a = synced
        .nodes
        .iter()
        .find(|node| node.record.id == a.id)
        .expect("B still lists A: the expulsion never reached it");
    assert!(!with_a.live);
    assert!(with_a.last_error.is_some(), "{with_a:?}");
}
