// SPDX-License-Identifier: MPL-2.0
//! Indoor occlusion by cells and portals, stages 1 and 2 (`docs/specs/25-sim-bridge-and-do27.md`
//! § 7.7, ADR-7586, ADR-7673).
//!
//! A map's `<map>.viscells.ron` ([`VisCells::from_ron`]) lists cells (oriented boxes) and convex
//! portal quads between cell pairs; cell 0 is outdoors. An entity carries
//! [`regia_scene::components::VisCell`] to say which cell it belongs to. Insert the loaded
//! [`VisCells`] as a resource and, each frame, [`cull_unreachable_cells`] walks the portals from
//! every extracted view ([`traverse()`]; an eye in a doorway's wall gap starts in both of the
//! doorway's cells, ADR-7639) and leaves out of the colour pass the draws of entities
//! whose cell no view reaches. It runs in `Render` right after mesh LOD selection, so it sees the
//! frame's final draws after the ADR-7200 visibility filter (hidden entities never reach it), and
//! before [`crate::update_view_visibility`], so `ViewVisibility` is false for a culled entity.
//!
//! Rules of stage 1:
//!
//! - An entity without `VisCell` takes its nearest tagged ancestor's (stage 2, [`InheritedCells`],
//!   resolved once per hierarchy edit); one with none up its chain always draws. A `VisCell` naming
//!   a cell the file does not list draws too (counted in [`VisCellStats::unknown_cell_draws`]).
//! - The reachable set is the union over the frame's views (window and texture views): the frame
//!   has one draw list, so a cell any view reaches draws in every view.
//! - Shadow views are not culled: a culled draw that casts shadows stays as a shadow-only draw (the
//!   mesh LOD's shadow-only key, `SHADOW_LOD_MESH_TAG`), so an unreachable cell still casts into the
//!   sun's cascades; a culled draw that casts nothing leaves the frame.
//! - Without a [`VisCells`] resource, or with [`VisCellSettings::cull`] false, nothing is culled.
//! - Stage 2 (ADR-7673): the point and spot lights of unreached cells leave the frame when their
//!   range touches no reached cell ([`light_reaches`]), with their side tables and so their
//!   shadow-map updates; the culled entities are published in [`VisCellCulled`], and
//!   [`crate::extract_glass`] adds no glass instance for them. [`audio_zone_mouths`] derives the
//!   audio zones' mouths from the portals.
//! - Terrain and scatter (plan row W-22, ADR-8413): the walk of each window view also records the
//!   narrowed frusta through which it enters the outdoor cell, into
//!   [`ExtractedFrame::outdoor_portal_views`] ([`OutdoorPortalViews`], one [`OutdoorPortals`] per
//!   window view, ADR-8843), and their union into [`ExtractedFrame::outdoor_portals`]. The terrain
//!   draw leaves out the heightfield nodes, its grass patches and its impostor tiles, the water
//!   its bodies and ring chunks, and the scatter field the instance blocks that lie in none of
//!   them, so an eye inside a sealed cell draws only the outdoors its mouths show. An eye outdoors
//!   is never restricted.
//!
//! - Stage 3 (ADR-8414, [`sun`]): a cell the sun cannot reach through any portal ([`sun_walk`]),
//!   and whose boxes its host says lie under cover that stays in the cascades ([`SunCover`]), is
//!   sun-sealed: its rigid draws that fit inside its boxes leave the sun's cascades
//!   ([`SUN_SEALED_SHADOW`]) and keep only local-light shadows.
//! - Shadow views per cell (ADR-8874, [`VisCellShadowViews`]): the rigid draws of a cell no view
//!   reaches whose sun shadow can touch no reached cell's box and no outdoor frustum of a view
//!   ([`shadow_sweep_touches_box`], [`shadow_sweep_touches_planes`]) leave the sun's cascades the
//!   same way. Needs no cover. Every recorded outdoor frustum counts, also one the narrow walk
//!   missed and only the terrain's two-pixel edge slack reaches (ADR-10161). Off while any window
//!   view has no restricted outdoor list, or a texture view sees the outdoors even through that
//!   slack.
//!
//! The debug draw ([`VisCellGizmos`], gizmo group `viscells`, off by default) outlines every
//! cell box (green: reached last frame, red: not reached, yellow: the first view's cell) and
//! every portal (cyan outline and diagonals). Agent: `render.viscells`, `render.set_viscells`
//! (`book/agents/viscells.md`).

mod audio;
mod file;
mod inherit;
mod outdoor;
pub mod sun;
mod traverse;

