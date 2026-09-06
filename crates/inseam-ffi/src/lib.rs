//! C ABI over the node — the Swift/GUI transport adapter
//! (`design/node-api.md`). Like every transport, this is a thin, logic-free
//! skin over the `operations` seam: one handle wraps a booted kernel plus
//! the tokio runtime it needs; requests block the calling thread and
//! responses cross the boundary as JSON (the same serde views the seam
//! defines). The crate is itself a **distribution**: it links the
//! first-party linked plugins and ships a base composition, layered under the node's
//! `composition.toml` (`design/composition.md`).
//!
//! Conventions (mirrored in `include/inseam_ffi.h`):
//! - Fallible calls take `char **error_out`; on failure they return null
//!   and, when `error_out` is non-null, store a message the caller must
//!   free.
//! - Every `char *` returned by this library is freed with
//!   `inseam_string_free`, and nodes with `inseam_node_free`. Passing
//!   pointers from anywhere else is undefined behavior.

use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use inseam_kernel::substrate::{Composition, CompositionEdits, FiberState, Kernel, SubstrateError};
use inseam_seams::SeamError;
use inseam_seams::connection::CONNECTIONS;
use inseam_seams::oauth::{GrantId, Redirect};
use inseam_seams::operations::{
    AuthorizeGrantRequest, AwaitAuthorizationRequest, IndexRequest, InstallPluginRequest,
    OPERATIONS, Operations, QueryRequest, RevokeGrantRequest,
};
use tokio::runtime::Runtime;

pub mod bridge;
mod settings;
pub mod shell;

use bridge::{BridgedHost, BridgedHostDescription, InseamHostCallbacks};
use shell::{
    InseamEmbedderCallbacks, ShellEmbedder, ShellEmbedderDescription, ShellEmbedderFactory,
};

/// What a shell that brings its own embedder layers between the base and
/// the node's overlay: the `embedder` entry re-pointed at the shell's
/// plugin. The overlay still wins, so an owner can switch back to an
/// endpoint by naming one.
const SHELL_EMBEDDER_COMPOSITION: &str = r#"
[[entry]]
id = "embedder"
plugin = "embedder-app"
"#;

/// The plugins an embedded node mounts by default; the node's
/// `composition.toml` patches these entries by id.
const BASE_COMPOSITION: &str = r#"
[[entry]]
id = "connections"
plugin = "connections"

[[entry]]
id = "fs"
plugin = "connection-fs"

[[entry]]
id = "oauth"
plugin = "oauth"

[[entry]]
id = "google"
plugin = "connection-google"

[[entry]]
id = "llm"
plugin = "llm-endpoint"

[[entry]]
id = "embedder"
plugin = "embedder"

[[entry]]
id = "transforms"
plugin = "transforms"

[[entry]]
id = "markdown"
plugin = "transform-markdown"

[[entry]]
id = "chunker"
plugin = "transform-chunker"

[[entry]]
id = "summarizer"
plugin = "transform-summarizer"

[[entry]]
id = "entities"
plugin = "transform-entities"

[[entry]]
id = "finder"
plugin = "finder"

[[entry]]
id = "sweep"
plugin = "sweep"

[[entry]]
id = "operations"
plugin = "operations"
"#;

/// An open node: the runtime, the kernel (owning fibers and effects until
/// free), and the operations handle requests go through. `operations` is
/// None while the entries beneath it are parked (a missing API key, say):
/// the node still opens so the app can show what's parked and why, and
/// operation calls fail with that explanation until a config or secret
/// change reopens the node settled.
pub struct InseamNode {
    runtime: Runtime,
    /// Behind a mutex because installing a plugin reconciles the kernel
    /// while a health call may read it from another thread: overlapping
    /// calls on one handle serialize instead of racing.
    kernel: Mutex<Kernel>,
    operations: Option<Arc<dyn Operations>>,
    /// The applying end of the `composition` service, taken once at open:
    /// this handle is the distribution, so it is what applies the edits an
    /// `install_plugin` call submits ([`run_with_edits`]).
    edits: Mutex<CompositionEdits>,
    /// What edits are applied against: the distribution base and the
    /// node's overlay — its would-be path even before the file exists.
    base: Composition,
    overlay_path: PathBuf,
    /// Hosts the app shell bridges in ([`bridge`]): each with the disposer
    /// that withdraws it from the `connections` registry. Dropping the
    /// handle withdraws them all before the kernel shuts down.
    bridged_hosts: Mutex<Vec<BridgedRegistration>>,
}

struct BridgedRegistration {
    host_id: inseam_kernel::address::HostId,
    dispose: Box<dyn FnOnce() + Send>,
}

impl InseamNode {
    fn kernel(&self) -> MutexGuard<'_, Kernel> {
        self.kernel.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The core library version as a fresh C string.
#[unsafe(no_mangle)]
pub extern "C" fn inseam_version() -> *mut c_char {
    to_c_string(env!("CARGO_PKG_VERSION"))
}

/// Read the effective first-party settings at `composition_path`, with
/// plugin defaults filled in, as JSON for a native settings form.
///
/// # Safety
/// `composition_path` must be a valid NUL-terminated string; `error_out`
/// must be null or point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_settings_read(
    composition_path: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(path) = (unsafe { arg_str(composition_path) }) else {
        return fail(error_out, "composition_path must be a valid UTF-8 C string");
    };
    json_result(settings::read(Path::new(path)), error_out)
}

/// Validate and atomically write first-party settings JSON to the node's
/// composition, retaining entries owned by custom plugins.
///
/// # Safety
/// Both string arguments must be valid NUL-terminated strings; `error_out`
/// must be null or point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_settings_write(
    composition_path: *const c_char,
    settings_json: *const c_char,
    error_out: *mut *mut c_char,
) -> bool {
    // SAFETY: caller contract above.
    let Some(path) = (unsafe { arg_str(composition_path) }) else {
        set_error(error_out, "composition_path must be a valid UTF-8 C string");
        return false;
    };
    // SAFETY: caller contract above.
    let Some(json) = (unsafe { arg_str(settings_json) }) else {
        set_error(error_out, "settings_json must be a valid UTF-8 C string");
        return false;
    };
    match settings::write(Path::new(path), json) {
        Ok(()) => true,
        Err(message) => {
            set_error(error_out, &message);
            false
        }
    }
}

