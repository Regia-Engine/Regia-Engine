// SPDX-License-Identifier: MPL-2.0
//! Surface placement in the viewport (`docs/specs/19-editor.md` § 3, ADR-2696): vertex snap (hold
//! `V` while translating) and drop-to-floor (`End`), with the "align to surface normal" option.
//!
//! **Vertex snap.** A ray through the pointer finds the surface under it with the selection's own
//! meshes left out ([`vertex_snap_at`]); of the hit triangle's three vertices the one nearest the
//! hit point is the target, and the entity's pivot moves onto it. While a translate gizmo drag runs
//! with `V` held the viewport replaces the gizmo's translation with that target; a drag is one undo
//! step like any gizmo drag.
//!
//! **Drop to floor.** [`drop_to_floor`] moves each selected entity (its top-level ones, each on its
//! own) straight down until its lowest point rests on the first surface below it. The drop distance
//! is the smallest gap measured over the entity's lowest vertices and a grid of columns across its
//! world bounds: for a column, a ray up from below finds the entity's underside and a ray down from
//! its middle height finds the surface. The rays start inside the entity's bounds, so one sunk into
//! the floor comes back up. Surfaces are the meshes and terrains the viewport draws, without the
//! selection; a collider with no mesh is not a surface, because the edit world steps no physics.
//!
//! **Align to surface normal.** With the option on, the entity first turns so its local up is the
//! normal of the surface below its center, then slides straight down until its lowest point along
//! that normal lies in the surface's plane.
//!
//! The agent surface is `editor.vertex_snap`, `editor.drop_to_floor` and `editor.placement_options`
//! (`book/agents/viewport-tools.md`).

use std::collections::HashSet;

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Mat4, Quat, Vec2, Vec3};
use regia_ecs::prelude::*;
use regia_render::{MeshRenderer, SkinJoints};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

use crate::command::{CommandGroup, EditorCommand, SetTransform, UndoStack};
use crate::hierarchy_ops::{top_level, world_affine};
use crate::mesh_source::pick_owner;
use crate::outliner::subtree_of;
use crate::picking::{FaceCull, PickHit, draws_in_viewport, mesh_entities_of, raycast_entity, raycast_world};
use crate::selection::Selection;
use crate::viewport_agent::Handler;
use crate::views::ViewCamera;

/// Vertices sampled per entity: the lowest ones decide a drop onto something flat.
const LOWEST_VERTICES: usize = 32;
/// Further vertices sampled evenly, for surfaces that are not flat.
const SPREAD_VERTICES: usize = 16;
/// Columns per side of the bounds grid.
const GRID_SIDE: usize = 5;
/// How far down a ray looks for terrain, metres.
const MAX_DROP: f32 = 100_000.0;
/// Rays start this far above the height they test, metres.
const RAY_START_LIFT: f32 = 1.0e-3;
/// Half the distance between the samples that give a terrain its normal, metres.
const TERRAIN_NORMAL_STEP: f32 = 0.25;
/// Below this upward component a surface is a wall: an aligned entity moves along the normal.
const MIN_UP_NORMAL: f32 = 0.05;
/// An entity this close to where a drop would put it (metres, radians) already rests there.
const RESTED: f32 = 1.0e-5;

/// The placement options of the viewport toolbar.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementOptions {
    /// Drop-to-floor also turns the entity's up onto the surface normal.
    pub align_to_normal: bool,
    /// Vertex snap stays on without holding its key (the palette's toggle).
    pub vertex_snap: bool,
}

/// Surface snapping during a translate drag (ADR-7756): while it is on, the gizmo's pivot slides on
/// the mesh or terrain under the pointer (the moving entities left out), optionally turning the
/// active entity's up onto the surface normal, and lifted `offset` meters along that normal.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct SurfaceSnap {
    /// Snap to the surface under the pointer while translating.
    pub enabled: bool,
    /// Also turn the active entity's up onto the surface normal.
    pub align: bool,
    /// Distance along the normal from the surface to the pivot, meters.
    pub offset: f32,
}

/// The surface point under a pane pixel that [`surface_at`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfacePoint {
    /// World position of the hit.
    pub position: Vec3,
    /// Unit normal there, facing the camera.
    pub normal: Vec3,
    /// The level entity hit (a model's owner, or the terrain).
    pub entity: Entity,
}

