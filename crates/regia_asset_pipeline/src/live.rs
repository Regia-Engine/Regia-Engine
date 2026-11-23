// SPDX-License-Identifier: MPL-2.0
//! Live cook: re-cook changed sources in the background and tell the runtime
//! (`docs/specs/12-assets.md` § 6 "incremental via notify" and § 8).
//!
//! [`LiveCooker`] watches the project's `assets/` (and any extra roots, such as the engine's shader
//! directory in a dev checkout) with [`regia_asset::hot_reload::SourceWatcher`]. Each debounced
//! batch goes through [`affected_sources`] (only the changed assets and the sources whose cooked
//! output embeds them: a glTF's buffers and images, a terrain's inputs) and [`cook_sources`]. Every
//! cooked id is then sent as [`ReloadMessage::Reload`], every directly-read source as
//! [`ReloadMessage::Source`], and one [`ReloadMessage::Status`] per source carries the result for the
//! status line. Errors leave the previous cooked output in place.
//!
//! Desktop tools only (the editor and `regia run --dev`); never part of a shipped game.
//! `regia_asset::hot_reload` is not built for Android, so [`LiveCooker::spawn`] returns an error
//! there. [`affected_sources`] and [`cook_sources`] still run: a host can cook an Android pak
//! without a watcher.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(not(target_os = "android"))]
use std::time::Instant;

/// Quiet time before a saved source is re-cooked.
///
/// The same 120 ms as `regia_asset::hot_reload::ASSET_DEBOUNCE`. That module is desktop-only,
/// so the number lives here too and a desktop test keeps the two in lockstep.
const ASSET_DEBOUNCE: Duration = Duration::from_millis(120);

#[cfg(not(target_os = "android"))]
use regia_asset::hot_reload::SourceWatcher;
use regia_asset::{
    AssetId, AssetManifest, ManifestEntry, ReloadMessage, SourceKind, cooked_cache_dir, normalise_asset_path,
};
#[cfg(not(target_os = "android"))]
use regia_asset::{WatchEntry, WatchOutcome, display_path};
use tracing::instrument;
#[cfg(not(target_os = "android"))]
use tracing::{info, warn};

use crate::cook::{CookError, CookOptions, cook_one, is_terrain_layer, record_outcome};
use crate::cook_ledger::CookLedger;
use crate::db::AssetDatabase;

/// The result of re-cooking one source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCook {
    /// Absolute source path.
    pub source: PathBuf,
    /// Project-relative path (`assets/...`).
    pub rel_path: String,
    /// Kind of the source.
    pub kind: SourceKind,
    /// Every cooked id it produced (primary first, then sub-assets). Empty on error.
    pub ids: Vec<AssetId>,
    /// The cache key matched: nothing was rewritten.
    pub skipped: bool,
    /// The cook error, if any. The previous cooked output stays.
    pub error: Option<String>,
    /// Non-fatal cook warnings.
    pub warnings: Vec<String>,
}

/// The sources to re-cook for a set of changed paths: the cooked kinds themselves, a `.meta`'s
/// source, the `.gltf` files that reference a changed buffer or image, and the terrain layer
/// files whose inputs changed. Directly-read kinds (shaders, levels, …) and removed files are not
/// included.
#[must_use]
pub fn affected_sources(assets_root: &Path, platform: &str, changed: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    if changed.is_empty() {
        return out;
    }
    let sources = AssetDatabase::discover_sources(assets_root);
    // Both platforms cook terrain through the tiled path. `platform` stays in the signature.
    let _ = platform;
    let mut sidecar_owners: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for source in &sources {
        for sidecar in crate::cook::gltf_inputs(std::slice::from_ref(source)) {
            sidecar_owners.entry(sidecar).or_default().push(source.clone());
        }
    }
    // A channel-pack recipe re-cooks when a source image (or nested recipe) it reads changes
    // (ADR-3090).
    let mut pack_owners: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for source in sources.iter().filter(|s| crate::texture_pack::is_texpack(s)) {
        for input in crate::texture_pack::recipe_inputs(source) {
            pack_owners.entry(input).or_default().push(source.clone());
        }
    }
    let mut terrain_owners: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for terrain in crate::terrain_cook::discover_terrains(&sources) {
        if let Ok(files) = crate::terrain_cook::terrain_input_files(&terrain.source) {
            for file in files {
                terrain_owners
                    .entry(file)
                    .or_default()
                    .push(terrain.source.clone());
            }
        }
    }
    for path in changed {
        let path = match strip_meta_suffix(path) {
            Some(source) => source,
            None => path.clone(),
        };
        if !path.starts_with(assets_root) || !path.is_file() {
            continue;
        }
        // The recipes that read it; the file itself still cooks on its own below (unlike a
        // glTF's sidecars).
        if let Some(packs) = pack_owners.get(&path) {
            out.extend(packs.iter().cloned());
        }
        let owners: Vec<PathBuf> = sidecar_owners
            .get(&path)
            .into_iter()
            .chain(terrain_owners.get(&path))
            .flatten()
            .cloned()
            .collect();
        if !owners.is_empty() {
            out.extend(owners);
            if sidecar_owners.contains_key(&path)
                && !terrain_owners.contains_key(&path)
                && assets_root
                    .parent()
                    .is_some_and(|root| crate::gltf_texture_share::standalone_is_heightfield(root, &path))
            {
                out.insert(path);
            }
            continue;
        }
        if is_terrain_layer(&path) {
            out.insert(path);
            continue;
        }
        if cooks_to_fckd(&path) {
            out.insert(path);
        }
    }
    add_graph_dependents(&sources, changed, &mut out);
    out
}