/// Open the node under `data_dir`. `composition_path` may be null: then
/// `<data_dir>/composition.toml` is layered when present, else the base
/// composition boots alone.
///
/// # Safety
/// `data_dir` must be a valid NUL-terminated string; `composition_path`
/// must be null or valid; `error_out` must be null or point to writable
/// memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_open(
    data_dir: *const c_char,
    composition_path: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut InseamNode {
    // SAFETY: caller contract above.
    let Some(data_dir) = (unsafe { arg_str(data_dir) }) else {
        return fail(error_out, "data_dir must be a valid UTF-8 C string");
    };
    let data_dir = PathBuf::from(data_dir);
    // SAFETY: caller contract above.
    let composition_path = unsafe { arg_str(composition_path) }.map(PathBuf::from);

    match open_node(&data_dir, composition_path.as_deref(), None) {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(message) => fail(error_out, &message),
    }
}

/// Open the node with the shell's own capabilities mounted ([`shell`]):
/// today an embedder, so a device with an on-device model needs no API
/// key. `embedder_json` is a `ShellEmbedderDescription` (`{model,
/// dimensions}`); the callbacks are copied. The base composition's
/// `embedder` entry is re-pointed at the `embedder-app` plugin beneath the
/// node's overlay. On failure to open, `release` is NOT called and the
/// shell still owns `user_data`; on success it runs once at
/// `inseam_node_free`.
///
/// # Safety
/// As `inseam_node_open`, plus: `embedder_json` a valid C string,
/// `callbacks` a complete struct outliving the call, and the callbacks
/// upholding the threading contract in [`shell`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_open_with_shell(
    data_dir: *const c_char,
    composition_path: *const c_char,
    embedder_json: *const c_char,
    callbacks: *const InseamEmbedderCallbacks,
    user_data: *mut std::ffi::c_void,
    error_out: *mut *mut c_char,
) -> *mut InseamNode {
    // SAFETY: caller contract above.
    let Some(data_dir) = (unsafe { arg_str(data_dir) }) else {
        return fail(error_out, "data_dir must be a valid UTF-8 C string");
    };
    let data_dir = PathBuf::from(data_dir);
    // SAFETY: caller contract above.
    let composition_path = unsafe { arg_str(composition_path) }.map(PathBuf::from);
    // SAFETY: caller contract above.
    let Some(embedder_json) = (unsafe { arg_str(embedder_json) }) else {
        return fail(error_out, "embedder_json must be a valid UTF-8 C string");
    };
    // SAFETY: caller contract above.
    let Some(callbacks) = (unsafe { callbacks.as_ref() }) else {
        return fail(error_out, "callbacks must not be null");
    };
    let description: ShellEmbedderDescription = match serde_json::from_str(embedder_json) {
        Ok(description) => description,
        Err(e) => {
            return fail(
                error_out,
                &format!("embedder description is not valid: {e}"),
            );
        }
    };
    let embedder = match ShellEmbedder::new(&description, *callbacks, user_data) {
        Ok(embedder) => embedder,
        Err(message) => return fail(error_out, &message),
    };
    match open_node(&data_dir, composition_path.as_deref(), Some(embedder)) {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(message) => fail(error_out, &message),
    }
}

/// Close a node — the kernel unwinds every fiber's effects — and release
/// its runtime. Null is a no-op.
///
/// # Safety
/// `node` must have come from `inseam_node_open` and not been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_free(node: *mut InseamNode) {
    if !node.is_null() {
        // SAFETY: caller contract above; the box was leaked by open.
        let handle = unsafe { Box::from_raw(node) };
        let InseamNode {
            runtime,
            kernel,
            bridged_hosts,
            ..
        } = *handle;
        // Withdraw bridged hosts first so no fiber effect is left pointing
        // at a shell context the shell is about to reclaim.
        let bridged = bridged_hosts
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        for registration in bridged {
            (registration.dispose)();
        }
        let mut kernel = kernel.into_inner().unwrap_or_else(PoisonError::into_inner);
        runtime.block_on(kernel.shutdown());
    }
}

/// Run a finder query. Returns the `QueryResponse` as JSON.
///
/// # Safety
/// `node` must be a live handle from `inseam_node_open`; `text` a valid C
/// string; `error_out` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_query(
    node: *const InseamNode,
    text: *const c_char,
    limit: u32,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(text) = (unsafe { arg_str(text) }) else {
        return fail(error_out, "query text must be a valid UTF-8 C string");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let request = QueryRequest {
        text: text.to_owned(),
        limit: limit as usize,
    };
    let response = handle.runtime.block_on(operations.query(request));
    json_result(response, error_out)
}

/// Index a directory of the local filesystem host. Returns the sweep's
/// `IndexReport` as JSON.
///
/// # Safety
/// Same contracts as `inseam_node_query`; `dir` must be a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_index_dir(
    node: *const InseamNode,
    dir: *const c_char,
    rebuild: bool,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(dir) = (unsafe { arg_str(dir) }) else {
        return fail(error_out, "dir must be a valid UTF-8 C string");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let report = handle.runtime.block_on(operations.index(IndexRequest {
        host: None,
        root: dir.to_string(),
        rebuild,
        deep_budget: None,
        llm_lane: None,
    }));
    json_result(report, error_out)
}