/// The first drawn mesh or terrain under the pixel `pointer` of a `size` pane seen through `view`,
/// with the subtrees of `moving` left out. `None` when the ray meets neither.
#[must_use]
pub fn surface_at(
    world: &World,
    view: &ViewCamera,
    size: Vec2,
    pointer: Vec2,
    moving: &[Entity],
) -> Option<SurfacePoint> {
    let _span = tracing::info_span!(target: "regia_editor", "surface_at").entered();
    let (origin, direction) = view.ray(pointer, size);
    let ignored = subtrees(world, moving);
    let hit = raycast_world(world, origin, direction, FaceCull::None, &|e| {
        !ignored.contains(&pick_owner(world, e)) && draws_in_viewport(world, e)
    });
    let limit = hit.as_ref().map_or(MAX_DROP, |h| h.distance);
    if let Some(ground) = regia_pick::raycast_terrain(world, origin, direction, limit)
        && !ignored.contains(&ground.entity)
    {
        let above = Vec3::new(
            ground.point.x,
            ground.point.y.max(origin.y) + MAX_DROP * 1.0e-3,
            ground.point.z,
        );
        return Some(SurfacePoint {
            position: ground.point,
            normal: terrain_normal(world, above, ground.point),
            entity: ground.entity,
        });
    }
    let hit = hit?;
    // The normal that matters faces the camera.
    let normal = if hit.normal.dot(direction) > 0.0 {
        -hit.normal
    } else {
        hit.normal
    };
    Some(SurfacePoint {
        position: hit.position,
        normal: normal.normalize_or_zero(),
        entity: hit.owner,
    })
}

/// The world pose a surface snap gives the gizmo frame `frame` (its world pose) on `hit`: the pivot
/// on the surface, `options.offset` along the normal, and with `options.align` the frame's up
/// turned onto the normal by the shortest arc.
#[must_use]
pub fn surface_snap_pose(frame: &Transform, hit: &SurfacePoint, options: &SurfaceSnap) -> Transform {
    let offset = if options.offset.is_finite() {
        options.offset
    } else {
        0.0
    };
    let rotation = if options.align && hit.normal != Vec3::ZERO {
        (Quat::from_rotation_arc((frame.rotation * Vec3::Y).normalize(), hit.normal) * frame.rotation)
            .normalize()
    } else {
        frame.rotation
    };
    Transform {
        translation: hit.position + hit.normal * offset,
        rotation,
        scale: frame.scale,
    }
}

/// The vertex a [`vertex_snap_at`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VertexSnap {
    /// World position of the vertex.
    pub position: Vec3,
    /// Its index in the hit mesh's vertex list.
    pub vertex: u32,
    /// The surface hit whose triangle it belongs to.
    pub hit: PickHit,
}

/// The vertex of `hit`'s triangle nearest the hit point: its index and world position (skinned
/// meshes through their current palette, like the ray cast). `None` when the mesh changed under the
/// hit.
#[must_use]
pub fn nearest_triangle_vertex(world: &World, hit: &PickHit) -> Option<(u32, Vec3)> {
    let renderer = world.get::<MeshRenderer>(hit.entity)?;
    let model = crate::mesh_source::world_matrix(world, hit.entity).unwrap_or(Mat4::IDENTITY);
    let palette = world
        .get::<SkinJoints>(hit.entity)
        .map_or(&[][..], |s| s.palette.as_slice());
    let mut best: Option<(u32, Vec3, f32)> = None;
    for index in hit.indices {
        let vertex = renderer.mesh.vertices.get(index as usize)?;
        let position = crate::handles::vertex_world_position(vertex, model, palette);
        let distance = position.distance_squared(hit.position);
        if best.is_none_or(|(_, _, held)| distance < held) {
            best = Some((index, position, distance));
        }
    }
    best.map(|(index, position, _)| (index, position))
}

/// Every entity of the subtrees of `roots`.
fn subtrees(world: &World, roots: &[Entity]) -> HashSet<Entity> {
    roots.iter().flat_map(|e| subtree_of(world, *e)).collect()
}