pub use audio::{AudioZoneMouth, AudioZonePose, audio_zone_mouths};
pub use file::{
    CellBox, MAX_CELL_BOXES, MAX_VIS_CELLS, MAX_VIS_PORTALS, OUTDOOR_CELL, PORTAL_PLANAR_MIN_M,
    PORTAL_PLANAR_TOLERANCE, VISCELLS_FORMAT_VERSION, VisCellDef, VisCellInfo, VisCells, VisCellsDocument,
    VisCellsError, VisPortal, VisPortalDef, load_viscells_ron, viscells_file_version,
    viscells_migration_sources, viscells_ron_path, viscells_schema_fingerprint,
};
pub use inherit::{InheritedCells, VisCellMembers};
pub use outdoor::{
    OUTDOOR_EDGE_SLACK_PX, OUTDOOR_NODE_MARGIN_M, OutdoorCullCounts, OutdoorPortalViews, OutdoorPortals,
    outdoor_portals, widen_frustum,
};
pub use regia_scene::components::VisCell;
pub use sun::{
    SHADOW_VIEW_MARGIN_M, SHADOW_VIEW_SPREAD, SUN_COVER_MIN_ELEVATION_SIN, SUN_PORTAL_MIN_AREA_M2,
    SUN_SEAL_SLACK_M, SUN_SEALED_SHADOW, SunCover, SunWalk, SunWalkScratch, box_covered,
    shadow_sweep_touches_box, shadow_sweep_touches_planes, sun_walk,
};
pub use traverse::{
    EYE_ON_PORTAL_M, MAX_CELL_VISITS, MAX_OUTDOOR_FRUSTA, MAX_PLANES, MAX_PORTAL_DEPTH, PlaneSet, Traversal,
    TraversalScratch, traverse,
};

use regia_app::prelude::*;
use regia_core::math::{Color, Vec3};

use crate::gizmos::{AppGizmoBuilder, GizmoConfig, GizmoConfigGroup, Gizmos, gizmo_group_enabled};
use crate::mesh_lod::LodViewport;
use crate::runtime::{ExtractedFrame, ExtractedTarget};
use crate::{SHADOW_LOD_MESH_TAG, ShadowCast};

/// Cell culling switches (agent `render.set_viscells`).
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisCellSettings {
    /// Leave unreachable cells out of the colour pass (default true). False keeps the walk and
    /// its report but culls nothing.
    pub cull: bool,
}

impl Default for VisCellSettings {
    fn default() -> Self {
        Self { cull: true }
    }
}

/// Shadow views per cell (ADR-8874, agent `render.set_viscells { shadow_views }`). Without the
/// resource the cull runs (the default).
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisCellShadowViews {
    /// Leave out of the sun's cascades the draws of unreached cells whose shadow no view can see
    /// (module docs). Default true; needs [`VisCellSettings::cull`] too.
    pub cull: bool,
}

impl Default for VisCellShadowViews {
    fn default() -> Self {
        Self { cull: true }
    }
}

/// The debug overlay of cells and portals (gizmo group `viscells`). Off by default; turn it on
/// with `render.set_viscells { debug_draw: true }`, `debug.set_gizmos` or
/// [`crate::gizmos::GizmoConfigStore::set_enabled`].
#[derive(Default, Debug, Clone, Copy)]
pub struct VisCellGizmos;

impl GizmoConfigGroup for VisCellGizmos {
    const NAME: &'static str = "viscells";

    fn default_config() -> GizmoConfig {
        GizmoConfig {
            enabled: false,
            ..GizmoConfig::default()
        }
    }
}

/// One view's walk in the last [`cull_unreachable_cells`] run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VisCellViewStats {
    /// The view's index in [`ExtractedFrame::views`].
    pub view: usize,
    /// The view's eye, world metres.
    pub eye: [f32; 3],
    /// The id of the cell the eye is in (0 outdoors): the first of [`Self::eye_cells`].
    pub eye_cell: u16,
    /// The ids of the cells the walk started in, ascending: the eye's cell, or, for an eye in no
    /// cell box but in a doorway's wall gap, every cell of that doorway (ADR-7639).
    pub eye_cells: Vec<u16>,
    /// Cell entries the walk made ([`Traversal::visits`]).
    pub visits: u32,
    /// Portal passages ([`Traversal::portals_passed`]).
    pub portals_passed: u32,
    /// The walk hit [`MAX_CELL_VISITS`] and counted every cell reachable.
    pub capped: bool,
    /// A chain hit [`MAX_PORTAL_DEPTH`] and counted every cell connected beyond it reachable
    /// ([`Traversal::depth_capped`]).
    pub depth_capped: bool,
    /// The ids this view reaches, ascending.
    pub reachable: Vec<u16>,
}