/// Register a host the app shell bridges in ([`bridge`]): `host_json` is a
/// `BridgedHostDescription` (`{kind, principal, display_name,
/// capabilities?}`), `callbacks` the shell's enumerate/read/free/release
/// functions, `user_data` the shell's context handed back to each. The host
/// joins the `connections` registry at once — `inseam_node_hosts` lists it
/// and `inseam_node_index_host` sweeps it — and stays until
/// `inseam_node_unregister_host` or `inseam_node_free`. Returns the host's
/// `HostView` JSON. Refused, with the reason, when the callbacks are
/// incomplete, the description is invalid, the node already stewards the
/// host, or the handle holds `BRIDGED_HOSTS_PER_NODE_MAX` hosts.
///
/// On refusal `release` is NOT called: the shell still owns `user_data`.
///
/// # Safety
/// `node` must be a live handle; `host_json` a valid C string; `callbacks`
/// must point to a complete struct that outlives the call (it is copied);
/// the callbacks must uphold the threading contract in [`bridge`];
/// `error_out` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_register_host(
    node: *const InseamNode,
    host_json: *const c_char,
    callbacks: *const InseamHostCallbacks,
    user_data: *mut std::ffi::c_void,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(host_json) = (unsafe { arg_str(host_json) }) else {
        return fail(error_out, "host_json must be a valid UTF-8 C string");
    };
    // SAFETY: caller contract above.
    let Some(callbacks) = (unsafe { callbacks.as_ref() }) else {
        return fail(error_out, "callbacks must not be null");
    };
    let description: BridgedHostDescription = match serde_json::from_str(host_json) {
        Ok(description) => description,
        Err(e) => return fail(error_out, &format!("host description is not valid: {e}")),
    };
    let (host, bridged) = match BridgedHost::new(&description, *callbacks, user_data) {
        Ok(pair) => pair,
        Err(message) => return fail(error_out, &message),
    };
    match register_bridged_host(handle, host, bridged, description.capabilities) {
        Ok(view) => json_result(Ok::<_, SeamError>(view), error_out),
        Err(message) => fail(error_out, &message),
    }
}

/// Withdraw a bridged host from the registry. Its `release` callback runs
/// once every in-flight read has let go of it — possibly after this call
/// returns. Returns false, with the reason, when no bridged host has that
/// id (a host a plugin registered is not the shell's to withdraw).
///
/// # Safety
/// `node` must be a live handle; `host_id` a valid C string; `error_out`
/// null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_unregister_host(
    node: *const InseamNode,
    host_id: *const c_char,
    error_out: *mut *mut c_char,
) -> bool {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        set_error(error_out, "node handle is null");
        return false;
    };
    // SAFETY: caller contract above.
    let Some(host_id) = (unsafe { arg_str(host_id) }) else {
        set_error(error_out, "host_id must be a valid UTF-8 C string");
        return false;
    };
    let mut bridged = handle
        .bridged_hosts
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let Some(position) = bridged.iter().position(|r| r.host_id.as_str() == host_id) else {
        set_error(
            error_out,
            &format!("no bridged host `{host_id}` on this node"),
        );
        return false;
    };
    let registration = bridged.remove(position);
    (registration.dispose)();
    true
}

/// Index a scope of one host this node stewards — a bridged host, or any
/// other by id. `root` is the connection's scope (a bridged host's locator
/// prefix; `""` for all of it). Returns the sweep's `IndexReport` JSON.
///
/// # Safety
/// `node` must be a live handle; `host_id` and `root` valid C strings;
/// `error_out` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_index_host(
    node: *const InseamNode,
    host_id: *const c_char,
    root: *const c_char,
    rebuild: bool,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(host_id) = (unsafe { arg_str(host_id) }) else {
        return fail(error_out, "host_id must be a valid UTF-8 C string");
    };
    // SAFETY: caller contract above.
    let Some(root) = (unsafe { arg_str(root) }) else {
        return fail(error_out, "root must be a valid UTF-8 C string");
    };
    let host = match inseam_kernel::address::HostId::new(host_id) {
        Ok(host) => host,
        Err(e) => return fail(error_out, &format!("host_id: {e}")),
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let report = handle.runtime.block_on(operations.index(IndexRequest {
        host: Some(host),
        root: root.to_string(),
        rebuild,
        deep_budget: None,
        llm_lane: None,
    }));
    json_result(report, error_out)
}

/// Put a bridged host into the `connections` registry and remember its
/// disposer on the handle. The entry id names the shell as the plugin —
/// `app:<kind>` — since the shell's code is the composition for hosts only
/// it can reach (`design/ios-app.md`).
fn register_bridged_host(
    handle: &InseamNode,
    host: inseam_seams::connection::HostDescription,
    bridged: BridgedHost,
    capabilities: inseam_seams::connection::Capabilities,
) -> Result<inseam_seams::operations::HostView, String> {
    let mut bridged_hosts = handle
        .bridged_hosts
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if bridged_hosts.len() >= bridge::BRIDGED_HOSTS_PER_NODE_MAX {
        bridged.refuse();
        return Err(format!(
            "this node already holds {} bridged hosts, the ceiling",
            bridge::BRIDGED_HOSTS_PER_NODE_MAX
        ));
    }
    let registry = match handle.kernel().service(&CONNECTIONS) {
        Ok(registry) => registry,
        Err(e) => {
            bridged.refuse();
            return Err(e.to_string());
        }
    };
    let host_id = host.id.clone();
    let entry_id = format!("app:{}", host.kind);
    let view = inseam_seams::operations::HostView {
        id: host_id.clone(),
        kind: host.kind.clone(),
        display_name: host.display_name.clone(),
        entry: entry_id.clone(),
        capabilities,
    };
    if registry.resolve(&host.id).is_some() {
        let existing = registry
            .resolve(&host.id)
            .map(|r| r.entry_id.clone())
            .unwrap_or_default();
        bridged.refuse();
        return Err(format!(
            "host `{host_id}` is already stewarded by entry `{existing}`; one connection per host"
        ));
    }
    let dispose = registry
        .register(inseam_seams::connection::Registration {
            entry_id,
            host,
            capabilities,
            connection: Arc::new(bridged),
        })
        .map_err(|e| e.to_string())?;
    bridged_hosts.push(BridgedRegistration { host_id, dispose });
    assert!(bridged_hosts.len() <= bridge::BRIDGED_HOSTS_PER_NODE_MAX);
    Ok(view)
}

/// The hosts this node stewards, as a JSON array of `HostView`.
///
/// # Safety
/// `node` must be a live handle from `inseam_node_open`; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_hosts(
    node: *const InseamNode,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    json_result(handle.runtime.block_on(operations.hosts()), error_out)
}

/// The OAuth grants this node holds, as a JSON array of `GrantView`
/// (`{id, provider, scopes, client_id_env, client_secret_env, state}`).
///
/// # Safety
/// `node` must be a live handle from `inseam_node_open`; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_grants(
    node: *const InseamNode,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    json_result(handle.runtime.block_on(operations.grants()), error_out)
}

