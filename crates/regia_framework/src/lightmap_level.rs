// SPDX-License-Identifier: MPL-2.0
//! Installs a level's lightmap (`LevelLook::lightmap`, scene format v11, ADR-2313;
//! `book/agents/lightmaps.md`).
//!
//! [`LightmapLevelPlugin`] watches the open level's [`LevelSettings`]. When the level names a
//! `.flmap` it:
//!
//! 1. asks the renderer to warm the lightmap variants (`FrameRenderer::set_warm_lightmaps(true)`),
//!    so the first receiver does not keep the probe light while a variant compiles (a
//!    `RuntimeRenderer` inserted after the lightmap was installed is asked on the next sync);
//! 2. loads the lightmap through the [`AssetServer`] (the cooked asset, from a pak or the dev
//!    cache; an uncooked `assets/<path>` file in a dev run) and installs it in
//!    [`InstalledLightmap`], with the level's overrides on [`LightmapSettings`];
//! 3. attaches a [`LightmapReceiver`] to every drawn entity an entry was baked for: the level
//!    entity's own [`MeshRenderer`] (part 0) and every [`MeshPartOf`] part entity (part `n`), as
//!    they appear (a host or the editor resolves meshes frames after the level opens). An entity
//!    that already has a receiver (saved in the level, or set by the host) keeps it.
//!
//! A new file on disk, a hot reload of the cooked asset and a changed path or override reinstall
//! it; closing the level removes the lightmap this level installed, the receivers it attached and
//! the warm request, and restores the host's settings. A level that names no lightmap changes
//! nothing: no resource is inserted and the renderer is not touched.
//!
//! Additive levels (ADR-2635): every other active level whose settings name a lightmap has it
//! loaded too, and [`sync_additive_level_lightmaps`] installs one atlas holding the primary
//! level's lightmap with each additive one stacked below it
//! (`regia_render::lightmap::combine_lightmaps`), so the receivers of every level resolve by key.
//! A saved receiver of an additive level names an entry of its own level's lightmap: when stacking
//! renamed that entry (`name@<k>`, another level has the name), [`resolve_additive_receivers`] points
//! the receiver at the renamed entry. A world with no additive lightmap installs the primary's file
//! as before. An additive level's lightmap needs the primary level to name one
//! (`lightmap_no_primary` otherwise), and the primary's overrides (intensity, enabled) apply to all
//! of them.
//!
//! A missing file or a file this build does not read logs one warning and sets
//! [`LevelLightmapState::error`] with its code (`lightmap_not_found`, or the reader's
//! `lightmap_io`, `lightmap_bad_magic`, `lightmap_version`, `lightmap_header`, `lightmap_payload`,
//! `lightmap_layout`); the frame keeps the probe light.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::lightmap::Lightmap;
use regia_asset::{AssetServer, decode_cooked_file};
use regia_render::lightmap::{
    InstalledLightmap, LightmapReceiver, LightmapSettings, MeshPartOf, ReceiverCandidate, combine_lightmaps,
    has_keys, match_keyed_receivers,
};
use regia_render::{MeshRenderer, RuntimeRenderer};
use regia_scene::level::{
    LevelId, LevelLibrary, LevelLightmap, LevelManager, LevelMember, LevelSettings, LevelState,
};
use regia_scene::prelude::EntityGuid;
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{CellLoaded, CellUnloading};

use crate::baked_gi_level::{CachedSource, Source, Stamp, cache_of, resolve, source_current, stamp_of};

/// Most entry names [`LevelLightmapState::unmatched`] lists.
pub const MAX_UNMATCHED_LISTED: usize = 32;

/// The code of a lightmap the level names that is neither cooked nor on disk.
pub const LIGHTMAP_NOT_FOUND: &str = "lightmap_not_found";

/// The code of an additive level's lightmap while the primary level names none (ADR-2635).
pub const LIGHTMAP_NO_PRIMARY: &str = "lightmap_no_primary";

/// The code of an additive level's lightmap that cannot join the primary's atlas (the combined
/// atlas is too large, or the two disagree on the direct sun; ADR-2635; or it would need a page
/// of its own while this build's frame draws one page, ADR-3300).
pub const LIGHTMAP_COMBINE: &str = "lightmap_combine";

/// One additive level's lightmap (ADR-2635).
#[derive(Clone)]
struct AdditiveLightmap {
    level: LevelId,
    file: String,
    stamp: Stamp,
    map: Option<Arc<Lightmap>>,
    /// Entries the installed atlas holds under another name: the level's name, the atlas's.
    renames: Vec<(String, String)>,
    report: AdditiveLightmapReport,
    /// Where `file` was found when `stamp` was taken. A steady frame stats this instead of
    /// resolving the path again.
    cached: Option<CachedSource>,
    /// `AssetServer::manifest_epoch` when `cached` was taken: a chunk mounted since may now
    /// serve the path cooked, so a newer epoch resolves again.
    epoch: u64,
}

/// What became of an additive level's lightmap, for `render.get_lightmaps` `level.additive`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct AdditiveLightmapReport {
    /// The level (its id).
    pub level: String,
    /// The lightmap path the level names.
    pub file: String,
    /// Its entries are in the installed atlas.
    pub installed: bool,
    /// Its entries.
    pub entries: usize,
    /// First atlas row of its texels in the installed atlas.
    pub row: u32,
    /// Its atlas rows.
    pub rows: u32,
    /// Its entries renamed `name@<k>` because an earlier level has an entry of that name.
    pub renamed: usize,
    /// The installed atlas's page of its first page (ADR-3300): 0 while the levels stack in one
    /// page, else its pages follow the earlier levels' (`row` is then 0).
    pub page: u32,
    /// Its pages in the installed atlas (0 when not installed).
    pub pages: u32,
    /// Why it is not installed.
    pub error: Option<String>,
    /// A stable code for `error` (the reader's codes, `lightmap_not_found`,
    /// [`LIGHTMAP_NO_PRIMARY`], [`LIGHTMAP_COMBINE`]).
    pub error_code: Option<&'static str>,
}

/// What the level's lightmap install did, for the agent protocol (`render.get_lightmaps` `level`)
/// and host UIs.
#[derive(Resource, Default)]
pub struct LevelLightmapState {
    /// The lightmap path the open level names, as written in the level; `None` for none.
    pub file: Option<String>,
    /// The level's lightmap is the installed one.
    pub installed: bool,
    /// Why the lightmap could not be loaded; `None` when it loaded or none is named.
    pub error: Option<String>,
    /// A stable code for `error` (see the module docs).
    pub error_code: Option<&'static str>,
    /// Entries in the installed lightmap.
    pub entries: usize,
    /// Of those, entries keyed to a level entity and part.
    pub keyed_entries: usize,
    /// Receivers this level attached by key (and still holds).
    pub attached: usize,
    /// Keyed entries whose entity and part no drawn entity has (at most
    /// [`MAX_UNMATCHED_LISTED`]): a mesh not resolved yet, or a level edited since the bake.
    pub unmatched: Vec<String>,
    /// The level asked the renderer to warm the lightmap variants.
    pub warm: bool,
    wanted: Option<LevelLightmap>,
    stamp: Option<Stamp>,
    /// Cached source for `stamp`. A steady primary stats this instead of resolving again.
    source_cache: Option<CachedSource>,
    /// `AssetServer::manifest_epoch` when `source_cache` was taken (see `AdditiveLightmap::epoch`).
    source_epoch: u64,
    ours: Option<Arc<Lightmap>>,
    /// Bumped when `ours` changes, so receivers are matched again.
    generation: u64,
    matched_generation: u64,
    restore_settings: Option<LightmapSettings>,
    restore_warm: Option<bool>,
    /// The receivers this level inserted: the entity and the entry it was given.
    attached_entities: Vec<(Entity, String)>,
    /// The primary level's own lightmap (`ours` is it, or the atlas combined from it).
    primary: Option<Arc<Lightmap>>,
    /// The additive levels' lightmaps, in load order (ADR-2635).
    additive: Vec<AdditiveLightmap>,
    /// Scratch order of the additive levels this frame: load generation, id, level. Copy keys
    /// only, so a steady frame does not allocate the path strings.
    additive_order: Vec<(u64, u128, LevelId)>,
    /// The primary map `ours` was last built from (its address), to rebuild when it changes.
    combined_for: Option<usize>,
    /// The `generation` the additive levels' receivers were last resolved at.
    resolved_generation: u64,
    /// Additive levels may append atlas pages (ADR-3300); `None` follows
    /// `LIGHTMAP_PAGED_BINDINGS`.
    append_pages: Option<bool>,
    /// The streamed cells' lightmaps, by cell (ADR-3318).
    cells: std::collections::BTreeMap<CellCoord, CellLightmap>,
    /// The cell stack's layout.
    cell_stack: CellLightmapStack,
    /// The atlas the cells were stacked under.
    cell_base: Option<Arc<Lightmap>>,
    /// The stacked atlas installed (`ours`), `None` while no cell is stacked.
    cell_stacked: Option<Arc<Lightmap>>,
}

impl LevelLightmapState {
    /// Whether an additive level's lightmap may join on pages of its own when one atlas would
    /// overflow (ADR-3300). The default follows the frame's binding form
    /// (`regia_render::lightmap::LIGHTMAP_PAGED_BINDINGS`): a frame that binds one 2-D atlas draws
    /// page 0 only, so there the stack is refused ([`LIGHTMAP_COMBINE`]) as before pages.
    pub fn set_append_pages(&mut self, on: bool) {
        self.append_pages = Some(on);
        self.combined_for = None;
    }

    /// See [`Self::set_append_pages`].
    #[must_use]
    pub fn append_pages(&self) -> bool {
        self.append_pages
            .unwrap_or(regia_render::lightmap::LIGHTMAP_PAGED_BINDINGS)
    }

    /// `true` when the installed lightmap is the one this level installed.
    #[must_use]
    pub fn owns(&self, installed: Option<&Arc<Lightmap>>) -> bool {
        match (&self.ours, installed) {
            (Some(ours), Some(installed)) => Arc::ptr_eq(ours, installed),
            _ => false,
        }
    }

    /// The entities this level gave a receiver, with the entry each got (sorted by entity).
    #[must_use]
    pub fn attached_entities(&self) -> &[(Entity, String)] {
        &self.attached_entities
    }

    /// The status as `render.get_lightmaps` reports it under `level`.
    #[must_use]
    pub fn report(&self) -> LevelLightmapReport {
        LevelLightmapReport {
            file: self.file.clone(),
            installed: self.installed,
            error: self.error.clone(),
            error_code: self.error_code,
            entries: self.entries,
            keyed_entries: self.keyed_entries,
            attached: self.attached,
            unmatched: self.unmatched.clone(),
            warm: self.warm,
        }
    }

    /// The additive levels' lightmaps (ADR-2635), in load order, as `render.get_lightmaps`
    /// reports them under `level.additive`; empty when no additive level names one.
    #[must_use]
    pub fn additive_report(&self) -> Vec<AdditiveLightmapReport> {
        self.additive.iter().map(|a| a.report.clone()).collect()
    }
}

