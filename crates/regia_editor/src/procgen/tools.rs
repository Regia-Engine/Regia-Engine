// SPDX-License-Identifier: MPL-2.0
//! Placement tools (spec 67 § 5): viewport gestures that write the inputs a graph reads and keep a
//! generator bound to them. A session arms a kind and a preset graph, resolves its target at the
//! first gesture, edits it live (the generator re-runs), and ends with Apply (one undo entry) or
//! Cancel (everything it did is undone, the level is byte-identical).
//!
//! Built kinds: Paint (writes [`PaintedPoints`], each point with a random yaw drawn from the brush
//! seed), Box (a box `Shape` on the target, the generator bound with `ProcBounds::FromShape`; ADR-7215
//! retires the ADR-1675 (10) interim that wrote the generator's own box bounds), Path (an open
//! `Spline` on the target) and Area (a closed `Spline` plus a `Shape` hull of the outline, spec 64;
//! on a biome preset a `BiomeArea` instead of a generator, spec 65). Path and Area points are
//! projected down with the tool's hit filter, stored in the target's local space, and `in.self`
//! hands the spline (and, for a closed one, its polygon) to the preset graph. The generator's box
//! bounds grow to cover the points plus a margin.

use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_procgen::biome::ecs::{AreaShape, BiomeArea};
use regia_procgen::painted::{MAX_PAINT_LAYERS, PaintedPoint, PaintedPoints};
use regia_procgen::world::{ProcBounds, ProcGenerator};
use regia_scene::components::{EntityGuid, Generated, Name};
use regia_scene::prelude::Transform;
use regia_scene::shape::{SHAPE_HULL_MAX_POINTS, Shape, ShapeKind};
use regia_scene::spline::{SPLINE_MAX_POINTS, Spline, SplinePoint};

use super::doc::DocError;
use crate::command::{EditorCommand, UndoStack};

/// The tool kinds (spec 62 `flags.tool` names them in lowercase).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolKind {
    /// Draw an open spline.
    Path,
    /// Draw a closed outline.
    Area,
    /// Paint points.
    Paint,
    /// Drag a box.
    Box,
}

impl ToolKind {
    /// The name in `flags.tool` and the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Area => "area",
            Self::Paint => "paint",
            Self::Box => "box",
        }
    }

    /// Parses a name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "path" => Some(Self::Path),
            "area" => Some(Self::Area),
            "paint" => Some(Self::Paint),
            "box" => Some(Self::Box),
            _ => None,
        }
    }
}

/// What the cursor ray may hit (spec 67 § 5.3; saved per tool in editor preferences).
#[derive(Clone, Debug, PartialEq)]
pub struct HitFilter {
    /// Terrain surfaces.
    pub terrain: bool,
    /// Mesh surfaces.
    pub meshes: bool,
    /// Ignore entities carrying `Generated` (a tool never builds on its own output).
    pub skip_generated: bool,
    /// Only this entity (and its parts) may be hit.
    pub only_entity: Option<Entity>,
    /// Render layers a mesh must be on (bit per `MeshRenderer::layer`; all by default). The
    /// editor's pick path is mesh and terrain based, so the mask filters by render layer.
    pub physics_mask: u32,
}

impl Default for HitFilter {
    fn default() -> Self {
        Self {
            terrain: true,
            meshes: true,
            skip_generated: true,
            only_entity: None,
            physics_mask: u32::MAX,
        }
    }
}

/// A surface hit a gesture lands on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceHit {
    /// World position.
    pub position: Vec3,
    /// World unit normal.
    pub normal: Vec3,
    /// The entity hit.
    pub entity: Entity,
}

/// Whether `e` or an ancestor carries `Generated`.
fn is_generated(world: &World, e: Entity) -> bool {
    let mut cur = Some(e);
    let mut depth = 0;
    while let Some(x) = cur {
        if world.get::<Generated>(x).is_some() {
            return true;
        }
        depth += 1;
        if depth > 64 {
            break;
        }
        cur = world.get::<ChildOf>(x).map(ChildOf::parent);
    }
    false
}

/// The nearest surface along a ray under `filter` (terrain through the terrain query, meshes
/// through the editor's pick path).
#[must_use]
pub fn cast(world: &World, origin: Vec3, direction: Vec3, filter: &HitFilter) -> Option<SurfaceHit> {
    let _span = tracing::info_span!(target: "regia_editor", "procgen_tool_cast").entered();
    let dir = direction.try_normalize()?;
    let max = 100_000.0;
    let terrain = filter
        .terrain
        .then(|| regia_terrain::pick_terrain(world, origin, dir, max))
        .flatten()
        .filter(|h| filter.only_entity.is_none_or(|o| o == h.entity))
        .map(|h| {
            let n = world
                .get::<regia_terrain::Terrain>(h.entity)
                .map_or([0.0, 1.0, 0.0], |t| t.map.normal_at(h.position.x, h.position.z));
            (
                h.distance,
                SurfaceHit {
                    position: h.position,
                    normal: Vec3::from(n),
                    entity: h.entity,
                },
            )
        });
    let mesh = filter
        .meshes
        .then(|| {
            crate::picking::raycast_world(world, origin, dir, crate::picking::FaceCull::None, &|e| {
                let owner = crate::mesh_source::pick_owner(world, e);
                (!filter.skip_generated || !(is_generated(world, e) || is_generated(world, owner)))
                    && filter.only_entity.is_none_or(|o| o == e || o == owner)
                    && world
                        .get::<regia_render::MeshRenderer>(e)
                        .is_none_or(|m| m.layer > 31 || filter.physics_mask & (1 << m.layer) != 0)
            })
        })
        .flatten()
        .map(|h| {
            (
                h.distance,
                SurfaceHit {
                    position: h.position,
                    normal: h.normal,
                    entity: h.owner,
                },
            )
        });
    match (terrain, mesh) {
        (Some(t), Some(m)) => Some(if m.0 < t.0 { m.1 } else { t.1 }),
        (t, m) => t.or(m).map(|x| x.1),
    }
}

/// Projects a point straight down onto the surface under `filter` (from 10 km above it).
#[must_use]
pub fn project_down(world: &World, p: Vec3, filter: &HitFilter) -> Option<SurfaceHit> {
    cast(world, Vec3::new(p.x, p.y + 10_000.0, p.z), Vec3::NEG_Y, filter)
}

/// The components a session edits on its target (restored on cancel, swapped by undo).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ComponentSnapshot {
    /// `Transform`.
    pub transform: Option<Transform>,
    /// `ProcGenerator`.
    pub generator: Option<ProcGenerator>,
    /// `PaintedPoints`.
    pub painted: Option<PaintedPoints>,
    /// `Spline` (Path and Area).
    pub spline: Option<Spline>,
    /// `Shape` (Area's hull, Box's box).
    pub shape: Option<Shape>,
    /// `BiomeArea` (Area on a biome preset).
    pub biome_area: Option<BiomeArea>,
}

impl ComponentSnapshot {
    fn capture(world: &World, e: Entity) -> Self {
        Self {
            transform: world.get::<Transform>(e).copied(),
            generator: world.get::<ProcGenerator>(e).cloned(),
            painted: world.get::<PaintedPoints>(e).cloned(),
            spline: world.get::<Spline>(e).cloned(),
            shape: world.get::<Shape>(e).cloned(),
            biome_area: world.get::<BiomeArea>(e).cloned(),
        }
    }

    fn restore(&self, world: &mut World, e: Entity) {
        let Ok(mut ent) = world.get_entity_mut(e) else {
            return;
        };
        match self.transform {
            Some(t) => {
                ent.insert(t);
            }
            None => {
                ent.remove::<Transform>();
            }
        }
        match &self.generator {
            Some(g) => {
                ent.insert(g.clone());
            }
            None => {
                ent.remove::<ProcGenerator>();
            }
        }
        match &self.painted {
            Some(p) => {
                ent.insert(p.clone());
            }
            None => {
                ent.remove::<PaintedPoints>();
            }
        }
        match &self.spline {
            Some(p) => {
                ent.insert(p.clone());
            }
            None => {
                ent.remove::<Spline>();
            }
        }
        match &self.shape {
            Some(p) => {
                ent.insert(p.clone());
            }
            None => {
                ent.remove::<Shape>();
            }
        }
        match &self.biome_area {
            Some(p) => {
                ent.insert(p.clone());
            }
            None => {
                ent.remove::<BiomeArea>();
            }
        }
    }
}

/// An open placement session (spec 67 § 5.1).
#[derive(Clone, Debug)]
pub struct ToolSession {
    /// The kind.
    pub kind: ToolKind,
    /// The preset graph path.
    pub preset: String,
    /// The entity being edited.
    pub target: Entity,
    /// The session spawned `target`.
    pub created: bool,
    /// What the target held before the session (restored on cancel). One snapshot: a session
    /// edits one entity.
    pub undo_snapshot: Vec<ComponentSnapshot>,
    /// The hit filter in use.
    pub hit: HitFilter,
    /// Area on a biome preset: the biome the `BiomeArea` names (`None` for other sessions).
    pub biome: Option<String>,
    /// The target's guid and name (undo identity).
    guid: Guid,
    name: String,
}

