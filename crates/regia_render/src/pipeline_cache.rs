// SPDX-License-Identifier: MPL-2.0
//! Persistent pipeline cache (`docs/specs/13-rendering.md` § 2b, ADR-093).
//!
//! A first launch compiles every pipeline in the GPU driver (seconds on a cold driver cache:
//! mem-stream measured 6–12 s on NVIDIA). Vulkan pipeline caches let the engine keep that work:
//! wgpu 30's [`wgpu::PipelineCache`] (`Features::PIPELINE_CACHE`, Vulkan only) is filled by every
//! pipeline created with it, and its bytes are saved to the user cache directory
//! (`cache://pipelines/`) after warm-up and loaded before the first pipeline of the next launch.
//!
//! # File format (`.fpc`)
//!
//! ```text
//! magic b"FPCH" | version u32 | key_len u32 | key (UTF-8, canonical) | payload_len u64
//! | blake3(payload) [u8; 32] | payload (wgpu PipelineCache::get_data)
//! ```
//!
//! The key holds the adapter's backend, vendor/device id, driver name and version, the wgpu
//! version and the engine shader hash ([`PipelineCacheKey`]). A file whose magic, version, key,
//! length or checksum does not match is discarded (never handed to the driver) and replaced by the
//! next save. wgpu checks its own header (adapter, wgpu build) and the driver checks its own
//! (vendor, device, pipeline-cache UUID) on top of this; `fallback: true` turns any of those
//! rejections into an empty cache. Saves are atomic (temporary file, then rename; the rename
//! retries a momentarily held destination on Windows).
//!
//! # Routing
//!
//! Every pipeline descriptor in the engine sets `cache: pipeline_cache::cache_for(device).as_deref()`.
//! The cache is installed for one device per process ([`install`]). wgpu device handles from two
//! instances compare equal (each instance numbers its own devices), so [`cache_for`] also compares
//! the raw Vulkan devices: a cache handed to another instance's device makes wgpu panic (ADR-2073).
//! A headless device leaves the install with the live device that holds it and is created without
//! `PIPELINE_CACHE`; a window's device takes the install over. The install
//! lasts as long as the [`crate::RenderDevice`] that made it ([`InstallGuard`]): the cache holds
//! the device, and a Vulkan device still alive at process exit can deadlock NVIDIA's Linux
//! driver in its exit handlers (observed: `libEGL_nvidia` exit handler blocked on a PI futex). A device is
//! only created with `PIPELINE_CACHE` when a cache directory is configured
//! ([`set_directory`]); the windowed app configures `cache://pipelines`, headless tools opt in
//! with `REGIA_PIPELINE_CACHE_DIR`. `REGIA_PIPELINE_CACHE=0` turns the cache off.
//!
//! # Driver rule: Intel on Windows
//!
//! Intel's Windows Vulkan driver gets no engine cache ([`driver_skips_cache`], ADR-3288): on an
//! N150 laptop (driver 101.7088) the first compile of the Desktop High pipelines with a
//! `PipelineCache` attached crashed inside the driver (access violation in `igvk64.dll`) on every
//! launch, and the same compile without one succeeded. The driver keeps its own disk shader cache,
//! so a warm launch is as fast without the engine's file (first present 2.3 / 2.7 s without, 2.2 /
//! 3.0 s with). `REGIA_PIPELINE_CACHE=1` attaches the cache anyway, to re-test a new driver.
//!
//! # Shipped caches
//!
//! A game package can carry caches built on the GPUs and drivers it was tested on
//! (`regia package --pipeline-caches DIR`, `<exe dir>/content/pipelines/` in the package). When
//! the local file is missing or unusable, [`PersistentPipelineCache::open`] looks for a shipped
//! file whose key matches this adapter exactly ([`PipelineCacheKey::shipped_file_name`], then the
//! plain [`PipelineCacheKey::file_name`]) and hands its payload to the driver instead: the first
//! launch on a matching GPU and driver compiles almost nothing. The shipped file passes the same
//! checks as a local one (magic, version, full key, length, checksum, then wgpu's and the
//! driver's own headers); anything else is ignored and the launch compiles as before. The
//! first save writes the local file, so later launches no longer read the shipped one.
//! `REGIA_SHIPPED_PIPELINE_CACHE_DIR` overrides the shipped directory ([`shipped_directory`]).
//!
//! # Trust
//!
//! A cache payload goes to the GPU driver, which parses it beyond wgpu's checks, so the file
//! must be one this engine wrote. The checks above (key, length, blake3 checksum) reject stale,
//! foreign and corrupted files; they do not authenticate one. The local cache directory (owned by
//! the user) and the shipped directory (next to the executable, or the directory the environment
//! names) are trusted like the executable itself: whoever can write them can already replace the
//! game's code. Do not point either at a location other users or downloaded content can write.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Magic bytes of a Regia Engine pipeline cache file.
pub const FPCH_MAGIC: [u8; 4] = *b"FPCH";

/// Current pipeline cache file version.
pub const FPCH_VERSION: u32 = 1;

/// Largest cache file the loader accepts (a hostile or runaway file is discarded unread).
pub const FPCH_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Fixed bytes before the key: magic, version, key length.
const PREFIX_LEN: usize = 12;

/// Everything a cached pipeline blob depends on.
///
/// Two launches share a cache only when every field matches. The driver and wgpu validate their
/// own headers as well; this key also drops the cache when the engine's shaders change, so stale
/// entries do not accumulate across engine updates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineCacheKey {
    /// Backend name (`Vulkan`).
    pub backend: String,
    /// PCI vendor id.
    pub vendor: u32,
    /// PCI device id.
    pub device: u32,
    /// Driver name.
    pub driver: String,
    /// Driver version string.
    pub driver_info: String,
    /// wgpu version the cache was produced by.
    pub wgpu_version: String,
    /// Hash of the engine's shader sources and version ([`engine_shader_hash`]) plus the game's
    /// salt ([`set_content_salt`]).
    pub shader_hash: String,
}

impl PipelineCacheKey {
    /// The key for `info`, or `None` on a backend without pipeline caches (everything but Vulkan).
    #[must_use]
    pub fn for_adapter(info: &wgpu::AdapterInfo) -> Option<Self> {
        if !crate::backend_policy::uses_pipeline_cache(info.backend) {
            return None;
        }
        Some(Self {
            backend: format!("{:?}", info.backend),
            vendor: info.vendor,
            device: info.device,
            driver: info.driver.clone(),
            driver_info: info.driver_info.clone(),
            wgpu_version: WGPU_VERSION.to_string(),
            shader_hash: salted_shader_hash(),
        })
    }

    /// File name of this key's cache: one file per backend × adapter (a driver update replaces
    /// the file instead of adding one).
    #[must_use]
    pub fn file_name(&self) -> String {
        format!(
            "pipelines-{}-{:04x}-{:04x}.fpc",
            self.backend.to_ascii_lowercase(),
            self.vendor,
            self.device
        )
    }

    /// File name of this key's cache inside a package's shipped directory: the
    /// [`Self::file_name`] stem plus a hash of the whole key, so one package holds a file per
    /// adapter × driver × shader set.
    #[must_use]
    pub fn shipped_file_name(&self) -> String {
        let hash = blake3::hash(self.canonical().as_bytes()).to_hex();
        format!(
            "pipelines-{}-{:04x}-{:04x}-{}.fpc",
            self.backend.to_ascii_lowercase(),
            self.vendor,
            self.device,
            &hash[..16]
        )
    }