/// True when `cook_project` cooks `path` to a `.fckd` blob: the kinds `SourceKind::is_cooked`
/// names, plus the 2D atlases, tilemaps (`.tilemap.ron`, Tiled `.tmx` / `.tmj`), baked GI
/// volumes (`.fgiv`), lightmaps (`.flmap`), colour LUTs (`.cube`) and data tables that `cook_one`
/// also handles.
fn cooks_to_fckd(path: &Path) -> bool {
    if SourceKind::of(path).is_cooked() {
        return true;
    }
    // Sounds cook to their loudness record (ADR-1855); the runtime still reads the file.
    if SourceKind::of(path) == SourceKind::Audio {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    name.ends_with(".atlas.ron")
        || name.ends_with(".tilemap.ron")
        || name.ends_with(".autotile.ron")
        || name.ends_with(".tmx")
        || name.ends_with(".tmj")
        || name.ends_with(".fgiv")
        || name.ends_with(".flmap")
        || name.ends_with(".cube")
        || name.ends_with(crate::texture_pack::TEXPACK_SUFFIX)
        || crate::data_cook::is_data_table_name(&name)
}

/// Adds the material graphs and instances whose cooked bytes embed a changed file: a graph that
/// calls a changed `.matfunc.ron` or samples a changed image, and the instances of a changed (or
/// re-cooked) graph. Matched by file name in the source text, a superset: a graph that only
/// mentions the name re-cooks to the same cache key and reports `skipped`.
fn add_graph_dependents(sources: &[PathBuf], changed: &[PathBuf], out: &mut BTreeSet<PathBuf>) {
    let is_graph = |p: &Path| {
        matches!(
            SourceKind::of(p),
            SourceKind::MaterialGraph | SourceKind::MaterialInstance
        )
    };
    let file_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    let mut names: Vec<String> = changed
        .iter()
        .filter(|p| {
            matches!(
                SourceKind::of(p),
                SourceKind::MaterialGraph | SourceKind::MaterialFunction | SourceKind::Texture
            )
        })
        .filter_map(|p| file_name(p))
        .collect();
    if names.is_empty() {
        return;
    }
    let texts: Vec<(&PathBuf, String)> = sources
        .iter()
        .filter(|p| is_graph(p))
        .filter_map(|p| std::fs::read_to_string(p).ok().map(|text| (p, text)))
        .collect();
    // Function → graph → instance: at most two hops, but loop until nothing new is added.
    loop {
        let mut added = Vec::new();
        for (path, text) in &texts {
            if !out.contains(*path) && names.iter().any(|name| text.contains(name.as_str())) {
                added.push((*path).clone());
            }
        }
        if added.is_empty() {
            return;
        }
        for path in added {
            names.extend(file_name(&path));
            out.insert(path);
        }
    }
}

/// Cooks exactly `targets` (absolute paths under `assets/`) incrementally and updates the asset
/// database and the cache manifest. Unchanged sources hit the cache and report `skipped`.
///
/// # Errors
///
/// The database or manifest could not be read or written. Per-source failures are reported in
/// [`SourceCook::error`].
pub fn cook_sources(
    options: &CookOptions,
    targets: &BTreeSet<PathBuf>,
) -> Result<Vec<SourceCook>, CookError> {
    cook_sources_until(options, targets, None)
}

/// [`cook_sources`] that stops before the next source once `stop` is set (the live cooker being
/// dropped: closing the editor must not wait for a whole batch, audit concurrency-1103 L1). The
/// sources cooked so far are saved; the rest are not reported and cook next time.
#[instrument(skip_all, fields(n = targets.len()))]
pub(crate) fn cook_sources_until(
    options: &CookOptions,
    targets: &BTreeSet<PathBuf>,
    stop: Option<&AtomicBool>,
) -> Result<Vec<SourceCook>, CookError> {
    let assets_root = options.project_root.join("assets");
    let cache_dir = cooked_cache_dir(&options.project_root, &options.platform);
    std::fs::create_dir_all(&cache_dir)?;
    // Held until this returns: a concurrent full cook waits instead of losing rows (and the
    // other way round).
    let _cook_lock = crate::db::CookLock::acquire(&options.project_root);
    // Holes in the targets' glTF payloads are filled from `[cook.overlay_roots]` first, with the
    // lookup and order `cook_project` uses, so this cook writes what a batch cook would
    // (ADR-8155, ADR-8845). A bad table fails this cook as it fails the batch cook.
    fill_overlay_holes(&options.project_root, &assets_root, targets)?;
    let mut db = AssetDatabase::open(&options.project_root)?;
    // A changed source without a `.meta` may be a moved one: re-pair orphaned sidecars before the
    // cook would give it a new id (ADR-2428). Sources that all have a `.meta` skip the scan.
    if targets.iter().any(|t| !regia_asset::meta_path_for(t).exists()) {
        for line in db
            .repair_orphaned_metas(&options.project_root, true)
            .warning_lines()
        {
            tracing::warn!(target: "regia_asset_pipeline", "{line}");
        }
    }
    let mut ledger = CookLedger::open(&cache_dir, Some(&options.project_root), &db);
    let mut manifest_warnings = Vec::new();
    let mut manifest_map: BTreeMap<String, ManifestEntry> =
        crate::cook::read_manifest_map(&cache_dir, &mut manifest_warnings);
    for warning in &manifest_warnings {
        tracing::warn!(target: "regia_asset_pipeline", "{warning}");
    }
    let mut inputs = crate::cook_inputs::CookInputs::open(&options.project_root, &options.platform);
    let sidecars = crate::cook::gltf_inputs(&targets.iter().cloned().collect::<Vec<_>>());
    let _shared_textures =
        crate::gltf_texture_share::Scope::enter(&options.project_root, &cache_dir, &sidecars)?;
    // Sidecars may have lost their standalone heightfield opt-in since the last cook.
    // Its primary id can now hold a shared texture; retire the old standalone skip proof.
    for sidecar in &sidecars {
        if !crate::gltf_texture_share::standalone_is_heightfield(&options.project_root, sidecar)
            && let Some(rel) = crate::import_rules::project_rel(&options.project_root, sidecar)
        {
            db.entries.remove(&rel);
            ledger.forget(&rel);
            inputs.forget(&rel);
        }
    }
    let mut out = Vec::new();
    for source in targets {
        if sidecars.contains(source)
            && !crate::gltf_texture_share::standalone_is_heightfield(&options.project_root, source)
        {
            continue;
        }
        if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            break;
        }
        let rel = source
            .strip_prefix(&assets_root)
            .map(normalise_asset_path)
            .unwrap_or_else(|_| normalise_asset_path(source));
        let rel_path = format!("assets/{rel}");
        let kind = SourceKind::of(source);
        let mut result = SourceCook {
            source: source.clone(),
            rel_path: rel_path.clone(),
            kind,
            ids: Vec::new(),
            skipped: false,
            error: None,
            warnings: Vec::new(),
        };
        let outcome = if is_terrain_layer(source) {
            crate::terrain_cook::cook_terrain_with_ledger(
                source,
                &rel_path,
                &cache_dir,
                &options.platform,
                &ledger,
                options.incremental,
            )
            .map_err(|e| e.to_string())
            .map(|t| {
                result.warnings = t.warnings.clone();
                crate::cook::CookOutcome::from(t)
            })
        } else {
            cook_one(
                source,
                &rel_path,
                &cache_dir,
                &db,
                &ledger,
                options.incremental,
                &mut inputs,
            )
            .map_err(|e| e.to_string())
        };
        // Sub-asset re-maps and missing sub-assets of a live re-import (ADR-2428).
        result.warnings.extend(crate::subasset_remap::take_warnings());
        match outcome {
            Ok(outcome) => {
                result.skipped = outcome.skipped;
                result.ids.push(outcome.primary_id);
                result.ids.extend(outcome.sub_manifest.iter().map(|m| m.id));
                record_outcome(&mut db, &mut ledger, &mut manifest_map, &rel_path, outcome);
            }
            Err(err) => result.error = Some(err),
        }
        out.push(result);
    }
    for warning in crate::gltf_texture_share::publish(&mut manifest_map) {
        if let Some(result) = out.last_mut() {
            result.warnings.push(warning);
        }
    }
    ledger.save()?;
    db.save(&options.project_root)?;
    AssetManifest::from_map(manifest_map)
        .write_to_cache(&cache_dir)
        .map_err(CookError::Manifest)?;
    // Incremental registry update: only the re-cooked sources' records change (spec content-data
    // § 2.3). Derived data, so a failure is logged and the next full cook rewrites it.
    let mut recooked: Vec<String> = out
        .iter()
        .filter(|c| c.error.is_none())
        .map(|c| c.rel_path.clone())
        .collect();
    recooked.extend(crate::gltf_texture_share::source_paths());
    if !recooked.is_empty()
        && let Err(e) =
            crate::registry_build::update_cache_registry(&options.project_root, &cache_dir, &recooked, &db)
    {
        tracing::warn!(target: "regia_asset_pipeline", "asset registry not updated: {e}");
    }
    // A re-cooked sound refreshes the loudness table (ADR-1855). Derived data, as above.
    if out
        .iter()
        .any(|c| c.kind == SourceKind::Audio && c.error.is_none() && !c.skipped)
    {
        match crate::audio_cook::write_loudness_table(&options.project_root, &cache_dir, &db) {
            Ok((_, problems)) => {
                for problem in problems {
                    tracing::warn!(target: "regia_asset_pipeline", "{problem}");
                }
            }
            Err(e) => tracing::warn!(target: "regia_asset_pipeline", "loudness table not written: {e}"),
        }
    }
    // A cache: when it cannot be written the next cook hashes and imports again.
    if let Err(e) = inputs.save() {
        tracing::debug!(target: "regia_asset_pipeline", "cook inputs not saved: {e}");
    }
    Ok(out)
}