/// [`LevelLightmapState`] as the agent protocol reports it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct LevelLightmapReport {
    /// [`LevelLightmapState::file`].
    pub file: Option<String>,
    /// [`LevelLightmapState::installed`].
    pub installed: bool,
    /// [`LevelLightmapState::error`].
    pub error: Option<String>,
    /// [`LevelLightmapState::error_code`].
    pub error_code: Option<&'static str>,
    /// [`LevelLightmapState::entries`].
    pub entries: usize,
    /// [`LevelLightmapState::keyed_entries`].
    pub keyed_entries: usize,
    /// [`LevelLightmapState::attached`].
    pub attached: usize,
    /// [`LevelLightmapState::unmatched`].
    pub unmatched: Vec<String>,
    /// [`LevelLightmapState::warm`].
    pub warm: bool,
}

/// Installs the open level's lightmap and attaches its receivers ([`sync_level_lightmap`],
/// [`attach_level_receivers`]).
#[derive(Default)]
pub struct LightmapLevelPlugin;

impl Plugin for LightmapLevelPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<CellLoaded>().add_message::<CellUnloading>();
        app.init_resource::<LevelLightmapState>()
            .init_resource::<StreamedLightmapCells>()
            .add_systems(
                Update,
                (
                    sync_level_lightmap,
                    sync_additive_level_lightmaps,
                    sync_cell_lightmaps,
                    resolve_additive_receivers,
                    attach_level_receivers,
                )
                    .chain(),
            );
    }
}

/// The resources [`sync_level_lightmap`] changes.
type SyncTargets<'w> = (
    Option<ResMut<'w, RuntimeRenderer>>,
    Option<ResMut<'w, InstalledLightmap>>,
    Option<ResMut<'w, LightmapSettings>>,
);

/// `Update`: reconciles the installed lightmap, the lightmap settings and the warm request with the
/// open level's `look.lightmap`. Idle (no resource touched) while no level names one.
pub fn sync_level_lightmap(
    level: Option<Res<LevelSettings>>,
    manager: Option<Res<LevelManager>>,
    mut assets: Option<ResMut<AssetServer>>,
    (mut runtime, mut installed, mut settings): SyncTargets<'_>,
    mut state: ResMut<LevelLightmapState>,
    mut commands: Commands,
) {
    // The settings resource outlives the level that wrote it: a level-less world names none.
    let open = manager.is_none_or(|m| m.current().is_some());
    let named = level
        .as_deref()
        .filter(|_| open)
        .and_then(|settings| settings.look.lightmap.as_ref())
        .filter(|lightmap| !lightmap.file.trim().is_empty());
    let epoch = assets.as_deref().map_or(0, AssetServer::manifest_epoch);
    // The warm request is made as soon as a level with a lightmap is open, before the file loads,
    // and before the unchanged-level early return below: a renderer inserted after the install
    // still gets it (w7-runtime-smalls-z item 8).
    if named.is_some()
        && !state.warm
        && let Some(runtime) = runtime.as_deref_mut()
    {
        state.restore_warm = Some(runtime.0.warm_lightmaps());
        runtime.0.set_warm_lightmaps(true);
        state.warm = true;
    }
    if named.is_some_and(|wanted| {
        state.wanted.as_ref() == Some(wanted)
            && state.source_epoch == epoch
            && state.stamp.as_ref().is_some_and(|stamp| {
                state
                    .source_cache
                    .as_ref()
                    .is_some_and(|cached| source_current(cached, stamp, assets.as_deref()))
            })
    }) {
        return;
    }
    let wanted = named.cloned();
    let Some(wanted) = wanted else {
        if state.wanted.is_some() || state.ours.is_some() || state.warm {
            let _span = tracing::info_span!("level_lightmap_remove").entered();
            state.remove_ours(
                installed.as_deref_mut(),
                settings.as_deref_mut(),
                runtime.as_deref_mut(),
            );
            let generation = state.generation + 1;
            let attached = std::mem::take(&mut state.attached_entities);
            *state = LevelLightmapState {
                generation,
                matched_generation: generation,
                attached_entities: attached,
                ..LevelLightmapState::default()
            };
        }
        return;
    };
    let source = resolve(&wanted.file, assets.as_deref_mut());
    let stamp = stamp_of(&source, assets.as_deref());
    state.source_cache = Some(cache_of(&wanted.file, &source, assets.as_deref()));
    state.source_epoch = epoch;
    if state.wanted.as_ref() == Some(&wanted) && state.stamp.as_ref() == Some(&stamp) {
        return;
    }
    let _span = tracing::info_span!("level_lightmap_install").entered();
    let same_file = state.wanted.as_ref().is_some_and(|w| w.file == wanted.file);
    state.wanted = Some(wanted.clone());
    state.stamp = Some(stamp);
    state.file = Some(wanted.file.clone());
    let base = match settings.as_deref() {
        Some(s) => *state.restore_settings.get_or_insert(*s),
        None => *state
            .restore_settings
            .get_or_insert_with(LightmapSettings::default),
    };
    let level_settings = with_overrides(base, &wanted);
    match settings.as_deref_mut() {
        Some(s) => *s = level_settings,
        None => commands.insert_resource(level_settings),
    }
    match load(&wanted.file, &source, assets.as_deref_mut()) {
        Ok(map) => {
            let map = Arc::new(map);
            state.entries = map.entries.len();
            state.keyed_entries = map.entries.iter().filter(|e| e.key.is_some()).count();
            state.error = None;
            state.error_code = None;
            let label = format!("assets/{}", crate::baked_gi_level::relative_path(&wanted.file));
            match installed.as_deref_mut() {
                Some(i) => {
                    i.lightmap = Some(map.clone());
                    i.source = Some(label);
                    i.error = None;
                }
                None => commands.insert_resource(InstalledLightmap {
                    lightmap: Some(map.clone()),
                    source: Some(label),
                    error: None,
                }),
            }
            state.primary = Some(map.clone());
            state.ours = Some(map);
            state.installed = true;
            state.generation += 1;
        }
        Err((code, error)) => {
            tracing::warn!(
                target: "regia_framework",
                "level lightmap {}: {error}; receivers keep the probe light",
                wanted.file
            );
            state.error = Some(error);
            state.error_code = Some(code);
            // A failed reload of the same file keeps the previous one; another path was named by
            // a different level, whose lightmap must not light this one.
            if !same_file && state.ours.is_some() {
                if let Some(i) = installed.as_deref_mut()
                    && state.owns(i.lightmap.as_ref())
                {
                    i.clear();
                }
                state.ours = None;
                state.primary = None;
                state.generation += 1;
            }
            state.installed = state.ours.is_some();
            state.entries = state.ours.as_ref().map_or(0, |m| m.entries.len());
            state.keyed_entries = state
                .ours
                .as_ref()
                .map_or(0, |m| m.entries.iter().filter(|e| e.key.is_some()).count());
        }
    }
}

/// The host's settings with the level's overrides on top: a level that names a lightmap turns
/// lightmaps on unless it says `enabled: Some(false)`.
fn with_overrides(base: LightmapSettings, level: &LevelLightmap) -> LightmapSettings {
    LightmapSettings {
        enabled: level.enabled.unwrap_or(true),
        intensity: level
            .intensity
            .filter(|i| i.is_finite() && *i >= 0.0)
            .unwrap_or(base.intensity),
        ..base
    }
}

/// Reads the named lightmap from its source.
fn load(
    path: &str,
    source: &Source,
    assets: Option<&mut AssetServer>,
) -> Result<Lightmap, (&'static str, String)> {
    let _span = tracing::info_span!("level_lightmap_load").entered();
    let failed = |code: &'static str, e: String| (code, format!("{path}: {e}"));
    match source {
        Source::Cooked(id) => {
            let server = assets.ok_or_else(|| failed("lightmap_io", "no asset server".to_string()))?;
            let (bytes, _) = server
                .read_cooked(*id)
                .map_err(|e| failed("lightmap_io", e.to_string()))?;
            let (_, payload) =
                decode_cooked_file(&bytes).map_err(|e| failed("lightmap_io", e.to_string()))?;
            Lightmap::from_bytes(payload).map_err(|e| failed(e.code(), e.to_string()))
        }
        Source::Raw(file) => Lightmap::read(file).map_err(|e| failed(e.code(), e.to_string())),
        Source::Missing => Err(failed(LIGHTMAP_NOT_FOUND, "file not found".to_string())),
    }
}

impl LevelLightmapState {
    /// Removes the lightmap this level installed (not a host's), restores the host's settings
    /// and warm flag. The attached receivers are removed by [`attach_level_receivers`].
    fn remove_ours(
        &mut self,
        installed: Option<&mut InstalledLightmap>,
        settings: Option<&mut LightmapSettings>,
        runtime: Option<&mut RuntimeRenderer>,
    ) {
        if let Some(i) = installed
            && self.owns(i.lightmap.as_ref())
        {
            i.clear();
        }
        self.ours = None;
        self.primary = None;
        if let (Some(restore), Some(settings)) = (self.restore_settings.take(), settings) {
            *settings = restore;
        }
        if self.warm
            && let Some(runtime) = runtime
        {
            runtime
                .0
                .set_warm_lightmaps(self.restore_warm.take().unwrap_or(false));
        }
        self.warm = false;
        self.installed = false;
    }
}

/// The additive levels' lightmaps the world wants: every active level other than the primary
/// whose settings name a lightmap (not `enabled: Some(false)`), in load order.
fn wanted_additive(manager: Option<&LevelManager>, library: Option<&LevelLibrary>) -> Vec<(LevelId, String)> {
    let (Some(manager), Some(library)) = (manager, library) else {
        return Vec::new();
    };
    let current = manager.current();
    let mut wanted: Vec<(u64, u128, LevelId, String)> = manager
        .levels()
        .filter(|(id, state)| Some(*id) != current && *state == LevelState::Active)
        .filter_map(|(id, _)| {
            let lightmap = library.get(id)?.settings.look.lightmap.clone()?;
            (!lightmap.file.trim().is_empty() && lightmap.enabled != Some(false))
                .then(|| (manager.load_generation(id), id.0.as_u128(), id, lightmap.file))
        })
        .collect();
    wanted.sort_by_key(|(generation, id, _, _)| (*generation, *id));
    wanted.into_iter().map(|(_, _, id, file)| (id, file)).collect()
}

/// Fills `order` with the additive levels that name a lightmap, in the same order as
/// [`wanted_additive`]. The entries are `Copy`, so a warm buffer is not reallocated.
fn fill_additive_order(
    order: &mut Vec<(u64, u128, LevelId)>,
    manager: Option<&LevelManager>,
    library: Option<&LevelLibrary>,
) {
    order.clear();
    let (Some(manager), Some(library)) = (manager, library) else {
        return;
    };
    let current = manager.current();
    for (id, level_state) in manager.levels() {
        if Some(id) == current || level_state != LevelState::Active {
            continue;
        }
        let Some(lightmap) = library
            .get(id)
            .and_then(|level| level.settings.look.lightmap.as_ref())
        else {
            continue;
        };
        if lightmap.file.trim().is_empty() || lightmap.enabled == Some(false) {
            continue;
        }
        order.push((manager.load_generation(id), id.0.as_u128(), id));
    }
    // `sort_by_key` allocates a key vector. Generation then id is the same order, in place.
    order.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
}