    /// Parses [`Self::canonical`] text back (`None` when a field is missing or malformed).
    #[must_use]
    pub fn parse_canonical(text: &str) -> Option<Self> {
        let mut fields = std::collections::HashMap::new();
        for line in text.lines() {
            let (name, value) = line.split_once('=')?;
            fields.insert(name, value);
        }
        let hex = |name: &str| {
            fields
                .get(name)
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        };
        let key = Self {
            backend: (*fields.get("backend")?).to_string(),
            vendor: hex("vendor")?,
            device: hex("device")?,
            driver: (*fields.get("driver")?).to_string(),
            driver_info: (*fields.get("driver_info")?).to_string(),
            wgpu_version: (*fields.get("wgpu")?).to_string(),
            shader_hash: (*fields.get("shaders")?).to_string(),
        };
        (key.canonical() == text).then_some(key)
    }

    /// Canonical text form stored in the file header.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "backend={}\nvendor={:#x}\ndevice={:#x}\ndriver={}\ndriver_info={}\nwgpu={}\nshaders={}\n",
            self.backend,
            self.vendor,
            self.device,
            self.driver,
            self.driver_info,
            self.wgpu_version,
            self.shader_hash
        )
    }
}

/// The wgpu version the engine links (bump with the workspace pin; part of the cache key).
pub const WGPU_VERSION: &str = "30";

/// Hash of every engine shader source (32 hex digits), baked by `build.rs`: all WGSL of the
/// engine's shader crates (`regia_render`, `regia_terrain`, `regia_vfx`, `regia_ui`,
/// `regia_2d`, `regia_pt`, `regia_matgraph`) and every Rust source there that carries WGSL
/// text (the `shader_lib` composer, inline and generated shaders). See `src/shader_sources.rs`.
const ENGINE_SHADER_SOURCES: &str = env!("REGIA_ENGINE_SHADER_SOURCES");

/// Bumped when an engine pipeline layout changes in Rust alone (a WGSL edit already changes
/// [`engine_shader_hash`] through the shader sources). 1: the material constants binding is a
/// storage buffer on tier `MaterialSsbo` devices (bindless step 1, ADR-316).
pub const PIPELINE_LAYOUT_EPOCH: u32 = 1;

/// Hash of the engine's shader sources, pipeline layout epoch and crate version (16 hex digits).
///
/// Covers every engine shader that can reach a pipeline, so any engine WGSL change starts a fresh
/// cache file and `regia package` flags caches built before it. Material-graph shaders built
/// from game content are not engine sources: the game's [`set_content_salt`] covers them.
#[must_use]
pub fn engine_shader_hash() -> String {
    let mut h = blake3::Hasher::new();
    h.update(env!("CARGO_PKG_VERSION").as_bytes());
    h.update(ENGINE_SHADER_SOURCES.as_bytes());
    h.update(&PIPELINE_LAYOUT_EPOCH.to_le_bytes());
    h.finalize().to_hex()[..16].to_string()
}

/// `engine_shader_hash()` folded with the current content salt (the empty salt leaves it
/// unchanged). Backends without pipeline caches have no [`PipelineCacheKey`]; their stamp
/// keys (prepare) fold this so a game update with new material graphs still misses.
pub(crate) fn salted_shader_hash() -> String {
    let salt = SALT.lock().map(|s| s.clone()).unwrap_or_default();
    if salt.is_empty() {
        return engine_shader_hash();
    }
    let mut h = blake3::Hasher::new();
    h.update(engine_shader_hash().as_bytes());
    h.update(salt.as_bytes());
    h.finalize().to_hex()[..16].to_string()
}

/// Why a cache file was not used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    /// Shorter than its fixed header.
    Truncated,
    /// Not an `FPCH` file.
    BadMagic,
    /// Written by another cache file version.
    Version(u32),
    /// Written for another adapter, driver, wgpu or shader set.
    KeyMismatch,
    /// Declared payload length disagrees with the file.
    Length,
    /// Payload bytes do not match the stored checksum (corruption, torn write).
    Checksum,
    /// Larger than [`FPCH_MAX_BYTES`].
    TooLarge,
    /// The file could not be read.
    Io(String),
}

impl std::fmt::Display for DiscardReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated"),
            Self::BadMagic => f.write_str("bad magic"),
            Self::Version(v) => write!(f, "version {v} (expected {FPCH_VERSION})"),
            Self::KeyMismatch => f.write_str("adapter/driver/shader key mismatch"),
            Self::Length => f.write_str("payload length mismatch"),
            Self::Checksum => f.write_str("checksum mismatch"),
            Self::TooLarge => write!(f, "larger than {FPCH_MAX_BYTES} bytes"),
            Self::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

/// Encodes `payload` (wgpu cache bytes) for `key`.
#[must_use]
pub fn encode(key: &PipelineCacheKey, payload: &[u8]) -> Vec<u8> {
    let key = key.canonical();
    let mut out = Vec::with_capacity(PREFIX_LEN + key.len() + 40 + payload.len());
    out.extend_from_slice(&FPCH_MAGIC);
    out.extend_from_slice(&FPCH_VERSION.to_le_bytes());
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(blake3::hash(payload).as_bytes());
    out.extend_from_slice(payload);
    out
}

/// Reads the key stored in a cache file's header without matching it against an adapter
/// (packaging, inspection). Checks the magic, the version and that the key is canonical.
///
/// # Errors
///
/// [`DiscardReason`] when the header is not a valid one.
pub fn read_key(bytes: &[u8]) -> Result<PipelineCacheKey, DiscardReason> {
    let prefix = bytes.get(..PREFIX_LEN).ok_or(DiscardReason::Truncated)?;
    if prefix[..4] != FPCH_MAGIC {
        return Err(DiscardReason::BadMagic);
    }
    let version = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
    if version != FPCH_VERSION {
        return Err(DiscardReason::Version(version));
    }
    let key_len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]) as usize;
    let key_end = PREFIX_LEN.checked_add(key_len).ok_or(DiscardReason::Truncated)?;
    let stored = bytes.get(PREFIX_LEN..key_end).ok_or(DiscardReason::Truncated)?;
    std::str::from_utf8(stored)
        .ok()
        .and_then(PipelineCacheKey::parse_canonical)
        .ok_or(DiscardReason::KeyMismatch)
}

/// Validates a cache file against `key` and returns its payload.
///
/// Never panics: every length is checked before it is used (fuzzed in this module's tests).
///
/// # Errors
///
/// [`DiscardReason`] when any field does not match.
pub fn decode<'a>(bytes: &'a [u8], key: &PipelineCacheKey) -> Result<&'a [u8], DiscardReason> {
    if bytes.len() as u64 > FPCH_MAX_BYTES {
        return Err(DiscardReason::TooLarge);
    }
    let prefix = bytes.get(..PREFIX_LEN).ok_or(DiscardReason::Truncated)?;
    if prefix[..4] != FPCH_MAGIC {
        return Err(DiscardReason::BadMagic);
    }
    let version = u32::from_le_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
    if version != FPCH_VERSION {
        return Err(DiscardReason::Version(version));
    }
    let key_len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]) as usize;
    let key_end = PREFIX_LEN.checked_add(key_len).ok_or(DiscardReason::Truncated)?;
    let stored_key = bytes.get(PREFIX_LEN..key_end).ok_or(DiscardReason::Truncated)?;
    if stored_key != key.canonical().as_bytes() {
        return Err(DiscardReason::KeyMismatch);
    }
    let len_end = key_end + 8;
    let len_bytes = bytes.get(key_end..len_end).ok_or(DiscardReason::Truncated)?;
    let mut len = [0u8; 8];
    len.copy_from_slice(len_bytes);
    let payload_len = u64::from_le_bytes(len);
    let hash_end = len_end + 32;
    let hash = bytes.get(len_end..hash_end).ok_or(DiscardReason::Truncated)?;
    let payload = &bytes[hash_end..];
    if payload.len() as u64 != payload_len {
        return Err(DiscardReason::Length);
    }
    if blake3::hash(payload).as_bytes() != hash {
        return Err(DiscardReason::Checksum);
    }
    Ok(payload)
}

