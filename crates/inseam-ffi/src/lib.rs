//! C ABI over the node core for embedding in native apps — the Swift/GUI
//! transport adapter `design/node-api.md` promises. One handle wraps a `Node`
//! plus the tokio runtime it needs; requests block the calling thread and
//! responses cross the boundary as JSON (the same serde views `ops` defines).
//!
//! Conventions (mirrored in `include/inseam_ffi.h`):
//! - Fallible calls take `char **error_out`; on failure they return null and,
//!   when `error_out` is non-null, store a message the caller must free.
//! - Every `char *` returned by this library is freed with
//!   `inseam_string_free`, and nodes with `inseam_node_free`. Passing pointers
//!   from anywhere else is undefined behavior.

use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use inseam::llm::LlmClient;
use inseam::ops::{Node, QueryRequest};
use inseam::profile::IndexProfile;
use tokio::runtime::Runtime;

/// An open node plus the runtime that drives its async operations.
pub struct InseamNode {
    runtime: Runtime,
    node: Node,
}

/// The core library version as a fresh C string.
#[unsafe(no_mangle)]
pub extern "C" fn inseam_version() -> *mut c_char {
    to_c_string(env!("CARGO_PKG_VERSION"))
}

/// Open the node under `data_dir`. `profile_path` may be null: then
/// `<data_dir>/profile.toml` is used when present, else the default profile.
///
/// # Safety
/// `data_dir` must be a valid NUL-terminated string; `profile_path` must be
/// null or valid; `error_out` must be null or point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_open(
    data_dir: *const c_char,
    profile_path: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut InseamNode {
    // SAFETY: caller contract above.
    let Some(data_dir) = (unsafe { arg_str(data_dir) }) else {
        return fail(error_out, "data_dir must be a valid UTF-8 C string");
    };
    let data_dir = PathBuf::from(data_dir);
    // SAFETY: caller contract above.
    let profile_path = unsafe { arg_str(profile_path) }.map(PathBuf::from);

    match open_node(&data_dir, profile_path.as_deref()) {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(message) => fail(error_out, &message),
    }
}

/// Close a node and release its runtime. Null is a no-op.
///
/// # Safety
/// `node` must have come from `inseam_node_open` and not been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inseam_node_free(node: *mut InseamNode) {
    if !node.is_null() {
        // SAFETY: caller contract above; the box was leaked by open.
        drop(unsafe { Box::from_raw(node) });
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
    let response = handle.runtime.block_on(handle.node.query(request));
    json_result(response, error_out)
}

/// Index a directory of the local filesystem host. Returns a JSON report
/// `{"indexed": n, "unchanged": n, "removed": n}`-shaped per `IndexReport`.
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
    let report = handle
        .runtime
        .block_on(handle.node.index_dir(Path::new(dir), rebuild));
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

fn open_node(data_dir: &Path, profile_path: Option<&Path>) -> Result<InseamNode, String> {
    let profile = match profile_path {
        Some(path) => IndexProfile::load(path).map_err(|e| e.to_string())?,
        None => {
            let default_path = data_dir.join("profile.toml");
            if default_path.exists() {
                IndexProfile::load(&default_path).map_err(|e| e.to_string())?
            } else {
                IndexProfile::default()
            }
        }
    };
    let llm = LlmClient::from_config(&profile.endpoint).ok().map(Arc::new);
    let runtime = Runtime::new().map_err(|e| format!("tokio runtime: {e}"))?;
    let node = runtime
        .block_on(Node::open(data_dir, profile, llm))
        .map_err(|e| e.to_string())?;
    Ok(InseamNode { runtime, node })
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
        // An offline profile at the default location, proving the
        // <data_dir>/profile.toml discovery path.
        std::fs::write(
            dir.path().join("profile.toml"),
            "[embedding]\nprovider = \"hashed\"\nmodel = \"hashed\"\ndimensions = 64\n",
        )
        .unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node =
            unsafe { inseam_node_open(data_dir.as_ptr(), std::ptr::null(), &mut err) };
        assert!(err.is_null(), "{}", take_string(err));
        assert!(!node.is_null());

        let text = CString::new("anything").unwrap();
        // SAFETY: live node, valid C string, writable error slot.
        let json = unsafe { inseam_node_query(node, text.as_ptr(), 5, &mut err) };
        assert!(err.is_null(), "{}", take_string(err));
        let parsed: serde_json::Value = serde_json::from_str(&take_string(json)).unwrap();
        assert!(parsed["results"].is_array());

        // SAFETY: freeing the handle exactly once.
        unsafe { inseam_node_free(node) };
    }

    #[test]
    fn open_reports_error_for_bad_profile() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = CString::new(dir.path().to_str().unwrap()).unwrap();
        let missing = CString::new("/nonexistent/profile.toml").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: valid C strings and a writable error slot.
        let node =
            unsafe { inseam_node_open(data_dir.as_ptr(), missing.as_ptr(), &mut err) };
        assert!(node.is_null());
        assert!(!err.is_null());
        assert!(!take_string(err).is_empty());
    }
}