/// Whether the stored additive lightmaps already match `order`, the primary atlas, and each
/// file's cached stamp. A raw file is statted in place.
fn additive_sources_current(
    order: &[(u64, u128, LevelId)],
    additive: &[AdditiveLightmap],
    library: Option<&LevelLibrary>,
    assets: Option<&AssetServer>,
    epoch: u64,
    combined_for: Option<usize>,
    primary_key: Option<usize>,
) -> bool {
    if order.len() != additive.len() || combined_for != primary_key {
        return false;
    }
    let Some(library) = library else {
        return order.is_empty();
    };
    order.iter().zip(additive).all(|((_, _, id), prev)| {
        prev.level == *id
            && prev.epoch == epoch
            && library
                .get(*id)
                .and_then(|level| level.settings.look.lightmap.as_ref())
                .is_some_and(|lightmap| lightmap.file == prev.file)
            && prev
                .cached
                .as_ref()
                .is_some_and(|cached| source_current(cached, &prev.stamp, assets))
    })
}

/// `Update`, after [`sync_level_lightmap`] (ADR-2635): loads the lightmaps the additive levels
/// name and installs one atlas holding the primary level's lightmap and each of them
/// (`combine_lightmaps`), so the receivers of every level resolve. Idle (nothing read or touched)
/// while no additive level names a lightmap and none did.
pub fn sync_additive_level_lightmaps(
    manager: Option<Res<LevelManager>>,
    library: Option<Res<LevelLibrary>>,
    mut assets: Option<ResMut<AssetServer>>,
    mut installed: Option<ResMut<InstalledLightmap>>,
    mut state: ResMut<LevelLightmapState>,
) {
    fill_additive_order(&mut state.additive_order, manager.as_deref(), library.as_deref());
    if state.additive_order.is_empty() && state.additive.is_empty() {
        return;
    }
    let primary_key = state.primary.as_ref().map(|map| Arc::as_ptr(map) as usize);
    let epoch = assets.as_deref().map_or(0, AssetServer::manifest_epoch);
    if additive_sources_current(
        &state.additive_order,
        &state.additive,
        library.as_deref(),
        assets.as_deref(),
        epoch,
        state.combined_for,
        primary_key,
    ) {
        return;
    }
    let wanted = wanted_additive(manager.as_deref(), library.as_deref());
    if wanted.is_empty() && state.additive.is_empty() {
        return;
    }
    let mut changed = wanted.len() != state.additive.len();
    let mut next = Vec::with_capacity(wanted.len());
    for (k, (level, file)) in wanted.into_iter().enumerate() {
        let source = resolve(&file, assets.as_deref_mut());
        let stamp = stamp_of(&source, assets.as_deref());
        if let Some(prev) = state.additive.get(k)
            && prev.level == level
            && prev.file == file
            && prev.stamp == stamp
        {
            let mut kept = prev.clone();
            kept.cached = Some(cache_of(&kept.file, &source, assets.as_deref()));
            kept.epoch = epoch;
            next.push(kept);
            continue;
        }
        changed = true;
        let _span = tracing::info_span!("level_lightmap_additive_load").entered();
        let mut report = AdditiveLightmapReport {
            level: level.0.to_string(),
            file: file.clone(),
            ..AdditiveLightmapReport::default()
        };
        let map = match load(&file, &source, assets.as_deref_mut()) {
            Ok(map) => {
                report.entries = map.entries.len();
                Some(Arc::new(map))
            }
            Err((code, error)) => {
                tracing::warn!(
                    target: "regia_framework",
                    "additive level lightmap {file}: {error}; its receivers keep the probe light"
                );
                report.error = Some(error);
                report.error_code = Some(code);
                None
            }
        };
        let cached = Some(cache_of(&file, &source, assets.as_deref()));
        next.push(AdditiveLightmap {
            level,
            file,
            stamp,
            map,
            renames: Vec::new(),
            report,
            cached,
            epoch,
        });
    }
    let primary_key = state.primary.as_ref().map(|p| Arc::as_ptr(p) as usize);
    if !changed && state.combined_for == primary_key {
        state.additive = next;
        return;
    }
    let _span = tracing::info_span!("level_lightmap_combine", additive = next.len()).entered();
    state.combined_for = primary_key;
    for a in &mut next {
        a.report.installed = false;
        a.report.row = 0;
        a.report.rows = 0;
        a.report.renamed = 0;
        a.report.page = 0;
        a.report.pages = 0;
        a.renames.clear();
        if a.map.is_some() {
            a.report.error = None;
            a.report.error_code = None;
        }
    }
    let Some(primary) = state.primary.clone() else {
        for a in next.iter_mut().filter(|a| a.map.is_some()) {
            a.report.error = Some("the primary level names no lightmap to join".into());
            a.report.error_code = Some(LIGHTMAP_NO_PRIMARY);
        }
        state.additive = next;
        return;
    };
    let joined: Vec<usize> = (0..next.len()).filter(|&i| next[i].map.is_some()).collect();
    let target = if joined.is_empty() {
        primary
    } else {
        let mut sources: Vec<&Lightmap> = vec![primary.as_ref()];
        sources.extend(joined.iter().filter_map(|&i| next[i].map.as_deref()));
        let append = state.append_pages();
        let combined = combine_lightmaps(&sources).and_then(|(combined, regions)| {
            let one_page = sources.iter().all(|m| m.pages() <= 1);
            if combined.pages() > 1 && one_page && !append {
                return Err(regia_asset::lightmap::LightmapError::Layout(format!(
                    "the stacked lightmaps need {} atlas pages, and this build's frame draws one (ADR-3300)",
                    combined.pages()
                )));
            }
            Ok((combined, regions))
        });
        match combined {
            Ok((combined, regions)) => {
                for (&i, region) in joined.iter().zip(regions.iter().skip(1)) {
                    let at = &combined.entries[region.first_entry..region.first_entry + region.entries];
                    let renames: Vec<(String, String)> = next[i]
                        .map
                        .iter()
                        .flat_map(|map| map.entries.iter().zip(at))
                        .filter(|(own, stacked)| own.name != stacked.name)
                        .map(|(own, stacked)| (own.name.clone(), stacked.name.clone()))
                        .collect();
                    let additive = &mut next[i];
                    additive.renames = renames;
                    let report = &mut additive.report;
                    report.installed = true;
                    report.row = region.row;
                    report.rows = region.rows;
                    report.renamed = region.renamed;
                    report.page = region.page;
                    report.pages = region.pages;
                }
                Arc::new(combined)
            }
            Err(e) => {
                tracing::warn!(target: "regia_framework", "additive level lightmaps: {e}; only the primary's is installed");
                for &i in &joined {
                    next[i].report.error = Some(e.to_string());
                    next[i].report.error_code = Some(LIGHTMAP_COMBINE);
                }
                primary
            }
        }
    };
    state.additive = next;
    let ours_installed = installed
        .as_deref()
        .is_some_and(|i| state.owns(i.lightmap.as_ref()));
    if state.ours.as_ref().is_some_and(|o| Arc::ptr_eq(o, &target)) {
        return;
    }
    if ours_installed && let Some(i) = installed.as_deref_mut() {
        i.lightmap = Some(target.clone());
    }
    state.entries = target.entries.len();
    state.keyed_entries = target.entries.iter().filter(|e| e.key.is_some()).count();
    state.ours = Some(target);
    state.generation += 1;
}

/// The entry name a level wrote: `entry` without the `@<k>` stacking adds, unless the level's own
/// lightmap has an entry of that very name. A name saved while its level was stacked comes back
/// with the suffix (and another `k` may hold now).
fn own_entry_name<'a>(entry: &'a str, own: Option<&Lightmap>) -> &'a str {
    if own.is_some_and(|map| map.entry(entry).is_some()) {
        return entry;
    }
    match entry.rsplit_once('@') {
        Some((name, k)) if !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => entry,
    }
}

/// `Update`, after [`sync_additive_level_lightmaps`] (ADR-2635): each additive level's receivers
/// resolve in that level's lightmap, not in whichever level holds the same entry name first. Idle
/// while no additive level names a lightmap, and when neither the atlas nor the receivers changed.
pub fn resolve_additive_receivers(
    mut state: ResMut<LevelLightmapState>,
    mut receivers: Query<(&LevelMember, &mut LightmapReceiver)>,
) {
    if state.additive.is_empty() {
        return;
    }
    let _span = tracing::info_span!("level_lightmap_resolve_additive").entered();
    let atlas_changed = state.resolved_generation != state.generation;
    state.resolved_generation = state.generation;
    for (member, mut receiver) in &mut receivers {
        if !atlas_changed && !receiver.is_added() {
            continue;
        }
        let Some(additive) = state.additive.iter().find(|a| a.level.0.as_u128() == member.0) else {
            continue;
        };
        // A level whose map is not in the atlas (it failed to load or to combine) keeps its
        // receivers' names: stripping a stacked `@<k>` would name the primary's entry of the same
        // name and draw that level's texels.
        if !additive.report.installed {
            continue;
        }
        let own = own_entry_name(&receiver.entry, additive.map.as_deref());
        let name = additive
            .renames
            .iter()
            .find(|(from, _)| from == own)
            .map_or(own, |(_, to)| to.as_str());
        if name != receiver.entry {
            let name = name.to_string();
            receiver.entry = name;
        }
    }
}

/// A drawn entity as [`attach_level_receivers`] reads it.
type DrawnItem = (
    Entity,
    Option<&'static EntityGuid>,
    Option<&'static MeshPartOf>,
    Option<&'static LightmapReceiver>,
);

/// Entities that appeared since the last run and may be receivers.
type NewDrawn = Or<(Added<MeshRenderer>, Added<MeshPartOf>, Added<EntityGuid>)>;

/// `Update`, after [`sync_level_lightmap`]: gives every drawn entity the level's lightmap has a
/// keyed entry for a [`LightmapReceiver`] (unless it has one), when the lightmap changes or new
/// meshes appear; takes back the receivers it gave when the level's lightmap goes away.
pub fn attach_level_receivers(
    mut state: ResMut<LevelLightmapState>,
    added: Query<(), (NewDrawn, With<MeshRenderer>)>,
    drawn: Query<DrawnItem, With<MeshRenderer>>,
    guids: Query<&EntityGuid>,
    receivers: Query<&LightmapReceiver>,
    mut commands: Commands,
) {
    let Some(map) = state.ours.clone() else {
        if !state.attached_entities.is_empty() {
            let _span = tracing::info_span!("level_lightmap_detach").entered();
            for (entity, entry) in std::mem::take(&mut state.attached_entities) {
                if receivers.get(entity).is_ok_and(|r| r.entry == entry) {
                    commands.entity(entity).try_remove::<LightmapReceiver>();
                }
            }
            state.attached = 0;
        }
        return;
    };
    if state.matched_generation == state.generation && added.is_empty() {
        return;
    }
    let _span = tracing::info_span!("level_lightmap_attach").entered();
    state.matched_generation = state.generation;
    // Receivers from an earlier lightmap of this level whose entry changed are given again.
    let mut kept: Vec<(Entity, String)> = Vec::new();
    for (entity, entry) in std::mem::take(&mut state.attached_entities) {
        match receivers.get(entity) {
            Ok(r) if r.entry == entry && map.entry(&entry).is_some() => kept.push((entity, entry)),
            Ok(r) if r.entry == entry => {
                commands.entity(entity).try_remove::<LightmapReceiver>();
            }
            _ => {}
        }
    }
    let candidates: Vec<ReceiverCandidate> = if has_keys(&map) {
        drawn
            .iter()
            .filter_map(|(entity, guid, part, _)| match part {
                Some(of) => guids.get(of.owner).ok().map(|g| ReceiverCandidate {
                    entity,
                    owner: of.owner,
                    part: of.part,
                    guid: g.0,
                }),
                None => guid.map(|g| ReceiverCandidate {
                    entity,
                    owner: entity,
                    part: 0,
                    guid: g.0,
                }),
            })
            .collect()
    } else {
        Vec::new()
    };
    let found = match_keyed_receivers(&map, candidates);
    for receiver in &found {
        if kept.iter().any(|(e, _)| *e == receiver.entity) {
            continue;
        }
        let has_own = drawn
            .get(receiver.entity)
            .is_ok_and(|(_, _, _, existing)| existing.is_some());
        if !has_own {
            commands.entity(receiver.entity).try_insert(LightmapReceiver {
                entry: receiver.entry.clone(),
            });
            kept.push((receiver.entity, receiver.entry.clone()));
        }
    }
    kept.sort_by_key(|(e, _)| *e);
    state.attached = kept.len();
    state.attached_entities = kept;
    state.unmatched = map
        .entries
        .iter()
        .filter(|e| e.key.is_some() && !found.iter().any(|r| r.entry == e.name))
        .map(|e| e.name.clone())
        .take(MAX_UNMATCHED_LISTED)
        .collect();
}

