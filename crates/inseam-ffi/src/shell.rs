//! The app shell as a provider (`design/ios-app.md`): capabilities only the
//! shell's process has — an on-device embedding model behind a platform
//! framework — offered to the node through C callbacks, and mounted as a
//! linked plugin the shell's distribution carries. Today: the embedder.
//! The same shape extends to a transcriber or a vision describer later.
//!
//! Threading contract, as for bridged hosts: callbacks run on tokio's
//! blocking pool, on any thread, possibly several at once; `release` runs
//! exactly once when the node that took the shell is freed.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Arc;

use inseam_kernel::store::{EmbeddingIdentity, VectorScope};
use inseam_kernel::substrate::{
    ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, PluginFactory, STORE, parse_config,
};
use inseam_seams::SeamError;
use inseam_seams::embedder::{self, EMBEDDER, Embedder};
use serde::Deserialize;

/// The linked plugin name a composition mounts the shell's embedder under.
pub const EMBEDDER_PLUGIN_NAME: &str = "embedder-app";

/// Widest vector the shell may declare; on-device sentence models are
/// hundreds of dimensions, and the store's ANN copy grows with width.
pub const DIMENSIONS_MAX: u32 = 4_096;

/// Most texts one `embed` call hands the shell.
pub const TEXTS_PER_EMBED_MAX: usize = 1_024;

/// Characters of each text the shell sees; on-device models take short
/// sequences, and the sweep chunks upstream anyway.
pub const EMBED_INPUT_CHARS: usize = 6_000;

/// The shell's embedder callbacks. Mirrored in `include/inseam_ffi.h`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InseamEmbedderCallbacks {
    /// Embed `count` texts, given as a JSON array of strings, into
    /// `count × dimensions` little-endian `f32`s in one buffer, row-major.
    /// `null` with `error_out` set on failure.
    pub embed: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            texts_json: *const c_char,
            count: u32,
            error_out: *mut *mut c_char,
        ) -> *mut f32,
    >,
    pub free_string: Option<unsafe extern "C" fn(user_data: *mut c_void, s: *mut c_char)>,
    pub free_floats:
        Option<unsafe extern "C" fn(user_data: *mut c_void, floats: *mut f32, count: u64)>,
    /// Called exactly once when the node that took the shell is freed.
    pub release: Option<unsafe extern "C" fn(user_data: *mut c_void)>,
}

impl InseamEmbedderCallbacks {
    fn complete(&self) -> Result<(), String> {
        let missing = [
            ("embed", self.embed.is_none()),
            ("free_string", self.free_string.is_none()),
            ("free_floats", self.free_floats.is_none()),
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
                "embedder callbacks are incomplete: {} unset",
                missing.join(", ")
            ))
        }
    }
}

/// How the shell describes its embedder: the identity the store binds the
/// search surface to. Changing the model or width re-embeds, exactly as
/// for any other provider.
#[derive(Debug, Deserialize)]
pub struct ShellEmbedderDescription {
    /// The model identity ("apple/nlcontextualembedding-v1-latin").
    pub model: String,
    pub dimensions: u32,
}

/// The shell's embedder callbacks and context, shared by the plugin
/// instances the factory builds — one node, one shell.
pub struct ShellEmbedder {
    model: String,
    dimensions: u32,
    callbacks: InseamEmbedderCallbacks,
    user_data: *mut c_void,
    /// Whether `release` runs at drop. Cleared when the node fails to
    /// open, so a context the node never took stays the shell's.
    armed: std::sync::atomic::AtomicBool,
}

// SAFETY: `user_data` is opaque to Rust and reaches the shell only through
// callbacks whose contract (module doc) is thread-safety.
unsafe impl Send for ShellEmbedder {}
// SAFETY: as above; the fields are immutable after construction.
unsafe impl Sync for ShellEmbedder {}

impl Drop for ShellEmbedder {
    fn drop(&mut self) {
        let armed = self.armed.load(std::sync::atomic::Ordering::SeqCst);
        if let (true, Some(release)) = (armed, self.callbacks.release) {
            // SAFETY: called exactly once, when the last `Arc` is gone.
            unsafe { release(self.user_data) };
        }
    }
}

