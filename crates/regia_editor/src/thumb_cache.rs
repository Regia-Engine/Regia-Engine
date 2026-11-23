// SPDX-License-Identifier: MPL-2.0
//! Content Browser thumbnails (`docs/specs/19-editor.md` § 2, `book/agents/editor-asset-browser.md`,
//! ADR-1329).
//!
//! A [`ThumbCache`] (an edit-world resource) answers "is the picture for this asset ready?" for the
//! tiles the browser draws this frame, and never blocks the frame on the answer:
//!
//! - **Memory:** the last [`DEFAULT_MEMORY_ENTRIES`] pictures, least recently drawn evicted
//!   first. A picture becomes an egui texture only when a tile shows it.
//! - **Disk:** `<project>/.regia/thumbs/<key>.png`, where the key hashes the cache version, the
//!   picture size, the project-relative path, and the source file's length and modification time
//!   ([`thumb_key`]). An edited source gets a new key, so stale pictures are never shown as current;
//!   old files are pruned oldest-first when the folder grows past [`DEFAULT_DISK_BUDGET`]
//!   ([`prune_disk`]). The folder is a cache: deleting it is always safe.
//! - **Worker:** one background thread reads cached files and makes texture thumbnails (decode,
//!   shrink to fit, center on a transparent square, store). Requests are only made for tiles in
//!   view; a queued request that is not renewed for [`KEEP_FRAMES`] frames (the user scrolled
//!   away) is dropped before it costs anything.
//! - **GPU:** models and materials (on the built-in sphere) the disk cache does not have wait for
//!   [`ThumbCache::render_meshes`], which draws a few per frame on the main thread with the batch
//!   [`ThumbnailRenderer`] and hands the pixels to the worker to store.
//!
//! Every step is counted in [`ThumbStats`], which is how tests and agents check the cache without a
//! clock (`editor.asset_thumbnails`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_render::RenderDevice;
use serde_json::{Value, json};

use crate::mesh_source::{LoadedModel, MeshLibrary, ModelPart};
use crate::thumbnails::{ThumbJob, ThumbnailRenderer};
use crate::viewport_agent::Handler;

/// Bumped when the picture recipe changes; part of every key, so old files simply stop matching.
pub const THUMB_CACHE_VERSION: u32 = 1;
/// Thumbnail edge in pixels.
pub const DEFAULT_THUMB_PX: u32 = 96;
/// Pictures kept in memory (96 px RGBA: 36 KiB each, 18 MiB for 512).
pub const DEFAULT_MEMORY_ENTRIES: usize = 512;
/// The disk cache is pruned back to three quarters of this when it grows past it.
pub const DEFAULT_DISK_BUDGET: u64 = 256 * 1024 * 1024;
/// Requests handed to the worker at once.
pub const MAX_IN_FLIGHT: usize = 8;
/// A queued request not renewed for more than this many frames is dropped.
pub const KEEP_FRAMES: u64 = 2;

/// What a thumbnail is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbSource {
    /// An image file the worker decodes (`png`, `jpg`, `jpeg`, `tga`, `bmp`, `hdr`, `exr`).
    Texture,
    /// A glTF model the GPU renders (`glb`, `gltf`).
    Mesh,
    /// A material the GPU renders on the built-in sphere (`.material.ron`, `.matgraph.ron`,
    /// `.matinst.ron`).
    Material,
}

impl ThumbSource {
    /// The source kind of a path, or `None` when the browser shows an icon instead.
    #[must_use]
    pub fn of(path: &str) -> Option<Self> {
        const DECODABLE: [&str; 7] = [".png", ".jpg", ".jpeg", ".tga", ".bmp", ".hdr", ".exr"];
        let lower = path.to_ascii_lowercase();
        if DECODABLE.iter().any(|ext| lower.ends_with(ext)) {
            Some(Self::Texture)
        } else if lower.ends_with(".glb") || lower.ends_with(".gltf") {
            Some(Self::Mesh)
        } else if [".material.ron", ".matgraph.ron", ".matinst.ron"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            Some(Self::Material)
        } else {
            None
        }
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a (64-bit) of `bytes`, continuing from `hash` (start with the offset basis
/// `0xcbf29ce484222325`). Public-domain algorithm by Fowler, Noll and Vo.
#[must_use]
pub fn fnv1a64(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The cache key of a thumbnail: the cache version, the picture size, the project-relative path
/// (`\` read as `/`), and the source's length and modification time in nanoseconds.
#[must_use]
pub fn thumb_key(rel: &str, len: u64, modified_ns: u128, px: u32) -> u64 {
    let mut hash = fnv1a64(&THUMB_CACHE_VERSION.to_le_bytes(), FNV_OFFSET);
    hash = fnv1a64(&px.to_le_bytes(), hash);
    hash = fnv1a64(rel.replace('\\', "/").as_bytes(), hash);
    hash = fnv1a64(&[0], hash);
    hash = fnv1a64(&len.to_le_bytes(), hash);
    fnv1a64(&modified_ns.to_le_bytes(), hash)
}

/// The project's thumbnail folder (`<root>/.regia/thumbs`).
#[must_use]
pub fn cache_dir(root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(root).join("thumbs")
}

/// The cached picture for `key`.
#[must_use]
pub fn cache_file(root: &Path, key: u64) -> PathBuf {
    cache_dir(root).join(format!("{key:016x}.png"))
}

/// The answer to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbState {
    /// The picture is in memory ([`ThumbCache::image`], [`ThumbCache::texture`]).
    Ready,
    /// Queued or being read or made by the worker.
    Pending,
    /// A model or material waiting for [`ThumbCache::render_meshes`] (no GPU device yet, or its
    /// turn has not come).
    NeedsGpu,
    /// The source could not be read, decoded or drawn (the message says why). Not retried until
    /// the file changes or [`ThumbCache::forget`] is called.
    Failed(String),
    /// No thumbnail for this kind of file (the browser shows its icon), or no project.
    Unsupported,
}

impl ThumbState {
    /// The protocol name: `ready`, `pending`, `needs_gpu`, `failed` or `unsupported`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Pending => "pending",
            Self::NeedsGpu => "needs_gpu",
            Self::Failed(_) => "failed",
            Self::Unsupported => "unsupported",
        }
    }
}

/// How a picture in memory got there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbOrigin {
    /// Read from the disk cache.
    Disk,
    /// Made from a texture by the worker (and stored).
    Made,
    /// Rendered from a model or a material on the GPU (and stored).
    Rendered,
}

impl ThumbOrigin {
    /// The protocol name: `disk`, `made` or `rendered`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Made => "made",
            Self::Rendered => "rendered",
        }
    }
}

/// What the cache has done so far (counters only grow; [`ThumbCache::reset_stats`] zeroes them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThumbStats {
    /// Calls to [`ThumbCache::request`].
    pub requests: u64,
    /// Requests answered from memory.
    pub memory_hits: u64,
    /// Source files whose length and time were read (once per path until it changes).
    pub file_stats: u64,
    /// Requests handed to the worker.
    pub dispatched: u64,
    /// Pictures read from the disk cache.
    pub disk_hits: u64,
    /// Texture thumbnails made by the worker.
    pub made: u64,
    /// Model and material thumbnails rendered on the GPU.
    pub rendered: u64,
    /// Sources that failed (unreadable, undecodable, nothing to draw).
    pub failed: u64,
    /// Pictures evicted from memory.
    pub evicted: u64,
    /// Queued requests dropped because their tile left the view.
    pub dropped: u64,
    /// Worker results thrown away because the file changed (or the project closed) meanwhile.
    pub stale_results: u64,
    /// Disk files removed by pruning.
    pub pruned_files: u64,
}

/// Work for the thumbnail thread.
enum Job {
    /// Read the cached picture, or make one from a texture (meshes come back as `NeedsGpu`).
    Load {
        rel: String,
        key: u64,
        source: PathBuf,
        file: PathBuf,
        kind: ThumbSource,
        px: u32,
    },
    /// Store a GPU-rendered picture.
    Store { file: PathBuf, rgba: Vec<u8>, px: u32 },
    /// Prune the disk cache.
    Prune { dir: PathBuf, budget: u64 },
}

enum Outcome {
    Image(egui::ColorImage, ThumbOrigin),
    NeedsGpu,
    Failed(String),
}

enum Reply {
    Loaded { rel: String, key: u64, outcome: Outcome },
    Pruned(usize),
}

/// The thumbnail thread's channels (it exits when the job sender drops).
struct Worker {
    jobs: Sender<Job>,
    replies: Mutex<Receiver<Reply>>,
}

fn spawn_worker() -> Option<Worker> {
    let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
    let spawned = std::thread::Builder::new()
        .name("regia-thumbs".to_string())
        .spawn(move || {
            while let Ok(job) = job_rx.recv() {
                if let Some(reply) = run_job(job)
                    && reply_tx.send(reply).is_err()
                {
                    break;
                }
            }
        });
    match spawned {
        Ok(_) => Some(Worker {
            jobs: job_tx,
            replies: Mutex::new(reply_rx),
        }),
        Err(err) => {
            tracing::warn!(target: "regia_editor", "thumbnail thread: {err}; thumbnails load on the UI thread");
            None
        }
    }
}

fn run_job(job: Job) -> Option<Reply> {
    match job {
        Job::Load {
            rel,
            key,
            source,
            file,
            kind,
            px,
        } => {
            let _span = tracing::info_span!(target: "regia_editor", "thumb_load").entered();
            let outcome = load_or_make(&source, &file, kind, px);
            Some(Reply::Loaded { rel, key, outcome })
        }
        Job::Store { file, rgba, px } => {
            if let Err(err) = store_png(&file, &rgba, px) {
                tracing::warn!(target: "regia_editor", "thumbnail store: {err}");
            }
            None
        }
        Job::Prune { dir, budget } => Some(Reply::Pruned(prune_disk(&dir, budget).0)),
    }
}

fn load_or_make(source: &Path, file: &Path, kind: ThumbSource, px: u32) -> Outcome {
    if let Some(image) = read_cached(file, px) {
        return Outcome::Image(image, ThumbOrigin::Disk);
    }
    match kind {
        ThumbSource::Mesh | ThumbSource::Material => Outcome::NeedsGpu,
        ThumbSource::Texture => match make_texture_thumb(source, px) {
            Ok(rgba) => {
                if let Err(err) = store_png(file, &rgba, px) {
                    tracing::warn!(target: "regia_editor", "thumbnail store: {err}");
                }
                let side = px as usize;
                Outcome::Image(
                    egui::ColorImage::from_rgba_unmultiplied([side, side], &rgba),
                    ThumbOrigin::Made,
                )
            }
            Err(err) => Outcome::Failed(err),
        },
    }
}

/// A cached picture of exactly `px`×`px`, or `None` (missing, unreadable, or another size).
fn read_cached(file: &Path, px: u32) -> Option<egui::ColorImage> {
    let bytes = std::fs::read(file).ok()?;
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .ok()?
        .to_rgba8();
    let side = px as usize;
    (decoded.dimensions() == (px, px))
        .then(|| egui::ColorImage::from_rgba_unmultiplied([side, side], decoded.as_raw()))
}

/// The size a `width`×`height` picture is drawn at inside a `px` square: shrunk to fit with its
/// aspect kept (never below one pixel), or its own size when it already fits.
#[must_use]
pub fn fit_size(width: u32, height: u32, px: u32) -> (u32, u32) {
    if width <= px && height <= px {
        return (width, height);
    }
    let scale = |side: u32, long: u32| -> u32 {
        let scaled = u64::from(side) * u64::from(px) / u64::from(long.max(1));
        u32::try_from(scaled).unwrap_or(px).max(1)
    };
    if width >= height {
        (px, scale(height, width))
    } else {
        (scale(width, height), px)
    }
}