/// The vertex under the pixel `pointer` of a `size` pane seen through `view`, ignoring the subtrees
/// of `moving` (the entities being placed). `None` when the ray meets no drawn mesh, or terrain
/// hides what it meets.
#[must_use]
pub fn vertex_snap_at(
    world: &World,
    view: &ViewCamera,
    size: Vec2,
    pointer: Vec2,
    moving: &[Entity],
) -> Option<VertexSnap> {
    let _span = tracing::info_span!(target: "regia_editor", "vertex_snap").entered();
    let (origin, direction) = view.ray(pointer, size);
    let ignored = subtrees(world, moving);
    let hit = raycast_world(world, origin, direction, FaceCull::None, &|e| {
        !ignored.contains(&pick_owner(world, e)) && draws_in_viewport(world, e)
    })?;
    if crate::picking::pick_terrain(world, origin, direction, hit.distance).is_some() {
        return None;
    }
    let (vertex, position) = nearest_triangle_vertex(world, &hit)?;
    Some(VertexSnap {
        position,
        vertex,
        hit,
    })
}

/// The inverse of `entity`'s parent's world matrix (identity for a root). `None` when the parent
/// has a zero scale axis.
fn parent_inverse(world: &World, entity: Entity) -> Option<regia_core::math::Affine3A> {
    let Some(parent) = world.get::<ChildOf>(entity).map(ChildOf::parent) else {
        return Some(regia_core::math::Affine3A::IDENTITY);
    };
    let affine = world_affine(world, parent);
    (affine.matrix3.determinant().abs() > 1e-12).then(|| affine.inverse())
}

/// The local `Transform.translation` that puts `entity`'s pivot at the world point `target`.
/// `None` when the parent cannot be inverted.
#[must_use]
pub fn local_translation_for(world: &World, entity: Entity, target: Vec3) -> Option<Vec3> {
    let inverse = parent_inverse(world, entity)?;
    Some(inverse.transform_point3(target)).filter(|t| t.is_finite())
}

/// Brings `GlobalTransform`s and the hidden part entities up to date: an agent can edit and place
/// within one frame, and the ray casts read them.
fn refresh_world_state(world: &mut World) {
    regia_scene::propagate::propagate_transforms(world);
    crate::mesh_source::resolve_mesh_sources(world);
}

/// A surface met going straight down.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Surface {
    position: Vec3,
    /// Unit normal, facing the ray.
    normal: Vec3,
    entity: Entity,
}

/// The first drawn mesh or terrain straight below `origin`, leaving out `ignored`.
fn surface_below(world: &World, origin: Vec3, ignored: &HashSet<Entity>) -> Option<Surface> {
    let hit = raycast_world(world, origin, Vec3::NEG_Y, FaceCull::None, &|e| {
        !ignored.contains(&pick_owner(world, e)) && draws_in_viewport(world, e)
    });
    let limit = hit.as_ref().map_or(MAX_DROP, |h| h.distance);
    if let Some(ground) = regia_pick::raycast_terrain(world, origin, Vec3::NEG_Y, limit)
        && !ignored.contains(&ground.entity)
    {
        return Some(Surface {
            position: ground.point,
            normal: terrain_normal(world, origin, ground.point),
            entity: ground.entity,
        });
    }
    let hit = hit?;
    // A ray can meet a triangle's back: the normal that matters faces the ray.
    let normal = if hit.normal.y < 0.0 {
        -hit.normal
    } else {
        hit.normal
    };
    Some(Surface {
        position: hit.position,
        normal,
        // The level entity: a model's hidden part means nothing to the caller.
        entity: hit.owner,
    })
}

/// A terrain's normal at `at`, from the ground around it (the pick reports no normal). Straight up
/// when a neighbor sample leaves the terrain.
fn terrain_normal(world: &World, origin: Vec3, at: Vec3) -> Vec3 {
    let sample = |dx: f32, dz: f32| {
        regia_pick::raycast_terrain(
            world,
            Vec3::new(at.x + dx, origin.y, at.z + dz),
            Vec3::NEG_Y,
            MAX_DROP,
        )
        .map(|hit| hit.point)
    };
    let step = TERRAIN_NORMAL_STEP;
    let (Some(west), Some(east), Some(north), Some(south)) = (
        sample(-step, 0.0),
        sample(step, 0.0),
        sample(0.0, -step),
        sample(0.0, step),
    ) else {
        return Vec3::Y;
    };
    let normal = (south - north).cross(east - west).normalize_or_zero();
    if normal.y > 0.0 { normal } else { Vec3::Y }
}

/// The points of an entity subtree a drop measures.
struct Body {
    /// World points that can touch the floor: every mesh vertex, else the collider's outline, else
    /// the pivot.
    points: Vec<Vec3>,
    /// The mesh entities that draw it.
    meshes: Vec<Entity>,
    lo: Vec3,
    hi: Vec3,
}