/// The open session (world resource; at most one).
#[derive(Resource, Default)]
pub struct ProcToolSession(
    /// The session, when one is open.
    pub Option<ToolSession>,
);

/// One applied session on the undo stack: the target before and after; a created target is
/// despawned by undo and spawned again by redo.
pub struct ProcToolCommand {
    guid: Guid,
    name: String,
    preset: String,
    created: bool,
    before: ComponentSnapshot,
    after: ComponentSnapshot,
    label: String,
}

fn find_guid(world: &mut World, guid: Guid) -> Option<Entity> {
    world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
}

fn despawn_target(world: &mut World, e: Entity) {
    regia_procgen::world::clear(world, e);
    if let Ok(ent) = world.get_entity_mut(e) {
        ent.despawn();
    }
}

impl EditorCommand for ProcToolCommand {
    fn apply(&mut self, world: &mut World) {
        let e = match find_guid(world, self.guid) {
            Some(e) => e,
            None => world
                .spawn((EntityGuid(self.guid), Name::new(self.name.clone())))
                .id(),
        };
        self.after.restore(world, e);
        regia_procgen::world::request_run(world, e);
        rerun_preset_generators(world, &self.preset);
    }

    fn revert(&mut self, world: &mut World) {
        let Some(e) = find_guid(world, self.guid) else {
            return;
        };
        if self.created {
            despawn_target(world, e);
        } else {
            self.before.restore(world, e);
            regia_procgen::world::request_run(world, e);
        }
        rerun_preset_generators(world, &self.preset);
    }

    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        self.label.clone()
    }
}

/// A Paint brush (spec 67 § 5.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Brush {
    /// Radius, meters.
    pub radius_m: f32,
    /// Spacing of stamped points, meters.
    pub spacing_m: f32,
    /// Remove points under the brush instead of adding.
    pub erase: bool,
    /// Zero-based layer (digits 1 to 8 choose 0 to 7).
    pub layer: u8,
    /// Fraction of stamp candidates kept, [0, 1].
    pub density: f32,
    /// Give each stamped point a random yaw about its normal (default on).
    pub random_yaw: bool,
    /// Seed of the random yaw. A point's yaw depends only on the seed and where it lands, so the
    /// same stroke with the same seed stamps the same yaws.
    pub seed: u32,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            radius_m: 2.0,
            spacing_m: 1.0,
            erase: false,
            layer: 0,
            density: 1.0,
            random_yaw: true,
            seed: 0,
        }
    }
}

/// The random yaw of a point stamped at `p` by a brush with `seed`, radians in `[0, 2π)`: a hash of
/// the seed and the millimeter cell of the hit (x, z), so it does not depend on stroke order.
#[must_use]
pub fn stamp_yaw(seed: u32, p: Vec3) -> f32 {
    use regia_core::math::hash::pcg_hash;
    let cell = |v: f32| (f64::from(v) * 1000.0).round() as i64 as u32;
    let key = pcg_hash(seed ^ pcg_hash(cell(p.x)) ^ pcg_hash(cell(p.z)).rotate_left(16));
    let yaw = (f64::from(key) / 4_294_967_296.0 * std::f64::consts::TAU) as f32;
    // The f32 rounding of a key near the top can land on 2π itself.
    if yaw >= std::f32::consts::TAU { 0.0 } else { yaw }
}

/// What one tool call asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolGesture {
    /// Points (Path/Area: control points; Paint: stroke centers), projected with the hit filter.
    pub points: Vec<Vec3>,
    /// Path/Area: closed outline.
    pub closed: Option<bool>,
    /// Paint: the brush.
    pub brush: Option<Brush>,
    /// Box: `(min, max)`.
    pub box_extents: Option<(Vec3, Vec3)>,
    /// Path/Area: move control point `index` to the projected position (applied before `points`).
    pub move_point: Option<(usize, Vec3)>,
    /// Path/Area: remove control point `index` (applied after `move_point`, before `points`).
    pub remove_point: Option<usize>,
    /// Path/Area: drop every control point first.
    pub clear: bool,
    /// Area on a biome preset: the `.biome.ron` the `BiomeArea` names (default: the preset's
    /// `biome` parameter).
    pub biome: Option<String>,
}

/// What a tool call did.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcome {
    /// The target.
    pub entity: Entity,
    /// The session spawned it.
    pub created: bool,
    /// Painted points on the target after the call.
    pub painted: usize,
    /// Points added and removed by this call.
    pub added: usize,
    /// Points removed by this call.
    pub removed: usize,
    /// The session was applied (closed with one undo entry).
    pub applied: bool,
    /// Path/Area: control points on the target's spline after the call.
    pub spline_points: usize,
}

/// Whether the preset `path` serves `kind` (`flags.tool`).
fn preset_fits(world: &World, path: &str, kind: ToolKind) -> Result<(), DocError> {
    let text = super::doc::graph_text(world, path)
        .ok_or_else(|| DocError::new("procgen_unknown_graph", format!("cannot read preset `{path}`")))?;
    let g = regia_procgen::ProcGraph::from_ron(&text)
        .map_err(|e| DocError::new("procgen_graph_invalid", format!("{path}: {e}")))?;
    if g.flags.tool.as_deref() != Some(kind.name()) {
        return Err(DocError::new(
            "procgen_tool_preset_mismatch",
            format!(
                "`{path}` is not a {} tool graph (flags.tool is {:?})",
                kind.name(),
                g.flags.tool
            ),
        ));
    }
    Ok(())
}

