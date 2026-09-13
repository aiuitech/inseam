//! App-bridged hosts: a connection whose enumeration and reads are supplied
//! by the app shell through C callbacks (`design/ios-app.md`). Some hosts are
//! reachable only through a platform's own frameworks in the app's process
//! — the Photos library through PhotoKit, a Notes folder, a Files-picked
//! folder behind a security-scoped bookmark — so the shell is the connection
//! plugin for them: it enumerates and reads, the node catalogs, indexes, and
//! serves exactly as it does for any other host. Locators, envelopes, and
//! addresses are the shell's to mint; what crosses here is the same
//! `EnumeratedSource` shape every connection produces, as JSON.
//!
//! Threading contract: the callbacks run on tokio's blocking pool, never on
//! the thread that registered the host and possibly on several threads at
//! once. The shell's callbacks must be safe to call from any thread.
//! `release` runs exactly once, when the last reference to the host is
//! gone — after an unregister *and* after every in-flight read has
//! finished — so the shell frees `user_data` there and nowhere else.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;
use std::time::SystemTime;

use inseam_kernel::address::{
    Address, ContentLength, Envelope, HostId, Locator, Property, Timestamp, TrustLevel,
};
use inseam_kernel::fragment::Mimetype;
use inseam_seams::SeamError;
use inseam_seams::connection::{
    Capabilities, Connection, EnumeratedSource, HostDescription, HostKind, derive_host_id,
};
use inseam_seams::text::slice_lines;
use serde::Deserialize;

/// Most sources one enumeration may return. A phone's photo library is tens
/// of thousands of assets; a shell that has more scopes its roots.
pub const SOURCES_PER_ENUMERATION_MAX: usize = 200_000;

/// Largest single source the bridge reads into memory. Photos and call
/// recordings are megabytes; a shell with larger media hands the node a
/// derived form (a transcript, a downscaled image) instead.
pub const BYTES_PER_READ_MAX: u64 = 256 * 1024 * 1024;

/// Most properties one enumerated source may carry.
pub const PROPERTIES_PER_SOURCE_MAX: usize = 64;

/// Most bridged hosts one node handle holds at once.
pub const BRIDGED_HOSTS_PER_NODE_MAX: usize = 32;

/// The shell's side of a bridged host. Mirrored in `include/inseam_ffi.h`.
///
/// Every string the shell returns is freed through `free_string`, every
/// byte buffer through `free_bytes`, so the shell chooses its allocator.
/// An `error_out` a callback sets is also freed with `free_string`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InseamHostCallbacks {
    /// Every enumerable source under `root`, as a JSON array of
    /// `{locator, source_type, content_type, bytes, created?, modified?,
    /// title?, properties?: [{key, value}]}`. `null` with `error_out` set on
    /// failure.
    pub enumerate: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            root: *const c_char,
            error_out: *mut *mut c_char,
        ) -> *mut c_char,
    >,
    /// The raw bytes of one locator, length in `len_out`. `null` with
    /// `error_out` set on failure.
    pub read_bytes: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            locator: *const c_char,
            len_out: *mut u64,
            error_out: *mut *mut c_char,
        ) -> *mut u8,
    >,
    pub free_string: Option<unsafe extern "C" fn(user_data: *mut c_void, s: *mut c_char)>,
    pub free_bytes: Option<unsafe extern "C" fn(user_data: *mut c_void, bytes: *mut u8, len: u64)>,
    /// Called exactly once when the node is done with the host.
    pub release: Option<unsafe extern "C" fn(user_data: *mut c_void)>,
}

impl InseamHostCallbacks {
    /// Every callback is required: a bridged host with no way to free what
    /// it allocates is a leak by construction.
    fn complete(&self) -> Result<(), String> {
        let missing = [
            ("enumerate", self.enumerate.is_none()),
            ("read_bytes", self.read_bytes.is_none()),
            ("free_string", self.free_string.is_none()),
            ("free_bytes", self.free_bytes.is_none()),
            ("release", self.release.is_none()),
        ]
        .into_iter()
        .filter(|(_, absent)| *absent)
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "host callbacks are incomplete: {} unset",
                missing.join(", ")
            ))
        }
    }
}

/// How the shell describes the host it bridges. The id is derived here
/// from kind and principal, the same rule every connection follows
/// (`design/addressing.md`), so two devices bridging one library agree.
#[derive(Debug, Deserialize)]
pub struct BridgedHostDescription {
    pub kind: HostKind,
    /// The host's own identity: an iCloud account, a device identifier —
    /// never the app that reaches it.
    pub principal: String,
    pub display_name: String,
    #[serde(default = "Capabilities::read_only")]
    pub capabilities: Capabilities,
}

