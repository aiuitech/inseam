//! The Google Workspace connection end to end through the kernel
//! (`design/connections.md`): the `google` entry registers its own grant,
//! waits without parking anything while the client identity or the
//! authorization is missing, and — the moment the grant is authorized —
//! stewards one host per service, derived from the signed-in account;
//! revoking withdraws them, and a later authorization brings them back
//! without a restart. A second entry for the same account fails alone.
//!
//! The client identity is an environment variable, so the tests that need
//! one set it once, before any kernel boots in this binary.

mod common;

use std::sync::Once;

use inseam_kernel::substrate::FiberState;
use inseam_seams::connection::{HostKind, derive_host_id};
use inseam_seams::oauth::{GrantChanged, GrantId, GrantState};
use inseam_seams::operations::RevokeGrantRequest;

const CLIENT_ID_ENV: &str = "INSEAM_TEST_GOOGLE_CLIENT_ID";
const CLIENT_SECRET_ENV: &str = "INSEAM_TEST_GOOGLE_CLIENT_SECRET";

static CLIENT: Once = Once::new();

/// Put the test client identity in the environment, once. `set_var` is
/// unsafe because another thread may be reading the environment; here the
/// only readers are kernels booted by these tests, every one of which calls
/// this first and blocks on the `Once` until it is done.
fn ensure_client() {
    CLIENT.call_once(|| {
        // SAFETY: see above — no kernel in this binary reads the environment
        // before this `Once` has completed.
        unsafe {
            std::env::set_var(CLIENT_ID_ENV, "client-1.apps.googleusercontent.com");
            std::env::set_var(CLIENT_SECRET_ENV, "s3cret");
        }
    });
}

fn google_entry(id: &str, grant: &str) -> String {
    format!(
        r#"
[[entry]]
id = "oauth"
plugin = "oauth"

[[entry]]
id = "{id}"
plugin = "connection-google"
[entry.config]
grant = "{grant}"
client_id_env = "{CLIENT_ID_ENV}"
client_secret_env = "{CLIENT_SECRET_ENV}"
"#
    )
}

/// A version-2 credential file as the oauth plugin writes one, naming the
/// account the owner signed in as.
fn write_credentials(data_dir: &std::path::Path, grant: &str, account: &str) {
    let dir = data_dir.join("oauth");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let tokens = serde_json::json!({
        "version": 2,
        "access_token": "at",
        "refresh_token": "rt",
        "expires_at": null,
        "scopes": ["openid"],
        "token_type": "Bearer",
        "account": account,
    });
    std::fs::write(dir.join(format!("{grant}.json")), tokens.to_string()).expect("write");
}

fn google_host_ids(hosts: &[inseam_seams::operations::HostView]) -> Vec<String> {
    hosts
        .iter()
        .filter(|h| h.kind.as_str() != "fs")
        .map(|h| h.id.to_string())
        .collect()
}

#[tokio::test]
async fn the_google_entry_waits_for_its_client_without_parking_anything() {
    let data = tempfile::tempdir().expect("tempdir");
    let kernel = common::boot(
        data.path(),
        &google_entry("google", "google").replace(CLIENT_ID_ENV, "INSEAM_TEST_GOOGLE_NEVER_SET"),
    )
    .await;
    let google = kernel
        .fibers()
        .into_iter()
        .find(|f| f.id == "google")
        .expect("google fiber");
    assert!(
        matches!(google.state, FiberState::Active),
        "a missing client parks nothing"
    );
    let ops = common::ops(&kernel);
    let grants = ops.grants().await.expect("lists");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].id.as_str(), "google");
    assert_eq!(grants[0].provider, "accounts.google.com");
    assert_eq!(
        grants[0].state,
        GrantState::MissingSecret {
            env: "INSEAM_TEST_GOOGLE_NEVER_SET".to_string()
        }
    );
    assert!(
        grants[0]
            .scopes
            .iter()
            .any(|s| s.ends_with("gmail.readonly"))
    );
    assert!(google_host_ids(&ops.hosts().await.expect("lists")).is_empty());
}