/// Whether the preset is a biome graph (it reads `BiomeArea`s through a `biome.areas` node), and
/// the default biome its `biome` parameter names (`""` when it has none).
fn biome_preset(world: &World, path: &str) -> Option<String> {
    let text = super::doc::graph_text(world, path)?;
    let g = regia_procgen::ProcGraph::from_ron(&text).ok()?;
    if !g.nodes.values().any(|n| n.ty == "biome.areas") {
        return None;
    }
    let biome = g
        .params
        .iter()
        .find(|p| p.name == "biome")
        .and_then(|p| match &p.default {
            regia_procgen::SettingValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Some(biome)
}

/// Requests a run of every generator bound to `preset` (the biome generators an Area session on a
/// biome preset feeds).
fn rerun_preset_generators(world: &mut World, preset: &str) {
    let bound: Vec<Entity> = world
        .query::<(Entity, &ProcGenerator)>()
        .iter(world)
        .filter(|(_, g)| g.graph == preset)
        .map(|(e, _)| e)
        .collect();
    for e in bound {
        regia_procgen::world::request_run(world, e);
    }
}

/// Why `e` does not fit `kind`, or `None` when it does.
fn unfit_reason(world: &World, e: Entity, kind: ToolKind) -> Option<String> {
    match kind {
        ToolKind::Paint => world
            .get::<PaintedPoints>(e)
            .is_none()
            .then(|| "the selected entity has no PaintedPoints".to_string()),
        ToolKind::Box => {
            let boxed = world
                .get::<Shape>(e)
                .is_some_and(|s| matches!(s.kind, ShapeKind::Box { .. }));
            // A generator written by the ADR-1675 interim (its own box bounds, no `Shape`) still
            // fits; the session gives it a box `Shape`.
            let legacy = world.get::<Shape>(e).is_none()
                && matches!(
                    world.get::<ProcGenerator>(e).map(|g| g.bounds),
                    Some(ProcBounds::Box { .. })
                );
            (!(boxed || legacy)).then(|| "the selected entity has no box Shape".to_string())
        }
        ToolKind::Path | ToolKind::Area => world
            .get::<Spline>(e)
            .is_none()
            .then(|| "the selected entity has no Spline".to_string()),
    }
}

/// `<preset stem>_NN`, the first free number.
fn next_name(world: &mut World, preset: &str) -> String {
    let file = preset.rsplit(['/', '\\']).next().unwrap_or(preset);
    let stem = file
        .strip_suffix(".pgraph.ron")
        .or_else(|| file.strip_suffix(".pgvar.ron"))
        .unwrap_or(file);
    let taken: Vec<String> = world
        .query::<&Name>()
        .iter(world)
        .map(|n| n.as_str().to_string())
        .collect();
    (1..)
        .map(|k| format!("{stem}_{k:02}"))
        .find(|n| !taken.contains(n))
        .unwrap_or_else(|| format!("{stem}_01"))
}

/// Opens a session (or continues the open one), applies `gesture`, re-runs the generator, and
/// with `apply` commits everything as one undo entry.
///
/// # Errors
///
/// `procgen_tool_preset_mismatch`,
/// `procgen_tool_target_unfit` (with "edit selected only" on), `procgen_tool_no_hit`,
/// `procgen_tool_busy` (a session of another kind or preset is open), `procgen_tool_bad_gesture`.
pub fn run_tool(
    world: &mut World,
    kind: ToolKind,
    preset: &str,
    target: Option<Entity>,
    gesture: &ToolGesture,
    apply: bool,
) -> Result<ToolOutcome, DocError> {
    let _span = tracing::info_span!(target: "regia_editor", "procgen_tool").entered();
    let prefs = world
        .get_resource::<super::debug_view::ProcEditorPrefs>()
        .cloned()
        .unwrap_or_default();
    let hit = prefs.hit_filters.get(kind.name()).cloned().unwrap_or_default();
    let open = world.get_resource::<ProcToolSession>().and_then(|s| s.0.clone());
    let session = match open {
        Some(s) => {
            if s.kind != kind || s.preset != preset {
                return Err(DocError::new(
                    "procgen_tool_busy",
                    format!(
                        "a {} session with `{}` is open: apply or cancel it first",
                        s.kind.name(),
                        s.preset
                    ),
                ));
            }
            s
        }
        None => {
            if super::doc::graph_text(world, preset).is_none()
                && let Some(text) = super::presets::preset(preset)
            {
                super::doc::write_source(world, preset, text)?;
            }
            preset_fits(world, preset, kind)?;
            // Area on a biome preset writes a `BiomeArea` (spec 67 § 5.2); it needs a biome.
            let biome = match (kind, biome_preset(world, preset)) {
                (ToolKind::Area, Some(default)) => {
                    let name = gesture.biome.clone().unwrap_or(default);
                    if name.is_empty() {
                        return Err(DocError::new(
                            "procgen_tool_bad_gesture",
                            format!(
                                "`{preset}` is a biome preset with no `biome` parameter: name the .biome.ron (`biome`)"
                            ),
                        ));
                    }
                    Some(name)
                }
                _ => None,
            };
            let selected = target.or_else(|| {
                world
                    .get_resource::<crate::selection::Selection>()
                    .and_then(crate::selection::Selection::primary)
            });
            let fit =
                selected.filter(|e| world.get_entity(*e).is_ok() && unfit_reason(world, *e, kind).is_none());
            if fit.is_none()
                && prefs.edit_selected_only
                && let Some(e) = selected
            {
                let why = unfit_reason(world, e, kind).unwrap_or_else(|| "the selection is gone".into());
                return Err(DocError::new("procgen_tool_target_unfit", why));
            }
            let (entity, created) = match fit {
                Some(e) => (e, false),
                None => {
                    let name = next_name(world, preset);
                    let e = world
                        .spawn((
                            EntityGuid(Guid::new_random()),
                            Name::new(name),
                            Transform::IDENTITY,
                        ))
                        .id();
                    (e, true)
                }
            };
            let before = if created {
                ComponentSnapshot::default()
            } else {
                ComponentSnapshot::capture(world, entity)
            };
            let guid = world
                .get::<EntityGuid>(entity)
                .map_or_else(Guid::new_random, |g| g.0);
            if world.get::<EntityGuid>(entity).is_none() {
                world.entity_mut(entity).insert(EntityGuid(guid));
            }
            let existing = world.get::<Name>(entity).map(|n| n.as_str().to_string());
            let name = match existing {
                Some(n) => n,
                None => next_name(world, preset),
            };
            if let Some(name) = &biome {
                // A biome area feeds every biome generator; it carries no generator of its own.
                let mut area = world.get::<BiomeArea>(entity).cloned().unwrap_or_default();
                area.biome = Some(name.clone());
                area.shape = AreaShape::FromSpline;
                world.entity_mut(entity).insert(area);
            } else {
                // Bind the generator to the preset.
                let mut generator = world
                    .get::<ProcGenerator>(entity)
                    .cloned()
                    .unwrap_or_else(|| ProcGenerator::new(preset));
                generator.graph = preset.to_string();
                world.entity_mut(entity).insert(generator);
            }
            if kind == ToolKind::Paint && world.get::<PaintedPoints>(entity).is_none() {
                world.entity_mut(entity).insert(PaintedPoints::default());
            }
            if matches!(kind, ToolKind::Path | ToolKind::Area) && world.get::<Spline>(entity).is_none() {
                world.entity_mut(entity).insert(Spline::default());
            }
            let s = ToolSession {
                kind,
                preset: preset.to_string(),
                target: entity,
                created,
                undo_snapshot: vec![before],
                hit,
                biome,
                guid,
                name,
            };
            world.get_resource_or_insert_with(ProcToolSession::default).0 = Some(s.clone());
            s
        }
    };
    let e = session.target;
    let (mut added, mut removed) = (0, 0);
    match kind {
        ToolKind::Paint => {
            if let Some(brush) = gesture
                .brush
                .or_else(|| (!gesture.points.is_empty()).then(Brush::default))
            {
                let (a, r) = paint(world, e, &session.hit, &gesture.points, &brush)?;
                added = a;
                removed = r;
            }
        }
        ToolKind::Box => {
            if let Some((lo, hi)) = gesture.box_extents {
                let (lo, hi) = (lo.min(hi), lo.max(hi));
                let half = (hi - lo) * 0.5;
                if half.min_element() <= 0.0 {
                    return Err(DocError::new(
                        "procgen_tool_bad_gesture",
                        "a box needs a positive size on every axis",
                    ));
                }
                let mut t = world.get::<Transform>(e).copied().unwrap_or(Transform::IDENTITY);
                t.translation = (lo + hi) * 0.5;
                t.scale = Vec3::ONE;
                world.entity_mut(e).insert(t);
                // A box `Shape` (spec 67 § 5.2); the generator reads it (ADR-7215 retires the
                // ADR-1675 (10) interim of the generator's own box bounds). An existing shape keeps
                // its edge band.
                let mut shape = world.get::<Shape>(e).cloned().unwrap_or_default();
                shape.kind = ShapeKind::Box { half_extents: half };
                world.entity_mut(e).insert(shape);
                if let Some(mut g) = world.get_mut::<ProcGenerator>(e) {
                    g.bounds = ProcBounds::FromShape;
                }
            }
        }
        ToolKind::Path | ToolKind::Area => {
            edit_spline(world, e, kind, session.created, &session.hit, gesture)?;
            if apply {
                let n = world.get::<Spline>(e).map_or(0, |s| s.points.len());
                let need = if kind == ToolKind::Area { 3 } else { 2 };
                if n < need {
                    return Err(DocError::new(
                        "procgen_tool_bad_gesture",
                        format!(
                            "the {} tool needs at least {need} points to apply, the session has {n}",
                            kind.name()
                        ),
                    ));
                }
            }
        }
    }
    regia_procgen::world::request_run(world, e);
    if session.biome.is_some() {
        rerun_preset_generators(world, &session.preset);
    }
    if let Some(mut p) = world.get_resource_mut::<super::ProcPreview>() {
        p.tool_ticks = 0;
    }
    let painted = world.get::<PaintedPoints>(e).map_or(0, |p| p.points.len());
    let spline_points = world.get::<Spline>(e).map_or(0, |s| s.points.len());
    if apply {
        finish(world, true)?;
    }
    Ok(ToolOutcome {
        entity: e,
        created: session.created,
        painted,
        added,
        removed,
        applied: apply,
        spline_points,
    })
}

/// Margin around a path's points the generator's box covers, meters (road verges).
pub const PATH_MARGIN_M: f32 = 8.0;
/// Margin around an area's outline the generator's box covers, meters.
pub const AREA_MARGIN_M: f32 = 4.0;

/// World position to the entity's local space (scale, rotation, translation inverted; a zero scale
/// axis maps to 0).
fn to_local(tf: &Transform, p: Vec3) -> Vec3 {
    let d = tf.rotation.inverse() * (p - tf.translation);
    let inv = |a: f32, s: f32| if s.abs() > 1e-9 { a / s } else { 0.0 };
    Vec3::new(inv(d.x, tf.scale.x), inv(d.y, tf.scale.y), inv(d.z, tf.scale.z))
}

/// Applies a Path or Area gesture to the target's spline (clear, move, remove, append), then keeps
/// the closed flag, the Area hull shape and the generator's box bounds in step with the points.
fn edit_spline(
    world: &mut World,
    e: Entity,
    kind: ToolKind,
    created: bool,
    hit: &HitFilter,
    gesture: &ToolGesture,
) -> Result<(), DocError> {
    let _span = tracing::info_span!(target: "regia_editor", "procgen_tool_spline").entered();
    let bad = |m: String| DocError::new("procgen_tool_bad_gesture", m);
    let mut spline = world.get::<Spline>(e).cloned().unwrap_or_default();
    // Project every world point first so a failing gesture changes nothing.
    let project = |world: &World, p: Vec3, what: &str| -> Result<Vec3, DocError> {
        if !p.is_finite() {
            return Err(bad(format!("{what} holds a non-finite number")));
        }
        project_down(world, p, hit).map(|h| h.position).ok_or_else(|| {
            DocError::new(
                "procgen_tool_no_hit",
                format!("{what} ({p}) hits nothing under the tool's filter"),
            )
        })
    };
    let moved = gesture
        .move_point
        .map(|(i, p)| project(world, p, "the moved point").map(|w| (i, w)))
        .transpose()?;
    let added = gesture
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| project(world, *p, &format!("point {i}")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut tf = world.get::<Transform>(e).copied().unwrap_or(Transform::IDENTITY);
    if gesture.clear {
        spline.points.clear();
    }
    if spline.points.is_empty() && !added.is_empty() {
        // A spline that starts here is anchored at the first batch's center, so its local numbers
        // stay small and the box bounds sit on the outline.
        let (lo, hi) = added.iter().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(lo, hi), p| (lo.min(*p), hi.max(*p)),
        );
        if created {
            tf.translation = (lo + hi) * 0.5;
            world.entity_mut(e).insert(tf);
        }
    }
    if let Some((i, w)) = moved {
        let local = to_local(&tf, w);
        let n = spline.points.len();
        let p = spline
            .points
            .get_mut(i)
            .ok_or_else(|| bad(format!("move_point index {i} is not below {n}")))?;
        p.position = local;
    }
    if let Some(i) = gesture.remove_point {
        if i >= spline.points.len() {
            return Err(bad(format!(
                "remove_point index {i} is not below {}",
                spline.points.len()
            )));
        }
        spline.points.remove(i);
    }
    if spline.points.len() + added.len() > SPLINE_MAX_POINTS {
        return Err(bad(format!("a spline holds at most {SPLINE_MAX_POINTS} points")));
    }
    spline
        .points
        .extend(added.iter().map(|w| SplinePoint::at(to_local(&tf, *w))));
    spline.closed = match (kind, gesture.closed) {
        (ToolKind::Area, _) => true,
        (_, Some(c)) => c,
        (_, None) => spline.closed,
    };
    spline.validate().map_err(|err| bad(err.to_string()))?;
    // Bounds: a box centered on the entity, covering the points plus the margin.
    let margin = if kind == ToolKind::Area {
        AREA_MARGIN_M
    } else {
        PATH_MARGIN_M
    };
    let reach = spline
        .points
        .iter()
        .fold(Vec3::ZERO, |m, p| m.max(p.position.abs()));
    let half = (reach + Vec3::splat(margin) / tf.scale.abs().max(Vec3::splat(1e-3))).max(Vec3::ONE);
    if let Some(mut g) = world.get_mut::<ProcGenerator>(e) {
        g.bounds = ProcBounds::Box { half_extents: half };
    }
    let biome_area = world.get::<BiomeArea>(e).is_some() && world.get::<ProcGenerator>(e).is_none();
    if kind == ToolKind::Area && !biome_area {
        // The hull of the outline (Y flattened) as the region shape `in.self` also carries.
        let pts: Vec<Vec3> = spline
            .points
            .iter()
            .take(SHAPE_HULL_MAX_POINTS)
            .map(|p| Vec3::new(p.position.x, 0.0, p.position.z))
            .collect();
        if pts.is_empty() {
            world.entity_mut(e).remove::<Shape>();
        } else {
            world
                .entity_mut(e)
                .insert(Shape::new(ShapeKind::Hull { points: pts }));
        }
    }
    world.entity_mut(e).insert(spline);
    Ok(())
}

/// Most brush steps from the center to the edge (`radius_m / spacing_m`): 255 visits 511 x 511 cells
/// per stroke point.
pub const MAX_BRUSH_STEPS: u32 = 255;
/// Most stroke points one paint gesture takes.
pub const MAX_STROKE_POINTS: usize = 4096;
/// Most brush cells (ray casts) one paint gesture visits over all its stroke points.
pub const MAX_BRUSH_CELLS: u64 = 1 << 22;
/// Stamps or erases points around each stroke center; returns `(added, removed)`.
fn paint(
    world: &mut World,
    e: Entity,
    hit: &HitFilter,
    strokes: &[Vec3],
    brush: &Brush,
) -> Result<(usize, usize), DocError> {
    if usize::from(brush.layer) >= MAX_PAINT_LAYERS {
        return Err(DocError::new(
            "procgen_tool_bad_gesture",
            format!("layer {} is not below {MAX_PAINT_LAYERS}", brush.layer),
        ));
    }
    if !(brush.radius_m > 0.0 && brush.spacing_m > 0.0)
        || !brush.radius_m.is_finite()
        || !brush.spacing_m.is_finite()
        || !brush.density.is_finite()
    {
        return Err(DocError::new(
            "procgen_tool_bad_gesture",
            "brush radius and spacing are positive and finite, density finite",
        ));
    }
    // Each stroke point visits (2 * steps + 1)^2 cells, each a ray cast and a crowding scan: bound it
    // (zai audit M4: `radius_m: 1e300` or `spacing_m: 1e-9` froze the editor).
    let steps = (f64::from(brush.radius_m) / f64::from(brush.spacing_m)).floor();
    let cells = (2.0 * steps + 1.0).powi(2) * strokes.len() as f64;
    if steps > f64::from(MAX_BRUSH_STEPS) || cells > MAX_BRUSH_CELLS as f64 {
        return Err(DocError::new(
            "procgen_tool_bad_gesture",
            format!(
                "radius_m / spacing_m is {steps} over {} stroke points; at most {MAX_BRUSH_STEPS} steps and \
                 {MAX_BRUSH_CELLS} cells in all (raise spacing_m, lower radius_m or send fewer points)",
                strokes.len()
            ),
        ));
    }
    if strokes.len() > MAX_STROKE_POINTS {
        return Err(DocError::new(
            "procgen_tool_bad_gesture",
            format!("{} stroke points; at most {MAX_STROKE_POINTS}", strokes.len()),
        ));
    }
    let mut points = world.get::<PaintedPoints>(e).cloned().unwrap_or_default();
    let (mut added, mut removed) = (0, 0);
    for (si, center) in strokes.iter().enumerate() {
        let Some(c) = project_down(world, *center, hit) else {
            return Err(DocError::new(
                "procgen_tool_no_hit",
                format!("stroke point {si} ({center}) hits nothing under the tool's filter"),
            ));
        };
        let r2 = brush.radius_m * brush.radius_m;
        if brush.erase {
            let before = points.points.len();
            points.points.retain(|p| {
                let d = p.position - c.position;
                d.x * d.x + d.z * d.z > r2
            });
            removed += before - points.points.len();
            continue;
        }
        // A square grid of `spacing` over the disc, centered on the hit.
        let steps = (brush.radius_m / brush.spacing_m).floor() as i32;
        for j in -steps..=steps {
            for i in -steps..=steps {
                let (dx, dz) = (i as f32 * brush.spacing_m, j as f32 * brush.spacing_m);
                if dx * dx + dz * dz > r2 {
                    continue;
                }
                let key = regia_core::math::hash::pcg_hash(
                    (i as u32).wrapping_mul(73_856_093) ^ (j as u32).wrapping_mul(19_349_663) ^ (si as u32),
                );
                if brush.density < 1.0 && (key as f32 / u32::MAX as f32) >= brush.density {
                    continue;
                }
                let probe = Vec3::new(c.position.x + dx, c.position.y, c.position.z + dz);
                let Some(h) = project_down(world, probe, hit) else {
                    continue;
                };
                let min2 = (brush.spacing_m * 0.5).powi(2);
                let crowded = points.points.iter().any(|p| {
                    p.layer == brush.layer && {
                        let d = p.position - h.position;
                        d.x * d.x + d.z * d.z < min2
                    }
                });
                if crowded {
                    continue;
                }
                points.points.push(PaintedPoint {
                    position: h.position,
                    normal: h.normal,
                    layer: brush.layer,
                    yaw: if brush.random_yaw {
                        stamp_yaw(brush.seed, h.position)
                    } else {
                        0.0
                    },
                });
                added += 1;
            }
        }
    }
    world.entity_mut(e).insert(points);
    Ok((added, removed))
}

/// Ends the open session: `apply` commits it as one undo entry; otherwise everything it did is
/// undone (a spawned target is despawned, an edited one restored). Returns whether a session was
/// open.
///
/// # Errors
///
/// Never today (the signature leaves room for apply-time validation).
pub fn finish(world: &mut World, apply: bool) -> Result<bool, DocError> {
    let Some(session) = world
        .get_resource_mut::<ProcToolSession>()
        .and_then(|mut s| s.0.take())
    else {
        return Ok(false);
    };
    let e = session.target;
    let before = session.undo_snapshot.first().cloned().unwrap_or_default();
    if apply {
        let after = ComponentSnapshot::capture(world, e);
        let command = ProcToolCommand {
            guid: session.guid,
            name: session.name.clone(),
            preset: session.preset.clone(),
            created: session.created,
            before,
            after,
            label: format!("Procgen tool: {} with {}", session.kind.name(), session.preset),
        };
        world
            .get_resource_or_insert_with(UndoStack::new)
            .push(Box::new(command));
    } else if session.created {
        despawn_target(world, e);
    } else {
        before.restore(world, e);
        regia_procgen::world::request_run(world, e);
    }
    if !apply && session.biome.is_some() {
        rerun_preset_generators(world, &session.preset);
    }
    Ok(true)
}

/// How close a cursor ray must pass to a control point to grab it, metres.
pub const POINT_PICK_M: f32 = 0.45;
/// A press that moves at least this far on the surface is a drag, metres.
const DRAG_START_M: f32 = 0.05;

/// One sample of the pointer inside a procedural tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    /// The primary button went down.
    Press,
    /// The primary button is held and the pointer moved.
    Move,
    /// The primary button went up.
    Release,
}

/// A viewport gesture for the armed tool mode (spec 67 § 5.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToolEvent {
    /// A pointer sample. `origin` and `direction` are a world ray.
    Pointer {
        /// Press, move or release.
        phase: PointerPhase,
        /// Ray origin.
        origin: Vec3,
        /// Ray direction (not necessarily unit).
        direction: Vec3,
    },
    /// Drop the last control point.
    Backspace,
    /// Toggle the path's closed flag (an area stays closed).
    Close,
    /// Apply the session as one undo entry.
    Enter,
    /// Cancel the session.
    Escape,
    /// Paint: change the brush (`[` and `]` scale the radius, digits 1 to 8 pick the layer, Shift
    /// erases). `None` keeps a field.
    Brush {
        /// New radius, meters.
        radius_m: Option<f32>,
        /// New zero-based layer.
        layer: Option<u8>,
        /// Erase instead of stamping.
        erase: Option<bool>,
    },
}

