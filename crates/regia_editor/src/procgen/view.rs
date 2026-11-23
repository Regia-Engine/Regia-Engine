// SPDX-License-Identifier: MPL-2.0
//! Viewport side of the procedural editor (spec 66 § 4.1 and § 7, spec 67, spec 64 § 2.4):
//! "generate around view" (the editor camera becomes an anchor for `Anchored` generators), the
//! tiles overlay (one outline per cell, colored by state), `Shape` volume wireframes and `Spline`
//! curves with control points and tangent handles.
//!
//! Everything that decides *what* to draw is a pure function returning [`Line`]s or handle lists,
//! so tests run headless; [`tick`] only pushes the lines into [`regia_render::DebugDraw`].

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_procgen::debug_view::{state_color, tile_overlay};
use regia_procgen::tiles::{AnchorView, ProcGenExtraAnchors, ProcGenTiles, TileKey};
use regia_procgen::world::{ProcGenerator, world_shape, world_spline};
use regia_scene::shape::Shape;
use regia_scene::spline::{Spline, TangentMode};
use regia_scene::{GlobalTransform, Transform};

use super::debug_view::ProcPreview;
use super::inspect::ProcInspect;

/// One debug line: from, to, linear RGBA.
pub type Line = ([f32; 3], [f32; 3], [f32; 4]);

/// Segments in each circle of a sphere gizmo.
pub const CIRCLE_SEGMENTS: usize = 24;
/// Chord error of the spline curve, meters.
pub const SPLINE_ERROR_M: f32 = 0.05;
/// Budget of unselected volumes drawn per tick while `show_volumes` is on (the selection always
/// draws, uncapped).
pub const MAX_VOLUMES: usize = 256;
/// Selected control points and handles.
const SELECTED_COLOR: [f32; 4] = [1.0, 0.85, 0.0, 1.0];
/// Unselected volumes.
const VOLUME_COLOR: [f32; 4] = [0.2, 0.8, 1.0, 0.6];
/// Spline curves.
const CURVE_COLOR: [f32; 4] = [0.95, 0.55, 0.2, 1.0];
/// Tangent handle stems.
const TANGENT_COLOR: [f32; 4] = [0.8, 0.8, 0.3, 1.0];
/// Half size of the cross drawn at a control point or handle, meters.
const HANDLE_HALF_M: f32 = 0.15;

/// Sets the two switches of the procedural view (`None` keeps a value). With "generate around
/// view" off the editor camera is removed from the anchors on the next tick.
pub fn set_switches(
    world: &mut World,
    around: Option<bool>,
    overlay: Option<bool>,
    volumes: Option<bool>,
    biomes: Option<bool>,
) {
    let mut preview = world.get_resource_or_insert_with(ProcPreview::default);
    if let Some(v) = around {
        preview.generate_around_view = v;
    }
    if let Some(v) = overlay {
        preview.tiles_overlay = v;
    }
    if let Some(v) = volumes {
        preview.show_volumes = v;
    }
    if let Some(v) = biomes {
        preview.biome_overlay = v;
    }
    sync_view_anchor(world);
}

/// Sets the biome overlay views (spec 65 § 7; `None` keeps a value): claim tint, blend bands,
/// footprints and the filter view.
pub fn set_biome_views(
    world: &mut World,
    tint: Option<bool>,
    bands: Option<bool>,
    footprints: Option<bool>,
    filtered: Option<bool>,
) {
    let mut preview = world.get_resource_or_insert_with(ProcPreview::default);
    let v = &mut preview.biome_views;
    for (slot, value) in [
        (&mut v.tint, tint),
        (&mut v.bands, bands),
        (&mut v.footprints, footprints),
        (&mut v.filtered, filtered),
    ] {
        if let Some(value) = value {
            *slot = value;
        }
    }
}

/// Puts the editor camera into [`ProcGenExtraAnchors`] while "generate around view" is on, and takes
/// it out when it is off. The scheduler then requests and releases cells for it through the same
/// path a game camera uses. The editor owns one entry: syncing replaces only the entry the editor
/// inserted last (tracked in `ViewAnchorSlot`), never anchors other hosts supplied, and with no
/// [`crate::viewport::EditorCamera`] nothing is inserted — a headless world must not gain an
/// anchor at the default camera position.
pub fn sync_view_anchor(world: &mut World) {
    let on = world
        .get_resource::<ProcPreview>()
        .is_some_and(|p| p.generate_around_view);
    // The slot is an index. Value equality cannot find a non-finite pose (`NaN != NaN`), and it
    // removes a host anchor that happens to share the editor camera's pose.
    if let Some(index) = world.get_resource::<ViewAnchorSlot>().and_then(|slot| slot.0)
        && let Some(mut extra) = world.get_resource_mut::<ProcGenExtraAnchors>()
        && index < extra.0.len()
    {
        extra.0.remove(index);
    }
    world.get_resource_or_insert_with(ViewAnchorSlot::default).0 = None;
    if !on {
        return;
    }
    let Some(camera) = world.get_resource::<crate::viewport::EditorCamera>().cloned() else {
        return;
    };
    let pos = camera.eye();
    let view_dir = camera.forward();
    if !pos.is_finite() || !view_dir.is_finite() {
        return;
    }
    let anchor = AnchorView {
        pos,
        view_dir: Some(view_dir),
        frustum: editor_frustum(&camera),
        reach_scale: 1.0,
    };
    let index = {
        let mut extras = world.get_resource_or_insert_with(ProcGenExtraAnchors::default);
        let index = extras.0.len();
        extras.0.push(anchor);
        index
    };
    world.resource_mut::<ViewAnchorSlot>().0 = Some(index);
}

/// Index of the editor's own entry in [`ProcGenExtraAnchors`] from the last sync, so the next
/// sync replaces exactly that entry and leaves host-supplied anchors alone.
#[derive(Resource, Default)]
struct ViewAnchorSlot(Option<usize>);

/// The editor camera's view frustum, or `None` when the field of view cannot make a projection.
///
/// 16:9, the editor near and far, clip `0 ≤ z ≤ w` (the same convention a game camera anchor uses).
/// A tier with `view_gate` then drops cells outside it. `view_gate` still defaults to off, so a
/// generator that does not set the gate is unchanged.
fn editor_frustum(camera: &crate::viewport::EditorCamera) -> Option<regia_core::math::Frustum> {
    let fov = camera.fov_deg.to_radians();
    if !fov.is_finite() || fov <= 0.0 || fov >= std::f32::consts::PI {
        return None;
    }
    let proj = regia_core::glam::camera::rh::proj::directx::perspective(
        fov,
        16.0 / 9.0,
        crate::viewport::EditorCamera::NEAR,
        crate::viewport::EditorCamera::FAR,
    );
    Some(regia_core::math::Frustum::from_view_projection(
        &(proj * camera.view()),
    ))
}