#[tokio::test]
async fn an_authorized_grant_stewards_a_host_per_service_until_revoked() {
    ensure_client();
    let data = tempfile::tempdir().expect("tempdir");
    write_credentials(data.path(), "google", "Greg@Example.com");
    let mut kernel = common::boot(data.path(), &google_entry("google", "google")).await;
    let ops = common::ops(&kernel);

    let grants = ops.grants().await.expect("lists");
    assert_eq!(grants[0].state.account(), Some("Greg@Example.com"));

    let hosts = ops.hosts().await.expect("lists");
    let google_hosts: Vec<_> = hosts.iter().filter(|h| h.kind.as_str() != "fs").collect();
    assert_eq!(google_hosts.len(), 5, "one host per service");
    let gmail = derive_host_id(&HostKind::new("gmail").expect("valid"), "greg@example.com");
    let gmail_host = google_hosts
        .iter()
        .find(|h| h.id == gmail)
        .expect("the Gmail host id is derived from the account");
    assert_eq!(gmail_host.entry, "google");
    assert_eq!(gmail_host.display_name, "Gmail · Greg@Example.com");
    assert!(gmail_host.capabilities.enumerates);
    assert!(!gmail_host.capabilities.writable);
    let kinds: Vec<&str> = google_hosts.iter().map(|h| h.kind.as_str()).collect();
    for kind in [
        "gmail",
        "google-drive",
        "google-calendar",
        "google-contacts",
        "google-tasks",
    ] {
        assert!(kinds.contains(&kind), "{kind} is stewarded");
    }

    // Revoking withdraws the hosts; nothing restarts.
    let revoked = ops
        .revoke_grant(RevokeGrantRequest {
            grant: GrantId::new("google").expect("valid"),
        })
        .await
        .expect("revokes");
    assert_eq!(revoked.state, GrantState::Unauthorized);
    assert!(google_host_ids(&ops.hosts().await.expect("lists")).is_empty());
    assert!(!data.path().join("oauth/google.json").exists());

    // A later authorization — from any transport — brings them back, by
    // the event the oauth provider emits.
    kernel.bus().emit(&GrantChanged {
        grant: GrantId::new("google").expect("valid"),
        state: GrantState::Authorized {
            expires_at: None,
            scopes: Vec::new(),
            account: Some("greg@example.com".to_string()),
        },
    });
    assert_eq!(google_host_ids(&ops.hosts().await.expect("lists")).len(), 5);
    kernel.shutdown().await;
}

#[tokio::test]
async fn a_second_entry_for_the_same_account_fails_alone() {
    ensure_client();
    let data = tempfile::tempdir().expect("tempdir");
    write_credentials(data.path(), "google", "greg@example.com");
    write_credentials(data.path(), "google-two", "greg@example.com");
    let overlay = format!(
        "{}\n[[entry]]\nid = \"google-two\"\nplugin = \"connection-google\"\n[entry.config]\ngrant = \"google-two\"\nclient_id_env = \"{CLIENT_ID_ENV}\"\nclient_secret_env = \"{CLIENT_SECRET_ENV}\"\n",
        google_entry("google", "google")
    );
    let mut kernel = common::boot_unsettled(data.path(), &overlay).await;
    let states: Vec<(String, bool)> = kernel
        .fibers()
        .iter()
        .filter(|f| f.id.starts_with("google"))
        .map(|f| (f.id.clone(), matches!(f.state, FiberState::Failed(_))))
        .collect();
    assert!(
        states.contains(&("google".to_string(), false)),
        "{states:?}"
    );
    assert!(
        states.contains(&("google-two".to_string(), true)),
        "{states:?}"
    );
    let ops = common::ops(&kernel);
    assert_eq!(
        google_host_ids(&ops.hosts().await.expect("lists")).len(),
        5,
        "registered once"
    );
    kernel.shutdown().await;
}