/// What [`handle_event`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct PointerOutcome {
    /// Set when the event edited or applied the session.
    pub outcome: Option<ToolOutcome>,
    /// Escape cancelled an open session.
    pub cancelled: bool,
}

/// The armed viewport tool mode "Procedural" (spec 67 § 5). Mouse gestures go through
/// [`handle_event`], which calls [`run_tool`] and [`finish`], so a click sequence and the same
/// scripted calls write one document. Path and Area click points, Paint drags brush strokes (stamped
/// in the background, [`PaintStrokeQueue`]), Box drags a footprint and then a height.
#[derive(Resource, Clone, Debug)]
pub struct ProcViewportMode {
    /// The tool kind.
    pub kind: ToolKind,
    /// Preset graph path.
    pub preset: String,
    /// Area on a biome preset: the biome (default: the preset's `biome` parameter).
    pub biome: Option<String>,
    /// Paint: the brush.
    pub brush: Brush,
    /// Box: the footprint corner the press landed on, while the footprint drag runs.
    box_corner: Option<Vec3>,
    /// Box: the footprint `(min, max)` on the ground once drawn; the next drag sets the height.
    box_footprint: Option<(Vec3, Vec3)>,
    /// Paint: the last stroke point queued in this drag.
    stroke_last: Option<Vec3>,
    /// Control point a press landed on.
    grip: Option<usize>,
    /// The press has moved far enough to be a drag.
    dragging: bool,
    /// Surface point of the press, when the ray hit.
    press: Option<Vec3>,
    /// The last problem, shown in the viewport. Empty after a successful event.
    pub last_error: String,
}