// ---------------------------------------------------------------------------------------------
// Per-cell lightmaps of a streamed level (ADR-3318).
// ---------------------------------------------------------------------------------------------

/// The code of a streamed cell whose lightmap does not fit the stacked atlas (the atlas would pass
/// 8192 texels; ADR-3318). The cell keeps the probe light.
pub const LIGHTMAP_CELL_STACK_FULL: &str = "lightmap_cell_stack_full";

/// Slots a new cell stack holds before it grows (a cell and its eight neighbors).
pub const CELL_STACK_INITIAL_SLOTS: usize = 9;

/// The lightmap path of `cell` for a level whose lightmap is `primary`: the file
/// `regia bake-lightmaps --cells` writes (`maps/world.flmap` → `maps/world.cells/<x>_<z>.flmap`).
#[must_use]
pub fn cell_lightmap_path(primary: &str, cell: CellCoord) -> String {
    let stem = primary.strip_suffix(".flmap").unwrap_or(primary);
    format!("{stem}.cells/{}.flmap", cell.label())
}

/// Where a stacked cell's texels are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CellStackRegion {
    /// The slot.
    pub slot: usize,
    /// First atlas row.
    pub row: u32,
    /// Atlas rows of the cell's own map.
    pub rows: u32,
    /// Index of the cell's first entry in the stacked atlas.
    pub first_entry: usize,
    /// Its entries.
    pub entries: usize,
    /// Its entries renamed `name@<x>_<z>` because the base or another cell has the name.
    pub renamed: usize,
}

/// The layout of a cell stack (ADR-3318): the base atlas (the level's own lightmap) on top, then
/// fixed slots of `slot_width × slot_height` texels, one cell each. A loaded cell keeps its slot,
/// the atlas keeps its size and exposure while cells come and go, so every other cell's texels and
/// `scale_offset` stay bit for bit and their frames do not move. A cell larger than the slot, a base
/// that changes, a darker exposure or a full stack re-lays the atlas once (`relayouts`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct CellLightmapStack {
    /// Atlas width, texels.
    pub width: u32,
    /// Atlas height, texels.
    pub height: u32,
    /// Rows of the base atlas.
    pub base_rows: u32,
    /// Slot size, texels.
    pub slot_width: u32,
    /// Slot size, texels.
    pub slot_height: u32,
    /// The exposure every texel is stored at.
    pub exposure: f32,
    /// The cell in each slot.
    pub slots: Vec<Option<CellCoord>>,
    /// Times the layout changed (a size, the base or the exposure).
    pub relayouts: u32,
    /// The base the layout was made for (its address and size).
    #[serde(skip)]
    base_key: Option<(usize, u32, u32)>,
}

impl CellLightmapStack {
    /// The slot `cell` holds.
    #[must_use]
    pub fn slot_of(&self, cell: CellCoord) -> Option<usize> {
        self.slots.iter().position(|s| *s == Some(cell))
    }
}

/// Copies `src`'s texels, coverage and directions into `dst` at `row`, re-encoding the texels when
/// the exposures differ.
fn blit(dst: &mut Lightmap, src: &Lightmap, row: u32) {
    use regia_asset::baked_gi::{from_rgb9e5, rgb9e5};
    let same = src.exposure == dst.exposure;
    let scale = dst.exposure / src.exposure;
    let neutral = regia_asset::lightmap::encode_direction([0.0; 4]);
    for y in 0..src.height {
        for x in 0..src.width {
            let s = (y * src.width + x) as usize;
            let d = ((row + y) * dst.width + x) as usize;
            let (Some(&t), Some(&c)) = (src.texels.get(s), src.coverage.get(s)) else {
                continue;
            };
            dst.texels[d] = if same {
                t
            } else {
                rgb9e5(from_rgb9e5(t).map(|v| v * scale))
            };
            dst.coverage[d] = c;
            if !dst.directions.is_empty() {
                dst.directions[d] = src.directions.get(s).copied().unwrap_or(neutral);
            }
        }
    }
}

/// Stacks the cells' lightmaps under `base` in `stack`'s slots (see [`CellLightmapStack`]): cells
/// already in a slot keep it, new ones take the lowest free slot, and the slots of cells not listed
/// are freed. Returns the atlas and each listed cell's region (`None`: it did not fit, the stack is
/// full at 8192 rows). Block-compressed sources are read through their decode.
///
/// # Errors
///
/// The sources disagree on the direct sun, or a source is wider than 8192 texels.
pub fn stack_cell_lightmaps(
    base: Option<&Lightmap>,
    cells: &[(CellCoord, &Lightmap)],
    stack: &mut CellLightmapStack,
) -> Result<(Lightmap, Vec<Option<CellStackRegion>>), String> {
    use regia_asset::lightmap::{Coverage, LightmapContents, MAX_LIGHTMAP_SIZE};
    let _span = tracing::info_span!("lightmap_cell_stack", cells = cells.len()).entered();
    let sun = |m: &Lightmap| m.bake.contents & LightmapContents::SUN_DIRECT;
    let first = base.or_else(|| cells.first().map(|c| c.1));
    if let Some(f) = first
        && cells.iter().any(|(_, m)| sun(m) != sun(f))
    {
        return Err(
            "the cells disagree on the direct sun (bake every cell with or every cell without `--direct-sun`)".into(),
        );
    }
    // The layout: kept while nothing outgrows it.
    let base_key = base.map(|b| (std::ptr::from_ref(b) as usize, b.width, b.height));
    let (bw, bh) = base.map_or((0, 0), |b| (b.width, b.height));
    let slot_w = cells
        .iter()
        .map(|c| c.1.width)
        .max()
        .unwrap_or(0)
        .max(stack.slot_width);
    let slot_h = cells
        .iter()
        .map(|c| c.1.height)
        .max()
        .unwrap_or(0)
        .max(stack.slot_height);
    let exposure = base
        .iter()
        .map(|b| b.exposure)
        .chain(cells.iter().map(|c| c.1.exposure))
        .fold(
            if stack.exposure > 0.0 {
                stack.exposure
            } else {
                f32::INFINITY
            },
            f32::min,
        );
    let width = bw.max(slot_w).max(1);
    if width > MAX_LIGHTMAP_SIZE {
        return Err(format!(
            "a cell lightmap {width} texels wide is over {MAX_LIGHTMAP_SIZE}"
        ));
    }
    let fit = |slots: usize| {
        u64::from(bh) + slots as u64 * u64::from(slot_h.max(1)) <= u64::from(MAX_LIGHTMAP_SIZE)
    };
    let mut slots = stack.slots.len().max(CELL_STACK_INITIAL_SLOTS);
    while slots < cells.len() && fit(slots * 2) {
        slots *= 2;
    }
    while !fit(slots) && slots > 1 {
        slots -= 1;
    }
    let relayout = stack.base_key != base_key
        || stack.slot_width != slot_w
        || stack.slot_height != slot_h
        || stack.exposure != exposure
        || stack.width != width
        || stack.slots.len() != slots;
    if relayout {
        if stack.base_key.is_some() || !stack.slots.is_empty() {
            stack.relayouts += 1;
        }
        stack.base_key = base_key;
        stack.slot_width = slot_w;
        stack.slot_height = slot_h;
        stack.exposure = exposure;
        stack.width = width;
        stack.base_rows = bh;
        stack.slots.resize(slots, None);
        stack.height = (bh + slots as u32 * slot_h).max(1);
    }
    // Slots: kept, freed, then taken lowest first.
    for s in &mut stack.slots {
        if s.is_some_and(|c| !cells.iter().any(|(k, _)| *k == c)) {
            *s = None;
        }
    }
    for (c, _) in cells {
        if stack.slot_of(*c).is_none()
            && let Some(free) = stack.slots.iter().position(Option::is_none)
        {
            stack.slots[free] = Some(*c);
        }
    }
    let (w, h) = (stack.width, stack.height);
    let n = w as usize * h as usize;
    let directional =
        base.is_some_and(Lightmap::is_directional) || cells.iter().any(|c| c.1.is_directional());
    let template = first.cloned();
    // `Lightmap` keeps a private entry index (ADR-3300's lookup), so the stack starts from a clone
    // of one of its sources and sets every field; the index follows the entries it is given.
    let Some(mut out) = first.or(base).cloned() else {
        return Err("no lightmap to stack".into());
    };
    out.width = w;
    out.height = h;
    out.exposure = stack.exposure;
    out.density = first.map_or(1.0, |f| f.density);
    out.padding = first.map_or(0, |f| f.padding);
    out.entries = Vec::new();
    out.unwraps = Vec::new();
    out.bake = template.as_ref().map(|t| t.bake).unwrap_or_default();
    out.texels = vec![0; n];
    out.coverage = vec![Coverage::EMPTY; n];
    out.directions = if directional {
        vec![regia_asset::lightmap::encode_direction([0.0; 4]); n]
    } else {
        Vec::new()
    };
    out.meshlets = Vec::new();
    out.blocks = None;
    out.denoise = template.as_ref().and_then(|t| t.denoise);
    out.direction_blocks = None;
    out.source_hashes = Vec::new();
    let with_clusters =
        base.is_some_and(|b| !b.meshlets.is_empty()) || cells.iter().any(|c| !c.1.meshlets.is_empty());
    let with_hashes = base.is_some_and(|b| !b.source_hashes.is_empty())
        || cells.iter().any(|c| !c.1.source_hashes.is_empty());
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut place = |out: &mut Lightmap, src: &Lightmap, row: u32, tag: Option<&str>| -> (usize, usize) {
        blit(out, src, row);
        let (sx, sy) = (src.width as f32 / w as f32, src.height as f32 / h as f32);
        let oy = row as f32 / h as f32;
        let mut renamed = 0;
        for (i, entry) in src.entries.iter().enumerate() {
            let mut e = entry.clone();
            e.rect[1] += row;
            let so = entry.scale_offset;
            e.scale_offset = [so[0] * sx, so[1] * sy, so[2] * sx, so[3] * sy + oy];
            if !names.insert(e.name.clone())
                && let Some(tag) = tag
            {
                e.name = format!("{}@{tag}", e.name);
                names.insert(e.name.clone());
                renamed += 1;
            }
            out.entries.push(e);
            out.unwraps.push(src.unwraps.get(i).cloned().unwrap_or_default());
            if with_clusters {
                out.meshlets
                    .push(src.meshlets.get(i).cloned().unwrap_or_default());
            }
            if with_hashes {
                out.source_hashes.push(src.source_hash(i).unwrap_or(0));
            }
        }
        out.bake.contents |= src.bake.contents;
        (src.entries.len(), renamed)
    };
    if let Some(b) = base {
        place(&mut out, b, 0, None);
    }
    let mut regions = Vec::with_capacity(cells.len());
    for (c, map) in cells {
        let Some(slot) = stack.slot_of(*c) else {
            regions.push(None);
            continue;
        };
        let row = stack.base_rows + slot as u32 * stack.slot_height;
        let first_entry = out.entries.len();
        let (entries, renamed) = place(&mut out, map, row, Some(&c.label()));
        regions.push(Some(CellStackRegion {
            slot,
            row,
            rows: map.height,
            first_entry,
            entries,
            renamed,
        }));
    }
    // Each entry's first vertex and index in the stacked order (as a written file has them).
    let (mut v, mut i) = (0u32, 0u32);
    for (e, u) in out.entries.iter_mut().zip(&out.unwraps) {
        e.first_vertex = v;
        e.first_index = i;
        e.vertex_count = u.xref.len() as u32;
        e.index_count = u.indices.len() as u32;
        v += e.vertex_count;
        i += e.index_count;
    }
    out.validate().map_err(|e| e.to_string())?;
    Ok((out, regions))
}

