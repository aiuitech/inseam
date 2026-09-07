//! The `connections` seam end to end (`design/connections.md`): a node
//! stewards several hosts at once, each connection plugin registering its
//! host as an effect; the sweep and operations resolve by host; a scope
//! that names no host is only accepted while exactly one host is mounted.
//! Two filesystem entries with distinct host ids stand in for "a filesystem
//! and a mailbox" — to the registry they are two hosts, which is the point.

mod common;

use inseam_kernel::address::HostId;
use inseam_seams::SeamError;
use inseam_seams::connection::CONNECTIONS;
use inseam_seams::operations::IndexRequest;

fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.md"), "# Alpha\n\nzebras graze\n").expect("write");
    dir
}

const TWO_HOSTS: &str = r#"
[[entry]]
id = "fs"
plugin = "connection-fs"
[entry.config]
host_id = "fs-one"

[[entry]]
id = "fs-two"
plugin = "connection-fs"
[entry.config]
host_id = "fs-two"
"#;

#[tokio::test]
async fn a_node_stewards_several_hosts_and_scopes_are_explicit() {
    let data = tempfile::tempdir().expect("tempdir");
    let corpus = corpus();
    let kernel = common::boot(data.path(), TWO_HOSTS).await;
    let ops = common::ops(&kernel);

    let hosts = ops.hosts().await.expect("lists");
    let ids: Vec<&str> = hosts.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, vec!["fs-one", "fs-two"], "ordered by host id");
    assert_eq!(hosts[0].entry, "fs");
    assert_eq!(hosts[1].entry, "fs-two");
    assert!(
        hosts
            .iter()
            .all(|h| h.capabilities.enumerates && !h.capabilities.writable)
    );

    // Two hosts mounted: a scope without a host is refused by name.
    let ambiguous = ops
        .index(IndexRequest {
            host: None,
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await;
    assert!(matches!(ambiguous, Err(SeamError::AmbiguousHost(h)) if h.len() == 2));

    // Naming the host sweeps through that host's connection only.
    let report = ops
        .index(IndexRequest {
            host: Some(HostId::new("fs-two").expect("valid")),
            root: corpus.path().display().to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes");
    assert_eq!(report.indexed, 2, "the note and the folder holding it");
    let two = HostId::new("fs-two").expect("valid");
    let one = HostId::new("fs-one").expect("valid");
    assert_eq!(
        kernel
            .store()
            .sources_of_host(&two)
            .await
            .expect("ok")
            .len(),
        2
    );
    assert!(
        kernel
            .store()
            .sources_of_host(&one)
            .await
            .expect("ok")
            .is_empty()
    );

    // A host nobody stewards is unknown, not a crash.
    let unknown = ops
        .index(IndexRequest {
            host: Some(HostId::new("gmail-nobody").expect("valid")),
            root: "INBOX".to_string(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await;
    assert!(matches!(unknown, Err(SeamError::UnknownHost(_))));
}

#[tokio::test]
async fn a_second_connection_to_the_same_host_fails_its_own_fiber_only() {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(
        data.path(),
        r#"
[[entry]]
id = "fs"
plugin = "connection-fs"
[entry.config]
host_id = "shared"

[[entry]]
id = "fs-dup"
plugin = "connection-fs"
[entry.config]
host_id = "shared"
"#,
    )
    .await;
    let registered = kernel.service(&CONNECTIONS).expect("bound").snapshot();
    assert_eq!(registered.len(), 1, "one steward per host per node");
    let states: Vec<(String, bool)> = kernel
        .fibers()
        .iter()
        .filter(|f| f.id == "fs" || f.id == "fs-dup")
        .map(|f| {
            (
                f.id.clone(),
                matches!(f.state, inseam_kernel::substrate::FiberState::Failed(_)),
            )
        })
        .collect();
    assert!(
        states.contains(&("fs".to_string(), false)),
        "the first registration stands"
    );
    assert!(
        states.contains(&("fs-dup".to_string(), true)),
        "the duplicate fails alone"
    );
    // Everything downstream of the registry still runs.
    let ops = common::ops(&kernel);
    assert_eq!(ops.hosts().await.expect("lists").len(), 1);
}

#[tokio::test]
async fn unmounting_a_connection_unregisters_its_host() {
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = common::boot(data.path(), TWO_HOSTS).await;
    assert_eq!(common::ops(&kernel).hosts().await.expect("lists").len(), 2);
    common::reconcile(
        &mut kernel,
        &TWO_HOSTS.replace(
            "id = \"fs-two\"\nplugin = \"connection-fs\"",
            "id = \"fs-two\"\nplugin = \"connection-fs\"\ndisabled = true",
        ),
    )
    .await;
    let hosts = common::ops(&kernel).hosts().await.expect("lists");
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id.as_str(), "fs-one");
}
