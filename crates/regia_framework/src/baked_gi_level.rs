// SPDX-License-Identifier: MPL-2.0
//! Installs a level's baked GI volume (`LevelLook::baked_gi`, scene format v10, ADR-283;
//! `book/agents/baked-gi-maps.md`).
//!
//! [`BakedGiLevelPlugin`] watches the open level's [`LevelSettings`]. When the level names a
//! `.fgiv` it loads the volume through the [`AssetServer`] (the cooked asset, from a pak or the
//! dev cache; an uncooked `assets/<path>` file in a dev run), installs it with
//! `RuntimeRenderer::set_baked_gi`, applies the level's setting overrides and publishes
//! [`LevelBakedGiState`]. A new file on disk, a hot reload of the cooked asset and a changed
//! path or override all reinstall it; unloading the level removes it and restores the settings
//! the host had.
//!
//! A **split volume** (ADR-2618: a bake past one portable binding, `<stem>.fgiv` plus
//! `<stem>.part<k>.fgiv`) installs part 0 as the volume and the other parts as blend layers
//! (`RuntimeRenderer::set_baked_gi_layers`); the renderer blends them across their overlap
//! bands. Streamed levels' volumes join those layers (`baked_gi_stream`). The level is reloaded
//! when part 0 or any part file changes, appears or goes away, so a set read
//! halfway through a rebake is read again once the bake has finished.
//!
//! A **missing or damaged part** (or a part of another bake) leaves the other parts lit (ADR-3006):
//! they install, each lighting out to its own extent on the side of the missing neighbor, and
//! [`LevelBakedGiState::failed_parts`] names what failed with its stable code (`volume_not_found`,
//! `volume_unreadable`). Part 0 is no exception: without it the lowest surviving part is the
//! volume and the rest are layers. A reload of the same path never loses lit parts: a read that
//! finds fewer parts than the frame has (a rebake in progress) keeps them and reports the error.
//!
//! **Host layers** (ADR-3006): a level adds its parts to the renderer's layers and removes only
//! those, so layers a host set itself stay beside them, while the level is open and after.
//!
//! A missing file or a file of another version logs one warning and sets
//! [`LevelBakedGiState::error`]; the frame keeps its other GI (`balanced` or `depth`). A volume
//! the host installed itself is replaced only while the level names its own, and the level
//! removes only the volume it installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use regia_app::prelude::*;
use regia_asset::baked_gi::BakedGiVolume;
use regia_asset::baked_gi_split::{MAX_SPLIT_PARTS, split_part_path};
use regia_asset::{AssetId, AssetServer, decode_cooked_file};
use regia_render::baked_gi::BakedGiLayer;
use regia_render::{BakedGiSettings, RuntimeRenderer};
use regia_scene::level::{LevelBakedGi, LevelBakedGiSettings, LevelManager, LevelSettings};

/// One part of a split volume that did not load (ADR-3006).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedPart {
    /// The part's index in its set.
    pub index: u32,
    /// The part's path, as the level would name it.
    pub path: String,
    /// What went wrong.
    pub error: String,
    /// A stable code: `volume_not_found` (no such file) or `volume_unreadable` (damaged, another
    /// version, or a part of another bake).
    pub code: &'static str,
}

/// What the level's baked GI install did, for the agent protocol (`level.get_baked_gi`) and
/// host UIs.
#[derive(Resource, Default)]
pub struct LevelBakedGiState {
    /// The volume path the open level names, as written in the level; `None` for no bake.
    pub volume: Option<String>,
    /// The level's volume is installed in the renderer.
    pub installed: bool,
    /// Why the volume could not be loaded (missing file, wrong version, damaged); `None` when it
    /// loaded or none is named.
    pub error: Option<String>,
    /// A stable code for `error`: `volume_not_found` or `volume_unreadable` (a damaged or
    /// newer-version file, a split part whose group does not match part 0, or a path that is a
    /// split part other than 0). Read `error` before rebaking: `name part 0` means the path is
    /// not the file without a `.part` suffix.
    pub error_code: Option<&'static str>,
    /// Probes in the installed volume (0 when none); every part of a split volume counts, so
    /// probes of the overlap bands count once per part.
    pub probes: usize,
    /// Parts in the named volume: 1 for a whole volume, the set's count for a split one, 0 when
    /// none has been read.
    pub part_count: u32,
    /// The parts of a split volume that did not load, in index order; the others are lit.
    pub failed_parts: Vec<FailedPart>,
    wanted: Option<LevelBakedGi>,
    stamp: Option<Vec<Stamp>>,
    ours: Option<Arc<BakedGiVolume>>,
    /// Parts 1.. of a split volume, installed as layers (empty for a whole volume).
    parts: Vec<Arc<BakedGiVolume>>,
    /// Sources of part 0 and of every later part slot from the last resolve. Empty until then,
    /// so the next update can re-stat them without scanning the manifest.
    sources: Vec<Source>,
    /// `AssetServer::manifest_epoch` at that resolve. A newer epoch scans the paths again.
    manifest_epoch: u64,
    restore: Option<BakedGiSettings>,
}

impl LevelBakedGiState {
    /// `true` when the renderer's installed volume is the one this level installed.
    #[must_use]
    pub fn owns(&self, installed: Option<&Arc<BakedGiVolume>>) -> bool {
        match (&self.ours, installed) {
            (Some(ours), Some(installed)) => Arc::ptr_eq(ours, installed),
            _ => false,
        }
    }

    /// The renderer layers of the installed split volume's parts 1.. (empty for a whole volume
    /// or none); `baked_gi_stream` puts them before the streamed levels' layers.
    pub(crate) fn part_layers(&self) -> Vec<BakedGiLayer> {
        self.parts.iter().cloned().map(BakedGiLayer::new).collect()
    }

    /// `true` when `volume` is one of the split parts this level installed as a layer: every other
    /// layer of the renderer belongs to a streamed level or the host.
    #[must_use]
    pub fn holds_part(&self, volume: &Arc<BakedGiVolume>) -> bool {
        self.parts.iter().any(|p| Arc::ptr_eq(p, volume))
    }

    /// The volumes this level has in the renderer: the installed volume and its parts.
    pub fn installed_volumes(&self) -> impl Iterator<Item = &Arc<BakedGiVolume>> {
        self.ours.iter().chain(self.parts.iter())
    }
}

/// Where the named volume was found, and the version of it that was last read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// Which source held the volume: a cooked asset (by id), a file or none. Part of the stamp so
    /// a cooked asset that was never hot-reloaded (generation 0) differs from a missing one.
    kind: StampKind,
    /// Hot-reload count of the cooked asset (0 for a file).
    generation: u64,
    /// Size and modification time of an uncooked file; `None` for a cooked asset or no file.
    file: Option<(u64, Option<SystemTime>)>,
}

/// [`Stamp::kind`]: the [`Source`] without its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StampKind {
    Cooked(AssetId),
    Raw,
    Missing,
}

#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// A cooked asset in the manifest (paks or the dev cache).
    Cooked(AssetId),
    /// An uncooked file under the project's `assets/`.
    Raw(PathBuf),
    /// Neither exists.
    Missing,
}

/// Installs the open level's baked GI volume ([`sync_level_baked_gi`]).
#[derive(Default)]
pub struct BakedGiLevelPlugin;

impl Plugin for BakedGiLevelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LevelBakedGiState>()
            .add_systems(Update, sync_level_baked_gi);
        // Streamed levels' volumes blend beside the level's own (ADR-1097).
        crate::baked_gi_stream::add_to(app);
    }
}