/// Arms the viewport tool mode for `kind` with `preset`. An open session of another kind or preset
/// is cancelled first (its spawn is despawned, its edits restored).
pub fn arm(world: &mut World, kind: ToolKind, preset: &str) {
    arm_with(world, kind, preset, None, None);
}

/// [`arm`] with an Area biome and a Paint brush. Arming the same kind and preset again only updates
/// the biome and brush.
pub fn arm_with(
    world: &mut World,
    kind: ToolKind,
    preset: &str,
    biome: Option<String>,
    brush: Option<Brush>,
) {
    if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>()
        && mode.kind == kind
        && mode.preset == preset
    {
        if biome.is_some() {
            mode.biome = biome;
        }
        if let Some(b) = brush {
            mode.brush = b;
        }
        return;
    }
    let _ = finish(world, false);
    world.insert_resource(PaintStrokeQueue::default());
    world.insert_resource(ProcViewportMode {
        kind,
        preset: preset.to_string(),
        biome,
        brush: brush.unwrap_or_default(),
        box_corner: None,
        box_footprint: None,
        stroke_last: None,
        grip: None,
        dragging: false,
        press: None,
        last_error: String::new(),
    });
}

/// Paint strokes waiting to be stamped (spec 67 § 5.2: strokes run in the background). A drag
/// queues stroke points; each editor tick stamps them in order under a ray-cast budget
/// ([`PAINT_CELLS_PER_TICK`]), so a large brush never stalls a frame. The stamping needs the world's
/// surfaces (the editor's pick path), so it runs between frames on the editor thread rather than
/// on a thread of its own (ADR-7215).
#[derive(Resource, Default, Debug)]
pub struct PaintStrokeQueue {
    /// Stroke points and the brush each was drawn with, oldest first.
    pub pending: std::collections::VecDeque<(Vec3, Brush)>,
    /// Stroke points stamped so far in this session.
    pub stamped: u64,
    /// Editor ticks that stamped at least one stroke point.
    pub busy_ticks: u64,
}

/// Most brush cells (ray casts) the background stroke queue spends per editor tick; one stroke
/// point is always stamped whole, even when it alone is larger.
pub const PAINT_CELLS_PER_TICK: u64 = 2048;

/// Stroke points waiting in [`PaintStrokeQueue`].
#[must_use]
pub fn pending_strokes(world: &World) -> usize {
    world
        .get_resource::<PaintStrokeQueue>()
        .map_or(0, |q| q.pending.len())
}

fn brush_cells(brush: &Brush) -> u64 {
    let steps = (f64::from(brush.radius_m) / f64::from(brush.spacing_m.max(1e-6)))
        .floor()
        .clamp(0.0, f64::from(MAX_BRUSH_STEPS)) as u64;
    (2 * steps + 1) * (2 * steps + 1)
}

/// Stamps queued stroke points: all of them with `budget` `None`, else until `budget` brush cells
/// were spent (at least one point). Returns the number stamped.
fn drain_strokes(world: &mut World, budget: Option<u64>) -> usize {
    let Ok((kind, preset)) = mode_kind_preset(world) else {
        if let Some(mut q) = world.get_resource_mut::<PaintStrokeQueue>() {
            q.pending.clear();
        }
        return 0;
    };
    if kind != ToolKind::Paint {
        return 0;
    }
    let _span = tracing::info_span!(target: "regia_editor", "procgen_paint_strokes").entered();
    let mut spent = 0u64;
    let mut done = 0usize;
    loop {
        if budget.is_some_and(|b| done > 0 && spent >= b) {
            break;
        }
        let Some((point, brush)) = world
            .get_resource_mut::<PaintStrokeQueue>()
            .and_then(|mut q| q.pending.pop_front())
        else {
            break;
        };
        spent += brush_cells(&brush);
        done += 1;
        let gesture = ToolGesture {
            points: vec![point],
            brush: Some(brush),
            ..ToolGesture::default()
        };
        match run_tool(world, kind, &preset, None, &gesture, false) {
            Ok(_) => note_ok(world),
            Err(err) => note_error(world, &err),
        }
    }
    if done > 0
        && let Some(mut q) = world.get_resource_mut::<PaintStrokeQueue>()
    {
        q.stamped += done as u64;
        q.busy_ticks += 1;
    }
    done
}

/// Cancels an open session and leaves the viewport tool.
pub fn disarm(world: &mut World) {
    let _ = finish(world, false);
    world.remove_resource::<ProcViewportMode>();
    world.remove_resource::<PaintStrokeQueue>();
}

/// Whether the viewport tool mode is armed.
#[must_use]
pub fn armed(world: &World) -> bool {
    world.contains_resource::<ProcViewportMode>()
}

fn note_error(world: &mut World, err: &DocError) {
    if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
        mode.last_error = err.to_string();
    }
}

fn note_ok(world: &mut World) {
    if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
        mode.last_error.clear();
    }
}

fn mode_kind_preset(world: &World) -> Result<(ToolKind, String), DocError> {
    world
        .get_resource::<ProcViewportMode>()
        .map(|m| (m.kind, m.preset.clone()))
        .ok_or_else(|| DocError::new("procgen_tool_busy", "no procedural tool is armed"))
}

fn session_target(world: &World) -> Option<Entity> {
    world
        .get_resource::<ProcToolSession>()
        .and_then(|s| s.0.as_ref().map(|s| s.target))
}

fn ray_point_distance(origin: Vec3, dir: Vec3, point: Vec3) -> f32 {
    let Some(dir) = dir.try_normalize() else {
        return f32::MAX;
    };
    let w = point - origin;
    let t = w.dot(dir).max(0.0);
    (w - dir * t).length()
}

fn point_world(tf: &Transform, local: Vec3) -> Vec3 {
    tf.rotation * (local * tf.scale) + tf.translation
}

