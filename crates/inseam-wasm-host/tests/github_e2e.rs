//! The connection seam across the boundary, end to end: the loaded GitHub
//! connection (`plugins/github`) stewards a repository served by a fake
//! GitHub on a loopback port, and the sweep, the finder, and the fetch
//! operations cannot tell it from a linked host. The network the component
//! reaches is the node's guarded fetch under the manifest's host allow list
//! — the staged manifest adds the loopback host, as an owner editing a
//! manifest would — and the OAuth bearer, when the entry names a grant, is
//! attached by the bridge and never seen by the component.
//!
//! Requires the GitHub artifact to be built (see `plugins/github/README.md`);
//! the tests skip with a notice when it is absent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use axum::{Json, Router};

use inseam_kernel::substrate::{
    ApplyCx, Composition, Facts, FiberState, Kernel, Manifest, Plugin, PluginError, PluginFactory,
};
use inseam_seams::SeamError;
use inseam_seams::oauth::{
    AccessToken, AuthorizationCallback, AuthorizationStarted, Grant, GrantDisposer, GrantId,
    GrantSpec, GrantState, OAUTH, OAuth, Redirect,
};
use inseam_seams::operations::{FetchRequest, IndexRequest, OPERATIONS, QueryRequest};
use inseam_wasm_host::WasmSchemeFactory;

fn github_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/github");
    dir.join("github.wasm")
        .exists()
        .then(|| dir.canonicalize().expect("canonicalizes"))
}

/// Stage the plugin directory with a manifest whose allow list also names
/// the loopback host the fake GitHub listens on.
fn stage(from: &Path, into: &Path, hosts: &[&str]) -> PathBuf {
    for file in ["github.wasm", "github.checks.toml"] {
        std::fs::copy(from.join(file), into.join(file)).expect("stages sidecar");
    }
    std::fs::create_dir_all(into.join("fixtures")).expect("fixture dir");
    for fixture in ["tree.json", "contents.json"] {
        std::fs::copy(
            from.join("fixtures").join(fixture),
            into.join("fixtures").join(fixture),
        )
        .expect("stages fixture");
    }
    write_manifest(into, hosts);
    into.join("github.wasm")
}

fn write_manifest(dir: &Path, hosts: &[&str]) {
    let hosts: Vec<String> = hosts.iter().map(|h| format!("{h:?}")).collect();
    std::fs::write(
        dir.join("github.manifest.toml"),
        format!(
            "name = \"github\"\nversion = \"0.1.0\"\nseam = \"connection\"\nhost_kind = \"github\"\n\n\
             [capabilities]\nhosts = [{}]\ngrant = true\n",
            hosts.join(", ")
        ),
    )
    .expect("writes manifest");
}

// ---------------------------------------------------------------------------
// A fake GitHub
// ---------------------------------------------------------------------------

struct FakeGitHub {
    files: BTreeMap<String, Vec<u8>>,
    /// The `Authorization` header of every request, in order.
    authorizations: Mutex<Vec<Option<String>>>,
}

impl FakeGitHub {
    fn record(&self, headers: &HeaderMap) {
        let seen = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.authorizations.lock().expect("lock").push(seen);
    }
}

async fn tree(
    State(fake): State<Arc<FakeGitHub>>,
    UrlPath((_owner, _repo, _git_ref)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    fake.record(&headers);
    let entries: Vec<serde_json::Value> = fake
        .files
        .iter()
        .map(|(path, bytes)| {
            serde_json::json!({ "path": path, "mode": "100644", "type": "blob", "sha": "x", "size": bytes.len() })
        })
        .collect();
    Json(serde_json::json!({ "sha": "root", "tree": entries, "truncated": false }))
}

async fn contents(
    State(fake): State<Arc<FakeGitHub>>,
    UrlPath((_owner, _repo, path)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    fake.record(&headers);
    let bytes = fake.files.get(&path).ok_or(StatusCode::NOT_FOUND)?;
    let name = path.rsplit('/').next().unwrap_or(&path);
    Ok(Json(
        serde_json::json!({ "name": name, "path": path, "size": bytes.len(), "type": "file" }),
    ))
}

async fn raw(
    State(fake): State<Arc<FakeGitHub>>,
    UrlPath((_owner, _repo, _git_ref, path)): UrlPath<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Vec<u8>, StatusCode> {
    fake.record(&headers);
    fake.files.get(&path).cloned().ok_or(StatusCode::NOT_FOUND)
}

/// Serve the fake on a loopback port; returns the base URL.
async fn serve(fake: Arc<FakeGitHub>) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new()
        .route("/repos/{owner}/{repo}/git/trees/{git_ref}", get(tree))
        .route("/repos/{owner}/{repo}/contents/{*path}", get(contents))
        .route("/raw/{owner}/{repo}/{git_ref}/{*path}", get(raw))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("binds");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serves");
    });
    (base, server)
}