/// The cells the scheduler wants around the current anchors for one generator (what the editor camera
/// asks for when "generate around view" is on). Empty without [`ProcGenTiles`].
#[must_use]
pub fn requested_cells(world: &mut World, generator: Entity) -> Vec<TileKey> {
    let Some(runtime) = world
        .get::<ProcGenerator>(generator)
        .and_then(|g| g.tiles.as_ref())
        .and_then(|t| t.runtime.clone())
    else {
        return Vec::new();
    };
    let anchors = regia_procgen::tiles::gather_anchors(world, runtime.anchors);
    world
        .get_resource::<ProcGenTiles>()
        .and_then(|t| t.generators.get(&generator))
        .map(|g| g.wanted_keys(&anchors, &runtime))
        .unwrap_or_default()
}

/// The cells of `generator` that are resident now, sorted.
#[must_use]
pub fn resident_cells(world: &World, generator: Entity) -> Vec<TileKey> {
    use regia_procgen::tiles::TileState;
    world
        .get_resource::<ProcGenTiles>()
        .and_then(|t| t.generators.get(&generator))
        .map(|g| {
            g.entries
                .iter()
                .filter(|(_, e)| e.state == TileState::Resident)
                .map(|(k, _)| *k)
                .collect()
        })
        .unwrap_or_default()
}

/// A flat (XZ) circle of [`CIRCLE_SEGMENTS`] line segments centered at `center`.
fn circle_lines(center: Vec3, radius: f32, color: [f32; 4], out: &mut Vec<Line>) {
    if !radius.is_finite() || radius <= 0.0 {
        return;
    }
    for s in 0..CIRCLE_SEGMENTS {
        let at = |k: usize| {
            let a = std::f32::consts::TAU * k as f32 / CIRCLE_SEGMENTS as f32;
            let (sin, cos) = a.sin_cos();
            (center + Vec3::new(cos * radius, 0.0, sin * radius)).to_array()
        };
        out.push((at(s), at(s + 1), color));
    }
}

/// The tiles overlay: for every cell of every tiled generator one rectangle outline (four lines) at
/// the generator's height, colored like the spec 66 debug view — seam-risk cells in
/// [`regia_procgen::debug_view::SEAM_RISK_COLOR`], other cells by state — plus each anchor's
/// build and release circles. Empty while the overlay is off. (Queued-cell priority text needs a
/// text drawer and is runtime-only; the editor switch and `procgen.set_debug { tiles }` are two
/// independent draws of the same grid.)
#[must_use]
pub fn overlay_lines(world: &mut World) -> Vec<Line> {
    if !world
        .get_resource::<ProcPreview>()
        .is_some_and(|p| p.tiles_overlay)
    {
        return Vec::new();
    }
    // `requested_cells` is empty without the tile plugin. The overlay must not abort the tick.
    if world.get_resource::<ProcGenTiles>().is_none() {
        return Vec::new();
    }
    use regia_procgen::debug_view::{BUILD_COLOR, RELEASE_COLOR, SEAM_RISK_COLOR};
    let mut out = Vec::new();
    for overlay in tile_overlay(world) {
        for cell in &overlay.cells {
            let c = if cell.seam_risk {
                SEAM_RISK_COLOR
            } else {
                state_color(cell.state)
            };
            let color = [c.r, c.g, c.b, 1.0];
            let [x0, z0, x1, z1] = cell.rect;
            let y = overlay.y;
            let corners = [[x0, y, z0], [x1, y, z0], [x1, y, z1], [x0, y, z1]];
            for (i, from) in corners.iter().enumerate() {
                out.push((*from, corners[(i + 1) % 4], color));
            }
        }
        for (pos, build_m, release_m) in &overlay.circles {
            let build_c = [BUILD_COLOR.r, BUILD_COLOR.g, BUILD_COLOR.b, 1.0];
            let release_c = [RELEASE_COLOR.r, RELEASE_COLOR.g, RELEASE_COLOR.b, 1.0];
            circle_lines(*pos, *build_m, build_c, &mut out);
            circle_lines(*pos, *release_m, release_c, &mut out);
        }
    }
    out
}

/// A world transform from the entity's components.
fn transform_of(world: &World, entity: Entity) -> Transform {
    let has_parent = world.get::<ChildOf>(entity).is_some();
    match (
        has_parent,
        world.get::<GlobalTransform>(entity),
        world.get::<Transform>(entity),
    ) {
        (true, Some(g), _) => g.to_transform(),
        (_, _, Some(t)) => *t,
        (_, Some(g), None) => g.to_transform(),
        _ => Transform::default(),
    }
}

fn rotate(q: [f32; 4], v: Vec3) -> Vec3 {
    regia_core::math::Quat::from_xyzw(q[0], q[1], q[2], q[3]) * v
}

/// The wireframe of a `Shape` volume in world space: a box as its 12 edges, a sphere as three great
/// circles of [`CIRCLE_SEGMENTS`] segments; capsules and hulls as their bounding box (the form the
/// generator reads, [`world_shape`]). A shape with an edge band adds the outer form in a dimmer color.
#[must_use]
pub fn shape_lines(shape: &Shape, tf: &Transform, color: [f32; 4]) -> Vec<Line> {
    use regia_procgen::data::ShapeForm;
    let data = world_shape(shape, tf);
    let center = Vec3::from(data.position);
    let mut out = Vec::new();
    let form = |grow: f32, color: [f32; 4], out: &mut Vec<Line>| match data.form {
        ShapeForm::Box { half_extents } => {
            let h = Vec3::from(half_extents) + Vec3::splat(grow);
            let corner = |i: usize| {
                let c = Vec3::new(
                    if i & 1 == 0 { -h.x } else { h.x },
                    if i & 2 == 0 { -h.y } else { h.y },
                    if i & 4 == 0 { -h.z } else { h.z },
                );
                (center + rotate(data.rotation, c)).to_array()
            };
            for (a, b) in [
                (0, 1),
                (2, 3),
                (4, 5),
                (6, 7),
                (0, 2),
                (1, 3),
                (4, 6),
                (5, 7),
                (0, 4),
                (1, 5),
                (2, 6),
                (3, 7),
            ] {
                out.push((corner(a), corner(b), color));
            }
        }
        ShapeForm::Sphere { radius } => {
            let r = radius + grow;
            if r.is_finite() && r > 0.0 {
                for axis in 0..3 {
                    for s in 0..CIRCLE_SEGMENTS {
                        let at = |k: usize| {
                            let a = std::f32::consts::TAU * k as f32 / CIRCLE_SEGMENTS as f32;
                            let (sin, cos) = a.sin_cos();
                            let local = match axis {
                                0 => Vec3::new(0.0, cos, sin),
                                1 => Vec3::new(cos, 0.0, sin),
                                _ => Vec3::new(cos, sin, 0.0),
                            } * r;
                            (center + rotate(data.rotation, local)).to_array()
                        };
                        out.push((at(s), at(s + 1), color));
                    }
                }
            }
        }
    };
    form(0.0, color, &mut out);
    if data.edge_m > 0.0 {
        let dim = [color[0] * 0.5, color[1] * 0.5, color[2] * 0.5, color[3]];
        form(data.edge_m, dim, &mut out);
    }
    out
}