/// What the last [`cull_unreachable_cells`] run did (agent `render.viscells`). Counts only, no
/// timing. Its lists keep their capacity: a steady frame allocates nothing.
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct VisCellStats {
    /// Runs since the app started.
    pub runs: u64,
    /// Cells were loaded, culling was on and the frame had a view.
    pub active: bool,
    /// One line per extracted view (empty when not active).
    pub views: Vec<VisCellViewStats>,
    /// The ids some view reaches, ascending (the union the cull used).
    pub reachable: Vec<u16>,
    /// Draws of entities carrying a `VisCell`.
    pub cell_draws: usize,
    /// Colour draws left out (removed, or turned shadow-only).
    pub culled_draws: usize,
    /// Of [`Self::culled_draws`], those kept as shadow-only draws (they cast shadows).
    pub shadow_only_draws: usize,
    /// Draws whose `VisCell` names a cell the file does not list (drawn).
    pub unknown_cell_draws: usize,
    /// Cell entries over every view's walk.
    pub visits: u32,
    /// Point and spot lights whose entity (or nearest tagged ancestor) carries a listed
    /// `VisCell` (stage 2).
    pub cell_lights: usize,
    /// Of [`Self::cell_lights`], those left out of the frame: in an unreached cell, and their
    /// range touches no reached cell ([`light_reaches`]). A culled light shades nothing and gets
    /// no shadow-map update.
    pub culled_lights: usize,
    /// The cells the sun walk reached, ascending, cell 0 included (stage 3, ADR-8414). Empty when
    /// no [`SunCover`] for the current sun is installed or the frame has no sun shadow.
    pub sun_reached: Vec<u16>,
    /// Cell entries the sun walk made ([`SunWalk::visits`]).
    pub sun_walk_visits: u32,
    /// Cells that are covered ([`SunCover`]) and not sun-reached: sun-sealed.
    pub sun_sealed_cells: usize,
    /// Draws taken out of every sun cascade because they lie inside a sun-sealed cell's box
    /// (given [`SUN_SEALED_SHADOW`], or left out of the frame: [`Self::sun_dropped_draws`]).
    pub sun_sealed_draws: usize,
    /// Of [`Self::sun_sealed_draws`], shadow-only draws left out of the frame because it has no
    /// local light to cast for.
    pub sun_dropped_draws: usize,
    /// The shadow-view cull ran this frame (ADR-8874): a shadowed sun, [`VisCellShadowViews`] on,
    /// and no view sees the outdoors unrestricted.
    pub shadow_view_active: bool,
    /// Draws of unreached cells the shadow-view cull tested (rigid, still casting into the sun's
    /// cascades after the seal).
    pub shadow_view_tested: usize,
    /// Of [`Self::shadow_view_tested`], draws whose shadow no view can see: taken out of every sun
    /// cascade ([`SUN_SEALED_SHADOW`], or left out of the frame:
    /// [`Self::shadow_view_dropped_draws`]).
    pub shadow_view_culled_draws: usize,
    /// Of [`Self::shadow_view_culled_draws`], shadow-only draws left out of the frame because it has
    /// no local light to cast for.
    pub shadow_view_dropped_draws: usize,
}

/// The entities whose colour draw the last [`cull_unreachable_cells`] run left out, for the
/// entity-extracted sets that run after it (glass, stage 2). Inserted by
/// [`crate::RenderPlugin`]; without it those sets are not cell-culled.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct VisCellCulled {
    /// Ascending.
    entities: Vec<Entity>,
    /// Glass instances [`crate::extract_glass`] left out this frame (agent
    /// `render.viscells` `culled_glass`).
    pub glass: usize,
}

impl VisCellCulled {
    /// Whether the cull left out `entity`'s colour draw this frame.
    #[must_use]
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.binary_search(&entity).is_ok()
    }

    /// The culled entities, ascending.
    #[must_use]
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// [`Self::contains`], counting a hit in [`Self::glass`]: `true` means leave the pane out.
    pub fn note_glass(&mut self, entity: Entity) -> bool {
        let culled = self.contains(entity);
        self.glass += usize::from(culled);
        culled
    }
}

/// The cull's reusable scratch.
#[derive(Default)]
pub struct VisCellCullScratch {
    walk: TraversalScratch,
    union: Vec<bool>,
    inherited: InheritedCells,
    /// This frame's culled entities, swapped into [`VisCellCulled`].
    culled: Vec<Entity>,
    /// Per local light: keep it.
    keep_lights: Vec<bool>,
    /// The sun walk (stage 3).
    sun: SunWalkScratch,
    /// Per cell index: sun-sealed this frame.
    sealed: Vec<bool>,
    /// Object-space bounds of the meshes the seal tested.
    bounds: sun::MeshBoundsCache,
    /// A texture view's outdoor list through the edge slack (only whether it sees outdoors).
    texture_outdoor: OutdoorPortals,
}

/// The cull's report, culled set, sun cover and shadow-view switch.
type CullOutputs<'w> = (
    Option<ResMut<'w, VisCellStats>>,
    Option<ResMut<'w, VisCellCulled>>,
    Option<Res<'w, SunCover>>,
    Option<Res<'w, VisCellShadowViews>>,
);

/// What the sun-side stages read besides the cells: the members, the cover and the shadow-view
/// switch.
type SunInputs<'a, 'w, 's> = (
    &'a VisCellMembers<'w, 's>,
    Option<&'a SunCover>,
    VisCellShadowViews,
);