fn repository() -> Arc<FakeGitHub> {
    let mut files = BTreeMap::new();
    files.insert(
        "README.md".to_string(),
        b"# Hello\n\nA tiny repository about garden sheds.\n".to_vec(),
    );
    files.insert(
        "docs/guide.md".to_string(),
        b"# Guide\n\nHow to paint a shed.\n".to_vec(),
    );
    files.insert(
        "src/lib.rs".to_string(),
        b"//! The shed library.\npub fn shed() {}\n".to_vec(),
    );
    Arc::new(FakeGitHub {
        files,
        authorizations: Mutex::new(Vec::new()),
    })
}

// ---------------------------------------------------------------------------
// A fake oauth provider holding one grant
// ---------------------------------------------------------------------------

struct FakeGrant {
    spec: GrantSpec,
}

#[async_trait::async_trait]
impl Grant for FakeGrant {
    fn spec(&self) -> &GrantSpec {
        &self.spec
    }
    async fn state(&self) -> GrantState {
        GrantState::Authorized {
            expires_at: None,
            scopes: Vec::new(),
            account: None,
        }
    }
    async fn access_token(&self) -> Result<AccessToken, SeamError> {
        Ok(AccessToken::new("fake-token"))
    }
    async fn revoke(&self) -> Result<(), SeamError> {
        Ok(())
    }
}

struct FakeOAuth {
    grant: Arc<dyn Grant>,
}

#[async_trait::async_trait]
impl OAuth for FakeOAuth {
    fn grants(&self) -> Vec<Arc<dyn Grant>> {
        vec![Arc::clone(&self.grant)]
    }
    async fn register(
        &self,
        _spec: GrantSpec,
    ) -> Result<(Arc<dyn Grant>, GrantDisposer), SeamError> {
        Err(SeamError::Unavailable(
            "fake oauth registers nothing".into(),
        ))
    }
    async fn authorize(
        &self,
        _grant: &GrantId,
        _redirect: Redirect,
    ) -> Result<AuthorizationStarted, SeamError> {
        Err(SeamError::Unavailable(
            "fake oauth authorizes nothing".into(),
        ))
    }
    async fn await_authorization(&self, _state: &str) -> Result<GrantId, SeamError> {
        Err(SeamError::Unavailable(
            "fake oauth authorizes nothing".into(),
        ))
    }
    async fn complete_authorization(
        &self,
        _callback: AuthorizationCallback,
    ) -> Result<GrantId, SeamError> {
        Err(SeamError::Unavailable(
            "fake oauth authorizes nothing".into(),
        ))
    }
}

struct FakeOAuthPlugin;

#[async_trait::async_trait]
impl Plugin for FakeOAuthPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "fake-oauth",
            inject: &[],
            provides: &["oauth"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let grant: Arc<dyn Grant> = Arc::new(FakeGrant {
            spec: GrantSpec {
                id: GrantId::new("github").expect("valid"),
                authorization_url: "https://github.test/authorize".into(),
                token_url: "https://github.test/token".into(),
                scopes: Vec::new(),
                client_id_env: "X".into(),
                client_secret_env: None,
                authorization_params: BTreeMap::new(),
            },
        });
        cx.provide(
            &OAUTH,
            Arc::new(FakeOAuth { grant }) as Arc<dyn OAuth>,
            Facts::new(),
        )?;
        Ok(())
    }
}

struct FakeOAuthFactory;

impl PluginFactory for FakeOAuthFactory {
    fn name(&self) -> &str {
        "fake-oauth"
    }
    fn build(&self, _config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(FakeOAuthPlugin))
    }
}

// ---------------------------------------------------------------------------
// The node
// ---------------------------------------------------------------------------