/// Index of the session control point whose world position lies within [`POINT_PICK_M`] of the ray.
fn pick_point(world: &World, origin: Vec3, dir: Vec3) -> Option<usize> {
    let e = session_target(world)?;
    let spline = world.get::<Spline>(e)?;
    let tf = world.get::<Transform>(e).copied().unwrap_or(Transform::IDENTITY);
    let mut best: Option<(usize, f32)> = None;
    for (i, p) in spline.points.iter().enumerate() {
        let d = ray_point_distance(origin, dir, point_world(&tf, p.position));
        if d <= POINT_PICK_M && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

fn hit_of(world: &World, origin: Vec3, dir: Vec3) -> Result<Vec3, DocError> {
    let prefs = world
        .get_resource::<super::debug_view::ProcEditorPrefs>()
        .cloned()
        .unwrap_or_default();
    let kind = world
        .get_resource::<ProcViewportMode>()
        .map(|m| m.kind)
        .unwrap_or(ToolKind::Path);
    let filter = prefs.hit_filters.get(kind.name()).cloned().unwrap_or_default();
    cast(world, origin, dir, &filter)
        .map(|h| h.position)
        .ok_or_else(|| {
            DocError::new(
                "procgen_tool_no_hit",
                "the cursor ray hits nothing under the tool's filter",
            )
        })
}

fn gesture_call(
    world: &mut World,
    mut gesture: ToolGesture,
    apply: bool,
) -> Result<PointerOutcome, DocError> {
    let (kind, preset) = mode_kind_preset(world)?;
    if gesture.biome.is_none() {
        gesture.biome = world
            .get_resource::<ProcViewportMode>()
            .and_then(|m| m.biome.clone());
    }
    match run_tool(world, kind, &preset, None, &gesture, apply) {
        Ok(outcome) => {
            note_ok(world);
            if apply && let Some(mut stack) = world.get_resource_mut::<UndoStack>() {
                stack.seal();
            }
            Ok(PointerOutcome {
                outcome: Some(outcome),
                cancelled: false,
            })
        }
        Err(err) => {
            note_error(world, &err);
            Err(err)
        }
    }
}

fn pointer_event(
    world: &mut World,
    phase: PointerPhase,
    origin: Vec3,
    direction: Vec3,
) -> Result<PointerOutcome, DocError> {
    let Some(kind) = world.get_resource::<ProcViewportMode>().map(|m| m.kind) else {
        return Err(DocError::new("procgen_tool_busy", "no procedural tool is armed"));
    };
    match kind {
        ToolKind::Paint => return paint_pointer(world, phase, origin, direction),
        ToolKind::Box => return box_pointer(world, phase, origin, direction),
        ToolKind::Path | ToolKind::Area => {}
    }
    match phase {
        PointerPhase::Press => {
            let hit = hit_of(world, origin, direction).ok();
            let grip = hit.and_then(|_| pick_point(world, origin, direction));
            if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
                mode.press = hit;
                mode.grip = grip;
                mode.dragging = false;
                if hit.is_some() {
                    mode.last_error.clear();
                }
            }
            if hit.is_none() {
                let err = DocError::new(
                    "procgen_tool_no_hit",
                    "the cursor ray hits nothing under the tool's filter",
                );
                note_error(world, &err);
            }
            Ok(PointerOutcome {
                outcome: None,
                cancelled: false,
            })
        }
        PointerPhase::Move => {
            let (grip, press, already) = {
                let mode = world.get_resource::<ProcViewportMode>();
                match mode {
                    Some(m) => (m.grip, m.press, m.dragging),
                    None => (None, None, false),
                }
            };
            let Some(index) = grip else {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            };
            let Ok(hit) = hit_of(world, origin, direction) else {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            };
            let far = press.is_some_and(|p| p.distance(hit) >= DRAG_START_M);
            if far && let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
                mode.dragging = true;
            }
            if !(already || far) {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            }
            gesture_call(
                world,
                ToolGesture {
                    move_point: Some((index, hit)),
                    ..ToolGesture::default()
                },
                false,
            )
        }
        PointerPhase::Release => {
            let (grip, dragging, press) = {
                let mode = world.get_resource::<ProcViewportMode>();
                match mode {
                    Some(m) => (m.grip, m.dragging, m.press),
                    None => (None, false, None),
                }
            };
            if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
                mode.grip = None;
                mode.dragging = false;
                mode.press = None;
            }
            if grip.is_some() || dragging || press.is_none() {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            }
            let hit = match hit_of(world, origin, direction) {
                Ok(p) => p,
                Err(err) => {
                    note_error(world, &err);
                    return Ok(PointerOutcome {
                        outcome: None,
                        cancelled: false,
                    });
                }
            };
            gesture_call(
                world,
                ToolGesture {
                    points: vec![hit],
                    ..ToolGesture::default()
                },
                false,
            )
        }
    }
}

const NOTHING: PointerOutcome = PointerOutcome {
    outcome: None,
    cancelled: false,
};

/// Paint: a press starts a stroke, each move at least half the spacing from the last stroke point
/// queues another, the release ends it. Points are stamped in the background.
fn paint_pointer(
    world: &mut World,
    phase: PointerPhase,
    origin: Vec3,
    direction: Vec3,
) -> Result<PointerOutcome, DocError> {
    let hit = match hit_of(world, origin, direction) {
        Ok(p) => Some(p),
        Err(err) => {
            if phase == PointerPhase::Press {
                note_error(world, &err);
            }
            None
        }
    };
    let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() else {
        return Ok(NOTHING);
    };
    let brush = mode.brush;
    let queue_it = match phase {
        PointerPhase::Press => {
            mode.stroke_last = hit;
            hit
        }
        PointerPhase::Move => match (mode.stroke_last, hit) {
            (Some(last), Some(p)) if last.distance(p) >= brush.spacing_m * 0.5 => {
                mode.stroke_last = Some(p);
                Some(p)
            }
            _ => None,
        },
        PointerPhase::Release => {
            mode.stroke_last = None;
            None
        }
    };
    if let Some(p) = queue_it {
        // The stroke centers are surface hits; `run_tool` projects them down again, from above.
        let mut q = world.get_resource_or_insert_with(PaintStrokeQueue::default);
        if q.pending.len() >= MAX_STROKE_POINTS {
            let err = DocError::new(
                "procgen_tool_bad_gesture",
                format!("{MAX_STROKE_POINTS} stroke points are waiting; let them stamp first"),
            );
            note_error(world, &err);
            return Err(err);
        }
        q.pending.push_back((p, brush));
    }
    Ok(NOTHING)
}

/// The smallest box height a drag can give, meters.
pub const MIN_BOX_HEIGHT_M: f32 = 0.1;
/// The height a box footprint starts with before the height drag, meters.
pub const DEFAULT_BOX_HEIGHT_M: f32 = 1.0;

/// Where a ray passes closest to the vertical line through `at`: the height on that line.
fn ray_height_at(origin: Vec3, direction: Vec3, at: Vec3) -> Option<f32> {
    let d = direction.try_normalize()?;
    // Closest points of the ray `origin + t d` and the line `at + s Y`.
    let w = origin - at;
    let (a, b, c) = (d.dot(d), d.y, 1.0);
    let (dd, e) = (d.dot(w), w.y);
    let denom = a * c - b * b;
    if denom.abs() < 1e-6 {
        return None;
    }
    let s = (a * e - b * dd) / denom;
    Some(at.y + s)
}

/// Box: the first drag draws the footprint on the ground (press on one corner, release on the
/// other); the next drag sets the height from where the cursor ray passes the footprint's center.
/// Each release writes the box through [`run_tool`]; Enter applies.
fn box_pointer(
    world: &mut World,
    phase: PointerPhase,
    origin: Vec3,
    direction: Vec3,
) -> Result<PointerOutcome, DocError> {
    let (corner, footprint) = match world.get_resource::<ProcViewportMode>() {
        Some(m) => (m.box_corner, m.box_footprint),
        None => return Ok(NOTHING),
    };
    let footprint_box = |a: Vec3, b: Vec3| {
        let lo = a.min(b);
        let hi = a.max(b);
        (
            Vec3::new(lo.x, lo.y.min(hi.y), lo.z),
            Vec3::new(hi.x, lo.y.min(hi.y), hi.z),
        )
    };
    match (phase, footprint) {
        (PointerPhase::Press, None) => {
            let hit = match hit_of(world, origin, direction) {
                Ok(p) => p,
                Err(err) => {
                    note_error(world, &err);
                    return Ok(NOTHING);
                }
            };
            if let Some(mut m) = world.get_resource_mut::<ProcViewportMode>() {
                m.box_corner = Some(hit);
                m.last_error.clear();
            }
            Ok(NOTHING)
        }
        (PointerPhase::Move, _) => Ok(NOTHING),
        (PointerPhase::Release, None) => {
            let (Some(a), Ok(b)) = (corner, hit_of(world, origin, direction)) else {
                return Ok(NOTHING);
            };
            if let Some(mut m) = world.get_resource_mut::<ProcViewportMode>() {
                m.box_corner = None;
            }
            let (lo, hi) = footprint_box(a, b);
            if (hi.x - lo.x) <= 1e-3 || (hi.z - lo.z) <= 1e-3 {
                let err = DocError::new(
                    "procgen_tool_bad_gesture",
                    "drag a footprint with a size on both ground axes",
                );
                note_error(world, &err);
                return Ok(NOTHING);
            }
            if let Some(mut m) = world.get_resource_mut::<ProcViewportMode>() {
                m.box_footprint = Some((lo, hi));
            }
            gesture_call(
                world,
                ToolGesture {
                    box_extents: Some((lo, hi + Vec3::Y * DEFAULT_BOX_HEIGHT_M)),
                    ..ToolGesture::default()
                },
                false,
            )
        }
        (PointerPhase::Press, Some(_)) => Ok(NOTHING),
        (PointerPhase::Release, Some((lo, hi))) => {
            let center = (lo + hi) * 0.5;
            let Some(top) = ray_height_at(origin, direction, center) else {
                return Ok(NOTHING);
            };
            let height = (top - lo.y).max(MIN_BOX_HEIGHT_M);
            gesture_call(
                world,
                ToolGesture {
                    box_extents: Some((lo, Vec3::new(hi.x, lo.y + height, hi.z))),
                    ..ToolGesture::default()
                },
                false,
            )
        }
    }
}