/// Fills the missing buffers and images of the `.gltf` files among `targets` from the project's
/// overlay roots (`crate::overlay_roots::fill_gltf_documents`). Nothing happens when no overlay
/// roots are configured. `overlay_fill.json` stays the batch cook's summary: a live fill only
/// logs what it copied.
///
/// # Errors
/// `Regia.toml` does not read or its `[cook.overlay_roots]` table is invalid (as in the batch cook).
fn fill_overlay_holes(
    project_root: &Path,
    assets_root: &Path,
    targets: &BTreeSet<PathBuf>,
) -> Result<(), CookError> {
    if targets.is_empty() {
        return Ok(());
    }
    let roots = crate::overlay_roots::OverlayRoots::load(project_root)
        .map_err(|error| CookError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))?;
    if !roots.configured {
        return Ok(());
    }
    for missing in &roots.missing {
        tracing::warn!(target: "regia_asset_pipeline", "overlay root {missing} is not a directory; skipped");
    }
    let targets: Vec<PathBuf> = targets.iter().cloned().collect();
    let fill = crate::overlay_roots::fill_gltf_documents(assets_root, &roots.paths, &targets);
    for error in &fill.errors {
        tracing::warn!(target: "regia_asset_pipeline", "overlay fill: {error}");
    }
    if fill.files > 0 {
        tracing::info!(target: "regia_asset_pipeline", files = fill.files, bytes = fill.bytes, "live cook: overlay fill");
    }
    Ok(())
}