fn body_of(world: &World, entity: Entity) -> Body {
    let members = subtree_of(world, entity);
    let mut meshes: Vec<Entity> = members.iter().flat_map(|m| mesh_entities_of(world, *m)).collect();
    meshes.sort_by_key(|e| e.to_bits());
    meshes.dedup();
    let mut points = Vec::new();
    for mesh in &meshes {
        let Some(renderer) = world.get::<MeshRenderer>(*mesh) else {
            continue;
        };
        let model = crate::mesh_source::world_matrix(world, *mesh).unwrap_or(Mat4::IDENTITY);
        let palette = world
            .get::<SkinJoints>(*mesh)
            .map_or(&[][..], |s| s.palette.as_slice());
        points.extend(
            renderer
                .mesh
                .vertices
                .iter()
                .map(|v| crate::handles::vertex_world_position(v, model, palette))
                .filter(|p| p.is_finite()),
        );
    }
    if points.is_empty() {
        points.extend(
            members
                .iter()
                .flat_map(|m| crate::collider_handles::collider_outline_points(world, *m)),
        );
    }
    if points.is_empty() {
        points.push(world_affine(world, entity).translation.into());
    }
    let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for p in &points {
        lo = lo.min(*p);
        hi = hi.max(*p);
    }
    Body {
        points,
        meshes,
        lo,
        hi,
    }
}

/// The world points whose gaps to the surface below decide the drop: the lowest vertices, an even
/// spread of the others and the underside of each column of a grid over the bounds.
fn drop_samples(world: &World, body: &Body) -> Vec<Vec3> {
    let mut by_height: Vec<Vec3> = body.points.clone();
    by_height.sort_by(|a, b| a.y.total_cmp(&b.y));
    let mut samples: Vec<Vec3> = by_height.iter().copied().take(LOWEST_VERTICES).collect();
    let rest = by_height.get(LOWEST_VERTICES..).unwrap_or(&[]);
    samples.extend(
        rest.iter()
            .step_by(rest.len().div_ceil(SPREAD_VERTICES).max(1))
            .copied(),
    );
    let extent = body.hi - body.lo;
    if body.meshes.is_empty() || (extent.x <= 0.0 && extent.z <= 0.0) {
        return samples;
    }
    // Just inside the bounds, so a column on the outline still meets the entity.
    let inset = extent * 1.0e-3;
    let below = body.lo.y - extent.y.max(1.0);
    for ix in 0..GRID_SIDE {
        for iz in 0..GRID_SIDE {
            let fx = ix as f32 / (GRID_SIDE - 1) as f32;
            let fz = iz as f32 / (GRID_SIDE - 1) as f32;
            let x = body.lo.x + inset.x + (extent.x - inset.x * 2.0) * fx;
            let z = body.lo.z + inset.z + (extent.z - inset.z * 2.0) * fz;
            let origin = Vec3::new(x, below, z);
            let underside = body
                .meshes
                .iter()
                .filter_map(|m| raycast_entity(world, *m, origin, Vec3::Y, FaceCull::None))
                .min_by(|a, b| a.distance.total_cmp(&b.distance));
            if let Some(hit) = underside {
                samples.push(hit.position);
            }
        }
    }
    samples
}

/// One entity a drop placed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DroppedEntity {
    /// The entity.
    pub entity: Entity,
    /// World position of its pivot before.
    pub from: Vec3,
    /// World position of its pivot after.
    pub to: Vec3,
    /// The surface it rests on.
    pub surface: Entity,
    /// That surface's unit normal.
    pub normal: Vec3,
    /// Its local transform after.
    pub transform: Transform,
}

/// What [`drop_to_floor`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DropReport {
    /// The entities it placed.
    pub placed: Vec<DroppedEntity>,
    /// The entities with no surface below them, or that cannot be moved (left where they are).
    pub missed: Vec<Entity>,
    /// Whether an undo entry was pushed (none when every entity already rested).
    pub undoable: bool,
}