/// Begin authorizing a grant over the loopback redirect. Returns the
/// `AuthorizationStarted` JSON (`{grant, url, state, redirect_uri}`): the
/// app opens `url` in the owner's browser, then blocks on
/// `inseam_node_authorize_await` with `state`.
///
/// # Safety
/// `node` must be a live handle; `grant` a valid C string; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_authorize_begin(
    node: *const InseamNode,
    grant: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(grant) = (unsafe { arg_str(grant) }) else {
        return fail(error_out, "grant must be a valid UTF-8 C string");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let grant = match GrantId::new(grant) {
        Ok(grant) => grant,
        Err(e) => return fail(error_out, &e.to_string()),
    };
    let started = handle
        .runtime
        .block_on(operations.authorize_grant(AuthorizeGrantRequest {
            grant,
            redirect: Redirect::Loopback,
        }));
    json_result(started, error_out)
}

/// Wait for a begun authorization to finish — blocks up to the oauth
/// entry's timeout, so call it off the main thread — and return the grant's
/// `GrantView` JSON.
///
/// # Safety
/// `node` must be a live handle; `state` a valid C string; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_authorize_await(
    node: *const InseamNode,
    state: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(state) = (unsafe { arg_str(state) }) else {
        return fail(error_out, "state must be a valid UTF-8 C string");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let view = handle
        .runtime
        .block_on(operations.await_authorization(AwaitAuthorizationRequest {
            state: state.to_string(),
        }));
    json_result(view, error_out)
}

/// Forget a grant's tokens. Returns the grant's `GrantView` JSON afterwards.
///
/// # Safety
/// `node` must be a live handle; `grant` a valid C string; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_revoke_grant(
    node: *const InseamNode,
    grant: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(grant) = (unsafe { arg_str(grant) }) else {
        return fail(error_out, "grant must be a valid UTF-8 C string");
    };
    let Some(operations) = handle.operations.as_ref() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let grant = match GrantId::new(grant) {
        Ok(grant) => grant,
        Err(e) => return fail(error_out, &e.to_string()),
    };
    json_result(
        handle
            .runtime
            .block_on(operations.revoke_grant(RevokeGrantRequest { grant })),
        error_out,
    )
}

/// Per-entry health for status surfaces, as a JSON array of
/// `{id, plugin, state, error, missing}` — `state` is `active`, `pending`,
/// or `failed`; `error` is set only for failed entries and `missing` only
/// for pending ones. An app renders this as "what is parked and why".
///
/// # Safety
/// `node` must be a live handle from `inseam_node_open`; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_health(
    node: *const InseamNode,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    let entries: Vec<FiberHealth> = handle
        .kernel()
        .fibers()
        .into_iter()
        .map(|fiber| {
            let (state, error) = match fiber.state {
                FiberState::Active => ("active", None),
                FiberState::Pending => ("pending", None),
                FiberState::Failed(error) => ("failed", Some(error)),
            };
            FiberHealth {
                id: fiber.id,
                plugin: fiber.plugin,
                state,
                error,
                missing: fiber.missing,
                missing_secrets: fiber
                    .missing_secrets
                    .into_iter()
                    .map(|need| SecretNeedHealth {
                        env: need.env,
                        purpose: need.purpose,
                    })
                    .collect(),
            }
        })
        .collect();
    json_result(Ok::<_, SubstrateError>(entries), error_out)
}

/// Every composition entry as the kernel runs it, as a JSON array of
/// `PluginView` (`{id, plugin, state: {state: active|pending|failed,
/// reason?}, effects, missing, missing_secrets}`) — the `plugins` owner
/// operation every transport shares.
///
/// # Safety
/// `node` must be a live handle from `inseam_node_open`; `error_out` null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_plugins(
    node: *const InseamNode,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    let Some(operations) = handle.operations.clone() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let task = handle
        .runtime
        .spawn(async move { operations.plugins().await });
    match run_with_edits(handle, task) {
        Ok(outcome) => json_result(outcome, error_out),
        Err(message) => fail(error_out, &message),
    }
}

/// Install a loaded plugin into the open node — no reopen. `request_json`
/// is an `InstallPluginRequest`: `{id, files: [{path, bytes}], config?}`,
/// the plugin directory's files with `bytes` in standard base64 (at most 64
/// files, 32 MiB in total). The files land under `<data_dir>/plugins/<id>/`,
/// the entry is appended to the node's composition, and the kernel
/// reconciles in place; returns the new entry's `PluginView` JSON. An entry
/// that fails to activate is rolled back and the failure is the error.
/// Blocks for the mount (admission runs the conformance harness on first
/// sighting) — call off the main thread.
///
/// # Safety
/// `node` must be a live handle from `inseam_node_open`; `request_json` a
/// valid C string; `error_out` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_install_plugin(
    node: *const InseamNode,
    request_json: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: caller contract above.
    let Some(handle) = (unsafe { node.as_ref() }) else {
        return fail(error_out, "node handle is null");
    };
    // SAFETY: caller contract above.
    let Some(request_json) = (unsafe { arg_str(request_json) }) else {
        return fail(error_out, "request_json must be a valid UTF-8 C string");
    };
    let request: InstallPluginRequest = match serde_json::from_str(request_json) {
        Ok(request) => request,
        Err(error) => return fail(error_out, &format!("install request: {error}")),
    };
    let Some(operations) = handle.operations.clone() else {
        return fail(error_out, &unsettled_message(&handle.kernel()));
    };
    let task = handle
        .runtime
        .spawn(async move { operations.install_plugin(request).await });
    match run_with_edits(handle, task) {
        Ok(outcome) => json_result::<_, SeamError>(outcome, error_out),
        Err(message) => fail(error_out, &message),
    }
}

/// One entry of the health report `inseam_node_health` serializes.
#[derive(serde::Serialize)]
struct FiberHealth {
    id: String,
    plugin: String,
    state: &'static str,
    error: Option<String>,
    missing: Vec<String>,
    missing_secrets: Vec<SecretNeedHealth>,
}

/// A declared-but-absent secret in the health report: the environment
/// variable to set and the owner-facing reason to set it.
#[derive(serde::Serialize)]
struct SecretNeedHealth {
    env: String,
    purpose: String,
}