/// `Render`, after mesh LOD selection: walks the portals from every extracted view and drops from
/// the colour pass the draws of entities whose [`VisCell`] no view reaches (module docs). Writes
/// [`VisCellStats`]. Allocation-free in steady state.
pub fn cull_unreachable_cells(
    cells: Option<Res<VisCells>>,
    settings: Option<Res<VisCellSettings>>,
    extracted: Option<ResMut<ExtractedFrame>>,
    viewport: Option<Res<LodViewport>>,
    mut members: VisCellMembers,
    mut scratch: Local<VisCellCullScratch>,
    (stats, culled, cover, shadow_views): CullOutputs<'_>,
) {
    let _span = tracing::info_span!("viscells_cull").entered();
    scratch.inherited.note_edits(&mut members);
    scratch.culled.clear();
    let Some(mut stats) = stats else {
        // The report is the only consumer that matters for tests; culling works without it.
        if let (Some(cells), Some(mut extracted)) = (cells, extracted) {
            let mut sink = VisCellStats::default();
            run_cull(
                &cells,
                settings.as_deref().copied().unwrap_or_default(),
                &mut extracted,
                viewport.as_deref(),
                (
                    &members,
                    cover.as_deref(),
                    shadow_views.as_deref().copied().unwrap_or_default(),
                ),
                &mut scratch,
                &mut sink,
            );
        }
        publish_culled(&mut scratch, culled);
        return;
    };
    stats.runs = stats.runs.wrapping_add(1);
    match (cells, extracted) {
        (Some(cells), Some(mut extracted)) => run_cull(
            &cells,
            settings.as_deref().copied().unwrap_or_default(),
            &mut extracted,
            viewport.as_deref(),
            (
                &members,
                cover.as_deref(),
                shadow_views.as_deref().copied().unwrap_or_default(),
            ),
            &mut scratch,
            &mut stats,
        ),
        _ => clear_stats(&mut stats),
    }
    publish_culled(&mut scratch, culled);
}

/// Hands this frame's culled entities to the later sets however the run ended (none when it did
/// not cull); the two lists swap, so both keep their capacity.
fn publish_culled(scratch: &mut VisCellCullScratch, culled: Option<ResMut<VisCellCulled>>) {
    if let Some(mut culled) = culled {
        scratch.culled.sort_unstable();
        std::mem::swap(&mut culled.entities, &mut scratch.culled);
        culled.glass = 0;
    }
}

fn clear_stats(stats: &mut VisCellStats) {
    stats.active = false;
    stats.views.clear();
    stats.reachable.clear();
    stats.cell_draws = 0;
    stats.culled_draws = 0;
    stats.shadow_only_draws = 0;
    stats.unknown_cell_draws = 0;
    stats.visits = 0;
    stats.cell_lights = 0;
    stats.culled_lights = 0;
    stats.sun_reached.clear();
    stats.sun_walk_visits = 0;
    stats.sun_sealed_cells = 0;
    stats.sun_sealed_draws = 0;
    stats.sun_dropped_draws = 0;
    stats.shadow_view_active = false;
    stats.shadow_view_tested = 0;
    stats.shadow_view_culled_draws = 0;
    stats.shadow_view_dropped_draws = 0;
}