/// A tangent handle of a spline control point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TangentHandle {
    /// The control point it belongs to.
    pub point: usize,
    /// The incoming handle (else outgoing).
    pub incoming: bool,
    /// World position of the handle.
    pub position: [f32; 3],
}

/// What the editor shows for one spline.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplineGizmo {
    /// The curve as line segments (a polyline within [`SPLINE_ERROR_M`]).
    pub curve: Vec<Line>,
    /// World positions of the control points, in order.
    pub points: Vec<[f32; 3]>,
    /// Tangent handles of `Free` and `Aligned` points (the other modes derive their tangents).
    pub tangents: Vec<TangentHandle>,
}

/// The gizmo of a spline under a world transform. Control point handles sit exactly on the authored
/// control points (through [`world_spline`]); a tangent handle sits a third of the tangent away from its
/// point (the Hermite-to-Bezier control point), the incoming one on the far side.
#[must_use]
pub fn spline_gizmo(spline: &Spline, tf: &Transform) -> SplineGizmo {
    let world = world_spline(spline, tf);
    let poly = world.to_polyline(SPLINE_ERROR_M);
    let curve = poly
        .windows(2)
        .map(|w| (w[0].to_array(), w[1].to_array(), CURVE_COLOR))
        .collect();
    let mut tangents = Vec::new();
    for (i, p) in world.points.iter().enumerate() {
        let (t_in, t_out) = match p.mode {
            TangentMode::Free => (p.tangent_in, p.tangent_out),
            TangentMode::Aligned => {
                let dir = p.tangent_out.normalize_or_zero();
                (dir * p.tangent_in.length(), p.tangent_out)
            }
            _ => continue,
        };
        tangents.push(TangentHandle {
            point: i,
            incoming: true,
            position: (p.position - t_in / 3.0).to_array(),
        });
        tangents.push(TangentHandle {
            point: i,
            incoming: false,
            position: (p.position + t_out / 3.0).to_array(),
        });
    }
    SplineGizmo {
        curve,
        points: world.points.iter().map(|p| p.position.to_array()).collect(),
        tangents,
    }
}

fn cross(at: [f32; 3], color: [f32; 4], out: &mut Vec<Line>) {
    for axis in 0..3 {
        let mut a = at;
        let mut b = at;
        a[axis] -= HANDLE_HALF_M;
        b[axis] += HANDLE_HALF_M;
        out.push((a, b, color));
    }
}

/// Tessellated spline gizmos by entity (rescue review item 6: `to_polyline` rebuilds the whole
/// curve tables per call, and `tick` runs every editor frame). An entry is reused until the
/// spline's value or the entity's transform changes; entities that no longer hold a `Spline` are
/// pruned by [`volume_lines`].
#[derive(Resource, Default)]
struct SplineGizmoCache {
    entries: std::collections::BTreeMap<Entity, (Spline, Transform, SplineGizmo)>,
    /// Times a gizmo was tessellated (tests: a quiet tick reuses the cache).
    rebuilds: u64,
}

/// The gizmo of the entity's spline, tessellated once per change rather than per frame.
fn cached_spline_gizmo(world: &mut World, entity: Entity, tf: &Transform) -> Option<SplineGizmo> {
    let current = world.get::<Spline>(entity)?;
    if let Some((cached, ttf, gizmo)) = world
        .get_resource::<SplineGizmoCache>()
        .and_then(|c| c.entries.get(&entity))
        && *cached == *current
        && ttf == tf
    {
        return Some(gizmo.clone());
    }
    let gizmo = spline_gizmo(current, tf);
    let spline = current.clone();
    let mut cache = world.get_resource_or_insert_with(SplineGizmoCache::default);
    cache.rebuilds += 1;
    cache.entries.insert(entity, (spline, *tf, gizmo.clone()));
    Some(gizmo)
}

/// Lines for a spline's gizmo; the selected one adds crosses on control points and tangent handles
/// with stems.
fn spline_lines(g: &SplineGizmo, selected: bool, out: &mut Vec<Line>) {
    out.extend_from_slice(&g.curve);
    if !selected {
        return;
    }
    for p in &g.points {
        cross(*p, SELECTED_COLOR, out);
    }
    for h in &g.tangents {
        if let Some(p) = g.points.get(h.point) {
            out.push((*p, h.position, TANGENT_COLOR));
        }
        cross(h.position, TANGENT_COLOR, out);
    }
}

/// Shape and spline gizmos: the selected entities always (uncapped), every other volume while
/// [`ProcPreview::show_volumes`] is on, unselected ones in entity order up to [`MAX_VOLUMES`].
#[must_use]
pub fn volume_lines(world: &mut World) -> Vec<Line> {
    let show_all = world
        .get_resource::<ProcPreview>()
        .is_some_and(|p| p.show_volumes);
    let selected: Vec<Entity> = world
        .get_resource::<crate::selection::Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    if !show_all && selected.is_empty() {
        return Vec::new();
    }
    let mut volumes: Vec<Entity> = world
        .query_filtered::<Entity, Or<(With<Shape>, With<Spline>)>>()
        .iter(world)
        .collect();
    volumes.sort();
    // The selection always draws; the budget fills with unselected volumes (rescue review item 8:
    // a selected entity past the first MAX_VOLUMES used to be dropped with its handles).
    let mut entities: Vec<Entity> = volumes.iter().copied().filter(|e| selected.contains(e)).collect();
    if show_all {
        entities.extend(
            volumes
                .iter()
                .filter(|e| !selected.contains(e))
                .take(MAX_VOLUMES.saturating_sub(entities.len())),
        );
    }
    let mut out = Vec::new();
    for e in entities {
        let tf = transform_of(world, e);
        let is_selected = selected.contains(&e);
        if let Some(shape) = world.get::<Shape>(e) {
            let color = if is_selected { SELECTED_COLOR } else { VOLUME_COLOR };
            out.extend(shape_lines(shape, &tf, color));
        }
        if world.get::<Spline>(e).is_some()
            && let Some(gizmo) = cached_spline_gizmo(world, e, &tf)
        {
            spline_lines(&gizmo, is_selected, &mut out);
        }
    }
    let stale: Vec<Entity> = world
        .get_resource::<SplineGizmoCache>()
        .map(|c| c.entries.keys().copied().collect())
        .unwrap_or_default();
    for e in stale {
        if world.get::<Spline>(e).is_none()
            && let Some(mut cache) = world.get_resource_mut::<SplineGizmoCache>()
        {
            cache.entries.remove(&e);
        }
    }
    out
}