/// A texture thumbnail: the image decoded, shrunk to fit `px` (aspect kept) and centered on a
/// transparent `px`×`px` square. Returns unpremultiplied RGBA8.
///
/// # Errors
///
/// The file cannot be read or decoded, or the image is empty.
pub fn make_texture_thumb(source: &Path, px: u32) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(source).map_err(|err| format!("cannot read {}: {err}", source.display()))?;
    let decoded = image::load_from_memory(&bytes)
        .map_err(|err| format!("cannot decode {}: {err}", source.display()))?
        .to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err(format!("{} is an empty image", source.display()));
    }
    let (fit_w, fit_h) = fit_size(width, height, px);
    let small = if (fit_w, fit_h) == (width, height) {
        decoded
    } else {
        image::imageops::thumbnail(&decoded, fit_w, fit_h)
    };
    let mut canvas = image::RgbaImage::new(px, px);
    image::imageops::replace(
        &mut canvas,
        &small,
        i64::from((px - fit_w) / 2),
        i64::from((px - fit_h) / 2),
    );
    Ok(canvas.into_raw())
}

/// Writes a `px`×`px` RGBA8 picture as PNG, atomically (temporary file + rename).
fn store_png(file: &Path, rgba: &[u8], px: u32) -> Result<(), String> {
    use image::ImageEncoder as _;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(rgba, px, px, image::ExtendedColorType::Rgba8)
        .map_err(|err| format!("png encode: {err}"))?;
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let tmp = file.with_extension(format!("{}-{n}.tmp", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|err| format!("cannot write {}: {err}", tmp.display()))?;
    std::fs::rename(&tmp, file).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot rename to {}: {err}", file.display())
    })
}

/// The built-in sphere wearing the material at `rel` (a material's thumbnail subject).
fn material_sphere(library: &MeshLibrary, root: &Path, rel: &str) -> Result<Arc<LoadedModel>, String> {
    let material = library.material(Some(root), rel)?;
    let sphere = library.model(None, "builtin:sphere")?;
    let parts = sphere
        .parts
        .iter()
        .map(|part| ModelPart {
            mesh: Arc::clone(&part.mesh),
            material: Arc::clone(&material),
            material_name: rel.to_string(),
        })
        .collect();
    Ok(Arc::new(LoadedModel {
        parts,
        collision_parts: Vec::new(),
        aabb_min: sphere.aabb_min,
        aabb_max: sphere.aabb_max,
    }))
}

/// Unpremultiplied RGBA8 bytes of an egui picture (egui keeps premultiplied colors).
fn unmultiplied_rgba(image: &egui::ColorImage) -> Vec<u8> {
    image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect()
}

/// When the `.png` files in `dir` hold more than `budget` bytes, removes the oldest (by
/// modification time, then name) until they hold at most three quarters of it. Returns the files
/// and bytes removed. Other files are left alone.
#[must_use = "the removed counts are the only report"]
pub fn prune_disk(dir: &Path, budget: u64) -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut files: Vec<(SystemTime, PathBuf, u64)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.extension().is_some_and(|ext| ext == "png") {
                return None;
            }
            let meta = entry.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((meta.modified().unwrap_or(UNIX_EPOCH), path, meta.len()))
        })
        .collect();
    let total: u64 = files.iter().map(|(_, _, len)| len).sum();
    if total <= budget {
        return (0, 0);
    }
    files.sort();
    let target = budget / 4 * 3;
    let mut remaining = total;
    let mut removed = (0, 0);
    for (_, path, len) in files {
        if remaining <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            remaining -= len;
            removed.0 += 1;
            removed.1 += len;
        }
    }
    removed
}