/// What a [`LiveCooker`] watches and where it sends results.
#[derive(Clone, Debug)]
pub struct LiveCookerConfig {
    /// Project root (contains `assets/`).
    pub project_root: PathBuf,
    /// Cook platform (the host by default).
    pub platform: String,
    /// Debounce quiet time.
    pub quiet: Duration,
    /// Extra directories to watch for directly-read sources (for example the engine's WGSL
    /// directory in a dev checkout). Their changes are forwarded, never cooked.
    pub extra_roots: Vec<PathBuf>,
    /// Forward directly-read sources as [`ReloadMessage::Source`]. Off for `regia run --dev`,
    /// where the game watches those itself.
    pub forward_sources: bool,
}

impl LiveCookerConfig {
    /// Host platform, the default debounce, forwarding on, no extra roots.
    #[must_use]
    pub fn for_host(project_root: PathBuf) -> Self {
        Self {
            project_root,
            platform: crate::host_platform().to_string(),
            quiet: ASSET_DEBOUNCE,
            extra_roots: Vec::new(),
            forward_sources: true,
        }
    }
}

/// A background thread that cooks changed sources as they are saved. Dropping it stops and joins
/// the thread.
pub struct LiveCooker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    roots: Vec<PathBuf>,
    quiet: Duration,
}

