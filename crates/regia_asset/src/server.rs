// SPDX-License-Identifier: MPL-2.0
//! Runtime asset server: load cooked blobs from the dev cache and/or pak files
//! (`docs/specs/12-assets.md` § 3, § 5).

use parking_lot::Mutex;
use std::cell::Cell;
use std::collections::HashMap;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;

use regia_app::prelude::*;
use tracing::{error, info, instrument, warn};

use crate::cooked::{CookedImage, CookedModel, CookedTexture, CpuMesh};
use crate::id::{AssetId, Handle};
use crate::live::{AssetReloaded, AssetWatchStatus, LoadedKind, SourceAssetChanged};
use crate::loader::{
    AssetLoadError, cooked_file_name, load_image_bytes, load_material_bytes, load_mesh_bytes,
    load_model_bytes,
};
use crate::manifest::{AssetManifest, cooked_cache_dir};
use crate::material::CookedStandardMaterial;
use crate::pak::{PakPayload, PakReader};
use crate::storage::{AssetEvent, Assets, LoadState};

// The generic asset API (spec 12 §§ 2-3, ADR-7202): loaders, refcounted handles, load groups.
mod asset_api;
// The built-in kinds (mesh, texture, image, material) on that API (ADR-7256).
mod asset_builtin;
pub use asset_api::{
    AssetLoader, DEFAULT_UNLOAD_GRACE_FRAMES, HandleReport, HandleRow, KindHandles, LoadContext,
    LoadGroupHandle, LoadGroupProgress, LoadSettings, PrepareAsset, StrongHandle, WeakAssetHandle,
};

thread_local! {
    static ASSET_PATH_SLASH_COPIES: Cell<u64> = const { Cell::new(0) };
}

fn note_asset_path_slash_copy() {
    ASSET_PATH_SLASH_COPIES.with(|n| n.set(n.get().saturating_add(1)));
}

/// Times [`AssetServer::id_for_path`] copied a path to turn `\` into `/` on this thread since
/// [`reset_asset_path_slash_copies`]. A path that already uses `/` does not count.
#[must_use]
pub fn asset_path_slash_copies() -> u64 {
    ASSET_PATH_SLASH_COPIES.with(Cell::get)
}

/// Zeroes [`asset_path_slash_copies`] on this thread.
pub fn reset_asset_path_slash_copies() {
    ASSET_PATH_SLASH_COPIES.with(|n| n.set(0));
}

/// Message to reload an asset in place (hot reload, `docs/specs/12-assets.md` § 8).
#[derive(Clone, Debug)]
pub enum ReloadMessage {
    /// Re-read cooked data for this id from disk.
    Reload(AssetId),
    /// A source file the runtime reads directly changed (absolute path). Becomes a
    /// [`crate::SourceAssetChanged`] message.
    Source(PathBuf),
    /// A finished live-cook step (cook time included), for [`crate::AssetWatchStatus`]. Sent by
    /// the live cooker after the `Reload` messages of that step.
    Status(crate::live::WatchEntry),
}

/// Configuration for [`AssetServer`].
#[derive(Clone, Debug)]
pub struct AssetServerSettings {
    /// Project root (contains `assets/` and `.regia/`).
    pub project_root: PathBuf,
    /// Platform tag (`linux`, `windows`, `android`, ...).
    pub platform: String,
    /// Optional directory holding `content/*.fpak` for a shipped package.
    /// When set, paks in this directory are mounted at startup.
    /// Defaults to the automatic detection of [`AssetServerSettings::default`]:
    /// `<exe dir>/content` on desktop when that directory exists (a packaged
    /// build), `None` in dev runs (reads from the cooked cache only). Android
    /// resolves its content directory through the APK assets (see the umbrella
    /// `DefaultPlugins`).
    pub content_dir: Option<PathBuf>,
}

/// The platform tag of the current compile target.
pub fn current_platform() -> String {
    match std::env::consts::OS {
        "linux" => "linux",
        "windows" => "windows",
        "android" => "android",
        "macos" => "macos",
        other => other,
    }
    .to_string()
}

/// Walk up from `start` looking for the nearest directory containing `Regia.toml` (or, for a
/// project from before the rename to Regia Engine, the legacy manifest: ADR-1429).
fn project_root_from(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if regia_core::brand::is_project_root(&dir) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Detect the project root for dev runs: `$REGIA_PROJECT`, else the nearest
/// `Regia.toml` above the working directory, else above the executable.
/// Falls back to the working directory (shipped games resolve assets from paks,
/// so a missing project root only disables the dev cooked cache).
pub fn detect_project_root() -> PathBuf {
    if let Some(root) = regia_core::brand::env_var_os("REGIA_PROJECT")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
    {
        return root;
    }
    std::env::current_dir()
        .ok()
        .and_then(|cwd| project_root_from(&cwd))
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| project_root_from(exe.parent()?))
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The shipped-package content directory candidate for an executable directory:
/// `<exe dir>/content` when it exists (desktop packages place paks there).
pub fn content_dir_at(exe_dir: &Path) -> Option<PathBuf> {
    let dir = exe_dir.join("content");
    dir.is_dir().then_some(dir)
}

/// Automatic content-dir detection (desktop): `<exe dir>/content` when present.
#[cfg(not(target_os = "android"))]
fn detect_content_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    content_dir_at(exe.parent()?)
}

impl Default for AssetServerSettings {
    fn default() -> Self {
        Self {
            project_root: detect_project_root(),
            platform: current_platform(),
            #[cfg(not(target_os = "android"))]
            content_dir: detect_content_dir(),
            // Android resolves paks from the APK assets (umbrella `DefaultPlugins`).
            #[cfg(target_os = "android")]
            content_dir: None,
        }
    }
}

/// A single mounted pak file.
struct MountedPak {
    /// The parsed pak reader, shared with every payload served from it ([`PakReader::read_shared`]):
    /// a loaded texture's bytes point into this mapping and keep it alive.
    reader: Arc<PakReader>,
    /// Name stem for diagnostics.
    name: String,
    /// Manifest mount position of the chunk (spec 40); `None` mounts after every listed chunk.
    order: Option<u32>,
    /// The pak's file name: TOC dependency entries of other paks match it (spec 12 § 5).
    file: String,
}

/// A listed chunk the startup mount refused, as each of its ids records it (ADR-8810, ADR-8860).
#[derive(Clone, Debug)]
struct RefusedChunk {
    /// Chunk name.
    chunk: String,
    /// Why it was refused (`chunk_size_mismatch`, `chunk_unreadable`).
    code: &'static str,
    /// Manifest mount position: a pak mounted after it holds a newer version and serves the id.
    order: u32,
}

/// Records `ids` as refused with `chunk`; an id two refused chunks list keeps the later one (its
/// newest version is the refused one).
fn refuse_ids(
    refused: &mut HashMap<AssetId, RefusedChunk>,
    ids: impl IntoIterator<Item = AssetId>,
    chunk: &crate::content::ChunkEntry,
    code: &'static str,
) {
    for id in ids {
        if refused.get(&id).is_some_and(|r| r.order > chunk.mount_order) {
            continue;
        }
        refused.insert(
            id,
            RefusedChunk {
                chunk: chunk.name.clone(),
                code,
                order: chunk.mount_order,
            },
        );
    }
}

/// The mount-order key of a pak: listed chunks by manifest position, then unlisted ones by name.
fn mount_key(order: Option<u32>, name: &str) -> (bool, u32, &str) {
    (order.is_none(), order.unwrap_or(0), name)
}

/// Where an asset's bytes came from (for end-to-end test evidence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetSource {
    /// Loaded from a pak file.
    Pak(String),
    /// Loaded from the dev cooked cache.
    DevCache,
    /// Not yet loaded.
    Unknown,
}

/// The source roots for `project_root`. A bad `[cook.overlay_roots]` table fails the cook; at
/// runtime it is a warning and only `assets/` is searched.
fn open_source_roots(project_root: &Path) -> crate::overlay_roots::SourceRoots {
    crate::overlay_roots::SourceRoots::for_project(project_root).unwrap_or_else(|error| {
        warn!(target: "regia_asset", %error, "overlay roots ignored; only assets/ is searched");
        crate::overlay_roots::SourceRoots::new(project_root.join("assets"), Vec::new())
    })
}

/// Loads and stores cooked assets for a running app.
#[derive(Resource)]
pub struct AssetServer {
    settings: AssetServerSettings,
    cache_dir: PathBuf,
    manifest: AssetManifest,
    meshes: Assets<CpuMesh>,
    images: Assets<CookedImage>,
    textures: Assets<CookedTexture>,
    materials: Assets<CookedStandardMaterial>,
    mesh_id_by_handle: HashMap<u32, AssetId>,
    material_id_by_handle: HashMap<u32, AssetId>,
    load_state: HashMap<AssetId, LoadState>,
    /// Where each loaded asset came from.
    asset_sources: HashMap<AssetId, AssetSource>,
    reload_rx: Option<Mutex<Receiver<ReloadMessage>>>,
    /// Mounted paks: index 0 is earliest, last is latest (overrides earlier).
    mounted_paks: Vec<MountedPak>,
    /// Listed chunks the startup mount refused, by name, with their code (ADR-8810).
    refused_chunks: std::collections::BTreeMap<String, &'static str>,
    /// Ids the refused chunks list (their tables of contents when they read, and the chunk map),
    /// with the chunk: they fail to load (`chunk_refused`) instead of being served by an earlier
    /// pak; a pak mounted after the chunk still serves them (ADR-8810, ADR-8860). Shared with
    /// every [`PayloadReader`].
    refused_ids: Arc<HashMap<AssetId, RefusedChunk>>,
    /// Hot-reload count per id ([`Self::generation`]).
    generations: HashMap<AssetId, u64>,
    /// Ids whose payload another crate decodes ([`Self::track_external`]).
    external: std::collections::HashSet<AssetId>,
    /// The forms loaded so far ([`Self::load_forms`]), shared with every [`PayloadReader`].
    forms: Arc<crate::cooked_pack::LoadFormCounters>,
    /// Which chunk each cooked asset lives in (spec 40), for [`Self::unavailable_chunk`].
    chunk_map: Option<crate::content::ChunkMap>,
    /// Chunks already warned about since the last level load.
    warned_unavailable: std::collections::BTreeSet<String>,
    /// The fallback chains localized assets resolve through ([`Self::set_locale_chains`]).
    locale_chains: crate::localized::LocaleChains,
    /// Every base path resolved through [`Self::resolve_localized`]: its group and current path.
    localized: std::collections::BTreeMap<String, (crate::localized::LocaleGroup, String)>,
    /// Calls of [`Self::id_for_path`] on this server.
    path_lookups: AtomicU64,
    /// How many times the server has replaced its manifest entries ([`Self::manifest_epoch`]).
    manifest_epoch: u64,
    /// Loaders for content kinds a crate above this one owns ([`Self::set_content_hook`]).
    pub(crate) content_hook: Option<Arc<dyn crate::content_hook::ContentLoaderHook>>,
    /// Loaders, stores, handles and groups of the generic API (`asset_api`).
    generic: asset_api::GenericAssets,
    /// Ids pushed into this game (`assets.push`, ADR-7836): read from the cache before any pak.
    pushed: Arc<crate::push::PushedIds>,
    /// Per-asset load times ([`Self::load_times`], P-314).
    pub(crate) load_times: crate::load_times::LoadTimes,
    /// `assets/` plus the project's overlay roots, for sources read directly
    /// ([`Self::source_path`], ADR-8845).
    source_roots: crate::overlay_roots::SourceRoots,
}

impl AssetServer {
    /// Open the dev cache for `settings`. Returns an error when the cache is missing.
    ///
    /// If `settings.content_dir` is set and contains `*.fpak` files, they are mounted
    /// in filename order (later paks override earlier when the same `AssetId` appears
    /// in more than one).
    pub fn open(settings: AssetServerSettings) -> Result<Self, AssetLoadError> {
        let cache_dir = cooked_cache_dir(&settings.project_root, &settings.platform);
        let source_roots = open_source_roots(&settings.project_root);
        let manifest = AssetManifest::read_from_cache(&cache_dir)
            .map_err(|e| {
                AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(std::io::Error::other(
                    e.to_string(),
                )))
            })?
            .unwrap_or_default();

        let mut server = Self {
            settings,
            cache_dir,
            manifest,
            meshes: Assets::new(),
            images: Assets::new(),
            textures: Assets::new(),
            materials: Assets::new(),
            mesh_id_by_handle: HashMap::new(),
            material_id_by_handle: HashMap::new(),
            load_state: HashMap::new(),
            asset_sources: HashMap::new(),
            reload_rx: None,
            mounted_paks: Vec::new(),
            refused_chunks: std::collections::BTreeMap::new(),
            refused_ids: Arc::default(),
            generations: HashMap::new(),
            external: std::collections::HashSet::new(),
            forms: Arc::default(),
            chunk_map: None,
            warned_unavailable: std::collections::BTreeSet::new(),
            locale_chains: crate::localized::LocaleChains::default(),
            localized: std::collections::BTreeMap::new(),
            path_lookups: AtomicU64::new(0),
            manifest_epoch: 0,
            content_hook: None,
            generic: asset_api::GenericAssets::default(),
            pushed: crate::push::PushedIds::shared(),
            load_times: crate::load_times::LoadTimes::default(),
            source_roots,
        };

        // Counted loads of the built-in kinds (`load::<CpuMesh>`, ...), ADR-7256.
        asset_builtin::register_builtin_loaders(&mut server);

        // Mount content paks if a content_dir is configured.
        // A manifest-listed pak that fails verification refuses the open (ADR-7979): booting on a
        // partial mount set would hide a damaged install behind exit code 0.
        server.mount_content_paks()?;
        server.reindex_manifest();