/// A relative path `/`-separated (the form the Content Browser's index uses on every OS),
/// whichever separators it was written with.
fn index_rel(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// One picture in memory.
struct Slot {
    key: u64,
    image: Arc<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
    used: u64,
    origin: ThumbOrigin,
}

/// The Content Browser's thumbnail cache (see the module docs).
#[derive(Resource)]
pub struct ThumbCache {
    root: Option<PathBuf>,
    px: u32,
    capacity: usize,
    disk_budget: u64,
    frame: u64,
    entries: HashMap<String, Slot>,
    /// Path → current key (`None`: the file is missing), read once per path until forgotten.
    keys: HashMap<String, Option<u64>>,
    failed: HashMap<String, (u64, String)>,
    queue: VecDeque<String>,
    /// Queued path → the last frame it was requested.
    queued: HashMap<String, u64>,
    /// Outstanding (path, source key) pairs, including superseded versions awaiting replies.
    in_flight: HashSet<(String, u64)>,
    gpu_wait: VecDeque<(String, u64)>,
    gpu_waiting: HashSet<String>,
    worker: Option<Worker>,
    worker_started: bool,
    /// Replies of jobs run on the UI thread when the worker could not start.
    inline: VecDeque<Reply>,
    renderer: Mutex<Option<ThumbnailRenderer>>,
    stats: ThumbStats,
    /// Path → its custom thumbnail's length and time (`None`: it has none), read once per path until
    /// forgotten (ADR-7788).
    customs: HashMap<String, Option<(u64, u128)>>,
}

impl Default for ThumbCache {
    fn default() -> Self {
        Self::new(DEFAULT_THUMB_PX, DEFAULT_MEMORY_ENTRIES)
    }
}

impl ThumbCache {
    /// A cache of `px`-pixel pictures keeping `capacity` of them in memory (at least one).
    #[must_use]
    pub fn new(px: u32, capacity: usize) -> Self {
        Self {
            root: None,
            px: px.clamp(crate::thumbnails::MIN_SIZE, crate::thumbnails::MAX_SIZE),
            capacity: capacity.max(1),
            disk_budget: DEFAULT_DISK_BUDGET,
            frame: 0,
            entries: HashMap::new(),
            keys: HashMap::new(),
            failed: HashMap::new(),
            queue: VecDeque::new(),
            queued: HashMap::new(),
            in_flight: HashSet::new(),
            gpu_wait: VecDeque::new(),
            gpu_waiting: HashSet::new(),
            worker: None,
            worker_started: false,
            inline: VecDeque::new(),
            renderer: Mutex::new(None),
            stats: ThumbStats::default(),
            customs: HashMap::new(),
        }
    }

    /// The same cache with another disk budget (bytes; see [`prune_disk`]).
    #[must_use]
    pub fn with_disk_budget(mut self, budget: u64) -> Self {
        self.disk_budget = budget;
        self
    }

    /// The picture edge in pixels.
    #[must_use]
    pub fn px(&self) -> u32 {
        self.px
    }

    /// The project whose thumbnails are cached.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The counters so far.
    #[must_use]
    pub fn stats(&self) -> ThumbStats {
        self.stats
    }

    /// Zeroes the counters.
    pub fn reset_stats(&mut self) {
        self.stats = ThumbStats::default();
    }

    /// Pictures in memory.
    #[must_use]
    pub fn memory_entries(&self) -> usize {
        self.entries.len()
    }

    /// Requests waiting for the worker.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queued.len()
    }

    /// Requests the worker is busy with.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Models and materials waiting for [`Self::render_meshes`].
    #[must_use]
    pub fn gpu_waiting(&self) -> usize {
        self.gpu_wait.len()
    }

    /// Whether anything is still on its way (the panel keeps repainting while so).
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.queued.is_empty() || !self.in_flight.is_empty()
    }

    /// Switches to another project (or none): forgets everything of the old one and prunes the
    /// new one's disk cache in the background.
    pub fn set_root(&mut self, root: Option<&Path>) {
        if self.root.as_deref() == root {
            return;
        }
        self.root = root.map(Path::to_path_buf);
        self.entries.clear();
        self.keys.clear();
        self.failed.clear();
        self.queue.clear();
        self.queued.clear();
        self.in_flight.clear();
        self.gpu_wait.clear();
        self.gpu_waiting.clear();
        self.customs.clear();
        if let Some(root) = root {
            self.send(Job::Prune {
                dir: cache_dir(root),
                budget: self.disk_budget,
            });
        }
    }

    /// Starts a frame: requests made from now on are this frame's.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// Drops every picture from memory (the disk cache stays).
    pub fn clear_memory(&mut self) {
        self.entries.clear();
    }

    /// Forgets what is known about `paths` (changed on disk: the next request reads the file's
    /// time again and, when it changed, makes a new picture). Absolute paths under the project
    /// and project-relative paths are both accepted.
    pub fn forget(&mut self, paths: &[PathBuf]) {
        for path in paths {
            let rel = match &self.root {
                Some(root) if path.is_absolute() => match path.strip_prefix(root) {
                    Ok(rel) => index_rel(rel),
                    Err(_) => continue,
                },
                _ => index_rel(path),
            };
            self.keys.remove(&rel);
            self.failed.remove(&rel);
            self.customs.remove(&rel);
        }
    }

    /// The current key of a project-relative path (reads the file's length and time once).
    fn key_for(&mut self, rel: &str) -> Option<u64> {
        if let Some(key) = self.keys.get(rel) {
            return *key;
        }
        if let Some((len, modified)) = self.custom_stamp(rel) {
            let key = custom_key(rel, len, modified, self.px);
            self.keys.insert(rel.to_string(), Some(key));
            return Some(key);
        }
        let source = self.root.as_ref()?.join(rel);
        self.stats.file_stats += 1;
        let key = std::fs::metadata(&source)
            .ok()
            .filter(std::fs::Metadata::is_file)
            .map(|meta| {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_nanos());
                thumb_key(rel, meta.len(), modified, self.px)
            });
        self.keys.insert(rel.to_string(), key);
        key
    }

    /// Asks for the picture of a project-relative path this frame (call it for the tiles in view
    /// only). Never blocks: a missing picture is queued and the answer is [`ThumbState::Pending`].
    pub fn request(&mut self, rel: &str) -> ThumbState {
        self.stats.requests += 1;
        if self.root.is_none() || (ThumbSource::of(rel).is_none() && self.custom_stamp(rel).is_none()) {
            return ThumbState::Unsupported;
        }
        let Some(key) = self.key_for(rel) else {
            return ThumbState::Failed("the file is missing".to_string());
        };
        if let Some(slot) = self.entries.get_mut(rel)
            && slot.key == key
        {
            slot.used = self.frame;
            self.stats.memory_hits += 1;
            return ThumbState::Ready;
        }
        if let Some((failed_key, message)) = self.failed.get(rel)
            && *failed_key == key
        {
            return ThumbState::Failed(message.clone());
        }
        if self.gpu_waiting.contains(rel) {
            return ThumbState::NeedsGpu;
        }
        // At most MAX_IN_FLIGHT pairs: avoid allocating a path just for this lookup.
        if self
            .in_flight
            .iter()
            .any(|(path, active_key)| path == rel && *active_key == key)
        {
            return ThumbState::Pending;
        }
        match self.queued.get_mut(rel) {
            Some(last) => *last = self.frame,
            None => {
                self.queued.insert(rel.to_string(), self.frame);
                self.queue.push_back(rel.to_string());
            }
        }
        ThumbState::Pending
    }

    /// The state of a path without requesting it (no file access, no counters).
    #[must_use]
    pub fn state(&self, rel: &str) -> ThumbState {
        let custom = self.customs.get(rel).copied().flatten().is_some();
        if self.root.is_none() || (ThumbSource::of(rel).is_none() && !custom) {
            return ThumbState::Unsupported;
        }
        let key = self.keys.get(rel).copied().flatten();
        if let Some(slot) = self.entries.get(rel)
            && Some(slot.key) == key
        {
            return ThumbState::Ready;
        }
        if let Some((failed_key, message)) = self.failed.get(rel)
            && Some(*failed_key) == key
        {
            return ThumbState::Failed(message.clone());
        }
        if self.keys.get(rel) == Some(&None) {
            return ThumbState::Failed("the file is missing".to_string());
        }
        if self.gpu_waiting.contains(rel) {
            return ThumbState::NeedsGpu;
        }
        ThumbState::Pending
    }

    /// How the in-memory picture of a path got there.
    #[must_use]
    pub fn origin(&self, rel: &str) -> Option<ThumbOrigin> {
        self.entries.get(rel).map(|slot| slot.origin)
    }

    /// The picture of a path, when one is in memory (possibly of an older version of the file while
    /// the new one is on its way).
    #[must_use]
    pub fn image(&self, rel: &str) -> Option<Arc<egui::ColorImage>> {
        self.entries.get(rel).map(|slot| Arc::clone(&slot.image))
    }

    /// The egui texture of a path's picture (uploaded on first use, freed with the picture).
    pub fn texture(&mut self, ctx: &egui::Context, rel: &str) -> Option<egui::TextureId> {
        let slot = self.entries.get_mut(rel)?;
        let texture = slot.texture.get_or_insert_with(|| {
            ctx.load_texture(
                format!("regia-thumb-{:016x}", slot.key),
                (*slot.image).clone(),
                egui::TextureOptions::LINEAR,
            )
        });
        Some(texture.id())
    }

    /// Hands a job to the worker, starting it on first use; runs the job here when there is no
    /// worker.
    fn send(&mut self, job: Job) {
        if !self.worker_started {
            self.worker_started = true;
            self.worker = spawn_worker();
        }
        let job = match &self.worker {
            Some(worker) => match worker.jobs.send(job) {
                Ok(()) => return,
                Err(std::sync::mpsc::SendError(job)) => {
                    let keep = match &job {
                        Job::Load { rel, key, .. } => Some((rel.clone(), *key)),
                        _ => None,
                    };
                    self.worker_lost(keep.as_ref());
                    job
                }
            },
            None => job,
        };
        // No worker, or it stopped: run the job here.
        self.worker = None;
        if let Some(reply) = run_job(job) {
            self.inline.push_back(reply);
        }
    }

    /// The worker thread is gone: keep its buffered replies and fail only unanswered current
    /// source keys. `keep` is a send's request about to run here instead.
    fn worker_lost(&mut self, keep: Option<&(String, u64)>) {
        tracing::warn!(target: "regia_editor", "thumbnail thread stopped; thumbnails load on the UI thread");
        if let Some(worker) = self.worker.take() {
            let receiver = worker
                .replies
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while let Ok(reply) = receiver.try_recv() {
                self.inline.push_back(reply);
            }
        }
        let answered: HashSet<(String, u64)> = self
            .inline
            .iter()
            .filter_map(|reply| match reply {
                Reply::Loaded { rel, key, .. } => Some((rel.clone(), *key)),
                Reply::Pruned(_) => None,
            })
            .collect();
        for request in std::mem::take(&mut self.in_flight) {
            if answered.contains(&request) || keep == Some(&request) {
                self.in_flight.insert(request);
                continue;
            }
            let (rel, key) = request;
            if self.keys.get(&rel).copied().flatten() == Some(key) {
                self.failed
                    .insert(rel, (key, "the thumbnail thread stopped".to_string()));
                self.stats.failed += 1;
            }
        }
    }

    /// Hands queued requests to the worker (at most [`MAX_IN_FLIGHT`] at once), dropping the ones
    /// whose tiles left the view.
    fn dispatch(&mut self) {
        while self.in_flight.len() < MAX_IN_FLIGHT {
            let Some(rel) = self.queue.pop_front() else {
                break;
            };
            let Some(last) = self.queued.remove(&rel) else {
                continue;
            };
            if self.frame.saturating_sub(last) > KEEP_FRAMES {
                self.stats.dropped += 1;
                continue;
            }
            // A custom thumbnail (ADR-7788) is a picture: the worker shrinks it like a texture.
            let custom = self.customs.get(&rel).copied().flatten().is_some();
            let kind = if custom {
                Some(ThumbSource::Texture)
            } else {
                ThumbSource::of(&rel)
            };
            let (Some(root), Some(kind), Some(Some(key))) =
                (self.root.clone(), kind, self.keys.get(&rel).copied())
            else {
                continue;
            };
            self.in_flight.insert((rel.clone(), key));
            self.stats.dispatched += 1;
            self.send(Job::Load {
                source: if custom {
                    custom_file(&root, &rel)
                } else {
                    root.join(&rel)
                },
                file: cache_file(&root, key),
                rel,
                key,
                kind,
                px: self.px,
            });
        }
    }

    /// Takes the worker's results; with `block`, waits for one when none is ready and work is in
    /// flight. Returns how many results were taken.
    fn collect(&mut self, block: bool) -> usize {
        let mut replies: Vec<Reply> = self.inline.drain(..).collect();
        let mut disconnected = false;
        if let Some(worker) = &mut self.worker {
            let receiver = worker
                .replies
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if block && replies.is_empty() && !self.in_flight.is_empty() {
                match receiver.recv() {
                    Ok(reply) => replies.push(reply),
                    Err(_) => disconnected = true,
                }
            }
            loop {
                match receiver.try_recv() {
                    Ok(reply) => replies.push(reply),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            // Preserve replies already drained above before deciding which requests were lost.
            self.inline.extend(replies);
            self.worker_lost(None);
            replies = self.inline.drain(..).collect();
        }
        let count = replies.len();
        for reply in replies {
            self.accept(reply);
        }
        count
    }

    fn accept(&mut self, reply: Reply) {
        let (rel, key, outcome) = match reply {
            Reply::Loaded { rel, key, outcome } => (rel, key, outcome),
            Reply::Pruned(files) => {
                self.stats.pruned_files += files as u64;
                return;
            }
        };
        self.in_flight.remove(&(rel.clone(), key));
        if self.keys.get(&rel).copied().flatten() != Some(key) {
            self.stats.stale_results += 1;
            return;
        }
        match outcome {
            Outcome::Image(image, origin) => {
                match origin {
                    ThumbOrigin::Disk => self.stats.disk_hits += 1,
                    ThumbOrigin::Made => self.stats.made += 1,
                    ThumbOrigin::Rendered => self.stats.rendered += 1,
                }
                self.insert(rel, key, image, origin);
            }
            Outcome::NeedsGpu => {
                if self.gpu_waiting.insert(rel.clone()) {
                    self.gpu_wait.push_back((rel, key));
                }
            }
            Outcome::Failed(message) => {
                tracing::warn!(target: "regia_editor", "thumbnail {rel}: {message}");
                self.stats.failed += 1;
                self.failed.insert(rel, (key, message));
            }
        }
    }

    /// Puts a picture in memory, evicting the least recently drawn ones over capacity.
    fn insert(&mut self, rel: String, key: u64, image: egui::ColorImage, origin: ThumbOrigin) {
        self.entries.insert(
            rel,
            Slot {
                key,
                image: Arc::new(image),
                texture: None,
                used: self.frame,
                origin,
            },
        );
        while self.entries.len() > self.capacity {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(rel, slot)| (slot.used, *rel))
                .map(|(rel, _)| rel.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
            self.stats.evicted += 1;
        }
    }

    /// The per-frame step: takes finished work and hands out queued requests. Never blocks.
    #[tracing::instrument(target = "regia_editor", skip_all, name = "thumb_pump")]
    pub fn pump(&mut self) {
        self.collect(false);
        self.dispatch();
    }

    /// Runs until every queued request is answered (tests and the agent; the panel uses
    /// [`Self::pump`]). Meshes end up waiting for [`Self::render_meshes`].
    pub fn finish(&mut self) {
        loop {
            self.collect(false);
            self.dispatch();
            if self.in_flight.is_empty() && self.queued.is_empty() {
                break;
            }
            if self.collect(true) == 0 && self.worker.is_none() && self.inline.is_empty() {
                // Nothing can arrive any more.
                break;
            }
        }
    }

    /// Renders up to `budget` waiting model and material thumbnails on `device` (main thread; a
    /// few per frame keeps the frame smooth) and stores them on disk through the worker. Returns
    /// how many were rendered or failed.
    pub fn render_meshes(&mut self, device: &RenderDevice, library: &MeshLibrary, budget: usize) -> usize {
        if self.gpu_wait.is_empty() || budget == 0 {
            return 0;
        }
        let Some(root) = self.root.clone() else {
            return 0;
        };
        let _span = tracing::info_span!(target: "regia_editor", "thumb_render_meshes").entered();
        let mut done = 0;
        while done < budget {
            let Some((rel, key)) = self.gpu_wait.pop_front() else {
                break;
            };
            self.gpu_waiting.remove(&rel);
            done += 1;
            if self.keys.get(&rel).copied().flatten() != Some(key) {
                self.stats.stale_results += 1;
                continue;
            }
            let subject = match ThumbSource::of(&rel) {
                Some(ThumbSource::Material) => material_sphere(library, &root, &rel),
                _ => library.model(Some(root.as_path()), &rel),
            };
            let rendered = subject.and_then(|model| {
                let mut guard = self
                    .renderer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard
                    .as_ref()
                    .is_none_or(|renderer| renderer.device().device_id() != device.device_id())
                {
                    *guard = Some(ThumbnailRenderer::new(device));
                }
                let Some(renderer) = guard.as_mut() else {
                    return Err("no thumbnail renderer".to_string());
                };
                let mut images = renderer.render(&[ThumbJob::model(model, self.px)]);
                match (renderer.stats().rendered, images.pop()) {
                    (1, Some(image)) => Ok(image),
                    _ => Err("nothing to draw (no triangles or a GPU error)".to_string()),
                }
            });
            match rendered {
                Ok(image) => {
                    self.stats.rendered += 1;
                    self.send(Job::Store {
                        file: cache_file(&root, key),
                        rgba: unmultiplied_rgba(&image),
                        px: self.px,
                    });
                    self.insert(rel, key, image, ThumbOrigin::Rendered);
                }
                Err(message) => {
                    tracing::warn!(target: "regia_editor", "thumbnail {rel}: {message}");
                    self.stats.failed += 1;
                    self.failed.insert(rel, (key, message));
                }
            }
        }
        done
    }

    /// The counters and queue sizes as JSON (`editor.asset_thumbnails`).
    #[must_use]
    pub fn stats_json(&self) -> Value {
        let stats = self.stats;
        json!({
            "px": self.px,
            "memory_entries": self.entries.len(),
            "capacity": self.capacity,
            "queued": self.queued.len(),
            "in_flight": self.in_flight.len(),
            "gpu_waiting": self.gpu_wait.len(),
            "requests": stats.requests,
            "memory_hits": stats.memory_hits,
            "file_stats": stats.file_stats,
            "dispatched": stats.dispatched,
            "disk_hits": stats.disk_hits,
            "made": stats.made,
            "rendered": stats.rendered,
            "failed": stats.failed,
            "evicted": stats.evicted,
            "dropped": stats.dropped,
            "stale_results": stats.stale_results,
            "pruned_files": stats.pruned_files,
        })
    }
}

/// Forgets changed paths in the world's thumbnail cache (the live asset watcher's sources).
pub fn note_changed(world: &mut World, paths: &[PathBuf]) {
    if let Some(mut cache) = world.get_resource_mut::<ThumbCache>() {
        cache.forget(paths);
    }
}

fn thumb_error(error: RpcError, code: &str) -> RpcError {
    coded(error, "thumb_error", code)
}

/// `error` with `data.<key> = code` (the protocol's machine-readable cause).
fn coded(mut error: RpcError, key: &str, code: &str) -> RpcError {
    if let Some(data) = error.data.as_mut() {
        data[key] = json!(code);
    }
    error
}

/// `editor.asset_thumbnails`.
fn asset_thumbnails_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| {
            thumb_error(
                RpcError::not_found(
                    "no project is open",
                    "start the editor with --project <dir> (or open one in the project browser)",
                ),
                "thumb_no_project",
            )
        })?;
    let paths: Vec<String> = match params.get("paths").and_then(Value::as_array) {
        Some(list) if !list.is_empty() && list.len() <= 256 => list
            .iter()
            .map(|path| path.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()
            .ok_or_else(|| {
                thumb_error(
                    RpcError::invalid_params("every entry of `paths` must be a string"),
                    "thumb_bad_params",
                )
            })?,
        _ => {
            return Err(thumb_error(
                RpcError::invalid_params("`paths` must be an array of 1 to 256 project-relative paths"),
                "thumb_bad_params",
            ));
        }
    };
    for path in &paths {
        let normalized = path.replace('\\', "/");
        let escapes = normalized.split('/').any(|part| part == "..");
        if escapes || Path::new(path).is_absolute() || !normalized.starts_with("assets/") {
            return Err(thumb_error(
                RpcError::invalid_params(format!("{path:?} is not a project-relative path under assets/")),
                "thumb_bad_path",
            ));
        }
    }
    let wait = params.get("wait").and_then(Value::as_bool).unwrap_or(true);
    let clear = params
        .get("clear_memory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut cache = world.remove_resource::<ThumbCache>().unwrap_or_default();
    cache.set_root(Some(&root));
    if clear {
        cache.clear_memory();
    }
    cache.begin_frame();
    for path in &paths {
        cache.request(path);
    }
    if wait {
        cache.finish();
        if cache.gpu_waiting() > 0
            && let Some(device) = world.get_resource::<RenderDevice>().cloned()
        {
            let library = world.get_resource_or_insert_with(MeshLibrary::default).clone();
            cache.render_meshes(&device, &library, usize::MAX);
        }
    } else {
        cache.pump();
    }
    let rows: Vec<Value> = paths
        .iter()
        .map(|path| {
            let state = cache.state(path);
            let mut row = json!({ "path": path, "state": state.name() });
            if let ThumbState::Failed(message) = &state {
                row["error"] = json!(message);
            }
            if state == ThumbState::Ready
                && let Some(origin) = cache.origin(path)
            {
                row["origin"] = json!(origin.name());
            }
            row
        })
        .collect();
    let out = json!({
        "thumbnails": rows,
        "stats": cache.stats_json(),
        "cache_dir": cache_dir(&root).display().to_string(),
    });
    world.insert_resource(cache);
    Ok(out)
}

/// The thumbnail cache's agent surface.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.asset_thumbnails",
                tool: "asset_thumbnails",
                description: "The Content Browser's thumbnail cache: requests pictures for project-relative \
                paths (from memory, the .regia/thumbs disk cache, a texture decode, or a GPU render \
                of a mesh) and reports each path's state and the cache counters.",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: true,
                        description: "1 to 256 project-relative paths under assets/.",
                    },
                    ParamSchema {
                        name: "wait",
                        kind: "boolean",
                        required: false,
                        description: "Finish the requests before replying (meshes render when the editor has a \
                        GPU device). Default true; false only queues them.",
                    },
                    ParamSchema {
                        name: "clear_memory",
                        kind: "boolean",
                        required: false,
                        description: "Drop the in-memory pictures first (the next answers come from disk). \
                        Default false.",
                    },
                ],
                result: "{ thumbnails: [{ path, state, origin?, error? }], stats: { px, memory_entries, capacity, \
                queued, in_flight, gpu_waiting, requests, memory_hits, file_stats, dispatched, disk_hits, made, \
                rendered, failed, evicted, dropped, stale_results, pruned_files }, cache_dir }",
                mutating: false,
                permission: Permission::Read,
            },
            asset_thumbnails_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.content_view",
                tool: "content_view",
                description: "The Content Browser's view: one row per asset (list) or thumbnail tiles (grid), the \
                    tile size, the folder shown with its breadcrumbs and back/forward history, the folder tree's \
                    open folders and rows, and the collection shown. Without params it only reports the view.",
                params: &[
                    ParamSchema {
                        name: "grid",
                        kind: "boolean",
                        required: false,
                        description: "true: thumbnail tiles; false: the list.",
                    },
                    ParamSchema {
                        name: "tile",
                        kind: "number",
                        required: false,
                        description: "Tile edge in points, 48 to 192 (default 96).",
                    },
                    crate::content_favorites::NAV_FOLDER,
                    crate::content_favorites::NAV_BACK,
                    crate::content_favorites::NAV_FORWARD,
                    crate::content_favorites::NAV_EXPAND,
                    crate::content_favorites::NAV_COLLAPSE,
                    crate::content_favorites::NAV_COLLECTION,
                ],
                result: "{ grid, tile, folder, collection, breadcrumbs, can_back, can_forward, tree: { expanded, \
                    root_open, rows: [{ path, depth, has_children, expanded, favorite, color }] } }",
                mutating: false,
                permission: Permission::Read,
            },
            content_view_method as Handler,
        ),
    ]
}