/// Where one entity goes.
fn placement_of(
    world: &World,
    entity: Entity,
    ignored: &HashSet<Entity>,
    align: bool,
) -> Option<DroppedEntity> {
    let current = *world.get::<Transform>(entity)?;
    if world.get::<regia_scene::prelude::FeedOwned>(entity).is_some() {
        return None;
    }
    let inverse = parent_inverse(world, entity)?;
    let body = body_of(world, entity);
    let center = (body.lo + body.hi) * 0.5;
    // The rays start inside the bounds, so an entity sunk into the floor comes back up.
    let start = |at: Vec3| at.y.max(center.y) + RAY_START_LIFT;
    let mut gap = f32::MAX;
    let mut contact: Option<Surface> = None;
    for sample in drop_samples(world, &body) {
        let origin = Vec3::new(sample.x, start(sample), sample.z);
        if let Some(surface) = surface_below(world, origin, ignored) {
            let here = sample.y - surface.position.y;
            if here < gap {
                gap = here;
                contact = Some(surface);
            }
        }
    }
    let contact = contact?;
    let affine = world_affine(world, entity);
    let pivot: Vec3 = affine.translation.into();
    let (to, rotation, normal) = if align {
        let (_, world_rotation, _) = affine.to_scale_rotation_translation();
        let parent_rotation = inverse.to_scale_rotation_translation().1;
        let middle = Vec3::new(center.x, start(center), center.z);
        let surface = surface_below(world, middle, ignored).unwrap_or(contact);
        let turn = Quat::from_rotation_arc(world_rotation * Vec3::Y, surface.normal);
        // The lowest point along the normal, once turned about the pivot, reaches the plane.
        let distance = body
            .points
            .iter()
            .map(|p| (pivot + turn * (*p - pivot) - surface.position).dot(surface.normal))
            .fold(f32::MAX, f32::min);
        let to = if surface.normal.y >= MIN_UP_NORMAL {
            pivot - Vec3::Y * (distance / surface.normal.y)
        } else {
            pivot - surface.normal * distance
        };
        (
            to,
            (parent_rotation * turn * world_rotation).normalize(),
            surface.normal,
        )
    } else {
        // Only the translation changes: a world rotation read back from a sheared matrix (a turned
        // child of a non-uniformly scaled parent) would not be the local one.
        (pivot - Vec3::Y * gap, current.rotation, contact.normal)
    };
    let transform = Transform {
        translation: inverse.transform_point3(to),
        rotation,
        scale: current.scale,
    };
    (transform.translation.is_finite() && transform.rotation.is_finite()).then_some(DroppedEntity {
        entity,
        from: pivot,
        to,
        surface: contact.entity,
        normal,
        transform,
    })
}

/// Moves the top-level entities of `entities` down onto the first surface below each (none of the
/// listed entities count as surfaces), as one undo step. With `align`, each also turns its up onto
/// the surface normal.
pub fn drop_to_floor(world: &mut World, entities: &[Entity], align: bool) -> DropReport {
    let _span = tracing::info_span!(target: "regia_editor", "drop_to_floor").entered();
    refresh_world_state(world);
    let top = top_level(world, entities);
    let ignored = subtrees(world, &top);
    let mut report = DropReport::default();
    for entity in top {
        match placement_of(world, entity, &ignored, align) {
            Some(placed) => report.placed.push(placed),
            None => report.missed.push(entity),
        }
    }
    let mut group = CommandGroup::new("Drop to floor");
    for placed in &report.placed {
        let rested = world.get::<Transform>(placed.entity).is_some_and(|current| {
            current.translation.distance(placed.transform.translation) < RESTED
                && current.rotation.angle_between(placed.transform.rotation) < RESTED
        });
        if rested {
            continue;
        }
        let mut command = SetTransform::capture(world, placed.entity, placed.transform);
        command.apply(world);
        group.push(Box::new(command));
    }
    if !group.commands.is_empty() {
        report.undoable = true;
        world
            .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
            .push(Box::new(group));
        refresh_world_state(world);
    }
    report
}

/// The `End` key and the palette's `selection.drop_to_floor`: the selection with the toolbar's
/// option. Returns a status line.
pub fn drop_selection(world: &mut World) -> String {
    let selected = world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    if selected.is_empty() {
        return "Nothing selected to drop".to_owned();
    }
    let align = world
        .get_resource::<PlacementOptions>()
        .is_some_and(|o| o.align_to_normal);
    let report = drop_to_floor(world, &selected, align);
    match (report.placed.len(), report.missed.len()) {
        (0, _) => "No surface below the selection".to_owned(),
        (n, 0) => format!("Dropped {n} to the floor"),
        (n, missed) => format!("Dropped {n} to the floor; {missed} had no surface below"),
    }
}