trait ReadOnlyDefault {
    fn read_only() -> Self;
}

impl ReadOnlyDefault for Capabilities {
    fn read_only() -> Self {
        Self::READ_ONLY
    }
}

/// One enumerated source as the shell serializes it. Length is always
/// bytes at enumeration — a text source's line count is learned when it
/// is read, exactly as the filesystem connection does it.
#[derive(Debug, Deserialize)]
struct BridgedSource {
    locator: String,
    source_type: String,
    content_type: String,
    bytes: u64,
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    modified: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    properties: Vec<BridgedProperty>,
}

#[derive(Debug, Deserialize)]
struct BridgedProperty {
    key: String,
    value: String,
}

/// The connection over the shell's callbacks: a shared context the
/// blocking pool holds while a read is in flight, so `release` waits for
/// the last reader by construction.
pub struct BridgedHost {
    host_id: HostId,
    context: Arc<HostContext>,
}

/// The shell's callbacks and opaque context. The raw pointer is only ever
/// passed back to the shell's own functions, which is what the
/// `Send + Sync` claim below rests on.
struct HostContext {
    host_id: HostId,
    callbacks: InseamHostCallbacks,
    user_data: *mut c_void,
    /// Whether `release` runs at drop. Cleared when a registration is
    /// refused, so a context the node never took stays the shell's.
    armed: std::sync::atomic::AtomicBool,
}

// SAFETY: `user_data` is opaque to Rust and reaches the shell only through
// the callbacks, whose contract (module doc) is that they may run on any
// thread, concurrently. Rust never dereferences it.
unsafe impl Send for HostContext {}
// SAFETY: as above; the fields are immutable after construction.
unsafe impl Sync for HostContext {}

impl Drop for HostContext {
    fn drop(&mut self) {
        let armed = self.armed.load(std::sync::atomic::Ordering::SeqCst);
        if let (true, Some(release)) = (armed, self.callbacks.release) {
            // SAFETY: `release` is called exactly once, here, when the last
            // `Arc` to the context is gone — no reader can still hold it.
            unsafe { release(self.user_data) };
        }
    }
}

impl BridgedHost {
    /// Validate the callbacks and describe the host. The registration into
    /// the `connections` seam is the caller's (`lib.rs`), which holds the
    /// disposer for the handle's lifetime.
    pub fn new(
        description: &BridgedHostDescription,
        callbacks: InseamHostCallbacks,
        user_data: *mut c_void,
    ) -> Result<(HostDescription, Self), String> {
        callbacks.complete()?;
        if description.principal.trim().is_empty() {
            return Err("host principal may not be empty".to_string());
        }
        if description.display_name.trim().is_empty() {
            return Err("host display_name may not be empty".to_string());
        }
        let host_id = derive_host_id(&description.kind, &description.principal);
        let host = HostDescription {
            id: host_id.clone(),
            kind: description.kind.clone(),
            display_name: description.display_name.clone(),
        };
        let context = Arc::new(HostContext {
            host_id: host_id.clone(),
            callbacks,
            user_data,
            armed: std::sync::atomic::AtomicBool::new(true),
        });
        Ok((host, Self { host_id, context }))
    }

    pub fn host_id(&self) -> &HostId {
        &self.host_id
    }