/// What happened when the cache was opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheLoad {
    /// No file yet (first launch on this adapter/driver).
    Missing,
    /// A valid file of this many payload bytes was handed to wgpu.
    Loaded(u64),
    /// A file existed but was not used.
    Discarded(DiscardReason),
    /// No usable local file; a shipped cache of this many payload bytes, built on the same
    /// adapter, driver and shader set, was handed to wgpu instead ([`ShippedLoad::Seeded`]).
    Seeded(u64),
}

/// What the shipped cache directory gave (only consulted when the local file was missing or
/// discarded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShippedLoad {
    /// No shipped file for this adapter, driver and shader set: the launch compiles.
    NoMatch,
    /// This shipped file seeded the cache.
    Seeded(String),
    /// A shipped file under this adapter's name failed validation and was ignored.
    Discarded(String, DiscardReason),
}

/// Chooses the payload a new cache starts from: the local file at `path` if it is valid, else a
/// shipped file in `shipped` whose key matches `key` (see the module docs). Never panics on any
/// file content; every rejected file is reported, never handed to the driver.
#[must_use]
pub fn initial_payload(
    path: &Path,
    shipped: Option<&Path>,
    key: &PipelineCacheKey,
) -> (Option<Vec<u8>>, CacheLoad, Option<ShippedLoad>) {
    let local = match read_capped(path) {
        Ok(Some(bytes)) => match decode(&bytes, key) {
            Ok(payload) => {
                let n = payload.len() as u64;
                return (Some(payload.to_vec()), CacheLoad::Loaded(n), None);
            }
            Err(reason) => CacheLoad::Discarded(reason),
        },
        Ok(None) => CacheLoad::Missing,
        Err(reason) => CacheLoad::Discarded(reason),
    };
    let Some(dir) = shipped else {
        return (None, local, None);
    };
    let mut shipped_load = ShippedLoad::NoMatch;
    for name in [key.shipped_file_name(), key.file_name()] {
        let candidate = dir.join(&name);
        match read_capped(&candidate) {
            Ok(None) => continue,
            Ok(Some(bytes)) => match decode(&bytes, key) {
                Ok(payload) => {
                    let n = payload.len() as u64;
                    return (
                        Some(payload.to_vec()),
                        CacheLoad::Seeded(n),
                        Some(ShippedLoad::Seeded(candidate.display().to_string())),
                    );
                }
                Err(reason) => shipped_load = ShippedLoad::Discarded(candidate.display().to_string(), reason),
            },
            Err(reason) => shipped_load = ShippedLoad::Discarded(candidate.display().to_string(), reason),
        }
    }
    (None, local, Some(shipped_load))
}

/// A pipeline cache bound to a file in the cache directory.
pub struct PersistentPipelineCache {
    cache: wgpu::PipelineCache,
    path: PathBuf,
    key: PipelineCacheKey,
    load: CacheLoad,
    shipped: Option<ShippedLoad>,
    load_ms: f32,
    saved_bytes: AtomicU64,
    saves: AtomicU64,
    /// [`pipelines_created`] at the last save (skip saves with nothing new).
    creates_at_save: AtomicU64,
    save_lock: Mutex<()>,
}

impl PersistentPipelineCache {
    /// Opens (or starts) the cache for `device` in `dir`. `None` when `device` was created
    /// without `Features::PIPELINE_CACHE` or the backend has no pipeline caches.
    #[must_use]
    pub fn open(device: &wgpu::Device, info: &wgpu::AdapterInfo, dir: &Path) -> Option<Self> {
        let _span = tracing::info_span!("pipeline_cache_open").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PIPELINE_CACHE);
        if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
            return None;
        }
        let key = PipelineCacheKey::for_adapter(info)?;
        let path = dir.join(key.file_name());
        let start = std::time::Instant::now();
        let shipped_dir = shipped_directory();
        let (data, load, shipped) = initial_payload(&path, shipped_dir.as_deref(), &key);
        // SAFETY: wgpu requires `data` to come from `PipelineCache::get_data`; its own docs note
        // that a blob saved to disk cannot fully prove that. `data` is `None` or bytes that
        // passed `decode`: a file under this exact key (adapter, driver, wgpu version, shader
        // set) whose payload matches its blake3 checksum. The checksum catches corruption
        // (truncation, bit rot, a partial write), not forgery: anyone who can write the user
        // cache directory or the game's install directory can recompute it. Both directories
        // are trusted at the level of the executable and its libraries (see "Trust" in the
        // module docs). On top, wgpu checks its header, the driver checks its own (vendor,
        // device, pipeline-cache UUID), and `fallback: true` turns a rejection into an empty
        // cache.
        #[allow(unsafe_code)]
        let cache = unsafe {
            device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
                label: Some("regia pipeline cache"),
                data: data.as_deref(),
                fallback: true,
            })
        };
        let load_ms = start.elapsed().as_secs_f32() * 1000.0;
        match &shipped {
            Some(ShippedLoad::Seeded(from)) => tracing::info!(
                target: "regia_render",
                "pipeline cache seeded from shipped {from}"
            ),
            Some(ShippedLoad::Discarded(from, reason)) => tracing::warn!(
                target: "regia_render",
                "shipped pipeline cache {from} ignored ({reason}); compiling"
            ),
            Some(ShippedLoad::NoMatch) => tracing::info!(
                target: "regia_render",
                "no shipped pipeline cache for this GPU/driver ({}); compiling",
                key.shipped_file_name()
            ),
            None => {}
        }
        match &load {
            CacheLoad::Discarded(reason) => tracing::warn!(
                target: "regia_render",
                "pipeline cache {} discarded ({reason}); starting empty",
                path.display()
            ),
            other => tracing::info!(
                target: "regia_render",
                "pipeline cache {}: {other:?} in {load_ms:.1} ms",
                path.display()
            ),
        }
        Some(Self {
            cache,
            path,
            key,
            load,
            shipped,
            load_ms,
            saved_bytes: AtomicU64::new(0),
            saves: AtomicU64::new(0),
            creates_at_save: AtomicU64::new(0),
            save_lock: Mutex::new(()),
        })
    }

    /// The wgpu cache every pipeline descriptor passes.
    #[must_use]
    pub fn cache(&self) -> &wgpu::PipelineCache {
        &self.cache
    }

    /// The cache file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The key the file is validated against.
    #[must_use]
    pub fn key(&self) -> &PipelineCacheKey {
        &self.key
    }

    /// What opening the file found.
    #[must_use]
    pub fn load(&self) -> &CacheLoad {
        &self.load
    }

    /// What the shipped directory gave (`None`: not consulted, the local file was valid or no
    /// shipped directory exists).
    #[must_use]
    pub fn shipped(&self) -> Option<&ShippedLoad> {
        self.shipped.as_ref()
    }

    /// Writes the cache atomically (temporary file + rename) when pipelines were created since
    /// the last save. Returns the bytes written (`0`: nothing new or no data from the backend).
    ///
    /// Call off the frame path: `get_data` copies the driver's whole cache.
    ///
    /// # Errors
    ///
    /// The directory or file could not be written.
    pub fn save(&self) -> std::io::Result<u64> {
        let _span = tracing::info_span!("pipeline_cache_save").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PIPELINE_CACHE);
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let creates = pipelines_created();
        if self.saves.load(Ordering::Relaxed) > 0 && creates == self.creates_at_save.load(Ordering::Relaxed) {
            return Ok(0);
        }
        let Some(payload) = self.cache.get_data() else {
            return Ok(0);
        };
        let bytes = encode(&self.key, &payload);
        write_atomic(&self.path, &bytes)?;
        self.saved_bytes.store(bytes.len() as u64, Ordering::Relaxed);
        self.saves.fetch_add(1, Ordering::Relaxed);
        self.creates_at_save.store(creates, Ordering::Relaxed);
        tracing::info!(
            target: "regia_render",
            "pipeline cache saved: {} bytes ({} pipelines created this run) to {}",
            bytes.len(),
            creates,
            self.path.display()
        );
        let (timed, time) = crate::device::pipeline_create_time();
        if timed > 0 {
            // A separate line: hosts parse the one above.
            tracing::info!(
                target: "regia_render",
                "pipeline creation this run: {timed} pipelines, {:.0} ms on the creating threads, {:.1} ms each",
                time.as_secs_f64() * 1000.0,
                time.as_secs_f64() * 1000.0 / timed as f64
            );
        }
        Ok(bytes.len() as u64)
    }

    /// The bytes [`save`](Self::save) would write, read from the driver now, for writing after
    /// the device is gone ([`PendingCacheSave::write`]; the clean exit drops the device first).
    /// `None` when nothing is new since the last save or the driver has no data.
    #[must_use]
    pub fn snapshot(&self) -> Option<PendingCacheSave> {
        let _span = tracing::info_span!("pipeline_cache_snapshot").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PIPELINE_CACHE);
        let _guard = self
            .save_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let creates = pipelines_created();
        if self.saves.load(Ordering::Relaxed) > 0 && creates == self.creates_at_save.load(Ordering::Relaxed) {
            return None;
        }
        let payload = self.cache.get_data()?;
        Some(PendingCacheSave {
            path: self.path.clone(),
            bytes: encode(&self.key, &payload),
            creates,
        })
    }

    /// Status for progress reports and the agent protocol.
    #[must_use]
    pub fn status(&self) -> PipelineCacheStatus {
        PipelineCacheStatus {
            enabled: true,
            path: Some(self.path.display().to_string()),
            load: Some(self.load.clone()),
            shipped: self.shipped.clone(),
            load_ms: self.load_ms,
            saved_bytes: self.saved_bytes.load(Ordering::Relaxed),
            saves: self.saves.load(Ordering::Relaxed),
        }
    }
}