impl ShellEmbedder {
    pub fn new(
        description: &ShellEmbedderDescription,
        callbacks: InseamEmbedderCallbacks,
        user_data: *mut c_void,
    ) -> Result<Self, String> {
        callbacks.complete()?;
        if description.model.trim().is_empty() {
            return Err("embedder model may not be empty".to_string());
        }
        if description.dimensions == 0 || description.dimensions > DIMENSIONS_MAX {
            return Err(format!(
                "embedder dimensions must be 1..={DIMENSIONS_MAX}, not {}",
                description.dimensions
            ));
        }
        Ok(Self {
            model: description.model.clone(),
            dimensions: description.dimensions,
            callbacks,
            user_data,
            armed: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Give the context back to the shell without releasing it: the node
    /// never opened, so it never owned `user_data`.
    pub fn disarm(&self) {
        self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
    }

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
            // SAFETY: freeing the shell's string exactly once.
            unsafe { free_string(self.user_data, ptr) };
        }
        Some(owned)
    }

    fn embed_blocking(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, SeamError> {
        assert!(texts.len() <= TEXTS_PER_EMBED_MAX);
        let count = u32::try_from(texts.len())
            .map_err(|_| SeamError::failed("too many texts in one embed call"))?;
        let json = serde_json::to_string(texts)
            .map_err(|e| SeamError::failed(format!("encode embed texts: {e}")))?;
        let json =
            CString::new(json).map_err(|_| SeamError::failed("embed texts may not contain NUL"))?;
        let embed = self.callbacks.embed.ok_or_else(|| {
            SeamError::Unavailable("shell embedder has no embed callback".to_string())
        })?;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: the callback contract — a valid C string and a writable
        // error slot; the shell returns an owned buffer or null.
        let floats = unsafe { embed(self.user_data, json.as_ptr(), count, &mut error) };
        if floats.is_null() {
            let message = self
                .take_string(error)
                .unwrap_or_else(|| "embed failed".into());
            return Err(SeamError::failed(format!("shell embedder: {message}")));
        }
        let width = usize::try_from(self.dimensions).expect("u32 fits usize");
        let total = texts.len() * width;
        // SAFETY: the shell promised `count × dimensions` floats at `floats`
        // until free_floats; the copy completes before the free below.
        let flat = unsafe { std::slice::from_raw_parts(floats, total) }.to_vec();
        if let Some(free_floats) = self.callbacks.free_floats {
            let total = u64::try_from(total).expect("usize fits u64");
            // SAFETY: freeing the shell's buffer exactly once, with the
            // count it was asked for.
            unsafe { free_floats(self.user_data, floats, total) };
        }
        let vectors: Vec<Vec<f32>> = flat.chunks_exact(width).map(<[f32]>::to_vec).collect();
        assert_eq!(vectors.len(), texts.len());
        Ok(vectors)
    }
}

/// The `embedder-app` plugin config: only what the store's identity needs
/// beyond what the shell declared.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ShellEmbedderConfig {
    vectors: VectorScope,
}

/// Builds `embedder-app` entries over the one shell embedder the node was
/// opened with.
pub struct ShellEmbedderFactory {
    shell: Arc<ShellEmbedder>,
}

impl ShellEmbedderFactory {
    pub fn new(shell: Arc<ShellEmbedder>) -> Self {
        Self { shell }
    }
}

impl PluginFactory for ShellEmbedderFactory {
    fn name(&self) -> &str {
        EMBEDDER_PLUGIN_NAME
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        let config: ShellEmbedderConfig = parse_config(config)?;
        Ok(Box::new(ShellEmbedderPlugin {
            shell: Arc::clone(&self.shell),
            vectors: config.vectors,
        }))
    }
}

struct ShellEmbedderPlugin {
    shell: Arc<ShellEmbedder>,
    vectors: VectorScope,
}

#[async_trait::async_trait]
impl Plugin for ShellEmbedderPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("store")];
        Manifest {
            name: EMBEDDER_PLUGIN_NAME,
            inject: INJECT,
            provides: &["embedder"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let store = cx.get(&STORE)?;
        let dimensions = usize::try_from(self.shell.dimensions).expect("u32 fits usize");
        let identity = EmbeddingIdentity {
            model: self.shell.model.clone(),
            dimensions,
            vectors: self.vectors,
        };
        let facts = Facts::new()
            .with(embedder::facts::OFFLINE, true)
            .with(embedder::facts::MODEL, identity.model.as_str())
            .with(
                embedder::facts::DIMENSIONS,
                u64::from(self.shell.dimensions),
            )
            .with(embedder::facts::VECTORS, identity.vectors.as_str());
        store
            .declare_embedding(identity)
            .await
            .map_err(|e| PluginError(e.to_string()))?;
        let store_for_undo = Arc::clone(&store);
        cx.effect("declare embedding identity", move || {
            store_for_undo.withdraw_embedding();
        });
        let implementation: Arc<dyn Embedder> = Arc::new(ShellEmbedderService {
            shell: Arc::clone(&self.shell),
            vectors: self.vectors,
        });
        cx.provide(&EMBEDDER, implementation, facts)?;
        Ok(())
    }
}