/// Applies one viewport gesture. Click (press then release on empty space) appends a point, a drag
/// moves the grabbed point, Backspace drops the last point, Close toggles a path, Enter applies
/// the whole session as one undo entry and Escape cancels it.
///
/// # Errors
///
/// `procgen_tool_busy` when nothing is armed, plus the [`run_tool`] codes when a gesture is refused.
pub fn handle_event(world: &mut World, event: ToolEvent) -> Result<PointerOutcome, DocError> {
    if !armed(world) {
        return Err(DocError::new("procgen_tool_busy", "no procedural tool is armed"));
    }
    match event {
        ToolEvent::Pointer {
            phase,
            origin,
            direction,
        } => pointer_event(world, phase, origin, direction),
        ToolEvent::Backspace => {
            let Some(e) = session_target(world) else {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            };
            let n = world.get::<Spline>(e).map_or(0, |s| s.points.len());
            if n == 0 {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            }
            gesture_call(
                world,
                ToolGesture {
                    remove_point: Some(n - 1),
                    ..ToolGesture::default()
                },
                false,
            )
        }
        ToolEvent::Close => {
            let Some(e) = session_target(world) else {
                return Ok(PointerOutcome {
                    outcome: None,
                    cancelled: false,
                });
            };
            let closed = world.get::<Spline>(e).is_some_and(|s| s.closed);
            gesture_call(
                world,
                ToolGesture {
                    closed: Some(!closed),
                    ..ToolGesture::default()
                },
                false,
            )
        }
        ToolEvent::Enter => {
            // Strokes still waiting belong to this session: stamp them before it is applied.
            drain_strokes(world, None);
            if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
                mode.box_footprint = None;
                mode.box_corner = None;
            }
            if session_target(world).is_none() {
                return Ok(NOTHING);
            }
            gesture_call(world, ToolGesture::default(), true)
        }
        ToolEvent::Brush {
            radius_m,
            layer,
            erase,
        } => {
            let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() else {
                return Ok(NOTHING);
            };
            if let Some(r) = radius_m.filter(|r| r.is_finite() && *r > 0.0) {
                mode.brush.radius_m = r.clamp(0.05, 500.0);
            }
            if let Some(l) = layer.filter(|l| usize::from(*l) < MAX_PAINT_LAYERS) {
                mode.brush.layer = l;
            }
            if let Some(e) = erase {
                mode.brush.erase = e;
            }
            Ok(NOTHING)
        }
        ToolEvent::Escape => {
            if let Some(mut q) = world.get_resource_mut::<PaintStrokeQueue>() {
                q.pending.clear();
            }
            let cancelled = finish(world, false)?;
            if let Some(mut mode) = world.get_resource_mut::<ProcViewportMode>() {
                mode.grip = None;
                mode.dragging = false;
                mode.press = None;
                mode.box_corner = None;
                mode.box_footprint = None;
                mode.stroke_last = None;
                mode.last_error.clear();
            }
            Ok(PointerOutcome {
                outcome: None,
                cancelled,
            })
        }
    }
}

/// Stable identity of a placement, for comparing a mouse session with the same scripted calls.
/// The entity id and guid are left out (both are random); the name, spline, hull, graph and box
/// are not. An equality fingerprint, not a cryptographic hash.
#[must_use]
pub fn placement_hash(world: &World, entity: Entity) -> [u8; 32] {
    let name = world.get::<Name>(entity).map(|n| n.as_str()).unwrap_or("");
    let spline = world.get::<Spline>(entity);
    let closed = spline.is_some_and(|s| s.closed);
    let graph = world
        .get::<ProcGenerator>(entity)
        .map(|g| g.graph.as_str())
        .unwrap_or("");
    let half = world.get::<ProcGenerator>(entity).map(|g| match g.bounds {
        ProcBounds::Box { half_extents } => half_extents,
        ProcBounds::FromShape | ProcBounds::FromSpline { .. } => Vec3::ZERO,
    });
    let hull = world.get::<Shape>(entity).map_or(0, |s| match &s.kind {
        ShapeKind::Hull { points } => points.len(),
        _ => 0,
    });
    // FNV-1a over eight lanes. Public-domain hash, used only so two placements compare equal.
    let mut lanes = [0x811c_9dc5_u32; 8];
    let mut i = 0usize;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            let lane = i % 8;
            lanes[lane] ^= u32::from(*b);
            lanes[lane] = lanes[lane].wrapping_mul(0x0100_0193);
            i += 1;
        }
    };
    mix(name.as_bytes());
    mix(&[u8::from(closed)]);
    mix(graph.as_bytes());
    mix(&u32::try_from(hull).unwrap_or(u32::MAX).to_le_bytes());
    if let Some(h) = half {
        mix(&h.x.to_bits().to_le_bytes());
        mix(&h.y.to_bits().to_le_bytes());
        mix(&h.z.to_bits().to_le_bytes());
    }
    if let Some(spline) = spline {
        for p in &spline.points {
            mix(&p.position.x.to_bits().to_le_bytes());
            mix(&p.position.y.to_bits().to_le_bytes());
            mix(&p.position.z.to_bits().to_le_bytes());
        }
    }
    let mut out = [0u8; 32];
    for (lane_i, lane) in lanes.iter().enumerate() {
        out[lane_i * 4..lane_i * 4 + 4].copy_from_slice(&lane.to_le_bytes());
    }
    out
}

/// The readable form of [`placement_hash`], so a mismatch can be printed.
#[must_use]
pub fn placement_text(world: &World, entity: Entity) -> String {
    let name = world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default();
    let spline = world.get::<Spline>(entity).cloned().unwrap_or_default();
    let graph = world
        .get::<ProcGenerator>(entity)
        .map(|g| g.graph.clone())
        .unwrap_or_default();
    let half = world.get::<ProcGenerator>(entity).map(|g| match g.bounds {
        ProcBounds::Box { half_extents } => half_extents,
        ProcBounds::FromShape | ProcBounds::FromSpline { .. } => Vec3::ZERO,
    });
    let hull = world.get::<Shape>(entity).map_or(0, |s| match &s.kind {
        ShapeKind::Hull { points } => points.len(),
        _ => 0,
    });
    let mut lines = format!(
        "{name} closed={} graph={graph} hull={hull} half={half:?}\n",
        spline.closed
    );
    for p in &spline.points {
        lines.push_str(&format!("{:?} {:?}\n", p.position, p.mode));
    }
    lines
}