/// The smallest and largest grid tile edge, in points.
pub const TILE_RANGE: std::ops::RangeInclusive<f32> = 48.0..=192.0;

/// `editor.content_view`.
fn content_view_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut view = world
        .get_resource::<crate::content::ContentView>()
        .copied()
        .unwrap_or_default();
    if let Some(grid) = params.get("grid") {
        view.grid = grid.as_bool().ok_or_else(|| {
            coded(
                RpcError::invalid_params("`grid` must be true or false"),
                "view_error",
                "view_bad_params",
            )
        })?;
    }
    if let Some(tile) = params.get("tile") {
        let tile = tile
            .as_f64()
            .map(|tile| tile as f32)
            .filter(|tile| TILE_RANGE.contains(tile))
            .ok_or_else(|| {
                coded(
                    RpcError::invalid_params("`tile` must be a number from 48 to 192"),
                    "view_error",
                    "view_bad_params",
                )
            })?;
        view.tile = tile;
    }
    world.insert_resource(view);
    // The folder, breadcrumbs, history and tree (ADR-7759).
    let mut out = crate::content_favorites::view_nav(world, params)?;
    out["grid"] = json!(view.grid);
    out["tile"] = json!(view.tile);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // This worker has no thread: receiving a job and sending its reply are explicit barriers.
    // All decoding and disk IO still use the production worker path. Only delivery is controlled.
    struct ControlledWorker {
        jobs: Receiver<Job>,
        replies: Sender<Reply>,
    }

    impl ControlledWorker {
        fn install(cache: &mut ThumbCache, root: &Path) -> Self {
            let (job_tx, jobs) = std::sync::mpsc::channel();
            let (replies, reply_rx) = std::sync::mpsc::channel();
            cache.worker_started = true;
            cache.worker = Some(Worker {
                jobs: job_tx,
                replies: Mutex::new(reply_rx),
            });
            let worker = Self { jobs, replies };
            cache.set_root(Some(root));
            assert!(matches!(worker.jobs.try_recv(), Ok(Job::Prune { .. })));
            worker
        }

        fn load(&self, expected_rel: &str) -> (u64, PathBuf, Reply) {
            let job = self.jobs.try_recv().expect("a load was dispatched");
            let (key, file) = match &job {
                Job::Load { rel, key, file, .. } => {
                    assert_eq!(rel, expected_rel);
                    (*key, file.clone())
                }
                _ => panic!("expected a load"),
            };
            (key, file, run_job(job).expect("load reply"))
        }

        fn deliver(&self, cache: &mut ThumbCache, reply: Reply) {
            self.replies.send(reply).expect("cache is listening");
            assert_eq!(cache.collect(false), 1, "one explicit delivery");
        }
    }

    const EPOCH_PATH: &str = "assets/epoch.png";
    const OTHER_PATH: &str = "assets/other.png";

    fn epoch_project() -> PathBuf {
        project(
            "thumbnail_epoch_isolation",
            &[
                (EPOCH_PATH, png(4, 4, [255, 0, 0, 255])),
                (OTHER_PATH, png(4, 4, [0, 255, 0, 255])),
            ],
        )
    }

    fn edit_epoch(cache: &mut ThumbCache, root: &Path) {
        // Pad the valid PNG so length alone changes the source key, even with a frozen mtime.
        let path = root.join(EPOCH_PATH);
        let old = std::fs::metadata(&path).expect("source metadata");
        let modified = old.modified().expect("source mtime");
        let mut bytes = png(6, 6, [0, 0, 255, 255]);
        bytes.resize(bytes.len().max(old.len() as usize) + 17, 0);
        regia_test_utils::write_atomic(&path, &bytes).expect("atomic edit");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("edited source")
            .set_modified(modified)
            .expect("preserve mtime");
        let edited = std::fs::metadata(&path).expect("edited metadata");
        assert_ne!(edited.len(), old.len());
        assert_eq!(edited.modified().expect("edited mtime"), modified);
        cache.forget(&[path]);
    }

    fn assert_picture(cache: &ThumbCache, path: &str, color: egui::Color32) {
        assert_eq!(cache.state(path), ThumbState::Ready);
        let image = cache.image(path).expect("CPU picture");
        assert_eq!(pixel(&image, 8, 8), color);
    }

    fn epoch_reply_order(new_first: bool) {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 1);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.pump();
        let (key_a, file_a, reply_a) = worker.load(EPOCH_PATH);
        assert_eq!(cache.in_flight(), 1);

        edit_epoch(&mut cache, &root);
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.pump();
        assert_eq!(
            cache.in_flight(),
            2,
            "B must not be swallowed by the held A request"
        );
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.pump();
        assert_eq!(cache.stats().dispatched, 2, "the same B key is deduplicated");
        assert_eq!(cache.queued(), 0);
        let (key_b, file_b, reply_b) = worker.load(EPOCH_PATH);
        assert_ne!(key_a, key_b);
        assert_eq!(file_a, cache_file(&root, key_a));
        assert_eq!(file_b, cache_file(&root, key_b));
        let disk_b = std::fs::read(&file_b).expect("B disk picture");
        assert_eq!(
            pixel(&read_cached(&file_a, 16).expect("A disk picture"), 8, 8),
            egui::Color32::RED
        );
        assert_eq!(
            pixel(&read_cached(&file_b, 16).expect("B disk picture"), 8, 8),
            egui::Color32::BLUE
        );

        let (first, second) = if new_first {
            (reply_b, reply_a)
        } else {
            (reply_a, reply_b)
        };
        worker.deliver(&mut cache, first);
        assert_eq!(cache.in_flight(), 1, "one result cannot retire another epoch");
        assert!(cache.busy(), "the second result is still held");
        if new_first {
            assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
            assert_eq!(cache.stats().stale_results, 0);
        } else {
            assert_eq!(cache.state(EPOCH_PATH), ThumbState::Pending);
            assert!(cache.image(EPOCH_PATH).is_none(), "A cannot become current");
            assert_eq!(cache.stats().stale_results, 1);
        }
        worker.deliver(&mut cache, second);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.stats().stale_results, 1);
        assert_eq!(cache.stats().made, 1, "only the current epoch counts as made");
        assert_eq!(cache.in_flight(), 0);
        assert!(!cache.busy());
        assert_eq!(std::fs::read(&file_b).expect("B remains on disk"), disk_b);

        // A different visible tile finishes, evicts B, and re-entry must read B's disk key.
        cache.begin_frame();
        assert_eq!(cache.request(OTHER_PATH), ThumbState::Pending);
        cache.pump();
        let (_, _, reply) = worker.load(OTHER_PATH);
        worker.deliver(&mut cache, reply);
        assert_picture(&cache, OTHER_PATH, egui::Color32::GREEN);
        assert!(cache.image(EPOCH_PATH).is_none());
        assert_eq!(cache.stats().evicted, 1);
        cache.begin_frame();
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.pump();
        let (reentry_key, _, reply) = worker.load(EPOCH_PATH);
        assert_eq!(reentry_key, key_b);
        worker.deliver(&mut cache, reply);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!((cache.stats().made, cache.stats().disk_hits), (2, 1));
        assert_eq!(cache.stats().evicted, 2);
        assert_eq!(cache.stats().failed, 0);
        assert_eq!(cache.stats().dispatched, 4);
        assert_eq!(cache.in_flight(), 0);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn epoch_isolation_old_reply_then_new() {
        epoch_reply_order(false);
    }

    #[test]
    fn epoch_isolation_new_reply_then_old() {
        epoch_reply_order(true);
    }

    #[test]
    fn epoch_isolation_late_a_cannot_replace_b_after_disk_reentry() {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 1);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        cache.request(EPOCH_PATH);
        cache.pump();
        let (key_a, _, reply_a) = worker.load(EPOCH_PATH);
        edit_epoch(&mut cache, &root);
        cache.request(EPOCH_PATH);
        cache.pump();
        let (key_b, file_b, reply_b) = worker.load(EPOCH_PATH);
        assert_ne!(key_a, key_b);
        worker.deliver(&mut cache, reply_b);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.in_flight(), 1, "A stays held through eviction and re-entry");
        let disk_b = std::fs::read(&file_b).expect("B disk picture");

        cache.begin_frame();
        cache.request(OTHER_PATH);
        cache.pump();
        let (_, _, other) = worker.load(OTHER_PATH);
        worker.deliver(&mut cache, other);
        assert_picture(&cache, OTHER_PATH, egui::Color32::GREEN);
        assert!(cache.image(EPOCH_PATH).is_none(), "the other tile evicted B");
        assert_eq!(cache.in_flight(), 1);

        cache.begin_frame();
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.pump();
        assert_eq!(cache.in_flight(), 2, "held A must not swallow B's disk reload");
        let (reentry_key, reentry_file, reentry) = worker.load(EPOCH_PATH);
        assert_eq!(reentry_key, key_b);
        assert_eq!(reentry_file, file_b);
        worker.deliver(&mut cache, reentry);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.origin(EPOCH_PATH), Some(ThumbOrigin::Disk));
        assert_eq!(cache.in_flight(), 1);

        worker.deliver(&mut cache, reply_a);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.origin(EPOCH_PATH), Some(ThumbOrigin::Disk));
        assert_eq!(std::fs::read(&file_b).expect("B remains on disk"), disk_b);
        assert_eq!(cache.stats().stale_results, 1);
        assert_eq!((cache.stats().made, cache.stats().disk_hits), (2, 1));
        assert_eq!(cache.stats().evicted, 2);
        assert_eq!(cache.stats().file_stats, 3);
        assert_eq!(cache.stats().dispatched, 4);
        assert_eq!(cache.stats().failed, 0);
        assert_eq!(cache.in_flight(), 0);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn epoch_isolation_requested_edit_is_not_lost_when_a_finishes() {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 4);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        cache.request(EPOCH_PATH);
        cache.pump();
        let (_, _, reply_a) = worker.load(EPOCH_PATH);
        edit_epoch(&mut cache, &root);
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        worker.deliver(&mut cache, reply_a);
        cache.pump();
        // No second request for B: the first Pending answer must retain its work.
        let (_, _, reply_b) = worker.load(EPOCH_PATH);
        worker.deliver(&mut cache, reply_b);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.stats().stale_results, 1);
        assert_eq!(cache.in_flight(), 0);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn epoch_isolation_worker_failure_between_requests_recovers_inline() {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 4);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        cache.request(EPOCH_PATH);
        cache.pump();
        assert!(matches!(worker.jobs.try_recv(), Ok(Job::Load { .. })));
        edit_epoch(&mut cache, &root);
        // Both ends disappear before B is dispatched: send observes the failure first.
        drop(worker);
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.dispatch();
        cache.finish();
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.stats().failed, 0, "lost A must not fail the current B key");
        assert_eq!(cache.stats().dispatched, 2);
        assert_eq!(cache.in_flight(), 0);
        cache.request(OTHER_PATH);
        cache.finish();
        assert_picture(&cache, OTHER_PATH, egui::Color32::GREEN);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn epoch_isolation_lost_current_request_retries_only_after_forget() {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 4);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        cache.request(EPOCH_PATH);
        cache.pump();
        assert!(matches!(worker.jobs.try_recv(), Ok(Job::Load { .. })));
        edit_epoch(&mut cache, &root);
        cache.request(EPOCH_PATH);
        cache.pump();
        assert!(
            matches!(worker.jobs.try_recv(), Ok(Job::Load { .. })),
            "B dispatched"
        );
        drop(worker);
        cache.pump(); // collect observes the failure first; A and B are both lost.
        assert_eq!(cache.in_flight(), 0);
        assert_eq!(cache.stats().failed, 1, "only the current B request failed");
        assert!(
            matches!(cache.request(EPOCH_PATH), ThumbState::Failed(message) if message == "the thumbnail thread stopped")
        );
        let dispatched = cache.stats().dispatched;
        cache.finish();
        assert_eq!(
            cache.stats().dispatched,
            dispatched,
            "no automatic retry of failures"
        );
        cache.forget(&[PathBuf::from(EPOCH_PATH)]);
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.request(OTHER_PATH);
        cache.finish();
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_picture(&cache, OTHER_PATH, egui::Color32::GREEN);
        assert_eq!(cache.stats().failed, 1);
        assert_eq!(cache.in_flight(), 0);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn epoch_isolation_worker_loss_keeps_a_buffered_current_reply() {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 4);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        cache.request(EPOCH_PATH);
        cache.pump();
        assert!(matches!(worker.jobs.try_recv(), Ok(Job::Load { .. })));
        edit_epoch(&mut cache, &root);
        cache.request(EPOCH_PATH);
        cache.pump();
        let (_, _, reply_b) = worker.load(EPOCH_PATH);
        worker.replies.send(reply_b).expect("buffer B before worker loss");
        drop(worker);
        cache.pump();
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.stats().failed, 0, "B was answered; lost A is obsolete");
        assert_eq!(cache.in_flight(), 0);
        cache.clear_memory();
        assert_eq!(
            cache.request(EPOCH_PATH),
            ThumbState::Pending,
            "no hidden B failure"
        );
        cache.request(OTHER_PATH);
        cache.finish();
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_picture(&cache, OTHER_PATH, egui::Color32::GREEN);
        assert_eq!(cache.stats().disk_hits, 1);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn epoch_isolation_scrolled_away_b_is_dropped_and_reentry_renews_it() {
        let root = epoch_project();
        let mut cache = ThumbCache::new(16, 4);
        let worker = ControlledWorker::install(&mut cache, &root);
        cache.begin_frame();
        cache.request(EPOCH_PATH);
        cache.pump();
        let (_, _, reply_a) = worker.load(EPOCH_PATH);
        edit_epoch(&mut cache, &root);
        cache.request(EPOCH_PATH);
        assert_eq!(cache.queued(), 1, "B is queued independently of A");
        for _ in 0..=KEEP_FRAMES {
            cache.begin_frame();
        }
        cache.request(OTHER_PATH);
        cache.pump();
        assert_eq!(cache.stats().dropped, 1, "only the offscreen B request expires");
        assert_eq!(cache.queued(), 0);
        let (_, _, other) = worker.load(OTHER_PATH);
        worker.deliver(&mut cache, other);
        assert_picture(&cache, OTHER_PATH, egui::Color32::GREEN);
        assert_eq!(cache.in_flight(), 1, "A is still held");
        worker.deliver(&mut cache, reply_a);
        assert!(cache.image(EPOCH_PATH).is_none());
        assert_eq!(cache.stats().stale_results, 1);
        assert_eq!(cache.in_flight(), 0);
        cache.begin_frame();
        assert_eq!(cache.request(EPOCH_PATH), ThumbState::Pending);
        cache.pump();
        let (_, _, reply_b) = worker.load(EPOCH_PATH);
        worker.deliver(&mut cache, reply_b);
        assert_picture(&cache, EPOCH_PATH, egui::Color32::BLUE);
        assert_eq!(cache.stats().dispatched, 3, "A, other, and renewed B only");
        assert_eq!(cache.stats().made, 2);
        assert_eq!(cache.stats().failed, 0);
        assert!(!cache.busy());
        std::fs::remove_dir_all(root).expect("remove fixture");
    }

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        use image::ImageEncoder as _;
        let pixels = image::RgbaImage::from_pixel(width, height, image::Rgba(rgba));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(pixels.as_raw(), width, height, image::ExtendedColorType::Rgba8)
            .expect("png encode");
        bytes
    }

    fn project(tag: &str, files: &[(&str, Vec<u8>)]) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        for (rel, bytes) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            regia_test_utils::write_atomic(&path, bytes).expect("write");
        }
        root
    }

    /// A project-relative path as the Content Browser's index holds it (`/` on every OS).
    fn rel(path: &str) -> String {
        path.to_owned()
    }

    fn pixel(image: &egui::ColorImage, x: usize, y: usize) -> egui::Color32 {
        image.pixels[y * image.size[0] + x]
    }

    fn png_files(root: &Path) -> usize {
        std::fs::read_dir(cache_dir(root)).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
                .count()
        })
    }

    #[test]
    fn fnv_and_keys() {
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(b"", FNV_OFFSET), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a", FNV_OFFSET), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar", FNV_OFFSET), 0x8594_4171_f739_67e8);
        let key = thumb_key("assets/a.png", 10, 99, 96);
        assert_eq!(key, thumb_key("assets\\a.png", 10, 99, 96), "either separator");
        for other in [
            thumb_key("assets/b.png", 10, 99, 96),
            thumb_key("assets/a.png", 11, 99, 96),
            thumb_key("assets/a.png", 10, 100, 96),
            thumb_key("assets/a.png", 10, 99, 128),
        ] {
            assert_ne!(key, other);
        }
        assert_eq!(
            cache_file(Path::new("p"), 0xab)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("00000000000000ab.png")
        );
        assert!(cache_file(Path::new("p"), 0xab).starts_with(Path::new("p").join(".regia").join("thumbs")));
        assert_eq!(ThumbSource::of("assets/A.JPG"), Some(ThumbSource::Texture));
        assert_eq!(ThumbSource::of("assets/m.gltf"), Some(ThumbSource::Mesh));
        assert_eq!(ThumbSource::of("assets/x.level.ron"), None);
        assert_eq!(
            ThumbSource::of("assets/m/Rock.Material.ron"),
            Some(ThumbSource::Material)
        );
        assert_eq!(
            ThumbSource::of("assets/g.matgraph.ron"),
            Some(ThumbSource::Material)
        );
        assert_eq!(ThumbSource::of("assets/x.ktx2"), None, "no decoder: icon");
    }

    #[test]
    fn fit_keeps_the_aspect_and_never_upscales() {
        assert_eq!(fit_size(1024, 512, 96), (96, 48));
        assert_eq!(fit_size(512, 1024, 96), (48, 96));
        assert_eq!(fit_size(10, 4000, 96), (1, 96), "at least one pixel");
        assert_eq!(fit_size(32, 16, 96), (32, 16), "small pictures keep their size");
        assert_eq!(fit_size(96, 96, 96), (96, 96));
    }

    #[test]
    fn textures_are_made_once_then_read_from_disk() {
        let root = project(
            "thumb_made_then_disk",
            &[
                ("assets/tex/wide.png", png(64, 32, [255, 0, 0, 255])),
                ("assets/tex/tall.png", png(100, 200, [0, 255, 0, 255])),
                ("assets/tex/broken.png", b"not a png".to_vec()),
                ("assets/meshes/model.glb", b"glTF".to_vec()),
                ("assets/mat/stone.material.ron", b"()".to_vec()),
                ("assets/readme.txt", b"hello".to_vec()),
            ],
        );
        let material = rel("assets/mat/stone.material.ron");
        let (wide, tall, broken, model, readme) = (
            rel("assets/tex/wide.png"),
            rel("assets/tex/tall.png"),
            rel("assets/tex/broken.png"),
            rel("assets/meshes/model.glb"),
            rel("assets/readme.txt"),
        );
        let mut cache = ThumbCache::new(32, 16);
        cache.set_root(Some(&root));
        cache.begin_frame();
        for path in [&wide, &tall, &broken, &model, &material] {
            assert_eq!(cache.request(path), ThumbState::Pending, "{path}");
        }
        assert_eq!(cache.request(&readme), ThumbState::Unsupported);
        assert_eq!(
            cache.request(&rel("assets/tex/gone.png")).name(),
            "failed",
            "missing file"
        );
        cache.finish();

        let stats = cache.stats();
        assert_eq!(
            (stats.made, stats.disk_hits, stats.failed),
            (2, 0, 1),
            "{stats:?}"
        );
        assert_eq!(
            cache.gpu_waiting(),
            2,
            "the model and the material wait for a GPU"
        );
        assert_eq!(cache.request(&model), ThumbState::NeedsGpu);
        assert_eq!(cache.request(&material), ThumbState::NeedsGpu);
        assert!(matches!(cache.request(&broken), ThumbState::Failed(message) if message.contains("decode")));
        assert_eq!(cache.request(&wide), ThumbState::Ready);
        assert_eq!(cache.origin(&wide), Some(ThumbOrigin::Made));
        assert_eq!(png_files(&root), 2, "the two made pictures are stored");

        // 64×32 shrinks to 32×16, centered: red in the middle, clear above and below.
        let made = cache.image(&wide).expect("picture");
        assert_eq!(made.size, [32, 32]);
        assert_eq!(pixel(&made, 16, 16), egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(pixel(&made, 16, 2), egui::Color32::TRANSPARENT);
        let tall_image = cache.image(&tall).expect("picture");
        assert_eq!(
            pixel(&tall_image, 2, 16),
            egui::Color32::TRANSPARENT,
            "100×200 → 16×32"
        );
        assert_eq!(pixel(&tall_image, 16, 16), egui::Color32::from_rgb(0, 255, 0));

        // A second editor session reads both from disk, bit for bit.
        let dispatched = cache.stats().dispatched;
        let mut again = ThumbCache::new(32, 16);
        again.set_root(Some(&root));
        again.begin_frame();
        again.request(&wide);
        again.request(&tall);
        again.finish();
        assert_eq!((again.stats().disk_hits, again.stats().made), (2, 0));
        assert_eq!(again.origin(&wide), Some(ThumbOrigin::Disk));
        assert_eq!(again.image(&wide).expect("disk picture").pixels, made.pixels);
        // The first cache answers from memory and never asks the worker again.
        cache.begin_frame();
        assert_eq!(cache.request(&wide), ThumbState::Ready);
        assert_eq!(
            cache.request(&broken).name(),
            "failed",
            "failures are not retried"
        );
        cache.pump();
        assert_eq!(cache.stats().dispatched, dispatched);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit 11-03 editor M2: when a dead worker was first noticed by a send (not by `collect`),
    /// the requests it held stayed in flight forever and `request` answered `Pending` forever.
    #[test]
    fn a_worker_found_dead_by_a_send_fails_its_requests() {
        let root = project(
            "thumb_dead_worker",
            &[
                ("assets/a.png", png(4, 4, [255, 0, 0, 255])),
                ("assets/b.png", png(4, 4, [0, 255, 0, 255])),
                ("assets/c.png", png(4, 4, [0, 0, 255, 255])),
            ],
        );
        let mut cache = ThumbCache::new(16, 8);
        // A worker that takes jobs and then dies with them (it panicked mid-load).
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
        cache.worker_started = true;
        cache.worker = Some(Worker {
            jobs: job_tx,
            replies: Mutex::new(reply_rx),
        });
        cache.set_root(Some(&root));
        cache.begin_frame();
        assert_eq!(cache.request(&rel("assets/a.png")), ThumbState::Pending);
        assert_eq!(cache.request(&rel("assets/b.png")), ThumbState::Pending);
        cache.dispatch();
        assert_eq!(cache.in_flight.len(), 2, "both went to the worker");
        drop(job_rx);
        drop(reply_tx);
        // The next send is the first to see the dead thread.
        cache.request(&rel("assets/c.png"));
        cache.dispatch();
        cache.finish();
        assert_eq!(
            cache.request(&rel("assets/c.png")),
            ThumbState::Ready,
            "ran here instead"
        );
        for path in ["assets/a.png", "assets/b.png"] {
            let state = cache.request(&rel(path));
            assert!(
                matches!(state, ThumbState::Failed(_)),
                "{path} was orphaned in flight: {state:?}"
            );
        }
        assert!(cache.in_flight.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_edited_file_gets_a_new_picture() {
        let root = project("thumb_edited", &[("assets/a.png", png(8, 8, [255, 0, 0, 255]))]);
        let path = rel("assets/a.png");
        let mut cache = ThumbCache::new(16, 8);
        cache.set_root(Some(&root));
        cache.begin_frame();
        cache.request(&path);
        cache.finish();
        assert_eq!(cache.stats().made, 1);

        // Another size: another key, whatever the clock says.
        regia_test_utils::write_atomic(&root.join("assets/a.png"), &png(12, 12, [0, 0, 255, 255]))
            .expect("edit");
        cache.begin_frame();
        assert_eq!(
            cache.request(&path),
            ThumbState::Ready,
            "not told yet: the old picture"
        );
        cache.forget(&[root.join("assets/a.png")]);
        assert_eq!(
            cache.request(&path),
            ThumbState::Pending,
            "told: a new picture is on its way"
        );
        assert!(cache.image(&path).is_some(), "the old picture shows until then");
        cache.finish();
        assert_eq!(cache.stats().made, 2);
        let image = cache.image(&path).expect("new picture");
        assert_eq!(pixel(&image, 8, 8), egui::Color32::from_rgb(0, 0, 255));
        assert_eq!(png_files(&root), 2, "both versions stored until a prune");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn requests_for_tiles_that_left_the_view_are_dropped() {
        let files: Vec<(String, Vec<u8>)> = (0..12)
            .map(|i| (format!("assets/t{i:02}.png"), png(4, 4, [9, 9, 9, 255])))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(p, b)| (p.as_str(), b.clone())).collect();
        let root = project("thumb_dropped", &refs);
        let paths: Vec<String> = files.iter().map(|(p, _)| rel(p)).collect();
        let mut cache = ThumbCache::new(16, 64);
        cache.set_root(Some(&root));

        // Requested once, then scrolled past for more than KEEP_FRAMES frames.
        cache.begin_frame();
        for path in &paths {
            cache.request(path);
        }
        for _ in 0..=KEEP_FRAMES {
            cache.begin_frame();
        }
        cache.pump();
        assert_eq!(cache.stats().dispatched, 0);
        assert_eq!(cache.stats().dropped, 12);
        assert_eq!(cache.queued(), 0);

        // Requested and drawn again: at most MAX_IN_FLIGHT go to the worker per pump.
        cache.begin_frame();
        for path in &paths {
            cache.request(path);
        }
        cache.pump();
        assert_eq!(cache.stats().dispatched, MAX_IN_FLIGHT as u64);
        cache.finish();
        assert_eq!(cache.stats().made, 12);
        assert!(!cache.busy());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_keeps_the_most_recently_drawn() {
        let files: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| (format!("assets/t{i}.png"), png(4, 4, [1, 2, 3, 255])))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(p, b)| (p.as_str(), b.clone())).collect();
        let root = project("thumb_lru", &refs);
        let path = |i: usize| rel(&format!("assets/t{i}.png"));
        let mut cache = ThumbCache::new(16, 4);
        cache.set_root(Some(&root));
        cache.begin_frame();
        for i in 0..6 {
            cache.request(&path(i));
        }
        cache.finish();
        assert_eq!(cache.memory_entries(), 4);
        assert_eq!(cache.stats().evicted, 2);

        // t5 is drawn again; t0 comes back (from disk) and pushes out the oldest of the rest.
        cache.begin_frame();
        let kept: Vec<usize> = (0..6).filter(|i| cache.image(&path(*i)).is_some()).collect();
        let fresh = *kept.last().expect("kept");
        assert_eq!(cache.request(&path(fresh)), ThumbState::Ready);
        let evicted = (0..6).find(|i| !kept.contains(i)).expect("an evicted one");
        assert_eq!(cache.request(&path(evicted)), ThumbState::Pending);
        cache.finish();
        assert_eq!(cache.stats().disk_hits, 1, "back from disk, not made again");
        assert_eq!(cache.memory_entries(), 4);
        assert!(cache.image(&path(fresh)).is_some(), "drawn this frame: kept");
        assert!(cache.image(&path(evicted)).is_some(), "just loaded: kept");
        assert_eq!(cache.stats().evicted, 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_removes_the_oldest_pictures_only() {
        let dir = regia_test_utils::unique_temp_dir("thumb_prune");
        for i in 0..10u64 {
            let path = dir.join(format!("{i:016x}.png"));
            regia_test_utils::write_atomic(&path, &[0u8; 1000]).expect("write");
            let file = std::fs::File::options().write(true).open(&path).expect("open");
            file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i))
                .expect("set time");
        }
        regia_test_utils::write_atomic(&dir.join("notes.txt"), &[0u8; 10_000]).expect("write");
        assert_eq!(prune_disk(&dir, 20_000), (0, 0), "under budget: nothing");
        assert_eq!(
            prune_disk(&dir, 5_000),
            (7, 7_000),
            "down to three quarters of 5000"
        );
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "0000000000000007.png",
                "0000000000000008.png",
                "0000000000000009.png",
                "notes.txt"
            ],
            "the newest three and the non-picture"
        );
        assert_eq!(prune_disk(&dir, 5_000), (0, 0));
        assert_eq!(prune_disk(&dir.join("missing"), 1), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agent_method_reports_states_origins_and_errors() {
        let root = project(
            "thumb_agent",
            &[
                ("assets/a.png", png(8, 8, [255, 255, 255, 255])),
                ("assets/m.glb", b"glTF".to_vec()),
                ("assets/notes.txt", b"x".to_vec()),
            ],
        );
        let mut world = World::new();
        let code = |err: RpcError| err.data.and_then(|data| data.get("thumb_error").cloned());
        let err = asset_thumbnails_method(&mut world, &json!({ "paths": ["assets/a.png"] }))
            .expect_err("no project");
        assert_eq!(code(err), Some(json!("thumb_no_project")));
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        for (params, expected) in [
            (json!({}), "thumb_bad_params"),
            (json!({ "paths": [] }), "thumb_bad_params"),
            (json!({ "paths": [3] }), "thumb_bad_params"),
            (json!({ "paths": ["assets/../secret.png"] }), "thumb_bad_path"),
            (json!({ "paths": ["textures/a.png"] }), "thumb_bad_path"),
            (
                json!({ "paths": [root.join("assets/a.png").display().to_string()] }),
                "thumb_bad_path",
            ),
        ] {
            let err = asset_thumbnails_method(&mut world, &params).expect_err("bad params");
            assert_eq!(code(err), Some(json!(expected)), "{params}");
        }

        let params = json!({ "paths": ["assets/a.png", "assets/m.glb", "assets/notes.txt"] });
        let reply = asset_thumbnails_method(&mut world, &params).expect("thumbnails");
        let states: Vec<&str> = reply["thumbnails"]
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|row| row["state"].as_str())
            .collect();
        assert_eq!(states, ["ready", "needs_gpu", "unsupported"], "{reply}");
        assert_eq!(reply["thumbnails"][0]["origin"], "made");
        assert_eq!(reply["stats"]["made"], 1);
        assert_eq!(reply["stats"]["gpu_waiting"], 1, "no GPU device in this world");

        let reply = asset_thumbnails_method(
            &mut world,
            &json!({ "paths": ["assets/a.png"], "clear_memory": true }),
        )
        .expect("thumbnails");
        assert_eq!(reply["thumbnails"][0]["origin"], "disk", "{reply}");
        assert_eq!(reply["stats"]["disk_hits"], 1);
        assert!(
            world.get_resource::<ThumbCache>().is_some(),
            "the cache stays in the world"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_view_method_switches_and_checks_its_params() {
        let mut world = World::new();
        // The reply also carries the navigation (ADR-7759): with no project, the `assets` root.
        let with_nav = |grid: bool, tile: f64| {
            json!({
                "grid": grid,
                "tile": tile,
                "folder": "assets",
                "collection": null,
                "breadcrumbs": ["assets"],
                "can_back": false,
                "can_forward": false,
                "tree": {
                    "expanded": [],
                    "root_open": true,
                    "rows": [{
                        "path": "assets",
                        "depth": 0,
                        "has_children": false,
                        "expanded": false,
                        "favorite": false,
                        "color": null,
                    }],
                },
            })
        };
        let reply = content_view_method(&mut world, &json!({})).expect("view");
        assert_eq!(reply, with_nav(false, 96.0), "the list by default");
        let reply = content_view_method(&mut world, &json!({ "grid": true, "tile": 128 })).expect("view");
        assert_eq!(reply, with_nav(true, 128.0));
        assert_eq!(
            *world.resource::<crate::content::ContentView>(),
            crate::content::ContentView {
                grid: true,
                tile: 128.0
            }
        );
        for params in [
            json!({ "grid": "yes" }),
            json!({ "tile": 8 }),
            json!({ "tile": "big" }),
        ] {
            let err = content_view_method(&mut world, &params).expect_err("bad view params");
            assert_eq!(
                err.data.and_then(|data| data.get("view_error").cloned()),
                Some(json!("view_bad_params")),
                "{params}"
            );
        }
        assert!(
            world.resource::<crate::content::ContentView>().grid,
            "a refused call changes nothing"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Refresh and custom thumbnails captured from the viewport (spec addendum § 8.8, ADR-7788)
// ---------------------------------------------------------------------------------------------

/// The folder of custom thumbnails (`<root>/.regia/thumbs/custom`). Being in this folder is the
/// marker: [`prune_disk`] reads only the top folder, so custom pictures are never pruned, and a
/// refresh keeps them.
#[must_use]
pub fn custom_dir(root: &Path) -> PathBuf {
    cache_dir(root).join("custom")
}

/// The custom thumbnail of a project-relative path (named by the FNV-1a hash of the path, so it
/// follows no source edit: a custom picture stays until it is cleared).
#[must_use]
pub fn custom_file(root: &Path, rel: &str) -> PathBuf {
    let hash = fnv1a64(rel.replace('\\', "/").as_bytes(), FNV_OFFSET);
    custom_dir(root).join(format!("{hash:016x}.png"))
}

/// The cache key of a custom picture: [`thumb_key`] of the path with a `custom:` prefix and the
/// custom file's length and time, so a new capture gets a new key.
#[must_use]
pub fn custom_key(rel: &str, len: u64, modified_ns: u128, px: u32) -> u64 {
    thumb_key(
        &format!("custom:{}", rel.replace('\\', "/")),
        len,
        modified_ns,
        px,
    )
}

/// The largest centered square of `image`, shrunk (triangle filter) to `px`×`px` RGBA bytes.
#[must_use]
pub fn crop_square(image: &egui::ColorImage, px: u32) -> Vec<u8> {
    let [width, height] = image.size;
    let side = width.min(height).max(1);
    let (x0, y0) = ((width - side.min(width)) / 2, (height - side.min(height)) / 2);
    let mut square = image::RgbaImage::new(side as u32, side as u32);
    for y in 0..side.min(height) {
        for x in 0..side.min(width) {
            let pixel = image.pixels[(y0 + y) * width + x0 + x].to_srgba_unmultiplied();
            square.put_pixel(x as u32, y as u32, image::Rgba(pixel));
        }
    }
    image::imageops::resize(&square, px, px, image::imageops::FilterType::Triangle).into_raw()
}

/// Stores `image` (any size; its centered square is used) as the custom thumbnail of `rel`.
///
/// # Errors
///
/// The PNG cannot be encoded or written.
pub fn store_custom(root: &Path, rel: &str, image: &egui::ColorImage, px: u32) -> Result<PathBuf, String> {
    let rgba = crop_square(image, px);
    let mut bytes = Vec::new();
    {
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&rgba, px, px, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("cannot encode the thumbnail: {e}"))?;
    }
    let file = custom_file(root, rel);
    crate::atomic_write::write_atomic(&file, &bytes)?;
    Ok(file)
}

/// Removes the custom thumbnail of `rel`; `true` when there was one.
///
/// # Errors
///
/// The file exists but cannot be removed.
pub fn clear_custom(root: &Path, rel: &str) -> Result<bool, String> {
    let file = custom_file(root, rel);
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", file.display())),
    }
}

impl ThumbCache {
    /// The custom thumbnail's length and time for `rel` (read once per path until forgotten).
    fn custom_stamp(&mut self, rel: &str) -> Option<(u64, u128)> {
        if let Some(stamp) = self.customs.get(rel) {
            return *stamp;
        }
        let stamp = self.root.as_ref().and_then(|root| {
            let meta = std::fs::metadata(custom_file(root, rel))
                .ok()
                .filter(std::fs::Metadata::is_file)?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_nanos());
            Some((meta.len(), modified))
        });
        self.customs.insert(rel.to_string(), stamp);
        stamp
    }

    /// Whether `rel` shows a custom thumbnail.
    pub fn has_custom(&mut self, rel: &str) -> bool {
        self.custom_stamp(rel).is_some()
    }

    /// Refresh thumbnail: forgets the picture of `rel` and removes its automatic disk copy, so the
    /// next request makes it again from the source. A custom thumbnail stays (it is the user's
    /// choice; [`clear_custom`] removes it). Returns whether a custom thumbnail is shown.
    pub fn refresh(&mut self, rel: &str) -> bool {
        let rel = rel.replace('\\', "/");
        if let Some(root) = self.root.clone() {
            let automatic = std::fs::metadata(root.join(&rel)).ok().map(|meta| {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_nanos());
                thumb_key(&rel, meta.len(), modified, self.px)
            });
            for key in automatic
                .into_iter()
                .chain(self.keys.get(&rel).copied().flatten())
            {
                let _ = std::fs::remove_file(cache_file(&root, key));
            }
        }
        self.entries.remove(&rel);
        self.forget(&[PathBuf::from(&rel)]);
        self.has_custom(&rel)
    }
}

/// What the Content Browser's thumbnail menu asks for (one appended block in an asset's menu).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbRequest {
    /// Refresh thumbnail.
    Refresh(String),
    /// Capture thumbnail from viewport.
    Capture(String),
}

/// The pending request, run after the browser releases its borrow of the world.
#[derive(Resource, Debug, Clone, Default)]
pub struct PendingThumbRequest(pub Option<ThumbRequest>);

/// The two thumbnail items of an asset's context menu (Refresh, Capture from viewport). Menu items are
/// buttons, so the keyboard reaches them like every other menu entry.
pub fn thumbnail_menu_items(ui: &mut egui::Ui, world: &mut World, rel: &str) {
    if ui
        .button("Refresh thumbnail")
        .on_hover_text("Make this asset's picture again from its source")
        .clicked()
        && crate::asset_package::first_click_in_frame(world, ui.ctx().cumulative_frame_nr())
    {
        world.insert_resource(PendingThumbRequest(Some(ThumbRequest::Refresh(rel.to_string()))));
        ui.close();
    }
    if ui
        .button("Capture thumbnail from viewport")
        .on_hover_text("Use the active viewport's view, cropped square, as this asset's picture")
        .clicked()
        && crate::asset_package::first_click_in_frame(world, ui.ctx().cumulative_frame_nr())
    {
        world.insert_resource(PendingThumbRequest(Some(ThumbRequest::Capture(rel.to_string()))));
        ui.close();
    }
}

/// Runs a pending [`ThumbRequest`]; returns its status line.
pub fn run_pending(world: &mut World) -> Option<String> {
    let request = world.get_resource_mut::<PendingThumbRequest>()?.0.take()?;
    Some(match request {
        ThumbRequest::Refresh(rel) => match refresh_paths(world, std::slice::from_ref(&rel)) {
            Ok(_) => format!("Refreshed the thumbnail of {rel}"),
            Err(err) => err.message,
        },
        ThumbRequest::Capture(rel) => match capture_from_viewport(world, &rel) {
            Ok(_) => format!("Captured the thumbnail of {rel} from the viewport"),
            Err(err) => err.message,
        },
    })
}

fn project_of(world: &World) -> Result<PathBuf, RpcError> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|project| project.root.clone())
        .ok_or_else(|| {
            thumb_error(
                RpcError::not_found(
                    "no project is open",
                    "start the editor with --project <dir> (or open one in the project browser)",
                ),
                "thumb_no_project",
            )
        })
}