fn composition(artifact: &Path, base: &str, entry_extra: &str, plugin_extra: &str) -> Composition {
    Composition::parse(
        &format!(
            r#"
            [[entry]]
            id = "connections"
            plugin = "connections"

            [[entry]]
            id = "oauth"
            plugin = "fake-oauth"

            [[entry]]
            id = "embedder"
            plugin = "embedder"
            [entry.config]
            provider = "hashed"
            model = "hashed"
            dimensions = 64

            [[entry]]
            id = "transforms"
            plugin = "transforms"

            [[entry]]
            id = "markdown"
            plugin = "transform-markdown"

            [[entry]]
            id = "summarizer"
            plugin = "transform-summarizer"

            [[entry]]
            id = "finder"
            plugin = "finder"

            [[entry]]
            id = "sweep"
            plugin = "sweep"

            [[entry]]
            id = "operations"
            plugin = "operations"

            [[entry]]
            id = "github"
            plugin = "wasm:{artifact}"
            [entry.config]
            roots = [""]
            {entry_extra}
            [entry.config.plugin]
            repository = "octo/hello"
            ref = "main"
            api_base = "{base}"
            raw_base = "{base}/raw"
            {plugin_extra}
            "#,
            artifact = artifact.display(),
        ),
        "github e2e",
    )
    .expect("composition parses")
}

async fn kernel(data_dir: &Path) -> Kernel {
    let mut factories = inseam_plugins::factories();
    factories.push(Arc::new(FakeOAuthFactory));
    Kernel::boot(
        data_dir,
        factories,
        vec![Arc::new(WasmSchemeFactory::new(data_dir))],
    )
    .await
    .expect("boots")
}

fn fiber_state(kernel: &Kernel, id: &str) -> FiberState {
    kernel
        .fibers()
        .into_iter()
        .find(|f| f.id == id)
        .map(|f| f.state)
        .expect("fiber present")
}

