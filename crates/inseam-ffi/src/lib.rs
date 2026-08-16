//! C ABI over the node — the Swift/GUI transport adapter
//! (`design/node-api.md`). Like every transport, this is a thin, logic-free
//! skin over the `operations` seam: one handle wraps a booted kernel plus
//! the tokio runtime it needs; requests block the calling thread and
//! responses cross the boundary as JSON (the same serde views the seam
//! defines). The crate is itself a **distribution**: it links the native
//! plugin set and ships a base composition, layered under the node's
//! `composition.toml` (`design/composition.md`).
//!
//! Conventions (mirrored in `include/inseam_ffi.h`):
//! - Fallible calls take `char **error_out`; on failure they return null
//!   and, when `error_out` is non-null, store a message the caller must
//!   free.
//! - Every `char *` returned by this library is freed with
//!   `inseam_string_free`, and nodes with `inseam_node_free`. Passing
//!   pointers from anywhere else is undefined behavior.

use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use inseam_kernel::substrate::{Composition, Kernel, SubstrateError};
use inseam_seams::operations::{IndexRequest, Operations, QueryRequest, OPERATIONS};
use tokio::runtime::Runtime;

/// The plugins an embedded node mounts by default; the node's
/// `composition.toml` patches these entries by id.
const BASE_COMPOSITION: &str = r#"
[[entry]]
id = "fs"
plugin = "connection-fs"

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
/// free), and the operations handle requests go through.
pub struct InseamNode {
    runtime: Runtime,
    kernel: Kernel,
    operations: Arc<dyn Operations>,
}

/// The core library version as a fresh C string.
#[unsafe(no_mangle)]
pub extern "C" fn inseam_version() -> *mut c_char {
    to_c_string(env!("CARGO_PKG_VERSION"))
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

    match open_node(&data_dir, composition_path.as_deref()) {
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
        let mut handle = unsafe { Box::from_raw(node) };
        let InseamNode { runtime, ref mut kernel, .. } = *handle;
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
    let request = QueryRequest {
        text: text.to_owned(),
        limit: limit as usize,
    };
    let response = handle.runtime.block_on(handle.operations.query(request));
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
    let report = handle.runtime.block_on(handle.operations.index(IndexRequest {
        root: dir.to_string(),
        rebuild,
    }));
    json_result(report, error_out)
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

fn open_node(data_dir: &Path, composition_path: Option<&Path>) -> Result<InseamNode, String> {
    let base = Composition::parse(BASE_COMPOSITION, "<ffi base>")
        .expect("the base composition is valid");
    let overlay_path = match composition_path {
        Some(path) => Some(path.to_path_buf()),
        None => {
            let default = data_dir.join("composition.toml");
            default.exists().then_some(default)
        }
    };
    let composition = match overlay_path {
        Some(path) => base
            .layered(Composition::load(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?,
        None => base,
    };

    let runtime = Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let mut kernel = runtime
        .block_on(Kernel::boot(data_dir, inseam_plugins::factories(), Vec::new()))
        .map_err(|e| e.to_string())?;
    match runtime.block_on(kernel.reconcile(&composition)) {
        Ok(()) => {}
        // Loud but not fatal: an app can open a node whose llm entry failed
        // (no key) and still browse; operations-needing calls error below.
        Err(e @ SubstrateError::Unsettled { .. }) => eprintln!("inseam: warning: {e}"),
        Err(e) => return Err(e.to_string()),
    }
    let operations = kernel.service(&OPERATIONS).map_err(|e| e.to_string())?;
    Ok(InseamNode {
        runtime,
        kernel,
        operations,
    })
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
    if !error_out.is_null() {
        // SAFETY: fail is only reached from ffi entry points whose callers
        // promise error_out is null or writable, and null is checked above.
        unsafe { *error_out = to_c_string(message) };
    }
    std::ptr::null_mut()
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
    CString::new(bytes).expect("NUL-free by construction").into_raw()
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
        let node = unsafe {
            inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err)
        };
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
}