        Ok(server)
    }

    /// Scan `content_dir` for `*.fpak` files and mount them. A pak the content manifest lists
    /// that is not the manifest's length or does not open is refused and not mounted (ADR-8810,
    /// ADR-8860); one of the right length whose BLAKE3 differs is an error, not a skipped mount
    /// (ADR-7979, ADR-8714).
    fn mount_content_paks(&mut self) -> Result<(), crate::content::ContentError> {
        let Some(content_dir) = &self.settings.content_dir.clone() else {
            return Ok(());
        };
        let Ok(dir) = std::fs::read_dir(content_dir) else {
            warn!(target: "regia_asset", dir=%content_dir.display(), "cannot read content directory");
            return Ok(());
        };

        let mut pak_paths: Vec<PathBuf> = dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "fpak"))
            .map(|e| e.path())
            .collect();
        sort_pak_mount_order(&mut pak_paths);
        // With a content manifest (spec 40), listed chunks mount in manifest order before the
        // unlisted paks, which keep the file-name order. DLC entries (`<chunk>.chunk.ron`) count as
        // listed (audit v0.2.12: they were never read, so a DLC pak mounted as an unlisted one).
        let manifest = match crate::content::ContentManifest::read_from_dir_with_dlc(content_dir) {
            Ok(m) => m,
            Err(e) => {
                warn!(target: "regia_asset", dir=%content_dir.display(), "content manifest ignored: {e}");
                None
            }
        };
        let order_of = |path: &Path| -> Option<u32> {
            let file = path.file_name()?.to_str()?;
            manifest
                .as_ref()?
                .chunks
                .iter()
                .find(|c| c.file == file)
                .map(|c| c.mount_order)
        };
        pak_paths.sort_by_key(|p| order_of(p).map_or((1, 0), |o| (0, o)));
        // A DLC cooked against a base this build does not accept is not mounted (spec 40 § 2.4:
        // `chunk_base_mismatch`; the content manager reports it).
        if let Some(manifest) = &manifest {
            pak_paths.retain(|path| {
                let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
                let refused = manifest.chunks.iter().find(|c| c.file == file).is_some_and(|c| {
                    c.base_build().is_some_and(|base| !manifest.accepts_base(base))
                });
                if refused {
                    warn!(target: "regia_asset", pak=%path.display(), running=%manifest.build_id, "DLC built against another base; not mounted (chunk_base_mismatch)");
                }
                !refused
            });
        }
        match &manifest {
            Some(manifest) => {
                if let Ok(map) = crate::content::ChunkMap::read_from_dir_with_dlc(content_dir, manifest) {
                    self.chunk_map = Some(map);
                }
            }
            None => {
                if let Ok(Some(map)) = crate::content::ChunkMap::read_from_dir(content_dir) {
                    self.chunk_map = Some(map);
                }
            }
        }

        let verify = crate::pak_verify::PakVerifyOptions::from_env();
        // Open every pak first: only the header and the TOC are read here (asset bytes page in on
        // first access), and the TOC dependency lists decide the final mount order (spec 12 § 5).
        // Manifest-listed paks are then checked in two steps (ADR-8714): a wrong length or a pak
        // that does not open is a torn install, refused below (ADR-8810, ADR-8860); a listed pak of
        // the right length is hashed in full and a changed one fails the open (ADR-7871, ADR-7979).
        let mut opened: Vec<(PathBuf, PakReader)> = Vec::with_capacity(pak_paths.len());
        let mut unopened: Vec<PathBuf> = Vec::new();
        for path in &pak_paths {
            match PakReader::open(path) {
                Ok(reader) => opened.push((path.clone(), reader)),
                Err(e) => {
                    warn!(
                        target: "regia_asset",
                        pak=%path.display(),
                        error=%e,
                        "failed to open pak"
                    );
                    unopened.push(path.clone());
                }
            }
        }
        // ADR-8810: a listed chunk whose pak is not the manifest's length is not mounted (a torn
        // or half-written install). The length is the open reader's (no extra file access); the
        // ids its table of contents lists fail to load rather than fall back to an earlier pak.
        if let Some(manifest) = &manifest {
            let mut refused_ids: HashMap<AssetId, RefusedChunk> = HashMap::new();
            let chunk_map = self.chunk_map.as_ref();
            let map_ids = |name: &str| chunk_map.map(|m| m.ids_in(name)).unwrap_or_default();
            // ADR-8860: a listed pak that does not open at all (the commonest torn copy, a
            // truncated file, fails at its table of contents) is refused too: the ids the chunk
            // map homes in it fail rather than fall back to an earlier pak. One stat names why.
            for path in &unopened {
                let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
                let Some(chunk) = manifest.chunks.iter().find(|c| c.file == file) else {
                    continue;
                };
                let found = std::fs::metadata(path).ok().map(|m| m.len());
                let code = if found == Some(chunk.size) {
                    "chunk_unreadable"
                } else {
                    "chunk_size_mismatch"
                };
                warn!(
                    target: "regia_asset",
                    pak=%path.display(),
                    chunk=%chunk.name,
                    expected=chunk.size,
                    found=?found,
                    code,
                    "listed pak does not open; not mounted, its ids are refused"
                );
                self.refused_chunks.insert(chunk.name.clone(), code);
                refuse_ids(&mut refused_ids, map_ids(&chunk.name), chunk, code);
            }
            opened.retain(|(path, reader)| {
                let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
                let Some(chunk) = manifest.chunks.iter().find(|c| c.file == file) else {
                    return true;
                };
                if reader.len_bytes() == chunk.size {
                    return true;
                }
                warn!(
                    target: "regia_asset",
                    pak=%path.display(),
                    chunk=%chunk.name,
                    expected=chunk.size,
                    found=reader.len_bytes(),
                    "pak length differs from the content manifest; not mounted (chunk_size_mismatch)"
                );
                self.refused_chunks
                    .insert(chunk.name.clone(), "chunk_size_mismatch");
                let toc_ids = reader.toc.entries.iter().map(|e| e.id);
                refuse_ids(
                    &mut refused_ids,
                    toc_ids.chain(map_ids(&chunk.name)),
                    chunk,
                    "chunk_size_mismatch",
                );
                false
            });
            self.refused_ids = Arc::new(refused_ids);
            // ADR-7979 after ADR-8810 (ADR-8714): what is left of the listed paks has the
            // manifest's length and opens, so a mismatch here is changed content, not a torn copy.
            // Booting on a partial mount set would hide it behind exit code 0: the open fails.
            for (path, _) in &opened {
                let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
                if let Some(entry) = manifest.chunks.iter().find(|c| c.file == file)
                    && let Err(error) = entry.verify_installed_pak(path)
                {
                    tracing::error!(target: "regia_asset", pak=%path.display(), reason=error.reason(), %error,
                        "installed chunk failed manifest verification; the content is refused");
                    return Err(error);
                }
            }
        }
        order_paks_by_toc_dependencies(&mut opened);

        let mut mounted = Vec::with_capacity(opened.len());
        for (path, reader) in opened {
            mounted.push((
                path.clone(),
                zero_copy_gate_at_mount(&reader, &path, verify.as_ref()),
            ));
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            info!(
                target: "regia_asset",
                pak=%path.display(),
                entries=reader.toc.entries.len(),
                "mounted"
            );

            // Merge the pak's manifest into the server's manifest.
            // Later paks override earlier manifest entries for the same path.
            self.manifest.note_entries_changed();
            merge_manifest(&mut self.manifest.entries, &reader.toc.manifest);

            self.mounted_paks.push(MountedPak {
                reader: Arc::new(reader),
                name,
                order: order_of(&path),
                file: pak_file_name(&path),
            });
        }

        // Whole-file integrity check in a detached low-priority thread, only on the first launch
        // after a pak or the build changed; loading never waits for it (`pak_verify`). It opens
        // the zero-copy gate of each pak it verifies.
        if let Some(options) = verify {
            let _detached = crate::pak_verify::spawn_pak_verify_gated(mounted, options);
        }
        Ok(())
    }

    /// Attach a hot-reload channel (dev builds).
    pub fn set_reload_receiver(&mut self, rx: Receiver<ReloadMessage>) {
        self.reload_rx = Some(Mutex::new(rx));
    }

    /// Current load state for an id (mesh or image attempt).
    pub fn state(&self, id: AssetId) -> LoadState {
        self.load_state.get(&id).cloned().unwrap_or(LoadState::NotLoaded)
    }

    /// Load state for a mesh handle.
    pub fn mesh_state(&self, handle: &Handle<CpuMesh>) -> LoadState {
        self.mesh_id_by_handle
            .get(&handle.index)
            .map(|id| self.state(*id))
            .unwrap_or(LoadState::NotLoaded)
    }

    /// Lookup id for a project-relative path (requires manifest).
    ///
    /// The first entry of a duplicated path wins, the same rule as
    /// [`AssetManifest::id_for_path`]. A path that already uses `/` is borrowed. A path with `\`
    /// is copied with those separators rewritten to `/` ([`asset_path_slash_copies`]).
    pub fn id_for_path(&self, path: &str) -> Option<AssetId> {
        self.path_lookups.fetch_add(1, Ordering::Relaxed);
        let result = if path.contains('\\') {
            note_asset_path_slash_copy();
            let norm = path.replace('\\', "/");
            self.manifest.id_for_path(&norm)
        } else {
            self.manifest.id_for_path(path)
        };
        crate::access_trace::note_resolve(path, result.is_some());
        result
    }

    /// How many times [`Self::id_for_path`] has run on this server.
    #[must_use]
    pub fn path_lookup_count(&self) -> u64 {
        self.path_lookups.load(Ordering::Relaxed)
    }

    /// How many times the server has replaced its manifest entries.
    ///
    /// A caller that cached a path lookup repeats it when this changes.
    #[must_use]
    pub fn manifest_epoch(&self) -> u64 {
        self.manifest_epoch
    }

    /// Marks the manifest entries as replaced: the manifest's own path index is rebuilt on the
    /// next lookup, and cached path lookups see a new epoch.
    fn reindex_manifest(&mut self) {
        self.manifest.note_entries_changed();
        self.manifest_epoch = self.manifest_epoch.wrapping_add(1);
    }

    /// Convenience: load the mesh for a path under `assets/`.
    pub fn load_mesh_path(&mut self, path: &str) -> Result<Handle<CpuMesh>, AssetLoadError> {
        let id = self.id_for_path(path).ok_or_else(|| {
            AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no manifest entry for {path}"),
            )))
        })?;
        self.load_mesh(id)
    }

    /// Load a mesh by id: tries mounted paks first, then the cooked cache.
    pub fn load_mesh(&mut self, id: AssetId) -> Result<Handle<CpuMesh>, AssetLoadError> {
        let prior = self.load_state.get(&id).cloned();
        let started = crate::load_times::begin();
        let result = self.load_mesh_inner(id);
        self.load_times
            .record(crate::load_times::finish(started, id, "mesh", result.is_ok()));
        self.keep_loaded_on_error(id, prior, result.is_ok());
        if result.is_ok() {
            // The uncounted handle keeps it: counted handles never unload it (ADR-7256).
            self.pin_builtin(id, std::any::TypeId::of::<CpuMesh>());
        }
        if let Err(e) = &result {
            self.meshes.push_event(AssetEvent::LoadFailed {
                id,
                error: e.to_string(),
            });
        }
        crate::load_stats::note_load(result.is_ok());
        result
    }

    /// Body of [`Self::load_mesh`].
    fn load_mesh_inner(&mut self, id: AssetId) -> Result<Handle<CpuMesh>, AssetLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "mesh").entered();
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Assets);
        let _slow = regia_core::telemetry::SlowLoad::start("mesh", &id);
        self.load_state.insert(id, LoadState::Loading);

        // Try paks first.
        if let Some((bytes, pak_name)) = self.read_from_paks(id)? {
            match load_mesh_bytes(&bytes) {
                Ok(loaded) => {
                    self.note_mesh_form(&loaded.header, true);
                    let handle = self.meshes.insert(id, loaded.mesh);
                    self.mesh_id_by_handle.insert(handle.index, id);
                    self.load_state.insert(id, LoadState::Loaded);
                    self.asset_sources.insert(id, AssetSource::Pak(pak_name));
                    return Ok(handle);
                }
                Err(e) => {
                    warn!(target: "regia_asset", id=%id, error=%e, "pak mesh load failed, falling back to dev cache");
                }
            }
        }

        // Fall back to cooked cache.
        if let Some(e) = self.unavailable_error(id) {
            return Err(e);
        }
        let path = self.cache_dir.join(cooked_file_name(id));
        let bytes =
            crate::load_times::timed_read("cache", || crate::load_stats::read_file(&path)).map_err(|e| {
                self.load_state.insert(id, LoadState::Failed(e.to_string()));
                AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(e))
            })?;
        let loaded = load_mesh_bytes(&bytes).inspect_err(|e| {
            self.load_state.insert(id, LoadState::Failed(e.to_string()));
        })?;
        self.note_mesh_form(&loaded.header, false);
        let handle = self.meshes.insert(id, loaded.mesh);
        self.mesh_id_by_handle.insert(handle.index, id);
        self.load_state.insert(id, LoadState::Loaded);
        self.asset_sources.insert(id, AssetSource::DevCache);
        Ok(handle)
    }

    /// Load a compressed texture by id: tries paks first, then cooked cache.
    pub fn load_texture(&mut self, id: AssetId) -> Result<Handle<CookedTexture>, AssetLoadError> {
        let prior = self.load_state.get(&id).cloned();
        let started = crate::load_times::begin();
        let result = self.load_texture_inner(id);
        self.load_times
            .record(crate::load_times::finish(started, id, "texture", result.is_ok()));
        self.keep_loaded_on_error(id, prior, result.is_ok());
        if result.is_ok() {
            // The uncounted handle keeps it: counted handles never unload it (ADR-7256).
            self.pin_builtin(id, std::any::TypeId::of::<CookedTexture>());
        }
        if let Err(e) = &result {
            self.textures.push_event(AssetEvent::LoadFailed {
                id,
                error: e.to_string(),
            });
        }
        crate::load_stats::note_load(result.is_ok());
        result
    }

    /// Body of [`Self::load_texture`].
    fn load_texture_inner(&mut self, id: AssetId) -> Result<Handle<CookedTexture>, AssetLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "texture").entered();
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Assets);
        let _slow = regia_core::telemetry::SlowLoad::start("texture", &id);
        self.load_state.insert(id, LoadState::Loading);

        // Try paks first. The texture's KTX2 bytes stay a view of the pak entry (no copy).
        if let Some((bytes, pak_name)) = self.read_from_paks(id)? {
            match crate::loader::load_texture_shared(&bytes) {
                Ok((_header, tex)) => {
                    self.forms.note_texture();
                    self.forms.note_source(true);
                    let handle = self.textures.insert(id, tex);
                    self.load_state.insert(id, LoadState::Loaded);
                    self.asset_sources.insert(id, AssetSource::Pak(pak_name));
                    return Ok(handle);
                }
                Err(e) => {
                    warn!(target: "regia_asset", id=%id, error=%e, "pak texture load failed, falling back to dev cache");
                }
            }
        }

        // Fall back to cooked cache.
        if let Some(e) = self.unavailable_error(id) {
            return Err(e);
        }
        let path = self.cache_dir.join(cooked_file_name(id));
        let bytes =
            crate::load_times::timed_read("cache", || crate::load_stats::read_file(&path)).map_err(|e| {
                self.load_state.insert(id, LoadState::Failed(e.to_string()));
                AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(e))
            })?;
        let bytes = PakPayload::from(bytes);
        let (_header, tex) = crate::loader::load_texture_shared(&bytes).inspect_err(|e| {
            self.load_state.insert(id, LoadState::Failed(e.to_string()));
        })?;
        self.forms.note_texture();
        self.forms.note_source(false);
        let handle = self.textures.insert(id, tex);
        self.load_state.insert(id, LoadState::Loaded);
        self.asset_sources.insert(id, AssetSource::DevCache);
        Ok(handle)
    }

    /// Load an image by id: tries paks first, then cooked cache.
    pub fn load_image(&mut self, id: AssetId) -> Result<Handle<CookedImage>, AssetLoadError> {
        let prior = self.load_state.get(&id).cloned();
        let started = crate::load_times::begin();
        let result = self.load_image_inner(id);
        self.load_times
            .record(crate::load_times::finish(started, id, "image", result.is_ok()));
        self.keep_loaded_on_error(id, prior, result.is_ok());
        if result.is_ok() {
            // The uncounted handle keeps it: counted handles never unload it (ADR-7256).
            self.pin_builtin(id, std::any::TypeId::of::<CookedImage>());
        }
        if let Err(e) = &result {
            self.images.push_event(AssetEvent::LoadFailed {
                id,
                error: e.to_string(),
            });
        }
        crate::load_stats::note_load(result.is_ok());
        result
    }

    /// Body of [`Self::load_image`].
    fn load_image_inner(&mut self, id: AssetId) -> Result<Handle<CookedImage>, AssetLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "image").entered();
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Assets);
        let _slow = regia_core::telemetry::SlowLoad::start("image", &id);
        self.load_state.insert(id, LoadState::Loading);

        // Try paks first.
        if let Some((bytes, pak_name)) = self.read_from_paks(id)? {
            match load_image_bytes(&bytes) {
                Ok((_header, image)) => {
                    self.forms.note_image();
                    self.forms.note_source(true);
                    let handle = self.images.insert(id, image);
                    self.load_state.insert(id, LoadState::Loaded);
                    self.asset_sources.insert(id, AssetSource::Pak(pak_name));
                    return Ok(handle);
                }
                Err(e) => {
                    warn!(target: "regia_asset", id=%id, error=%e, "pak image load failed, falling back to dev cache");
                }
            }
        }

        // Fall back to cooked cache.
        if let Some(e) = self.unavailable_error(id) {
            return Err(e);
        }
        let path = self.cache_dir.join(cooked_file_name(id));
        let bytes =
            crate::load_times::timed_read("cache", || crate::load_stats::read_file(&path)).map_err(|e| {
                self.load_state.insert(id, LoadState::Failed(e.to_string()));
                AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(e))
            })?;
        let (_header, image) = load_image_bytes(&bytes).inspect_err(|e| {
            self.load_state.insert(id, LoadState::Failed(e.to_string()));
        })?;
        self.forms.note_image();
        self.forms.note_source(false);
        let handle = self.images.insert(id, image);
        self.load_state.insert(id, LoadState::Loaded);
        self.asset_sources.insert(id, AssetSource::DevCache);
        Ok(handle)
    }

    /// Load a cooked standard material by project-relative path.
    pub fn load_material_path(
        &mut self,
        path: &str,
    ) -> Result<Handle<CookedStandardMaterial>, AssetLoadError> {
        let id = self.id_for_path(path).ok_or_else(|| {
            AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no manifest entry for {path}"),
            )))
        })?;
        self.load_material(id)
    }

    /// Load a cooked standard material by id: tries mounted paks first, then the cooked cache.
    ///
    /// A stale cooked payload ([`crate::MaterialLoadError::UnsupportedVersion`]) is an error
    /// here. The cooker rebuilds that cache; the runtime does not.
    pub fn load_material(&mut self, id: AssetId) -> Result<Handle<CookedStandardMaterial>, AssetLoadError> {
        let prior = self.load_state.get(&id).cloned();
        let started = crate::load_times::begin();
        let result = self.load_material_inner(id);
        self.load_times
            .record(crate::load_times::finish(started, id, "material", result.is_ok()));
        self.keep_loaded_on_error(id, prior, result.is_ok());
        if result.is_ok() {
            // The uncounted handle keeps it: counted handles never unload it (ADR-7256).
            self.pin_builtin(id, std::any::TypeId::of::<CookedStandardMaterial>());
        }
        if let Err(e) = &result {
            self.materials.push_event(AssetEvent::LoadFailed {
                id,
                error: e.to_string(),
            });
        }
        crate::load_stats::note_load(result.is_ok());
        result
    }

    /// Body of [`Self::load_material`].
    fn load_material_inner(&mut self, id: AssetId) -> Result<Handle<CookedStandardMaterial>, AssetLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "material").entered();
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Assets);
        let _slow = regia_core::telemetry::SlowLoad::start("material", &id);
        self.load_state.insert(id, LoadState::Loading);

        if let Some((bytes, pak_name)) = self.read_from_paks(id)? {
            match load_material_bytes(&bytes) {
                Ok((_header, material)) => {
                    let handle = self.materials.insert(id, material);
                    self.material_id_by_handle.insert(handle.index, id);
                    self.load_state.insert(id, LoadState::Loaded);
                    self.asset_sources.insert(id, AssetSource::Pak(pak_name));
                    return Ok(handle);
                }
                Err(e) => {
                    warn!(
                        target: "regia_asset",
                        id=%id,
                        error=%e,
                        "pak material load failed, falling back to dev cache"
                    );
                }
            }
        }

        if let Some(e) = self.unavailable_error(id) {
            return Err(e);
        }
        let path = self.cache_dir.join(cooked_file_name(id));
        let bytes =
            crate::load_times::timed_read("cache", || crate::load_stats::read_file(&path)).map_err(|e| {
                self.load_state.insert(id, LoadState::Failed(e.to_string()));
                AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(e))
            })?;
        let (_header, material) = load_material_bytes(&bytes).inspect_err(|e| {
            self.load_state.insert(id, LoadState::Failed(e.to_string()));
        })?;
        let handle = self.materials.insert(id, material);
        self.material_id_by_handle.insert(handle.index, id);
        self.load_state.insert(id, LoadState::Loaded);
        self.asset_sources.insert(id, AssetSource::DevCache);
        Ok(handle)
    }

    /// Load an imported model's part list (its `model` sub-asset) by id: mounted paks first,
    /// then the cooked cache. Not stored: the caller loads the parts' meshes and materials.
    pub fn load_model(&mut self, id: AssetId) -> Result<CookedModel, AssetLoadError> {
        let prior = self.load_state.get(&id).cloned();
        let started = crate::load_times::begin();
        let result = self.load_model_inner(id);
        self.load_times
            .record(crate::load_times::finish(started, id, "model", result.is_ok()));
        self.keep_loaded_on_error(id, prior, result.is_ok());
        crate::load_stats::note_load(result.is_ok());
        result
    }

    /// Body of [`Self::load_model`].
    fn load_model_inner(&mut self, id: AssetId) -> Result<CookedModel, AssetLoadError> {
        let _span = tracing::info_span!("asset_load", kind = "model").entered();
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Assets);
        let _slow = regia_core::telemetry::SlowLoad::start("model", &id);
        if let Some((bytes, pak_name)) = self.read_from_paks(id)? {
            match load_model_bytes(&bytes) {
                Ok((_header, model)) => {
                    self.load_state.insert(id, LoadState::Loaded);
                    self.asset_sources.insert(id, AssetSource::Pak(pak_name));
                    return Ok(model);
                }
                Err(e) => {
                    warn!(target: "regia_asset", id=%id, error=%e, "pak model load failed, falling back to dev cache");
                }
            }
        }
        if let Some(e) = self.unavailable_error(id) {
            return Err(e);
        }
        let path = self.cache_dir.join(cooked_file_name(id));
        let bytes =
            crate::load_times::timed_read("cache", || crate::load_stats::read_file(&path)).map_err(|e| {
                self.load_state.insert(id, LoadState::Failed(e.to_string()));
                AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(e))
            })?;
        let (_header, model) = load_model_bytes(&bytes).inspect_err(|e| {
            self.load_state.insert(id, LoadState::Failed(e.to_string()));
        })?;
        self.load_state.insert(id, LoadState::Loaded);
        self.asset_sources.insert(id, AssetSource::DevCache);
        Ok(model)
    }

    /// Read raw bytes for an asset id from mounted paks.
    ///
    /// Searches in reverse mount order (later paks override earlier).
    /// Returns `(bytes, pak_name)` on success: an entry stored uncompressed is a view of the
    /// mapped pak (no copy), a compressed one its decompressed bytes.
    ///
    /// # Errors
    /// The id is listed by a chunk the startup mount refused (`chunk_refused`), or the newest pak
    /// that holds it fails to read its entry (`pak_entry_corrupt`): never served by an earlier pak
    /// or the cache instead (ADR-8810). The load state records the code.
    fn read_from_paks(&mut self, id: AssetId) -> Result<Option<(PakPayload, String)>, AssetLoadError> {
        // A pushed id is served from the cooked cache, over every pak (ADR-7836).
        if self.pushed.contains(id) {
            return Ok(None);
        }
        let started = std::time::Instant::now();
        let read = read_checked(
            &self.refused_ids,
            self.mounted_paks
                .iter()
                .map(|m| (&m.reader, m.name.as_str(), m.order)),
            id,
        );
        if let Ok(Some((bytes, _))) = &read {
            crate::load_times::note_read(crate::load_times::since(started), bytes.len() as u64, "pak");
        }
        if let Err(error) = &read {
            self.load_state.insert(id, LoadState::Failed(error.to_string()));
        }
        read
    }

    /// Listed chunks the startup mount refused, with their code, by chunk name:
    /// `chunk_size_mismatch` (the pak's length is not the content manifest's, ADR-8810; a
    /// truncated pak that does not open counts, ADR-8860) or `chunk_unreadable` (the manifest's
    /// length, but the pak does not open).
    #[must_use]
    pub fn refused_chunks(&self) -> &std::collections::BTreeMap<String, &'static str> {
        &self.refused_chunks
    }

    /// Per-asset load times: one row per asset, its newest load's read, decode and upload time,
    /// bytes and source (P-314; the editor Profiler's Loads tab).
    #[must_use]
    pub fn load_times(&self) -> &crate::load_times::LoadTimes {
        &self.load_times
    }

    /// A thread-safe reader of this server's mounted paks and cooked cache, for decoding assets
    /// on worker threads (`regia_render::cooked_model` decodes meshes in parallel with it).
    /// Mark what it loaded with [`Self::note_external_load`].
    #[must_use]
    pub fn payload_reader(&self) -> PayloadReader {
        PayloadReader {
            paks: self
                .mounted_paks
                .iter()
                .map(|m| (Arc::clone(&m.reader), m.name.clone(), m.order))
                .collect(),
            cache_dir: self.cache_dir.clone(),
            forms: Arc::clone(&self.forms),
            refused_ids: Arc::clone(&self.refused_ids),
            pushed: Arc::clone(&self.pushed),
        }
    }

    /// Accepts a pushed cooked asset (`assets.push`, ADR-7836): writes `bytes` as `id`'s file in
    /// the cooked cache (atomically; the path is the id's file name, never the caller's) and from
    /// then on serves `id` from the cache before any mounted pak. Call [`Self::reload_id`] (or
    /// send [`ReloadMessage::Reload`]) after it to swap the loaded value.
    ///
    /// # Errors
    ///
    /// [`crate::push::PushError::TooLarge`] over [`crate::push::PUSH_MAX_BYTES`];
    /// [`crate::push::PushError::Io`] when the write failed (nothing changes then).
    pub fn accept_push(
        &mut self,
        id: AssetId,
        bytes: &[u8],
    ) -> Result<crate::push::PushOutcome, crate::push::PushError> {
        let outcome = crate::push::write_pushed(&self.cache_dir.join(cooked_file_name(id)), bytes)?;
        self.pushed.insert(id);
        Ok(outcome)
    }

    /// Accepts a pushed source file the runtime reads directly (a UI document): writes `bytes` to
    /// `<project>/assets/<…>` at the relative path `rel` (see [`crate::push::confine_source_path`]).
    /// Send [`ReloadMessage::Source`] with the returned path after it.
    ///
    /// # Errors
    ///
    /// [`crate::push::PushError::BadPath`] when `rel` leaves `assets/`; the others as
    /// [`Self::accept_push`].
    pub fn accept_source_push(
        &self,
        rel: &str,
        bytes: &[u8],
    ) -> Result<crate::push::PushOutcome, crate::push::PushError> {
        let path = crate::push::confine_source_path(&self.settings.project_root, rel)?;
        crate::push::write_pushed(&path, bytes)
    }

    /// Whether `id` was pushed into this game ([`Self::accept_push`]).
    #[must_use]
    pub fn is_pushed(&self, id: AssetId) -> bool {
        self.pushed.contains(id)
    }

    /// How many distinct ids were pushed into this game.
    #[must_use]
    pub fn pushed_count(&self) -> usize {
        self.pushed.len()
    }

    /// Counts a loaded mesh's form and source ([`Self::load_forms`]).
    fn note_mesh_form(&self, header: &crate::cooked_header::CookedHeader, from_pak: bool) {
        if let Some(form) = crate::cooked_pack::MeshForm::of(header.type_tag) {
            self.forms.note_mesh(form);
        }
        self.forms.note_source(from_pak);
    }

    /// The forms this server (and its [`PayloadReader`]s) loaded so far: GPU-ready or compact
    /// meshes, KTX2 textures, RGBA8 images, pak or dev-cache reads (ADR-1594). A shipped game on
    /// cooked packs loads no compact mesh on desktop and nothing from the dev cache.
    #[must_use]
    pub fn load_forms(&self) -> crate::cooked_pack::LoadForms {
        self.forms.snapshot()
    }

    /// The platform tag this server loads for.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.settings.platform
    }

    /// A [`crate::cooked_pack::PakSummary`] of every mounted pak, in mount order. With
    /// `inspect_textures`, each texture entry's KTX2 header is read and checked against the
    /// server's platform (reads every texture entry; an agent or a startup check, not a frame).
    #[must_use]
    pub fn mounted_pak_summaries(&self, inspect_textures: bool) -> Vec<crate::cooked_pack::PakSummary> {
        self.mounted_paks
            .iter()
            .map(|m| {
                crate::cooked_pack::summarize_pak(
                    &m.reader,
                    &m.name,
                    &self.settings.platform,
                    inspect_textures,
                )
            })
            .collect()
    }

    /// Records that `id` was loaded from `source` by a decoder outside this server (a
    /// [`PayloadReader`] on a worker thread): its state becomes [`LoadState::Loaded`], its
    /// source is reported by [`Self::source_of`], and a hot reload of it bumps
    /// [`Self::generation`] ([`Self::track_external`]).
    pub fn note_external_load(&mut self, id: AssetId, source: AssetSource) {
        self.load_state.insert(id, LoadState::Loaded);
        self.asset_sources.insert(id, source);
        self.external.insert(id);
    }

    /// Where the bytes for `id` were loaded from.
    pub fn source_of(&self, id: AssetId) -> AssetSource {
        self.asset_sources
            .get(&id)
            .cloned()
            .unwrap_or(AssetSource::Unknown)
    }

    /// The configured content directory (pak mount root), if any.
    pub fn settings_content_dir(&self) -> Option<&Path> {
        self.settings.content_dir.as_deref()
    }

    /// Load the mesh at a project-relative path and report where its bytes came
    /// from. Used by the `REGIA_PROBE_ASSET` startup probe of [`AssetPlugin`].
    pub fn probe(&mut self, path: &str) -> Result<AssetSource, AssetLoadError> {
        let _handle = self.load_mesh_path(path)?;
        let id = self.id_for_path(path).ok_or_else(|| {
            AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no manifest entry for {path} after loading"),
            )))
        })?;
        Ok(self.source_of(id))
    }

    /// Whether any paks are mounted.
    pub fn has_paks(&self) -> bool {
        !self.mounted_paks.is_empty()
    }

    /// Number of mounted paks.
    pub fn pak_count(&self) -> usize {
        self.mounted_paks.len()
    }

    /// Number of mounted paks serving zero-copy views now (their `pak_verify` gate is open; see
    /// `regia_asset::pak`'s module docs).
    pub fn zero_copy_pak_count(&self) -> usize {
        self.mounted_paks.iter().filter(|m| m.reader.zero_copy()).count()
    }

    /// Access mesh storage.
    pub fn meshes(&self) -> &Assets<CpuMesh> {
        &self.meshes
    }

    /// Access image storage.
    pub fn images(&self) -> &Assets<CookedImage> {
        &self.images
    }

    /// Access cooked texture storage.
    pub fn textures(&self) -> &Assets<CookedTexture> {
        &self.textures
    }

    /// Access cooked standard-material storage.
    pub fn materials(&self) -> &Assets<CookedStandardMaterial> {
        &self.materials
    }

    /// The whole cooked file of `id` (header and payload) from the mounted paks, else the
    /// cooked cache, and where it came from. For payload types this crate does not decode (a
    /// crate above it registers its own loader: material graphs, scatter sets).
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::Io`] when neither a pak nor the cache holds the id.
    pub fn read_cooked(&mut self, id: AssetId) -> Result<(Vec<u8>, AssetSource), AssetLoadError> {
        let read = self.read_cooked_inner(id);
        crate::load_stats::note_load(read.is_ok());
        read
    }

    /// Body of [`Self::read_cooked`].
    fn read_cooked_inner(&mut self, id: AssetId) -> Result<(Vec<u8>, AssetSource), AssetLoadError> {
        if let Some((bytes, pak)) = self.read_from_paks(id)? {
            self.asset_sources.insert(id, AssetSource::Pak(pak.clone()));
            return Ok((bytes.into_vec(), AssetSource::Pak(pak)));
        }
        if let Some(e) = self.unavailable_error(id) {
            return Err(e);
        }
        let bytes = crate::load_stats::read_file(self.cache_dir.join(cooked_file_name(id)))?;
        self.asset_sources.insert(id, AssetSource::DevCache);
        Ok((bytes, AssetSource::DevCache))
    }

    /// Marks `id` as decoded outside this crate: a hot reload of it only bumps its
    /// [`Self::generation`] (the owner re-reads it with [`Self::read_cooked`]).
    pub fn track_external(&mut self, id: AssetId) {
        self.external.insert(id);
    }

    /// How many times `id` was hot-reloaded. A consumer that cached something built from the
    /// asset compares this to the value it saw to know when to rebuild.
    #[must_use]
    pub fn generation(&self, id: AssetId) -> u64 {
        self.generations.get(&id).copied().unwrap_or(0)
    }

    /// Reload mesh, material, texture or image for `id` in place, keeping its handles valid.
    ///
    /// Every call bumps [`Self::generation`]. An id marked with [`Self::track_external`] is not
    /// decoded here ([`LoadedKind::External`]: its owner re-reads it). Returns the storage it
    /// went to, or `None` when `id` was never loaded and has no cooked file. On a decode error
    /// the previous value stays live.
    pub fn reload_id(&mut self, id: AssetId) -> Result<Option<LoadedKind>, AssetLoadError> {
        *self.generations.entry(id).or_insert(0) += 1;
        if self.external.contains(&id) {
            return Ok(Some(LoadedKind::External));
        }
        let prior = self.load_state.get(&id).cloned();
        let result = self.reload_decode(id);
        if result.is_err()
            && self.is_loaded(id)
            && let Some(prior) = prior
        {
            // The previous value stays live after a failed decode, so its state must not
            // read Failed (a consumer would drop its GPU copy and never upload again).
            self.load_state.insert(id, prior);
        }
        result
    }

    /// After a failed load of an id that was `Loaded`, the previous value is still live: put its
    /// state back so a consumer does not read `Failed` and drop a copy that is still valid.
    fn keep_loaded_on_error(&mut self, id: AssetId, prior: Option<LoadState>, ok: bool) {
        if !ok && prior == Some(LoadState::Loaded) {
            self.load_state.insert(id, LoadState::Loaded);
        }
    }

    /// The decode half of [`Self::reload_id`].
    fn reload_decode(&mut self, id: AssetId) -> Result<Option<LoadedKind>, AssetLoadError> {
        if self.generic_type_of(id).is_some() {
            self.reload_generic(id)?;
            // A counted built-in value reports its own kind, like a typed reload would: the kind
            // after the reload (a texture that became an image reports `Image`, ADR-7284).
            let type_id = self.generic_type_of(id);
            return Ok(Some(
                type_id
                    .and_then(asset_builtin::builtin_loaded_kind)
                    .unwrap_or(LoadedKind::Generic),
            ));
        }
        if self.meshes.handle_for_id(id).is_some() {
            self.load_mesh(id)?;
            return Ok(Some(LoadedKind::Mesh));
        }
        if self.materials.handle_for_id(id).is_some() {
            self.load_material(id)?;
            return Ok(Some(LoadedKind::Material));
        }
        let was_texture = self.textures.handle_for_id(id).is_some();
        if was_texture || self.images.handle_for_id(id).is_some() {
            // An image can become a compressed texture when it grows past the minimum size, and
            // back. Paks first, like `load_image` / `load_texture`: a pak-only image has no cache
            // file. On a successful kind change the id leaves the other slab.
            let bytes = match self.read_from_paks(id)? {
                Some((bytes, _)) => Some(bytes.into_vec()),
                None => crate::load_stats::read_file(self.cache_dir.join(cooked_file_name(id))).ok(),
            };
            let now_texture = match &bytes {
                Some(bytes) if crate::loader::load_texture_bytes(bytes).is_ok() => true,
                Some(bytes) if load_image_bytes(bytes).is_ok() => false,
                _ => was_texture, // undecodable: the slab's own loader reports the error
            };
            if now_texture {
                self.load_texture(id)?;
                self.images.remove(id);
                return Ok(Some(LoadedKind::Texture));
            }
            self.load_image(id)?;
            self.textures.remove(id);
            return Ok(Some(LoadedKind::Image));
        }
        let path = self.cache_dir.join(cooked_file_name(id));
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = crate::load_stats::read_file(&path)?;
        let kind = if load_mesh_bytes(&bytes).is_ok() {
            self.load_mesh(id)?;
            LoadedKind::Mesh
        } else if crate::loader::load_texture_bytes(&bytes).is_ok() {
            self.load_texture(id)?;
            LoadedKind::Texture
        } else if load_material_bytes(&bytes).is_ok() {
            self.load_material(id)?;
            LoadedKind::Material
        } else {
            self.load_image(id)?;
            LoadedKind::Image
        };
        Ok(Some(kind))
    }

    /// True when `id` is loaded in any storage.
    #[must_use]
    pub fn is_loaded(&self, id: AssetId) -> bool {
        self.is_generic_loaded(id)
            || self.meshes.handle_for_id(id).is_some()
            || self.materials.handle_for_id(id).is_some()
            || self.textures.handle_for_id(id).is_some()
            || self.images.handle_for_id(id).is_some()
    }

    /// Sets (or clears) the chunk map that [`Self::unavailable_chunk`] answers from (spec 40).
    /// Paks mounted from a content directory with a `chunk-map.bin` set it at startup.
    pub fn set_chunk_map(&mut self, map: Option<crate::content::ChunkMap>) {
        self.chunk_map = map;
    }

    /// The chunk an asset lives in when that chunk is not mounted, so loading it reads
    /// `LoadState::Failed("unavailable:<chunk>")`. `None` when the id is in a mounted chunk, in
    /// no chunk, or no chunk map is set. `shared = "duplicate"` copies an asset into every chunk
    /// that claims it while the map names only its home, so any mounted pak that holds it counts.
    #[must_use]
    pub fn unavailable_chunk(&self, id: AssetId) -> Option<&str> {
        let chunk = self.chunk_map.as_ref()?.chunk_of(id)?;
        (!self.mounted_paks.iter().any(|m| m.name == chunk) && self.chunks_holding(id).next().is_none())
            .then_some(chunk)
    }

    /// The mounted chunks whose pak holds `id`.
    pub fn chunks_holding(&self, id: AssetId) -> impl Iterator<Item = &str> {
        self.mounted_paks
            .iter()
            .filter(move |m| m.reader.contains(id))
            .map(|m| m.name.as_str())
    }

    /// Chunks that had an unavailable load since the last level load (each logged one warning).
    #[must_use]
    pub fn warned_unavailable_chunks(&self) -> Vec<&str> {
        self.warned_unavailable.iter().map(String::as_str).collect()
    }

    /// Starts a new "one warning per chunk" window (a level loaded, spec 40 § 2.5).
    pub fn reset_unavailable_warnings(&mut self) {
        self.warned_unavailable.clear();
    }

    /// Records the unavailable state of `id` and returns the load error, when its chunk is not
    /// mounted.
    fn unavailable_error(&mut self, id: AssetId) -> Option<AssetLoadError> {
        if self.pushed.contains(id) {
            return None;
        }
        let chunk = self.unavailable_chunk(id)?.to_owned();
        let reason = format!("unavailable:{chunk}");
        if !self.warned_unavailable.contains(&chunk) {
            warn!(target: "regia_asset", %chunk, "asset {id} skipped: its chunk is not mounted");
            self.warned_unavailable.insert(chunk);
        }
        self.load_state.insert(id, LoadState::Failed(reason.clone()));
        Some(AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(
            std::io::Error::new(std::io::ErrorKind::NotFound, reason),
        )))
    }

    /// The manifest path of `id` (cache and mounted paks), if known. A linear scan: tools and the
    /// agent, not per-frame code.
    #[must_use]
    pub fn path_for_id(&self, id: AssetId) -> Option<&str> {
        self.manifest
            .entries
            .iter()
            .rev()
            .find(|e| e.id == id)
            .map(|e| e.path.as_str())
    }

    /// Manifest paths (cache and mounted paks) whose file name ends with `suffix` (ASCII case
    /// ignored), sorted and without duplicates: every `.netprofile.ron` or `.data.ron` the project
    /// ships, say. A linear scan: start-up and tools, not per-frame code (ADR-7232).
    #[must_use]
    pub fn paths_with_suffix(&self, suffix: &str) -> Vec<String> {
        let suffix = suffix.to_ascii_lowercase();
        let mut out: Vec<String> = self
            .manifest
            .entries
            .iter()
            .filter(|e| !e.path.contains('#') && e.path.to_ascii_lowercase().ends_with(&suffix))
            .map(|e| e.path.clone())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Names (file stems) of the mounted paks, in mount order.
    #[must_use]
    pub fn mounted_pak_names(&self) -> Vec<String> {
        self.mounted_paks.iter().map(|m| m.name.clone()).collect()
    }

    /// Mounts the pak of chunk `name` at manifest position `order` (`None`: after every listed
    /// chunk, by name). A pak already mounted under `name` is replaced. Assets of the new pak
    /// that are loaded and now served from it are reloaded in place (their handles stay valid);
    /// their ids are returned.
    ///
    /// This raw host API has no manifest entry to compare. Manifest-managed installations must
    /// use [`crate::content::ContentManager::mount`], which checks size and full-file BLAKE3 first.
    ///
    /// # Errors
    /// The pak does not open.
    pub fn mount_chunk_pak(
        &mut self,
        path: &Path,
        name: &str,
        order: Option<u32>,
    ) -> Result<Vec<AssetId>, AssetLoadError> {
        let _span = tracing::info_span!(target: "regia_asset", "content_mount", chunk = name).entered();
        let reader = PakReader::open(path).map_err(|e| {
            AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(std::io::Error::other(
                format!("{}: {e}", path.display()),
            )))
        })?;
        self.mounted_paks.retain(|m| m.name != name);
        // A good copy of a chunk the startup mount refused (fetched again) lifts the refusal.
        if self.refused_chunks.remove(name).is_some() {
            let mut refused = (*self.refused_ids).clone();
            refused.retain(|_, r| r.chunk != name);
            self.refused_ids = Arc::new(refused);
        }
        let key = |order: Option<u32>, name: &str| (order.is_none(), order.unwrap_or(0), name.to_owned());
        let new_key = key(order, name);
        let at = self
            .mounted_paks
            .iter()
            .position(|m| key(m.order, &m.name) > new_key)
            .unwrap_or(self.mounted_paks.len());
        let ids: Vec<AssetId> = reader.toc.entries.iter().map(|e| e.id).collect();
        self.mounted_paks.insert(
            at,
            MountedPak {
                reader: Arc::new(reader),
                name: name.to_owned(),
                order,
                file: pak_file_name(path),
            },
        );
        // The list is in TOC dependency order (startup re-sorts it), not in key order, so the
        // key position alone can land a pak before its dependency or after a dependent: re-sort
        // the whole list with the startup rule (a no-op when no pak declares dependencies).
        let files: Vec<String> = self.mounted_paks.iter().map(|m| m.file.clone()).collect();
        let deps: Vec<&[String]> = self
            .mounted_paks
            .iter()
            .map(|m| m.reader.toc.dependencies.as_slice())
            .collect();
        let (dep_order, cycles) = toc_dependency_order(&files, &deps);
        warn_toc_cycles(&files, &cycles);
        permute(&mut self.mounted_paks, &dep_order);
        let at = self
            .mounted_paks
            .iter()
            .position(|m| m.name == name)
            .unwrap_or_default();
        self.rebuild_pak_manifest();
        info!(target: "regia_asset", pak=%path.display(), chunk = name, position = at, "chunk mounted");
        // Live assets now served by the new pak (it is the latest mount holding them) reload.
        let mut reloaded = Vec::new();
        for id in ids {
            // Only a live asset can reload, so look that up first; the pak lookup is the reader's
            // id index, not a scan of every TOC entry (a scan per id was quadratic in the size of
            // a hostile chunk's table of contents, ADR-5869).
            if !self.is_loaded(id) {
                continue;
            }
            let served_here = self.mounted_paks[at..]
                .iter()
                .rev()
                .find(|m| m.reader.contains(id))
                .is_some_and(|m| m.name == name);
            if served_here && self.reload_id(id).is_ok() {
                reloaded.push(id);
            }
        }
        Ok(reloaded)
    }

    /// Unmounts the pak of chunk `name`. Assets loaded from it read as unavailable
    /// (`LoadState::Failed("unavailable:<name>")`), unless another mounted pak still holds them:
    /// those reload from it. Returns whether a pak was mounted.
    pub fn unmount_chunk_pak(&mut self, name: &str) -> bool {
        let before = self.mounted_paks.len();
        self.mounted_paks.retain(|m| m.name != name);
        if self.mounted_paks.len() == before {
            return false;
        }
        self.rebuild_pak_manifest();
        let source = AssetSource::Pak(name.to_owned());
        let stale: Vec<AssetId> = self
            .asset_sources
            .iter()
            .filter(|(_, s)| **s == source)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.asset_sources.remove(&id);
            // Another mounted pak still holds it (`shared = "duplicate"`, or the base under a
            // patch): load it from there, so its state stays Loaded (audit v0.2.12: it read
            // `unavailable:<name>` and a load set holding it failed).
            if self.chunks_holding(id).next().is_some() {
                match self.reload_id(id) {
                    Ok(Some(LoadedKind::External)) => {
                        if let Some(pak) = self.chunks_holding(id).last().map(str::to_owned) {
                            self.asset_sources.insert(id, AssetSource::Pak(pak));
                            self.load_state.insert(id, LoadState::Loaded);
                        }
                        continue;
                    }
                    Ok(Some(_)) if self.state(id) == LoadState::Loaded => continue,
                    Ok(_) => {}
                    Err(e) => {
                        warn!(target: "regia_asset", id=%id, chunk = name, error=%e, "reload from another mounted pak failed");
                    }
                }
            }
            self.load_state
                .insert(id, LoadState::Failed(format!("unavailable:{name}")));
        }
        info!(target: "regia_asset", chunk = name, "chunk unmounted");
        true
    }

    /// The path manifest: dev-cache entries, then every mounted pak's entries in mount order
    /// (later paks win).
    fn rebuild_pak_manifest(&mut self) {
        let cache = AssetManifest::read_from_cache(&self.cache_dir)
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut entries = cache.entries;
        for pak in &self.mounted_paks {
            merge_manifest(&mut entries, &pak.reader.toc.manifest);
        }
        self.manifest.note_entries_changed();
        self.manifest.entries = entries;
        self.reindex_manifest();
    }

    /// Re-reads the dev cache's path manifest (a live cook may have added assets). Pak entries
    /// mounted at startup keep precedence for their paths.
    pub fn refresh_manifest(&mut self) {
        let Ok(Some(cache)) = AssetManifest::read_from_cache(&self.cache_dir) else {
            return;
        };
        let pak_paths: std::collections::HashSet<String> = self
            .mounted_paks
            .iter()
            .flat_map(|m| m.reader.toc.manifest.iter().map(|e| e.path.clone()))
            .collect();
        self.manifest.note_entries_changed();
        self.manifest.entries.retain(|e| pak_paths.contains(&e.path));
        self.manifest
            .entries
            .extend(cache.entries.into_iter().filter(|e| !pak_paths.contains(&e.path)));
        self.reindex_manifest();
    }

    /// Project root.
    pub fn project_root(&self) -> &Path {
        &self.settings.project_root
    }

    /// The project's `assets/` plus the overlay roots `Regia.toml` `[cook.overlay_roots]` (or
    /// `REGIA_COOK_OVERLAY_ROOTS`) names, read when the server opened (ADR-8845).
    #[must_use]
    pub fn source_roots(&self) -> &crate::overlay_roots::SourceRoots {
        &self.source_roots
    }

    /// The file behind a source the game reads directly, at `rel` under `assets/`: the project's
    /// own file when there is one, else the earliest overlay root that holds it, else
    /// `assets/<rel>` (the same order the project cook fills from; ADR-8155, ADR-8845).
    #[must_use]
    pub fn source_path(&self, rel: &str) -> PathBuf {
        self.source_roots.path_for(rel)
    }

    /// Cooked cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Sets the text and audio fallback chains localized assets resolve through (spec 36 § 6); the
    /// UI plugin keeps them in step with its `Localization`. Call [`Self::reload_localized`] after.
    pub fn set_locale_chains(&mut self, chains: crate::localized::LocaleChains) {
        self.locale_chains = chains;
    }

    /// The chains set by [`Self::set_locale_chains`].
    #[must_use]
    pub fn locale_chains(&self) -> &crate::localized::LocaleChains {
        &self.locale_chains
    }

    /// The path to load for the asset at `rel` (spec 36 § 6): the first `localized/<L>/<rel>` of the
    /// group's chain that the manifest lists with the base's cooked type, else `rel`. The base path is
    /// remembered so [`Self::reload_localized`] can report when a locale switch moves it.
    pub fn resolve_localized(&mut self, rel: &str, group: crate::localized::LocaleGroup) -> String {
        let rel = rel.replace('\\', "/");
        let resolved =
            crate::localized::resolve_localized_with(&rel, self.locale_chains.chain(group), |path| {
                self.manifest
                    .entries
                    .iter()
                    .find(|e| e.path == path)
                    .map(|e| e.type_path.as_str())
            });
        self.localized
            .insert(rel.trim_start_matches('/').to_owned(), (group, resolved.clone()));
        resolved
    }

    /// Re-resolves every path [`Self::resolve_localized`] handed out against the current chains and
    /// returns the ones that moved (a locale switch), in path order. The caller (the UI plugin) sends
    /// them as [`crate::localized::LocalizedAssetChanged`] messages; the owner of each asset loads the
    /// new path, so a texture swaps without a restart.
    pub fn reload_localized(&mut self) -> Vec<crate::localized::LocalizedAssetChanged> {
        let _span = tracing::debug_span!("asset_reload_localized").entered();
        let tracked: Vec<(String, crate::localized::LocaleGroup, String)> = self
            .localized
            .iter()
            .map(|(rel, (group, path))| (rel.clone(), *group, path.clone()))
            .collect();
        let mut out = Vec::new();
        for (rel, group, from) in tracked {
            let to = self.resolve_localized(&rel, group);
            if to != from {
                out.push(crate::localized::LocalizedAssetChanged { rel, group, from, to });
            }
        }
        out
    }
}

/// Plugin registering [`AssetServer`] when settings are present.
///
/// When the `REGIA_PROBE_ASSET` environment variable names a project-relative
/// asset path (e.g. `assets/tiny.glb`), a startup system loads it through the
/// server, logs where the bytes came from (`regia_asset` target, info level)
/// and exits with an error when the load fails. This is how the packaging
/// end-to-end test proves a shipped game serves the asset from its pak.
#[derive(Default)]
pub struct AssetPlugin {
    /// Server settings.
    pub settings: AssetServerSettings,
}

impl Plugin for AssetPlugin {
    fn finish(&self, app: &mut App) {
        Self::finish_dev_watch(app);
    }

    fn build(&self, app: &mut App) {
        // `Vfs::mount_pak` reads archives through this crate (ADR-7232).
        crate::vfs_pak::install_vfs_pak_mounts();
        app.init_resource::<crate::data_asset::DataAssets>()
            .init_resource::<crate::engine_source::EngineAssets>();
        app.add_message::<AssetReloaded>()
            .add_message::<SourceAssetChanged>()
            .init_resource::<AssetWatchStatus>()
            .init_resource::<crate::LoadSets>()
            .add_message::<AssetEvent<CpuMesh>>()
            .add_message::<AssetEvent<CookedTexture>>()
            .add_message::<AssetEvent<CookedImage>>()
            .add_message::<AssetEvent<CookedStandardMaterial>>()
            .add_message::<LoadGroupCompleted>()
            .add_systems(PreUpdate, asset_hot_reload_system)
            .add_systems(
                PreUpdate,
                asset_server_update_system.after(asset_hot_reload_system),
            )
            .add_systems(
                PreUpdate,
                (
                    forward_asset_events::<CpuMesh>,
                    forward_asset_events::<CookedTexture>,
                    forward_asset_events::<CookedImage>,
                    forward_asset_events::<CookedStandardMaterial>,
                )
                    .after(asset_server_update_system),
            )
            .add_systems(PreUpdate, crate::load_set::refresh_load_sets);
        let mut server = match AssetServer::open(self.settings.clone()) {
            Ok(s) => s,
            // Installed content that fails its manifest check ends the app with an error exit
            // code at startup, so a damaged install does not run without its content (ADR-7979).
            Err(AssetLoadError::Content(e)) => {
                tracing::error!(target: "regia_asset", reason = e.reason(),
                    "installed content failed verification; exiting: {e}");
                app.add_systems(Startup, |mut exit: MessageWriter<AppExit>| {
                    exit.write(AppExit::Error(PROBE_EXIT_CODE));
                });
                return;
            }
            Err(e) => {
                tracing::warn!(target: "regia_asset", "asset cache not available: {e}");
                return;
            }
        };
        // Load groups report to the app's registry, so loading screens read one progress source;
        // the built-in slabs report `AssetEvent`s, forwarded as messages each frame (ADR-7202).
        server.set_load_sets(app.world().resource::<crate::LoadSets>().clone());
        server.enable_events::<CpuMesh>();
        server.enable_events::<CookedTexture>();
        server.enable_events::<CookedImage>();
        server.enable_events::<CookedStandardMaterial>();
        let mounted_paks = server.pak_count();
        tracing::info!(
            target: "regia_asset",
            mounted_paks,
            content_dir = ?server.settings_content_dir(),
            "asset server ready"
        );
        app.insert_resource(server);
        if let Some(path) = regia_core::brand::env_var_os("REGIA_PROBE_ASSET") {
            let path = path.to_string_lossy().into_owned();
            app.add_systems(
                Startup,
                move |server: Option<ResMut<AssetServer>>, mut exit: MessageWriter<AppExit>| {
                    let Some(mut server) = server else {
                        error!(
                            target: "regia_asset",
                            "probe: no asset server; cannot load {path:?}"
                        );
                        exit.write(AppExit::Error(PROBE_EXIT_CODE));
                        return;
                    };
                    match server.probe(&path) {
                        Ok(AssetSource::Pak(name)) => {
                            info!(target: "regia_asset", "probe: {path} loaded from pak {name:?}")
                        }
                        Ok(AssetSource::DevCache) => {
                            info!(target: "regia_asset", "probe: {path} loaded from dev cache (not a pak)")
                        }
                        Ok(AssetSource::Unknown) => {
                            info!(target: "regia_asset", "probe: {path} loaded (source unknown)")
                        }
                        Err(e) => {
                            error!(target: "regia_asset", "probe: {path} FAILED to load: {e}");
                            exit.write(AppExit::Error(PROBE_EXIT_CODE));
                        }
                    }
                },
            );
        }
    }
}

impl AssetPlugin {
    /// `finish`: in a `dev-assets` build with no mounted paks, start the dev watchers (after every
    /// plugin had the chance to add [`crate::DevWatchRoots`]).
    fn finish_dev_watch(app: &mut App) {
        #[cfg(all(feature = "dev-assets", not(target_os = "android")))]
        {
            let Some(server) = app.world().get_resource::<AssetServer>() else {
                return;
            };
            if server.pak_count() == 0 && dev_watch_enabled() {
                let cache = server.cache_dir().to_path_buf();
                let assets = server.project_root().join("assets");
                // Overlay roots are watched too: a directly-read source served from one reloads.
                let overlays = server.source_roots().overlays().to_vec();
                start_dev_watch(app, cache, assets, overlays);
            }
        }
        #[cfg(not(all(feature = "dev-assets", not(target_os = "android"))))]
        let _ = app;
    }
}

/// A load group finished: every member loaded or failed ([`AssetServer::load_group`]). Written once
/// per group by [`asset_server_update_system`].
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct LoadGroupCompleted {
    /// The group's name ([`LoadGroupHandle::name`]).
    pub name: String,
}

/// `PreUpdate`, after [`asset_hot_reload_system`]: one [`AssetServer::update`] (finished loads,
/// unload grace, budgets) and a [`LoadGroupCompleted`] per completed group.
pub fn asset_server_update_system(
    server: Option<ResMut<AssetServer>>,
    mut completed: MessageWriter<LoadGroupCompleted>,
) {
    let Some(mut server) = server else {
        return;
    };
    server.update();
    for name in server.take_completed_groups() {
        completed.write(LoadGroupCompleted { name });
    }
}

/// `PreUpdate`, after [`asset_server_update_system`]: the server's [`AssetEvent`]s of `T` as
/// messages.
pub fn forward_asset_events<T: Send + Sync + 'static>(
    server: Option<ResMut<AssetServer>>,
    mut out: MessageWriter<AssetEvent<T>>,
) {
    let Some(mut server) = server else {
        return;
    };
    let events = server.drain_events::<T>();
    if !events.is_empty() {
        out.write_batch(events);
    }
}

/// Registers generic asset loaders on an [`App`] that has the [`AssetPlugin`].
pub trait AssetAppExt {
    /// Registers `loader` with the [`AssetServer`], turns on its [`AssetEvent`]s and forwards them
    /// as messages each frame. Without a server (no cooked cache) it logs and does nothing else.
    fn register_asset_loader<L: AssetLoader>(&mut self, loader: L) -> &mut Self;
}

impl AssetAppExt for App {
    fn register_asset_loader<L: AssetLoader>(&mut self, loader: L) -> &mut Self {
        self.add_message::<AssetEvent<L::Asset>>().add_systems(
            PreUpdate,
            forward_asset_events::<L::Asset>.after(asset_server_update_system),
        );
        match self.world_mut().get_resource_mut::<AssetServer>() {
            Some(mut server) => {
                server.register_loader(loader);
                server.enable_events::<L::Asset>();
            }
            None => {
                warn!(target: "regia_asset", kind = loader.kind(), "no asset server; loader not registered");
            }
        }
        self
    }
}

/// Exit code the probe requests when its asset fails to load, and the startup exit when installed
/// content fails its manifest check (ADR-7979).
const PROBE_EXIT_CODE: NonZeroU8 = match NonZeroU8::new(1) {
    Some(code) => code,
    None => match NonZeroU8::new(2) {
        Some(code) => code,
        None => unreachable!(), // invariant: NonZeroU8::new(1) always yields Some
    },
};

/// Resource holding the reload receiver until [`attach_reload_receiver`] runs.
#[derive(Resource)]
pub struct PendingAssetReload(pub Option<Mutex<Receiver<ReloadMessage>>>);

/// Attach a reload channel to the running [`AssetServer`].
pub fn attach_reload_receiver(world: &mut World) {
    let Some(pending) = world.remove_resource::<PendingAssetReload>() else {
        return;
    };
    if let (Some(rx), Some(mut server)) = (pending.0, world.get_resource_mut::<AssetServer>()) {
        server.set_reload_receiver(rx.into_inner());
    }
}

/// Channel the live cooker (editor) or the dev watchers (`regia run --dev`) feed. Drained by
/// [`asset_hot_reload_system`] each frame.
#[derive(Resource)]
pub struct AssetReloadInbox(pub Mutex<Receiver<ReloadMessage>>);

impl AssetReloadInbox {
    /// An inbox and the sender that feeds it.
    #[must_use]
    pub fn channel() -> (std::sync::mpsc::Sender<ReloadMessage>, Self) {
        let (tx, rx) = std::sync::mpsc::channel();
        (tx, Self(Mutex::new(rx)))
    }
}

/// Drain reload messages and refresh assets (`docs/specs/12-assets.md` § 8).
///
/// `Reload(id)` replaces the asset in place and writes [`AssetReloaded`]; `Source(path)` writes
/// [`SourceAssetChanged`]; `Status` lands in [`AssetWatchStatus`]. A failed reload keeps the old
/// value and is recorded as an error.
#[instrument(skip_all, name = "asset_hot_reload")]
pub fn asset_hot_reload_system(world: &mut World) {
    let mut messages: Vec<ReloadMessage> = Vec::new();
    if let Some(inbox) = world.get_resource::<AssetReloadInbox>() {
        messages.extend(inbox.0.lock().try_iter());
    }
    if let Some(server) = world.get_resource::<AssetServer>()
        && let Some(rx) = server.reload_rx.as_ref()
    {
        messages.extend(rx.lock().try_iter());
    }
    apply_reload_messages(world, messages);
}

/// Orders pak files for mounting (later paks override earlier ones): by lowercased file name,
/// then by path, so `Patch.fpak` mounts after `base.fpak` whatever the byte order of `P` and `b`.
fn sort_pak_mount_order(paths: &mut [PathBuf]) {
    paths.sort_by_cached_key(|p| {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        (name, p.clone())
    });
}

/// The mount order the TOC dependency lists ask for (spec 12 § 5), over paks given in their
/// order without dependencies.
///
/// `files` are the pak file names and `deps` their TOC dependency lists. A dependency entry
/// matches a pak by file name (case-folded), else by file stem; when several mounted paks share
/// that name or stem it matches every one of them. Entries that match nothing, and a pak naming
/// itself, are ignored. The paks are grouped into strongly connected components (a
/// dependency cycle is one component; every other pak is its own), and the components are placed
/// by a stable topological pass: the earliest component (by its first pak in the incoming order)
/// whose outside dependencies are all placed goes next, its paks in the incoming order. So a pak
/// keeps its position whenever no dependency constrains it, only the paks IN a cycle fall back to
/// the incoming order among themselves, and a pak that depends on a cycle still mounts after it.
/// Returns the order (indices into `files`) and each cycle (incoming order), for the warning.
fn toc_dependency_order(files: &[String], deps: &[&[String]]) -> (Vec<usize>, Vec<Vec<usize>>) {
    use std::collections::HashMap;

    let n = files.len();
    // Two mounted paks can share a file name (a runtime chunk pak from a downloads dir next to a
    // content pak of the same name). A dependency entry matching that name binds to EVERY pak
    // with it, so the dependent mounts after all of them: a patch that names its base still
    // overrides a same-named downloaded copy (spec 12 § 5). Matching a name takes precedence
    // over matching a stem; a pak never depends on itself.
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, file) in files.iter().enumerate() {
        by_name.entry(file.to_lowercase()).or_default().push(i);
        if let Some(stem) = Path::new(file).file_stem() {
            by_stem
                .entry(stem.to_string_lossy().to_lowercase())
                .or_default()
                .push(i);
        }
    }
    let edges: Vec<Vec<usize>> = (0..n)
        .map(|i| {
            let mut out: Vec<usize> = Vec::new();
            for dep in deps.get(i).copied().unwrap_or_default() {
                let name = Path::new(dep)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                let Some(targets) = by_name.get(&name).or_else(|| by_stem.get(&name)) else {
                    tracing::debug!(
                        target: "regia_asset",
                        pak = %files[i],
                        dependency = %dep,
                        "TOC dependency matches no mounted pak; ignored"
                    );
                    continue;
                };
                if targets.len() > 1 {
                    tracing::debug!(
                        target: "regia_asset",
                        pak = %files[i],
                        dependency = %dep,
                        matches = targets.len(),
                        "TOC dependency matches several mounted paks of that name; it mounts after each"
                    );
                }
                out.extend(targets.iter().copied().filter(|&j| j != i));
            }
            out.sort_unstable();
            out.dedup();
            out
        })
        .collect();

    // reach[i][j]: pak i depends on pak j, directly or through others.
    let mut reach = vec![vec![false; n]; n];
    for (i, row) in reach.iter_mut().enumerate() {
        let mut stack = edges[i].clone();
        while let Some(j) = stack.pop() {
            if !row[j] {
                row[j] = true;
                stack.extend(edges[j].iter().copied());
            }
        }
    }
    // Components in the order of their first pak; members in the incoming order.
    let mut component = vec![usize::MAX; n];
    let mut components: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        if component[i] != usize::MAX {
            continue;
        }
        let members: Vec<usize> = (i..n)
            .filter(|&j| j == i || (component[j] == usize::MAX && reach[i][j] && reach[j][i]))
            .collect();
        for &j in &members {
            component[j] = components.len();
        }
        components.push(members);
    }

    let mut placed = vec![false; components.len()];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        // The condensation is acyclic, so some unplaced component is always ready; the fallback
        // (the first unplaced one) only guards against a logic slip.
        let ready = |c: usize| {
            components[c].iter().all(|&i| {
                edges[i]
                    .iter()
                    .all(|&j| component[j] == c || placed[component[j]])
            })
        };
        let Some(c) = (0..components.len())
            .filter(|&c| !placed[c])
            .find(|&c| ready(c))
            .or_else(|| (0..components.len()).find(|&c| !placed[c]))
        else {
            break;
        };
        placed[c] = true;
        order.extend(components[c].iter().copied());
    }
    let cycles = components
        .into_iter()
        .filter(|members| members.len() > 1)
        .collect();
    (order, cycles)
}

/// Applies `order` (a permutation of indices into `items`) to `items`.
fn permute<T>(items: &mut Vec<T>, order: &[usize]) {
    let mut slots: Vec<Option<T>> = std::mem::take(items).into_iter().map(Some).collect();
    items.extend(
        order
            .iter()
            .filter_map(|&i| slots.get_mut(i).and_then(Option::take)),
    );
    // Anything `order` missed keeps its relative order at the end (never expected).
    items.extend(slots.into_iter().flatten());
}

/// Logs each TOC dependency cycle `toc_dependency_order` found.
fn warn_toc_cycles(files: &[String], cycles: &[Vec<usize>]) {
    for cycle in cycles {
        let paks: Vec<&str> = cycle.iter().map(|&i| files[i].as_str()).collect();
        tracing::warn!(
            target: "regia_asset",
            paks = ?paks,
            "TOC dependency cycle; these paks mount in the content-manifest, then file-name order among themselves"
        );
    }
}

/// Re-sorts opened paks so every TOC dependency mounts before the pak that lists it (spec 12 § 5),
/// with the rules of `toc_dependency_order`. `opened` arrives in the order without dependencies
/// (content-manifest order, then the case-folded file name, then the path).
fn order_paks_by_toc_dependencies(opened: &mut Vec<(PathBuf, PakReader)>) {
    let files: Vec<String> = opened.iter().map(|(path, _)| pak_file_name(path)).collect();
    let deps: Vec<&[String]> = opened
        .iter()
        .map(|(_, reader)| reader.toc.dependencies.as_slice())
        .collect();
    let (order, cycles) = toc_dependency_order(&files, &deps);
    warn_toc_cycles(&files, &cycles);
    permute(opened, &order);
}

/// A pak's file name (the key TOC dependency entries match).
fn pak_file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Applies reload messages to `world` (what [`asset_hot_reload_system`] does with its inbox).
/// The editor uses it to feed the edit world and the play-in-editor world from one live cooker.
pub fn apply_reload_messages(world: &mut World, messages: Vec<ReloadMessage>) {
    if messages.is_empty() {
        return;
    }
    let mut reloaded = Vec::new();
    let mut changed = Vec::new();
    let mut statuses = Vec::new();
    let mut failures = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(mut server) = world.get_resource_mut::<AssetServer>() {
        if messages.iter().any(|m| matches!(m, ReloadMessage::Reload(_))) {
            server.refresh_manifest();
        }
        for message in messages {
            match message {
                ReloadMessage::Reload(id) => {
                    if !seen.insert(id) {
                        continue;
                    }
                    match server.reload_id(id) {
                        Ok(Some(kind)) => reloaded.push(AssetReloaded { id, kind }),
                        Ok(None) => {}
                        Err(err) => {
                            warn!(target: "regia_asset", %id, %err, "hot reload failed; keeping the previous version");
                            failures.push((id, err.to_string()));
                        }
                    }
                }
                ReloadMessage::Source(path) => changed.push(SourceAssetChanged::new(path)),
                ReloadMessage::Status(entry) => statuses.push(entry),
            }
        }
        // Generic assets that use a reloaded one reloaded with it (ADR-7202).
        for id in server.take_reloaded_dependents() {
            reloaded.push(AssetReloaded {
                id,
                kind: LoadedKind::Generic,
            });
        }
    } else {
        for message in messages {
            match message {
                ReloadMessage::Reload(_) => {}
                ReloadMessage::Source(path) => changed.push(SourceAssetChanged::new(path)),
                ReloadMessage::Status(entry) => statuses.push(entry),
            }
        }
    }
    if let Some(mut status) = world.get_resource_mut::<AssetWatchStatus>() {
        for entry in statuses {
            let line = status.record(entry);
            info!(target: "regia_asset", "{line}");
        }
        for (id, err) in failures {
            status.record(crate::live::WatchEntry {
                path: id.to_string(),
                kind: crate::live::SourceKind::Other,
                millis: 0,
                outcome: crate::live::WatchOutcome::Error(err),
            });
        }
    }
    // A world without the asset plugin (a minimal play-in-editor world) has no message queues.
    if world.contains_resource::<Messages<AssetReloaded>>() {
        for message in reloaded {
            world.write_message(message);
        }
    }
    if world.contains_resource::<Messages<SourceAssetChanged>>() {
        for message in changed {
            world.write_message(message);
        }
    }
}

/// Marks a world whose reload messages come from elsewhere (the editor's live cooker): the
/// [`AssetPlugin`] then starts no watcher of its own.
#[derive(Resource, Debug, Default)]
pub struct ExternalReloadSource;

/// `dev-assets` builds watch unless `REGIA_ASSET_WATCH=0`.
#[cfg(all(feature = "dev-assets", not(target_os = "android")))]
fn dev_watch_enabled() -> bool {
    regia_core::brand::env_var("REGIA_ASSET_WATCH").map_or(true, |v| v != "0")
}

/// Keeps the dev watchers of a standalone dev run alive.
#[cfg(all(feature = "dev-assets", not(target_os = "android")))]
#[derive(Resource)]
pub struct DevAssetWatch {
    _cooked: Option<notify::RecommendedWatcher>,
    _sources: Option<crate::hot_reload::SourceForwarder>,
}

/// Starts the game side of a standalone dev run (`docs/specs/12-assets.md` § 8): a watcher on the
/// cooked cache (`regia run --dev` cooks into it) and a forwarder for the kinds the runtime
/// reads from source. Both feed an [`AssetReloadInbox`].
#[cfg(all(feature = "dev-assets", not(target_os = "android")))]
fn start_dev_watch(app: &mut App, cache: PathBuf, assets: PathBuf, overlays: Vec<PathBuf>) {
    if app.world().contains_resource::<AssetReloadInbox>()
        || app.world().contains_resource::<ExternalReloadSource>()
    {
        return;
    }
    let (tx, inbox) = AssetReloadInbox::channel();
    let _ = std::fs::create_dir_all(&cache);
    let mut source_roots: Vec<PathBuf> = Vec::new();
    if assets.is_dir() {
        source_roots.push(assets);
    }
    source_roots.extend(overlays.into_iter().filter(|dir| dir.is_dir()));
    if let Some(extra) = app.world().get_resource::<crate::live::DevWatchRoots>() {
        source_roots.extend(extra.0.iter().filter(|dir| dir.is_dir()).cloned());
    }
    let cooked = match crate::hot_reload::spawn_cooked_watcher(&cache, tx.clone()) {
        Ok(w) => Some(w),
        Err(err) => {
            warn!(target: "regia_asset", %err, "cooked-cache watcher unavailable; asset hot reload off");
            None
        }
    };
    let sources = if !source_roots.is_empty() {
        match crate::hot_reload::spawn_source_forwarder(
            &source_roots,
            crate::hot_reload::ASSET_DEBOUNCE,
            crate::live::SourceKind::is_read_at_runtime,
            tx,
        ) {
            Ok(f) => Some(f),
            Err(err) => {
                warn!(target: "regia_asset", %err, "source watcher unavailable");
                None
            }
        }
    } else {
        None
    };
    let mut status = AssetWatchStatus {
        watching: cooked.is_some() || sources.is_some(),
        debounce_ms: crate::hot_reload::ASSET_DEBOUNCE.as_millis() as u64,
        ..AssetWatchStatus::default()
    };
    if cooked.is_some() {
        status.roots.push(cache);
    }
    if let Some(forwarder) = &sources {
        status.roots.extend(forwarder.roots().iter().cloned());
    }
    info!(target: "regia_asset", roots = ?status.roots, "dev asset watch on");
    app.insert_resource(status)
        .insert_resource(inbox)
        .insert_resource(DevAssetWatch {
            _cooked: cooked,
            _sources: sources,
        });
}

/// Reads `id` from the latest of `mounts` (reader, chunk name, manifest position) that holds it.
/// `Ok(None)`: no mounted pak holds it.
///
/// # Errors
/// `id` belongs to a refused chunk and no pak mounted after that chunk holds it
/// (`chunk_refused`), or the latest pak that holds it fails to read the entry
/// (`pak_entry_corrupt`, logged and noted as `pak_corrupt` telemetry). An earlier pak's copy is
/// never served instead: that would be the asset's previous version (ADR-8810). A later pak's
/// copy is newer than the refused one, so it serves the id (ADR-8860).
fn read_checked<'a>(
    refused: &HashMap<AssetId, RefusedChunk>,
    mounts: impl DoubleEndedIterator<Item = (&'a Arc<PakReader>, &'a str, Option<u32>)>,
    id: AssetId,
) -> Result<Option<(PakPayload, String)>, AssetLoadError> {
    let invalid = |message: String| {
        AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )))
    };
    let newest = mounts.rev().find(|(reader, _, _)| reader.contains(id));
    if let Some(r) = refused.get(&id) {
        let newer = newest
            .as_ref()
            .is_some_and(|(_, name, order)| mount_key(*order, name) > mount_key(Some(r.order), &r.chunk));
        if !newer {
            return Err(invalid(format!(
                "chunk_refused: asset {id} is in chunk `{}`, whose pak was not mounted ({})",
                r.chunk, r.code
            )));
        }
    }
    let Some((reader, name, _)) = newest else {
        return Ok(None);
    };
    match reader.read_shared(id) {
        Ok(bytes) => {
            crate::load_stats::note_payload(bytes.len());
            Ok(Some((bytes, name.to_string())))
        }
        Err(e) => {
            note_pak_error(&e, name, id);
            warn!(
                target: "regia_asset",
                id=%id,
                pak=%name,
                error=%e,
                "pak entry does not read; the asset fails (no earlier pak serves it)"
            );
            Err(invalid(format!("pak_entry_corrupt: {name} entry {id}: {e}")))
        }
    }
}

/// Reads cooked payloads from an [`AssetServer`]'s mounts and cache on any thread
/// ([`AssetServer::payload_reader`]). Cheap to clone: it shares the mounted paks.
#[derive(Clone)]
pub struct PayloadReader {
    paks: Vec<(Arc<PakReader>, String, Option<u32>)>,
    cache_dir: PathBuf,
    forms: Arc<crate::cooked_pack::LoadFormCounters>,
    refused_ids: Arc<HashMap<AssetId, RefusedChunk>>,
    pushed: Arc<crate::push::PushedIds>,
}

impl PayloadReader {
    /// The cooked file of `id` (wrapper header + payload) and where it came from: the latest
    /// mounted pak that holds it, else the cooked cache.
    ///
    /// # Errors
    ///
    /// [`AssetLoadError::Cooked`] when neither a pak nor the cache holds it.
    pub fn read(&self, id: AssetId) -> Result<(PakPayload, AssetSource), AssetLoadError> {
        if !self.pushed.contains(id)
            && let Some((bytes, pak)) = read_checked(
                &self.refused_ids,
                self.paks.iter().map(|(r, n, o)| (r, n.as_str(), *o)),
                id,
            )?
        {
            return Ok((bytes, AssetSource::Pak(pak)));
        }
        let bytes = crate::load_stats::read_file(self.cache_dir.join(cooked_file_name(id)))
            .map_err(|e| AssetLoadError::Cooked(crate::cooked_header::CookedIoError::Io(e)))?;
        Ok((PakPayload::from(bytes), AssetSource::DevCache))
    }

    /// Counts a mesh this reader's caller decoded, in its server's [`AssetServer::load_forms`].
    pub fn note_mesh_form(&self, form: crate::cooked_pack::MeshForm, source: &AssetSource) {
        self.forms.note_mesh(form);
        self.forms.note_source(matches!(source, AssetSource::Pak(_)));
    }

    /// Counts a KTX2 texture this reader's caller decoded ([`AssetServer::load_forms`]).
    pub fn note_texture(&self, source: &AssetSource) {
        self.forms.note_texture();
        self.forms.note_source(matches!(source, AssetSource::Pak(_)));
    }

    /// Counts an RGBA8 image this reader's caller decoded ([`AssetServer::load_forms`]).
    pub fn note_image(&self, source: &AssetSource) {
        self.forms.note_image();
        self.forms.note_source(matches!(source, AssetSource::Pak(_)));
    }
}

/// Merges a mounted pak's manifest into `entries`: an entry replaces every earlier entry of its
/// path, and within the pak the last entry of a path wins, in pak order. One pass over each list
/// (ADR-530); removing each entry's path from the whole list was quadratic in the entry count.
fn merge_manifest(entries: &mut Vec<crate::ManifestEntry>, pak: &[crate::ManifestEntry]) {
    let last: HashMap<&str, usize> = pak
        .iter()
        .enumerate()
        .map(|(i, e)| (e.path.as_str(), i))
        .collect();
    entries.retain(|e| !last.contains_key(e.path.as_str()));
    entries.extend(
        pak.iter()
            .enumerate()
            .filter(|&(i, e)| last.get(e.path.as_str()) == Some(&i))
            .map(|(_, e)| e.clone()),
    );
}

/// Closes the zero-copy gate of a hashed pak that has not passed the whole-file check with this
/// build (the first launch after an update) and returns it for the check to open; `None` when the
/// pak may serve views of its mapping now (verified on an earlier launch, unhashed, or the check
/// is off). See `pak_verify`'s module docs.
fn zero_copy_gate_at_mount(
    reader: &PakReader,
    path: &Path,
    verify: Option<&crate::pak_verify::PakVerifyOptions>,
) -> Option<crate::pak::ZeroCopyGate> {
    // One rule for every reader of a pak (the terrain tile source applies it too).
    crate::pak_verify::gate_until_verified(reader, path, verify)
}

/// Notes a pak integrity failure for telemetry ([`regia_core::telemetry::NoteKind::PakCorrupt`]).
fn note_pak_error(error: &crate::pak::PakError, pak: &str, id: AssetId) {
    use crate::pak::PakError;
    let offset = match error {
        PakError::Corrupt { offset, .. } => *offset as f64,
        PakError::HashMismatch { .. } => -1.0,
        _ => return,
    };
    let label = format!("{pak} entry {id}");
    regia_core::telemetry::note(regia_core::telemetry::NoteKind::PakCorrupt, &label, offset);
}

#[cfg(test)]
mod tests {
    #[test]
    fn pak_mount_order_ignores_name_case() {
        let mut paths = vec![
            PathBuf::from("/p/Patch.fpak"),
            PathBuf::from("/p/base.fpak"),
            PathBuf::from("/p/Zeta.fpak"),
        ];
        sort_pak_mount_order(&mut paths);
        let names: Vec<_> = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .expect("file name")
                    .to_str()
                    .expect("utf-8 name")
                    .to_string()
            })
            .collect();
        assert_eq!(names, ["base.fpak", "Patch.fpak", "Zeta.fpak"]);
    }

    use super::*;
    use crate::ManifestEntry;
    use crate::cooked::CpuMesh;
    use crate::cooked_header::pack_importer_version;
    use crate::cooked_header::{COOKED_FORMAT_VERSION, CookedHeader, encode_cooked_file};
    use crate::loader::AssetTypeTag;
    use crate::pak::{FPAK_BLOB_ALIGNMENT, FPAK_VERSION, PakEntry, PakToc, encode_pak_header};

    #[test]
    fn load_mesh_from_cache() {
        let tmp = std::env::temp_dir().join(format!("regia_asset_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("tempdir");
        let platform = "linux";
        let cache = cooked_cache_dir(&tmp, platform);
        std::fs::create_dir_all(&cache).expect("cache");
        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let payload = mesh.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        std::fs::write(cache.join(cooked_file_name(id)), file).expect("write");
        let manifest = AssetManifest {
            entries: vec![crate::manifest::ManifestEntry {
                path: "meshes/tri.mesh".to_string(),
                id,
                type_path: "regia_asset::CpuMesh".to_string(),
            }],
        };
        manifest.write_to_cache(&cache).expect("manifest");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: platform.to_string(),
            content_dir: None,
        })
        .expect("open");
        let h = server.load_mesh_path("meshes/tri.mesh").expect("load");
        let m = server.meshes().get(&h).expect("get");
        assert_eq!(m.indices.len(), 3);
        assert_eq!(server.source_of(id), AssetSource::DevCache);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// P-314 acceptance: a loaded asset reports its load time: the typed load and the counted
    /// (`load::<T>`) load of the same cooked mesh each leave one row with the cooked file's bytes,
    /// its source and read/decode times that add up to the row's total.
    #[test]
    fn a_loaded_asset_reports_its_load_time() {
        let tmp = regia_test_utils::unique_temp_dir("asset-load-time");
        let platform = "linux";
        let cache = cooked_cache_dir(&tmp, platform);
        std::fs::create_dir_all(&cache).expect("cache");
        let cook = |id: AssetId| {
            let mesh = CpuMesh {
                positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
                tangents: Vec::new(),
                indices: vec![0, 1, 2],
                ..Default::default()
            };
            let payload = mesh.to_postcard().expect("postcard");
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: AssetTypeTag::mesh_legacy_cpu().0,
                asset_id: id,
                importer_version: pack_importer_version(1, 0, 0),
                dependencies: vec![],
                payload_len: payload.len() as u64,
            };
            let file = encode_cooked_file(&header, &payload).expect("encode");
            std::fs::write(cache.join(cooked_file_name(id)), &file).expect("write");
            file.len() as u64
        };
        let (typed, counted) = (AssetId::new_random(), AssetId::new_random());
        let (typed_bytes, counted_bytes) = (cook(typed), cook(counted));
        AssetManifest::default().write_to_cache(&cache).expect("manifest");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: platform.to_string(),
            content_dir: None,
        })
        .expect("open");
        assert!(server.load_times().get(typed).is_none(), "nothing loaded yet");
        server.load_mesh(typed).expect("typed load");
        let _handle = server.load::<CpuMesh>(counted).expect("counted load");
        server.wait_for_loads();
        for (id, bytes, kind) in [(typed, typed_bytes, "mesh"), (counted, counted_bytes, "mesh")] {
            let row = server.load_times().get(id).expect("the load recorded its time");
            assert!(row.ok, "{row:?}");
            assert_eq!((row.bytes, row.source), (bytes, "cache"), "{row:?}");
            assert_eq!(row.kind, kind);
            assert!(row.total_ns() > 0, "a read and a decode take time: {row:?}");
            assert_eq!(row.total_ns(), row.read_ns + row.decode_ns + row.upload_ns);
        }
        let rows = server.load_times().rows();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [typed, counted]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_failed_reload_keeps_the_loaded_state_of_the_live_value() {
        let tmp = regia_test_utils::unique_temp_dir("asset-failed-reload");
        let platform = "linux";
        let cache = cooked_cache_dir(&tmp, platform);
        std::fs::create_dir_all(&cache).expect("cache");
        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let payload = mesh.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let file_path = cache.join(cooked_file_name(id));
        std::fs::write(&file_path, file).expect("write");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: platform.to_string(),
            content_dir: None,
        })
        .expect("open");
        let handle = server.load_mesh(id).expect("load");
        assert_eq!(server.state(id), LoadState::Loaded);
        // A broken file on disk: the reload fails, the old mesh stays live and loaded.
        std::fs::write(&file_path, b"not a cooked file").expect("corrupt");
        assert!(server.reload_id(id).is_err());
        assert_eq!(server.state(id), LoadState::Loaded);
        assert!(server.meshes().get(&handle).is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn write_cooked(path: &std::path::Path, id: AssetId, tag: AssetTypeTag, payload: &[u8]) {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: tag.0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        std::fs::write(path, encode_cooked_file(&header, payload).expect("encode")).expect("write");
    }

    /// A second direct `load_mesh` that fails keeps the live mesh's `Loaded` state.
    #[test]
    fn a_failed_second_load_keeps_the_loaded_state() {
        let tmp = regia_test_utils::unique_temp_dir("asset-failed-second-load");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0; 2]; 3],
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let file_path = cache.join(cooked_file_name(id));
        write_cooked(
            &file_path,
            id,
            AssetTypeTag::mesh_legacy_cpu(),
            &mesh.to_postcard().expect("postcard"),
        );
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");
        let handle = server.load_mesh(id).expect("load");
        std::fs::write(&file_path, b"garbage").expect("corrupt");
        assert!(server.load_mesh(id).is_err());
        assert_eq!(server.state(id), LoadState::Loaded);
        assert_eq!(server.mesh_state(&handle), LoadState::Loaded);
        assert!(server.meshes().get(&handle).is_some());
        // An id that never loaded still reports the failure.
        let other = AssetId::new_random();
        std::fs::write(cache.join(cooked_file_name(other)), b"garbage").expect("write");
        assert!(server.load_mesh(other).is_err());
        assert!(matches!(server.state(other), LoadState::Failed(_)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An id whose bytes change between image and compressed texture moves slabs, both ways.
    #[test]
    fn a_reload_that_changes_image_to_texture_and_back_moves_slabs() {
        let tmp = regia_test_utils::unique_temp_dir("asset-kind-change");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let id = AssetId::new_random();
        let path = cache.join(cooked_file_name(id));
        let image = CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![1, 2, 3, 4],
        };
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: crate::cooked::TextureCookUsage::ColorSrgb,
            ktx2: crate::cooked::tests::minimal_ktx2().into(),
        };
        let image_bytes = image.to_postcard().expect("postcard");
        let texture_bytes = texture.to_postcard().expect("postcard");
        write_cooked(&path, id, AssetTypeTag::image(), &image_bytes);
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");
        let image_handle = server.load_image(id).expect("image");

        write_cooked(&path, id, AssetTypeTag::texture(), &texture_bytes);
        assert_eq!(
            server.reload_id(id).expect("to texture"),
            Some(LoadedKind::Texture)
        );
        assert!(server.textures().handle_for_id(id).is_some());
        assert!(
            server.images().get(&image_handle).is_none(),
            "the old image is gone"
        );
        // A second reload stays a texture and serves the new bytes.
        assert_eq!(server.reload_id(id).expect("again"), Some(LoadedKind::Texture));

        write_cooked(&path, id, AssetTypeTag::image(), &image_bytes);
        assert_eq!(server.reload_id(id).expect("to image"), Some(LoadedKind::Image));
        assert!(
            server.textures().handle_for_id(id).is_none(),
            "the old texture is gone"
        );
        let handle = server.images().handle_for_id(id).expect("image handle");
        assert_eq!(
            server.images().get(&handle).expect("image").rgba8,
            vec![1, 2, 3, 4]
        );
        assert_eq!(server.state(id), LoadState::Loaded);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn external_payloads_read_raw_and_count_reloads() {
        let tmp = std::env::temp_dir().join(format!("regia_asset_test_{}", uuid::Uuid::new_v4()));
        let platform = "linux";
        let cache = cooked_cache_dir(&tmp, platform);
        std::fs::create_dir_all(&cache).expect("cache");
        let id = AssetId::new_random();
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: regia_core::id::TypePathHash::of("other_crate::Payload"),
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: 3,
        };
        let file = encode_cooked_file(&header, b"abc").expect("encode");
        std::fs::write(cache.join(cooked_file_name(id)), &file).expect("write");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: platform.to_string(),
            content_dir: None,
        })
        .expect("open");
        let (bytes, source) = server.read_cooked(id).expect("raw read");
        assert_eq!(bytes, file);
        assert_eq!(source, AssetSource::DevCache);
        assert!(server.read_cooked(AssetId::new_random()).is_err());
        server.track_external(id);
        assert_eq!(server.generation(id), 0);
        server.reload_id(id).expect("an external reload does not decode");
        server.reload_id(id).expect("again");
        assert_eq!(server.generation(id), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The one-pass merge keeps exactly what removing each entry's path and pushing it did.
    #[test]
    fn manifest_merge_matches_the_per_entry_override() {
        let entry = |path: &str, n: u128| ManifestEntry {
            path: path.to_string(),
            id: AssetId(regia_core::id::Guid::from_u128(n)),
            type_path: "t".to_string(),
        };
        let reference = |entries: &mut Vec<ManifestEntry>, pak: &[ManifestEntry]| {
            for e in pak {
                entries.retain(|old| old.path != e.path);
                entries.push(e.clone());
            }
        };
        let base = vec![entry("a", 1), entry("b", 2), entry("c", 3)];
        let paks = [
            vec![entry("b", 20), entry("d", 40)],
            vec![entry("d", 41), entry("a", 10), entry("d", 42)],
            vec![],
            vec![entry("e", 5), entry("e", 6), entry("c", 30)],
        ];
        let (mut fast, mut slow) = (base.clone(), base);
        for pak in &paks {
            merge_manifest(&mut fast, pak);
            reference(&mut slow, pak);
            assert_eq!(fast, slow);
        }
    }

    /// Build a minimal `.fpak` file in memory for testing.
    fn make_pak_bytes(entries: &[(AssetId, Vec<u8>)], manifest: &[ManifestEntry]) -> Vec<u8> {
        make_pak_bytes_with_deps(entries, manifest, &[])
    }

    /// [`make_pak_bytes`] with a TOC dependency list (spec 12 § 5).
    fn make_pak_bytes_with_deps(
        entries: &[(AssetId, Vec<u8>)],
        manifest: &[ManifestEntry],
        dependencies: &[String],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        let mut toc_entries = Vec::new();

        // Header placeholder.
        data.extend_from_slice(&[0u8; 60]);

        for (id, payload) in entries {
            let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
            if rem != 0 {
                data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
            }
            let offset = data.len() as u64;
            let size = payload.len() as u64;
            data.extend_from_slice(payload);
            toc_entries.push(PakEntry {
                id: *id,
                type_tag: regia_core::id::TypePathHash(0),
                offset,
                size,
                compressed_size: size,
                block_table_offset: 0,
            });
        }

        let toc_offset = data.len() as u64;
        let toc = PakToc {
            entries: toc_entries,
            manifest: manifest.to_vec(),
            dependencies: dependencies.to_vec(),
        };
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc encode");
        let toc_len = toc_bytes.len() as u64;
        data.extend_from_slice(&toc_bytes);

        let content_hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
        let header = encode_pak_header(FPAK_VERSION, 0, toc_offset, toc_len, &content_hash);
        data[..60].copy_from_slice(&header);

        data
    }

    #[test]
    fn reloading_a_pak_only_image_does_not_need_a_cache_file() {
        let tmp = std::env::temp_dir().join(format!("regia_pak_image_reload_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let id = AssetId::new_random();
        let image = CookedImage {
            width: 1,
            height: 1,
            rgba8: vec![1, 2, 3, 4],
        };
        let payload = image.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::image().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let pak_bytes = make_pak_bytes(&[(id, file)], &[]);
        std::fs::write(content.join("base.fpak"), &pak_bytes).expect("write pak");
        std::fs::create_dir_all(cooked_cache_dir(&tmp, "linux")).expect("cache");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content),
        })
        .expect("open");
        server.load_image(id).expect("load from the pak");
        let reloaded = server.reload_id(id).expect("reload reads the pak");
        assert_eq!(reloaded, Some(LoadedKind::Image));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A pushed asset (`assets.push`, ADR-7836) is served from the cache over the pak that holds
    /// its id: the load, the reload and every payload reader see the pushed bytes.
    #[test]
    fn a_pushed_asset_wins_over_the_pak_that_holds_its_id() {
        let tmp = regia_test_utils::unique_temp_dir("asset-push-over-pak");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let id = AssetId::new_random();
        let cooked = |rgba8: Vec<u8>| {
            let payload = CookedImage {
                width: 1,
                height: 1,
                rgba8,
            }
            .to_postcard()
            .expect("postcard");
            let header = CookedHeader {
                format_version: COOKED_FORMAT_VERSION,
                type_tag: AssetTypeTag::image().0,
                asset_id: id,
                importer_version: pack_importer_version(1, 0, 0),
                dependencies: vec![],
                payload_len: payload.len() as u64,
            };
            encode_cooked_file(&header, &payload).expect("encode")
        };
        std::fs::write(
            content.join("base.fpak"),
            make_pak_bytes(&[(id, cooked(vec![1, 2, 3, 4]))], &[]),
        )
        .expect("write pak");
        std::fs::create_dir_all(cooked_cache_dir(&tmp, "linux")).expect("cache");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content),
        })
        .expect("open");
        let handle = server.load_image(id).expect("load from the pak");
        assert_eq!(
            server.images().get(&handle).map(|i| i.rgba8.clone()),
            Some(vec![1, 2, 3, 4])
        );
        let reader = server.payload_reader();

        let pushed = cooked(vec![9, 8, 7, 6]);
        let outcome = server.accept_push(id, &pushed).expect("push");
        assert!(outcome.changed);
        assert_eq!(outcome.path, server.cache_dir().join(cooked_file_name(id)));
        assert!(server.is_pushed(id));
        assert_eq!(server.reload_id(id).expect("reload"), Some(LoadedKind::Image));
        assert_eq!(
            server.images().get(&handle).map(|i| i.rgba8.clone()),
            Some(vec![9, 8, 7, 6]),
            "the reload serves the pushed bytes, not the pak's"
        );
        let (bytes, source) = reader.read(id).expect("payload reader");
        assert_eq!(
            source,
            AssetSource::DevCache,
            "a reader made before the push sees it"
        );
        assert_eq!(&bytes[..], &pushed[..]);
        assert_eq!(server.pushed_count(), 1);

        let bad = server.accept_source_push("assets/../Regia.toml", b"x");
        assert!(matches!(bad, Err(crate::push::PushError::BadPath(_))));
        assert!(!tmp.join("Regia.toml").exists());
        let ui = server
            .accept_source_push("assets/ui/hud.ui.ron", b"(root: ())")
            .expect("source push");
        assert_eq!(ui.path, tmp.join("assets").join("ui").join("hud.ui.ron"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn refresh_does_not_undo_an_eviction() {
        use crate::load_set::{LoadKind, LoadSet, LoadSets};
        let tmp = std::env::temp_dir().join(format!("regia_evict_refresh_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0]; 3],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let payload = mesh.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let manifest = vec![ManifestEntry {
            path: "meshes/tri.mesh".to_string(),
            id,
            type_path: "regia_asset::CpuMesh".to_string(),
        }];
        std::fs::write(
            content.join("base.fpak"),
            make_pak_bytes(&[(id, file)], &manifest),
        )
        .expect("pak");
        std::fs::create_dir_all(cooked_cache_dir(&tmp, "linux")).expect("cache");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content),
        })
        .expect("open");
        server.load_mesh_path("meshes/tri.mesh").expect("load");

        let sets = LoadSets::default();
        sets.define(LoadSet::new("s").with(LoadKind::Mesh, "meshes/tri.mesh", 0, 1));
        sets.refresh_from_assets(&server);
        assert!(sets.item_resident(LoadKind::Mesh, "meshes/tri.mesh"));
        sets.mark_evicted(LoadKind::Mesh, "meshes/tri.mesh");
        sets.refresh_from_assets(&server);
        assert!(!sets.item_resident(LoadKind::Mesh, "meshes/tri.mesh"));
        sets.mark_resident(LoadKind::Mesh, "meshes/tri.mesh");
        assert!(sets.item_resident(LoadKind::Mesh, "meshes/tri.mesh"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn load_mesh_from_pak() {
        let tmp = std::env::temp_dir().join(format!("regia_pak_load_test_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");

        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let mesh_payload = mesh.to_postcard().expect("postcard");

        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: mesh_payload.len() as u64,
        };
        let cooked_file = encode_cooked_file(&header, &mesh_payload).expect("encode");

        let manifest_entries = vec![ManifestEntry {
            path: "meshes/tri.mesh".to_string(),
            id,
            type_path: "regia_asset::CpuMesh".to_string(),
        }];

        let pak_bytes = make_pak_bytes(&[(id, cooked_file)], &manifest_entries);
        std::fs::write(content.join("base.fpak"), &pak_bytes).expect("write pak");

        // Also create a minimal cache dir so open() doesn't fail.
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        assert!(server.has_paks());
        assert_eq!(server.pak_count(), 1);

        // Load via path → should resolve from pak.
        let h = server.load_mesh_path("meshes/tri.mesh").expect("load");
        let m = server.meshes().get(&h).expect("get");
        assert_eq!(m.indices.len(), 3);
        assert_eq!(server.source_of(id), AssetSource::Pak("base".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A cooked texture loaded from a pak entry stored uncompressed is a view of the mapped pak
    /// (no copy), and it stays valid when the pak file is replaced by rename (a re-cook) or
    /// deleted, because the view holds the reader.
    #[test]
    fn pak_texture_is_served_from_the_mapping_and_outlives_a_replaced_pak() {
        let tmp = std::env::temp_dir().join(format!("regia_pak_tex_test_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        std::fs::create_dir_all(cooked_cache_dir(&tmp, "linux")).expect("cache");
        let id = AssetId::new_random();
        let ktx2 = crate::cooked::tests::minimal_ktx2();
        let texture = CookedTexture {
            width: 4,
            height: 4,
            usage: crate::cooked::TextureCookUsage::ColorSrgb,
            ktx2: ktx2.clone().into(),
        };
        let payload = texture.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::texture().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("encode");
        let manifest = vec![ManifestEntry {
            path: "textures/t.ktx2".to_string(),
            id,
            type_path: "regia_asset::CookedTexture".to_string(),
        }];
        let pak = content.join("base.fpak");
        std::fs::write(&pak, make_pak_bytes(&[(id, file)], &manifest)).expect("write pak");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");
        // First launch after an update: until `pak_verify` passes, the gate is closed and the
        // texture is a copy read through the file handle.
        server.mounted_paks[0].reader.set_zero_copy(false);
        let handle = server.load_texture(id).expect("load");
        let copied = server.textures().get(&handle).expect("texture").clone();
        assert_eq!(copied.ktx2, ktx2);
        assert!(!copied.ktx2.is_mapped(), "an unverified pak serves copies");
        // Verified: the same load is a view of the mapping.
        server.mounted_paks[0].reader.zero_copy_gate().open();
        let handle = server.load_texture(id).expect("load");
        assert_eq!(server.source_of(id), AssetSource::Pak("base".to_string()));
        let loaded = server.textures().get(&handle).expect("texture").clone();
        assert_eq!(loaded.ktx2, ktx2);
        let reader = &server.mounted_paks[0].reader;
        let entry = reader.read_raw(id).expect("raw").as_ptr_range();
        assert!(
            entry.contains(&loaded.ktx2.as_ptr()),
            "the KTX2 bytes point into the pak entry, not a heap copy"
        );
        assert_eq!(loaded.ktx2.is_mapped(), reader.is_mapped());

        // The texture outlives the server (its mounts) and, on Unix, a pak replaced by rename and
        // then deleted (Windows may refuse to replace a mapped file: an OS property).
        drop(server);
        #[cfg(unix)]
        {
            let next = content.join("base.fpak.tmp");
            std::fs::write(&next, vec![0u8; 8192]).expect("write");
            std::fs::rename(&next, &pak).expect("rename over");
            std::fs::remove_file(&pak).expect("remove");
        }
        assert_eq!(loaded.ktx2, ktx2);
        assert!(loaded.validate().is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pak_override_order_later_pak_wins() {
        let tmp = std::env::temp_dir().join(format!("regia_pak_override_test_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");

        let id = AssetId::new_random();

        // Pak 1 (earlier): a single triangle.
        let mesh1 = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let payload1 = mesh1.to_postcard().expect("postcard");
        let header1 = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload1.len() as u64,
        };
        let cooked1 = encode_cooked_file(&header1, &payload1).expect("encode");

        // Pak 2 (later): a quad (6 indices).
        let mesh2 = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 1, 3, 2],
            ..Default::default()
        };
        let payload2 = mesh2.to_postcard().expect("postcard");
        let header2 = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload2.len() as u64,
        };
        let cooked2 = encode_cooked_file(&header2, &payload2).expect("encode");

        let manifest_entries = vec![ManifestEntry {
            path: "meshes/test.mesh".to_string(),
            id,
            type_path: "regia_asset::CpuMesh".to_string(),
        }];

        // Write paks: "01_base.fpak" < "02_patch.fpak" alphabetically.
        let pak1_bytes = make_pak_bytes(&[(id, cooked1)], &manifest_entries);
        std::fs::write(content.join("01_base.fpak"), &pak1_bytes).expect("write pak1");
        let pak2_bytes = make_pak_bytes(&[(id, cooked2)], &manifest_entries);
        std::fs::write(content.join("02_patch.fpak"), &pak2_bytes).expect("write pak2");

        // Create minimal cache.
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        assert_eq!(server.pak_count(), 2);

        let h = server.load_mesh_path("meshes/test.mesh").expect("load");
        let m = server.meshes().get(&h).expect("get");
        // Should get the quad from the later pak (02_patch).
        assert_eq!(m.indices.len(), 6, "later pak must override earlier");
        assert_eq!(server.source_of(id), AssetSource::Pak("02_patch".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn toc_dependencies_mount_before_dependents() {
        let tmp = regia_test_utils::unique_temp_dir("pak-toc-deps");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id = AssetId::new_random();
        let base = make_pak_bytes(&[(id, vec![1, 2, 3])], &[]);
        std::fs::write(content.join("z_base.fpak"), &base).expect("write base");
        // The patch sorts BEFORE its base by file name, but declares it in the TOC dependencies.
        let patch = make_pak_bytes_with_deps(&[(id, vec![4, 5, 6])], &[], &["z_base.fpak".to_string()]);
        std::fs::write(content.join("a_patch.fpak"), &patch).expect("write patch");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        assert_eq!(
            server.mounted_pak_names(),
            vec!["z_base".to_string(), "a_patch".to_string()],
            "spec 12 § 5: a pak's TOC dependencies mount before it, whatever the file-name order"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn toc_dependency_cycle_still_mounts_everything() {
        let tmp = regia_test_utils::unique_temp_dir("pak-toc-cycle");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id_a = AssetId::new_random();
        let id_b = AssetId::new_random();
        // A circular dependency pair: both mount, in file-name order, without hanging.
        let a = make_pak_bytes_with_deps(&[(id_a, vec![1])], &[], &["b.fpak".to_string()]);
        std::fs::write(content.join("a.fpak"), &a).expect("write a");
        let b = make_pak_bytes_with_deps(&[(id_b, vec![2])], &[], &["a.fpak".to_string()]);
        std::fs::write(content.join("b.fpak"), &b).expect("write b");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        assert_eq!(server.pak_count(), 2);
        assert_eq!(
            server.mounted_pak_names(),
            vec!["a".to_string(), "b".to_string()],
            "a dependency cycle falls back to the file-name order"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Rev-kimi-28 (audio-k item 1): two mounted paks can share a file name (a runtime chunk
    /// pak from a downloads dir next to a content pak of the same name). A TOC dependency entry
    /// matching the shared name binds to EVERY pak with that name, so the dependent mounts after
    /// all of them: a patch that names its base still overrides a same-named downloaded copy.
    /// (Dropping the edge mounted the patch first, so both bases overrode its fixes.)
    #[test]
    fn toc_dependency_on_a_duplicated_pak_name_binds_to_every_match() {
        let files = ["a.fpak".to_string(), "b.fpak".to_string(), "b.fpak".to_string()];
        let dep = ["b.fpak".to_string()];
        let deps: Vec<&[String]> = vec![&dep, &[], &[]];
        let (order, cycles) = toc_dependency_order(&files, &deps);
        assert_eq!(order, vec![1, 2, 0], "the dependent mounts after both `b.fpak`s");
        assert!(cycles.is_empty());

        // The same for a duplicated file stem ("b" below matches `b.fpak` and `b.pak`).
        let files = ["z.fpak".to_string(), "b.fpak".to_string(), "b.pak".to_string()];
        let dep = ["b".to_string()];
        let deps: Vec<&[String]> = vec![&dep, &[], &[]];
        let (order, cycles) = toc_dependency_order(&files, &deps);
        assert_eq!(order, vec![1, 2, 0], "the dependent mounts after both `b` stems");
        assert!(cycles.is_empty());

        // A pak never depends on itself: a downloaded `b.fpak` that names `b.fpak` mounts after
        // the OTHER `b.fpak` (a same-named patch of a content pak).
        let files = ["b.fpak".to_string(), "b.fpak".to_string()];
        let dep = ["b.fpak".to_string()];
        let deps: Vec<&[String]> = vec![&dep, &[]];
        let (order, cycles) = toc_dependency_order(&files, &deps);
        assert_eq!(order, vec![1, 0]);
        assert!(cycles.is_empty());
    }

    /// The same rule end to end: a content `base.fpak`, a content `a_patch.fpak` that depends on
    /// `base.fpak`, and a downloaded chunk whose file is also `base.fpak`. The patch mounts after
    /// both bases, so its bytes win for the id all three carry.
    #[test]
    fn a_patch_mounts_after_every_pak_named_like_its_base() {
        let tmp = regia_test_utils::unique_temp_dir("pak-toc-dup-base");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id = AssetId::new_random();
        let base = make_pak_bytes(&[(id, vec![1, 2, 3])], &[]);
        std::fs::write(content.join("base.fpak"), &base).expect("write base");
        let patch = make_pak_bytes_with_deps(&[(id, vec![4, 5, 6])], &[], &["base.fpak".to_string()]);
        std::fs::write(content.join("a_patch.fpak"), &patch).expect("write patch");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");
        assert_eq!(
            server.mounted_pak_names(),
            vec!["base".to_string(), "a_patch".to_string()]
        );

        let downloads = tmp.join("downloads");
        std::fs::create_dir_all(&downloads).expect("downloads");
        let dlc = make_pak_bytes(&[(id, vec![7, 8, 9])], &[]);
        std::fs::write(downloads.join("base.fpak"), &dlc).expect("write dlc base");
        server
            .mount_chunk_pak(&downloads.join("base.fpak"), "dlc_base", None)
            .expect("mount the downloaded base");
        assert_eq!(
            server.mounted_pak_names(),
            vec!["base".to_string(), "dlc_base".to_string(), "a_patch".to_string()],
            "spec 12 § 5: the patch mounts after every pak named like its base"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn toc_dependency_on_a_missing_pak_is_ignored() {
        let tmp = regia_test_utils::unique_temp_dir("pak-toc-missing");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id = AssetId::new_random();
        let pak = make_pak_bytes_with_deps(&[(id, vec![1, 2, 3])], &[], &["ghost.fpak".to_string()]);
        std::fs::write(content.join("base.fpak"), &pak).expect("write pak");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        assert_eq!(server.pak_count(), 1);
        assert_eq!(server.mounted_pak_names(), vec!["base".to_string()]);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Review rev-kimi-9 finding 4: a dependency cycle falls back to the incoming order for the
    /// paks IN the cycle only. A pak that merely depends on a cycle member still mounts after it.
    #[test]
    fn toc_dependency_cycle_does_not_pull_an_outside_dependent_ahead() {
        let tmp = regia_test_utils::unique_temp_dir("pak-toc-cycle-outside");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id = AssetId::new_random();
        // `0_patch` -> `x`, and `x` <-> `y` form a cycle. The patch sorts first by file name.
        let patch = make_pak_bytes_with_deps(&[(id, vec![9])], &[], &["x.fpak".to_string()]);
        std::fs::write(content.join("0_patch.fpak"), &patch).expect("write patch");
        let x = make_pak_bytes_with_deps(&[(id, vec![1])], &[], &["y.fpak".to_string()]);
        std::fs::write(content.join("x.fpak"), &x).expect("write x");
        let y = make_pak_bytes_with_deps(&[(AssetId::new_random(), vec![2])], &[], &["x.fpak".to_string()]);
        std::fs::write(content.join("y.fpak"), &y).expect("write y");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        assert_eq!(
            server.mounted_pak_names(),
            vec!["x".to_string(), "y".to_string(), "0_patch".to_string()],
            "the cycle mounts in file-name order, and the patch still mounts after its dependency"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Review rev-kimi-9 finding 7: a chunk mounted at runtime (a download or a remount) keeps
    /// the TOC dependency order the startup mount established.
    #[test]
    fn a_chunk_mounted_later_keeps_the_toc_dependency_order() {
        let tmp = regia_test_utils::unique_temp_dir("pak-toc-runtime-mount");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id = AssetId::new_random();
        let base = make_pak_bytes(&[(id, vec![1, 2, 3])], &[]);
        std::fs::write(content.join("z_base.fpak"), &base).expect("write base");
        let patch = make_pak_bytes_with_deps(&[(id, vec![4, 5, 6])], &[], &["z_base.fpak".to_string()]);
        std::fs::write(content.join("a_patch.fpak"), &patch).expect("write patch");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");
        assert_eq!(
            server.mounted_pak_names(),
            vec!["z_base".to_string(), "a_patch".to_string()]
        );

        // A downloaded chunk that depends on the base: it sorts before `z_base` by name.
        let later_dir = tmp.join("downloads");
        std::fs::create_dir_all(&later_dir).expect("downloads");
        let m_patch = make_pak_bytes_with_deps(&[(id, vec![7])], &[], &["z_base.fpak".to_string()]);
        std::fs::write(later_dir.join("m_patch.fpak"), &m_patch).expect("write m_patch");
        server
            .mount_chunk_pak(&later_dir.join("m_patch.fpak"), "m_patch", None)
            .expect("mount m_patch");
        let names = server.mounted_pak_names();
        let pos = |n: &str| names.iter().position(|m| m == n).expect("mounted");
        assert!(
            pos("z_base") < pos("m_patch"),
            "a later chunk mounts after its TOC dependency: {names:?}"
        );
        assert!(pos("z_base") < pos("a_patch"), "{names:?}");

        // A remount of the base keeps it under every pak that depends on it.
        server
            .mount_chunk_pak(&content.join("z_base.fpak"), "z_base", None)
            .expect("remount base");
        let names = server.mounted_pak_names();
        let pos = |n: &str| names.iter().position(|m| m == n).expect("mounted");
        assert!(
            pos("z_base") < pos("a_patch"),
            "a remounted base stays under its patch: {names:?}"
        );
        assert!(pos("z_base") < pos("m_patch"), "{names:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn without_content_dir_no_paks_are_mounted() {
        let tmp = std::env::temp_dir().join(format!("regia_no_pak_test_{}", uuid::Uuid::new_v4()));
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");

        assert!(!server.has_paks());
        assert_eq!(server.pak_count(), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pak_manifest_merges_with_server_manifest() {
        let tmp = std::env::temp_dir().join(format!("regia_pak_manifest_test_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");

        let id = AssetId::new_random();
        let payload = vec![1u8, 2, 3];
        let manifest_entries = vec![ManifestEntry {
            path: "levels/test.level.ron".to_string(),
            id,
            type_path: "regia_scene::Level".to_string(),
        }];

        let pak_bytes = make_pak_bytes(&[(id, payload)], &manifest_entries);
        std::fs::write(content.join("base.fpak"), &pak_bytes).expect("write pak");

        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content),
        })
        .expect("open");

        // The manifest should now contain the pak's entry.
        let found = server.id_for_path("levels/test.level.ron");
        assert_eq!(found, Some(id));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn localized_variants_resolve_through_the_chain_and_report_switches() {
        let tmp = regia_test_utils::unique_temp_dir("asset-localized");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let entry = |path: &str, type_path: &str| ManifestEntry {
            path: path.to_string(),
            id: AssetId::new_random(),
            type_path: type_path.to_string(),
        };
        let entries = vec![
            entry("ui/sign.png", "regia_asset::CookedTexture"),
            entry("localized/fr/ui/sign.png", "regia_asset::CookedTexture"),
            entry("localized/de/ui/sign.png", "regia_asset::CpuMesh"),
            entry("audio/hello.ogg", "regia_audio::Sound"),
            entry("localized/ja/audio/hello.ogg", "regia_audio::Sound"),
        ];
        let payloads: Vec<(AssetId, Vec<u8>)> = entries.iter().map(|e| (e.id, vec![0u8])).collect();
        std::fs::write(content.join("base.fpak"), make_pak_bytes(&payloads, &entries)).expect("pak");
        std::fs::create_dir_all(cooked_cache_dir(&tmp, "linux")).expect("cache");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content),
        })
        .expect("open");
        use crate::localized::{LocaleChains, LocaleGroup};
        let chains = |text: &[&str], audio: &[&str]| LocaleChains {
            text: text.iter().map(|t| (*t).to_owned()).collect(),
            audio: audio.iter().map(|t| (*t).to_owned()).collect(),
        };
        server.set_locale_chains(chains(&["fr-CA", "fr", "en"], &["fr-CA", "fr", "en"]));
        assert_eq!(
            server.resolve_localized("ui/sign.png", LocaleGroup::Text),
            "localized/fr/ui/sign.png"
        );
        assert_eq!(
            server.resolve_localized("audio/hello.ogg", LocaleGroup::Audio),
            "audio/hello.ogg"
        );
        assert!(server.reload_localized().is_empty(), "nothing moved");
        // Text to German (its variant is a mesh: ignored), audio split to Japanese.
        server.set_locale_chains(chains(&["de", "en"], &["ja", "en"]));
        let moved = server.reload_localized();
        assert_eq!(moved.len(), 2, "{moved:?}");
        assert_eq!(moved[0].rel, "audio/hello.ogg");
        assert_eq!(moved[0].to, "localized/ja/audio/hello.ogg");
        assert_eq!(moved[0].group, LocaleGroup::Audio);
        assert_eq!(
            (moved[1].from.as_str(), moved[1].to.as_str()),
            ("localized/fr/ui/sign.png", "ui/sign.png")
        );
        assert!(
            server.id_for_path(&moved[0].to).is_some(),
            "the new path is loadable"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn localized_refresh_tracks_manifest_history_and_keeps_pak_and_explicit_precedence() {
        let tmp = regia_test_utils::unique_temp_dir("asset-localized-refresh-history");
        let content = tmp.join("content");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&content).expect("content");
        std::fs::create_dir_all(&cache).expect("cache");
        let entry = |path: &str, type_path: &str| ManifestEntry {
            path: path.to_owned(),
            id: AssetId::new_random(),
            type_path: type_path.to_owned(),
        };
        let base = entry("ui/sign.png", "regia_asset::CookedTexture");
        let icon_base = entry("ui/icon.png", "regia_asset::CookedTexture");
        let voice = entry("audio/hello.ogg", "regia_audio::Sound");
        let pinned_audio = entry("localized/ja/audio/hello.ogg", "regia_audio::Sound");
        let explicit = entry("localized/fr/ui/explicit.png", "regia_asset::CookedTexture");
        let pak_shadow = entry("localized/fr/ui/sign.png", "regia_asset::CookedTexture");
        let pak_rows = vec![pak_shadow.clone()];
        std::fs::write(
            content.join("base.fpak"),
            make_pak_bytes(&[(pak_shadow.id, vec![0])], &pak_rows),
        )
        .expect("pak");

        let write_manifest = |rows: Vec<ManifestEntry>| {
            AssetManifest { entries: rows }
                .write_to_cache(&cache)
                .expect("write manifest");
        };
        write_manifest(vec![
            base.clone(),
            icon_base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
        ]);
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_owned(),
            content_dir: Some(content),
        })
        .expect("open");
        use crate::localized::{LocaleChains, LocaleGroup, LocalizedAssetChanged};
        let chains = |text: &[&str], audio: &[&str]| LocaleChains {
            text: text.iter().map(|tag| (*tag).to_owned()).collect(),
            audio: audio.iter().map(|tag| (*tag).to_owned()).collect(),
        };
        server.set_locale_chains(chains(&["fr-CA", "fr"], &["ja"]));
        assert_eq!(
            server.resolve_localized("ui/sign.png", LocaleGroup::Text),
            "localized/fr/ui/sign.png"
        );
        assert_eq!(
            server.resolve_localized("ui/icon.png", LocaleGroup::Text),
            "ui/icon.png"
        );
        assert_eq!(
            server.resolve_localized("audio/hello.ogg", LocaleGroup::Audio),
            "localized/ja/audio/hello.ogg"
        );
        assert_eq!(
            server.resolve_localized("localized/fr/ui/explicit.png", LocaleGroup::Text),
            "localized/fr/ui/explicit.png"
        );
        assert!(server.reload_localized().is_empty());

        let variant = entry("localized/fr/ui/sign.png", "regia_asset::CookedTexture");
        let icon_variant = entry("localized/fr-CA/ui/icon.png", "regia_asset::CookedTexture");
        write_manifest(vec![
            base.clone(),
            icon_base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
            variant.clone(),
        ]);
        server.refresh_manifest();
        assert!(
            server.reload_localized().is_empty(),
            "same-path manifest addition is not an effective resolution change"
        );
        assert!(
            server.reload_localized().is_empty(),
            "repeated reload is idempotent"
        );

        // Replacing the dev row with a wrong type cannot displace the same-path mounted pak.
        let wrong = entry("localized/fr/ui/sign.png", "regia_asset::CpuMesh");
        write_manifest(vec![
            base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
            wrong,
        ]);
        server.refresh_manifest();
        assert!(
            server.reload_localized().is_empty(),
            "the pak shadow retains precedence"
        );

        write_manifest(vec![
            base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
        ]);
        server.refresh_manifest();
        assert!(
            server.reload_localized().is_empty(),
            "removing the shadowed dev row changes nothing"
        );

        // An unshadowed, more-specific path traverses add, wrong type, removal and restore.
        let extra = entry("localized/fr-CA/ui/sign.png", "regia_asset::CookedTexture");
        write_manifest(vec![
            base.clone(),
            icon_base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
            extra.clone(),
            icon_variant.clone(),
        ]);
        server.refresh_manifest();
        assert_eq!(
            server.reload_localized(),
            vec![
                LocalizedAssetChanged {
                    rel: "ui/icon.png".to_owned(),
                    group: LocaleGroup::Text,
                    from: "ui/icon.png".to_owned(),
                    to: "localized/fr-CA/ui/icon.png".to_owned(),
                },
                LocalizedAssetChanged {
                    rel: "ui/sign.png".to_owned(),
                    group: LocaleGroup::Text,
                    from: "localized/fr/ui/sign.png".to_owned(),
                    to: "localized/fr-CA/ui/sign.png".to_owned(),
                },
            ]
        );
        assert!(server.reload_localized().is_empty());
        let wrong_extra = entry("localized/fr-CA/ui/sign.png", "regia_asset::CpuMesh");
        write_manifest(vec![
            base.clone(),
            icon_base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
            wrong_extra,
            icon_variant.clone(),
        ]);
        server.refresh_manifest();
        assert_eq!(
            server.reload_localized(),
            vec![LocalizedAssetChanged {
                rel: "ui/sign.png".to_owned(),
                group: LocaleGroup::Text,
                from: "localized/fr-CA/ui/sign.png".to_owned(),
                to: "localized/fr/ui/sign.png".to_owned(),
            }]
        );
        write_manifest(vec![
            base.clone(),
            icon_base.clone(),
            voice.clone(),
            pinned_audio.clone(),
            explicit.clone(),
            icon_variant.clone(),
        ]);
        server.refresh_manifest();
        assert!(
            server.reload_localized().is_empty(),
            "removing the invalid candidate keeps fallback"
        );
        write_manifest(vec![
            base,
            icon_base,
            voice,
            pinned_audio,
            explicit,
            extra,
            icon_variant,
        ]);
        server.refresh_manifest();
        assert_eq!(
            server.reload_localized(),
            vec![LocalizedAssetChanged {
                rel: "ui/sign.png".to_owned(),
                group: LocaleGroup::Text,
                from: "localized/fr/ui/sign.png".to_owned(),
                to: "localized/fr-CA/ui/sign.png".to_owned(),
            }]
        );
        assert_eq!(
            server.resolve_localized("localized/fr/ui/explicit.png", LocaleGroup::Text),
            "localized/fr/ui/explicit.png"
        );
        assert_eq!(
            server.resolve_localized("audio/hello.ogg", LocaleGroup::Audio),
            "localized/ja/audio/hello.ogg"
        );
        server.set_locale_chains(chains(&["fr"], &["ja"]));
        assert_eq!(
            server.reload_localized(),
            vec![
                LocalizedAssetChanged {
                    rel: "ui/icon.png".to_owned(),
                    group: LocaleGroup::Text,
                    from: "localized/fr-CA/ui/icon.png".to_owned(),
                    to: "ui/icon.png".to_owned(),
                },
                LocalizedAssetChanged {
                    rel: "ui/sign.png".to_owned(),
                    group: LocaleGroup::Text,
                    from: "localized/fr-CA/ui/sign.png".to_owned(),
                    to: "localized/fr/ui/sign.png".to_owned(),
                },
            ],
            "changing only the text chain leaves the pinned audio path untouched"
        );
        assert!(server.reload_localized().is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pak_manifest_duplicate_paths_keep_last_row_once() {
        let tmp = regia_test_utils::unique_temp_dir("pak_manifest_dup");
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let (first, second) = (AssetId::new_random(), AssetId::new_random());
        let row = |id| ManifestEntry {
            path: "levels/dup.level.ron".to_string(),
            id,
            type_path: "regia_scene::Level".to_string(),
        };
        let manifest_entries = vec![row(first), row(second)];
        let pak_bytes = make_pak_bytes(&[(second, vec![1u8, 2, 3])], &manifest_entries);
        std::fs::write(content.join("base.fpak"), &pak_bytes).expect("write pak");
        std::fs::create_dir_all(cooked_cache_dir(&tmp, "linux")).expect("cache");

        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content),
        })
        .expect("open");

        assert_eq!(server.id_for_path("levels/dup.level.ron"), Some(second));
        let count = server
            .manifest
            .entries
            .iter()
            .filter(|e| e.path == "levels/dup.level.ron")
            .count();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A manifest that lists one path twice keeps the earlier id, including through the path index
    /// and a backslash query.
    #[test]
    fn a_duplicated_manifest_path_keeps_the_first_id() {
        let tmp = regia_test_utils::unique_temp_dir("manifest_first_id");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        let first = AssetId::new_random();
        let second = AssetId::new_random();
        let row = |id| ManifestEntry {
            path: "maps/a.fgiv".to_string(),
            id,
            type_path: "regia_asset::baked_gi::BakedGiVolume".to_string(),
        };
        let manifest = AssetManifest {
            entries: vec![row(first), row(second)],
        };
        manifest.write_to_cache(&cache).expect("manifest");
        let server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");
        assert_eq!(server.id_for_path("maps/a.fgiv"), Some(first));
        assert_eq!(server.id_for_path(r"maps\a.fgiv"), Some(first));
        assert_eq!(server.manifest.id_for_path("maps/a.fgiv"), Some(first));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn pak_contents_byte_identical_to_dev_cache() {
        // When the same cooked payload is stored in both the pak and dev cache,
        // loading from the pak should produce the same bytes.
        let tmp = std::env::temp_dir().join(format!("regia_byte_id_test_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");

        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let mesh_payload = mesh.to_postcard().expect("postcard");

        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: mesh_payload.len() as u64,
        };
        let cooked_file = encode_cooked_file(&header, &mesh_payload).expect("encode");

        // Write to dev cache.
        std::fs::write(cache.join(cooked_file_name(id)), &cooked_file).expect("write");

        // Write to pak.
        let manifest_entries = vec![ManifestEntry {
            path: "meshes/tri.mesh".to_string(),
            id,
            type_path: "regia_asset::CpuMesh".to_string(),
        }];
        let pak_bytes = make_pak_bytes(&[(id, cooked_file.clone())], &manifest_entries);
        std::fs::write(content.join("base.fpak"), &pak_bytes).expect("write pak");

        // Server with content_dir (paks mounted) → load from pak.
        let mut server_pak = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");

        let h_pak = server_pak.load_mesh(id).expect("load from pak");
        let mesh_from_pak = server_pak.meshes().get(&h_pak).expect("get");
        assert_eq!(server_pak.source_of(id), AssetSource::Pak("base".to_string()));

        // Server without content_dir → load from dev cache.
        let mut server_cache = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");

        let h_cache = server_cache.load_mesh(id).expect("load from cache");
        let mesh_from_cache = server_cache.meshes().get(&h_cache).expect("get");
        assert_eq!(server_cache.source_of(id), AssetSource::DevCache);

        // Byte-identical mesh data.
        assert_eq!(
            mesh_from_pak, mesh_from_cache,
            "pak and dev cache must produce identical asset data"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn content_dir_detected_next_to_executable() {
        let tmp = std::env::temp_dir().join(format!("regia_content_dir_detect_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("mkdir");

        // No content dir → None.
        assert_eq!(content_dir_at(&tmp), None);

        // Empty content dir → detected (the shipped layout), paks optional.
        std::fs::create_dir_all(tmp.join("content")).expect("content dir");
        assert_eq!(content_dir_at(&tmp), Some(tmp.join("content")));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn project_root_walks_up_to_regia_toml() {
        let tmp = std::env::temp_dir().join(format!("regia_project_root_detect_{}", uuid::Uuid::new_v4()));
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(tmp.join("Regia.toml"), "[project]\nname = \"x\"\n").expect("write");

        assert_eq!(project_root_from(&nested), Some(tmp.clone()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn project_root_walks_up_to_a_manifest_from_before_the_rename() {
        let tmp = std::env::temp_dir().join(format!("regia_project_root_legacy_{}", uuid::Uuid::new_v4()));
        let nested = tmp.join("launcher").join("src");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(
            tmp.join(regia_core::brand::LEGACY_MANIFEST_FILE),
            "[project]\nname = \"x\"\n",
        )
        .expect("write");

        assert_eq!(project_root_from(&nested), Some(tmp.clone()));
        assert_eq!(
            crate::cooked_cache_dir(&tmp, "linux"),
            tmp.join(regia_core::brand::LEGACY_PROJECT_STATE_DIR)
                .join("cooked")
                .join("linux"),
            "a project not upgraded yet keeps the state directory its .gitignore covers"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn default_settings_describe_the_current_platform() {
        let settings = AssetServerSettings::default();
        assert_eq!(settings.platform, current_platform());
        // The test binary has no `content/` next to it → dev mode.
        assert!(
            settings.content_dir.is_none(),
            "test binaries must not pick up a shipped content dir: {:?}",
            settings.content_dir
        );
    }

    #[test]
    fn probe_reports_pak_and_dev_cache_sources() {
        let tmp = std::env::temp_dir().join(format!("regia_probe_test_{}", uuid::Uuid::new_v4()));
        let content = tmp.join("content");
        std::fs::create_dir_all(&content).expect("mkdir");

        let id = AssetId::new_random();
        let mesh = CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2],
            ..Default::default()
        };
        let payload = mesh.to_postcard().expect("postcard");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: AssetTypeTag::mesh_legacy_cpu().0,
            asset_id: id,
            importer_version: pack_importer_version(1, 0, 0),
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let cooked_file = encode_cooked_file(&header, &payload).expect("encode");

        let manifest_entries = vec![ManifestEntry {
            path: "meshes/test.mesh".to_string(),
            id,
            type_path: "regia_asset::CpuMesh".to_string(),
        }];
        let pak_bytes = make_pak_bytes(&[(id, cooked_file.clone())], &manifest_entries);
        std::fs::write(content.join("base.fpak"), &pak_bytes).expect("write pak");

        // Same cooked file in the dev cache.
        let cache = cooked_cache_dir(&tmp, "linux");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::write(cache.join(cooked_file_name(id)), &cooked_file).expect("write cache");
        AssetManifest {
            entries: manifest_entries.clone(),
        }
        .write_to_cache(&cache)
        .expect("manifest");

        let mut server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: Some(content.clone()),
        })
        .expect("open");
        assert_eq!(
            server.probe("meshes/test.mesh").expect("probe from pak"),
            AssetSource::Pak("base".to_string())
        );

        let mut dev_server = AssetServer::open(AssetServerSettings {
            project_root: tmp.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");
        assert_eq!(
            dev_server.probe("meshes/test.mesh").expect("probe from cache"),
            AssetSource::DevCache
        );

        // Missing asset → error (no silent fallback).
        assert!(dev_server.probe("meshes/nope.mesh").is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The mount closes the zero-copy gate of a hashed pak this build has not verified and
    /// leaves it open for a recorded one, an unhashed one, or with the check off.
    #[test]
    fn mount_gates_zero_copy_until_the_pak_is_verified() {
        let tmp = std::env::temp_dir().join(format!("regia_asset_gate_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).expect("tempdir");
        let path = tmp.join("base.fpak");
        std::fs::write(&path, make_pak_bytes(&[], &[])).expect("write pak");
        let options = crate::pak_verify::PakVerifyOptions {
            record: Some(tmp.join("cache").join(crate::pak_verify::PAK_VERIFY_RECORD)),
            start_delay: std::time::Duration::ZERO,
            bytes_per_sec: 0,
            force: false,
        };

        let reader = PakReader::open(&path).expect("open");
        assert!(
            zero_copy_gate_at_mount(&reader, &path, None).is_none(),
            "check off"
        );
        assert!(reader.zero_copy());
        let gate = zero_copy_gate_at_mount(&reader, &path, Some(&options)).expect("unverified: gated");
        assert!(!reader.zero_copy() && !gate.is_open());

        let handle =
            crate::pak_verify::spawn_pak_verify_gated(vec![(path.clone(), Some(gate))], options.clone())
                .expect("thread");
        let (results, done) = handle.wait(std::time::Duration::from_secs(30));
        assert!(done);
        assert_eq!(results[0].1, crate::pak_verify::PakVerifyOutcome::Verified);
        assert!(reader.zero_copy(), "the check opened the gate");

        // The next launch: recorded, so the mount does not gate it.
        let again = PakReader::open(&path).expect("open");
        assert!(zero_copy_gate_at_mount(&again, &path, Some(&options)).is_none());
        assert!(again.zero_copy());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