struct ShellEmbedderService {
    shell: Arc<ShellEmbedder>,
    vectors: VectorScope,
}

#[async_trait::async_trait]
impl Embedder for ShellEmbedderService {
    fn dimensions(&self) -> Option<usize> {
        Some(usize::try_from(self.shell.dimensions).expect("u32 fits usize"))
    }

    fn vectors(&self) -> VectorScope {
        self.vectors
    }

    /// Bounded per call and per text, then one blocking round trip to the
    /// shell per chunk — the model runs in the shell's process either way.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SeamError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(TEXTS_PER_EMBED_MAX) {
            let bounded: Vec<String> = chunk
                .iter()
                .map(|t| t.chars().take(EMBED_INPUT_CHARS).collect())
                .collect();
            let shell = Arc::clone(&self.shell);
            let mut produced = tokio::task::spawn_blocking(move || shell.embed_blocking(&bounded))
                .await
                .map_err(|e| SeamError::failed(format!("shell embed task failed: {e}")))??;
            vectors.append(&mut produced);
        }
        assert_eq!(vectors.len(), texts.len());
        Ok(vectors)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A shell embedder in Rust: unit vectors keyed by the text's first
    //! character, enough to prove the plumbing and that nearest-neighbor
    //! search sees what the shell produced.

    use super::*;
    use std::sync::atomic::AtomicU32;

    pub const DIMENSIONS: u32 = 8;

    pub struct FakeEmbedder {
        pub released: Arc<AtomicU32>,
        pub calls: AtomicU32,
    }

    impl FakeEmbedder {
        pub fn new() -> Box<Self> {
            Box::new(Self {
                released: Arc::new(AtomicU32::new(0)),
                calls: AtomicU32::new(0),
            })
        }

        pub fn callbacks() -> InseamEmbedderCallbacks {
            InseamEmbedderCallbacks {
                embed: Some(embed),
                free_string: Some(free_string),
                free_floats: Some(free_floats),
                release: Some(release),
            }
        }
    }

    unsafe extern "C" fn embed(
        user_data: *mut c_void,
        texts_json: *const c_char,
        count: u32,
        _error_out: *mut *mut c_char,
    ) -> *mut f32 {
        // SAFETY: tests pass a leaked Box<FakeEmbedder>.
        let fake = unsafe { &*user_data.cast::<FakeEmbedder>() };
        fake.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: the bridge passes a valid C string.
        let json = unsafe { CStr::from_ptr(texts_json) }.to_str().unwrap();
        let texts: Vec<String> = serde_json::from_str(json).unwrap();
        assert_eq!(texts.len(), count as usize);
        let width = DIMENSIONS as usize;
        let mut flat = vec![0.0f32; texts.len() * width];
        for (row, text) in texts.iter().enumerate() {
            let slot = text.bytes().next().map_or(0, |b| usize::from(b) % width);
            flat[row * width + slot] = 1.0;
        }
        let mut boxed = flat.into_boxed_slice();
        let ptr = boxed.as_mut_ptr();
        std::mem::forget(boxed);
        ptr
    }

    unsafe extern "C" fn free_string(_user_data: *mut c_void, s: *mut c_char) {
        if !s.is_null() {
            // SAFETY: every string this fake hands out came from into_raw.
            drop(unsafe { CString::from_raw(s) });
        }
    }

    unsafe extern "C" fn free_floats(_user_data: *mut c_void, floats: *mut f32, count: u64) {
        if !floats.is_null() {
            let count = usize::try_from(count).expect("fits");
            // SAFETY: a forgotten boxed slice of exactly `count` floats.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(floats, count)) });
        }
    }

    unsafe extern "C" fn release(user_data: *mut c_void) {
        // SAFETY: reclaiming the leaked Box exactly once.
        let fake = unsafe { Box::from_raw(user_data.cast::<FakeEmbedder>()) };
        fake.released
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{DIMENSIONS, FakeEmbedder};
    use super::*;
    use std::sync::atomic::Ordering;

    fn description(model: &str, dimensions: u32) -> ShellEmbedderDescription {
        ShellEmbedderDescription {
            model: model.to_string(),
            dimensions,
        }
    }

    #[test]
    fn refuses_a_bad_description_or_missing_callbacks() {
        let fake = FakeEmbedder::new();
        let user_data = Box::into_raw(fake).cast::<c_void>();
        let error = ShellEmbedder::new(&description("", 8), FakeEmbedder::callbacks(), user_data)
            .err()
            .expect("an empty model is refused");
        assert!(error.contains("model"), "{error}");
        let error = ShellEmbedder::new(&description("m", 0), FakeEmbedder::callbacks(), user_data)
            .err()
            .expect("zero width is refused");
        assert!(error.contains("dimensions"), "{error}");
        let error = ShellEmbedder::new(
            &description("m", DIMENSIONS_MAX + 1),
            FakeEmbedder::callbacks(),
            user_data,
        )
        .err()
        .expect("an oversize width is refused");
        assert!(error.contains("dimensions"), "{error}");
        let mut callbacks = FakeEmbedder::callbacks();
        callbacks.embed = None;
        let error = ShellEmbedder::new(&description("m", 8), callbacks, user_data)
            .err()
            .expect("a missing embed callback is refused");
        assert!(error.contains("embed"), "{error}");
        // SAFETY: nothing took ownership; reclaim the leak.
        drop(unsafe { Box::from_raw(user_data.cast::<FakeEmbedder>()) });
    }

    #[tokio::test]
    async fn embeds_in_bounded_chunks_and_releases_once() {
        let fake = FakeEmbedder::new();
        let released = Arc::clone(&fake.released);
        let user_data = Box::into_raw(fake).cast::<c_void>();
        let shell = ShellEmbedder::new(
            &description("fake/unit", DIMENSIONS),
            FakeEmbedder::callbacks(),
            user_data,
        )
        .unwrap();
        let service = ShellEmbedderService {
            shell: Arc::new(shell),
            vectors: VectorScope::All,
        };
        assert_eq!(service.dimensions(), Some(8));

        let texts: Vec<String> = (0..TEXTS_PER_EMBED_MAX + 3)
            .map(|i| format!("{}", i % 10))
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = service.embed(&refs).await.unwrap();
        assert_eq!(vectors.len(), refs.len());
        assert!(vectors.iter().all(|v| v.len() == 8));
        assert_eq!(vectors[0], vectors[10], "equal texts embed equally");
        assert_ne!(vectors[0], vectors[1]);
        // SAFETY: the test still owns the fake through the shell's pointer.
        let calls = unsafe { &*user_data.cast::<FakeEmbedder>() }
            .calls
            .load(Ordering::SeqCst);
        assert_eq!(calls, 2, "one round trip per chunk");

        assert_eq!(released.load(Ordering::SeqCst), 0);
        drop(service);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }
}