fn run_cull(
    cells: &VisCells,
    settings: VisCellSettings,
    extracted: &mut ExtractedFrame,
    viewport: Option<&LodViewport>,
    (members, cover, shadow_views): SunInputs<'_, '_, '_>,
    scratch: &mut VisCellCullScratch,
    stats: &mut VisCellStats,
) {
    let runs = stats.runs;
    extracted.outdoor_portals.clear();
    extracted.outdoor_portal_views.clear();
    // Keep the per-view lines (and their `reachable` lists' capacity) from frame to frame:
    // clearing them freed each list, so every steady frame allocated it again.
    let views_kept = std::mem::take(&mut stats.views);
    clear_stats(stats);
    stats.runs = runs;
    if !settings.cull || extracted.views.is_empty() {
        return;
    }
    stats.active = true;
    stats.views = views_kept;
    let window = viewport.map_or([1920, 1080], |v| [v.width, v.height]);
    scratch.union.clear();
    scratch.union.resize(cells.cells.len(), false);
    let views = extracted.views.len();
    if stats.views.len() > views {
        stats.views.truncate(views);
    }
    stats.views.resize_with(views, Default::default);
    // The window views (split-screen players) each get their outdoor list (ADR-8843).
    let windows = extracted
        .views
        .iter()
        .filter(|v| matches!(v.target, ExtractedTarget::Window))
        .count();
    let outdoor = cells.index_of(file::OUTDOOR_CELL);
    // A texture view that reaches the outdoors records no outdoor frusta: what it sees there is
    // not known, so the shadow-view cull stands down (ADR-8874).
    let mut outdoor_unknown = false;
    for (index, view) in extracted.views.iter().enumerate() {
        let texture = match view.target {
            ExtractedTarget::Texture { asset_id } => {
                extracted.texture_view_settings.get(&asset_id).map(|s| s.size)
            }
            ExtractedTarget::Window => None,
        };
        let frustum = crate::visibility::view_frustum(view, window, texture);
        let eye = Vec3::from_array(view.camera.eye);
        let walk = traverse(cells, eye, frustum.planes(), &mut scratch.walk);
        let line = &mut stats.views[index];
        line.view = index;
        line.eye = view.camera.eye;
        line.eye_cell = cells.cells[walk.eye_cell].id;
        line.visits = walk.visits;
        line.portals_passed = walk.portals_passed;
        line.capped = walk.capped;
        line.depth_capped = walk.depth_capped;
        line.eye_cells.clear();
        line.eye_cells.extend(
            scratch
                .walk
                .start_cells()
                .iter()
                .map(|i| cells.cells[*i as usize].id),
        );
        line.reachable.clear();
        for (i, reached) in scratch.walk.reachable().iter().enumerate() {
            if *reached {
                line.reachable.push(cells.cells[i].id);
                scratch.union[i] = true;
            }
        }
        stats.visits = stats.visits.saturating_add(walk.visits);
        let reaches_outdoor =
            outdoor.is_some_and(|o| scratch.walk.reachable().get(o).copied().unwrap_or(true));
        if !matches!(view.target, ExtractedTarget::Window) && !outdoor_unknown {
            if reaches_outdoor || (shadow_views.cull && walk.depth_capped) {
                // A depth-capped walk floods past the cap, where the slack walk below may narrow
                // differently: not known either.
                outdoor_unknown = true;
            } else if shadow_views.cull {
                // The terrain also draws two pixels past this view's edge (ADR-8413): outdoors
                // seen only through that slack is still seen (ADR-10161). (A walk that hit the
                // visit cap reached every cell, so `reaches_outdoor` already holds.)
                let base = texture.unwrap_or(window);
                let size = [
                    (base[0] as f32 * view.viewport[2]).max(1.0) as u32,
                    (base[1] as f32 * view.viewport[3]).max(1.0) as u32,
                ];
                let list = &mut scratch.texture_outdoor;
                outdoor_portals(
                    cells,
                    view.camera.eye,
                    frustum.planes(),
                    size,
                    &mut scratch.walk,
                    list,
                );
                outdoor_unknown = !list.is_restricted() || !list.frusta().is_empty();
            }
        }
        // The outdoors the terrain, its grass and impostors, the water and the scatter may draw
        // for this window view: a second walk with the frustum widened by the terrain's edge
        // slack (ADR-8413), into the view's list (every window view, ADR-8843). Its own walk
        // already started outdoors or hit a cap: no list, the view is unrestricted (the widened
        // walk would only repeat the capped one).
        if matches!(view.target, ExtractedTarget::Window)
            && !walk.started_outdoors
            && !walk.capped
            && !walk.depth_capped
        {
            let size = [
                (window[0] as f32 * view.viewport[2]).max(1.0) as u32,
                (window[1] as f32 * view.viewport[3]).max(1.0) as u32,
            ];
            outdoor_portals(
                cells,
                view.camera.eye,
                frustum.planes(),
                size,
                &mut scratch.walk,
                extracted.outdoor_portal_views.push(index),
            );
        }
    }
    // The frame's one draw list (the scatter) and a host that draws the first view only take the
    // union: restricted only when every window view is.
    extracted
        .outdoor_portal_views
        .union_into(windows, &mut extracted.outdoor_portals);
    for (i, reached) in scratch.union.iter().enumerate() {
        if *reached {
            stats.reachable.push(cells.cells[i].id);
        }
    }
    scratch.inherited.refresh(members);
    let (union, inherited, culled_entities) = (&scratch.union, &scratch.inherited, &mut scratch.culled);
    let (mut cell_draws, mut culled, mut shadow_only, mut unknown) = (0, 0, 0, 0);
    extracted.meshes.retain_mut(|draw| {
        // The draw's own tag, else its nearest tagged ancestor's (stage 2); none: always drawn.
        let Some(cell) = members.cell_of(draw.entity, inherited) else {
            return true;
        };
        cell_draws += 1;
        let Some(index) = cells.index_of(cell) else {
            unknown += 1;
            return true;
        };
        if union[index] || crate::frame::is_shadow_lod_key(draw.lod_key) {
            // Reached, or already a shadow-only draw (shadow views are not culled in stage 1).
            return true;
        }
        culled += 1;
        culled_entities.push(draw.entity);
        if draw.shadow == ShadowCast::Off {
            return false;
        }
        draw.lod_key |= SHADOW_LOD_MESH_TAG;
        shadow_only += 1;
        true
    });
    stats.cell_draws = cell_draws;
    stats.culled_draws = culled;
    stats.shadow_only_draws = shadow_only;
    stats.unknown_cell_draws = unknown;
    cull_lights(cells, extracted, members, scratch, stats);
    seal_sun_casters(cells, cover, extracted, members, scratch, stats);
    if shadow_views.cull && !outdoor_unknown {
        cull_shadow_views(cells, extracted, members, scratch, stats);
    }
}