/// `Update`: reconciles the renderer's baked volume with the open level's `look.baked_gi`.
/// Does nothing without a `RuntimeRenderer`.
pub fn sync_level_baked_gi(
    level: Option<Res<LevelSettings>>,
    manager: Option<Res<LevelManager>>,
    mut assets: Option<ResMut<AssetServer>>,
    runtime: Option<ResMut<RuntimeRenderer>>,
    mut state: ResMut<LevelBakedGiState>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };
    // The settings resource outlives the level that wrote it: a level-less world names no bake.
    let open = manager.is_none_or(|m| m.current().is_some());
    let wanted = level
        .as_deref()
        .filter(|_| open)
        .and_then(|l| l.look.baked_gi.clone())
        .filter(|b| !b.volume.trim().is_empty());
    state.reconcile(wanted, assets.as_deref_mut(), &mut runtime);
}

/// The renderer's settings with the level's overrides on top.
fn with_overrides(base: BakedGiSettings, over: &LevelBakedGiSettings) -> BakedGiSettings {
    BakedGiSettings {
        intensity: over.intensity.unwrap_or(base.intensity),
        sky_scale: over.sky_scale.unwrap_or(base.sky_scale),
        emissive_scale: over.emissive_scale.unwrap_or(base.emissive_scale),
        replace_live_fill: over.replace_live_fill.unwrap_or(base.replace_live_fill),
        ..base
    }
}

/// `assets/x`, `assets://x` and `./x` all name `x` under `assets/`.
pub(crate) fn relative_path(path: &str) -> String {
    let mut p = path.trim().replace('\\', "/");
    for prefix in ["assets://", "./", "assets/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            p = rest.to_string();
        }
    }
    p
}

pub(crate) fn resolve(path: &str, assets: Option<&mut AssetServer>) -> Source {
    let rel = relative_path(path);
    let root = match assets {
        Some(server) => {
            for key in [format!("assets/{rel}"), rel.clone()] {
                if let Some(id) = server.id_for_path(&key) {
                    // A hot reload of the cooked file bumps the generation instead of decoding it.
                    server.track_external(id);
                    return Source::Cooked(id);
                }
            }
            server.project_root().to_path_buf()
        }
        None => PathBuf::new(),
    };
    let file = root.join("assets").join(&rel);
    if file.is_file() {
        Source::Raw(file)
    } else {
        Source::Missing
    }
}

pub(crate) fn stamp_of(source: &Source, assets: Option<&AssetServer>) -> Stamp {
    match source {
        Source::Cooked(id) => Stamp {
            kind: StampKind::Cooked(*id),
            generation: assets.map_or(0, |a| a.generation(*id)),
            file: None,
        },
        Source::Raw(path) => Stamp {
            kind: StampKind::Raw,
            generation: 0,
            file: std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok())),
        },
        Source::Missing => Stamp {
            kind: StampKind::Missing,
            generation: 0,
            file: None,
        },
    }
}

/// A [`Source`] remembered with the path a missing file would occupy, so the next frame can
/// check it without [`resolve`] (framework5-perf, level lightmaps).
#[derive(Clone, Debug)]
pub(crate) enum CachedSource {
    /// A cooked asset id.
    Cooked(AssetId),
    /// An uncooked file.
    Raw(PathBuf),
    /// The file was absent at this path.
    Absent(PathBuf),
}

/// The file [`resolve`] would stat for an uncooked `path` (`<project>/assets/<relative>`).
pub(crate) fn asset_file(path: &str, assets: Option<&AssetServer>) -> PathBuf {
    let rel = relative_path(path);
    let root = assets.map_or_else(PathBuf::new, |server| server.project_root().to_path_buf());
    root.join("assets").join(rel)
}

/// The [`CachedSource`] to store beside a stamp just taken from `source`.
pub(crate) fn cache_of(path: &str, source: &Source, assets: Option<&AssetServer>) -> CachedSource {
    match source {
        Source::Cooked(id) => CachedSource::Cooked(*id),
        Source::Raw(file) => CachedSource::Raw(file.clone()),
        Source::Missing => CachedSource::Absent(asset_file(path, assets)),
    }
}

/// Whether one cached source still matches `stamp`. A raw path is statted without resolving it;
/// an absent path that now exists is not current.
pub(crate) fn source_current(cached: &CachedSource, stamp: &Stamp, assets: Option<&AssetServer>) -> bool {
    match cached {
        CachedSource::Cooked(id) => {
            let generation = assets.map_or(0, |server| server.generation(*id));
            stamp.kind == StampKind::Cooked(*id) && generation == stamp.generation && stamp.file.is_none()
        }
        CachedSource::Raw(path) => {
            let file = std::fs::metadata(path)
                .ok()
                .map(|meta| (meta.len(), meta.modified().ok()));
            stamp.kind == StampKind::Raw && stamp.generation == 0 && stamp.file == file
        }
        CachedSource::Absent(path) => {
            !path.is_file()
                && stamp.kind == StampKind::Missing
                && stamp.generation == 0
                && stamp.file.is_none()
        }
    }
}