    /// Give the context back to the shell without releasing it: the
    /// registration was refused, so the node never owned `user_data`.
    pub fn refuse(self) {
        assert_eq!(
            Arc::strong_count(&self.context),
            1,
            "an unregistered host has no readers"
        );
        self.context
            .armed
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl HostContext {
    fn own(&self, address: &Address) -> Result<(), SeamError> {
        if address.host == self.host_id {
            Ok(())
        } else {
            Err(SeamError::UnknownHost(address.host.clone()))
        }
    }

    /// Take ownership of a string the shell returned, freeing the shell's
    /// copy. `None` for null.
    fn take_string(&self, ptr: *mut c_char) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: the shell returned a NUL-terminated string it owns until
        // free_string; both are the callback contract.
        let owned = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        if let Some(free_string) = self.callbacks.free_string {
            // SAFETY: freeing the shell's string exactly once, through the
            // shell's own deallocator.
            unsafe { free_string(self.user_data, ptr) };
        }
        Some(owned)
    }

    fn enumerate_blocking(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let root = CString::new(root)
            .map_err(|_| SeamError::failed("a scope root may not contain NUL"))?;
        let enumerate = self.callbacks.enumerate.ok_or_else(|| {
            SeamError::Unavailable("bridged host has no enumerate callback".to_string())
        })?;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: the callback contract — a valid C string and a writable
        // error slot; the shell returns an owned string or null.
        let json = unsafe { enumerate(self.user_data, root.as_ptr(), &mut error) };
        let Some(json) = self.take_string(json) else {
            let message = self
                .take_string(error)
                .unwrap_or_else(|| "enumerate failed".into());
            return Err(SeamError::failed(message));
        };
        let listed: Vec<BridgedSource> = serde_json::from_str(&json)
            .map_err(|e| SeamError::failed(format!("bridged enumeration is not valid: {e}")))?;
        if listed.len() > SOURCES_PER_ENUMERATION_MAX {
            return Err(SeamError::failed(format!(
                "bridged enumeration returned {} sources; the ceiling is {SOURCES_PER_ENUMERATION_MAX}",
                listed.len()
            )));
        }
        let observed = Timestamp::from(SystemTime::now());
        listed
            .into_iter()
            .map(|source| self.source_from(source, observed))
            .collect()
    }

    fn source_from(
        &self,
        source: BridgedSource,
        observed: Timestamp,
    ) -> Result<EnumeratedSource, SeamError> {
        if source.properties.len() > PROPERTIES_PER_SOURCE_MAX {
            return Err(SeamError::failed(format!(
                "source `{}` carries {} properties; the ceiling is {PROPERTIES_PER_SOURCE_MAX}",
                source.locator,
                source.properties.len()
            )));
        }
        let locator = Locator::new(source.locator)?;
        let content_type = Mimetype::parse(&source.content_type).map_err(|e| {
            SeamError::failed(format!("source `{}` content type: {e}", locator.as_str()))
        })?;
        let properties = source
            .properties
            .into_iter()
            .map(|p| Property {
                key: p.key,
                value: p.value,
                trust: TrustLevel::Claimed,
            })
            .collect();
        let envelope = Envelope {
            source_type: source.source_type,
            content_type,
            length: ContentLength::Bytes(source.bytes),
            created: source.created.map(Timestamp),
            modified: source.modified.map(Timestamp),
            observed,
            properties,
            facets: Vec::new(),
            hint: source.title,
            content_digest: None,
        };
        Ok(EnumeratedSource {
            address: Address::new(self.host_id.clone(), locator),
            envelope,
            raw_bytes: source.bytes,
        })
    }

    fn read_bytes_blocking(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        self.own(address)?;
        let locator = CString::new(address.locator.as_str())
            .map_err(|_| SeamError::failed("a locator may not contain NUL"))?;
        let read_bytes = self.callbacks.read_bytes.ok_or_else(|| {
            SeamError::Unavailable("bridged host has no read_bytes callback".to_string())
        })?;
        let mut len: u64 = 0;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: the callback contract — valid C string, writable length
        // and error slots; the shell returns an owned buffer of `len` bytes
        // or null.
        let bytes = unsafe { read_bytes(self.user_data, locator.as_ptr(), &mut len, &mut error) };
        if bytes.is_null() {
            let message = self
                .take_string(error)
                .unwrap_or_else(|| "read failed".into());
            return Err(SeamError::failed(format!("{address}: {message}")));
        }
        let outcome = if len > BYTES_PER_READ_MAX {
            Err(SeamError::FetchTooLarge {
                address: address.clone(),
                bytes: len,
                limit: BYTES_PER_READ_MAX,
            })
        } else {
            // usize is what the slice API demands; the ceiling above keeps
            // the conversion in range on every supported target.
            let count = usize::try_from(len)
                .map_err(|_| SeamError::failed(format!("{address}: {len} bytes do not fit")))?;
            // SAFETY: the shell promised `len` readable bytes at `bytes`
            // until free_bytes; the copy completes before the free below.
            Ok(unsafe { std::slice::from_raw_parts(bytes, count) }.to_vec())
        };
        if let Some(free_bytes) = self.callbacks.free_bytes {
            // SAFETY: freeing the shell's buffer exactly once, through the
            // shell's own deallocator, with the length it reported.
            unsafe { free_bytes(self.user_data, bytes, len) };
        }
        outcome
    }
}

#[async_trait::async_trait]
impl Connection for BridgedHost {
    async fn enumerate(&self, root: &str) -> Result<Vec<EnumeratedSource>, SeamError> {
        let context = Arc::clone(&self.context);
        let root = root.to_string();
        tokio::task::spawn_blocking(move || context.enumerate_blocking(&root))
            .await
            .map_err(|e| SeamError::failed(format!("bridged enumeration task failed: {e}")))?
    }