/// Shadow views per cell (ADR-8874, module docs): the rigid draws of unreached cells whose sun
/// shadow touches no reached cell's box and no outdoor frustum leave the sun's cascades. Runs
/// after the seal (which already took what it seals) and the light cull (it reads which local
/// lights stay).
fn cull_shadow_views(
    cells: &VisCells,
    extracted: &mut ExtractedFrame,
    members: &VisCellMembers,
    scratch: &mut VisCellCullScratch,
    stats: &mut VisCellStats,
) {
    if !extracted.directional_shadows {
        return;
    }
    let Some(travel) = extracted
        .directional
        .as_ref()
        .map(|sun| Vec3::from_array(sun.direction))
    else {
        return;
    };
    if travel.normalize_or_zero() == Vec3::ZERO || !travel.is_finite() {
        return;
    }
    let outdoor = cells.index_of(file::OUTDOOR_CELL);
    // Every window view must see the outdoors only through its recorded frusta: one with no
    // restricted list (it starts outdoors, its walk hit a cap, its slack walk overflowed) lets
    // the terrain draw everything, so nothing is known. Whether its narrow walk reached cell 0
    // does not matter: the slack walk can record a mouth the narrow one missed (ADR-10161).
    for (index, view) in extracted.views.iter().enumerate() {
        if matches!(view.target, ExtractedTarget::Window)
            && !extracted
                .outdoor_portal_views
                .get(index)
                .is_some_and(OutdoorPortals::is_restricted)
        {
            return;
        }
    }
    let _span = tracing::info_span!("viscells_shadow_views").entered();
    stats.shadow_view_active = true;
    let VisCellCullScratch {
        union,
        bounds,
        inherited,
        ..
    } = scratch;
    let (union, inherited) = (&*union, &*inherited);
    let portal_views = &extracted.outdoor_portal_views;
    let no_local_lights = extracted.local_lights.is_empty();
    let (mut tested, mut culled, mut dropped) = (0, 0, 0);
    extracted.meshes.retain_mut(|draw| {
        if !draw.joints.is_empty() || !draw.shadow.casts_sun() {
            return true;
        }
        let Some(index) = members
            .cell_of(draw.entity, inherited)
            .and_then(|cell| cells.index_of(cell))
        else {
            return true;
        };
        if union.get(index).copied().unwrap_or(true) {
            return true;
        }
        let Some((lo, hi)) = bounds.bounds(&draw.mesh) else {
            return true;
        };
        tested += 1;
        let (lo, hi) = sun::world_aabb(lo, hi, &draw.model);
        let indoor_seen = cells.cells.iter().enumerate().any(|(i, cell)| {
            Some(i) != outdoor
                && union[i]
                && cell.bounds.iter().any(|b| {
                    shadow_sweep_touches_box(lo, hi, travel, b, SHADOW_VIEW_MARGIN_M, SHADOW_VIEW_SPREAD)
                })
        });
        // Every recorded frustum, also one only the edge slack reaches (ADR-10161).
        let outdoor_seen = portal_views.iter().any(|(_, list)| {
            list.frusta().iter().any(|f| {
                shadow_sweep_touches_planes(lo, hi, travel, f, SHADOW_VIEW_MARGIN_M, SHADOW_VIEW_SPREAD)
            })
        });
        if indoor_seen || outdoor_seen {
            return true;
        }
        culled += 1;
        if no_local_lights && crate::frame::is_shadow_lod_key(draw.lod_key) {
            dropped += 1;
            return false;
        }
        draw.shadow = SUN_SEALED_SHADOW;
        true
    });
    stats.shadow_view_tested = tested;
    stats.shadow_view_culled_draws = culled;
    stats.shadow_view_dropped_draws = dropped;
}

/// Stage 3 (ADR-8414, [`sun`]): takes the draws inside sun-sealed cells out of the sun's
/// cascades. Runs after the colour and light culls, so it sees which local lights stay.
fn seal_sun_casters(
    cells: &VisCells,
    cover: Option<&SunCover>,
    extracted: &mut ExtractedFrame,
    members: &VisCellMembers,
    scratch: &mut VisCellCullScratch,
    stats: &mut VisCellStats,
) {
    let Some(cover) = cover.filter(|c| !c.cells().is_empty()) else {
        return;
    };
    if !extracted.directional_shadows {
        return;
    }
    let Some(direction) = extracted.directional.as_ref().map(|sun| sun.direction) else {
        return;
    };
    if !cover.matches_sun(direction) {
        // A cover computed for another sun vouches for nothing.
        return;
    }
    let _span = tracing::info_span!("viscells_sun_seal").entered();
    let VisCellCullScratch {
        sun,
        sealed,
        bounds,
        inherited,
        ..
    } = scratch;
    let walk = sun_walk(cells, Vec3::from_array(direction), sun);
    stats.sun_walk_visits = walk.visits;
    sealed.clear();
    sealed.resize(cells.cells.len(), false);
    let mut sealed_cells = 0;
    for (i, cell) in cells.cells.iter().enumerate() {
        let reached = sun.reached().get(i).copied().unwrap_or(true);
        if reached {
            stats.sun_reached.push(cell.id);
        } else if i > 0 && cover.contains(cell.id) {
            sealed[i] = true;
            sealed_cells += 1;
        }
    }
    stats.sun_sealed_cells = sealed_cells;
    if sealed_cells == 0 {
        return;
    }
    let no_local_lights = extracted.local_lights.is_empty();
    let (mut sealed_draws, mut dropped) = (0, 0);
    let inherited = &*inherited;
    extracted.meshes.retain_mut(|draw| {
        if !draw.joints.is_empty() || !draw.shadow.casts_sun() {
            return true;
        }
        let Some(index) = members
            .cell_of(draw.entity, inherited)
            .and_then(|cell| cells.index_of(cell))
        else {
            return true;
        };
        if !sealed[index] {
            return true;
        }
        let Some((lo, hi)) = bounds.bounds(&draw.mesh) else {
            return true;
        };
        if !cells.cells[index]
            .bounds
            .iter()
            .any(|b| sun::inside_box(lo, hi, &draw.model, b))
        {
            return true;
        }
        sealed_draws += 1;
        if no_local_lights && crate::frame::is_shadow_lod_key(draw.lod_key) {
            dropped += 1;
            return false;
        }
        draw.shadow = SUN_SEALED_SHADOW;
        true
    });
    stats.sun_sealed_draws = sealed_draws;
    stats.sun_dropped_draws = dropped;
}