impl LiveCooker {
    /// Starts watching. Results go to `tx` (see the module docs).
    ///
    /// # Errors
    ///
    /// The file watcher could not start (no watchable root, or the platform refused).
    /// On Android the watcher is not built, so this always returns an error.
    pub fn spawn(config: LiveCookerConfig, tx: Sender<ReloadMessage>) -> Result<Self, notify::Error> {
        // `SourceWatcher` lives in `regia_asset::hot_reload`, which is desktop-only.
        #[cfg(target_os = "android")]
        {
            let _ = (config, tx);
            Err(notify::Error::generic(
                "live asset watching is desktop-only and is not built for Android",
            ))
        }
        #[cfg(not(target_os = "android"))]
        {
            let assets_root = config.project_root.join("assets");
            std::fs::create_dir_all(&assets_root).map_err(notify::Error::io)?;
            // The overlay roots are watched too: a changed overlay file re-cooks the glTFs whose
            // payload it is, and a new one fills their holes (ADR-8845).
            let sources =
                regia_asset::SourceRoots::for_project(&config.project_root).unwrap_or_else(|error| {
                    warn!(target: "regia_asset_pipeline", %error, "overlay roots not watched");
                    regia_asset::SourceRoots::new(assets_root.clone(), Vec::new())
                });
            let mut roots = vec![assets_root];
            roots.extend(config.extra_roots.iter().filter(|r| r.is_dir()).cloned());
            roots.extend(sources.overlays().iter().filter(|r| r.is_dir()).cloned());
            let mut watcher = SourceWatcher::open(&roots, config.quiet)?;
            let roots = watcher.roots().to_vec();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let quiet = config.quiet;
            let thread = std::thread::Builder::new()
                .name("regia-live-cook".into())
                .spawn(move || {
                    let mut worker = Worker::new(config, tx, Arc::clone(&stop_flag), sources);
                    while !stop_flag.load(Ordering::Relaxed) {
                        let Ok(batch) = watcher.wait(Duration::from_millis(200)) else {
                            warn!(target: "regia_asset_pipeline", "file watcher stopped; live cook off");
                            return;
                        };
                        if !batch.is_empty() && !worker.step(batch) {
                            return;
                        }
                    }
                })
                .map_err(notify::Error::io)?;
            info!(target: "regia_asset_pipeline", roots = ?roots, "live cook watching");
            Ok(Self {
                stop,
                thread: Some(thread),
                roots,
                quiet,
            })
        }
    }

    /// The watched directories.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The debounce quiet time.
    #[must_use]
    pub fn quiet(&self) -> Duration {
        self.quiet
    }
}

impl Drop for LiveCooker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A source's `.meta` without the sub-asset list a cook writes: the fields only the user (or a
/// tool acting for them) changes. `None` when there is no readable sidecar.
#[cfg(not(target_os = "android"))]
fn user_meta_fields(source: &Path) -> Option<regia_asset::MetaSidecar> {
    let bytes = std::fs::read(regia_asset::meta_path_for(source)).ok()?;
    let mut meta = regia_asset::MetaSidecar::from_ron(&bytes).ok()?;
    meta.sub_assets.clear();
    Some(meta)
}

/// The cooker thread's state: content hashes of what it last saw, so its own writes (`.meta`
/// sidecars) and no-op saves do not loop back into a re-cook.
#[cfg(not(target_os = "android"))]
struct Worker {
    options: CookOptions,
    config: LiveCookerConfig,
    tx: Sender<ReloadMessage>,
    seen: HashMap<PathBuf, blake3::Hash>,
    /// Set when the cooker is dropped: a running batch stops between sources.
    stop: Arc<AtomicBool>,
    /// `assets/` and the overlay roots read when the cooker started.
    sources: regia_asset::SourceRoots,
}

#[cfg(not(target_os = "android"))]
impl Worker {
    fn new(
        config: LiveCookerConfig,
        tx: Sender<ReloadMessage>,
        stop: Arc<AtomicBool>,
        sources: regia_asset::SourceRoots,
    ) -> Self {
        let options = CookOptions {
            project_root: config.project_root.clone(),
            platform: config.platform.clone(),
            incremental: true,
        };
        Self {
            options,
            config,
            tx,
            seen: HashMap::new(),
            stop,
            sources,
        }
    }