// ---------------------------------------------------------------------------------------------
// Agent protocol
// ---------------------------------------------------------------------------------------------

fn refused(reason: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, reason, message, hint)
}

fn selection_or_param(world: &World, params: &Value) -> Result<Vec<Entity>, RpcError> {
    if let Some(list) = params.get("entities").and_then(Value::as_array) {
        return list
            .iter()
            .map(|bits| {
                bits.as_u64()
                    .and_then(|bits| crate::command::resolve_entity(world, bits))
                    .ok_or_else(|| {
                        RpcError::not_found(
                            format!("{bits} is not a live entity"),
                            "use scene.query_entities",
                        )
                    })
            })
            .collect();
    }
    Ok(world
        .get_resource::<Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default())
}

/// `editor.vertex_snap`.
fn vertex_snap_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = if params.get("entity").is_some() {
        crate::undo_agent::live_entity(world, params, "entity")?
    } else {
        world
            .get_resource::<Selection>()
            .and_then(Selection::primary)
            .ok_or_else(|| {
                refused(
                    "vertex_snap_no_entity",
                    "name `entity` or select one",
                    "editor.select",
                )
            })?
    };
    let coord = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (pixels) is required")))
    };
    let pointer = Vec2::new(coord("x")?, coord("y")?);
    refresh_world_state(world);
    let (view, size) = crate::handles::pane_view(world, params)?;
    let Some(snap) = vertex_snap_at(world, &view, size, pointer, &[entity]) else {
        return Ok(json!({ "snapped": false, "entity": entity.to_bits() }));
    };
    let apply = params.get("apply").and_then(Value::as_bool).unwrap_or(true);
    let mut out = json!({
        "snapped": true,
        "entity": entity.to_bits(),
        "position": snap.position.to_array(),
        "vertex": snap.vertex,
        "triangle": snap.hit.triangle,
        "target": snap.hit.entity.to_bits(),
        "target_owner": snap.hit.owner.to_bits(),
        "applied": false,
    });
    if !apply {
        return Ok(out);
    }
    let Some(current) = world.get::<Transform>(entity).copied() else {
        return Err(refused(
            "vertex_snap_no_transform",
            "the entity has no Transform",
            "add one, or snap another entity",
        ));
    };
    let Some(translation) = local_translation_for(world, entity, snap.position) else {
        return Err(refused(
            "vertex_snap_bad_parent",
            "the entity's parent has a zero scale",
            "fix the parent's scale",
        ));
    };
    let mut command = SetTransform::capture(
        world,
        entity,
        Transform {
            translation,
            ..current
        },
    );
    command.apply(world);
    let mut group = CommandGroup::new("Vertex snap");
    group.push(Box::new(command));
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(group));
    refresh_world_state(world);
    out["applied"] = json!(true);
    out["translation"] = json!(translation.to_array());
    Ok(out)
}

/// `editor.drop_to_floor`.
fn drop_to_floor_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = selection_or_param(world, params)?;
    if entities.is_empty() {
        return Err(refused(
            "drop_no_selection",
            "nothing to drop: name `entities` or select some",
            "editor.select",
        ));
    }
    let align = params.get("align").and_then(Value::as_bool).unwrap_or_else(|| {
        world
            .get_resource::<PlacementOptions>()
            .is_some_and(|o| o.align_to_normal)
    });
    let report = drop_to_floor(world, &entities, align);
    if report.placed.is_empty() {
        return Err(refused(
            "drop_no_surface",
            "no mesh or terrain below the entities, or they cannot move",
            "move them over a surface; a collider with no mesh is not a surface",
        ));
    }
    Ok(json!({
        "placed": report.placed.iter().map(|p| json!({
            "entity": p.entity.to_bits(),
            "from": p.from.to_array(),
            "to": p.to.to_array(),
            "surface": p.surface.to_bits(),
            "normal": p.normal.to_array(),
            "translation": p.transform.translation.to_array(),
            "rotation": p.transform.rotation.to_array(),
        })).collect::<Vec<_>>(),
        "missed": report.missed.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "align": align,
        "undoable": report.undoable,
    }))
}