/// Biome areas outlined by the biome overlay.
const BIOME_COLOR: [f32; 4] = [0.35, 0.85, 0.35, 1.0];
/// A biome area with `preview_alone` (the editor regenerates only these while any is set).
const BIOME_ALONE_COLOR: [f32; 4] = [1.0, 0.2, 0.9, 1.0];
/// The Inspect table's hovered row: its element's beam is white (the selected one is gold).
const INSPECT_HOVER_COLOR: [f32; 4] = [0.85, 0.95, 1.0, 1.0];
/// The beam's height, metres (the selected element's marker in `debug_view` is 1.2 m).
const INSPECT_HOVER_BEAM_M: f32 = 1.2;

/// The Inspect table's hovered element as a vertical beam (spec 67 § 3: row hover highlights the
/// element in the viewport; [`crate::procgen::inspect::set_hover`] sets it). Empty with nothing
/// hovered.
#[must_use]
pub fn inspect_hover_lines(world: &World) -> Vec<Line> {
    let Some((pos, _)) = (world.get_resource::<ProcInspect>()).and_then(|inspect| inspect.hovered_point())
    else {
        return Vec::new();
    };
    if !pos.iter().all(|v| v.is_finite()) {
        return Vec::new();
    }
    vec![(
        pos,
        [pos[0], pos[1] + INSPECT_HOVER_BEAM_M, pos[2]],
        INSPECT_HOVER_COLOR,
    )]
}

/// The biome debug overlay (spec 65 § 7): every `BiomeArea` outlined (its `Shape` or the transform as a
/// unit box, its closed `Spline`, or its map rectangle at the entity's height). Areas with
/// `preview_alone` draw in magenta; while any is set the generator resolves only those areas
/// (`regia_procgen::biome::ecs`). Empty while the overlay is off.
#[must_use]
pub fn biome_lines(world: &mut World) -> Vec<Line> {
    use regia_procgen::biome::ecs::{AreaShape, BiomeArea};
    if !world
        .get_resource::<ProcPreview>()
        .is_some_and(|p| p.biome_overlay)
    {
        return Vec::new();
    }
    let mut areas: Vec<(Entity, BiomeArea)> = world
        .query::<(Entity, &BiomeArea)>()
        .iter(world)
        .map(|(e, a)| (e, a.clone()))
        .collect();
    areas.sort_by_key(|a| a.0);
    areas.truncate(MAX_VOLUMES);
    let mut out = Vec::new();
    for (e, area) in areas {
        let color = if area.preview_alone {
            BIOME_ALONE_COLOR
        } else {
            BIOME_COLOR
        };
        let tf = transform_of(world, e);
        match &area.shape {
            AreaShape::FromShape => {
                let shape = world.get::<Shape>(e).cloned().unwrap_or_default();
                out.extend(shape_lines(&shape, &tf, color));
            }
            AreaShape::FromSpline => {
                if let Some(gizmo) = cached_spline_gizmo(world, e, &tf) {
                    out.extend(gizmo.curve.into_iter().map(|(a, b, _)| (a, b, color)));
                }
            }
            AreaShape::Map { rect, .. } => {
                let y = tf.translation.y;
                let [x0, z0, x1, z1] = *rect;
                let c = [[x0, y, z0], [x1, y, z0], [x1, y, z1], [x0, y, z1]];
                for (i, from) in c.iter().enumerate() {
                    out.push((*from, c[(i + 1) % 4], color));
                }
            }
        }
    }
    out
}