/// Pipeline cache bytes read while the device lived, not yet on disk
/// ([`PersistentPipelineCache::snapshot`]).
#[derive(Debug, Clone)]
pub struct PendingCacheSave {
    path: PathBuf,
    bytes: Vec<u8>,
    creates: u64,
}

impl PendingCacheSave {
    /// Size of the file it writes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Nothing to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Writes the cache file (atomically, like [`PersistentPipelineCache::save`]); returns its size.
    pub fn write(self) -> std::io::Result<u64> {
        let _span = tracing::info_span!("pipeline_cache_save").entered();
        let _mem = regia_core::memory::enter_owner(&crate::heap_owners::PIPELINE_CACHE);
        write_atomic(&self.path, &self.bytes)?;
        tracing::info!(
            target: "regia_render",
            "pipeline cache saved: {} bytes ({} pipelines created this run) to {}",
            self.bytes.len(),
            self.creates,
            self.path.display()
        );
        Ok(self.bytes.len() as u64)
    }
}

/// Persistent cache state (see [`status`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PipelineCacheStatus {
    /// A cache is installed for the render device.
    pub enabled: bool,
    /// The cache file.
    pub path: Option<String>,
    /// What opening found.
    pub load: Option<CacheLoad>,
    /// What the shipped directory gave, when it was consulted.
    pub shipped: Option<ShippedLoad>,
    /// Time to read, validate and hand the file to the driver.
    pub load_ms: f32,
    /// Size of the last save.
    pub saved_bytes: u64,
    /// Saves this run.
    pub saves: u64,
}

/// Reads `path` if it exists, refusing files over [`FPCH_MAX_BYTES`].
fn read_capped(path: &Path) -> Result<Option<Vec<u8>>, DiscardReason> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DiscardReason::Io(e.to_string())),
        Ok(meta) if meta.len() > FPCH_MAX_BYTES => return Err(DiscardReason::TooLarge),
        Ok(_) => {}
    }
    // Stream with a hard cap: a file swapped for a larger one after the stat above cannot
    // allocate past the limit.
    read_at_most(path, FPCH_MAX_BYTES)
}

/// Reads `path` in full, `Err(TooLarge)` when it holds more than `cap` bytes.
fn read_at_most(path: &Path, cap: u64) -> Result<Option<Vec<u8>>, DiscardReason> {
    use std::io::Read as _;
    let io = |e: std::io::Error| DiscardReason::Io(e.to_string());
    let file = std::fs::File::open(path).map_err(io)?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > cap {
        return Err(DiscardReason::TooLarge);
    }
    Ok(Some(bytes))
}

/// Writes `bytes` to `path` through a temporary sibling and a rename, so a crash mid-write
/// leaves either the old file or the new one, never a torn one. The rename replaces an existing
/// destination and retries a momentarily held one on Windows ([`rename_replace`]).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_with(path, bytes, rename_replace)
}

/// [`write_atomic`] with `on_retry` invoked before each backoff sleep of the rename; the
/// Windows held-destination test releases the blocking handle from there while driving the
/// whole write (temp file, sync, rename, temp cleanup), so the wiring `write_atomic` runs is
/// what the test exercises.
#[cfg(all(windows, test))]
fn write_atomic_hooked(path: &Path, bytes: &[u8], on_retry: impl FnMut(u32)) -> std::io::Result<()> {
    write_atomic_with(path, bytes, |from, to| rename_replace_hooked(from, to, on_retry))
}

/// The body of [`write_atomic`]: write `bytes` to a temporary sibling of `path`, sync it,
/// then `rename` it over `path` (replacing an existing destination), removing the temporary
/// when the rename fails.
fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("fpc.tmp{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Renames `from` over `to`, replacing an existing target. Atomic on Linux and Android. On
/// Windows a virus scanner, indexer or another reader can hold the target open for a moment,
/// which fails the replace with a sharing violation; those are retried a few times with a short
/// backoff before the error is returned (the same retry `regia_platform`'s VFS `rename_replace`
/// uses; the write is never torn either way).
#[cfg(any(windows, test))]
fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    rename_replace_hooked(from, to, |_| {})
}

/// [`rename_replace`] with `on_retry` invoked before each backoff sleep; tests release the
/// handle that blocks the replace from there.
#[cfg(any(windows, test))]
fn rename_replace_hooked(from: &Path, to: &Path, on_retry: impl FnMut(u32)) -> std::io::Result<()> {
    with_sharing_retry(|| std::fs::rename(from, to), on_retry)
}

/// Runs `op`, retrying a transient sharing failure up to six attempts with a short backoff
/// between them. Only exists on Windows (and in test builds, where the retry semantics are
/// covered cross-platform); other builds call `std::fs::rename` directly.
#[cfg(any(windows, test))]
fn with_sharing_retry(
    mut op: impl FnMut() -> std::io::Result<()>,
    mut on_retry: impl FnMut(u32),
) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 6;
    let mut attempt = 0;
    loop {
        match op() {
            Err(err) if attempt + 1 < ATTEMPTS && is_transient_sharing_error(&err) => {
                attempt += 1;
                on_retry(attempt);
                std::thread::sleep(std::time::Duration::from_millis(5 << attempt));
            }
            other => return other,
        }
    }
}