    /// Locators are the shell's flat id space; a root is a prefix over it
    /// (`""` is the whole host), so reconciliation covers what the shell's
    /// enumeration of that root covers.
    fn locator_prefix(&self, root: &str) -> Option<String> {
        Some(root.to_string())
    }

    async fn read_text(&self, address: &Address) -> Result<String, SeamError> {
        let bytes = self.read_bytes(address).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn read_lines(
        &self,
        address: &Address,
        start: u64,
        end: u64,
    ) -> Result<String, SeamError> {
        let text = self.read_text(address).await?;
        // Wrapped by message so this compiles whether `slice_lines` reports
        // a `String` or a `SeamError`; drop the wrap once the seam settles.
        slice_lines(&text, start, end).map_err(|e| SeamError::failed(e.to_string()))
    }

    async fn read_bytes(&self, address: &Address) -> Result<Vec<u8>, SeamError> {
        let context = Arc::clone(&self.context);
        let address = address.clone();
        tokio::task::spawn_blocking(move || context.read_bytes_blocking(&address))
            .await
            .map_err(|e| SeamError::failed(format!("bridged read task failed: {e}")))?
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A bridged host written in Rust: an in-memory table behind the same
    //! C callbacks a Swift shell supplies, so the FFI is exercised end to
    //! end without a shell.

    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub struct FakeHost {
        pub sources: BTreeMap<String, (String, Vec<u8>)>,
        pub released: Arc<AtomicU32>,
        pub reads: Mutex<Vec<String>>,
    }

    impl FakeHost {
        pub fn new(sources: &[(&str, &str, &str)]) -> Box<Self> {
            let sources = sources
                .iter()
                .map(|(locator, mimetype, text)| {
                    (
                        (*locator).to_string(),
                        ((*mimetype).to_string(), text.as_bytes().to_vec()),
                    )
                })
                .collect();
            Box::new(Self {
                sources,
                released: Arc::new(AtomicU32::new(0)),
                reads: Mutex::new(Vec::new()),
            })
        }

        pub fn callbacks() -> InseamHostCallbacks {
            InseamHostCallbacks {
                enumerate: Some(enumerate),
                read_bytes: Some(read_bytes),
                free_string: Some(free_string),
                free_bytes: Some(free_bytes),
                release: Some(release),
            }
        }
    }

    unsafe fn host<'a>(user_data: *mut c_void) -> &'a FakeHost {
        // SAFETY: tests pass a leaked Box<FakeHost> as user_data.
        unsafe { &*user_data.cast::<FakeHost>() }
    }

    unsafe extern "C" fn enumerate(
        user_data: *mut c_void,
        root: *const c_char,
        _error_out: *mut *mut c_char,
    ) -> *mut c_char {
        // SAFETY: test contract.
        let host = unsafe { host(user_data) };
        // SAFETY: the bridge passes a valid C string.
        let root = unsafe { CStr::from_ptr(root) }.to_str().unwrap();
        let listed: Vec<serde_json::Value> = host
            .sources
            .iter()
            .filter(|(locator, _)| locator.starts_with(root))
            .map(|(locator, (mimetype, bytes))| {
                serde_json::json!({
                    "locator": locator,
                    "source_type": "note",
                    "content_type": mimetype,
                    "bytes": bytes.len(),
                    "title": locator,
                    "properties": [{"key": "participant", "value": "+15550100"}],
                })
            })
            .collect();
        CString::new(serde_json::to_string(&listed).unwrap())
            .unwrap()
            .into_raw()
    }

    unsafe extern "C" fn read_bytes(
        user_data: *mut c_void,
        locator: *const c_char,
        len_out: *mut u64,
        error_out: *mut *mut c_char,
    ) -> *mut u8 {
        // SAFETY: test contract.
        let host = unsafe { host(user_data) };
        // SAFETY: the bridge passes a valid C string.
        let locator = unsafe { CStr::from_ptr(locator) }.to_str().unwrap();
        host.reads.lock().unwrap().push(locator.to_string());
        match host.sources.get(locator) {
            Some((_, bytes)) => {
                let mut copy = bytes.clone().into_boxed_slice();
                // SAFETY: the bridge passes a writable slot.
                unsafe { *len_out = u64::try_from(copy.len()).expect("fits") };
                let ptr = copy.as_mut_ptr();
                std::mem::forget(copy);
                ptr
            }
            None => {
                // SAFETY: the bridge passes a writable slot.
                unsafe {
                    *error_out = CString::new(format!("no source `{locator}`"))
                        .unwrap()
                        .into_raw()
                };
                std::ptr::null_mut()
            }
        }
    }

    unsafe extern "C" fn free_string(_user_data: *mut c_void, s: *mut c_char) {
        if !s.is_null() {
            // SAFETY: every string this fake hands out came from into_raw.
            drop(unsafe { CString::from_raw(s) });
        }
    }

    unsafe extern "C" fn free_bytes(_user_data: *mut c_void, bytes: *mut u8, len: u64) {
        if !bytes.is_null() {
            // SAFETY: every buffer this fake hands out is a forgotten boxed
            // slice of exactly `len` bytes.
            let count = usize::try_from(len).expect("the fake never hands out more than memory");
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(bytes, count)) });
        }
    }