/// Leaves out the point and spot lights of unreached cells (stage 2, ADR-7673): a light whose
/// entity resolves to a listed cell no view reaches, and whose range touches no reached cell
/// ([`light_reaches`]). [`ExtractedFrame::local_lights`] and its index-aligned side tables
/// ([`ExtractedFrame::local_light_entities`], the light extras' IES and LTC marks) are compacted
/// together; lights without an entity (area stand-ins, particle lights) stay.
fn cull_lights(
    cells: &VisCells,
    extracted: &mut ExtractedFrame,
    members: &VisCellMembers,
    scratch: &mut VisCellCullScratch,
    stats: &mut VisCellStats,
) {
    let tagged = extracted
        .local_light_entities
        .len()
        .min(extracted.local_lights.len());
    let keep = &mut scratch.keep_lights;
    keep.clear();
    keep.resize(extracted.local_lights.len(), true);
    let (mut cell_lights, mut culled) = (0, 0);
    for (i, light) in extracted.local_lights.iter().take(tagged).enumerate() {
        let entity = extracted.local_light_entities[i];
        let Some(index) = members
            .cell_of(entity, &scratch.inherited)
            .and_then(|cell| cells.index_of(cell))
        else {
            continue;
        };
        cell_lights += 1;
        if scratch.union[index] {
            continue;
        }
        let (position, range) = match light {
            crate::LabLocalLight::Point(p) => (p.position, p.radius),
            crate::LabLocalLight::Spot(s) => (s.position, s.range),
        };
        if light_reaches(cells, index, &scratch.union, Vec3::from_array(position), range) {
            continue;
        }
        keep[i] = false;
        culled += 1;
    }
    stats.cell_lights = cell_lights;
    stats.culled_lights = culled;
    if culled == 0 {
        return;
    }
    retain_by(&mut extracted.local_lights, keep);
    retain_by(&mut extracted.local_light_entities, keep);
    retain_by(&mut extracted.light_extras.per_light, keep);
    retain_by(&mut extracted.light_extras.ltc_spheres, keep);
}

/// Keeps `list[i]` where `keep[i]` (and every entry past `keep`).
fn retain_by<T>(list: &mut Vec<T>, keep: &[bool]) {
    let mut i = 0;
    list.retain(|_| {
        let kept = keep.get(i).copied().unwrap_or(true);
        i += 1;
        kept
    });
}

/// Whether a light at `position` with influence `range` (metres), in the unreached cell at
/// `own` (an index into [`VisCells::cells`]), can light a reached cell: its sphere touches a box
/// of a reached indoor cell, or the outdoors (cell 0) is reached and the sphere is not inside
/// one of its own cell's boxes. A non-finite or non-positive range always reaches (ADR-7673:
/// conservative, so culling a light never changes a pixel of a reached cell's surfaces).
#[must_use]
pub fn light_reaches(cells: &VisCells, own: usize, reached: &[bool], position: Vec3, range: f32) -> bool {
    if !(range.is_finite() && range > 0.0) || !position.is_finite() {
        return true;
    }
    for (i, cell) in cells.cells.iter().enumerate() {
        if !reached.get(i).copied().unwrap_or(false) {
            continue;
        }
        if i == 0 {
            let enclosed = cells.cells.get(own).is_some_and(|own| {
                own.bounds.iter().any(|b| {
                    let local = b.to_local(position).abs();
                    let half = Vec3::from_array(b.half_extents);
                    (local + Vec3::splat(range)).cmple(half).all()
                })
            });
            if !enclosed {
                return true;
            }
            continue;
        }
        if cell.bounds.iter().any(|b| b.distance(position) <= range) {
            return true;
        }
    }
    false
}

/// One debug segment of [`debug_segments`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisCellSegment {
    /// Start, world metres.
    pub a: [f32; 3],
    /// End, world metres.
    pub b: [f32; 3],
    /// Line colour.
    pub color: Color,
}

/// Colour of a cell some view reached last frame.
pub const REACHED_COLOR: Color = Color::rgb(0.3, 1.0, 0.4);
/// Colour of a cell no view reached last frame (or with culling off).
pub const UNREACHED_COLOR: Color = Color::rgb(1.0, 0.3, 0.3);
/// Colour of the first view's own cell (each of its cells when it stands in a doorway).
pub const EYE_CELL_COLOR: Color = Color::rgb(1.0, 0.9, 0.2);
/// Colour of portals.
pub const PORTAL_COLOR: Color = Color::rgb(0.3, 0.9, 1.0);

/// Box edges by corner index ([`CellBox::corners`]).
const BOX_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0),
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