/// Renames `from` over `to`, replacing an existing target. Atomic on Linux and Android; on
/// those targets a rename failure is final, so no retry exists there.
#[cfg(not(any(windows, test)))]
fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// Whether `err` is a Windows access denied (5), sharing violation (32) or lock violation (33),
/// which clear once the other holder closes the file.
#[cfg(any(windows, test))]
fn is_transient_sharing_error(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

// ---------------------------------------------------------------------------------------------
// Process-wide configuration and the installed cache.
// ---------------------------------------------------------------------------------------------

/// Where the cache lives; `None` = not configured (no cache).
static DIRECTORY: RwLock<Option<PathBuf>> = RwLock::new(None);
/// Where shipped caches live; `None` = the default (`<exe dir>/content/pipelines`).
static SHIPPED_DIRECTORY: RwLock<Option<PathBuf>> = RwLock::new(None);
/// Extra key material from the game (its own shader/content version).
static SALT: Mutex<String> = Mutex::new(String::new());
/// The installed cache and the device it belongs to.
static INSTALLED: RwLock<Option<(wgpu::Device, Arc<PersistentPipelineCache>)>> = RwLock::new(None);
/// Pipelines created through [`cache_for`] this process (every engine creation site).
static CREATED: AtomicU64 = AtomicU64::new(0);
/// Set when the render device was created without a cache by [`driver_skips_cache`].
static DRIVER_SKIPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The environment variable that turns the cache off (`0`) or attaches it despite a driver rule
/// (`1`, [`driver_skips_cache`]).
pub const PIPELINE_CACHE_ENV: &str = "REGIA_PIPELINE_CACHE";

/// PCI vendor id of Intel.
pub const INTEL_VENDOR: u32 = 0x8086;

/// Why [`driver_skips_cache`] leaves a device without a cache, as the agent's `platform.paths`
/// (`pipeline_cache.skipped`) and `render.pipeline_warm` (`cache.skipped`) report it.
pub const DRIVER_SKIP_REASON: &str = "Intel Vulkan driver on Windows: no engine pipeline cache (the driver's own disk cache is used; ADR-3288)";

/// Whether a Vulkan device of PCI `vendor` on `os` (spelled as [`std::env::consts::OS`]) is
/// created without the engine's pipeline cache: Intel on Windows (ADR-3288; module docs). `env`
/// is the value of [`PIPELINE_CACHE_ENV`]: `1`/`true` attaches the cache anyway; `0` is handled
/// by [`directory`] (no cache on any device).
#[must_use]
pub fn driver_skips_cache(vendor: u32, os: &str, env: Option<&str>) -> bool {
    match env.map(str::trim) {
        Some("1" | "true") => false,
        _ => os == "windows" && vendor == INTEL_VENDOR,
    }
}

/// [`driver_skips_cache`] for the adapter a device is being created on, with this process's OS and
/// environment. Records the answer for [`driver_skipped`] and logs a skip once per device.
pub(crate) fn driver_skips_cache_for(info: &wgpu::AdapterInfo) -> bool {
    let env = regia_core::brand::env_var(PIPELINE_CACHE_ENV).ok();
    let skip = driver_skips_cache(info.vendor, std::env::consts::OS, env.as_deref());
    DRIVER_SKIPPED.store(skip, Ordering::Relaxed);
    if skip {
        tracing::info!(
            target: "regia_render",
            "pipeline cache: not used on {} ({DRIVER_SKIP_REASON}); {PIPELINE_CACHE_ENV}=1 attaches it",
            info.name
        );
    }
    skip
}

/// Whether the last render device was created without a cache by [`driver_skips_cache`].
#[must_use]
pub fn driver_skipped() -> bool {
    DRIVER_SKIPPED.load(Ordering::Relaxed)
}

/// Sets the directory the next render device keeps its cache in (`None`: no cache).
///
/// The windowed app sets `cache://pipelines` before the first device is created;
/// `REGIA_PIPELINE_CACHE_DIR` overrides it and `REGIA_PIPELINE_CACHE=0` disables it.
pub fn set_directory(dir: Option<PathBuf>) {
    if let Ok(mut slot) = DIRECTORY.write() {
        *slot = dir;
    }
}

/// Sets the directory shipped caches are read from (default: `<exe dir>/content/pipelines`, where
/// `regia package --pipeline-caches` puts them). Call before the device is created.
pub fn set_shipped_directory(dir: Option<PathBuf>) {
    if let Ok(mut slot) = SHIPPED_DIRECTORY.write() {
        *slot = dir;
    }
}

/// The shipped cache directory: `REGIA_SHIPPED_PIPELINE_CACHE_DIR` (empty = none), else
/// [`set_shipped_directory`], else `<exe dir>/content/pipelines` when it exists (desktop packages).
#[must_use]
pub fn shipped_directory() -> Option<PathBuf> {
    if let Some(dir) = regia_core::brand::env_var_os("REGIA_SHIPPED_PIPELINE_CACHE_DIR") {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    if let Some(dir) = SHIPPED_DIRECTORY.read().ok().and_then(|d| d.clone()) {
        return Some(dir);
    }
    if cfg!(target_os = "android") {
        return None;
    }
    let dir = std::env::current_exe()
        .ok()?
        .parent()?
        .join("content")
        .join("pipelines");
    dir.is_dir().then_some(dir)
}

/// Folds the game's own shader/content version into the cache key, so a game update with new
/// material graphs starts a fresh file. Call before the device is created.
pub fn set_content_salt(salt: &str) {
    if let Ok(mut s) = SALT.lock() {
        *s = salt.to_string();
    }
}

/// The configured directory after the environment overrides.
#[must_use]
pub fn directory() -> Option<PathBuf> {
    directory_from(
        regia_core::brand::env_var(PIPELINE_CACHE_ENV).ok().as_deref(),
        regia_core::brand::env_var_os("REGIA_PIPELINE_CACHE_DIR"),
        || DIRECTORY.read().ok().and_then(|d| d.clone()),
    )
}

/// [`directory`] for given environment values: `REGIA_PIPELINE_CACHE=0` turns the cache off, a
/// non-empty `REGIA_PIPELINE_CACHE_DIR` names the directory, else the configured one.
fn directory_from(
    cache: Option<&str>,
    dir: Option<std::ffi::OsString>,
    configured: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if cache == Some("0") {
        return None;
    }
    if let Some(dir) = dir.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    configured()
}

/// Installs `cache` for `device`: from now on [`cache_for`]`(device)` returns it. A later
/// install (a window's device, a device recreated after a loss) replaces it; the device that held
/// it keeps working without a cache. Dropping the returned guard uninstalls
/// it again if it is still the installed one ([`crate::RenderDevice`] keeps the guard, so the
/// install ends with the last clone of the device that made it).
pub fn install(device: &wgpu::Device, cache: PersistentPipelineCache) -> InstallGuard {
    let cache = Arc::new(cache);
    if let Ok(mut slot) = INSTALLED.write() {
        *slot = Some((device.clone(), cache.clone()));
    }
    InstallGuard(Arc::downgrade(&cache))
}

/// Uninstalls the cache [`install`] installed when dropped, unless another install replaced it.
#[derive(Debug)]
pub struct InstallGuard(std::sync::Weak<PersistentPipelineCache>);

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = INSTALLED.write()
            && slot
                .as_ref()
                .is_some_and(|(_, cache)| std::ptr::eq(Arc::as_ptr(cache), self.0.as_ptr()))
        {
            *slot = None;
        }
    }
}

/// Removes the installed cache (tests, device teardown). Pipelines being created right now keep
/// their [`PipelineCacheRef`] until their descriptor is done; the cache and its device are freed
/// after that.
pub fn uninstall() {
    if let Ok(mut slot) = INSTALLED.write() {
        *slot = None;
    }
}

/// The installed persistent cache, if any.
#[must_use]
pub fn installed() -> Option<Arc<PersistentPipelineCache>> {
    INSTALLED
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().map(|(_, c)| c.clone()))
}