#[tokio::test]
async fn a_loaded_connection_stewards_a_repository_the_node_can_index_and_read() {
    let Some(dir) = github_dir() else {
        eprintln!("skipping: plugins/github/github.wasm not built");
        return;
    };
    let fake = repository();
    let (base, server) = serve(Arc::clone(&fake)).await;
    let staged = tempfile::tempdir().expect("tempdir");
    let artifact = stage(
        &dir,
        staged.path(),
        &["api.github.com", "raw.githubusercontent.com", "127.0.0.1"],
    );
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(data.path()).await;
    kernel
        .reconcile(&composition(&artifact, &base, "", ""))
        .await
        .expect("settles with the loaded connection mounted");
    assert_eq!(fiber_state(&kernel, "github"), FiberState::Active);
    let ops = kernel.service(&OPERATIONS).expect("operations");

    // The host is registered like any linked one: derived id, kind,
    // display name, configured roots.
    let hosts = ops.hosts().await.expect("hosts");
    let host = hosts
        .iter()
        .find(|h| h.kind.as_str() == "github")
        .expect("github host registered");
    assert!(host.id.as_str().starts_with("github-"));
    assert_eq!(host.display_name, "GitHub · octo/hello");
    assert_eq!(host.roots, vec![String::new()]);
    assert!(host.capabilities.enumerates);
    assert!(!host.capabilities.writable);

    let report = ops
        .index(IndexRequest {
            host: Some(host.id.clone()),
            root: String::new(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes");
    assert_eq!(report.sources_seen, 3, "{report:?}");
    assert_eq!(report.indexed, 3, "{report:?}");

    let found = ops
        .query(QueryRequest::new("painting a garden shed", 5))
        .await
        .expect("queries");
    assert!(
        !found.results.is_empty(),
        "the repository's text is findable"
    );
    assert!(found.results.iter().all(|r| r.address.host == host.id));

    let readme = found
        .results
        .iter()
        .find(|r| r.address.locator.as_str() == "README.md")
        .expect("README.md indexed");
    let fetched = ops
        .fetch(FetchRequest {
            address: readme.address.clone(),
        })
        .await
        .expect("fetches");
    assert!(fetched.text.contains("garden sheds"), "{fetched:?}");
    assert!(fetched.content_type.starts_with("text/markdown"));

    // No entry grant: nothing was authorized, and the component never
    // asked (authorize is false), so no bearer ever went out.
    assert!(
        fake.authorizations
            .lock()
            .expect("lock")
            .iter()
            .all(Option::is_none)
    );

    // A narrower scope on the next run reconciles by the locator prefix
    // the component reported: the docs folder only.
    let report = ops
        .index(IndexRequest {
            host: Some(host.id.clone()),
            root: "docs/".into(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await
        .expect("indexes a scope");
    assert_eq!(report.sources_seen, 1, "{report:?}");
    kernel.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn the_grant_bearer_is_attached_by_the_bridge_only_when_the_entry_names_a_grant() {
    let Some(dir) = github_dir() else {
        eprintln!("skipping: plugins/github/github.wasm not built");
        return;
    };
    let fake = repository();
    let (base, server) = serve(Arc::clone(&fake)).await;
    let staged = tempfile::tempdir().expect("tempdir");
    let artifact = stage(
        &dir,
        staged.path(),
        &["api.github.com", "raw.githubusercontent.com", "127.0.0.1"],
    );
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(data.path()).await;

    // The plugin asks for authorization, the entry names the grant: every
    // request carries the bearer the fake grant issued.
    kernel
        .reconcile(&composition(
            &artifact,
            &base,
            "grant = \"github\"",
            "authorize = true",
        ))
        .await
        .expect("settles");
    assert_eq!(fiber_state(&kernel, "github"), FiberState::Active);
    let ops = kernel.service(&OPERATIONS).expect("operations");
    let hosts = ops.hosts().await.expect("hosts");
    let host = hosts
        .iter()
        .find(|h| h.kind.as_str() == "github")
        .expect("github host");
    ops.index(IndexRequest {
        host: Some(host.id.clone()),
        root: String::new(),
        rebuild: false,
        deep_budget: None,
        llm_lane: None,
    })
    .await
    .expect("indexes with the bearer");
    {
        let seen = fake.authorizations.lock().expect("lock");
        assert!(!seen.is_empty());
        assert!(
            seen.iter()
                .all(|a| a.as_deref() == Some("Bearer fake-token")),
            "every request was authorized: {seen:?}"
        );
    }

    // The plugin asks for authorization but the entry names no grant: the
    // bridge refuses before any request leaves the node.
    fake.authorizations.lock().expect("lock").clear();
    kernel
        .reconcile(&composition(&artifact, &base, "", "authorize = true"))
        .await
        .expect("settles");
    let ops = kernel.service(&OPERATIONS).expect("operations");
    let refused = ops
        .index(IndexRequest {
            host: Some(host.id.clone()),
            root: String::new(),
            rebuild: false,
            deep_budget: None,
            llm_lane: None,
        })
        .await;
    assert!(
        refused.is_err(),
        "an unauthorized request is an error, not a silent empty sweep"
    );
    assert!(
        fake.authorizations.lock().expect("lock").is_empty(),
        "nothing reached the host"
    );

    // An entry naming a grant the manifest did not request cannot build.
    let denied = composition(&artifact, &base, "grant = \"github\"", "");
    write_manifest(staged.path(), &["127.0.0.1"]);
    let mut manifest =
        std::fs::read_to_string(staged.path().join("github.manifest.toml")).expect("reads");
    manifest = manifest.replace("grant = true", "grant = false");
    std::fs::write(staged.path().join("github.manifest.toml"), manifest).expect("writes");
    let outcome = kernel.reconcile(&denied).await;
    assert!(
        outcome.is_err(),
        "a build-time refusal, before any fiber runs: {outcome:?}"
    );
    kernel.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn widening_the_host_allow_list_needs_explicit_approval() {
    let Some(dir) = github_dir() else {
        eprintln!("skipping: plugins/github/github.wasm not built");
        return;
    };
    let fake = repository();
    let (base, server) = serve(Arc::clone(&fake)).await;
    let staged = tempfile::tempdir().expect("tempdir");
    let artifact = stage(
        &dir,
        staged.path(),
        &["api.github.com", "raw.githubusercontent.com", "127.0.0.1"],
    );
    let data = tempfile::tempdir().expect("tempdir");
    let mut kernel = kernel(data.path()).await;
    kernel
        .reconcile(&composition(&artifact, &base, "", ""))
        .await
        .expect("settles");
    assert_eq!(fiber_state(&kernel, "github"), FiberState::Active);

    // The next version names one more host: the diff, not the clock, is
    // the question, and the fiber holds until the owner answers it.
    write_manifest(
        staged.path(),
        &[
            "api.github.com",
            "raw.githubusercontent.com",
            "127.0.0.1",
            "*.example.com",
        ],
    );
    let mut wider = composition(&artifact, &base, "cooldown_days = 0", "");
    if let Some(entry) = wider.entries.iter_mut().find(|e| e.id == "github") {
        entry
            .config
            .insert("fuel".into(), toml::Value::Integer(1_999_999_999));
    }
    kernel.reconcile(&wider).await.expect("the rest settles");
    let FiberState::Failed(reason) = fiber_state(&kernel, "github") else {
        panic!("expected the widening gate to hold the fiber");
    };
    assert!(reason.contains("different capabilities"), "{reason}");
    assert!(
        reason.contains("*.example.com"),
        "the diff names the new host: {reason}"
    );

    let approved = composition(&artifact, &base, "allow_new = true", "");
    kernel.reconcile(&approved).await.expect("settles");
    assert_eq!(fiber_state(&kernel, "github"), FiberState::Active);
    kernel.shutdown().await;
    server.abort();
}