/// Free a string returned by this library. Null is a no-op.
///
/// # Safety
/// `s` must have been returned by an `inseam_*` function and not freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: caller contract above; the string came from CString::into_raw.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Open a node: the distribution's base composition — with the shell's
/// embedder patched in when it brings one — under the node's overlay. A
/// shell that fails to open keeps its embedder context: the factory that
/// would have released it is dropped disarmed.
fn open_node(
    data_dir: &Path,
    composition_path: Option<&Path>,
    shell_embedder: Option<ShellEmbedder>,
) -> Result<InseamNode, String> {
    let shell_embedder = shell_embedder.map(Arc::new);
    let outcome = open_node_with(data_dir, composition_path, shell_embedder.as_ref());
    if let (Err(_), Some(embedder)) = (&outcome, shell_embedder.as_ref()) {
        embedder.disarm();
    }
    outcome
}

fn open_node_with(
    data_dir: &Path,
    composition_path: Option<&Path>,
    shell_embedder: Option<&Arc<ShellEmbedder>>,
) -> Result<InseamNode, String> {
    let mut base =
        Composition::parse(BASE_COMPOSITION, "<ffi base>").expect("the base composition is valid");
    let mut factories = inseam_plugins::factories();
    if let Some(embedder) = shell_embedder {
        let patch = Composition::parse(SHELL_EMBEDDER_COMPOSITION, "<shell embedder>")
            .expect("the shell embedder patch is valid");
        base = base
            .layered(patch)
            .expect("the shell embedder patch applies to the base");
        factories.push(Arc::new(ShellEmbedderFactory::new(Arc::clone(embedder))));
    }
    // The overlay's path is fixed whether or not the file exists yet: an
    // install writes it there if it is absent.
    let overlay_path = composition_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join("composition.toml"));
    let composition = if composition_path.is_some() || overlay_path.exists() {
        base.clone()
            .layered(Composition::load(&overlay_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        base.clone()
    };

    let runtime = Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let mut kernel = runtime
        .block_on(Kernel::boot(
            data_dir,
            factories,
            scheme_factories(data_dir),
        ))
        .map_err(|e| e.to_string())?;
    match runtime.block_on(kernel.reconcile(&composition)) {
        Ok(()) => {}
        // Loud but not fatal: an app can open a node whose llm entry failed
        // (no key) — parked entries are contained, `inseam_node_health`
        // says what and why, and operation calls error with the same story.
        Err(e @ SubstrateError::Unsettled { .. }) => eprintln!("inseam: warning: {e}"),
        Err(e) => return Err(e.to_string()),
    }
    let operations = kernel.service(&OPERATIONS).ok();
    let edits = kernel
        .take_composition_edits()
        .expect("a freshly booted kernel hands out its edits once");
    Ok(InseamNode {
        runtime,
        kernel: Mutex::new(kernel),
        operations,
        edits: Mutex::new(edits),
        base,
        overlay_path,
        bridged_hosts: Mutex::new(Vec::new()),
    })
}

/// The loaded-plugin tier, when this build carries it: the `wasm:` scheme
/// mounts components from the data directory. Without the feature no
/// scheme is registered and a `wasm:` entry fails contained, by name.
#[cfg(feature = "loaded-plugins")]
fn scheme_factories(data_dir: &Path) -> Vec<Arc<dyn inseam_kernel::substrate::SchemeFactory>> {
    vec![Arc::new(inseam_wasm_host::WasmSchemeFactory::new(data_dir))]
}

#[cfg(not(feature = "loaded-plugins"))]
fn scheme_factories(_data_dir: &Path) -> Vec<Arc<dyn inseam_kernel::substrate::SchemeFactory>> {
    Vec::new()
}

/// Run one operation to completion while applying the composition edits
/// it submits — the loop `inseam serve` runs beside its transport, here
/// run for the duration of one call. The kernel and the edits stay locked
/// throughout, so a concurrent health call waits rather than observing a
/// half-applied reconcile; the operation itself runs on the runtime's
/// workers and only ever touches the kernel through the edit channel.
fn run_with_edits<T: Send + 'static>(
    handle: &InseamNode,
    mut task: tokio::task::JoinHandle<T>,
) -> Result<T, String> {
    let mut kernel = handle.kernel();
    let mut edits = handle.edits.lock().unwrap_or_else(PoisonError::into_inner);
    handle.runtime.block_on(async {
        loop {
            tokio::select! {
                outcome = &mut task => {
                    return outcome.map_err(|error| format!("operation task: {error}"));
                }
                pending = edits.next() => {
                    let Some(pending) = pending else {
                        return Err("the kernel closed its composition edits".to_string());
                    };
                    let outcome = kernel
                        .apply_composition_edit(&handle.base, &handle.overlay_path, pending.edit())
                        .await;
                    pending.reply(outcome);
                }
            }
        }
    })
}

/// The explanation an operation call returns while `operations` is parked:
/// every failed entry with its error, then every pending entry with the
/// service keys it waits on.
fn unsettled_message(kernel: &Kernel) -> String {
    let mut lines: Vec<String> = Vec::new();
    for fiber in kernel.fibers() {
        match fiber.state {
            FiberState::Failed(error) => {
                lines.push(format!("`{}` failed: {}", fiber.id, error));
            }
            FiberState::Pending => {
                lines.push(format!(
                    "`{}` is waiting on `{}`",
                    fiber.id,
                    fiber.missing.join("`, `")
                ));
            }
            FiberState::Active => {}
        }
    }
    // Reaching here requires the operations lookup to have failed, and a
    // settled tree always binds it; assert the message carries substance.
    assert!(
        !lines.is_empty(),
        "operations missing but every fiber active"
    );
    format!("node is not fully settled: {}", lines.join("; "))
}

/// Borrow a nullable C string as `&str`; `None` for null or non-UTF-8.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated string outliving the call.
unsafe fn arg_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller contract above.
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

fn json_result<T, E>(result: Result<T, E>, error_out: *mut *mut c_char) -> *mut c_char
where
    T: serde::Serialize,
    E: std::fmt::Display,
{
    match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(json) => to_c_string(&json),
            Err(e) => fail(error_out, &format!("serialize response: {e}")),
        },
        Err(e) => fail(error_out, &e.to_string()),
    }
}