    /// `changed` plus, for each changed file under an overlay root, the project path it fills
    /// (`assets/<same relative path>`) when that exists after a fill of every glTF hole, so the
    /// glTFs that read it re-cook. The overlay paths themselves stay in the list (they are
    /// forwarded as directly-read sources and are not under `assets/`).
    fn with_overlay_targets(&self, assets_root: &Path, mut changed: Vec<PathBuf>) -> Vec<PathBuf> {
        let keys: Vec<String> = changed
            .iter()
            .filter_map(|path| self.sources.overlay_key(path).map(|(_, key)| key))
            .collect();
        if keys.is_empty() {
            return changed;
        }
        let fill = crate::overlay_roots::fill_missing_gltf_payloads(assets_root, self.sources.overlays());
        for error in &fill.errors {
            warn!(target: "regia_asset_pipeline", "overlay fill: {error}");
        }
        for key in keys {
            let project = assets_root.join(&key);
            if project.is_file() && !changed.contains(&project) {
                changed.push(project);
            }
        }
        changed
    }

    /// True when `path`'s bytes differ from the last time it was seen (and records them).
    fn changed(&mut self, path: &Path) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            self.seen.remove(path);
            return true;
        };
        let hash = blake3::hash(&bytes);
        self.seen.insert(path.to_path_buf(), hash) != Some(hash)
    }

    /// Handles one debounced batch. False when the receiver is gone.
    #[instrument(skip_all, name = "live_cook_step", fields(n = batch.len()))]
    fn step(&mut self, batch: Vec<(PathBuf, Instant)>) -> bool {
        let started = Instant::now();
        let mut changed = Vec::new();
        for (path, _) in batch {
            if self.changed(&path) {
                changed.push(path);
            }
        }
        if changed.is_empty() {
            return true;
        }
        let assets_root = self.config.project_root.join("assets");
        // Directly-read sources: forward as they are.
        if self.config.forward_sources {
            for path in &changed {
                if !SourceKind::of(path).is_read_at_runtime() {
                    continue;
                }
                if self.tx.send(ReloadMessage::Source(path.clone())).is_err() {
                    return false;
                }
            }
        }
        let changed = self.with_overlay_targets(&assets_root, changed);
        let targets = affected_sources(&assets_root, &self.config.platform, &changed);
        if targets.is_empty() {
            return true;
        }
        // Each target's `.meta` as it was before the cook, without the cook-owned sub-asset list:
        // a sidecar that differs from this afterwards in anything but that list was edited during
        // the cook, so its hash must not be recorded as our own write (audit concurrency-1103 M3).
        let before: HashMap<PathBuf, Option<regia_asset::MetaSidecar>> = targets
            .iter()
            .map(|source| (source.clone(), user_meta_fields(source)))
            .collect();
        let results = match cook_sources_until(&self.options, &targets, Some(&self.stop)) {
            Ok(results) => results,
            Err(err) => {
                warn!(target: "regia_asset_pipeline", %err, "live cook failed");
                let entry = WatchEntry {
                    path: display_path(&self.config.project_root, &assets_root),
                    kind: SourceKind::Other,
                    millis: started.elapsed().as_millis() as u64,
                    outcome: WatchOutcome::Error(err.to_string()),
                };
                return self.tx.send(ReloadMessage::Status(entry)).is_ok();
            }
        };
        let millis = started.elapsed().as_millis() as u64;
        for result in results {
            // The cook may have written this source's `.meta`: remember it so that write does
            // not come back as a change.
            let meta = regia_asset::meta_path_for(&result.source);
            let edited_meanwhile = before
                .get(&result.source)
                .is_some_and(|was| was.is_some() && *was != user_meta_fields(&result.source));
            if meta.is_file() && !edited_meanwhile {
                let _ = self.changed(&meta);
            }
            for warning in &result.warnings {
                warn!(target: "regia_asset_pipeline", "{}: {warning}", result.rel_path);
            }
            let outcome = match &result.error {
                Some(err) => WatchOutcome::Error(err.clone()),
                None => WatchOutcome::Reloaded,
            };
            if result.error.is_none() && !result.skipped {
                for id in &result.ids {
                    if self.tx.send(ReloadMessage::Reload(*id)).is_err() {
                        return false;
                    }
                }
            }
            let entry = WatchEntry {
                path: result.rel_path.clone(),
                kind: result.kind,
                millis,
                outcome,
            };
            match &entry.outcome {
                WatchOutcome::Reloaded => info!(target: "regia_asset_pipeline", "{}", entry.toast()),
                WatchOutcome::Error(_) => warn!(target: "regia_asset_pipeline", "{}", entry.toast()),
            }
            if self.tx.send(ReloadMessage::Status(entry)).is_err() {
                return false;
            }
        }
        true
    }
}