/// The overlay's segments: 12 per cell box (coloured by `stats`: reached, not reached, the first
/// view's cells) and 6 per portal (outline and both diagonals). `out` is cleared first.
pub fn debug_segments(cells: &VisCells, stats: Option<&VisCellStats>, out: &mut Vec<VisCellSegment>) {
    out.clear();
    let first = stats.and_then(|s| s.views.first());
    for cell in &cells.cells {
        let reached = stats.is_some_and(|s| s.reachable.binary_search(&cell.id).is_ok());
        let eye_cell = first.is_some_and(|v| v.eye_cell == cell.id || v.eye_cells.contains(&cell.id));
        let color = if eye_cell {
            EYE_CELL_COLOR
        } else if reached {
            REACHED_COLOR
        } else {
            UNREACHED_COLOR
        };
        for b in &cell.bounds {
            let c = b.corners();
            for (i, j) in BOX_EDGES {
                out.push(VisCellSegment {
                    a: c[i].to_array(),
                    b: c[j].to_array(),
                    color,
                });
            }
        }
    }
    for portal in &cells.portals {
        let q = portal.quad;
        for (i, j) in [(0, 1), (1, 2), (2, 3), (3, 0), (0, 2), (1, 3)] {
            out.push(VisCellSegment {
                a: q[i].to_array(),
                b: q[j].to_array(),
                color: PORTAL_COLOR,
            });
        }
    }
}

/// `PostUpdate`, only while the `viscells` gizmo group is on: draws [`debug_segments`] with last
/// frame's reached set.
pub fn draw_viscells(
    cells: Option<Res<VisCells>>,
    stats: Option<Res<VisCellStats>>,
    mut segments: Local<Vec<VisCellSegment>>,
    mut gizmos: Gizmos<VisCellGizmos>,
) {
    let Some(cells) = cells else {
        return;
    };
    debug_segments(&cells, stats.as_deref(), &mut segments);
    for s in segments.iter() {
        gizmos.line(Vec3::from_array(s.a), Vec3::from_array(s.b), s.color);
    }
}

/// Registers the settings, the report, the gizmo group and both systems (called by
/// [`crate::RenderPlugin`]).
pub(crate) fn install(app: &mut App) {
    app.init_resource::<VisCellSettings>()
        .init_resource::<VisCellStats>()
        .init_resource::<VisCellCulled>()
        .init_gizmo_group::<VisCellGizmos>()
        .add_systems(
            Render,
            cull_unreachable_cells
                .after(crate::mesh_lod::select_mesh_lods)
                .before(crate::glass::apply_glass_damage)
                .before(crate::visibility::update_view_visibility),
        )
        .add_systems(
            PostUpdate,
            draw_viscells.run_if(gizmo_group_enabled::<VisCellGizmos>),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Room A (cell 1, z −4 to 4) and room B (cell 2, z −12 to −4), 3 m tall, 8 m wide.
    fn rooms() -> VisCells {
        VisCells::from_ron(
            "(cells: [
                (id: 1, bounds: [(center: (0.0, 1.5, 0.0), half_extents: (4.0, 1.5, 4.0))]),
                (id: 2, bounds: [(center: (0.0, 1.5, -8.0), half_extents: (4.0, 1.5, 4.0))]),
            ], portals: [
                (a: 1, b: 2, quad: [(-1.0, 0.0, -4.0), (1.0, 0.0, -4.0), (1.0, 2.0, -4.0), (-1.0, 2.0, -4.0)]),
            ])",
        )
        .expect("loads")
    }

    #[test]
    fn a_light_reaches_a_reached_room_its_range_touches() {
        let cells = rooms();
        let b = cells.index_of(2).expect("listed");
        // Only room A (index 1) reached.
        let reached = [false, true, false];
        let at = Vec3::new(0.0, 1.5, -9.0);
        assert!(
            !light_reaches(&cells, b, &reached, at, 3.0),
            "5 m from A: 3 m falls short"
        );
        assert!(light_reaches(&cells, b, &reached, at, 5.0), "exactly at A's face");
        assert!(light_reaches(&cells, b, &reached, at, 6.0));
        // Unbounded or broken ranges always reach (conservative).
        for range in [0.0, -1.0, f32::INFINITY, f32::NAN] {
            assert!(light_reaches(&cells, b, &reached, at, range), "range {range}");
        }
        assert!(light_reaches(&cells, b, &reached, Vec3::NAN, 1.0));
    }

    #[test]
    fn with_the_outdoors_reached_only_a_light_enclosed_by_its_own_box_stays_out() {
        let cells = rooms();
        let b = cells.index_of(2).expect("listed");
        // Only the outdoors (index 0) reached.
        let reached = [true, false, false];
        let centre = Vec3::new(0.0, 1.5, -8.0);
        assert!(
            !light_reaches(&cells, b, &reached, centre, 1.0),
            "1 m inside a 1.5 m half-height box"
        );
        assert!(
            light_reaches(&cells, b, &reached, centre, 2.0),
            "pokes out through the ceiling"
        );
        assert!(
            light_reaches(&cells, b, &reached, Vec3::new(3.5, 1.5, -8.0), 1.0),
            "pokes out the side"
        );
    }
}