fn fail<T>(error_out: *mut *mut c_char, message: &str) -> *mut T {
    set_error(error_out, message);
    std::ptr::null_mut()
}

fn set_error(error_out: *mut *mut c_char, message: &str) {
    if !error_out.is_null() {
        // SAFETY: fail is only reached from ffi entry points whose callers
        // promise error_out is null or writable, and null is checked above.
        unsafe { *error_out = to_c_string(message) };
    }
}

fn to_c_string(s: &str) -> *mut c_char {
    // Interior NULs cannot come from our version/JSON/error strings; strip
    // them rather than panic if one ever does.
    let sanitized;
    let bytes = if s.as_bytes().contains(&0) {
        sanitized = s.replace('\0', "");
        sanitized.as_str()
    } else {
        s
    };
    // Provably infallible: bytes contains no interior NUL after the check.
    #[allow(clippy::expect_used)]
    CString::new(bytes)
        .expect("NUL-free by construction")
        .into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn take_string(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        // SAFETY: ptr came from to_c_string in this test.
        let s = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_owned();
        // SAFETY: freeing the same pointer exactly once.
        unsafe { inseam_string_free(ptr) };
        s
    }

    #[test]
    fn version_matches_crate() {
        assert_eq!(take_string(inseam_version()), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn open_query_and_free_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // An offline composition at the default location, proving the
        // <data_dir>/composition.toml discovery path.
        std::fs::write(
            dir.path().join("composition.toml"),
            r#"
            [[entry]]
            id = "embedder"
            [entry.config]
            provider = "hashed"
            model = "hashed"
            dimensions = 64

            [[entry]]
            id = "llm"
            disabled = true
            "#,
        )
        .unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(node.is_null() || err.is_null());
        assert!(!node.is_null(), "node opens offline");

        let text = CString::new("anything").unwrap();
        // SAFETY: live node, valid strings, writable error slot.
        let response = unsafe { inseam_node_query(node, text.as_ptr(), 5, &mut err) };
        assert!(!response.is_null(), "query on an empty index succeeds");
        let json = take_string(response);
        assert!(json.contains("\"results\""));

        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };
    }

    fn offline_composition(dir: &std::path::Path) {
        std::fs::write(
            dir.join("composition.toml"),
            r#"
            [[entry]]
            id = "embedder"
            [entry.config]
            provider = "hashed"
            model = "hashed"
            dimensions = 64

            [[entry]]
            id = "llm"
            disabled = true
            "#,
        )
        .unwrap();
    }

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn bridged_host_is_registered_indexed_queried_and_released() {
        use bridge::fake::FakeHost;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        offline_composition(dir.path());
        let data_dir = c(dir.path().to_str().unwrap());
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(!node.is_null(), "node opens offline");

        let host = FakeHost::new(&[
            (
                "calls/2026-09-01",
                "text/plain",
                "Talked about the kitchen renovation quote.",
            ),
            (
                "calls/2026-09-02",
                "text/plain",
                "Dentist appointment moved to Thursday.",
            ),
        ]);
        let released = Arc::clone(&host.released);
        let user_data = Box::into_raw(host).cast::<std::ffi::c_void>();
        let description =
            c(r#"{"kind":"phone","principal":"greg-iphone","display_name":"Greg's iPhone"}"#);
        let callbacks = FakeHost::callbacks();
        // SAFETY: live node, valid strings, a complete callback struct.
        let view = unsafe {
            inseam_node_register_host(node, description.as_ptr(), &callbacks, user_data, &mut err)
        };
        assert!(err.is_null(), "{}", take_string(err));
        let view = take_string(view);
        assert!(view.contains("\"id\":\"phone-"), "{view}");
        assert!(view.contains("\"entry\":\"app:phone\""), "{view}");
        let host_id: String = serde_json::from_str::<serde_json::Value>(&view).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // A second registration of the same host is refused by name, and
        // the refused context is left to the shell (release not called).
        let twin = FakeHost::new(&[]);
        let twin_released = Arc::clone(&twin.released);
        let twin_data = Box::into_raw(twin).cast::<std::ffi::c_void>();
        // SAFETY: as above.
        let refused = unsafe {
            inseam_node_register_host(node, description.as_ptr(), &callbacks, twin_data, &mut err)
        };
        assert!(refused.is_null());
        let message = take_string(err);
        assert!(message.contains(&host_id), "{message}");
        assert_eq!(twin_released.load(Ordering::SeqCst), 0);
        err = std::ptr::null_mut();
        // SAFETY: the refused context is still the test's to reclaim.
        drop(unsafe { Box::from_raw(twin_data.cast::<FakeHost>()) });

        // SAFETY: live node and a writable error slot.
        let hosts = take_string(unsafe { inseam_node_hosts(node, &mut err) });
        assert!(hosts.contains("Greg's iPhone"), "{hosts}");

        let host_c = c(&host_id);
        let root = c("calls/");
        // SAFETY: live node, valid strings, writable error slot.
        let report = unsafe {
            inseam_node_index_host(node, host_c.as_ptr(), root.as_ptr(), false, &mut err)
        };
        assert!(err.is_null(), "{}", take_string(err));
        let report: serde_json::Value = serde_json::from_str(&take_string(report)).unwrap();
        assert_eq!(report["indexed"], 2, "{report}");

        let text = c("dentist appointment");
        // SAFETY: live node, valid strings, writable error slot.
        let response = take_string(unsafe { inseam_node_query(node, text.as_ptr(), 5, &mut err) });
        assert!(response.contains("calls/2026-09-02"), "{response}");

        // Unregister withdraws the host; release follows once no reader
        // holds it, which here is immediately.
        // SAFETY: live node, valid string, writable error slot.
        assert!(unsafe { inseam_node_unregister_host(node, host_c.as_ptr(), &mut err) });
        assert_eq!(released.load(Ordering::SeqCst), 1, "release runs once");
        // SAFETY: as above.
        assert!(!unsafe { inseam_node_unregister_host(node, host_c.as_ptr(), &mut err) });
        let message = take_string(err);
        assert!(message.contains("no bridged host"), "{message}");
        err = std::ptr::null_mut();
        // SAFETY: live node and a writable error slot.
        let hosts = take_string(unsafe { inseam_node_hosts(node, &mut err) });
        assert!(!hosts.contains("Greg's iPhone"), "{hosts}");

        // A host still registered at free is withdrawn and released there.
        let late = FakeHost::new(&[]);
        let late_released = Arc::clone(&late.released);
        let late_data = Box::into_raw(late).cast::<std::ffi::c_void>();
        // SAFETY: as above.
        let view = unsafe {
            inseam_node_register_host(node, description.as_ptr(), &callbacks, late_data, &mut err)
        };
        assert!(!view.is_null(), "{}", take_string(err));
        take_string(view);
        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };
        assert_eq!(late_released.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shell_embedder_opens_indexes_queries_and_releases_once() {
        use shell::fake::{DIMENSIONS, FakeEmbedder};
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        // No embedder config in the overlay: the shell's patch is the
        // embedder, and the llm stays off so nothing parks on a key.
        std::fs::write(
            dir.path().join("composition.toml"),
            r#"
            [[entry]]
            id = "llm"
            disabled = true
            "#,
        )
        .unwrap();
        let notes = dir.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("a.md"), "apples are red\n").unwrap();
        std::fs::write(notes.join("b.md"), "bananas are yellow\n").unwrap();

        let data_dir = c(dir.path().to_str().unwrap());
        let description = c(&format!(
            r#"{{"model":"fake/unit","dimensions":{DIMENSIONS}}}"#
        ));
        let callbacks = FakeEmbedder::callbacks();
        let mut err: *mut c_char = std::ptr::null_mut();

        // A node that fails to open leaves the shell's context alone.
        let refused = FakeEmbedder::new();
        let refused_released = Arc::clone(&refused.released);
        let refused_data = Box::into_raw(refused).cast::<std::ffi::c_void>();
        let missing = c(dir.path().join("absent.toml").to_str().unwrap());
        // SAFETY: valid strings, a complete callback struct, writable slot.
        let node = unsafe {
            inseam_node_open_with_shell(
                data_dir.as_ptr(),
                missing.as_ptr(),
                description.as_ptr(),
                &callbacks,
                refused_data,
                &mut err,
            )
        };
        assert!(node.is_null());
        take_string(err);
        err = std::ptr::null_mut();
        assert_eq!(
            refused_released.load(Ordering::SeqCst),
            0,
            "not released on failure"
        );
        // SAFETY: the shell still owns the refused context.
        drop(unsafe { Box::from_raw(refused_data.cast::<FakeEmbedder>()) });

        let fake = FakeEmbedder::new();
        let released = Arc::clone(&fake.released);
        let user_data = Box::into_raw(fake).cast::<std::ffi::c_void>();
        // SAFETY: as above.
        let node = unsafe {
            inseam_node_open_with_shell(
                data_dir.as_ptr(),
                std::ptr::null(),
                description.as_ptr(),
                &callbacks,
                user_data,
                &mut err,
            )
        };
        assert!(!node.is_null(), "{}", take_string(err));

        // SAFETY: live node and a writable error slot.
        let health = take_string(unsafe { inseam_node_health(node, &mut err) });
        assert!(
            health.contains("\"id\":\"embedder\",\"plugin\":\"embedder-app\",\"state\":\"active\""),
            "{health}"
        );

        let root = c(notes.to_str().unwrap());
        // SAFETY: live node, valid string, writable error slot.
        let report = unsafe { inseam_node_index_dir(node, root.as_ptr(), false, &mut err) };
        assert!(!report.is_null(), "{}", take_string(err));
        let report: serde_json::Value = serde_json::from_str(&take_string(report)).unwrap();
        assert_eq!(report["indexed"], 2, "{report}");
        // SAFETY: the test still owns the fake through the shell's pointer.
        let calls = unsafe { &*user_data.cast::<FakeEmbedder>() }
            .calls
            .load(Ordering::SeqCst);
        assert!(calls >= 1, "the sweep embedded through the shell");

        let text = c("bananas");
        // SAFETY: live node, valid strings, writable error slot.
        let response = take_string(unsafe { inseam_node_query(node, text.as_ptr(), 5, &mut err) });
        assert!(response.contains("b.md"), "{response}");

        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };
        assert_eq!(
            released.load(Ordering::SeqCst),
            1,
            "release runs once, at free"
        );
    }

    #[test]
    fn grants_and_hosts_are_listed_and_the_google_grant_waits_for_its_client() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composition.toml"),
            r#"
            [[entry]]
            id = "embedder"
            [entry.config]
            provider = "hashed"
            model = "hashed"
            dimensions = 64

            [[entry]]
            id = "llm"
            disabled = true

            [[entry]]
            id = "google"
            [entry.config]
            client_id_env = "INSEAM_TEST_GOOGLE_CLIENT_ID_NEVER_SET"
            "#,
        )
        .unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(!node.is_null(), "node opens");
        // SAFETY: live node, writable error slot.
        let grants = take_string(unsafe { inseam_node_grants(node, &mut err) });
        assert!(grants.contains("\"id\":\"google\""), "got: {grants}");
        assert!(
            grants.contains("\"state\":\"missing_secret\""),
            "got: {grants}"
        );
        assert!(
            grants.contains("INSEAM_TEST_GOOGLE_CLIENT_ID_NEVER_SET"),
            "got: {grants}"
        );
        // SAFETY: live node, writable error slot.
        let hosts = take_string(unsafe { inseam_node_hosts(node, &mut err) });
        assert!(hosts.contains("\"kind\":\"fs\""), "got: {hosts}");
        assert!(
            !hosts.contains("gmail"),
            "no Google hosts before authorization"
        );
        let google = CString::new("google").unwrap();
        // SAFETY: live node, valid strings, writable error slot.
        let begun = unsafe { inseam_node_authorize_begin(node, google.as_ptr(), &mut err) };
        assert!(begun.is_null(), "a grant without its client cannot begin");
        let message = take_string(err);
        assert!(
            message.contains("INSEAM_TEST_GOOGLE_CLIENT_ID_NEVER_SET"),
            "got: {message}"
        );
        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };
    }

    #[test]
    fn missing_api_key_parks_entries_but_opens() {
        let dir = tempfile::tempdir().unwrap();
        // Point the llm entry at a variable that is certainly unset, so the
        // default endpoint embedder (and everything above it) parks.
        std::fs::write(
            dir.path().join("composition.toml"),
            r#"
            [[entry]]
            id = "llm"
            [entry.config]
            api_key_env = "INSEAM_TEST_KEY_THAT_IS_NOT_SET"
            "#,
        )
        .unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(!node.is_null(), "the node opens with entries parked");

        // SAFETY: live node, writable error slot.
        let health = unsafe { inseam_node_health(node, &mut err) };
        let health_json = take_string(health);
        assert!(health_json.contains("\"state\":\"failed\""));
        assert!(health_json.contains("\"state\":\"pending\""));
        // The declared secret need surfaces with its owner-facing reason.
        assert!(health_json.contains("\"env\":\"INSEAM_TEST_KEY_THAT_IS_NOT_SET\""));
        assert!(health_json.contains("unlocks the language model"));

        let text = CString::new("anything").unwrap();
        // SAFETY: live node, valid strings, writable error slot.
        let response = unsafe { inseam_node_query(node, text.as_ptr(), 5, &mut err) };
        assert!(
            response.is_null(),
            "operations are parked, not silently empty"
        );
        let message = take_string(err);
        assert!(message.contains("not fully settled"), "got: {message}");
        assert!(message.contains("`llm` failed"), "got: {message}");

        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };
    }

    /// The OCR plugin's directory, when its artifact is built (see
    /// `plugins/ocr/README.md`); the install tests skip without it.
    fn ocr_directory() -> Option<PathBuf> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ocr");
        directory.join("ocr.wasm").exists().then_some(directory)
    }

    /// The install request the app builds: every file of the plugin
    /// directory, base64, paths relative to it.
    fn install_request_json(id: &str, manifest_override: Option<&str>) -> String {
        use base64::Engine as _;
        let directory = ocr_directory().expect("callers checked");
        let files: Vec<serde_json::Value> = [
            "ocr.wasm",
            "ocr.manifest.toml",
            "ocr.checks.toml",
            "fixtures/pixel.png",
        ]
        .into_iter()
        .map(|relative| {
            let bytes = match (relative, manifest_override) {
                ("ocr.manifest.toml", Some(text)) => text.as_bytes().to_vec(),
                _ => std::fs::read(directory.join(relative)).expect("reads"),
            };
            serde_json::json!({
                "path": relative,
                "bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect();
        serde_json::json!({ "id": id, "files": files }).to_string()
    }

    const OFFLINE: &str = r#"
            [[entry]]
            id = "embedder"
            [entry.config]
            provider = "hashed"
            model = "hashed"
            dimensions = 64

            [[entry]]
            id = "llm"
            disabled = true
            "#;

    #[test]
    fn an_uploaded_plugin_mounts_into_the_open_node() {
        if ocr_directory().is_none() {
            eprintln!("skipping: plugins/ocr/ocr.wasm not built");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("composition.toml"), OFFLINE).unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(!node.is_null(), "node opens offline");

        let request = CString::new(install_request_json("ocr", None)).unwrap();
        // SAFETY: live node, valid string, writable error slot.
        let view = unsafe { inseam_node_install_plugin(node, request.as_ptr(), &mut err) };
        assert!(!view.is_null(), "install succeeds: {}", take_string(err));
        let view: serde_json::Value = serde_json::from_str(&take_string(view)).unwrap();
        assert_eq!(view["id"], "ocr");
        assert_eq!(view["state"]["state"], "active", "{view}");
        assert!(dir.path().join("plugins/ocr/ocr.wasm").is_file());
        assert!(dir.path().join("plugins/ocr/fixtures/pixel.png").is_file());
        let overlay = std::fs::read_to_string(dir.path().join("composition.toml")).unwrap();
        assert!(overlay.contains("id = \"ocr\""), "{overlay}");
        assert!(overlay.contains("plugin = \"wasm:"), "{overlay}");

        // Listed by the operation and by the health view alike.
        // SAFETY: live node, writable error slot.
        let plugins = take_string(unsafe { inseam_node_plugins(node, &mut err) });
        assert!(plugins.contains("\"id\":\"ocr\""), "{plugins}");
        // SAFETY: live node, writable error slot.
        let health = take_string(unsafe { inseam_node_health(node, &mut err) });
        assert!(
            health.contains("\"id\":\"ocr\",\"plugin\":\"wasm:"),
            "{health}"
        );

        // The same id again is refused before anything is written.
        // SAFETY: live node, valid string, writable error slot.
        let again = unsafe { inseam_node_install_plugin(node, request.as_ptr(), &mut err) };
        assert!(again.is_null());
        assert!(take_string(err).contains("already"));
        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };

        // And a reopen converges to the same tree: the file is the truth.
        // SAFETY: valid C strings and a writable error slot.
        let reopened = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(!reopened.is_null());
        // SAFETY: live node, writable error slot.
        let health = take_string(unsafe { inseam_node_health(reopened, &mut err) });
        assert!(
            health.contains("\"id\":\"ocr\",\"plugin\":\"wasm:"),
            "{health}"
        );
        assert!(!health.contains("\"state\":\"failed\""), "{health}");
        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(reopened) };
    }

    #[test]
    fn a_plugin_that_fails_to_mount_is_rolled_back() {
        if ocr_directory().is_none() {
            eprintln!("skipping: plugins/ocr/ocr.wasm not built");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("composition.toml"), OFFLINE).unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node = unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(!node.is_null());

        let request = CString::new(install_request_json(
            "broken",
            Some("name = \"ocr\"\nversion = \"0.0.1\"\nseam = \"finder\"\n"),
        ))
        .unwrap();
        // SAFETY: live node, valid string, writable error slot.
        let view = unsafe { inseam_node_install_plugin(node, request.as_ptr(), &mut err) };
        assert!(
            view.is_null(),
            "a plugin for an unsupported seam does not mount"
        );
        let message = take_string(err);
        assert!(message.contains("broken"), "{message}");
        assert!(
            !dir.path().join("plugins/broken").exists(),
            "files came out with the entry"
        );
        let overlay = std::fs::read_to_string(dir.path().join("composition.toml")).unwrap();
        assert_eq!(overlay, OFFLINE, "the overlay is exactly as it was");
        // SAFETY: freeing the node exactly once.
        unsafe { inseam_node_free(node) };
    }
}