/// Whether a live device holds the install, so a new headless device should leave it there. A lost
/// device does not count: nothing it holds is worth keeping ([`crate::device_lost`]).
pub(crate) fn held_by_live_device() -> bool {
    !crate::gpu_health::device_lost() && INSTALLED.read().is_ok_and(|slot| slot.is_some())
}

/// Whether `a` and `b` are one device. `wgpu::Device` equality alone cannot say: it compares the
/// number the device's instance gave it, and the first device of every instance has the same
/// number. The raw Vulkan device is unique while `a` and `b` hold it (the only backend with a cache,
/// [`crate::backend_policy::uses_pipeline_cache`]).
///
/// Headless devices of one descriptor share one instance (ADR-7557), where `==` alone would do,
/// but a window's instance and a headless instance with other flags are separate instances whose
/// first devices still compare equal in wgpu 30, so the raw check stays (ADR-7643).
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
fn same_device(a: &wgpu::Device, b: &wgpu::Device) -> bool {
    if a != b {
        return false;
    }
    // SAFETY: the hal device guards are only read, for the handles compared here; nothing is
    // created or destroyed, and both guards drop before this function returns, while `a` and `b`
    // keep their devices alive. Another backend's device gives `None`.
    #[allow(unsafe_code)]
    unsafe {
        match (
            a.as_hal::<wgpu::hal::api::Vulkan>(),
            b.as_hal::<wgpu::hal::api::Vulkan>(),
        ) {
            (Some(a), Some(b)) => a.raw_device().handle() == b.raw_device().handle(),
            _ => false,
        }
    }
}

/// No Vulkan backend is built here, so no cache is ever installed.
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "android")))]
fn same_device(_: &wgpu::Device, _: &wgpu::Device) -> bool {
    false
}

/// The installed cache, borrowed for one pipeline descriptor: `cache_for(device).as_deref()`.
#[derive(Clone)]
pub struct PipelineCacheRef(Arc<PersistentPipelineCache>);

impl std::fmt::Debug for PipelineCacheRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PipelineCacheRef").field(&self.0.path).finish()
    }
}

impl std::ops::Deref for PipelineCacheRef {
    type Target = wgpu::PipelineCache;

    fn deref(&self) -> &wgpu::PipelineCache {
        self.0.cache()
    }
}

/// Anything a pipeline can be created on: [`wgpu::Device`], [`crate::TrackedDevice`],
/// [`crate::RenderDevice`] and references to them (so every creation site passes its device
/// handle as it holds it).
pub trait PipelineDevice {
    /// The wgpu device.
    fn wgpu_device(&self) -> &wgpu::Device;
}

impl PipelineDevice for wgpu::Device {
    fn wgpu_device(&self) -> &wgpu::Device {
        self
    }
}

impl PipelineDevice for crate::TrackedDevice {
    fn wgpu_device(&self) -> &wgpu::Device {
        self
    }
}

impl PipelineDevice for crate::RenderDevice {
    fn wgpu_device(&self) -> &wgpu::Device {
        &self.device
    }
}

impl<T: PipelineDevice + ?Sized> PipelineDevice for &T {
    fn wgpu_device(&self) -> &wgpu::Device {
        (**self).wgpu_device()
    }
}

/// The pipeline cache to pass in a pipeline descriptor created on `device`.
///
/// Every engine pipeline creation site calls this (it also counts the creation for
/// [`pipelines_created`] and the warm-up progress). `None` unless a cache is installed for this
/// very device and the device has `PIPELINE_CACHE`. Pass it as `cache: cache_for(device).as_deref()`.
#[must_use]
#[track_caller]
pub fn cache_for(device: &impl PipelineDevice) -> Option<PipelineCacheRef> {
    let device = device.wgpu_device();
    CREATED.fetch_add(1, Ordering::Relaxed);
    THREAD_CREATED.with(|n| n.set(n.get() + 1));
    crate::pipeline_warm::note_pipeline(std::panic::Location::caller());
    let slot = INSTALLED.read().ok()?;
    let (owner, cache) = slot.as_ref()?;
    (device.features().contains(wgpu::Features::PIPELINE_CACHE) && same_device(owner, device))
        .then(|| PipelineCacheRef(cache.clone()))
}

/// Pipelines created through engine creation sites since the process started.
#[must_use]
pub fn pipelines_created() -> u64 {
    CREATED.load(Ordering::Relaxed)
}