/// `editor.placement_options`.
fn placement_options_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut options = world
        .get_resource::<PlacementOptions>()
        .copied()
        .unwrap_or_default();
    if let Some(on) = params.get("align_to_normal").and_then(Value::as_bool) {
        options.align_to_normal = on;
    }
    if let Some(on) = params.get("vertex_snap").and_then(Value::as_bool) {
        options.vertex_snap = on;
    }
    // Surface snapping while dragging (ADR-7756).
    let mut surface = world.get_resource::<SurfaceSnap>().copied().unwrap_or_default();
    if let Some(offset) = params.get("surface_offset").filter(|v| !v.is_null()) {
        surface.offset = offset
            .as_f64()
            .map(|v| v as f32)
            .filter(|v| v.is_finite())
            .ok_or_else(|| RpcError::invalid_params("`surface_offset` must be a finite number of meters"))?;
    }
    if let Some(on) = params.get("surface_snap").and_then(Value::as_bool) {
        surface.enabled = on;
    }
    if let Some(on) = params.get("surface_align").and_then(Value::as_bool) {
        surface.align = on;
    }
    world.insert_resource(options);
    world.insert_resource(surface);
    Ok(json!({
        "align_to_normal": options.align_to_normal,
        "vertex_snap": options.vertex_snap,
        "surface_snap": surface.enabled,
        "surface_align": surface.align,
        "surface_offset": surface.offset,
    }))
}