/// One editor frame of the viewport side: keeps the camera anchor current and draws the overlay and
/// the volume gizmos into the debug lines.
#[tracing::instrument(target = "regia_editor", skip_all, name = "procgen_view_tick")]
pub fn tick(world: &mut World) {
    if world.contains_resource::<ProcPreview>() {
        sync_view_anchor(world);
    }
    let mut lines = overlay_lines(world);
    lines.extend(volume_lines(world));
    lines.extend(biome_lines(world));
    lines.extend(super::biome_overlay::biome_view_lines(world));
    lines.extend(inspect_hover_lines(world));
    if lines.is_empty() {
        return;
    }
    if let Some(mut draw) = world.get_resource_mut::<regia_render::DebugDraw>() {
        for (a, b, c) in lines {
            draw.line(a, b, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rescue review item 6: a quiet tick reuses the tessellated gizmo; editing the spline or
    /// moving the transform rebuilds it.
    #[test]
    fn spline_gizmo_cache_rebuilds_only_on_change() {
        let mut world = World::new();
        world.insert_resource(crate::selection::Selection::default());
        let mut preview = ProcPreview::default();
        preview.show_volumes = true;
        world.insert_resource(preview);
        let e = world
            .spawn((
                Spline::through([Vec3::ZERO, Vec3::X * 8.0, Vec3::new(8.0, 0.0, 8.0)]),
                Transform::default(),
            ))
            .id();
        let first = volume_lines(&mut world).len();
        assert!(first > 0);
        let rebuilds = |w: &World| w.resource::<SplineGizmoCache>().rebuilds;
        assert_eq!(rebuilds(&world), 1);
        assert_eq!(volume_lines(&mut world).len(), first, "same lines");
        assert_eq!(rebuilds(&world), 1, "a quiet tick does not re-tessellate");
        // Edit the spline: the change tick moves, so the gizmo rebuilds.
        world.get_mut::<Spline>(e).expect("spline").points[0].position.y += 1.0;
        assert_eq!(volume_lines(&mut world).len(), first, "same shape count");
        assert_eq!(rebuilds(&world), 2, "an edit re-tessellates");
        // Move the transform: the cached transform no longer matches.
        world.get_mut::<Transform>(e).expect("transform").translation.x += 2.0;
        assert!(!volume_lines(&mut world).is_empty());
        assert_eq!(rebuilds(&world), 3, "a transform move re-tessellates");
    }

    /// A non-finite anchor radius must not push NaN segments into the debug buffer.
    #[test]
    fn circle_lines_drop_a_non_finite_radius() {
        let mut out = Vec::new();
        circle_lines(Vec3::ZERO, f32::NAN, [1.0, 0.0, 0.0, 1.0], &mut out);
        circle_lines(Vec3::ZERO, f32::INFINITY, [1.0, 0.0, 0.0, 1.0], &mut out);
        circle_lines(Vec3::ZERO, f32::NEG_INFINITY, [1.0, 0.0, 0.0, 1.0], &mut out);
        assert!(
            out.is_empty(),
            "non-finite radii must not enter the debug buffer, got {} segments",
            out.len()
        );
        circle_lines(Vec3::ZERO, 2.0, [1.0, 0.0, 0.0, 1.0], &mut out);
        assert_eq!(out.len(), CIRCLE_SEGMENTS);
    }

    /// An infinite sphere radius must not push non-finite segments. `f32::max` keeps `+∞`.
    #[test]
    fn shape_lines_drop_a_non_finite_sphere() {
        use regia_scene::shape::ShapeKind;

        let shape = Shape {
            kind: ShapeKind::Sphere {
                radius: f32::INFINITY,
            },
            ..Shape::default()
        };
        let lines = shape_lines(&shape, &Transform::default(), [1.0, 0.0, 0.0, 1.0]);
        assert!(
            lines
                .iter()
                .all(|(a, b, _)| { a.iter().chain(b).all(|c| c.is_finite()) }),
            "a non-finite sphere entered the debug buffer ({} segments)",
            lines.len()
        );
    }

    /// Biome spline outlines share the volume gizmo cache: `to_polyline` does not run again on a
    /// quiet editor tick.
    #[test]
    fn biome_spline_overlay_tessellates_once_until_the_spline_changes() {
        use regia_procgen::biome::ecs::{AreaShape, BiomeArea};

        let mut world = World::new();
        let mut preview = ProcPreview::default();
        preview.biome_overlay = true;
        preview.show_volumes = false;
        world.insert_resource(preview);
        let mut area = BiomeArea::new("meadow");
        area.shape = AreaShape::FromSpline;
        let entity = world
            .spawn((
                area,
                Spline::through([Vec3::ZERO, Vec3::X * 8.0, Vec3::new(8.0, 0.0, 8.0)]),
                Transform::default(),
            ))
            .id();
        let first = biome_lines(&mut world);
        assert!(!first.is_empty(), "a spline biome draws its curve");
        let rebuilds = |w: &World| w.resource::<SplineGizmoCache>().rebuilds;
        assert_eq!(rebuilds(&world), 1);
        assert_eq!(biome_lines(&mut world), first);
        assert_eq!(
            rebuilds(&world),
            1,
            "a quiet overlay tick reuses the tessellation"
        );
        world.get_mut::<Spline>(entity).expect("spline").points[0]
            .position
            .y += 1.0;
        assert!(!biome_lines(&mut world).is_empty());
        assert_eq!(rebuilds(&world), 2, "editing the spline re-tessellates");
    }

    /// A non-finite editor yaw must not push a new procgen anchor on every sync. `NaN != NaN`, so
    /// matching the previous slot by value never finds it and the list grows without bound.
    #[test]
    fn a_non_finite_camera_does_not_accumulate_procgen_anchors() {
        let mut world = World::new();
        let mut preview = ProcPreview::default();
        preview.generate_around_view = true;
        world.insert_resource(preview);
        let camera = crate::viewport::EditorCamera {
            yaw: f32::NAN,
            ..Default::default()
        };
        world.insert_resource(camera);
        sync_view_anchor(&mut world);
        sync_view_anchor(&mut world);
        let anchors = world
            .get_resource::<ProcGenExtraAnchors>()
            .map(|e| e.0.len())
            .unwrap_or(0);
        assert_eq!(anchors, 0, "a non-finite eye must not be stored, got {anchors}");
        world.resource_mut::<crate::viewport::EditorCamera>().yaw = 0.6;
        sync_view_anchor(&mut world);
        let list = &world.resource::<ProcGenExtraAnchors>().0;
        assert_eq!(list.len(), 1);
        assert!(list[0].pos.is_finite());
    }

    /// The editor slot is the entry sync inserted, even when a host anchor has the same pose.
    /// Value equality removes the host copy, which sorts first, and leaves the old editor entry.
    #[test]
    fn sync_keeps_a_host_anchor_that_matches_the_editor_camera() {
        let mut world = World::new();
        let mut preview = ProcPreview::default();
        preview.generate_around_view = true;
        world.insert_resource(preview);
        let camera = crate::viewport::EditorCamera::default();
        let host_same = AnchorView {
            pos: camera.eye(),
            view_dir: Some(camera.forward()),
            frustum: None,
            reach_scale: 1.0,
        };
        let host_other = AnchorView::at(Vec3::new(100.0, 0.0, 0.0));
        world.insert_resource(camera);
        world.insert_resource(ProcGenExtraAnchors(vec![host_same, host_other]));
        sync_view_anchor(&mut world);
        sync_view_anchor(&mut world);
        let extras = &world.resource::<ProcGenExtraAnchors>().0;
        assert_eq!(
            extras.len(),
            3,
            "two host anchors plus the editor slot, got {extras:?}"
        );
        assert_eq!(
            extras[0], host_same,
            "the matching host anchor was removed, got {extras:?}"
        );
        assert_eq!(extras[1], host_other);
    }

    /// The tiles overlay is empty when the tile plugin never inserted `ProcGenTiles`.
    /// `requested_cells` already answers empty in that world; the overlay must not abort the tick.
    #[test]
    fn tiles_overlay_without_the_tile_plugin_draws_nothing() {
        use regia_procgen::tiles::{AnchorSettings, TileSettings};
        use regia_procgen::world::ProcGenerator;

        let mut world = World::new();
        let mut preview = ProcPreview::default();
        preview.tiles_overlay = true;
        world.insert_resource(preview);
        world.spawn(ProcGenerator {
            tiles: Some(TileSettings {
                cell_m: 64.0,
                runtime: Some(AnchorSettings::default()),
                ..TileSettings::default()
            }),
            ..ProcGenerator::new("s.pgraph.ron")
        });
        let lines = overlay_lines(&mut world);
        assert!(
            lines.is_empty(),
            "no tile runtime means no overlay, got {} lines",
            lines.len()
        );
    }
}

/// How close a cursor ray must pass to a control point or tangent handle to grab it, metres.
pub const HANDLE_PICK_M: f32 = 0.45;

/// Which handle a drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleGrip {
    /// A control point.
    Point(usize),
    /// A Free or Aligned tangent handle.
    Tangent {
        /// The control point.
        point: usize,
        /// The incoming handle (else outgoing).
        incoming: bool,
    },
}

/// An in-progress handle drag. The spline is edited live; one undo entry is logged on release.
#[derive(Resource, Default)]
struct HandleDrag {
    entity: Option<Entity>,
    grip: Option<HandleGrip>,
    before: Option<Spline>,
}

fn to_local_point(tf: &Transform, p: Vec3) -> Vec3 {
    let d = tf.rotation.inverse() * (p - tf.translation);
    let inv = |a: f32, s: f32| if s.abs() > 1e-9 { a / s } else { 0.0 };
    Vec3::new(inv(d.x, tf.scale.x), inv(d.y, tf.scale.y), inv(d.z, tf.scale.z))
}

fn to_local_vec(tf: &Transform, v: Vec3) -> Vec3 {
    let d = tf.rotation.inverse() * v;
    let inv = |a: f32, s: f32| if s.abs() > 1e-9 { a / s } else { 0.0 };
    Vec3::new(inv(d.x, tf.scale.x), inv(d.y, tf.scale.y), inv(d.z, tf.scale.z))
}

fn from_array(p: [f32; 3]) -> Vec3 {
    Vec3::new(p[0], p[1], p[2])
}

/// Splines the viewport may grab: the selection, anything carrying `Generated`, and every spline
/// while volumes are shown.
fn draggable(world: &World) -> Vec<Entity> {
    let show_all = world
        .get_resource::<ProcPreview>()
        .is_some_and(|p| p.show_volumes);
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    let mut entities: Vec<Entity> = world
        .iter_entities()
        .filter(|entity| entity.get::<Spline>().is_some())
        .filter(|entity| {
            let id = entity.id();
            show_all || selected.contains(&id) || entity.get::<regia_scene::components::Generated>().is_some()
        })
        .map(|entity| entity.id())
        .collect();
    entities.sort();
    entities.truncate(MAX_VOLUMES);
    entities
}

/// The distance from `point` to the ray `origin → dir` (at or after the origin; `f32::MAX` for a
/// zero direction). Shared with the biome overlay's cursor tooltip.
pub(crate) fn ray_distance(origin: Vec3, dir: Vec3, point: Vec3) -> f32 {
    let Some(dir) = dir.try_normalize() else {
        return f32::MAX;
    };
    let w = point - origin;
    let t = w.dot(dir).max(0.0);
    (w - dir * t).length()
}

/// How close the cursor ray must pass to a filtered mark to tooltip it, metres.
pub const FILTER_TOOLTIP_M: f32 = 1.0;

/// The biome filter view's cursor tooltip (spec 65 § 7): the dim mark under the pointer names its
/// entry and the filter that removed it, next to the cursor (the agent query is
/// `editor.procgen_biome_overlay { at }`). View-only.
fn biome_filter_tooltip(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: egui::Vec2,
) {
    let views = world
        .get_resource::<ProcPreview>()
        .map(|p| p.biome_views)
        .unwrap_or_default();
    if !views.filtered {
        return;
    }
    let Some(pointer) = response.hover_pos() else {
        return;
    };
    let _span = tracing::debug_span!(target: "regia_editor", "biome_filter_tooltip").entered();
    let local = pointer - response.rect.left_top();
    let (origin, direction) = crate::viewport::pick_ray(
        camera,
        regia_core::math::Vec2::new(local.x, local.y),
        regia_core::math::Vec2::new(size.x, size.y),
    );
    let Some(text) = biome_tooltip_text(world, origin, direction) else {
        return;
    };
    ui.painter().text(
        pointer + egui::vec2(12.0, -12.0),
        egui::Align2::LEFT_BOTTOM,
        text,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(210, 218, 230),
    );
}

/// The filter view's tooltip cache (w11-biome-procgen-tooling item 2): the filtered marks are
/// gathered once per preview run (keyed by the run's `DebugOutputs`, which a new run or isolate
/// replaces) and the answer is looked up once per hover target (the cursor ray), instead of
/// gathering every frame the pointer rests on the viewport.
#[derive(Resource, Default)]
pub struct BiomeTooltipCache {
    /// The run the marks were gathered from (held, so the pointer identity cannot be reused).
    debug: Option<std::sync::Arc<regia_procgen::world::DebugOutputs>>,
    marks: Vec<super::biome_overlay::FilteredMark>,
    /// The hover target the answer is for: the ray's origin and direction bits.
    hover: Option<[u32; 6]>,
    text: Option<String>,
    /// Times the marks were gathered (tests: one per run, none per quiet frame).
    pub gathers: u64,
    /// Times a hover target was looked up against the marks (tests: one per hover change).
    pub lookups: u64,
}

impl BiomeTooltipCache {
    /// The tooltip for the ray `origin → direction` over the run `debug`: `gather` runs only when
    /// the run changed since the last call, the ray test only when the hover target changed.
    /// `None` without a run or without a mark within [`FILTER_TOOLTIP_M`] of the ray.
    pub fn lookup(
        &mut self,
        debug: Option<std::sync::Arc<regia_procgen::world::DebugOutputs>>,
        gather: impl FnOnce(&regia_procgen::world::DebugOutputs) -> Vec<super::biome_overlay::FilteredMark>,
        origin: Vec3,
        direction: Vec3,
    ) -> Option<String> {
        let Some(debug) = debug else {
            self.debug = None;
            self.marks.clear();
            self.hover = None;
            self.text = None;
            return None;
        };
        if !self
            .debug
            .as_ref()
            .is_some_and(|d| std::sync::Arc::ptr_eq(d, &debug))
        {
            self.marks = gather(&debug);
            self.debug = Some(debug);
            self.gathers += 1;
            self.hover = None;
        }
        let o = origin.to_array();
        let d = direction.to_array();
        let key = [o[0], o[1], o[2], d[0], d[1], d[2]].map(f32::to_bits);
        if self.hover != Some(key) {
            self.lookups += 1;
            self.text = super::biome_overlay::filtered_on_ray(&self.marks, o, d, FILTER_TOOLTIP_M)
                .map(super::biome_overlay::filter_tooltip_text);
            self.hover = Some(key);
        }
        self.text.clone()
    }
}

/// The filter view's tooltip text for a cursor ray, through [`BiomeTooltipCache`].
pub fn biome_tooltip_text(world: &mut World, origin: Vec3, direction: Vec3) -> Option<String> {
    let debug = super::debug_view::debug_outputs(world);
    let mut cache = world.remove_resource::<BiomeTooltipCache>().unwrap_or_default();
    let text = cache.lookup(
        debug,
        |debug| {
            let (_, order) = super::biome_overlay::preview_debug(world);
            super::biome_overlay::filtered_marks(debug, &order)
        },
        origin,
        direction,
    );
    world.insert_resource(cache);
    text
}

/// The nearest control point or tangent handle within [`HANDLE_PICK_M`] of the ray.
fn pick_handle(world: &World, origin: Vec3, dir: Vec3) -> Option<(Entity, HandleGrip)> {
    let mut best: Option<(Entity, HandleGrip, f32)> = None;
    for e in draggable(world) {
        let Some(spline) = world.get::<Spline>(e) else {
            continue;
        };
        let tf = transform_of(world, e);
        let gizmo = spline_gizmo(spline, &tf);
        for (i, p) in gizmo.points.iter().enumerate() {
            let d = ray_distance(origin, dir, from_array(*p));
            if d <= HANDLE_PICK_M && best.as_ref().is_none_or(|b| d < b.2) {
                best = Some((e, HandleGrip::Point(i), d));
            }
        }
        for h in &gizmo.tangents {
            let d = ray_distance(origin, dir, from_array(h.position));
            if d <= HANDLE_PICK_M && best.as_ref().is_none_or(|b| d < b.2) {
                best = Some((
                    e,
                    HandleGrip::Tangent {
                        point: h.point,
                        incoming: h.incoming,
                    },
                    d,
                ));
            }
        }
    }
    best.map(|(e, g, _)| (e, g))
}

fn grip_anchor(spline: &Spline, tf: &Transform, grip: HandleGrip) -> Option<Vec3> {
    let gizmo = spline_gizmo(spline, tf);
    match grip {
        HandleGrip::Point(i) => gizmo.points.get(i).copied().map(from_array),
        HandleGrip::Tangent { point, incoming } => gizmo
            .tangents
            .iter()
            .find(|h| h.point == point && h.incoming == incoming)
            .map(|h| from_array(h.position)),
    }
}

fn plane_hit(origin: Vec3, dir: Vec3, anchor: Vec3) -> Option<Vec3> {
    let dir = dir.try_normalize()?;
    let t = crate::handles::ray_plane(origin, dir, Vec3::new(0.0, anchor.y, 0.0), Vec3::Y)?;
    Some(origin + dir * t)
}

/// Moves one handle onto the horizontal plane through its current height. The inverse of
/// [`spline_gizmo`]: a control sits on the ray's plane hit, and a tangent handle is a third of
/// the Hermite tangent away from the point.
fn move_handle(
    spline: &mut Spline,
    tf: &Transform,
    grip: HandleGrip,
    hit: Vec3,
) -> Result<(), super::doc::DocError> {
    let world = world_spline(spline, tf);
    let miss = |m: String| super::doc::DocError::new("procgen_handle_miss", m);
    match grip {
        HandleGrip::Point(i) => {
            let p = spline
                .points
                .get_mut(i)
                .ok_or_else(|| miss(format!("no control point {i}")))?;
            p.position = to_local_point(tf, hit);
        }
        HandleGrip::Tangent { point, incoming } => {
            let point_world = world
                .points
                .get(point)
                .ok_or_else(|| miss(format!("no control point {point}")))?
                .position;
            let mode = spline
                .points
                .get(point)
                .ok_or_else(|| miss(format!("no control point {point}")))?
                .mode;
            let p = spline
                .points
                .get_mut(point)
                .ok_or_else(|| miss(format!("no control point {point}")))?;
            match (mode, incoming) {
                (TangentMode::Free, false) | (TangentMode::Aligned, false) => {
                    let out_w = (hit - point_world) * 3.0;
                    if mode == TangentMode::Aligned {
                        let len = p.tangent_in.length();
                        p.tangent_out = to_local_vec(tf, out_w);
                        let dir = p.tangent_out.normalize_or_zero();
                        p.tangent_in = dir * len;
                    } else {
                        p.tangent_out = to_local_vec(tf, out_w);
                    }
                }
                (TangentMode::Free, true) => {
                    p.tangent_in = to_local_vec(tf, (point_world - hit) * 3.0);
                }
                (TangentMode::Aligned, true) => {
                    let dir_w = world
                        .points
                        .get(point)
                        .map(|q| q.tangent_out.normalize_or_zero())
                        .unwrap_or(Vec3::ZERO);
                    let in_w = dir_w * (point_world - hit).length() * 3.0;
                    p.tangent_in = to_local_vec(tf, in_w);
                }
                _ => {
                    return Err(miss(
                        "tangent handles exist on Free and Aligned points".to_string(),
                    ));
                }
            }
        }
    }
    spline.validate().map_err(|err| miss(err.to_string()))?;
    Ok(())
}

fn commit_handle(world: &mut World, entity: Entity, before: Spline) {
    let Some(after) = world.get::<Spline>(entity).cloned() else {
        return;
    };
    if before == after {
        return;
    }
    world
        .get_resource_or_insert_with(regia_agent::SplineEditLog::default)
        .0
        .push(regia_agent::SplineEdit {
            entity,
            before: Some(before),
            after,
        });
}

/// Moves `grip` on `entity` to the plane hit of the ray and logs one spline undo entry.
///
/// # Errors
///
/// `procgen_handle_miss` when the entity has no spline, the index is out of range, the ray misses
/// the handle's plane, or the resulting spline is not finite.
pub fn apply_handle_drag(
    world: &mut World,
    entity: Entity,
    grip: HandleGrip,
    origin: Vec3,
    dir: Vec3,
) -> Result<(), super::doc::DocError> {
    let before = world.get::<Spline>(entity).cloned().ok_or_else(|| {
        super::doc::DocError::new(
            "procgen_handle_miss",
            format!("entity {} has no Spline", entity.to_bits()),
        )
    })?;
    let tf = transform_of(world, entity);
    let anchor = grip_anchor(&before, &tf, grip).ok_or_else(|| {
        super::doc::DocError::new("procgen_handle_miss", "that handle is not on the spline")
    })?;
    let hit = plane_hit(origin, dir, anchor).ok_or_else(|| {
        super::doc::DocError::new("procgen_handle_miss", "the ray misses the handle's plane")
    })?;
    let mut spline = before.clone();
    move_handle(&mut spline, &tf, grip, hit)?;
    world.entity_mut(entity).insert(spline);
    commit_handle(world, entity, before);
    Ok(())
}

/// One pointer sample for spline handles. Press grabs the nearest handle, move updates it, release
/// logs one undo entry. Returns whether the pointer was consumed.
pub fn handle_pointer(world: &mut World, phase: super::tools::PointerPhase, origin: Vec3, dir: Vec3) -> bool {
    match phase {
        super::tools::PointerPhase::Press => {
            let Some((entity, grip)) = pick_handle(world, origin, dir) else {
                if let Some(mut d) = world.get_resource_mut::<HandleDrag>() {
                    d.entity = None;
                    d.grip = None;
                    d.before = None;
                }
                return false;
            };
            let before = world.get::<Spline>(entity).cloned();
            let mut drag = world.get_resource_or_insert_with(HandleDrag::default);
            drag.entity = Some(entity);
            drag.grip = Some(grip);
            drag.before = before;
            true
        }
        super::tools::PointerPhase::Move => {
            let (entity, grip, before) = {
                let drag = world.get_resource::<HandleDrag>();
                match drag {
                    Some(d) => (d.entity, d.grip, d.before.clone()),
                    None => (None, None, None),
                }
            };
            let (Some(entity), Some(grip), Some(before)) = (entity, grip, before) else {
                return false;
            };
            let tf = transform_of(world, entity);
            let Some(anchor) = grip_anchor(&before, &tf, grip) else {
                return true;
            };
            let Some(hit) = plane_hit(origin, dir, anchor) else {
                return true;
            };
            let mut spline = before;
            if move_handle(&mut spline, &tf, grip, hit).is_ok() {
                world.entity_mut(entity).insert(spline);
            }
            true
        }
        super::tools::PointerPhase::Release => {
            let (entity, before) = {
                let mut drag = world.get_resource_or_insert_with(HandleDrag::default);
                let entity = drag.entity.take();
                drag.grip = None;
                (entity, drag.before.take())
            };
            if let (Some(entity), Some(before)) = (entity, before) {
                commit_handle(world, entity, before);
                return true;
            }
            false
        }
    }
}

/// Viewport input for spline control points and tangent handles. Returns `true` while a handle
/// drag owns the pointer.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: egui::Vec2,
) -> bool {
    let alt = ui.input(|i| i.modifiers.alt);
    if alt {
        return false;
    }
    // The biome filter view's cursor tooltip draws under the pointer whatever else the pane does.
    biome_filter_tooltip(ui, world, response, camera, size);
    let (pressed, down, released) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
        )
    });
    let dragging = world
        .get_resource::<HandleDrag>()
        .is_some_and(|d| d.entity.is_some());
    let hovered = response.hovered() || response.dragged();
    if !hovered && !dragging {
        return false;
    }
    let phase = if pressed {
        Some(super::tools::PointerPhase::Press)
    } else if released && dragging {
        Some(super::tools::PointerPhase::Release)
    } else if down && dragging {
        Some(super::tools::PointerPhase::Move)
    } else {
        None
    };
    let Some(phase) = phase else {
        return dragging;
    };
    let Some(pointer) = response.interact_pointer_pos().or(response.hover_pos()) else {
        return dragging;
    };
    let local = pointer - response.rect.left_top();
    let (origin, direction) = crate::viewport::pick_ray(
        camera,
        regia_core::math::Vec2::new(local.x, local.y),
        regia_core::math::Vec2::new(size.x, size.y),
    );
    let owned = handle_pointer(world, phase, origin, direction);
    owned || dragging
}