/// The source a `.meta` sidecar path belongs to (suffix matched ignoring ASCII case), or `None`
/// for any other path or a non-Unicode file name.
fn strip_meta_suffix(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    let keep = name.len().checked_sub(".meta".len())?;
    if !crate::cook::name_ends_with(name, ".meta") {
        return None;
    }
    Some(path.with_file_name(name.get(..keep)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Android build cannot see `regia_asset::hot_reload`, so the shared quiet time is
    /// duplicated. This locks the desktop copy to the runtime watcher.
    #[test]
    #[cfg(not(target_os = "android"))]
    fn debounce_matches_runtime_watcher() {
        assert_eq!(ASSET_DEBOUNCE, regia_asset::hot_reload::ASSET_DEBOUNCE);
    }

    #[test]
    fn affected_sources_maps_sidecars_meta_and_skips_direct_kinds() {
        let root = std::env::temp_dir().join(format!("regia_live_aff_{}", uuid::Uuid::new_v4()));
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("dirs");
        let gltf = assets.join("crate.gltf");
        std::fs::write(
            &gltf,
            r#"{"asset":{"version":"2.0"},"buffers":[{"uri":"crate.bin","byteLength":4}],
               "images":[{"uri":"crate_albedo.png"}]}"#,
        )
        .expect("gltf");
        std::fs::write(assets.join("crate.bin"), [0u8; 4]).expect("bin");
        std::fs::write(assets.join("crate_albedo.png"), b"png").expect("png");
        std::fs::write(assets.join("rock.png"), b"png").expect("png");
        std::fs::write(assets.join("rock.png.meta"), b"meta").expect("meta");
        std::fs::write(assets.join("fire.vfx.ron"), b"()").expect("vfx");
        let changed = vec![
            assets.join("crate.bin"),
            assets.join("crate_albedo.png"),
            assets.join("rock.png.meta"),
            assets.join("fire.vfx.ron"),
            assets.join("gone.png"),
        ];
        let got = affected_sources(&assets, "linux", &changed);
        assert_eq!(got, BTreeSet::from([gltf, assets.join("rock.png")]));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Atlases, tilemaps, baked GI volumes and colour LUTs are cooked by `cook_project`, so saving
    /// one re-cooks it (a `.fgiv` and `.tmx` used to be dropped, and no `Reload` went out; a
    /// `.cube` used to be dropped too, and with cooked-first LUT resolution its stale cooked copy
    /// then won over the edited loose file forever).
    #[test]
    fn affected_sources_includes_atlas_tilemap_and_baked_gi_sources() {
        let root = std::env::temp_dir().join(format!("regia_live_aff2_{}", uuid::Uuid::new_v4()));
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("dirs");
        let names = [
            "ui.atlas.ron",
            "a.tilemap.ron",
            "level.tmx",
            "level2.tmj",
            "gi.fgiv",
            "map.flmap",
            "grade.cube",
        ];
        let mut changed = Vec::new();
        for name in names {
            std::fs::write(assets.join(name), b"x").expect("write");
            changed.push(assets.join(name));
        }
        let got = affected_sources(&assets, "linux", &changed);
        assert_eq!(got, changed.into_iter().collect::<BTreeSet<_>>());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn affected_sources_follows_material_functions_graphs_and_instances() {
        let root = std::env::temp_dir().join(format!("regia_live_graph_{}", uuid::Uuid::new_v4()));
        let assets = root.join("assets");
        std::fs::create_dir_all(assets.join("materials")).expect("dirs");
        let func = assets.join("materials/wear.matfunc.ron");
        let graph = assets.join("materials/hull.matgraph.ron");
        let other = assets.join("materials/glass.matgraph.ron");
        let inst = assets.join("materials/hull_red.matinst.ron");
        let tex = assets.join("materials/scratches.png");
        std::fs::write(&func, "()").expect("func");
        std::fs::write(
            &graph,
            r#"(nodes: { "w": Function(path: "assets/materials/wear.matfunc.ron") })"#,
        )
        .expect("graph");
        std::fs::write(
            &other,
            r#"(textures: [(path: "assets/materials/scratches.png")])"#,
        )
        .expect("other");
        std::fs::write(&inst, r#"(parent: "assets/materials/hull.matgraph.ron")"#).expect("inst");
        std::fs::write(&tex, b"png").expect("png");
        let got = affected_sources(&assets, "linux", std::slice::from_ref(&func));
        assert_eq!(
            got,
            BTreeSet::from([graph.clone(), inst.clone()]),
            "function → graph → instance"
        );
        let got = affected_sources(&assets, "linux", std::slice::from_ref(&graph));
        assert_eq!(got, BTreeSet::from([graph, inst]), "graph → its instances");
        let got = affected_sources(&assets, "linux", std::slice::from_ref(&tex));
        assert_eq!(got, BTreeSet::from([tex, other]), "image → the graph sampling it");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn meta_suffix_strips_ignoring_case() {
        assert_eq!(
            strip_meta_suffix(Path::new("a/hero.glb.META")),
            Some(PathBuf::from("a/hero.glb"))
        );
        assert_eq!(
            strip_meta_suffix(Path::new("a/hero.glb.meta")),
            Some(PathBuf::from("a/hero.glb"))
        );
        assert_eq!(strip_meta_suffix(Path::new("a/hero.glb")), None);
    }

    /// Audit concurrency-1103 pipeline L1: a dropped live cooker stops its batch between sources
    /// instead of cooking the rest first; what was not cooked is not reported (it cooks next time).
    #[test]
    fn a_set_stop_flag_ends_a_batch_before_the_next_source() {
        let root = regia_test_utils::unique_temp_dir("live-cook-stop");
        let assets = root.join("assets");
        std::fs::create_dir_all(&assets).expect("assets");
        let targets: BTreeSet<PathBuf> = ["a.vfx.ron", "b.vfx.ron"]
            .iter()
            .map(|name| {
                let path = assets.join(name);
                std::fs::write(&path, "()").expect("source");
                path
            })
            .collect();
        let options = CookOptions {
            project_root: root.clone(),
            platform: "linux".to_string(),
            incremental: true,
        };
        let stop = AtomicBool::new(true);
        let cooked = cook_sources_until(&options, &targets, Some(&stop)).expect("cook");
        assert!(cooked.is_empty(), "{cooked:?}");
        stop.store(false, Ordering::Relaxed);
        let cooked = cook_sources_until(&options, &targets, Some(&stop)).expect("cook");
        assert_eq!(cooked.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Audit concurrency-1103 pipeline M3: the before/after comparison the live cooker uses to
    /// tell its own `.meta` write (sub-assets) from a user's settings edit made during the cook.
    #[test]
    #[cfg(not(target_os = "android"))]
    fn user_meta_fields_ignore_the_sub_asset_list_only() {
        let root = regia_test_utils::unique_temp_dir("live-cook-meta-fields");
        let source = root.join("hero.glb");
        let mut meta = regia_asset::MetaSidecar::new("gltf", regia_asset::AssetId::new_random());
        let write = |meta: &regia_asset::MetaSidecar| {
            std::fs::write(
                regia_asset::meta_path_for(&source),
                meta.to_ron_string().expect("ron"),
            )
            .expect("meta");
        };
        write(&meta);
        let before = user_meta_fields(&source);
        assert!(before.is_some());
        meta.sub_assets.push(regia_asset::SubAssetEntry {
            path: "mesh0".into(),
            id: regia_asset::AssetId::new_random(),
            type_path: "regia_render::Mesh".into(),
        });
        write(&meta);
        assert_eq!(
            user_meta_fields(&source),
            before,
            "a cook's sub-asset write is ours"
        );
        meta.settings = Some("(scale: 2.0)".into());
        write(&meta);
        assert_ne!(user_meta_fields(&source), before, "a settings edit is the user's");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ADR-5294 (supersedes ADR-4914): Android tiles a terrain's textures as ASTC 4×4, so every
    /// terrain input belongs to the tiled path on Android as on desktop: a changed layer file,
    /// ground input or texture input re-cooks the layer only, never a generic source.
    #[test]
    fn affected_sources_recooks_the_terrain_layer_on_every_platform() {
        let root = regia_test_utils::unique_temp_dir("live-android-terrain");
        let dir = root.join("assets").join("maps").join("ground");
        std::fs::create_dir_all(&dir).expect("dirs");
        let assets = root.join("assets");
        let layer = dir.join("terrain.terrain.layers.ron");
        std::fs::write(
            &layer,
            include_str!("../../regia_terrain/tests/fixtures/ground.terrain.layers.ron"),
        )
        .expect("layers");
        let height = dir.join("heightfield.png");
        let extents = dir.join("heightfield.json");
        let albedo = dir.join("macro_albedo.png");
        for path in [&height, &extents, &albedo] {
            std::fs::write(path, b"x").expect("input");
        }
        let one =
            |platform: &str, path: &PathBuf| affected_sources(&assets, platform, std::slice::from_ref(path));
        for platform in ["android", "linux"] {
            for path in [&layer, &height, &extents, &albedo] {
                assert_eq!(
                    one(platform, path),
                    BTreeSet::from([layer.clone()]),
                    "{platform}: {} re-cooks the terrain layer only",
                    path.display()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