fn checked_rel(root: &Path, path: &str) -> Result<String, RpcError> {
    let normalized = path.replace('\\', "/");
    if normalized.split('/').any(|part| part == "..")
        || Path::new(path).is_absolute()
        || !normalized.starts_with("assets/")
    {
        return Err(thumb_error(
            RpcError::invalid_params(format!("{path:?} is not a project-relative path under assets/")),
            "thumb_bad_path",
        ));
    }
    if !root.join(&normalized).is_file() {
        return Err(thumb_error(
            RpcError::not_found(
                format!("{path} does not exist"),
                "list assets with editor.content_query",
            ),
            "thumb_missing",
        ));
    }
    Ok(normalized)
}

/// Refreshes `paths` in the world's cache; answers `(path, custom)` per path.
fn refresh_paths(world: &mut World, paths: &[String]) -> Result<Vec<(String, bool)>, RpcError> {
    let root = project_of(world)?;
    let rels = paths
        .iter()
        .map(|path| checked_rel(&root, path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut cache = world.remove_resource::<ThumbCache>().unwrap_or_default();
    cache.set_root(Some(&root));
    let out = rels
        .into_iter()
        .map(|rel| {
            let custom = cache.refresh(&rel);
            (rel, custom)
        })
        .collect();
    world.insert_resource(cache);
    crate::content_index::invalidate(world);
    Ok(out)
}

/// Capture thumbnail from viewport: renders the active pane's current view (no overlays), crops it
/// square and stores it as the custom thumbnail of `path`.
///
/// # Errors
///
/// No project, a bad path, no GPU device, or a render or write failure (with a `thumb_error` code).
pub fn capture_from_viewport(world: &mut World, path: &str) -> Result<PathBuf, RpcError> {
    let root = project_of(world)?;
    let rel = checked_rel(&root, path)?;
    let device = crate::agent_ext::viewport_device(world).map_err(|e| thumb_error(e, "thumb_no_device"))?;
    let layout = world
        .get_resource::<crate::views::ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let pane = layout.active.min(layout.panes.len().saturating_sub(1));
    let (kind, camera) = layout.pane_camera(world, pane);
    let px = world
        .get_resource::<ThumbCache>()
        .map_or(DEFAULT_THUMB_PX, ThumbCache::px);
    // Twice the picture size, so the shrink has pixels to average.
    let side = (px * 2).clamp(64, 512);
    let request = crate::render::PaneRequest {
        pane,
        kind,
        camera,
        width: side,
        height: side,
        ids: false,
        warm_frames: 0,
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let frame = crate::render::render_pane(&device, world, &request).ok_or_else(|| {
        thumb_error(
            RpcError::internal("the viewport render produced no image"),
            "thumb_capture_failed",
        )
    })?;
    let file = store_custom(&root, &rel, &frame.image, px)
        .map_err(|message| thumb_error(RpcError::internal(message), "thumb_capture_failed"))?;
    let mut cache = world.remove_resource::<ThumbCache>().unwrap_or_default();
    cache.set_root(Some(&root));
    cache.refresh(&rel);
    world.insert_resource(cache);
    Ok(file)
}

fn paths_param(params: &Value) -> Result<Vec<String>, RpcError> {
    match params.get("paths").and_then(Value::as_array) {
        Some(list) if !list.is_empty() && list.len() <= 256 => list
            .iter()
            .map(|path| path.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()
            .ok_or_else(|| {
                thumb_error(
                    RpcError::invalid_params("every entry of `paths` must be a string"),
                    "thumb_bad_params",
                )
            }),
        _ => Err(thumb_error(
            RpcError::invalid_params("`paths` must be an array of 1 to 256 project-relative paths"),
            "thumb_bad_params",
        )),
    }
}

/// `editor.thumbnail_refresh { paths, clear_custom? }`.
fn thumbnail_refresh_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let paths = paths_param(params)?;
    let clear = params
        .get("clear_custom")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if clear {
        let root = project_of(world)?;
        for path in &paths {
            let rel = checked_rel(&root, path)?;
            clear_custom(&root, &rel).map_err(|m| thumb_error(RpcError::internal(m), "thumb_io"))?;
        }
    }
    let rows = refresh_paths(world, &paths)?;
    Ok(json!({
        "refreshed": rows
            .iter()
            .map(|(path, custom)| json!({ "path": path, "custom": custom }))
            .collect::<Vec<_>>(),
    }))
}

/// `editor.thumbnail_capture { path }`.
fn thumbnail_capture_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params.get("path").and_then(Value::as_str).ok_or_else(|| {
        thumb_error(
            RpcError::invalid_params("`path` (a project-relative asset path) is required"),
            "thumb_bad_params",
        )
    })?;
    let file = capture_from_viewport(world, path)?;
    Ok(json!({ "path": path.replace('\\', "/"), "custom": true, "file": file.display().to_string() }))
}

/// Refresh and capture on the agent protocol (ADR-7788).
#[must_use]
pub fn refresh_capture_methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.thumbnail_refresh",
                tool: "thumbnail_refresh",
                description: "Refresh thumbnail (spec addendum § 8.8): forget the pictures of `paths` and remove their \
                    automatic copies in .regia/thumbs, so the next request makes them again from the source. A custom \
                    thumbnail (captured from the viewport) stays unless `clear_custom` is true.",
                params: &[
                    ParamSchema {
                        name: "paths",
                        kind: "array",
                        required: true,
                        description: "1 to 256 project-relative paths under assets/.",
                    },
                    ParamSchema {
                        name: "clear_custom",
                        kind: "boolean",
                        required: false,
                        description: "Also remove the custom thumbnails (default false).",
                    },
                ],
                result: "{ refreshed: [{ path, custom }] }",
                mutating: true,
                permission: Permission::Assets,
            },
            thumbnail_refresh_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.thumbnail_capture",
                tool: "thumbnail_capture",
                description: "Capture thumbnail from viewport (spec addendum § 8.8): render the active viewport pane's \
                    current view without overlays, crop it square and store it as the custom thumbnail of `path` \
                    (.regia/thumbs/custom; a refresh keeps it). Needs a GPU device.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative path under assets/ (any asset kind, levels and prefabs included).",
                }],
                result: "{ path, custom: true, file }",
                mutating: true,
                permission: Permission::Assets,
            },
            thumbnail_capture_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod custom_tests {
    use super::*;

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut image = image::RgbaImage::new(width, height);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgba(rgba);
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("png");
        bytes
    }

    fn project(tag: &str) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        std::fs::write(root.join("assets/a.png"), png(8, 8, [255, 0, 0, 255])).expect("png");
        std::fs::write(root.join("assets/hall.level.ron"), b"()").expect("level");
        root
    }

    fn center(cache: &ThumbCache, rel: &str) -> egui::Color32 {
        let image = cache.image(rel).expect("picture");
        image.pixels[image.width() * (image.height() / 2) + image.width() / 2]
    }

    #[test]
    fn crop_takes_the_centered_square_and_shrinks_it() {
        // 6×2: red, red, green, green, blue, blue columns; the centered 2×2 square is green.
        let mut image = egui::ColorImage::new([6, 2], vec![egui::Color32::RED; 12]);
        for y in 0..2 {
            for x in 0..6 {
                image.pixels[y * 6 + x] = match x {
                    0 | 1 => egui::Color32::RED,
                    2 | 3 => egui::Color32::GREEN,
                    _ => egui::Color32::BLUE,
                };
            }
        }
        let rgba = crop_square(&image, 4);
        assert_eq!(rgba.len(), 4 * 4 * 4);
        assert!(
            rgba.as_chunks::<4>().0.iter().all(|p| p == &[0, 255, 0, 255]),
            "{rgba:?}"
        );
    }

    #[test]
    fn refresh_remakes_the_automatic_picture_and_keeps_a_custom_one() {
        let root = project("thumb_refresh");
        let mut cache = ThumbCache::default();
        cache.set_root(Some(&root));
        cache.begin_frame();
        cache.request("assets/a.png");
        cache.finish();
        assert_eq!(cache.origin("assets/a.png"), Some(ThumbOrigin::Made));
        let key = cache.keys.get("assets/a.png").copied().flatten().expect("key");
        assert!(cache_file(&root, key).is_file());

        // Refresh: the disk copy goes, the next request makes the picture again.
        assert!(!cache.refresh("assets/a.png"), "no custom thumbnail yet");
        assert!(!cache_file(&root, key).exists());
        assert!(cache.image("assets/a.png").is_none());
        cache.begin_frame();
        cache.request("assets/a.png");
        cache.finish();
        assert_eq!(
            cache.origin("assets/a.png"),
            Some(ThumbOrigin::Made),
            "made again, not read from disk"
        );

        // A custom picture replaces it and survives a refresh, even of a kind with no automatic picture.
        let blue = egui::ColorImage::new([40, 20], vec![egui::Color32::BLUE; 800]);
        for rel in ["assets/a.png", "assets/hall.level.ron"] {
            store_custom(&root, rel, &blue, cache.px()).expect("store");
            cache.forget(&[PathBuf::from(rel)]);
        }
        assert_eq!(
            prune_disk(&cache_dir(&root), 0).0,
            1,
            "pruning removes the automatic copy only"
        );
        assert!(custom_file(&root, "assets/a.png").is_file());
        for rel in ["assets/a.png", "assets/hall.level.ron"] {
            assert!(cache.refresh(rel), "{rel}: the custom thumbnail stays");
            cache.begin_frame();
            assert_ne!(cache.request(rel), ThumbState::Unsupported, "{rel}");
            cache.finish();
            assert_eq!(center(&cache, rel), egui::Color32::BLUE, "{rel}");
        }
        assert!(clear_custom(&root, "assets/a.png").expect("clear"));
        cache.refresh("assets/a.png");
        cache.begin_frame();
        cache.request("assets/a.png");
        cache.finish();
        assert_eq!(
            center(&cache, "assets/a.png"),
            egui::Color32::RED,
            "back to the source picture"
        );
    }

    #[test]
    fn the_agent_methods_check_params_and_refresh() {
        let root = project("thumb_refresh_agent");
        let mut world = World::new();
        crate::agent_ext::install(&mut world);
        let extension = world.resource::<regia_agent::extension::AgentExtension>().clone();
        let (_, refresh) = extension.method("editor.thumbnail_refresh").expect("registered");
        let (_, capture) = extension.method("editor.thumbnail_capture").expect("registered");
        let code = |err: RpcError| err.data.and_then(|d| d.get("thumb_error").cloned());
        let err = refresh(&mut world, &json!({ "paths": ["assets/a.png"] })).expect_err("no project");
        assert_eq!(code(err), Some(json!("thumb_no_project")));
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        let err = refresh(&mut world, &json!({ "paths": [] })).expect_err("empty");
        assert_eq!(code(err), Some(json!("thumb_bad_params")));
        let err = refresh(&mut world, &json!({ "paths": ["assets/none.png"] })).expect_err("missing");
        assert_eq!(code(err), Some(json!("thumb_missing")));
        let err = capture(&mut world, &json!({ "path": "../x.png" })).expect_err("escape");
        assert_eq!(code(err), Some(json!("thumb_bad_path")));
        store_custom(
            &root,
            "assets/a.png",
            &egui::ColorImage::new([8, 8], vec![egui::Color32::BLUE; 64]),
            DEFAULT_THUMB_PX,
        )
        .expect("store");
        let reply = refresh(&mut world, &json!({ "paths": ["assets/a.png"] })).expect("refresh");
        assert_eq!(reply["refreshed"][0]["custom"], json!(true));
        let reply = refresh(
            &mut world,
            &json!({ "paths": ["assets/a.png"], "clear_custom": true }),
        )
        .expect("clear");
        assert_eq!(reply["refreshed"][0]["custom"], json!(false));
        assert!(!custom_file(&root, "assets/a.png").exists());
    }

    #[test]
    fn capture_stores_the_viewport_as_a_custom_thumbnail_when_a_device_exists() {
        let root = project("thumb_capture");
        let mut world = World::new();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        match capture_from_viewport(&mut world, "assets/hall.level.ron") {
            Ok(file) => {
                assert_eq!(file, custom_file(&root, "assets/hall.level.ron"));
                let decoded = image::open(&file).expect("png").to_rgba8();
                assert_eq!(decoded.dimensions(), (DEFAULT_THUMB_PX, DEFAULT_THUMB_PX));
                let mut cache = world.remove_resource::<ThumbCache>().expect("cache");
                cache.begin_frame();
                assert_ne!(cache.request("assets/hall.level.ron"), ThumbState::Unsupported);
                cache.finish();
                assert!(cache.image("assets/hall.level.ron").is_some());
            }
            Err(err) => {
                // No Vulkan adapter on this host: the error is coded, nothing is written.
                let code = err.data.as_ref().and_then(|d| d.get("thumb_error").cloned());
                assert_eq!(code, Some(json!("thumb_no_device")), "{err:?}");
                assert!(!custom_file(&root, "assets/hall.level.ron").exists());
            }
        }
    }
}