#[cfg(test)]
mod tooltip_tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::procgen::biome_overlay::FilteredMark;

    fn marks() -> Vec<FilteredMark> {
        vec![
            FilteredMark {
                position: [0.0, 1.0, 0.0],
                entry: "reed".to_string(),
                filter: "0:Altitude".to_string(),
            },
            FilteredMark {
                position: [10.0, 1.0, 0.0],
                entry: "pine".to_string(),
                filter: "1:Slope".to_string(),
            },
        ]
    }

    /// w11-biome-procgen-tooling item 2 (view.rs:923 gathered the marks every hovered frame): the
    /// cache gathers once per preview run and looks the ray up once per hover change; frames that
    /// rest on the same target do neither, and a new run gathers once more.
    #[test]
    fn the_filter_tooltip_gathers_once_per_run_and_looks_up_once_per_hover_change() {
        let gathered = Cell::new(0_u32);
        let gather = |_: &regia_procgen::world::DebugOutputs| {
            gathered.set(gathered.get() + 1);
            marks()
        };
        let mut cache = BiomeTooltipCache::default();
        let run = Arc::new(BTreeMap::new());
        let west = (Vec3::new(-5.0, 1.0, 0.0), Vec3::X);
        let above = (Vec3::new(10.0, 50.0, 0.0), Vec3::NEG_Y);
        // Ten frames resting on the first mark: one gather, one lookup.
        for _ in 0..10 {
            assert_eq!(
                cache.lookup(Some(run.clone()), gather, west.0, west.1).as_deref(),
                Some("reed · removed by 0:Altitude")
            );
        }
        assert_eq!((gathered.get(), cache.gathers, cache.lookups), (1, 1, 1));
        // A hover change: one lookup, no gather; resting there again costs nothing.
        for _ in 0..5 {
            assert_eq!(
                cache
                    .lookup(Some(run.clone()), gather, above.0, above.1)
                    .as_deref(),
                Some("pine · removed by 1:Slope")
            );
        }
        assert_eq!((gathered.get(), cache.gathers, cache.lookups), (1, 1, 2));
        // A ray that passes no mark answers none (and is cached too).
        let empty = (Vec3::new(5.0, 50.0, 0.0), Vec3::NEG_Y);
        assert_eq!(cache.lookup(Some(run.clone()), gather, empty.0, empty.1), None);
        assert_eq!(cache.lookup(Some(run.clone()), gather, empty.0, empty.1), None);
        assert_eq!((gathered.get(), cache.lookups), (1, 3));
        // A new run (a regenerate or an isolate) gathers once, and answers the same target again.
        let next = Arc::new(BTreeMap::new());
        for _ in 0..3 {
            assert!(
                cache
                    .lookup(Some(next.clone()), gather, empty.0, empty.1)
                    .is_none()
            );
        }
        assert_eq!((gathered.get(), cache.gathers, cache.lookups), (2, 2, 4));
        // No run: no tooltip, no gather.
        assert_eq!(cache.lookup(None, gather, west.0, west.1), None);
        assert_eq!(gathered.get(), 2);
    }
}