/// The placement agent methods.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.vertex_snap",
                tool: "vertex_snap",
                description: "Vertex snap: casts a ray through a pane pixel with the entity's own meshes left out, takes the vertex of the hit triangle nearest the hit and moves the entity's pivot onto it (one undo step). snapped is false when the ray meets no mesh.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: false,
                        description: "The entity to move. Default: the primary selection.",
                    },
                    ParamSchema {
                        name: "x",
                        kind: "number",
                        required: true,
                        description: "Pointer pixel x in the width×height pane.",
                    },
                    ParamSchema {
                        name: "y",
                        kind: "number",
                        required: true,
                        description: "Pointer pixel y.",
                    },
                    ParamSchema {
                        name: "apply",
                        kind: "boolean",
                        required: false,
                        description: "Move the entity (default true); false only reports the vertex.",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "number",
                        required: false,
                        description: "Pane width in pixels. Default 640.",
                    },
                    ParamSchema {
                        name: "height",
                        kind: "number",
                        required: false,
                        description: "Pane height in pixels. Default 480.",
                    },
                    ParamSchema {
                        name: "view",
                        kind: "string",
                        required: false,
                        description: "Pane camera: perspective (default), top, front or side.",
                    },
                ],
                result: "{ snapped, entity, position?, vertex?, triangle?, target?, target_owner?, applied?, translation? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            vertex_snap_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.drop_to_floor",
                tool: "drop_to_floor",
                description: "Moves entities straight down onto the first mesh or terrain below each, using their bounds and vertices, as one undo step. With align, each also turns its up onto the surface normal.",
                params: &[
                    ParamSchema {
                        name: "entities",
                        kind: "array",
                        required: false,
                        description: "Entity ids. Default: the selection.",
                    },
                    ParamSchema {
                        name: "align",
                        kind: "boolean",
                        required: false,
                        description: "Align up to the surface normal. Default: the placement option.",
                    },
                ],
                result: "{ placed: [{ entity, from, to, surface, normal, translation, rotation }], missed, align, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            drop_to_floor_method as Handler,
        ),
        (
            MethodSchema {
                name: "editor.placement_options",
                tool: "placement_options",
                description: "Reads or sets the viewport's placement options: align drop-to-floor to the surface normal, vertex snap left on without holding its key, and surface snapping while dragging (with normal alignment and an offset along the normal).",
                params: &[
                    ParamSchema {
                        name: "align_to_normal",
                        kind: "boolean",
                        required: false,
                        description: "Drop-to-floor aligns to the surface normal.",
                    },
                    ParamSchema {
                        name: "vertex_snap",
                        kind: "boolean",
                        required: false,
                        description: "Vertex snap stays on while translating.",
                    },
                    ParamSchema {
                        name: "surface_snap",
                        kind: "boolean",
                        required: false,
                        description: "While translating, the pivot slides on the mesh or terrain under the pointer.",
                    },
                    ParamSchema {
                        name: "surface_align",
                        kind: "boolean",
                        required: false,
                        description: "Surface snap also turns the active entity's up onto the surface normal.",
                    },
                    ParamSchema {
                        name: "surface_offset",
                        kind: "number",
                        required: false,
                        description: "Surface snap lifts the pivot this many meters along the normal.",
                    },
                ],
                result: "{ align_to_normal, vertex_snap, surface_snap, surface_align, surface_offset }",
                mutating: false,
                permission: Permission::Read,
            },
            placement_options_method as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    use std::sync::Arc;

    use regia_render::{CpuMesh, LabMaterial};
    use regia_scene::GlobalTransform;

    fn spawn_box(world: &mut World, half: [f32; 3], transform: Transform) -> Entity {
        let (vertices, indices) = regia_render::box_mesh(half);
        world
            .spawn((
                transform,
                GlobalTransform(transform.compute_affine()),
                MeshRenderer::new(
                    Arc::new(CpuMesh { vertices, indices }),
                    Arc::new(LabMaterial::default()),
                ),
            ))
            .id()
    }

    #[test]
    fn a_surface_snap_pose_sits_on_the_hit_lifted_and_optionally_aligned() {
        let normal = Quat::from_rotation_z(0.3) * Vec3::Y;
        let hit = SurfacePoint {
            position: Vec3::new(1.0, 2.0, 3.0),
            normal,
            entity: Entity::PLACEHOLDER,
        };
        let frame = Transform {
            translation: Vec3::new(9.0, 9.0, 9.0),
            rotation: Quat::from_rotation_y(0.5),
            scale: Vec3::splat(2.0),
        };
        let plain = surface_snap_pose(
            &frame,
            &hit,
            &SurfaceSnap {
                enabled: true,
                align: false,
                offset: 0.0,
            },
        );
        assert_eq!(plain.translation, hit.position);
        assert_eq!(plain.rotation, frame.rotation);
        assert_eq!(plain.scale, frame.scale);
        let lifted = surface_snap_pose(
            &frame,
            &hit,
            &SurfaceSnap {
                enabled: true,
                align: true,
                offset: 0.5,
            },
        );
        assert!((lifted.translation - (hit.position + normal * 0.5)).length() < 1e-6);
        assert!((lifted.rotation * Vec3::Y - normal).length() < 1e-5);
        // The turn is the shortest arc: the heading about the normal is kept.
        assert!((lifted.rotation * Vec3::Z).dot(frame.rotation * Vec3::Z) > 0.9);
    }

    #[test]
    fn the_triangle_vertex_nearest_the_hit_wins() {
        let mut world = World::new();
        let moved = Transform {
            translation: Vec3::new(10.0, 0.0, 0.0),
            ..Transform::IDENTITY
        };
        let target = spawn_box(&mut world, [1.0, 1.0, 1.0], moved);
        let hit = raycast_entity(
            &world,
            target,
            Vec3::new(10.9, 0.95, 5.0),
            Vec3::NEG_Z,
            FaceCull::None,
        )
        .unwrap();
        let (_, position) = nearest_triangle_vertex(&world, &hit).unwrap();
        assert_eq!(position, Vec3::new(11.0, 1.0, 1.0));
    }

    #[test]
    fn a_parent_moves_the_local_translation() {
        let mut world = World::new();
        let parent = world
            .spawn(Transform {
                translation: Vec3::new(1.0, 2.0, 3.0),
                rotation: Quat::IDENTITY,
                scale: Vec3::splat(2.0),
            })
            .id();
        let child = world.spawn((Transform::IDENTITY, ChildOf(parent))).id();
        assert_eq!(
            local_translation_for(&world, child, Vec3::new(3.0, 2.0, 3.0)),
            Some(Vec3::new(1.0, 0.0, 0.0))
        );
        world.entity_mut(parent).insert(Transform {
            scale: Vec3::ZERO,
            ..Transform::IDENTITY
        });
        assert_eq!(local_translation_for(&world, child, Vec3::ONE), None);
    }

    #[test]
    fn a_body_without_meshes_or_colliders_drops_its_pivot() {
        let mut world = World::new();
        spawn_box(
            &mut world,
            [5.0, 0.5, 5.0],
            Transform {
                translation: Vec3::new(0.0, -0.5, 0.0),
                ..Transform::IDENTITY
            },
        );
        let marker = world
            .spawn(Transform {
                translation: Vec3::new(0.5, 3.0, 0.5),
                ..Transform::IDENTITY
            })
            .id();
        let report = drop_to_floor(&mut world, &[marker], false);
        assert_eq!(report.placed.len(), 1);
        assert!(world.get::<Transform>(marker).unwrap().translation.y.abs() < 1e-3);
    }
}