/// One streamed cell's lightmap (ADR-3318).
#[derive(Clone)]
struct CellLightmap {
    file: String,
    stamp: Stamp,
    map: Option<Arc<Lightmap>>,
    report: CellLightmapReport,
}

/// What became of a streamed cell's lightmap, for `render.get_lightmaps` `level.cells`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CellLightmapReport {
    /// The cell, `"<x>_<z>"`.
    pub cell: String,
    /// The lightmap path of the cell.
    pub file: String,
    /// Its entries are in the installed atlas.
    pub installed: bool,
    /// Its entries.
    pub entries: usize,
    /// Its slot in the stack.
    pub slot: Option<usize>,
    /// First atlas row of its texels.
    pub row: u32,
    /// Its atlas rows.
    pub rows: u32,
    /// Its entries renamed `name@<x>_<z>`.
    pub renamed: usize,
    /// Why it is not installed (no file is not an error: the cell has no lightmapped part).
    pub error: Option<String>,
    /// A stable code for `error` (the reader's codes, [`LIGHTMAP_CELL_STACK_FULL`],
    /// [`LIGHTMAP_COMBINE`], [`LIGHTMAP_NOT_FOUND`] when the cell has no file).
    pub error_code: Option<&'static str>,
}

/// The streamed cells of the open level whose lightmaps are wanted, by message (ADR-3318).
#[derive(Resource, Default)]
pub struct StreamedLightmapCells {
    /// Cells loaded and not unloading, of the level each belongs to.
    loaded: std::collections::BTreeMap<CellCoord, LevelId>,
}

impl StreamedLightmapCells {
    /// The loaded cells, in coordinate order.
    pub fn cells(&self) -> impl Iterator<Item = CellCoord> + '_ {
        self.loaded.keys().copied()
    }
}

impl LevelLightmapState {
    /// The streamed cells' lightmaps (ADR-3318), in coordinate order, as `render.get_lightmaps`
    /// reports them under `level.cells`; empty when no cell of the level is loaded.
    #[must_use]
    pub fn cell_report(&self) -> Vec<CellLightmapReport> {
        self.cells.values().map(|c| c.report.clone()).collect()
    }

    /// The cell stack's layout (ADR-3318), `None` while no cell lightmap is stacked.
    #[must_use]
    pub fn cell_stack(&self) -> Option<&CellLightmapStack> {
        self.cell_stacked.as_ref().map(|_| &self.cell_stack)
    }
}