    unsafe extern "C" fn release(user_data: *mut c_void) {
        // SAFETY: reclaiming the Box the test leaked, exactly once.
        let host = unsafe { Box::from_raw(user_data.cast::<FakeHost>()) };
        host.released.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHost;
    use super::*;

    fn description(kind: &str) -> BridgedHostDescription {
        serde_json::from_value(serde_json::json!({
            "kind": kind,
            "principal": "test-device",
            "display_name": "Test device",
        }))
        .unwrap()
    }

    #[test]
    fn refuses_incomplete_callbacks() {
        let mut callbacks = FakeHost::callbacks();
        callbacks.free_bytes = None;
        callbacks.release = None;
        let host = FakeHost::new(&[]);
        let released = Arc::clone(&host.released);
        let user_data = Box::into_raw(host).cast::<c_void>();
        let error = BridgedHost::new(&description("photos"), callbacks, user_data)
            .err()
            .expect("incomplete callbacks are refused");
        assert!(error.contains("free_bytes"), "{error}");
        assert!(error.contains("release"), "{error}");
        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 0);
        // SAFETY: nothing took ownership; reclaim the leak.
        drop(unsafe { Box::from_raw(user_data.cast::<FakeHost>()) });
    }

    #[test]
    fn refuses_an_empty_principal() {
        let host = FakeHost::new(&[]);
        let user_data = Box::into_raw(host).cast::<c_void>();
        let mut description = description("photos");
        description.principal = "  ".to_string();
        let error = BridgedHost::new(&description, FakeHost::callbacks(), user_data)
            .err()
            .expect("an empty principal is refused");
        assert!(error.contains("principal"), "{error}");
        // SAFETY: nothing took ownership; reclaim the leak.
        drop(unsafe { Box::from_raw(user_data.cast::<FakeHost>()) });
    }

    #[tokio::test]
    async fn enumerates_reads_and_releases_once() {
        let host = FakeHost::new(&[
            (
                "calls/2026-09-01",
                "text/plain",
                "first line\nsecond line\n",
            ),
            ("calls/2026-09-02", "text/plain", "another call"),
            ("photos/1", "image/jpeg", "not really a jpeg"),
        ]);
        let released = Arc::clone(&host.released);
        let user_data = Box::into_raw(host).cast::<c_void>();
        let (desc, bridged) =
            BridgedHost::new(&description("phone"), FakeHost::callbacks(), user_data).unwrap();
        assert!(desc.id.as_str().starts_with("phone-"));

        let calls = bridged.enumerate("calls/").await.unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].envelope.source_type, "note");
        assert_eq!(calls[0].envelope.length, ContentLength::Bytes(23));
        assert_eq!(calls[0].envelope.properties[0].key, "participant");
        assert_eq!(calls[0].envelope.properties[0].trust, TrustLevel::Claimed);
        assert_eq!(bridged.locator_prefix("calls/").as_deref(), Some("calls/"));
        assert_eq!(bridged.enumerate("").await.unwrap().len(), 3);

        let address = calls[0].address.clone();
        assert_eq!(
            bridged.read_lines(&address, 2, 2).await.unwrap(),
            "second line"
        );
        assert_eq!(
            bridged.read_bytes(&calls[1].address).await.unwrap(),
            b"another call"
        );

        let foreign = Address::new(
            HostId::new("fs-elsewhere").unwrap(),
            address.locator.clone(),
        );
        assert!(matches!(
            bridged.read_bytes(&foreign).await,
            Err(SeamError::UnknownHost(_))
        ));
        let missing = Address::new(desc.id.clone(), Locator::new("calls/none").unwrap());
        let error = bridged.read_bytes(&missing).await.unwrap_err().to_string();
        assert!(error.contains("no source `calls/none`"), "{error}");

        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(bridged);
        assert_eq!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "release runs once"
        );
    }
}