/// The project-relative path of part `index` of the split volume whose part 0 is `rel`
/// (`split_part_path`, with `/` separators).
fn part_rel(rel: &str, index: u32) -> String {
    split_part_path(Path::new(rel), index)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Part 0 plus every later part slot of a split set (ADR-2618), resolved together.
pub(crate) struct ResolvedSet {
    /// Where part 0 was found.
    pub(crate) source: Source,
    /// `source`, then parts 1..`MAX_SPLIT_PARTS`.
    pub(crate) sources: Vec<Source>,
    /// One stamp per entry of `sources`.
    pub(crate) stamps: Vec<Stamp>,
    /// `AssetServer::manifest_epoch` at the resolve (`0` with no server).
    pub(crate) epoch: u64,
}

/// Resolves `path` and every part file a split set of it may have.
///
/// A part that appears, changes or goes away after part 0 was read changes the stamp, so a set
/// read halfway through a rebake (or a cook that reached part 0 first) is loaded again instead
/// of keeping the error.
pub(crate) fn resolve_set(path: &str, mut assets: Option<&mut AssetServer>) -> ResolvedSet {
    let epoch = assets.as_deref().map_or(0, AssetServer::manifest_epoch);
    let mut sources = Vec::with_capacity(MAX_SPLIT_PARTS as usize);
    let mut stamps = Vec::with_capacity(MAX_SPLIT_PARTS as usize);
    let source = resolve(path, assets.as_deref_mut());
    stamps.push(stamp_of(&source, assets.as_deref()));
    sources.push(source.clone());
    let rel = relative_path(path);
    for k in 1..MAX_SPLIT_PARTS {
        let part_source = resolve(&part_rel(&rel, k), assets.as_deref_mut());
        stamps.push(stamp_of(&part_source, assets.as_deref()));
        sources.push(part_source);
    }
    ResolvedSet {
        source,
        sources,
        stamps,
        epoch,
    }
}

/// Stamps for sources already resolved, without looking paths up again.
///
/// `None` when there is no cache, the manifest epoch moved, or a part that was missing now has
/// a loose file under `assets/`. The caller then resolves the set again.
pub(crate) fn stamp_if_cached(
    path: &str,
    sources: &[Source],
    epoch: u64,
    assets: Option<&AssetServer>,
) -> Option<Vec<Stamp>> {
    let server = assets?;
    if sources.is_empty() || server.manifest_epoch() != epoch {
        return None;
    }
    let rel = relative_path(path);
    for (index, source) in sources.iter().enumerate() {
        if !matches!(source, Source::Missing) {
            continue;
        }
        let Ok(part_index) = u32::try_from(index) else {
            return None;
        };
        let part = if part_index == 0 {
            rel.clone()
        } else {
            part_rel(&rel, part_index)
        };
        if server.project_root().join("assets").join(&part).is_file() {
            return None;
        }
    }
    Some(
        sources
            .iter()
            .map(|source| stamp_of(source, Some(server)))
            .collect(),
    )
}

/// Why a volume could not be loaded: a stable code and the message.
pub(crate) type LoadError = (&'static str, String);

const NOT_FOUND: &str = "volume_not_found";
const UNREADABLE: &str = "volume_unreadable";

pub(crate) fn load(
    path: &str,
    source: &Source,
    assets: Option<&mut AssetServer>,
) -> Result<BakedGiVolume, LoadError> {
    let _span = tracing::info_span!("baked_gi_level_load").entered();
    let unreadable = |e: String| (UNREADABLE, format!("{path}: {e}"));
    let volume = match source {
        Source::Cooked(id) => {
            let server = assets.ok_or_else(|| unreadable("no asset server".to_string()))?;
            let (bytes, _) = server.read_cooked(*id).map_err(|e| unreadable(e.to_string()))?;
            let (_, payload) = decode_cooked_file(&bytes).map_err(|e| unreadable(e.to_string()))?;
            BakedGiVolume::from_bytes(payload)
        }
        Source::Raw(file) => {
            let bytes = std::fs::read(file).map_err(|e| unreadable(e.to_string()))?;
            BakedGiVolume::from_bytes(&bytes)
        }
        Source::Missing => return Err((NOT_FOUND, format!("{path}: file not found"))),
    };
    volume.map_err(|e| unreadable(e.to_string()))
}

/// What [`load_set`] read: the volumes that load, in index order (at least one), and the parts of
/// their set that did not.
pub(crate) struct LoadedSet {
    pub(crate) volumes: Vec<BakedGiVolume>,
    pub(crate) failed: Vec<FailedPart>,
    /// Parts in the set (1 for a whole volume).
    pub(crate) count: u32,
}

impl LoadedSet {
    /// The first failure as the state's error: its message with how much of the set is lit, and
    /// its code.
    pub(crate) fn error(&self) -> Option<LoadError> {
        self.failed.first().map(|f| {
            (
                f.code,
                format!(
                    "{}; {} of {} parts light the frame",
                    f.error,
                    self.volumes.len(),
                    self.count
                ),
            )
        })
    }
}

/// Part `index` at `rel` read as a member of the set `want` names, if `want` does.
fn load_part(
    rel: &str,
    index: u32,
    want: Option<(u64, u32)>,
    mut assets: Option<&mut AssetServer>,
) -> Result<BakedGiVolume, FailedPart> {
    let path = part_rel(rel, index);
    let source = resolve(&path, assets.as_deref_mut());
    let fail = |(code, error): LoadError, path: &str| FailedPart {
        index,
        path: path.to_string(),
        error,
        code,
    };
    let part = load(&path, &source, assets).map_err(|e| fail(e, &path))?;
    let matches = part.split.as_ref().is_some_and(|s| {
        s.index == index && want.is_none_or(|(group, count)| s.group == group && s.count == count)
    });
    if !matches {
        let error = format!("{path}: not part {index} of this split volume (rebake the map)");
        return Err(fail((UNREADABLE, error), &path));
    }
    Ok(part)
}

/// The volume at `path` and, when it is part 0 of a split volume (ADR-2618), every part of its set
/// that loads (each resolved as `path` is: cooked asset or file). A missing or damaged part is
/// listed in [`LoadedSet::failed`] and the others still load (ADR-3006); that goes for part 0 too,
/// whose loss the parts beside it reveal. `Err` only when nothing at all loads, or `path` names a
/// part other than 0.
pub(crate) fn load_set(
    path: &str,
    source: &Source,
    mut assets: Option<&mut AssetServer>,
) -> Result<LoadedSet, LoadError> {
    let rel = relative_path(path);
    let first = load(path, source, assets.as_deref_mut());
    let mut volumes = Vec::new();
    let mut failed = Vec::new();
    let (mut want, mut count) = (None, MAX_SPLIT_PARTS);
    let mut first_error = None;
    match first {
        Ok(volume) => {
            let Some(split) = volume.split.clone() else {
                return Ok(LoadedSet {
                    volumes: vec![volume],
                    failed,
                    count: 1,
                });
            };
            if split.index != 0 {
                return Err((
                    UNREADABLE,
                    format!(
                        "{path}: split part {} of {}; name part 0 (the file without a .part suffix)",
                        split.index, split.count
                    ),
                ));
            }
            want = Some((split.group, split.count));
            count = split.count;
            volumes.push(volume);
        }
        Err(e) => first_error = Some(e),
    }
    // Without part 0 the first part that reads names the set.
    let mut failed_later = Vec::new();
    for k in 1..count.min(MAX_SPLIT_PARTS) {
        match load_part(&rel, k, want, assets.as_deref_mut()) {
            Ok(part) => {
                if let Some(s) = part.split.as_ref().filter(|_| want.is_none()) {
                    want = Some((s.group, s.count));
                    count = s.count;
                }
                volumes.push(part);
            }
            Err(f) => failed_later.push(f),
        }
    }
    if let Some(e) = first_error {
        if volumes.is_empty() {
            return Err(e);
        }
        let (code, error) = e;
        failed.push(FailedPart {
            index: 0,
            path: path.to_string(),
            error,
            code,
        });
    }
    // A failure before the set was named may be past the set's end.
    failed.extend(failed_later.into_iter().filter(|f| f.index < count));
    Ok(LoadedSet {
        volumes,
        failed,
        count,
    })
}

impl LevelBakedGiState {
    fn reconcile(
        &mut self,
        wanted: Option<LevelBakedGi>,
        mut assets: Option<&mut AssetServer>,
        runtime: &mut RuntimeRenderer,
    ) {
        let Some(wanted) = wanted else {
            if self.wanted.is_some() || self.ours.is_some() {
                self.remove_ours(runtime);
                *self = Self::default();
            }
            return;
        };
        if self.wanted.as_ref() == Some(&wanted)
            && let Some(stamp) = stamp_if_cached(
                &wanted.volume,
                &self.sources,
                self.manifest_epoch,
                assets.as_deref(),
            )
            && self.stamp.as_ref() == Some(&stamp)
        {
            return;
        }
        let resolved = resolve_set(&wanted.volume, assets.as_deref_mut());
        self.sources = resolved.sources;
        self.manifest_epoch = resolved.epoch;
        if self.wanted.as_ref() == Some(&wanted) && self.stamp.as_ref() == Some(&resolved.stamps) {
            return;
        }
        let source = resolved.source;
        let stamp = resolved.stamps;
        let same_volume = self.wanted.as_ref().is_some_and(|w| w.volume == wanted.volume);
        self.wanted = Some(wanted.clone());
        self.stamp = Some(stamp);
        self.volume = Some(wanted.volume.clone());
        match load_set(&wanted.volume, &source, assets) {
            // A reload of the same path never loses lit parts (a rebake in progress).
            Ok(set) if same_volume && set.volumes.len() < self.lit_volumes() => {
                self.note_failure(&wanted, set.error());
                self.part_count = set.count;
                self.failed_parts = set.failed;
                self.keep_lit(&wanted, runtime);
            }
            Ok(set) => {
                self.note_failure(&wanted, set.error());
                self.part_count = set.count;
                self.failed_parts = set.failed;
                let mut volumes = set.volumes.into_iter().map(Arc::new);
                let Some(volume) = volumes.next() else {
                    return;
                };
                let parts: Vec<Arc<BakedGiVolume>> = volumes.collect();
                let base = *self.restore.get_or_insert_with(|| runtime.0.baked_gi_settings());
                runtime
                    .0
                    .set_baked_gi_settings(with_overrides(base, &wanted.settings));
                runtime.0.set_baked_gi(Some(volume.clone()));
                self.ours = Some(volume);
                self.set_parts(parts, runtime);
                self.installed = true;
                self.probes = self.lit_probes();
            }
            Err((code, error)) => {
                self.note_failure(&wanted, Some((code, error)));
                self.part_count = 0;
                self.failed_parts.clear();
                // A failed reload of the same volume keeps the previous one; another path was
                // named by a different level, whose volume must not light this one.
                if same_volume {
                    self.keep_lit(&wanted, runtime);
                } else {
                    self.remove_ours(runtime);
                }
            }
        }
    }

    /// The volumes this level has in the renderer.
    fn lit_volumes(&self) -> usize {
        self.installed_volumes().count()
    }

    fn lit_probes(&self) -> usize {
        self.installed_volumes().map(|v| v.probe_count()).sum()
    }

    /// Records `failure` (none: clears the error), with one warning.
    fn note_failure(&mut self, wanted: &LevelBakedGi, failure: Option<LoadError>) {
        let Some((code, error)) = failure else {
            self.error = None;
            self.error_code = None;
            return;
        };
        tracing::warn!(
            target: "regia_framework",
            "level baked GI {}: {error}; the frame keeps its other GI",
            wanted.volume
        );
        self.error = Some(error);
        self.error_code = Some(code);
    }

    /// Keeps the volumes that are lit through a failed reload; they still follow the level's
    /// current overrides.
    fn keep_lit(&mut self, wanted: &LevelBakedGi, runtime: &mut RuntimeRenderer) {
        if self.ours.is_some() {
            let base = *self.restore.get_or_insert_with(|| runtime.0.baked_gi_settings());
            runtime
                .0
                .set_baked_gi_settings(with_overrides(base, &wanted.settings));
        }
        self.installed = self.ours.is_some();
        self.probes = self.lit_probes();
    }

    /// Removes the volume this level installed (not a host's) and restores the host's settings.
    fn remove_ours(&mut self, runtime: &mut RuntimeRenderer) {
        if let Some(ours) = self.ours.take()
            && runtime.0.baked_gi().is_some_and(|now| Arc::ptr_eq(now, &ours))
        {
            runtime.0.set_baked_gi(None);
        }
        self.set_parts(Vec::new(), runtime);
        if let Some(settings) = self.restore.take() {
            runtime.0.set_baked_gi_settings(settings);
        }
        self.installed = false;
        self.probes = 0;
    }

    /// Installs `parts` (a split volume's other parts) as the renderer's layers in place of the
    /// ones it had, ahead of every other layer: those are a host's or a streamed level's and stay
    /// (ADR-3006). A level that has and had none does not touch the list.
    fn set_parts(&mut self, parts: Vec<Arc<BakedGiVolume>>, runtime: &mut RuntimeRenderer) {
        if self.parts.is_empty() && parts.is_empty() {
            return;
        }
        let others: Vec<BakedGiLayer> = runtime
            .0
            .baked_gi_layers()
            .iter()
            .filter(|l| !self.holds_part(&l.volume))
            .cloned()
            .collect();
        self.parts = parts;
        let mut layers = self.part_layers();
        layers.extend(others);
        runtime.0.set_baked_gi_layers(&layers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, GI_COMPONENTS, ProbeData, ProbeFlags, ProbeMeta, ShRgb, TerrainLayer,
        VolumeLayout,
    };
    use regia_asset::{
        AssetManifest, AssetServerSettings, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry,
        cooked_cache_dir, cooked_file_name, encode_cooked_file,
    };
    use regia_core::id::{Guid, TypePathHash};
    use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level, unload_level};
    use regia_scene::prelude::ScenePlugin;
    use regia_test_utils::unique_temp_dir;

    /// A 2 × 2 × 1-probe terrain volume (valid probes, uniform sky).
    fn tiny_volume() -> BakedGiVolume {
        volume_of(2)
    }

    /// A `columns` × 2 × 1-probe terrain volume (valid probes, uniform sky).
    fn volume_of(columns: u32) -> BakedGiVolume {
        let layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [columns, 2],
                levels: vec![1.0],
                ground: vec![0.0; columns as usize * 2],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let sh: ShRgb = [[0.5, 0.0, 0.0, 0.0]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [sh; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("tiny volume")
    }

    /// A project directory with `assets/` and an asset server over it.
    fn project(tag: &str) -> (PathBuf, AssetServer) {
        let root = unique_temp_dir(tag);
        std::fs::create_dir_all(root.join("assets").join("maps")).expect("assets dir");
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open asset server");
        (root, server)
    }

    fn level_text(settings: &str) -> String {
        format!(
            r#"SceneFile(
    format_version: 10,
    entities: [],
    resources: {{
        "regia_scene::level::LevelSettings": (look: (baked_gi: Some((volume: "maps/a.fgiv"{settings})))),
    }},
)"#
        )
    }

    fn app_with(server: AssetServer, level: &str) -> (App, LevelId) {
        let mut app = App::new();
        app.add_plugins((ScenePlugin, BakedGiLevelPlugin));
        app.insert_resource(server);
        app.insert_resource(RuntimeRenderer::default());
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, level).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        (app, id)
    }

    fn installed(app: &App) -> bool {
        app.world()
            .resource::<RuntimeRenderer>()
            .0
            .baked_gi_status()
            .installed
    }

    /// A level naming a volume installs it, applies the overrides, and unloading removes it and
    /// restores the host's settings.
    #[test]
    fn a_level_installs_its_volume_and_unload_removes_it() {
        let (root, server) = project("gi-level-install");
        tiny_volume()
            .write(&root.join("assets").join("maps").join("a.fgiv"))
            .expect("write fgiv");
        let (mut app, id) = app_with(server, &level_text(", settings: (sky_scale: Some(0.25))"));
        let host = app.world().resource::<RuntimeRenderer>().0.baked_gi_settings();

        app.update();
        assert!(!installed(&app), "no level is open yet");
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();

        assert!(installed(&app));
        let renderer = &app.world().resource::<RuntimeRenderer>().0;
        assert_eq!(renderer.baked_gi_settings().sky_scale, 0.25);
        assert_eq!(renderer.baked_gi_settings().intensity, host.intensity);
        let state = app.world().resource::<LevelBakedGiState>();
        assert!(state.installed);
        assert_eq!(state.error, None);
        assert_eq!(state.volume.as_deref(), Some("maps/a.fgiv"));
        assert_eq!(state.probes, 4);
        assert!(state.owns(renderer.baked_gi()));

        unload_level(app.world_mut(), id).expect("unload");
        app.update();
        assert!(!installed(&app));
        let renderer = &app.world().resource::<RuntimeRenderer>().0;
        assert_eq!(renderer.baked_gi_settings(), host);
        assert!(!app.world().resource::<LevelBakedGiState>().installed);
    }

    /// Once the level's volume is installed, a later update does not look the path up again.
    #[test]
    fn an_unchanged_level_does_not_look_up_its_volume_path() {
        let (root, server) = project("gi-level-lookup-once");
        tiny_volume()
            .write(&root.join("assets").join("maps").join("a.fgiv"))
            .expect("write fgiv");
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app));
        let before = app.world().resource::<AssetServer>().path_lookup_count();
        app.update();
        let after = app.world().resource::<AssetServer>().path_lookup_count();
        assert_eq!(
            after, before,
            "an unchanged level volume must not call id_for_path"
        );
    }

    /// A missing file is a status and a warning, never a panic, and installs nothing.
    #[test]
    fn a_missing_volume_sets_the_error_status() {
        let (_root, server) = project("gi-level-missing");
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        app.update();
        assert!(!installed(&app));
        let state = app.world().resource::<LevelBakedGiState>();
        assert!(!state.installed);
        assert_eq!(state.volume.as_deref(), Some("maps/a.fgiv"));
        assert!(
            state.error.as_deref().is_some_and(|e| e.contains("not found")),
            "{:?}",
            state.error
        );
        assert_eq!(state.error_code, Some("volume_not_found"));
    }

    /// A file that is not a `.fgiv` (or of another version) is refused with the reason.
    #[test]
    fn a_damaged_volume_sets_the_error_status() {
        let (root, server) = project("gi-level-damaged");
        std::fs::write(root.join("assets").join("maps").join("a.fgiv"), b"not a volume").expect("write");
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(!installed(&app));
        let state = app.world().resource::<LevelBakedGiState>();
        assert!(state.error.is_some());
        assert_eq!(state.error_code, Some("volume_unreadable"));
        assert!(!state.installed);
    }

    /// The file appearing later (a bake finishing) installs it without reopening the level, and
    /// a replaced file is picked up.
    #[test]
    fn a_volume_that_appears_or_changes_is_installed() {
        let (root, server) = project("gi-level-appears");
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(!installed(&app));

        let path = root.join("assets").join("maps").join("a.fgiv");
        regia_test_utils::write_atomic(&path, &tiny_volume().to_bytes().expect("encode")).expect("write");
        app.update();
        assert!(installed(&app));
        assert_eq!(app.world().resource::<LevelBakedGiState>().error, None);
        let first = app.world().resource::<RuntimeRenderer>().0.baked_gi().cloned();

        // A different file (a larger volume, so its size differs even on a coarse clock).
        regia_test_utils::write_atomic(&path, &volume_of(3).to_bytes().expect("encode")).expect("write");
        app.update();
        let second = app.world().resource::<RuntimeRenderer>().0.baked_gi().cloned();
        assert!(installed(&app));
        let (Some(first), Some(second)) = (first, second) else {
            panic!("a volume is installed");
        };
        assert!(!Arc::ptr_eq(&first, &second), "the changed file was loaded again");
        assert_eq!(second.probe_count(), 6);
    }

    /// A same-path reload that fails keeps the old volume but still applies new overrides.
    #[test]
    fn a_failed_reload_still_applies_changed_overrides() {
        let (root, mut server) = project("gi-level-failed-reload");
        let path = root.join("assets").join("maps").join("a.fgiv");
        regia_test_utils::write_atomic(&path, &tiny_volume().to_bytes().expect("encode")).expect("write");
        let mut state = LevelBakedGiState::default();
        let mut runtime = RuntimeRenderer::default();
        let wanted = |sky_scale: Option<f32>| LevelBakedGi {
            volume: "maps/a.fgiv".to_string(),
            settings: LevelBakedGiSettings {
                sky_scale,
                ..LevelBakedGiSettings::default()
            },
        };
        state.reconcile(Some(wanted(None)), Some(&mut server), &mut runtime);
        assert!(state.installed);
        let base = runtime.0.baked_gi_settings().sky_scale;

        regia_test_utils::write_atomic(&path, b"not a volume").expect("damage");
        state.reconcile(Some(wanted(Some(base + 0.25))), Some(&mut server), &mut runtime);
        assert!(state.error.is_some());
        assert!(state.installed, "the previous volume stays");
        assert_eq!(runtime.0.baked_gi_settings().sky_scale, base + 0.25);
    }

    /// A volume the host installed is left alone when the level names none.
    #[test]
    fn a_host_volume_is_not_removed_by_a_level_without_a_bake() {
        let (_root, server) = project("gi-level-host");
        let (mut app, id) = app_with(
            server,
            r#"SceneFile(format_version: 10, entities: [], resources: {})"#,
        );
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi(Some(Arc::new(tiny_volume())));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app), "the host's volume stays");
        assert!(!app.world().resource::<LevelBakedGiState>().installed);
    }

    /// The cooked path: the manifest names the asset, its payload is the `.fgiv` unchanged.
    #[test]
    fn a_cooked_volume_installs_through_the_asset_server() {
        let root = unique_temp_dir("gi-level-cooked");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let id = AssetId::new_random();
        let payload = tiny_volume().to_bytes().expect("encode");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of("regia_asset::baked_gi::BakedGiVolume"),
            asset_id: id,
            importer_version: 1_000_000,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("cooked file");
        std::fs::write(cache.join(cooked_file_name(id)), file).expect("write cooked");
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "assets/maps/a.fgiv".to_string(),
            ManifestEntry {
                path: "assets/maps/a.fgiv".to_string(),
                id,
                type_path: "regia_asset::baked_gi::BakedGiVolume".to_string(),
            },
        );
        AssetManifest::from_map(map)
            .write_to_cache(&cache)
            .expect("manifest");
        let server = AssetServer::open(AssetServerSettings {
            project_root: root,
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");

        let (mut app, level) = app_with(server, &level_text(""));
        open_level(app.world_mut(), level, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app));

        // A hot reload bumps the generation; the volume is read again.
        let before = app.world().resource::<RuntimeRenderer>().0.baked_gi().cloned();
        app.world_mut()
            .resource_mut::<AssetServer>()
            .reload_id(id)
            .expect("reload");
        app.update();
        let after = app.world().resource::<RuntimeRenderer>().0.baked_gi().cloned();
        let (Some(before), Some(after)) = (before, after) else {
            panic!("a volume is installed");
        };
        assert!(!Arc::ptr_eq(&before, &after));
    }

    /// Writes the dev-cache manifest naming `entries` (path, id) as baked GI volumes.
    fn write_cache_manifest(cache: &std::path::Path, entries: &[(&str, AssetId)]) {
        let map = entries
            .iter()
            .map(|(path, id)| {
                (
                    (*path).to_string(),
                    ManifestEntry {
                        path: (*path).to_string(),
                        id: *id,
                        type_path: "regia_asset::baked_gi::BakedGiVolume".to_string(),
                    },
                )
            })
            .collect();
        AssetManifest::from_map(map)
            .write_to_cache(cache)
            .expect("manifest");
    }

    /// Writes `volume` as cooked asset `id` into the dev cache at `cache`.
    fn write_cooked_volume(cache: &std::path::Path, id: AssetId, volume: &BakedGiVolume) {
        let payload = volume.to_bytes().expect("encode");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of("regia_asset::baked_gi::BakedGiVolume"),
            asset_id: id,
            importer_version: 1_000_000,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("cooked file");
        std::fs::write(cache.join(cooked_file_name(id)), file).expect("write cooked");
    }

    /// A cooked asset that was never hot-reloaded (generation 0) is not the same stamp as a
    /// missing file (audit framework5 #2): a volume that only joins the manifest after the first
    /// miss installs, and one that leaves the manifest is removed with `volume_not_found`.
    #[test]
    fn a_cooked_volume_at_generation_0_is_not_a_missing_one() {
        let root = unique_temp_dir("gi-level-cooked-gen0");
        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        write_cache_manifest(&cache, &[]);
        let server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");
        let (mut app, level) = app_with(server, &level_text(""));
        open_level(app.world_mut(), level, LoadMode::Replace).expect("open");
        app.update();
        assert!(!installed(&app));
        assert_eq!(
            app.world().resource::<LevelBakedGiState>().error_code,
            Some("volume_not_found")
        );

        // A live cook adds the volume; the manifest refresh does not bump its generation.
        let id = AssetId::new_random();
        write_cooked_volume(&cache, id, &tiny_volume());
        write_cache_manifest(&cache, &[("assets/maps/a.fgiv", id)]);
        app.world_mut().resource_mut::<AssetServer>().refresh_manifest();
        assert_eq!(app.world().resource::<AssetServer>().generation(id), 0);
        app.update();
        assert!(installed(&app), "the cooked volume that appeared is installed");
        {
            let state = app.world().resource::<LevelBakedGiState>();
            assert!(state.installed);
            assert_eq!(state.error_code, None);
        }

        // The volume leaves the manifest again (a pak unmounted, a cook removed it). As for a
        // deleted file, the failed reload of the same path keeps the previous volume, and the
        // miss is reported.
        write_cache_manifest(&cache, &[]);
        app.world_mut().resource_mut::<AssetServer>().refresh_manifest();
        app.update();
        let state = app.world().resource::<LevelBakedGiState>();
        assert_eq!(state.error_code, Some("volume_not_found"));
        assert!(
            state.error.as_deref().is_some_and(|e| e.contains("not found")),
            "{:?}",
            state.error
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `volume_of(columns)` split at 70 % of its runtime bytes (ADR-2618), written as
    /// `maps/a.fgiv` plus its part files.
    fn write_split(root: &std::path::Path, columns: u32) -> Vec<BakedGiVolume> {
        let whole = volume_of(columns);
        let cap = whole.layout().runtime_buffer_bytes() * 7 / 10;
        let parts = whole.split_parts(cap).expect("the volume splits");
        assert!(parts.len() >= 2, "{} parts", parts.len());
        regia_asset::baked_gi_split::write_split_set(
            &parts,
            &root.join("assets").join("maps").join("a.fgiv"),
        )
        .expect("write parts");
        parts
    }

    fn layer_count(app: &App) -> usize {
        app.world()
            .resource::<RuntimeRenderer>()
            .0
            .baked_gi_layers()
            .len()
    }

    /// A level naming part 0 of a split volume installs part 0 as the volume and every other
    /// part as a layer (ADR-2618); unloading removes all of them.
    #[test]
    fn a_split_volume_installs_every_part_and_unload_removes_them() {
        let (root, server) = project("gi-level-split");
        let parts = write_split(&root, 24);
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();

        assert!(installed(&app));
        let state = app.world().resource::<LevelBakedGiState>();
        assert_eq!(state.error, None);
        let probes: usize = parts.iter().map(BakedGiVolume::probe_count).sum();
        assert_eq!(state.probes, probes);
        let renderer = &app.world().resource::<RuntimeRenderer>().0;
        assert!(state.owns(renderer.baked_gi()));
        let split = |v: &BakedGiVolume| v.split.as_ref().map(|s| (s.index, s.count, s.group));
        assert_eq!(
            renderer.baked_gi().and_then(|v| split(v.as_ref())),
            split(&parts[0])
        );
        let layers = renderer.baked_gi_layers();
        assert_eq!(layers.len(), parts.len() - 1);
        for (layer, part) in layers.iter().zip(&parts[1..]) {
            assert_eq!(split(layer.volume.as_ref()), split(part));
            assert_eq!((layer.priority, layer.weight), (0, 1.0));
        }
        assert_eq!(renderer.baked_gi_status().error, None);

        unload_level(app.world_mut(), id).expect("unload");
        app.update();
        assert!(!installed(&app));
        assert_eq!(layer_count(&app), 0);
    }

    /// A missing part leaves the other part lit and names what failed (`volume_not_found`), a part
    /// of another bake is refused the same way (`volume_unreadable`); naming a part other than 0
    /// installs nothing.
    #[test]
    fn a_split_volume_with_a_missing_or_foreign_part_lights_the_rest() {
        let (root, server) = project("gi-level-split-missing");
        write_split(&root, 24);
        let part1 = root.join("assets").join("maps").join("a.part1.fgiv");
        let kept = std::fs::read(&part1).expect("part 1");
        std::fs::remove_file(&part1).expect("remove part 1");
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app), "part 0 lights the frame");
        assert_eq!(layer_count(&app), 0);
        let state = app.world().resource::<LevelBakedGiState>();
        assert!(state.installed);
        assert_eq!(state.error_code, Some("volume_not_found"));
        assert!(
            state.error.as_deref().is_some_and(|e| e.contains("a.part1.fgiv")),
            "{:?}",
            state.error
        );
        assert_eq!(state.part_count, 2);
        assert_eq!(state.failed_parts.len(), 1);
        assert_eq!(state.failed_parts[0].index, 1);
        assert_eq!(state.failed_parts[0].code, "volume_not_found");

        // Part 1 of another bake (a different layout, so another group).
        let other = volume_of(30);
        let cap = other.layout().runtime_buffer_bytes() * 7 / 10;
        let foreign = other.split_parts(cap).expect("splits");
        regia_test_utils::write_atomic(&part1, &foreign[1].to_bytes().expect("encode")).expect("write");
        let mut server = AssetServer::open(AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open");
        let mut state = LevelBakedGiState::default();
        let mut runtime = RuntimeRenderer::default();
        let wanted = |volume: &str| LevelBakedGi {
            volume: volume.to_string(),
            settings: LevelBakedGiSettings::default(),
        };
        state.reconcile(Some(wanted("maps/a.fgiv")), Some(&mut server), &mut runtime);
        assert_eq!(state.error_code, Some("volume_unreadable"), "{:?}", state.error);
        assert!(state.installed, "the foreign part stays out, part 0 lights");
        assert!(runtime.0.baked_gi_layers().is_empty());
        assert_eq!(state.failed_parts[0].index, 1);

        regia_test_utils::write_atomic(&part1, &kept).expect("restore part 1");
        let mut named_part = LevelBakedGiState::default();
        let mut bare = RuntimeRenderer::default();
        named_part.reconcile(Some(wanted("maps/a.part1.fgiv")), Some(&mut server), &mut bare);
        assert_eq!(
            named_part.error_code,
            Some("volume_unreadable"),
            "{:?}",
            named_part.error
        );
        assert!(!named_part.installed);
        assert!(
            named_part
                .error
                .as_deref()
                .is_some_and(|e| e.contains("name part 0")),
            "{:?}",
            named_part.error
        );

        let mut state = LevelBakedGiState::default();
        state.reconcile(Some(wanted("maps/a.fgiv")), Some(&mut server), &mut runtime);
        assert!(state.installed, "{:?}", state.error);
        assert_eq!(runtime.0.baked_gi_layers().len(), 1);
        assert!(state.failed_parts.is_empty());
        assert_eq!(state.error, None);
    }

    /// `volume_of(columns)` split into three parts, written as `maps/a.fgiv` and its part files.
    fn write_three_parts(root: &std::path::Path) -> Vec<BakedGiVolume> {
        let whole = volume_of(48);
        let cap = whole.layout().runtime_buffer_bytes() * 45 / 100;
        let parts = whole.split_parts(cap).expect("the volume splits");
        assert_eq!(parts.len(), 3, "the fixture is three parts");
        regia_asset::baked_gi_split::write_split_set(
            &parts,
            &root.join("assets").join("maps").join("a.fgiv"),
        )
        .expect("write parts");
        parts
    }

    /// The renderer's blended lookup at the middle of what `part` owns.
    fn sample_in(runtime: &RuntimeRenderer, part: &BakedGiVolume) -> regia_render::StaticGiSample {
        let (Some(split), Some(t)) = (part.split.as_ref(), part.terrain.as_ref()) else {
            panic!("a split terrain part");
        };
        let o = split.owned_columns;
        let middle =
            |a: usize| t.origin[a] + (0.5 * (o[a] + o[a + 2]).saturating_sub(1) as f32) * t.spacing[a];
        let up = [0.0, 1.0, 0.0];
        runtime.0.sample_baked_gi([middle(0), 1.0, middle(1)], up, up)
    }

    /// A renderer holding every one of `parts`.
    fn whole_set(parts: &[BakedGiVolume]) -> RuntimeRenderer {
        let mut runtime = RuntimeRenderer::default();
        let mut arcs = parts.iter().cloned().map(Arc::new);
        runtime.0.set_baked_gi(arcs.next());
        let layers: Vec<BakedGiLayer> = arcs.map(BakedGiLayer::new).collect();
        runtime.0.set_baked_gi_layers(&layers);
        runtime
    }

    /// Any one part of three missing or damaged: the two others stay lit where they own the map,
    /// the lost part's own ground goes dark, and the code is stable (ADR-3006, part 0 included).
    #[test]
    fn a_missing_or_damaged_part_leaves_the_other_parts_lit() {
        for (lost, damage) in [
            (0u32, false),
            (1, false),
            (2, false),
            (0, true),
            (1, true),
            (2, true),
        ] {
            let (root, mut server) = project("gi-level-split-lost");
            let parts = write_three_parts(&root);
            let file = root.join("assets").join("maps").join(if lost == 0 {
                "a.fgiv".to_string()
            } else {
                format!("a.part{lost}.fgiv")
            });
            if damage {
                let mut bytes = std::fs::read(&file).expect("read the part");
                // The middle of the file is section bytes, which the payload hash covers.
                let middle = bytes.len() / 2;
                bytes[middle] ^= 0x5a;
                regia_test_utils::write_atomic(&file, &bytes).expect("damage the part");
            } else {
                std::fs::remove_file(&file).expect("remove the part");
            }
            let mut state = LevelBakedGiState::default();
            let mut runtime = RuntimeRenderer::default();
            let wanted = LevelBakedGi {
                volume: "maps/a.fgiv".to_string(),
                settings: LevelBakedGiSettings::default(),
            };
            state.reconcile(Some(wanted), Some(&mut server), &mut runtime);

            let case = format!("part {lost}, damaged {damage}");
            let code = if damage {
                "volume_unreadable"
            } else {
                "volume_not_found"
            };
            assert!(state.installed, "{case}: {:?}", state.error);
            assert_eq!(state.error_code, Some(code), "{case}");
            assert_eq!(state.part_count, 3, "{case}");
            assert_eq!(state.failed_parts.len(), 1, "{case}");
            assert_eq!(
                (state.failed_parts[0].index, state.failed_parts[0].code),
                (lost, code),
                "{case}"
            );
            assert_eq!(state.installed_volumes().count(), 2, "{case}");
            let probes: usize = (0..3u32)
                .filter(|k| *k != lost)
                .map(|k| parts[k as usize].probe_count())
                .sum();
            assert_eq!(state.probes, probes, "{case}");
            let complete = whole_set(&parts);
            for (k, part) in parts.iter().enumerate() {
                let got = sample_in(&runtime, part);
                let want = sample_in(&complete, part);
                assert!(want.coverage > 0.0, "{case}: the fixture lights part {k}");
                if k as u32 == lost {
                    assert_eq!(got.coverage, 0.0, "{case}: the lost part's ground holds no light");
                } else {
                    assert_eq!(got, want, "{case}: part {k} lights as in the complete set");
                }
            }
        }
    }

    /// With every part gone nothing installs and part 0's error is the state's error.
    #[test]
    fn a_split_volume_with_no_part_left_installs_nothing() {
        let (root, mut server) = project("gi-level-split-none");
        write_three_parts(&root);
        let maps = root.join("assets").join("maps");
        for name in ["a.fgiv", "a.part1.fgiv", "a.part2.fgiv"] {
            std::fs::remove_file(maps.join(name)).expect("remove");
        }
        let mut state = LevelBakedGiState::default();
        let mut runtime = RuntimeRenderer::default();
        let wanted = LevelBakedGi {
            volume: "maps/a.fgiv".to_string(),
            settings: LevelBakedGiSettings::default(),
        };
        state.reconcile(Some(wanted), Some(&mut server), &mut runtime);
        assert!(!state.installed);
        assert_eq!(state.error_code, Some("volume_not_found"));
        assert!(state.failed_parts.is_empty());
        assert!(runtime.0.baked_gi().is_none());
    }

    /// A reload that finds fewer parts than are lit keeps them and reports the error (a rebake in
    /// progress must not take light away); the set read again once it is whole clears it.
    #[test]
    fn a_reload_never_loses_lit_parts() {
        let (root, server) = project("gi-level-split-reload");
        let parts = write_three_parts(&root);
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert_eq!(layer_count(&app), 2);
        let part2 = root.join("assets").join("maps").join("a.part2.fgiv");
        let kept = std::fs::read(&part2).expect("part 2");

        std::fs::remove_file(&part2).expect("remove");
        app.update();
        assert_eq!(layer_count(&app), 2, "the lit part stays");
        let state = app.world().resource::<LevelBakedGiState>();
        assert_eq!(state.error_code, Some("volume_not_found"));
        assert_eq!(state.failed_parts.len(), 1);
        assert_eq!(
            state.probes,
            parts.iter().map(BakedGiVolume::probe_count).sum::<usize>()
        );

        regia_test_utils::write_atomic(&part2, &kept).expect("part 2 is back");
        app.update();
        assert_eq!(layer_count(&app), 2);
        let state = app.world().resource::<LevelBakedGiState>();
        assert_eq!(state.error, None);
        assert!(state.failed_parts.is_empty());
    }

    /// A set read halfway through a rebake (part 0 new, part 1 not written yet) lights part 0 and
    /// completes once part 1 arrives, though part 0 did not change again: every part file is in the
    /// stamp.
    #[test]
    fn a_part_that_arrives_after_part_0_completes_the_set() {
        let (root, server) = project("gi-level-split-late-part");
        write_split(&root, 24);
        let part1 = root.join("assets").join("maps").join("a.part1.fgiv");
        let kept = std::fs::read(&part1).expect("part 1");
        std::fs::remove_file(&part1).expect("remove part 1");
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app));
        assert_eq!(layer_count(&app), 0);
        assert_eq!(
            app.world().resource::<LevelBakedGiState>().error_code,
            Some("volume_not_found")
        );
        app.update();
        assert_eq!(layer_count(&app), 0, "nothing changed, nothing installs");
        regia_test_utils::write_atomic(&part1, &kept).expect("part 1 arrives");
        app.update();
        let state = app.world().resource::<LevelBakedGiState>();
        assert!(state.installed, "{:?}", state.error);
        assert!(state.error.is_none());
        assert_eq!(layer_count(&app), 1);
    }

    fn host_layer(x0: f32) -> (Arc<BakedGiVolume>, BakedGiLayer) {
        let mut layout = volume_of(4).layout();
        if let Some(t) = layout.terrain.as_mut() {
            t.origin = [x0, 0.0];
        }
        let probes = vec![
            ProbeData {
                sh: [[[0.5, 0.0, 0.0, 0.0]; 3]; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            };
            layout.probe_count()
        ];
        let volume = Arc::new(
            BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
                .expect("host volume"),
        );
        let layer = BakedGiLayer {
            priority: 3,
            blend_distance: 2.0,
            weight: 0.5,
            ..BakedGiLayer::new(volume.clone())
        };
        (volume, layer)
    }

    /// The host's layers as the renderer holds them: volume identity and its blend values.
    fn holds_host_layer(app: &App, host: &Arc<BakedGiVolume>) -> bool {
        app.world()
            .resource::<RuntimeRenderer>()
            .0
            .baked_gi_layers()
            .iter()
            .any(|l| {
                Arc::ptr_eq(&l.volume, host) && l.priority == 3 && l.blend_distance == 2.0 && l.weight == 0.5
            })
    }

    /// A level with a split volume and the host's own layers coexist: the parts join the host's
    /// layers instead of replacing them, and closing the level leaves the host's (the known issue
    /// of the v0.1.28 split fragment).
    #[test]
    fn a_split_volume_keeps_the_host_layers_while_it_is_open_and_after() {
        let (root, server) = project("gi-level-split-host");
        let parts = write_split(&root, 24);
        let (mut app, id) = app_with(server, &level_text(""));
        let (host, layer) = host_layer(500.0);
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(std::slice::from_ref(&layer));

        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app));
        assert!(
            holds_host_layer(&app, &host),
            "the host's layer stays beside the parts"
        );
        assert_eq!(
            layer_count(&app),
            parts.len(),
            "the other parts and the host's one"
        );

        // Nothing changed: the renderer's list is left alone, frame after frame.
        app.update();
        assert_eq!(layer_count(&app), parts.len());

        unload_level(app.world_mut(), id).expect("unload");
        app.update();
        let renderer = &app.world().resource::<RuntimeRenderer>().0;
        assert!(renderer.baked_gi().is_none(), "the level's volume went with it");
        assert!(
            holds_host_layer(&app, &host),
            "closing the level leaves the host's layer"
        );
        assert_eq!(layer_count(&app), 1);
    }

    /// A host that sets its layers while a split level is open replaces only its own: the parts
    /// stay lit (the host's call used to drop them for as long as the level stayed open).
    #[test]
    fn a_host_that_sets_layers_during_a_split_level_keeps_the_parts() {
        let (root, server) = project("gi-level-split-host-late");
        let parts = write_split(&root, 24);
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert_eq!(layer_count(&app), parts.len() - 1);

        let (host, layer) = host_layer(500.0);
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(std::slice::from_ref(&layer));
        app.update();
        assert!(holds_host_layer(&app, &host));
        assert_eq!(
            layer_count(&app),
            parts.len(),
            "the parts came back beside the host's"
        );

        // The host clears its own: only the parts remain.
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(&[]);
        app.update();
        assert_eq!(layer_count(&app), parts.len() - 1);
        assert!(!holds_host_layer(&app, &host));
    }

    /// A rebake that replaces the parts swaps them in the renderer and leaves the host's alone.
    #[test]
    fn a_rebake_swaps_the_parts_and_keeps_the_host_layers() {
        let (root, server) = project("gi-level-split-host-rebake");
        write_split(&root, 24);
        let (mut app, id) = app_with(server, &level_text(""));
        let (host, layer) = host_layer(500.0);
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_baked_gi_layers(std::slice::from_ref(&layer));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let before = app
            .world()
            .resource::<RuntimeRenderer>()
            .0
            .baked_gi_layers()
            .to_vec();

        let rebaked = write_split(&root, 30);
        app.update();
        let after = app
            .world()
            .resource::<RuntimeRenderer>()
            .0
            .baked_gi_layers()
            .to_vec();
        assert_eq!(after.len(), rebaked.len());
        assert!(holds_host_layer(&app, &host));
        for old in before.iter().filter(|l| !Arc::ptr_eq(&l.volume, &host)) {
            assert!(
                !after.iter().any(|l| Arc::ptr_eq(&l.volume, &old.volume)),
                "the old part is gone"
            );
        }
    }

    /// A level replaced by one with a whole volume clears the parts it installed.
    #[test]
    fn a_whole_volume_after_a_split_one_clears_the_part_layers() {
        let (root, mut server) = project("gi-level-split-then-whole");
        write_split(&root, 24);
        tiny_volume()
            .write(&root.join("assets").join("maps").join("b.fgiv"))
            .expect("write b");
        let mut state = LevelBakedGiState::default();
        let mut runtime = RuntimeRenderer::default();
        let wanted = |volume: &str| LevelBakedGi {
            volume: volume.to_string(),
            settings: LevelBakedGiSettings::default(),
        };
        state.reconcile(Some(wanted("maps/a.fgiv")), Some(&mut server), &mut runtime);
        assert_eq!(runtime.0.baked_gi_layers().len(), 1);
        state.reconcile(Some(wanted("maps/b.fgiv")), Some(&mut server), &mut runtime);
        assert!(state.installed);
        assert_eq!(state.probes, 4);
        assert!(runtime.0.baked_gi_layers().is_empty());
    }

    #[test]
    fn overrides_leave_unset_fields_alone() {
        let base = BakedGiSettings {
            intensity: 2.0,
            ..BakedGiSettings::default()
        };
        let over = LevelBakedGiSettings {
            emissive_scale: Some(0.5),
            replace_live_fill: Some(false),
            ..LevelBakedGiSettings::default()
        };
        let got = with_overrides(base, &over);
        assert_eq!(got.intensity, 2.0);
        assert_eq!(got.emissive_scale, 0.5);
        assert!(!got.replace_live_fill);
        assert_eq!(got.sky_scale, base.sky_scale);
    }

    /// A terrain volume of 24 × 4 four-metre columns with one long brick across most of it that is
    /// larger than the cap (ADR-3237): no split that keeps it whole fits, so the brick is cropped at
    /// the cut. Written as `maps/a.fgiv` plus its part files.
    fn write_cropped_split(root: &std::path::Path) -> Vec<BakedGiVolume> {
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [24, 4],
                levels: vec![1.0],
                ground: vec![0.0; 96],
            }),
            bricks: vec![regia_asset::baked_gi::DetailBrick {
                origin: [2.0, 0.0, 2.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [85, 6, 6],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let sh: ShRgb = [[0.5, 0.0, 0.0, 0.0]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [sh; GI_COMPONENTS],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        let whole = BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
            .expect("volume");
        let cap = layout.bricks[0].probe_count() as u64 * 136 * 7 / 10;
        let parts = whole.split_parts(cap).expect("the brick is cropped at the cut");
        assert!(
            parts.iter().any(|p| p
                .split
                .as_ref()
                .is_some_and(|s| s.cropped_bricks(&p.layout()) > 0)),
            "a part holds a cropped brick"
        );
        regia_asset::baked_gi_split::write_split_set(
            &parts,
            &root.join("assets").join("maps").join("a.fgiv"),
        )
        .expect("write parts");
        parts
    }

    /// ADR-3237: a split whose parts hold cropped bricks (format version 5) installs every part like
    /// any split, and the level counts every part's probes.
    #[test]
    fn a_cropped_split_volume_installs_every_part() {
        let (root, server) = project("gi-level-split-cropped");
        let parts = write_cropped_split(&root);
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app));
        assert_eq!(layer_count(&app), parts.len() - 1);
        let state = app.world().resource::<LevelBakedGiState>();
        assert_eq!(state.error, None);
        assert!(state.failed_parts.is_empty());
        assert_eq!(
            state.probes,
            parts.iter().map(BakedGiVolume::probe_count).sum::<usize>()
        );
        let installed: Vec<&Arc<BakedGiVolume>> = state.installed_volumes().collect();
        assert_eq!(installed.len(), parts.len());
        for (got, want) in installed.iter().zip(&parts) {
            assert_eq!(got.as_ref(), want);
        }
    }

    /// Part files a bake wrote before ADR-3237 (format version 4) still install as a split set.
    #[test]
    fn version_4_part_files_still_install() {
        let (root, server) = project("gi-level-split-v4");
        let parts = write_split(&root, 24);
        for (k, part) in parts.iter().enumerate() {
            let at = regia_asset::baked_gi_split::split_part_path(
                &root.join("assets").join("maps").join("a.fgiv"),
                k as u32,
            );
            std::fs::write(&at, part.to_bytes_v4().expect("v4 part")).expect("rewrite as v4");
        }
        let (mut app, id) = app_with(server, &level_text(""));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(installed(&app));
        assert_eq!(layer_count(&app), parts.len() - 1);
        let state = app.world().resource::<LevelBakedGiState>();
        assert_eq!(state.error, None);
        assert_eq!(state.installed_volumes().count(), parts.len());
    }

    #[test]
    fn asset_paths_normalize() {
        assert_eq!(relative_path("assets://maps/a.fgiv"), "maps/a.fgiv");
        assert_eq!(relative_path("./assets/maps\\a.fgiv"), "maps/a.fgiv");
        assert_eq!(relative_path("maps/a.fgiv"), "maps/a.fgiv");
    }
}