/// Viewport input for the armed tool mode. Returns `true` while the tool is armed and the
/// pointer is in the pane (or a point drag is in progress), so box select does not start.
pub fn viewport_input(
    ui: &egui::Ui,
    world: &mut World,
    response: &egui::Response,
    camera: &crate::viewport::EditorCamera,
    size: egui::Vec2,
) -> bool {
    if !armed(world) {
        return false;
    }
    let keyboard_free = !ui.ctx().egui_wants_keyboard_input();
    let hovered = response.hovered() || response.dragged();
    if hovered && keyboard_free {
        let key = ui.input(|i| {
            if i.key_pressed(egui::Key::Backspace) {
                Some(ToolEvent::Backspace)
            } else if i.key_pressed(egui::Key::C) {
                Some(ToolEvent::Close)
            } else if i.key_pressed(egui::Key::Enter) {
                Some(ToolEvent::Enter)
            } else if i.key_pressed(egui::Key::Escape) {
                Some(ToolEvent::Escape)
            } else {
                None
            }
        });
        if let Some(event) = key {
            let _ = handle_event(world, event);
        }
        // Paint brush keys: `[` and `]` scale the radius, digits 1 to 8 pick the layer, Shift erases.
        let paint = world
            .get_resource::<ProcViewportMode>()
            .filter(|m| m.kind == ToolKind::Paint)
            .map(|m| m.brush);
        if let Some(brush) = paint {
            let (smaller, larger, layer, shift) = ui.input(|i| {
                let digits = [
                    egui::Key::Num1,
                    egui::Key::Num2,
                    egui::Key::Num3,
                    egui::Key::Num4,
                    egui::Key::Num5,
                    egui::Key::Num6,
                    egui::Key::Num7,
                    egui::Key::Num8,
                ];
                (
                    i.key_pressed(egui::Key::OpenBracket),
                    i.key_pressed(egui::Key::CloseBracket),
                    digits.iter().position(|k| i.key_pressed(*k)),
                    i.modifiers.shift,
                )
            });
            let radius = if smaller {
                Some(brush.radius_m / 1.25)
            } else if larger {
                Some(brush.radius_m * 1.25)
            } else {
                None
            };
            let layer = layer.and_then(|l| u8::try_from(l).ok());
            if radius.is_some() || layer.is_some() || shift != brush.erase {
                let _ = handle_event(
                    world,
                    ToolEvent::Brush {
                        radius_m: radius,
                        layer,
                        erase: Some(shift),
                    },
                );
            }
        }
    }
    let (pressed, down, released, alt) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
            i.modifiers.alt,
        )
    });
    let dragging = world
        .get_resource::<ProcViewportMode>()
        .is_some_and(|m| m.grip.is_some() || m.stroke_last.is_some() || m.box_corner.is_some());
    if !alt && (hovered || dragging) {
        let phase = if pressed {
            Some(PointerPhase::Press)
        } else if released {
            Some(PointerPhase::Release)
        } else if down && dragging {
            Some(PointerPhase::Move)
        } else {
            None
        };
        if let Some(phase) = phase
            && let Some(pointer) = response.interact_pointer_pos().or(response.hover_pos())
        {
            let local = pointer - response.rect.left_top();
            let (origin, direction) = crate::viewport::pick_ray(
                camera,
                regia_core::math::Vec2::new(local.x, local.y),
                regia_core::math::Vec2::new(size.x, size.y),
            );
            let _ = handle_event(
                world,
                ToolEvent::Pointer {
                    phase,
                    origin,
                    direction,
                },
            );
        }
    }
    if hovered {
        let (label, err) =
            world
                .get_resource::<ProcViewportMode>()
                .map_or((String::new(), String::new()), |m| {
                    let extra = match m.kind {
                        ToolKind::Paint => format!(
                            " · r {:.2} m · layer {}{}",
                            m.brush.radius_m,
                            m.brush.layer + 1,
                            if m.brush.erase { " · erase" } else { "" }
                        ),
                        ToolKind::Box if m.box_footprint.is_some() => " · drag the height".to_string(),
                        _ => String::new(),
                    };
                    (
                        format!("{} · {}{extra}", m.kind.name(), m.preset),
                        m.last_error.clone(),
                    )
                });
        ui.painter().text(
            response.rect.left_bottom() + egui::vec2(8.0, -8.0),
            egui::Align2::LEFT_BOTTOM,
            if err.is_empty() { label } else { err },
            egui::FontId::proportional(11.0),
            egui::Color32::from_rgb(220, 200, 140),
        );
    }
    hovered || dragging
}

/// Re-runs the open session's generator at most `tool_refresh_hz` times a second (counted in
/// editor ticks). Called from the preview tick.
pub(crate) fn tick(world: &mut World) {
    if pending_strokes(world) > 0 {
        drain_strokes(world, Some(PAINT_CELLS_PER_TICK));
    }
    let Some(target) = world
        .get_resource::<ProcToolSession>()
        .and_then(|s| s.0.as_ref().map(|s| s.target))
    else {
        return;
    };
    let hz = world
        .get_resource::<super::debug_view::ProcEditorPrefs>()
        .map_or(10, |p| p.tool_refresh_hz.max(1));
    let every = (super::debug_view::TICKS_PER_SECOND / hz).max(1);
    let stale = world
        .get::<regia_procgen::world::ProcGenState>(target)
        .is_some_and(|s| s.stale);
    let due = world
        .get_resource::<super::ProcPreview>()
        .is_some_and(|p| p.tool_ticks >= every);
    if stale && due {
        world.resource_mut::<super::ProcPreview>().tool_ticks = 0;
        regia_procgen::world::request_run(world, target);
    }
}

/// Every tool kind, in the order the panel and the preferences list them.
pub const ALL_KINDS: [ToolKind; 4] = [ToolKind::Path, ToolKind::Area, ToolKind::Paint, ToolKind::Box];

/// The editor preference key of one hit-filter field: `procgen.hit.<tool>.<field>` (spec 67 § 5.3;
/// `field` is `terrain`, `meshes`, `skip_generated` or `physics_mask`).
#[must_use]
pub fn hit_pref_key(kind: ToolKind, field: &str) -> String {
    format!("procgen.hit.{}.{field}", kind.name())
}

/// The `EditorPreferences` generation the hit filters were last read from.
#[derive(Resource, Default)]
struct HitPrefsSeen(Option<u64>);

/// The saved part of `kind`'s hit filter, read from the editor preferences (`only_entity` is a
/// session value and is not saved).
fn filter_from_prefs(prefs: &crate::editor_prefs::EditorPreferences, kind: ToolKind) -> HitFilter {
    let d = HitFilter::default();
    let mask = prefs.int(&hit_pref_key(kind, "physics_mask"));
    HitFilter {
        terrain: prefs.bool(&hit_pref_key(kind, "terrain")),
        meshes: prefs.bool(&hit_pref_key(kind, "meshes")),
        skip_generated: prefs.bool(&hit_pref_key(kind, "skip_generated")),
        only_entity: None,
        physics_mask: u32::try_from(mask).unwrap_or(d.physics_mask),
    }
}

/// Copies the per-tool hit filters from the editor preferences (the file the editor saves) into
/// [`super::ProcEditorPrefs`] when the preferences changed. A world whose preferences are all at
/// their defaults and were never changed leaves the filters alone, so hosts that set them directly
/// keep theirs. Runs once per editor tick; cheap when nothing changed.
pub(crate) fn sync_hit_filter_prefs(world: &mut World) {
    let Some(prefs) = world.get_resource::<crate::editor_prefs::EditorPreferences>() else {
        return;
    };
    let generation = prefs.generation();
    let seen = world.get_resource::<HitPrefsSeen>().and_then(|s| s.0);
    if seen == Some(generation) {
        return;
    }
    let any_set = ALL_KINDS.iter().any(|k| {
        ["terrain", "meshes", "skip_generated", "physics_mask"]
            .iter()
            .any(|f| !prefs.is_default(&hit_pref_key(*k, f)))
    });
    let filters: Vec<(ToolKind, HitFilter)> = ALL_KINDS
        .iter()
        .map(|k| (*k, filter_from_prefs(prefs, *k)))
        .collect();
    world.get_resource_or_insert_with(HitPrefsSeen::default).0 = Some(generation);
    if seen.is_none() && !any_set {
        return;
    }
    let mut proc_prefs = world.get_resource_or_insert_with(super::debug_view::ProcEditorPrefs::default);
    for (kind, mut filter) in filters {
        let entry = proc_prefs.hit_filters.entry(kind.name().to_string()).or_default();
        filter.only_entity = entry.only_entity;
        if *entry != filter {
            *entry = filter;
        }
    }
}

/// Sets `kind`'s hit filter for the tools and, when the editor keeps preferences, saves its saved
/// part there too (the editor writes the preferences file on its next frame).
pub fn set_hit_filter(world: &mut World, kind: ToolKind, filter: HitFilter) {
    use crate::editor_prefs::PrefValue;
    if let Some(mut prefs) = world.get_resource_mut::<crate::editor_prefs::EditorPreferences>() {
        let values = [
            ("terrain", PrefValue::Bool(filter.terrain)),
            ("meshes", PrefValue::Bool(filter.meshes)),
            ("skip_generated", PrefValue::Bool(filter.skip_generated)),
            ("physics_mask", PrefValue::Int(i64::from(filter.physics_mask))),
        ];
        for (field, value) in values {
            if let Err(err) = prefs.set(&hit_pref_key(kind, field), &value) {
                tracing::warn!(target: "regia_editor", "hit filter preference not set: {err}");
            }
        }
    }
    world
        .get_resource_or_insert_with(super::debug_view::ProcEditorPrefs::default)
        .hit_filters
        .insert(kind.name().to_string(), filter);
}

/// The hit filter `kind` uses now.
#[must_use]
pub fn hit_filter(world: &World, kind: ToolKind) -> HitFilter {
    world
        .get_resource::<super::debug_view::ProcEditorPrefs>()
        .and_then(|p| p.hit_filters.get(kind.name()).cloned())
        .unwrap_or_default()
}