thread_local! {
    /// Pipelines created through [`cache_for`] on this thread (ADR-8584).
    // The initializer is `const`; clippy misfires on Android's emulated-TLS expansion of `thread_local!`.
    #[allow(clippy::missing_const_for_thread_local)]
    static THREAD_CREATED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Pipelines created through engine creation sites on the calling thread since it started
/// (ADR-8584). A host checks that a UI frame created none: the editor's tests read it before
/// and after the frame that opens a panel.
#[must_use]
pub fn pipelines_created_on_this_thread() -> u64 {
    THREAD_CREATED.with(std::cell::Cell::get)
}

/// Status of the installed cache (disabled when none).
#[must_use]
pub fn status() -> PipelineCacheStatus {
    installed().map_or_else(PipelineCacheStatus::default, |cache| cache.status())
}

/// Saves the installed cache if it has new pipelines (see [`PersistentPipelineCache::save`]).
/// Never once the device is lost ([`crate::device_lost`]): the driver may hand back truncated
/// data, which would overwrite a good cache just before the player relaunches (ADR-171).
pub fn save_installed() -> Option<std::io::Result<u64>> {
    if crate::gpu_health::device_lost() {
        tracing::warn!(target: "regia_render", "pipeline cache not saved: the GPU device was lost");
        return None;
    }
    installed().map(|cache| cache.save())
}

/// [`save_installed`] on a short-lived background thread (`regia-pcache-save`), off the frame
/// path. The renderer calls it once after the first game frame (ADR-5857): until then the warm
/// queue only saves when it has drained, which on a first launch can be seconds after the first
/// frame, and a player who closes the app (or a system that kills it) in that window lost every
/// pipeline the first frame compiled, so the next start compiled them again. Returns the
/// thread's handle (dropping it detaches the thread); `None` when no cache is installed, the
/// device is lost, or the thread could not start.
pub fn save_installed_in_background(reason: &'static str) -> Option<std::thread::JoinHandle<()>> {
    if crate::gpu_health::device_lost() || installed().is_none() {
        return None;
    }
    let spawned = std::thread::Builder::new()
        .name("regia-pcache-save".to_string())
        .spawn(move || {
            let _span = tracing::info_span!("pipeline_cache_checkpoint", reason).entered();
            if let Some(Err(err)) = save_installed() {
                tracing::warn!(target: "regia_render", "pipeline cache save ({reason}) failed: {err}");
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::warn!(target: "regia_render", "pipeline cache save ({reason}) not started: {err}");
            None
        }
    }
}

/// [`PersistentPipelineCache::snapshot`] of the installed cache; never once the device is lost
/// (see [`save_installed`]).
#[must_use]
pub fn snapshot_installed() -> Option<PendingCacheSave> {
    if crate::gpu_health::device_lost() {
        tracing::warn!(target: "regia_render", "pipeline cache not saved: the GPU device was lost");
        return None;
    }
    installed().and_then(|cache| cache.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_stops_at_the_cap() {
        let dir = regia_test_utils::unique_temp_dir("fpc-cap");
        let path = dir.join("a.fpc");
        std::fs::write(&path, [1u8; 9]).expect("write");
        assert_eq!(read_at_most(&path, 9), Ok(Some(vec![1u8; 9])));
        assert_eq!(read_at_most(&path, 8), Err(DiscardReason::TooLarge));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn key() -> PipelineCacheKey {
        PipelineCacheKey {
            backend: "Vulkan".into(),
            vendor: 0x10de,
            device: 0x2b85,
            driver: "NVIDIA".into(),
            driver_info: "580.95".into(),
            wgpu_version: WGPU_VERSION.into(),
            shader_hash: engine_shader_hash(),
        }
    }

    #[test]
    fn round_trips_and_rejects_every_mismatch() {
        let payload = (0..5000u32).map(|i| (i * 7) as u8).collect::<Vec<_>>();
        let file = encode(&key(), &payload);
        assert_eq!(decode(&file, &key()), Ok(payload.as_slice()));

        let mut other = key();
        other.driver_info = "580.96".into();
        assert_eq!(decode(&file, &other), Err(DiscardReason::KeyMismatch));
        let mut other = key();
        other.shader_hash = "0000".into();
        assert_eq!(decode(&file, &other), Err(DiscardReason::KeyMismatch));

        let mut bad = file.clone();
        bad[0] = b'X';
        assert_eq!(decode(&bad, &key()), Err(DiscardReason::BadMagic));
        let mut bad = file.clone();
        bad[4] = 9;
        assert_eq!(decode(&bad, &key()), Err(DiscardReason::Version(9)));
        let mut bad = file.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert_eq!(decode(&bad, &key()), Err(DiscardReason::Checksum));
        assert_eq!(
            decode(&file[..file.len() - 1], &key()),
            Err(DiscardReason::Length)
        );
        assert_eq!(decode(&file[..10], &key()), Err(DiscardReason::Truncated));
        assert_eq!(decode(&[], &key()), Err(DiscardReason::Truncated));
    }

    /// SplitMix64 (Steele, Lea & Flood 2014), enough for deterministic fuzz inputs.
    struct SplitMix(u64);

    impl SplitMix {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    #[test]
    fn fuzzed_files_never_panic_and_never_pass_altered() {
        let payload = (0..2048u32).map(|i| (i * 31 + 5) as u8).collect::<Vec<_>>();
        let file = encode(&key(), &payload);
        let mut rng = SplitMix(0x5eed_fca1);
        for _ in 0..4000 {
            let mut bytes = file.clone();
            match rng.next_u64() % 4 {
                0 => {
                    let n = 1 + (rng.next_u64() % 8) as usize;
                    for _ in 0..n {
                        let i = (rng.next_u64() as usize) % bytes.len();
                        bytes[i] ^= 1 << (rng.next_u64() % 8);
                    }
                }
                1 => bytes.truncate((rng.next_u64() as usize) % bytes.len()),
                2 => {
                    let at = (rng.next_u64() as usize) % bytes.len();
                    let extra = (0..(rng.next_u64() % 64)).map(|i| i as u8);
                    bytes.splice(at..at, extra);
                }
                _ => {
                    // Hostile key length / payload length fields.
                    let field = if rng.next_u64().is_multiple_of(2) {
                        8
                    } else {
                        PREFIX_LEN + key().canonical().len()
                    };
                    let v = rng.next_u64().to_le_bytes();
                    let width = if field == 8 { 4 } else { 8 };
                    bytes[field..field + width].copy_from_slice(&v[..width]);
                }
            }
            if let Ok(p) = decode(&bytes, &key()) {
                assert_eq!(
                    p,
                    payload.as_slice(),
                    "an altered file decoded to different bytes"
                );
                assert_eq!(bytes, file, "only the untouched file may decode");
            }
        }
    }

    /// Short and unaligned inputs (unsafe audit 2026-10-21): every proper prefix of a valid file
    /// is refused, so a save cut short never reaches the driver, and a file at an odd address
    /// decodes exactly like an aligned one (every field is read byte by byte).
    #[test]
    fn every_proper_prefix_is_refused_and_alignment_does_not_matter() {
        let payload = (0..300u32).map(|i| (i * 13 + 1) as u8).collect::<Vec<_>>();
        let file = encode(&key(), &payload);
        for len in 0..file.len() {
            assert!(
                decode(&file[..len], &key()).is_err(),
                "a {len}-byte prefix decoded"
            );
        }
        for shift in 1..8 {
            let mut shifted = vec![0u8; shift];
            shifted.extend_from_slice(&file);
            assert_eq!(decode(&shifted[shift..], &key()), Ok(payload.as_slice()));
        }
    }

    #[test]
    fn file_names_split_by_adapter_and_key_follows_the_salt() {
        assert_eq!(key().file_name(), "pipelines-vulkan-10de-2b85.fpc");
        let info = wgpu::AdapterInfo {
            name: "test".into(),
            vendor: 0x10de,
            device: 0x2b85,
            device_type: wgpu::DeviceType::DiscreteGpu,
            device_pci_bus_id: String::new(),
            driver: "NVIDIA".into(),
            driver_info: "580".into(),
            backend: wgpu::Backend::Vulkan,
            subgroup_min_size: 32,
            subgroup_max_size: 32,
            transient_saves_memory: None,
            limit_bucket: None,
        };
        let plain = PipelineCacheKey::for_adapter(&info).expect("vulkan key");
        set_content_salt("do27-shaders-7");
        let salted = PipelineCacheKey::for_adapter(&info).expect("vulkan key");
        set_content_salt("");
        assert_ne!(plain.shader_hash, salted.shader_hash);
        let gl = wgpu::AdapterInfo {
            backend: wgpu::Backend::Gl,
            ..info
        };
        assert!(PipelineCacheKey::for_adapter(&gl).is_none());
    }

    #[test]
    fn header_key_round_trips_and_shipped_names_split_by_driver() {
        let file = encode(&key(), b"payload");
        assert_eq!(read_key(&file), Ok(key()));
        assert_eq!(PipelineCacheKey::parse_canonical(&key().canonical()), Some(key()));
        assert_eq!(PipelineCacheKey::parse_canonical("backend=Vulkan\n"), None);
        assert_eq!(read_key(b"FPCH"), Err(DiscardReason::Truncated));
        let mut other = key();
        other.driver_info = "590.10".into();
        assert_eq!(other.file_name(), key().file_name());
        assert_ne!(other.shipped_file_name(), key().shipped_file_name());
        assert!(
            key()
                .shipped_file_name()
                .starts_with("pipelines-vulkan-10de-2b85-")
        );
    }

    #[test]
    fn shipped_cache_seeds_only_on_an_exact_key_match() {
        let root = std::env::temp_dir().join(format!("regia-fpc-ship-{}", std::process::id()));
        let (local_dir, ship) = (root.join("local"), root.join("ship"));
        std::fs::create_dir_all(&local_dir).expect("dir");
        std::fs::create_dir_all(&ship).expect("dir");
        let local = local_dir.join(key().file_name());
        let payload = (0..4096u32).map(|i| (i * 13) as u8).collect::<Vec<_>>();

        // Nothing anywhere: compile.
        let (data, load, shipped) = initial_payload(&local, Some(&ship), &key());
        assert_eq!(
            (data, load, shipped),
            (None, CacheLoad::Missing, Some(ShippedLoad::NoMatch))
        );

        // A cache from another driver under this adapter's name is rejected, never handed over.
        let mut wrong = key();
        wrong.driver_info = "575.0".into();
        let named = ship.join(key().shipped_file_name());
        std::fs::write(&named, encode(&wrong, &payload)).expect("write");
        let (data, load, shipped) = initial_payload(&local, Some(&ship), &key());
        assert_eq!(data, None);
        assert_eq!(load, CacheLoad::Missing);
        assert!(matches!(
            shipped,
            Some(ShippedLoad::Discarded(_, DiscardReason::KeyMismatch))
        ));

        // Garbage and a torn payload are rejected too.
        std::fs::write(&named, b"not a pipeline cache at all").expect("write");
        assert!(initial_payload(&local, Some(&ship), &key()).0.is_none());
        let mut torn = encode(&key(), &payload);
        torn.truncate(torn.len() - 7);
        std::fs::write(&named, &torn).expect("write");
        let (data, _, shipped) = initial_payload(&local, Some(&ship), &key());
        assert_eq!(data, None);
        assert!(matches!(
            shipped,
            Some(ShippedLoad::Discarded(_, DiscardReason::Length))
        ));

        // The matching shipped file seeds.
        std::fs::write(&named, encode(&key(), &payload)).expect("write");
        let (data, load, shipped) = initial_payload(&local, Some(&ship), &key());
        assert_eq!(data.as_deref(), Some(payload.as_slice()));
        assert_eq!(load, CacheLoad::Seeded(payload.len() as u64));
        assert!(matches!(shipped, Some(ShippedLoad::Seeded(_))));

        // A shipped file under the plain local name works as well.
        std::fs::remove_file(&named).expect("rm");
        std::fs::write(ship.join(key().file_name()), encode(&key(), &payload)).expect("write");
        assert_eq!(
            initial_payload(&local, Some(&ship), &key()).1,
            CacheLoad::Seeded(4096)
        );

        // A valid local file wins and the shipped directory is not consulted.
        write_atomic(&local, &encode(&key(), b"local")).expect("write");
        let (data, load, shipped) = initial_payload(&local, Some(&ship), &key());
        assert_eq!(data.as_deref(), Some(&b"local"[..]));
        assert_eq!((load, shipped), (CacheLoad::Loaded(5), None));

        // A corrupt local file falls back to the shipped one.
        std::fs::write(&local, b"FPCH junk").expect("write");
        assert_eq!(
            initial_payload(&local, Some(&ship), &key()).1,
            CacheLoad::Seeded(4096)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn atomic_write_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("regia-fpc-{}", std::process::id()));
        let path = dir.join("pipelines-test.fpc");
        write_atomic(&path, b"first").expect("write");
        write_atomic(&path, b"second").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        assert_eq!(read_capped(&dir.join("missing.fpc")), Ok(None));
        let leftovers = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 1, "no temporary files remain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sharing_retry_retries_transient_errors_then_succeeds() {
        let mut calls = 0_u32;
        let result = with_sharing_retry(
            || {
                calls += 1;
                if calls <= 3 {
                    Err(std::io::Error::from_raw_os_error(32))
                } else {
                    Ok(())
                }
            },
            |_| {},
        );
        assert!(result.is_ok());
        assert_eq!(calls, 4, "three sharing violations, then the success");
    }

    #[test]
    fn sharing_retry_gives_up_after_the_attempt_budget() {
        let mut calls = 0_u32;
        let result = with_sharing_retry(
            || {
                calls += 1;
                Err(std::io::Error::from_raw_os_error(5))
            },
            |_| {},
        );
        assert_eq!(result.expect_err("held forever").raw_os_error(), Some(5));
        assert_eq!(calls, 6, "six attempts, like `regia_platform`'s `rename_replace`");
    }

    #[test]
    fn sharing_retry_does_not_retry_permanent_errors() {
        let mut calls = 0_u32;
        let result = with_sharing_retry(
            || {
                calls += 1;
                Err(std::io::Error::from_raw_os_error(2))
            },
            |_| {},
        );
        assert_eq!(
            result.expect_err("not found stays not found").raw_os_error(),
            Some(2)
        );
        assert_eq!(calls, 1, "a permanent error surfaces at once");
    }

    #[test]
    fn transient_sharing_error_codes() {
        // Windows access denied, sharing violation and lock violation clear once the other
        // holder closes the file; everything else (here: not found) is not retried.
        for code in [5, 32, 33] {
            assert!(
                is_transient_sharing_error(&std::io::Error::from_raw_os_error(code)),
                "raw os error {code} is retried"
            );
        }
        assert!(!is_transient_sharing_error(&std::io::Error::from_raw_os_error(2)));
    }

    /// A destination held open without `FILE_SHARE_DELETE` (a virus scanner, indexer or
    /// another reader) fails the replace with a sharing-class error until it closes:
    /// `write_atomic` must retry through that window instead of losing the save. The test
    /// drives the whole write through the body `write_atomic` runs (temp file, sync, rename,
    /// temp cleanup) and releases the handle from the retry hook.
    #[cfg(windows)]
    #[test]
    fn atomic_write_retries_while_destination_is_held_open() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = regia_test_utils::unique_temp_dir("regia-fpc-held-dest");
        let path = dir.join("held.fpc");
        write_atomic(&path, b"old").expect("seed");
        // No sharing flags at all: the rename cannot replace the file while this is open.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .expect("open without share-delete");
        let mut held = Some(held);
        write_atomic_hooked(&path, b"new", |_| {
            // Release the handle from inside the retry window: the next attempt succeeds.
            held.take();
        })
        .expect("write_atomic retries past the held handle");
        assert_eq!(std::fs::read(&path).expect("read"), b"new");
        assert!(held.is_none(), "the handle was closed by the retry hook");
        let leftovers = std::fs::read_dir(&dir).expect("dir").count();
        assert_eq!(leftovers, 1, "the temporary is renamed away, not left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn intel_on_windows_gets_no_engine_cache_unless_forced() {
        const NVIDIA: u32 = 0x10de;
        const AMD: u32 = 0x1002;
        // The N150 crash (ADR-3288): Intel's Windows Vulkan driver.
        assert!(driver_skips_cache(INTEL_VENDOR, "windows", None));
        assert!(driver_skips_cache(INTEL_VENDOR, "windows", Some("")));
        assert!(driver_skips_cache(INTEL_VENDOR, "windows", Some("0")));
        // `1` re-tests a new driver with the cache attached.
        assert!(!driver_skips_cache(INTEL_VENDOR, "windows", Some("1")));
        assert!(!driver_skips_cache(INTEL_VENDOR, "windows", Some(" true ")));
        // Mesa's Intel driver on Linux and every other vendor keep the cache.
        for os in ["linux", "android", "macos"] {
            assert!(!driver_skips_cache(INTEL_VENDOR, os, None), "{os}");
        }
        for vendor in [NVIDIA, AMD, 0x5143, 0x13b5] {
            assert!(!driver_skips_cache(vendor, "windows", None), "{vendor:#x}");
        }
        assert!(DRIVER_SKIP_REASON.contains("ADR-3288"));
    }

    /// Spec 13 § 2b: `REGIA_PIPELINE_CACHE=0` disables the cache, `REGIA_PIPELINE_CACHE_DIR`
    /// names its directory for tools, and otherwise the configured directory (the windowed app's
    /// `cache://pipelines`) is used; with neither, test devices carry no cache.
    #[test]
    fn the_cache_directory_follows_the_environment_overrides() {
        let configured = || Some(PathBuf::from("/game/cache/pipelines"));
        let tool = Some(std::ffi::OsString::from("/tmp/tool-cache"));
        assert_eq!(directory_from(None, None, configured), configured());
        assert_eq!(directory_from(None, None, || None), None);
        assert_eq!(
            directory_from(None, tool.clone(), configured),
            Some(PathBuf::from("/tmp/tool-cache"))
        );
        assert_eq!(directory_from(Some("0"), tool, configured), None);
        assert_eq!(directory_from(Some("0"), None, configured), None);
        assert_eq!(directory_from(Some("1"), None, configured), configured());
        assert_eq!(
            directory_from(None, Some(std::ffi::OsString::new()), configured),
            configured(),
            "an empty directory value is unset"
        );
    }
}