/// `Update`, after [`sync_additive_level_lightmaps`] (ADR-3318): the cells of the open streamed
/// level that are loaded (`CellLoaded`, until `CellUnloading`) have their lightmaps
/// ([`cell_lightmap_path`] of the level's own) stacked under the level's atlas in fixed slots
/// ([`stack_cell_lightmaps`]), so a cell's receivers resolve by key while it is streamed in and the
/// other cells' frames do not move when it streams out. Idle while no cell is loaded and none was.
pub fn sync_cell_lightmaps(
    mut loaded_msgs: MessageReader<CellLoaded>,
    mut unloading_msgs: MessageReader<CellUnloading>,
    manager: Option<Res<LevelManager>>,
    mut wanted: ResMut<StreamedLightmapCells>,
    mut assets: Option<ResMut<AssetServer>>,
    mut installed: Option<ResMut<InstalledLightmap>>,
    mut state: ResMut<LevelLightmapState>,
) {
    let current = manager.as_deref().and_then(LevelManager::current);
    let mut changed = false;
    for m in loaded_msgs.read() {
        changed |= wanted.loaded.insert(m.cell, m.level) != Some(m.level);
    }
    for m in unloading_msgs.read() {
        changed |= wanted.loaded.remove(&m.cell).is_some();
    }
    // Cells of a level that is no longer the open one go.
    let before = wanted.loaded.len();
    wanted.loaded.retain(|_, level| Some(*level) == current);
    changed |= wanted.loaded.len() != before;
    let primary = state.file.clone().filter(|_| state.installed);
    if wanted.loaded.is_empty() && state.cells.is_empty() && state.cell_stacked.is_none() {
        return;
    }
    let base_moved = match (&state.ours, &state.cell_stacked) {
        (Some(ours), Some(stacked)) => !Arc::ptr_eq(ours, stacked),
        (Some(_), None) => true,
        (None, _) => true,
    };
    // Reload a cell whose file changed on disk or in the cache (one stamp per loaded cell a frame).
    let poll = tracing::info_span!("level_lightmap_cells_poll", cells = wanted.loaded.len()).entered();
    let mut next = std::collections::BTreeMap::new();
    if let Some(primary) = &primary {
        for &cell in wanted.loaded.keys() {
            let file = cell_lightmap_path(primary, cell);
            let source = resolve(&file, assets.as_deref_mut());
            let stamp = stamp_of(&source, assets.as_deref());
            if let Some(prev) = state.cells.get(&cell)
                && prev.file == file
                && prev.stamp == stamp
            {
                next.insert(cell, prev.clone());
                continue;
            }
            changed = true;
            let _span = tracing::info_span!("level_lightmap_cell_load").entered();
            let mut report = CellLightmapReport {
                cell: cell.label(),
                file: file.clone(),
                ..CellLightmapReport::default()
            };
            let map = match load(&file, &source, assets.as_deref_mut()) {
                Ok(map) => {
                    report.entries = map.entries.len();
                    Some(Arc::new(map))
                }
                Err((code, error)) => {
                    if code != LIGHTMAP_NOT_FOUND {
                        tracing::warn!(target: "regia_framework", "cell lightmap {file}: {error}; its receivers keep the probe light");
                    }
                    report.error = Some(error);
                    report.error_code = Some(code);
                    None
                }
            };
            next.insert(
                cell,
                CellLightmap {
                    file,
                    stamp,
                    map,
                    report,
                },
            );
        }
    }
    drop(poll);
    changed |= next.len() != state.cells.len();
    if !changed && !base_moved {
        state.cells = next;
        return;
    }
    let _span = tracing::info_span!("level_lightmap_cells", cells = next.len()).entered();
    // The base: what the level and its additive levels installed (not an earlier stack).
    let base = if base_moved {
        state.ours.clone()
    } else {
        state.cell_base.clone()
    };
    state.cell_base = base.clone();
    let joined: Vec<(CellCoord, Arc<Lightmap>)> = next
        .iter()
        .filter_map(|(c, l)| l.map.clone().map(|m| (*c, m)))
        .collect();
    for l in next.values_mut() {
        l.report.installed = false;
        l.report.slot = None;
        l.report.row = 0;
        l.report.rows = 0;
        l.report.renamed = 0;
        if l.map.is_some() {
            l.report.error = None;
            l.report.error_code = None;
        }
    }
    let target = if joined.is_empty() {
        state.cell_stack = CellLightmapStack::default();
        base.clone()
    } else {
        let sources: Vec<(CellCoord, &Lightmap)> = joined.iter().map(|(c, m)| (*c, m.as_ref())).collect();
        let mut stack = std::mem::take(&mut state.cell_stack);
        let result = stack_cell_lightmaps(base.as_deref(), &sources, &mut stack);
        state.cell_stack = stack;
        match result {
            Ok((stacked, regions)) => {
                for ((cell, _), region) in joined.iter().zip(regions) {
                    let Some(l) = next.get_mut(cell) else { continue };
                    match region {
                        Some(r) => {
                            l.report.installed = true;
                            l.report.slot = Some(r.slot);
                            l.report.row = r.row;
                            l.report.rows = r.rows;
                            l.report.renamed = r.renamed;
                        }
                        None => {
                            l.report.error = Some("the stacked atlas is full (8192 rows)".into());
                            l.report.error_code = Some(LIGHTMAP_CELL_STACK_FULL);
                        }
                    }
                }
                Some(Arc::new(stacked))
            }
            Err(e) => {
                tracing::warn!(target: "regia_framework", "cell lightmaps: {e}; the cells keep the probe light");
                for (cell, _) in &joined {
                    if let Some(l) = next.get_mut(cell) {
                        l.report.error = Some(e.clone());
                        l.report.error_code = Some(LIGHTMAP_COMBINE);
                    }
                }
                base.clone()
            }
        }
    };
    state.cells = next;
    let stacked = target
        .as_ref()
        .filter(|t| base.as_ref().is_none_or(|b| !Arc::ptr_eq(b, t)));
    state.cell_stacked = stacked.cloned();
    let Some(target) = target else {
        return;
    };
    if state.ours.as_ref().is_some_and(|o| Arc::ptr_eq(o, &target)) {
        return;
    }
    let ours_installed = installed
        .as_deref()
        .is_some_and(|i| state.owns(i.lightmap.as_ref()));
    if ours_installed && let Some(i) = installed.as_deref_mut() {
        i.lightmap = Some(target.clone());
    }
    state.entries = target.entries.len();
    state.keyed_entries = target.entries.iter().filter(|e| e.key.is_some()).count();
    state.ours = Some(target);
    state.generation += 1;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use regia_asset::AssetServerSettings;
    use regia_asset::lightmap::{
        Coverage, LightmapBakeInfo, LightmapEntry, LightmapEntryKey, LightmapLayout, MeshUnwrap,
    };
    use regia_core::id::Guid;
    use regia_scene::level::{Level, LevelId, LevelLibrary, LoadMode, open_level, unload_level};
    use regia_scene::prelude::ScenePlugin;
    use regia_test_utils::unique_temp_dir;

    use super::*;

    const A: &str = "00000000-0000-0000-0000-0000000000a1";
    const B: &str = "00000000-0000-0000-0000-0000000000b2";

    fn guid(s: &str) -> Guid {
        s.parse().expect("guid")
    }

    /// A lightmap keyed like `regia bake-lightmaps --scene`: entity A's parts 0 and 1, entity B's
    /// part 0.
    fn keyed_map() -> Lightmap {
        let entry = |name: String, x: u32, key: LightmapEntryKey| LightmapEntry {
            name,
            source_vertices: 3,
            rect: [x, 0, 2, 2],
            scale_offset: [1.0 / 3.0, 1.0, x as f32 / 6.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: 1.0,
            key: Some(key),
            seams: None,
            page: 0,
        };
        let unwrap = MeshUnwrap {
            xref: vec![0, 1, 2],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
            indices: vec![0, 1, 2],
        };
        let keys = [(A, 0u32), (A, 1), (B, 0)];
        let entries = keys
            .iter()
            .enumerate()
            .map(|(i, (g, part))| {
                entry(
                    format!("entity_{g}_{part}"),
                    2 * i as u32,
                    LightmapEntryKey {
                        entity: guid(g),
                        part: *part,
                    },
                )
            })
            .collect();
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 6,
                height: 2,
                density: 1.0,
                padding: 1,
                entries,
                unwraps: vec![unwrap.clone(), unwrap.clone(), unwrap],
            },
            &[[0.5; 3]; 12],
            vec![Coverage::BAKED; 12],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

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

    /// A level with entities A (two parts) and B; B saved its own receiver.
    fn level_text(look: &str) -> String {
        format!(
            r#"SceneFile(
    format_version: 11,
    entities: [
        (guid: "{A}", name: "hall", components: {{}}),
        (guid: "{B}", name: "crate", components: {{
            "regia_render::lightmap::LightmapReceiver": (entry: "entity_{B}_0"),
        }}),
    ],
    resources: {{
        "regia_scene::level::LevelSettings": (look: ({look})),
    }},
)"#
        )
    }

    fn app_with(server: AssetServer, text: &str) -> (App, LevelId) {
        let mut app = App::new();
        app.add_plugins((ScenePlugin, LightmapLevelPlugin));
        regia_render::register_render_types(&mut app);
        app.insert_resource(server);
        app.insert_resource(RuntimeRenderer::default());
        let id = LevelId(Guid::new_random());
        let level = Level::from_ron_str(id, text).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        (app, id)
    }

    fn entity_of(app: &mut App, g: &str) -> Entity {
        let want = guid(g);
        let mut q = app.world_mut().query::<(Entity, &EntityGuid)>();
        q.iter(app.world())
            .find(|(_, id)| id.0 == want)
            .map(|(e, _)| e)
            .expect("spawned")
    }

    fn renderer() -> MeshRenderer {
        MeshRenderer::new(
            Arc::new(regia_render::CpuMesh {
                vertices: Vec::new(),
                indices: Vec::new(),
            }),
            Arc::new(regia_render::LabMaterial::default()),
        )
    }

    /// What a host's mesh resolver does: part 0 on the level entity, part 1 on a part entity.
    fn resolve_meshes(app: &mut App) -> (Entity, Entity, Entity) {
        let a = entity_of(app, A);
        let b = entity_of(app, B);
        app.world_mut().entity_mut(a).insert(renderer());
        app.world_mut().entity_mut(b).insert(renderer());
        let part = app
            .world_mut()
            .spawn((renderer(), MeshPartOf { owner: a, part: 1 }))
            .id();
        (a, part, b)
    }

    fn entry_of(app: &App, e: Entity) -> Option<String> {
        app.world().get::<LightmapReceiver>(e).map(|r| r.entry.clone())
    }

    /// A level naming its lightmap warms the variants, installs the file with lightmaps on, and
    /// attaches every keyed part (the level entity's own mesh and its part entity) as the meshes
    /// appear, keeping a receiver the level saved. Closing it takes all of that back.
    #[test]
    fn a_level_installs_its_lightmap_attaches_every_part_and_unload_restores() {
        let (root, server) = project("lightmap-level-install");
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap", intensity: Some(2.0)))"#),
        );
        app.update();
        assert!(!app.world().resource::<RuntimeRenderer>().0.warm_lightmaps());
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        assert!(
            app.world().resource::<RuntimeRenderer>().0.warm_lightmaps(),
            "the level asked for the warm"
        );
        let installed = app.world().resource::<InstalledLightmap>();
        assert_eq!(installed.source.as_deref(), Some("assets/maps/yard.flmap"));
        let settings = *app.world().resource::<LightmapSettings>();
        assert!(settings.enabled, "a level naming a lightmap turns it on");
        assert_eq!(settings.intensity, 2.0);
        {
            let state = app.world().resource::<LevelLightmapState>();
            assert!(state.installed && state.warm);
            assert_eq!((state.entries, state.keyed_entries, state.attached), (3, 3, 0));
            assert_eq!(state.error_code, None);
        }

        // The meshes resolve a frame later; every part finds its entry.
        let (a, part, b) = resolve_meshes(&mut app);
        app.update();
        assert_eq!(entry_of(&app, a), Some(format!("entity_{A}_0")));
        assert_eq!(entry_of(&app, part), Some(format!("entity_{A}_1")));
        assert_eq!(
            entry_of(&app, b),
            Some(format!("entity_{B}_0")),
            "the saved receiver"
        );
        let state = app.world().resource::<LevelLightmapState>();
        assert_eq!(state.attached, 2, "B kept its own receiver");
        assert!(state.unmatched.is_empty(), "{:?}", state.unmatched);
        let report = state.report();
        assert_eq!(report.file.as_deref(), Some("maps/yard.flmap"));

        unload_level(app.world_mut(), id).expect("unload");
        app.update();
        app.update();
        assert!(!app.world().resource::<RuntimeRenderer>().0.warm_lightmaps());
        assert!(app.world().resource::<InstalledLightmap>().lightmap.is_none());
        assert_eq!(
            *app.world().resource::<LightmapSettings>(),
            LightmapSettings::default()
        );
        assert_eq!(
            entry_of(&app, part),
            None,
            "the part entity's receiver is taken back"
        );
        let state = app.world().resource::<LevelLightmapState>();
        assert!(!state.installed && !state.warm);
        assert_eq!(state.attached, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// w7-runtime-smalls-z item 8: a `RuntimeRenderer` that appears after the level lightmap was
    /// installed (the fast path had nothing left to do) still gets the warm request on the next
    /// sync, and a later level without a lightmap restores the renderer's own value.
    #[test]
    fn a_renderer_inserted_after_the_install_is_warmed_and_restored() {
        let (root, server) = project("lightmap-level-late-renderer");
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        app.world_mut().remove_resource::<RuntimeRenderer>();
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        app.update();
        {
            let state = app.world().resource::<LevelLightmapState>();
            assert!(state.installed, "installed with no renderer");
            assert!(!state.warm, "no renderer to warm yet");
        }
        app.insert_resource(RuntimeRenderer::default());
        assert!(!app.world().resource::<RuntimeRenderer>().0.warm_lightmaps());
        app.update();
        assert!(
            app.world().resource::<RuntimeRenderer>().0.warm_lightmaps(),
            "the late renderer is asked for the warm"
        );
        assert!(app.world().resource::<LevelLightmapState>().warm);

        // A later level that names no lightmap gives the renderer its own value back.
        let plain = LevelId(Guid::new_random());
        let level = Level::from_ron_str(plain, &level_text("exposure_bias_ev: 0.0")).expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), plain, LoadMode::Replace).expect("open plain");
        app.update();
        app.update();
        assert!(
            !app.world().resource::<RuntimeRenderer>().0.warm_lightmaps(),
            "restored to the renderer's value before the warm"
        );
        assert!(!app.world().resource::<LevelLightmapState>().warm);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A level that names no lightmap touches nothing: no resource appears, the renderer is not
    /// asked to warm, and no receiver is attached.
    #[test]
    fn a_level_without_a_lightmap_changes_nothing() {
        let (root, server) = project("lightmap-level-none");
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(server, &level_text("exposure_bias_ev: 0.0"));
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let (a, part, _) = resolve_meshes(&mut app);
        app.update();
        app.update();
        assert!(!app.world().contains_resource::<InstalledLightmap>());
        assert!(!app.world().contains_resource::<LightmapSettings>());
        assert!(!app.world().resource::<RuntimeRenderer>().0.warm_lightmaps());
        assert_eq!(entry_of(&app, a), None);
        assert_eq!(entry_of(&app, part), None);
        assert_eq!(
            app.world().resource::<LevelLightmapState>().report(),
            LevelLightmapReport::default()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A missing file is a status with its code, never a panic; the warm request stands (the file
    /// may appear), and the file appearing later installs it and attaches the receivers.
    #[test]
    fn a_missing_lightmap_sets_the_error_and_a_late_file_installs() {
        let (root, server) = project("lightmap-level-missing");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap", enabled: Some(false)))"#),
        );
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let (a, _, _) = resolve_meshes(&mut app);
        app.update();
        {
            let state = app.world().resource::<LevelLightmapState>();
            assert!(!state.installed);
            assert_eq!(state.error_code, Some(LIGHTMAP_NOT_FOUND));
            assert!(state.warm);
        }
        assert!(
            !app.world().resource::<LightmapSettings>().enabled,
            "enabled: Some(false)"
        );
        assert_eq!(entry_of(&app, a), None);

        regia_test_utils::write_atomic(
            &root.join("assets/maps/yard.flmap"),
            &keyed_map().to_bytes().expect("encode"),
        )
        .expect("write");
        app.update();
        app.update();
        let state = app.world().resource::<LevelLightmapState>();
        assert!(state.installed, "{:?}", state.error);
        assert_eq!(state.error_code, None);
        assert_eq!(entry_of(&app, a), Some(format!("entity_{A}_0")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A lightmap that only becomes a cooked asset after the level opened (a live cook, a chunk
    /// mounted later) installs: the steady-frame cache must notice the manifest changed.
    #[test]
    fn a_lightmap_cooked_after_open_installs() {
        use regia_asset::{
            AssetId, AssetManifest, COOKED_FORMAT_VERSION, CookedHeader, ManifestEntry, cooked_cache_dir,
            cooked_file_name, encode_cooked_file,
        };
        use regia_core::id::TypePathHash;

        let (root, server) = project("lightmap-level-cooked-late");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let (a, _, _) = resolve_meshes(&mut app);
        app.update();
        assert_eq!(
            app.world().resource::<LevelLightmapState>().error_code,
            Some(LIGHTMAP_NOT_FOUND)
        );

        let cache = cooked_cache_dir(&root, "linux");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let asset = AssetId::new_random();
        let payload = keyed_map().to_bytes().expect("encode");
        let type_path = "regia_asset::lightmap::Lightmap";
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(type_path),
            asset_id: asset,
            importer_version: 1_000_000,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("cooked file");
        std::fs::write(cache.join(cooked_file_name(asset)), file).expect("write cooked");
        let path = "assets/maps/yard.flmap".to_string();
        let entry = ManifestEntry {
            path: path.clone(),
            id: asset,
            type_path: type_path.to_string(),
        };
        AssetManifest::from_map(std::collections::BTreeMap::from([(path, entry)]))
            .write_to_cache(&cache)
            .expect("manifest");
        app.world_mut().resource_mut::<AssetServer>().refresh_manifest();
        app.update();
        app.update();
        let state = app.world().resource::<LevelLightmapState>();
        assert!(state.installed, "{:?}", state.error);
        assert_eq!(state.error_code, None);
        assert_eq!(entry_of(&app, a), Some(format!("entity_{A}_0")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A damaged file reports the reader's code and installs nothing.
    #[test]
    fn a_damaged_lightmap_reports_the_reader_code() {
        let (root, server) = project("lightmap-level-damaged");
        std::fs::write(root.join("assets/maps/yard.flmap"), b"not a lightmap").expect("write");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let state = app.world().resource::<LevelLightmapState>();
        assert!(!state.installed);
        let code = state.error_code.unwrap_or_default();
        assert!(
            code.starts_with("lightmap_") && code != LIGHTMAP_NOT_FOUND,
            "{code}"
        );
        assert!(
            app.world()
                .get_resource::<InstalledLightmap>()
                .is_none_or(|i| i.lightmap.is_none()),
            "nothing installed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    const C: &str = "00000000-0000-0000-0000-0000000000c3";

    /// A one-entry lightmap keyed to entity C's part 0 (an additive level's bake), `rows` tall.
    fn additive_map(rows: u32) -> Lightmap {
        let n = (2 * rows) as usize;
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 2,
                height: rows,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: format!("entity_{C}_0"),
                    source_vertices: 3,
                    rect: [0, 0, 2, rows],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: Some(LightmapEntryKey {
                        entity: guid(C),
                        part: 0,
                    }),
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &vec![[2.0; 3]; n],
            vec![Coverage::BAKED; n],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    /// An additive level holding entity C, naming `look`.
    fn additive_level_text(look: &str) -> String {
        format!(
            r#"SceneFile(
    format_version: 11,
    entities: [ (guid: "{C}", name: "annex", components: {{}}) ],
    resources: {{
        "regia_scene::level::LevelSettings": (look: ({look})),
    }},
)"#
        )
    }

    /// An additive level's lightmap joins the primary's (ADR-2635): one atlas holds both, the
    /// receivers of both levels attach, the report places it; unloading the additive level puts
    /// the primary's own lightmap back.
    #[test]
    fn an_additive_level_s_lightmap_joins_the_primary_atlas() {
        let (root, server) = project("lightmap-level-additive");
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        additive_map(4)
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        let annex = LevelId(Guid::new_random());
        let level = Level::from_ron_str(
            annex,
            &additive_level_text(r#"lightmap: Some((file: "maps/annex.flmap"))"#),
        )
        .expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let primary = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        assert_eq!((primary.width, primary.height), (6, 2));

        open_level(app.world_mut(), annex, LoadMode::Additive).expect("open additive");
        app.update();
        let (a, part, _) = resolve_meshes(&mut app);
        let c = entity_of(&mut app, C);
        app.world_mut().entity_mut(c).insert(renderer());
        app.update();
        let installed = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        assert_eq!(
            (installed.width, installed.height, installed.entries.len()),
            (6, 6, 4),
            "the annex's 2 x 4 atlas sits below the yard's 6 x 2"
        );
        assert_eq!(installed.irradiance_at(0, 3), [2.0; 3], "the annex's texels");
        assert_eq!(installed.irradiance_at(0, 0), [0.5; 3], "the yard's texels");
        assert_eq!(entry_of(&app, a), Some(format!("entity_{A}_0")));
        assert_eq!(entry_of(&app, part), Some(format!("entity_{A}_1")));
        assert_eq!(
            entry_of(&app, c),
            Some(format!("entity_{C}_0")),
            "the annex's receiver"
        );
        {
            let state = app.world().resource::<LevelLightmapState>();
            assert!(state.owns(Some(&installed)));
            assert_eq!(state.attached, 3);
            let additive = state.additive_report();
            assert_eq!(additive.len(), 1);
            let row = &additive[0];
            assert!(row.installed, "{row:?}");
            assert_eq!((row.row, row.rows, row.entries, row.error_code), (2, 4, 1, None));
        }

        unload_level(app.world_mut(), annex).expect("unload annex");
        app.update();
        app.update();
        let back = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        assert!(Arc::ptr_eq(&back, &primary), "the yard's own lightmap again");
        assert!(
            app.world()
                .resource::<LevelLightmapState>()
                .additive_report()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two levels with an entry of one name (ADR-2635): a receiver saved in the additive level draws
    /// that level's texels, the primary's keeps its own, and a name saved while stacked (another
    /// `k`) finds its way back.
    #[test]
    fn a_saved_receiver_of_an_additive_level_keeps_its_level_s_entry() {
        let (root, server) = project("lightmap-level-additive-clash");
        let mut yard = keyed_map();
        yard.entries[0].name = "instance_0".into();
        yard.write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        let mut annex_map = additive_map(4);
        annex_map.entries[0].name = "instance_0".into();
        annex_map
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        let annex = LevelId(Guid::new_random());
        let level = Level::from_ron_str(
            annex,
            &additive_level_text(r#"lightmap: Some((file: "maps/annex.flmap"))"#),
        )
        .expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        open_level(app.world_mut(), annex, LoadMode::Additive).expect("open additive");
        let receiver = |entry: &str| LightmapReceiver { entry: entry.into() };
        let in_yard = app
            .world_mut()
            .spawn((LevelMember::from_level(id), receiver("instance_0")))
            .id();
        let in_annex = app
            .world_mut()
            .spawn((LevelMember::from_level(annex), receiver("instance_0")))
            .id();
        let saved_stacked = app
            .world_mut()
            .spawn((LevelMember::from_level(annex), receiver("instance_0@7")))
            .id();
        let unrelated = app.world_mut().spawn(receiver("instance_0")).id();
        app.update();
        let installed = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        let entry = |app: &App, e: Entity| {
            app.world()
                .get::<LightmapReceiver>(e)
                .map(|r| r.entry.clone())
                .unwrap_or_default()
        };
        assert_eq!(entry(&app, in_yard), "instance_0");
        assert_eq!(entry(&app, unrelated), "instance_0");
        assert_eq!(entry(&app, in_annex), "instance_0@1");
        assert_eq!(entry(&app, saved_stacked), "instance_0@1");
        let index = installed.entry("instance_0@1").expect("the annex's entry");
        assert_eq!(installed.entries[index].rect[1], 2, "below the yard's rows");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A primary and an additive level opened before the same update, with no
    /// [`InstalledLightmap`] in the world yet: the chained systems' sync point applies the
    /// primary's deferred insert before the additive system runs, so the combined atlas is
    /// installed on that first update and stays installed (audit framework5 #1).
    #[test]
    fn a_primary_and_an_additive_level_opened_together_install_the_combined_atlas() {
        let (root, server) = project("lightmap-level-additive-together");
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        additive_map(4)
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        let annex = LevelId(Guid::new_random());
        let level = Level::from_ron_str(
            annex,
            &additive_level_text(r#"lightmap: Some((file: "maps/annex.flmap"))"#),
        )
        .expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        assert!(!app.world().contains_resource::<InstalledLightmap>());
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        open_level(app.world_mut(), annex, LoadMode::Additive).expect("open additive");
        for update in 0..2 {
            app.update();
            let installed = app
                .world()
                .resource::<InstalledLightmap>()
                .lightmap
                .clone()
                .expect("installed");
            assert_eq!(
                (installed.width, installed.height, installed.entries.len()),
                (6, 6, 4),
                "update {update}: the combined atlas, not the primary's alone"
            );
            assert!(
                app.world()
                    .resource::<LevelLightmapState>()
                    .owns(Some(&installed))
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A combine that fails after a good one (here the frame stops drawing appended pages) must
    /// not point an additive level's renamed receiver at the primary's entry of the same name:
    /// the receiver keeps its stacked name, which the primary-only atlas does not hold, so it
    /// keeps the probe light (audit framework5 #4).
    #[test]
    fn a_failed_combine_does_not_point_an_additive_receiver_at_the_primary_entry() {
        let (root, server) = project("lightmap-level-additive-combine-fails");
        let mut yard = keyed_map();
        yard.entries[0].name = "instance_0".into();
        yard.write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        let mut annex_map = additive_map(regia_asset::lightmap::MAX_LIGHTMAP_SIZE);
        annex_map.entries[0].name = "instance_0".into();
        annex_map
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        app.world_mut()
            .resource_mut::<LevelLightmapState>()
            .set_append_pages(true);
        let annex = LevelId(Guid::new_random());
        let level = Level::from_ron_str(
            annex,
            &additive_level_text(r#"lightmap: Some((file: "maps/annex.flmap"))"#),
        )
        .expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        open_level(app.world_mut(), annex, LoadMode::Additive).expect("open additive");
        let in_annex = app
            .world_mut()
            .spawn((
                LevelMember::from_level(annex),
                LightmapReceiver {
                    entry: "instance_0".into(),
                },
            ))
            .id();
        app.update();
        assert_eq!(entry_of(&app, in_annex).as_deref(), Some("instance_0@1"));
        assert!(app.world().resource::<LevelLightmapState>().additive_report()[0].installed);

        app.world_mut()
            .resource_mut::<LevelLightmapState>()
            .set_append_pages(false);
        app.update();
        let installed = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        assert_eq!(installed.pages(), 1, "the primary's own atlas again");
        let row = &app.world().resource::<LevelLightmapState>().additive_report()[0];
        assert!(!row.installed, "{row:?}");
        assert_eq!(row.error_code, Some(LIGHTMAP_COMBINE));
        let entry = entry_of(&app, in_annex).expect("receiver kept");
        assert!(
            installed.entry(&entry).is_none(),
            "the annex receiver now names {entry:?}, which samples the yard's texels"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without a primary lightmap, an additive level's lightmap is reported, not installed.
    #[test]
    fn an_additive_lightmap_without_a_primary_one_reports_its_code() {
        let (root, server) = project("lightmap-level-additive-alone");
        additive_map(2)
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(server, &level_text("exposure_bias_ev: 0.0"));
        let annex = LevelId(Guid::new_random());
        let level = Level::from_ron_str(
            annex,
            &additive_level_text(r#"lightmap: Some((file: "maps/annex.flmap"))"#),
        )
        .expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        open_level(app.world_mut(), annex, LoadMode::Additive).expect("open additive");
        app.update();
        app.update();
        assert!(
            app.world()
                .get_resource::<InstalledLightmap>()
                .is_none_or(|i| i.lightmap.is_none())
        );
        let additive = app.world().resource::<LevelLightmapState>().additive_report();
        assert_eq!(additive.len(), 1);
        assert_eq!(additive[0].error_code, Some(LIGHTMAP_NO_PRIMARY));
        assert!(!additive[0].installed);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An additive level whose lightmap would push the stack past one page is appended as a page
    /// of its own (ADR-3300): the installed lightmap has two pages, the annex's entry is on page 1
    /// at its own rectangle, its receiver attaches, and the report says where it went.
    #[test]
    fn an_additive_lightmap_past_one_page_is_appended_as_a_page() {
        additive_past_one_page(true);
    }

    /// The same stack in a build whose frame binds one 2-D atlas (the default until the ADR-3300
    /// frame patch lands): the annex is refused with [`LIGHTMAP_COMBINE`] and the primary's
    /// one-page lightmap stays installed, as before pages.
    #[test]
    fn an_additive_lightmap_past_one_page_is_refused_when_the_frame_draws_one_page() {
        assert_eq!(
            LevelLightmapState::default().append_pages(),
            regia_render::lightmap::LIGHTMAP_PAGED_BINDINGS
        );
        additive_past_one_page(false);
    }

    fn additive_past_one_page(append: bool) {
        let (root, server) = project(if append {
            "lightmap-level-additive-pages"
        } else {
            "lightmap-level-additive-pages-refused"
        });
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        let tall = regia_asset::lightmap::MAX_LIGHTMAP_SIZE;
        additive_map(tall)
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        app.world_mut()
            .resource_mut::<LevelLightmapState>()
            .set_append_pages(append);
        let annex = LevelId(Guid::new_random());
        let level = Level::from_ron_str(
            annex,
            &additive_level_text(r#"lightmap: Some((file: "maps/annex.flmap"))"#),
        )
        .expect("level parses");
        app.world_mut().resource_mut::<LevelLibrary>().insert(level);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        open_level(app.world_mut(), annex, LoadMode::Additive).expect("open additive");
        app.update();
        let c = entity_of(&mut app, C);
        app.world_mut().entity_mut(c).insert(renderer());
        app.update();
        let installed = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        if !append {
            assert_eq!(installed.pages(), 1, "the primary's one page stays installed");
            assert!(installed.entry(&format!("entity_{C}_0")).is_none());
            let additive = app.world().resource::<LevelLightmapState>().additive_report();
            let row = &additive[0];
            assert!(!row.installed, "{row:?}");
            assert_eq!(row.error_code, Some(LIGHTMAP_COMBINE), "{row:?}");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert_eq!(
            (installed.pages(), installed.width, installed.height),
            (2, 6, tall)
        );
        let k = installed
            .entry(&format!("entity_{C}_0"))
            .expect("the annex's entry");
        assert_eq!(
            (installed.entries[k].page, installed.entries[k].rect),
            (1, [0, 0, 2, tall])
        );
        let lit = installed.sample_entry(k, [0.5, 0.5]);
        assert!(
            lit.iter().all(|v| (v - 2.0).abs() < 1e-4),
            "the annex's texels on page 1: {lit:?}"
        );
        assert_eq!(entry_of(&app, c), Some(format!("entity_{C}_0")));
        let additive = app.world().resource::<LevelLightmapState>().additive_report();
        let row = &additive[0];
        assert!(row.installed, "{row:?}");
        assert_eq!((row.page, row.pages, row.row, row.rows), (1, 1, 0, tall));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two additive lightmaps that share an entry name keep their rename tables on the next update.
    #[test]
    fn steady_additive_lightmap_keeps_its_rename_vec() {
        let (root, server) = project("lightmap-level-steady");
        keyed_map()
            .write(&root.join("assets/maps/yard.flmap"))
            .expect("write flmap");
        additive_map(2)
            .write(&root.join("assets/maps/annex.flmap"))
            .expect("write flmap");
        additive_map(2)
            .write(&root.join("assets/maps/barn.flmap"))
            .expect("write flmap");
        let (mut app, id) = app_with(
            server,
            &level_text(r#"lightmap: Some((file: "maps/yard.flmap"))"#),
        );
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        for file in ["maps/annex.flmap", "maps/barn.flmap"] {
            let level_id = LevelId(Guid::new_random());
            let level = Level::from_ron_str(
                level_id,
                &additive_level_text(&format!(r#"lightmap: Some((file: "{file}"))"#)),
            )
            .expect("level parses");
            app.world_mut().resource_mut::<LevelLibrary>().insert(level);
            open_level(app.world_mut(), level_id, LoadMode::Additive).expect("open additive");
        }
        app.update();
        // `run_system_cached` spends a `resource_scope` on every call. The count is the
        // initialized system, which is what the schedule runs after the first frame.
        use regia_ecs::ecs::system::{IntoSystem, System};
        let mut system = IntoSystem::into_system(sync_additive_level_lightmaps);
        {
            let world = app.world_mut();
            system.initialize(world);
            system.run((), world).expect("warmup");
        }
        let (generation, rename_ptr, renamed) = {
            let state = app.world().resource::<LevelLightmapState>();
            let renamed = state.additive.iter().map(|a| a.renames.len()).sum::<usize>();
            let rename_ptr = state
                .additive
                .iter()
                .find(|a| !a.renames.is_empty())
                .map(|a| a.renames.as_ptr() as usize);
            (state.generation, rename_ptr, renamed)
        };
        assert!(renamed > 0, "the shared entry name was renamed");
        // `metadata` may allocate a `CString` for the syscall. That cost is the steady frame's
        // budget: the frame stats the two cached files and does not build a new rename table.
        let annex = root.join("assets/maps/annex.flmap");
        let barn = root.join("assets/maps/barn.flmap");
        let metadata_allocs = crate::test_alloc::count(|| {
            let _ = std::fs::metadata(&annex);
            let _ = std::fs::metadata(&barn);
        });
        let allocated = crate::test_alloc::count(|| {
            system.run((), app.world_mut()).expect("steady");
        });
        let state = app.world().resource::<LevelLightmapState>();
        assert_eq!(state.generation, generation);
        assert_eq!(
            state
                .additive
                .iter()
                .find(|a| !a.renames.is_empty())
                .map(|a| a.renames.as_ptr() as usize),
            rename_ptr,
            "the rename table was replaced"
        );
        assert!(
            allocated <= metadata_allocs,
            "the steady frame allocated {allocated} times; stating the two files allocated {metadata_allocs}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A one-entry map keyed to entity `g` part 0 (`size` × `size` texels).
    fn one_entry_map(g: &str, size: u32) -> Lightmap {
        let n = (size * size) as usize;
        Lightmap::from_irradiance(
            LightmapLayout {
                width: size,
                height: size,
                density: 1.0,
                padding: 0,
                entries: vec![LightmapEntry {
                    name: format!("entity_{g}_0"),
                    source_vertices: 3,
                    rect: [0, 0, size, size],
                    page: 0,
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: Some(LightmapEntryKey {
                        entity: guid(g),
                        part: 0,
                    }),
                    seams: None,
                }],
                unwraps: vec![MeshUnwrap {
                    xref: vec![0, 1, 2],
                    uv: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                    indices: vec![0, 1, 2],
                }],
            },
            &vec![[0.5; 3]; n],
            vec![Coverage::BAKED; n],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    /// Streamed cells (ADR-3318): a `CellLoaded` cell's lightmap stacks under the level's (an empty
    /// primary here) and its entities get receivers; a second cell takes the next slot, the first
    /// keeps its own; `CellUnloading` takes a cell out and leaves the other in its slot; a cell
    /// with no file is reported, not an error.
    #[test]
    fn streamed_cells_stack_their_lightmaps_in_fixed_slots_and_unload() {
        use regia_scene::streaming::{CellLoaded, CellUnloading};
        let (root, server) = project("lightmap-level-cells");
        let maps = root.join("assets/maps");
        std::fs::create_dir_all(maps.join("w.cells")).expect("cell dir");
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 4,
                height: 4,
                density: 1.0,
                padding: 0,
                entries: Vec::new(),
                unwraps: Vec::new(),
            },
            &[[0.0; 3]; 16],
            vec![Coverage::EMPTY; 16],
            LightmapBakeInfo::default(),
        )
        .expect("empty primary")
        .write(&maps.join("w.flmap"))
        .expect("write primary");
        one_entry_map(A, 8)
            .write(&maps.join("w.cells/0_0.flmap"))
            .expect("cell 0_0");
        one_entry_map(B, 8)
            .write(&maps.join("w.cells/1_0.flmap"))
            .expect("cell 1_0");
        let text = format!(
            r#"SceneFile(
    format_version: 11,
    entities: [
        (guid: "{A}", name: "a", components: {{}}),
        (guid: "{B}", name: "b", components: {{}}),
    ],
    resources: {{
        "regia_scene::level::LevelSettings": (look: (lightmap: Some((file: "maps/w.flmap")))),
    }},
)"#
        );
        let (mut app, id) = app_with(server, &text);
        open_level(app.world_mut(), id, LoadMode::Replace).expect("open");
        app.update();
        let (a, b) = (entity_of(&mut app, A), entity_of(&mut app, B));
        app.world_mut().entity_mut(a).insert(renderer());
        app.world_mut().entity_mut(b).insert(renderer());
        app.update();
        assert_eq!(
            (entry_of(&app, a), entry_of(&app, b)),
            (None, None),
            "no cell loaded yet"
        );
        let load = |app: &mut App, x: i64| {
            app.world_mut().write_message(CellLoaded {
                level: id,
                cell: CellCoord::new(x, 0),
                entities: Vec::new(),
                nav: None,
            });
            app.update();
        };
        load(&mut app, 0);
        assert_eq!(entry_of(&app, a), Some(format!("entity_{A}_0")));
        assert_eq!(entry_of(&app, b), None);
        let slot = |app: &App, cell: &str| {
            app.world()
                .resource::<LevelLightmapState>()
                .cell_report()
                .into_iter()
                .find(|r| r.cell == cell)
                .and_then(|r| r.slot)
        };
        assert_eq!(slot(&app, "0_0"), Some(0));
        load(&mut app, 1);
        assert_eq!(entry_of(&app, b), Some(format!("entity_{B}_0")));
        assert_eq!((slot(&app, "0_0"), slot(&app, "1_0")), (Some(0), Some(1)));
        let stack = app
            .world()
            .resource::<LevelLightmapState>()
            .cell_stack()
            .cloned()
            .expect("a stack");
        assert_eq!((stack.slot_width, stack.slot_height, stack.base_rows), (8, 8, 4));
        let installed = app
            .world()
            .resource::<InstalledLightmap>()
            .lightmap
            .clone()
            .expect("installed");
        assert_eq!(installed.entries.len(), 2);
        app.world_mut().write_message(CellUnloading {
            level: id,
            cell: CellCoord::new(0, 0),
        });
        app.update();
        app.update();
        assert_eq!(slot(&app, "1_0"), Some(1), "the other cell keeps its slot");
        assert_eq!(slot(&app, "0_0"), None);
        assert_eq!(
            entry_of(&app, a),
            None,
            "the unloaded cell's receiver is taken back"
        );
        assert_eq!(entry_of(&app, b), Some(format!("entity_{B}_0")));
        load(&mut app, 5);
        let report = app.world().resource::<LevelLightmapState>().cell_report();
        let missing = report.iter().find(|r| r.cell == "5_0").expect("reported");
        assert_eq!(
            (missing.installed, missing.error_code),
            (false, Some(LIGHTMAP_NOT_FOUND))
        );
        assert_eq!(
            app.world()
                .resource::<LevelLightmapState>()
                .cell_stack()
                .map(|s| s.relayouts),
            Some(0)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
