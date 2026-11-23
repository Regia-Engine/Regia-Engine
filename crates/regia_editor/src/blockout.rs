// SPDX-License-Identifier: MPL-2.0
//! Ordered blockout brushes, background preview and undoable GLB baking (ADR-7809).
//! The editor owns these authoring components; a bake produces ordinary runtime mesh assets.

use crate::command::{CommandGroup, DespawnEntities, EditorCommand, SpawnEntities};
pub use crate::csg::{BrushOp, BrushShape};
use crate::editor_jobs::WorkerJob;
use crate::selection::{SelectMode, Selection};
use glam::DMat4;
use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::id::Guid;
use regia_core::math::{Mat4, Vec3};
use regia_ecs::prelude::*;
use regia_render::runtime::{CpuMaterial, CpuMesh};
use regia_render::{MeshRenderer, MeshSource};
use regia_scene::prelude::{EntityGuid, Transform};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A saved editor brush. The containing scene's open component map needs no layout change.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct BlockoutBrush {
    /// Primitive solid, centered on the entity transform.
    pub shape: BrushShape,
    /// Boolean operation against earlier brushes in the group.
    pub op: BrushOp,
    /// Evaluation order; ties use the entity's stable GUID.
    pub order: i32,
    /// Group name. Only brushes with the same name combine.
    pub group: String,
    /// Full dimensions in meters (X, Y, Z).
    pub size: Vec3,
    /// Stair count, 1 through 64.
    pub steps: u32,
    /// Material slot inherited by every face, including cut faces, 0 through 255.
    pub material: u32,
}
impl Default for BlockoutBrush {
    fn default() -> Self {
        Self {
            shape: BrushShape::Box,
            op: BrushOp::Add,
            order: 0,
            group: "Blockout".into(),
            size: Vec3::splat(2.0),
            steps: 6,
            material: 0,
        }
    }
}

/// Registers authoring components before a scene loads.
pub fn register(world: &mut World) {
    let registry = world
        .get_resource_or_insert_with(AppTypeRegistry::default)
        .clone();
    let mut registry = registry.write();
    registry.register::<BlockoutBrush>();
    registry.register::<BrushShape>();
    registry.register::<BrushOp>();
}

fn valid(brush: &BlockoutBrush) -> bool {
    !brush.group.trim().is_empty()
        && brush.group.len() <= 128
        && brush.size.is_finite()
        && brush.size.min_element() > 1e-5
        && brush.size.max_element() <= 10_000.0
        && (1..=64).contains(&brush.steps)
        && brush.material <= 255
}

/// Creates a brush at `position`, selects it and records one shared undo entry.
///
/// # Errors
/// Invalid dimensions/group, missing scene types or failed reflected serialization.
pub fn add(world: &mut World, brush: BlockoutBrush, position: Vec3) -> Result<Entity, String> {
    if !valid(&brush) || !position.is_finite() || position.abs().max_element() > 1e6 {
        return Err("blockout_invalid_brush".into());
    }
    register(world);
    let guid = Guid::new_random();
    let mut command = SpawnEntities::new_entity(
        world,
        guid,
        &format!("{:?} brush", brush.shape),
        Transform::from_translation(position),
    )
    .and_then(|cmd| cmd.with_component(world, &brush))
    .map_err(|e| e.to_string())?;
    command.apply(world);
    crate::undo_agent::push_discrete(world, Box::new(command));
    let entity = crate::hierarchy_ops::guid_entity(world, guid).ok_or("blockout_spawn_failed")?;
    world
        .get_resource_or_insert_with(Selection::default)
        .select(entity, SelectMode::Replace);
    Ok(entity)
}

struct EditBrush {
    entity: u64,
    old: BlockoutBrush,
    new: BlockoutBrush,
}
impl EditorCommand for EditBrush {
    fn apply(&mut self, world: &mut World) {
        if let Some(e) = crate::command::resolve_entity(world, self.entity) {
            world.entity_mut(e).insert(self.new.clone());
        }
    }
    fn revert(&mut self, world: &mut World) {
        if let Some(e) = crate::command::resolve_entity(world, self.entity) {
            world.entity_mut(e).insert(self.old.clone());
        }
    }
    fn merge(&mut self, _next: &dyn EditorCommand) -> bool {
        false
    }
    fn description(&self) -> String {
        "Edit blockout brush".into()
    }
}

/// Changes a brush through the same operation as keyboard fields and handles.
///
/// # Errors
/// Missing brush or invalid parameters. No mutation on failure.
pub fn edit(world: &mut World, entity: Entity, new: BlockoutBrush) -> Result<(), String> {
    if !valid(&new) {
        return Err("blockout_invalid_brush".into());
    }
    let old = world
        .get::<BlockoutBrush>(entity)
        .cloned()
        .ok_or("blockout_missing")?;
    if old == new {
        return Ok(());
    }
    let mut command = EditBrush {
        entity: entity.to_bits(),
        old,
        new,
    };
    command.apply(world);
    crate::undo_agent::push_discrete(world, Box::new(command));
    Ok(())
}

fn matrix(world: &World, entity: Entity) -> Mat4 {
    Mat4::from(crate::hierarchy_ops::world_affine(world, entity))
}

#[derive(Clone, Debug, PartialEq)]
struct Record {
    entity: u64,
    guid: String,
    input: crate::csg::Input,
}
fn groups(world: &mut World) -> BTreeMap<String, Vec<Record>> {
    let mut groups = BTreeMap::<String, Vec<Record>>::new();
    let brushes: Vec<_> = world
        .query::<(Entity, &BlockoutBrush, &EntityGuid)>()
        .iter(world)
        .map(|(entity, brush, guid)| (entity, brush.clone(), guid.0.to_string()))
        .collect();
    for (entity, b, guid) in brushes {
        let transform = DMat4::from_cols_array(&matrix(world, entity).to_cols_array().map(f64::from));
        groups.entry(b.group.clone()).or_default().push(Record {
            entity: entity.to_bits(),
            guid,
            input: crate::csg::Input {
                shape: b.shape,
                op: b.op,
                order: b.order,
                size: b.size.as_dvec3(),
                steps: b.steps,
                transform,
                material: b.material,
            },
        });
    }
    for records in groups.values_mut() {
        records.sort_by(|a, b| (a.input.order, &a.guid).cmp(&(b.input.order, &b.guid)));
    }
    groups
}

#[derive(Default)]
struct GroupState {
    records: Vec<Record>,
    stable: u8,
    attempted: bool,
    preview: Option<Entity>,
    error: Option<String>,
}
struct Bake {
    out: PathBuf,
    temp: PathBuf,
    relative: String,
    collider: bool,
    guid: Guid,
}
struct Pending {
    group: String,
    records: Vec<Record>,
    bake: Option<Bake>,
    job: WorkerJob<Result<crate::csg::Mesh, String>>,
}
#[derive(Resource, Default)]
struct State {
    groups: BTreeMap<String, GroupState>,
    pending: Option<Pending>,
}

fn preview_mesh(mesh: &crate::csg::Mesh) -> CpuMesh {
    let mut vertices = Vec::with_capacity(mesh.triangles.len() * 3);
    for triangle in &mesh.triangles {
        let p = triangle.map(|i| mesh.positions[i as usize]);
        let normal = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        let tangent = normal.any_orthonormal_vector();
        for point in p {
            vertices.push(regia_render::LabVertex {
                position: point.as_vec3().to_array(),
                normal: normal.as_vec3().to_array(),
                uv: [point.x as f32, point.z as f32],
                tangent: [tangent.x as f32, tangent.y as f32, tangent.z as f32, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    CpuMesh {
        indices: (0..vertices.len() as u32).collect(),
        vertices,
    }
}

fn show_preview(world: &mut World, mesh: &crate::csg::Mesh, old: Option<Entity>) -> Option<Entity> {
    if let Some(e) = old {
        let _ = world.despawn(e);
    }
    if mesh.triangles.is_empty() {
        return None;
    }
    Some(
        world
            .spawn((
                crate::editor_only::EditorOnly,
                crate::visibility::EditorHidden,
                // Like resolved model parts, this is a render helper, not authored content.
                // A local Transform would make ensure_scene_identity assign a saved GUID.
                regia_scene::GlobalTransform::default(),
                MeshRenderer::new(Arc::new(preview_mesh(mesh)), Arc::new(CpuMaterial::default())),
            ))
            .id(),
    )
}

fn start(
    world: &mut World,
    group: String,
    records: Vec<Record>,
    bake: Option<Bake>,
) -> Result<Pending, String> {
    let inputs: Vec<_> = records.iter().map(|r| r.input.clone()).collect();
    let temp = bake.as_ref().map(|b| b.temp.clone());
    let root = regia_agent::req_path::project_root(world);
    let job = WorkerJob::spawn(
        world,
        "blockout",
        "blockout",
        format!("Evaluate {group}"),
        "regia-blockout",
        move |cx| {
            let mesh = crate::csg::evaluate_cancellable(&inputs, &|| cx.cancelled())?;
            if let Some(path) = temp {
                if mesh.triangles.is_empty() {
                    return Err("blockout_empty".into());
                }
                cx.progress(Some(0.8), "Writing mesh");
                regia_agent::req_path::check_derived(&root, "out", &path).map_err(|e| e.message)?;
                let bytes = crate::export_glb::blockout_glb(&mesh)?;
                regia_asset_pipeline::scene_export::write_glb(&path, &bytes).map_err(|e| e.to_string())?;
            }
            Ok(mesh)
        },
    )?;
    Ok(Pending {
        group,
        records,
        bake,
        job,
    })
}

fn has_other_children(world: &World, records: &[Record]) -> bool {
    records.iter().any(|record| {
        world
            .get::<Children>(Entity::from_bits(record.entity))
            .is_some_and(|children| {
                children
                    .iter()
                    .any(|child| !records.iter().any(|r| r.entity == child.to_bits()))
            })
    })
}

/// Starts an asynchronous bake. The group changes only after its worker succeeds and the
/// input is still current. Undo restores all brushes; the generated asset stays for redo.
///
/// # Errors
/// Unknown/empty group, a pending bake, an invalid/outside path or an existing output file.
pub fn bake(world: &mut World, group: &str, out: Option<&str>, collider: bool) -> Result<String, RpcError> {
    let records = groups(world)
        .remove(group)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| refused("blockout_missing", "No brushes in this group"))?;
    if has_other_children(world, &records) {
        return Err(refused(
            "blockout_hierarchy",
            "Detach children outside this blockout group before baking",
        ));
    }
    let mut state = world.remove_resource::<State>().unwrap_or_default();
    if state.pending.is_some() {
        world.insert_resource(state);
        return Err(refused(
            "blockout_busy",
            "Wait for the current blockout job before baking",
        ));
    }
    let guid = Guid::new_random();
    let relative = out
        .map(str::to_owned)
        .unwrap_or_else(|| format!("assets/blockout/{guid}.glb"));
    let prepare = || -> Result<Bake, RpcError> {
        if !relative.starts_with("assets/") || !relative.ends_with(".glb") {
            return Err(refused(
                "blockout_path",
                "Output must be an assets/ path ending in .glb",
            ));
        }
        let path = regia_agent::req_path::project_path(world, "out", &relative)?;
        if path.exists() {
            return Err(refused("blockout_exists", "Choose a new asset path"));
        }
        let temp = path.with_file_name(format!(".blockout-{guid}.glb.tmp"));
        Ok(Bake {
            out: path,
            temp,
            relative,
            collider,
            guid,
        })
    };
    let result = prepare().and_then(|b| {
        start(world, group.into(), records, Some(b)).map_err(|e| refused("blockout_failed", e))
    });
    let result = match result {
        Ok(pending) => {
            let id = pending.job.id.clone();
            state.pending = Some(pending);
            Ok(id)
        }
        Err(e) => Err(e),
    };
    world.insert_resource(state);
    result
}

fn finish_bake(
    world: &mut World,
    records: &[Record],
    bake: &Bake,
    mesh: &crate::csg::Mesh,
) -> Result<Value, String> {
    // Children can be attached while the worker runs. Refuse before publishing or deleting.
    if has_other_children(world, records) {
        return Err("blockout_hierarchy: Detach children outside this group before baking".into());
    }
    let root = regia_agent::req_path::project_root(world);
    regia_agent::req_path::check_derived(&root, "out", &bake.out).map_err(|e| e.message)?;
    let mut spawned = SpawnEntities::new_entity(world, bake.guid, "Baked blockout", Transform::default())
        .and_then(|cmd| cmd.with_component(world, &MeshSource::new(&bake.relative, "")))
        .map_err(|e| e.to_string())?;
    if bake.collider {
        let collider = regia_physics::Collider {
            shape: regia_physics::ColliderShape::TriMesh {
                vertices: mesh.positions.iter().map(|p| p.as_vec3()).collect(),
                indices: mesh.triangles.clone(),
            },
            ..Default::default()
        };
        spawned = spawned
            .with_component(world, &collider)
            .map_err(|e| e.to_string())?;
    }
    // An atomic no-replace publication: a competing writer never loses its output.
    std::fs::hard_link(&bake.temp, &bake.out).map_err(|e| e.to_string())?;
    let mut command = CommandGroup::new("Bake blockout to mesh");
    command.push(Box::new(DespawnEntities {
        entities: records.iter().map(|r| r.entity).collect(),
        snapshot: None,
    }));
    command.push(Box::new(spawned));
    command.apply(world);
    crate::undo_agent::push_discrete(world, Box::new(command));
    let entity = crate::hierarchy_ops::guid_entity(world, bake.guid).ok_or("blockout_spawn_failed")?;
    world
        .get_resource_or_insert_with(Selection::default)
        .select(entity, SelectMode::Replace);
    Ok(
        json!({"entity":entity.to_bits(),"out":bake.relative,"triangles":mesh.triangles.len(),"collider":bake.collider}),
    )
}

/// Polls worker completion and schedules changed groups after two unchanged frames.
/// Called by the editor's existing job service, once per engine frame.
pub fn tick(world: &mut World) {
    let _span = tracing::info_span!("blockout_tick").entered();
    let current = groups(world);
    if current.is_empty() && !world.contains_resource::<State>() {
        return;
    }
    let mut state = world.remove_resource::<State>().unwrap_or_default();
    if let Some(mut pending) = state.pending.take() {
        let unchanged = current.get(&pending.group) == Some(&pending.records);
        if !unchanged {
            pending.job.cancel();
        }
        pending
            .job
            .sync_row(world, "Blockout", pending.job.context().fraction());
        if let Some(outcome) = pending.job.take() {
            let cancelled = pending.job.context().cancelled() || !unchanged;
            let result = if cancelled {
                Ok((json!({"discarded":true}), true))
            } else {
                match outcome.and_then(|r| r) {
                    Ok(mesh) => {
                        if let Some(bake) = &pending.bake {
                            finish_bake(world, &pending.records, bake, &mesh)
                                .map(|v| (v, false))
                                .map_err(|e| ("blockout_failed", e))
                        } else {
                            let entry = state.groups.entry(pending.group.clone()).or_default();
                            entry.preview = show_preview(world, &mesh, entry.preview.take());
                            entry.error = None;
                            Ok((json!({"triangles":mesh.triangles.len()}), false))
                        }
                    }
                    Err(e) => {
                        state.groups.entry(pending.group.clone()).or_default().error = Some(e.clone());
                        Err(("blockout_failed", e))
                    }
                }
            };
            pending.job.end(world, result);
            if let Some(b) = pending.bake {
                let _ = std::fs::remove_file(b.temp);
            }
        } else {
            state.pending = Some(pending);
        }
    }
    // Re-read after a completed bake removes brushes.
    let current = groups(world);
    state.groups.retain(|group, entry| {
        if current.contains_key(group) {
            true
        } else {
            if let Some(e) = entry.preview.take() {
                let _ = world.despawn(e);
            }
            false
        }
    });
    for (group, records) in current {
        let entry = state.groups.entry(group.clone()).or_default();
        if entry.records != records {
            entry.records = records.clone();
            entry.stable = 0;
            entry.attempted = false;
        } else {
            entry.stable = entry.stable.saturating_add(1);
        }
        if entry.stable >= 2 && !entry.attempted && state.pending.is_none() {
            entry.attempted = true;
            match start(world, group, records, None) {
                Ok(p) => state.pending = Some(p),
                Err(e) => entry.error = Some(e),
            }
        }
    }
    world.insert_resource(state);
}

/// Joins the current worker without applying its outcome. For deterministic tests and host
/// shutdown only; interactive callers poll `agent.job_status` while the frame service ticks.
pub fn wait_for_worker(world: &mut World) {
    if let Some(mut state) = world.get_resource_mut::<State>()
        && let Some(pending) = &mut state.pending
    {
        pending.job.wait();
    }
}

fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(
        regia_agent::protocol::INVALID_PARAMS,
        reason,
        message,
        "See book/agents/blockout.md",
    )
}
fn shape(text: &str) -> Option<BrushShape> {
    Some(match text {
        "box" => BrushShape::Box,
        "cylinder" => BrushShape::Cylinder,
        "wedge" => BrushShape::Wedge,
        "stairs" => BrushShape::Stairs,
        "sphere" => BrushShape::Sphere,
        _ => return None,
    })
}
fn op(text: &str) -> Option<BrushOp> {
    Some(match text {
        "add" => BrushOp::Add,
        "subtract" => BrushOp::Subtract,
        "intersect" => BrushOp::Intersect,
        _ => return None,
    })
}
fn vector(value: &Value) -> Result<Vec3, RpcError> {
    let a = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| refused("blockout_params", "Expected three finite numbers"))?;
    let mut result = [0.0; 3];
    for (dst, v) in result.iter_mut().zip(a) {
        *dst = v
            .as_f64()
            .filter(|v| v.is_finite() && v.abs() <= 1e6)
            .ok_or_else(|| refused("blockout_params", "Expected three finite numbers"))?
            as f32;
    }
    Ok(Vec3::from_array(result))
}
fn patch(mut b: BlockoutBrush, p: &Value) -> Result<BlockoutBrush, RpcError> {
    if let Some(v) = p.get("shape") {
        b.shape = v
            .as_str()
            .and_then(shape)
            .ok_or_else(|| refused("blockout_params", "Unknown brush shape"))?;
    }
    if let Some(v) = p.get("op") {
        b.op = v
            .as_str()
            .and_then(op)
            .ok_or_else(|| refused("blockout_params", "Unknown brush operation"))?;
    }
    if let Some(v) = p.get("group") {
        b.group = v
            .as_str()
            .ok_or_else(|| refused("blockout_params", "group must be a string"))?
            .into();
    }
    if let Some(v) = p.get("size") {
        b.size = vector(v)?;
    }
    if let Some(v) = p.get("steps") {
        b.steps = v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| refused("blockout_params", "steps must be 1..64"))?;
    }
    if let Some(v) = p.get("material") {
        b.material = v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| refused("blockout_params", "material must be 0..255"))?;
    }
    if let Some(v) = p.get("order") {
        b.order = v
            .as_i64()
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| refused("blockout_params", "order must be a signed 32-bit integer"))?;
    }
    if !valid(&b) {
        return Err(refused(
            "blockout_invalid_brush",
            "Invalid dimensions, steps, material or group",
        ));
    }
    Ok(b)
}
fn add_method(world: &mut World, p: &Value) -> Result<Value, RpcError> {
    if p.get("shape").is_none() || p.get("op").is_none() {
        return Err(refused("blockout_params", "shape and op are required"));
    }
    let mut default = BlockoutBrush::default();
    if let Some(selected) = world
        .get_resource::<Selection>()
        .and_then(Selection::primary)
        .and_then(|e| world.get::<BlockoutBrush>(e))
    {
        default.group = selected.group.clone();
    }
    let mut b = patch(default, p)?;
    if p.get("order").is_none() {
        b.order = groups(world)
            .get(&b.group)
            .and_then(|r| r.iter().map(|r| r.input.order).max())
            .map_or(Some(0), |i| i.checked_add(1))
            .ok_or_else(|| {
                refused(
                    "blockout_invalid_brush",
                    "No evaluation order remains; specify order explicitly",
                )
            })?;
    }
    let position = p.get("translation").map(vector).transpose()?.unwrap_or_else(|| {
        world
            .get_resource::<crate::viewport::EditorCamera>()
            .map_or(Vec3::ZERO, |c| c.target)
    });
    let group = b.group.clone();
    let entity = add(world, b, position).map_err(|e| refused("blockout_failed", e))?;
    Ok(json!({"entity":entity.to_bits(),"group":group}))
}
fn edit_method(world: &mut World, p: &Value) -> Result<Value, RpcError> {
    let entity = crate::undo_agent::live_entity(world, p, "entity")?;
    let old = world
        .get::<BlockoutBrush>(entity)
        .cloned()
        .ok_or_else(|| refused("blockout_missing", "Entity has no blockout brush"))?;
    edit(world, entity, patch(old, p)?).map_err(|e| refused("blockout_failed", e))?;
    Ok(json!({"entity":entity.to_bits()}))
}
fn bake_method(world: &mut World, p: &Value) -> Result<Value, RpcError> {
    let group = p
        .get("group")
        .and_then(Value::as_str)
        .ok_or_else(|| refused("blockout_params", "group is required"))?;
    let out = p
        .get("out")
        .map(|v| {
            v.as_str()
                .ok_or_else(|| refused("blockout_params", "out must be a string"))
        })
        .transpose()?;
    let collider = p
        .get("collider")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| refused("blockout_params", "collider must be a boolean"))
        })
        .transpose()?
        .unwrap_or(false);
    bake(world, group, out, collider).map(|job| json!({"job":job}))
}

/// The blockout agent contract and dispatcher handlers.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    const fn p(
        name: &'static str,
        kind: &'static str,
        required: bool,
        description: &'static str,
    ) -> ParamSchema {
        ParamSchema {
            name,
            kind,
            required,
            description,
        }
    }
    const FIELDS: &[ParamSchema] = &[
        p("entity", "integer", true, "Brush entity bits"),
        p("shape", "string", false, "box, cylinder, wedge, stairs or sphere"),
        p("op", "string", false, "add, subtract or intersect"),
        p("group", "string", false, "Group name"),
        p("size", "array", false, "Full X/Y/Z dimensions"),
        p("steps", "integer", false, "Stair count, 1..64"),
        p("order", "integer", false, "Evaluation order"),
        p("material", "integer", false, "Face slot, 0..255"),
    ];
    const ADD_FIELDS: &[ParamSchema] = &[
        p("shape", "string", true, "box, cylinder, wedge, stairs or sphere"),
        p("op", "string", true, "add, subtract or intersect"),
        p(
            "group",
            "string",
            false,
            "Default selected brush group, else Blockout",
        ),
        p("size", "array", false, "Full X/Y/Z dimensions, default 2 meters"),
        p("steps", "integer", false, "Stair count, default 6"),
        p("order", "integer", false, "Default after the last brush"),
        p("material", "integer", false, "Face slot, default 0"),
        p("translation", "array", false, "Default viewport focus point"),
    ];
    const BAKE_FIELDS: &[ParamSchema] = &[
        p("group", "string", true, "Group name"),
        p(
            "out",
            "string",
            false,
            "New assets/ path ending in .glb; defaults to a unique assets/blockout path",
        ),
        p(
            "collider",
            "boolean",
            false,
            "Add a static triangle mesh collider, default false",
        ),
    ];
    vec![
        (
            MethodSchema {
                name: "editor.blockout_add",
                tool: "blockout_add",
                description: "Create and select an ordered blockout brush in one undo step. Preview runs in a debounced editor job.",
                params: ADD_FIELDS,
                result: "{ entity, group }",
                mutating: true,
                permission: Permission::EditScene,
            },
            add_method,
        ),
        (
            MethodSchema {
                name: "editor.blockout_edit",
                tool: "blockout_edit",
                description: "Edit a blockout brush, as its numeric fields and grips do, in one undo step.",
                params: FIELDS,
                result: "{ entity }",
                mutating: true,
                permission: Permission::EditScene,
            },
            edit_method,
        ),
        (
            MethodSchema {
                name: "editor.blockout_bake",
                tool: "blockout_bake",
                description: "Bake a group to a new GLB asset on a worker. Poll agent.job_status. An unchanged group is replaced by one placed mesh in one undo step; undo restores brushes and keeps the asset for redo.",
                params: BAKE_FIELDS,
                result: "{ job }",
                mutating: true,
                permission: Permission::Assets,
            },
            bake_method,
        ),
    ]
}

#[derive(Resource, Default)]
struct UiState {
    op: BrushOp,
    collider: bool,
    last_edit: Option<u64>,
    drag: Option<(u64, BlockoutBrush)>,
}

fn once(world: &mut World, frame: u64) -> bool {
    let mut state = world.get_resource_or_insert_with(UiState::default);
    if state.last_edit == Some(frame) {
        return false;
    }
    state.last_edit = Some(frame);
    true
}

/// Contents of Create > Blockout. All fields are normal focusable egui widgets; the numeric
/// fields are also the keyboard alternative to the viewport grips. Logical points follow
/// the editor's `appearance.ui_scale` zoom; all visuals come from the active egui theme.
pub fn menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let mut toast = None;
    let mut operation = world.get_resource_or_insert_with(UiState::default).op;
    egui::ComboBox::from_id_salt("blockout_new_operation")
        .selected_text(format!("{operation:?}"))
        .show_ui(ui, |ui| {
            for (value, label) in [
                (BrushOp::Add, "Add"),
                (BrushOp::Subtract, "Subtract"),
                (BrushOp::Intersect, "Intersect"),
            ] {
                ui.selectable_value(&mut operation, value, label);
            }
        });
    world.resource_mut::<UiState>().op = operation;
    for kind in ["box", "cylinder", "wedge", "stairs", "sphere"] {
        if crate::agent_parity::action_button_text(
            ui,
            &crate::agent_parity::CREATE_BLOCKOUT,
            crate::shapes::shape_label(kind),
        )
        .clicked()
            && once(world, ui.ctx().cumulative_frame_nr())
        {
            let op = match operation {
                BrushOp::Add => "add",
                BrushOp::Subtract => "subtract",
                BrushOp::Intersect => "intersect",
            };
            toast = Some(match add_method(world, &json!({"shape":kind,"op":op})) {
                Ok(_) => "Blockout brush created".into(),
                Err(e) => e.message,
            });
        }
    }
    if let Some(entity) = world.get_resource::<Selection>().and_then(Selection::primary)
        && world.get::<BlockoutBrush>(entity).is_some()
    {
        ui.separator();
        if let Err(error) = fields(ui, world, entity) {
            toast = Some(error);
        }
        let group = world
            .get::<BlockoutBrush>(entity)
            .map(|b| b.group.clone())
            .unwrap_or_default();
        let mut collider = world.resource::<UiState>().collider;
        egui::ComboBox::from_id_salt("blockout_collision")
            .selected_text(if collider {
                "Mesh and collider"
            } else {
                "Mesh only"
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut collider, false, "Mesh only");
                ui.selectable_value(&mut collider, true, "Mesh and collider");
            });
        world.resource_mut::<UiState>().collider = collider;
        if let Some(error) = world
            .get_resource::<State>()
            .and_then(|s| s.groups.get(&group))
            .and_then(|g| g.error.as_ref())
        {
            ui.label(error);
        }
    }
    let selected_group = world
        .get_resource::<Selection>()
        .and_then(Selection::primary)
        .and_then(|e| world.get::<BlockoutBrush>(e))
        .map(|b| b.group.clone());
    if crate::agent_parity::action_enabled(
        ui,
        selected_group.is_some(),
        &crate::agent_parity::BAKE_BLOCKOUT,
        None,
    )
    .clicked()
        && once(world, ui.ctx().cumulative_frame_nr())
        && let Some(group) = selected_group
    {
        let collider = world.resource::<UiState>().collider;
        toast = Some(match bake(world, &group, None, collider) {
            Ok(id) => format!("Baking: {id}"),
            Err(e) => e.message,
        });
    }

    toast
}

fn fields(ui: &mut egui::Ui, world: &mut World, entity: Entity) -> Result<egui::Id, String> {
    let mut width_id = ui.id();
    let Some(mut brush) = world.get::<BlockoutBrush>(entity).cloned() else {
        return Ok(width_id);
    };
    let old = brush.clone();
    ui.label("Selected brush");
    ui.horizontal(|ui| {
        let label = ui.label("Group");
        ui.text_edit_singleline(&mut brush.group).labelled_by(label.id);
    });
    egui::ComboBox::from_id_salt("blockout_operation")
        .selected_text(format!("{:?}", brush.op))
        .show_ui(ui, |ui| {
            for (op, label) in [
                (BrushOp::Add, "Add"),
                (BrushOp::Subtract, "Subtract"),
                (BrushOp::Intersect, "Intersect"),
            ] {
                ui.selectable_value(&mut brush.op, op, label);
            }
        });
    for (axis, name) in ["Width", "Height", "Depth"].into_iter().enumerate() {
        ui.horizontal(|ui| {
            let label = ui.label(name);
            let response = ui
                .push_id(("blockout_size", axis), |ui| {
                    ui.add(
                        egui::DragValue::new(&mut brush.size[axis])
                            .range(0.001..=10_000.0)
                            .speed(0.1),
                    )
                })
                .inner;
            if axis == 0 {
                width_id = response.id;
            }
            response.labelled_by(label.id);
        });
    }
    ui.horizontal(|ui| {
        let l = ui.label("Steps");
        ui.add(egui::DragValue::new(&mut brush.steps).range(1..=64))
            .labelled_by(l.id);
    });
    ui.horizontal(|ui| {
        let l = ui.label("Order");
        ui.add(egui::DragValue::new(&mut brush.order)).labelled_by(l.id);
    });
    ui.horizontal(|ui| {
        let l = ui.label("Material slot");
        ui.add(egui::DragValue::new(&mut brush.material).range(0..=255))
            .labelled_by(l.id);
    });
    if brush != old && valid(&brush) && once(world, ui.ctx().cumulative_frame_nr()) {
        edit(world, entity, brush)?;
    }
    Ok(width_id)
}

fn end_drag(world: &mut World) {
    let drag = world
        .get_resource_mut::<UiState>()
        .and_then(|mut s| s.drag.take());
    if let Some((bits, old)) = drag
        && let Some(entity) = crate::command::resolve_entity(world, bits)
        && let Some(new) = world.get::<BlockoutBrush>(entity).cloned()
    {
        world.entity_mut(entity).insert(old);
        if let Err(error) = edit(world, entity, new) {
            tracing::warn!(%error,"blockout handle edit failed");
        }
    }
}

/// Whether a selected brush uses the viewport's shared handle-tool toggle.
#[must_use]
pub fn handles_show(world: &World, entity: Entity) -> bool {
    world.get::<BlockoutBrush>(entity).is_some()
        && world
            .get_resource::<crate::collider_handles::ColliderHandleTool>()
            .is_some_and(|t| t.enabled)
}

/// Draws and edits brush size/step grips with the shared handle state machine. Colors are theme
/// tokens and hit radii use logical points. Create > Blockout provides every grip's keyboard path.
pub fn viewport_input(
    ui: &mut egui::Ui,
    world: &mut World,
    view: &crate::views::ViewCamera,
    rect: egui::Rect,
    pane: usize,
) -> bool {
    use crate::handles::{DragPhase, Handle, HandleState, PointerInput, SnapSet};
    let primary = world.get_resource::<Selection>().and_then(Selection::primary);
    let active = world
        .get_resource::<UiState>()
        .and_then(|s| s.drag.as_ref())
        .map(|d| d.0);
    if active.is_some_and(|bits| primary.is_none_or(|e| e.to_bits() != bits || !handles_show(world, e))) {
        end_drag(world);
    }
    let Some(entity) = primary.filter(|e| handles_show(world, *e)) else {
        return false;
    };
    let Some(brush) = world.get::<BlockoutBrush>(entity).cloned() else {
        return false;
    };
    if !valid(&brush) {
        return false;
    }
    let _span = tracing::info_span!("blockout_grips").entered();
    let transform = matrix(world, entity);
    if !transform.is_finite() || transform.determinant().abs() < 1e-12 {
        return false;
    }
    let mut handles = Vec::new();
    for axis in 0..3 {
        let mut p = Vec3::ZERO;
        p[axis] = brush.size[axis] * 0.5;
        let tip = transform.transform_point3(p);
        let base = transform.transform_point3(p * 0.75);
        handles.push(Handle::Arrow {
            origin: base,
            dir: tip - base,
        });
    }
    if brush.shape == BrushShape::Stairs {
        let base = transform.transform_point3(Vec3::new(brush.size.x * 0.6, 0.0, 0.0));
        let tip = transform.transform_point3(Vec3::new(brush.size.x * 0.6, brush.steps as f32 * 0.1, 0.0));
        handles.push(Handle::Arrow {
            origin: base,
            dir: tip - base,
        });
    }
    let id = ui.id().with(("blockout_grips", entity.to_bits(), pane));
    let mut state: HandleState = ui.ctx().data(|d| d.get_temp(id)).unwrap_or_default();
    let size = regia_core::math::Vec2::new(rect.width(), rect.height());
    let input = ui.input(|i| {
        let pos = i
            .pointer
            .latest_pos()
            .filter(|p| rect.contains(*p) || state.dragging());
        PointerInput {
            pos: pos.map(|p| regia_core::math::Vec2::new(p.x - rect.min.x, p.y - rect.min.y)),
            pressed: pos.is_some() && i.pointer.primary_pressed(),
            down: i.pointer.primary_down(),
        }
    });
    // egui can redraw a panel twice: advance a grip only once per cumulative frame.
    let frame = ui.ctx().cumulative_frame_nr();
    let frame_id = id.with("frame");
    let already = ui.ctx().data(|d| d.get_temp::<u64>(frame_id)) == Some(frame);
    let event = if already {
        None
    } else {
        state.update(view, size, input, &handles, &SnapSet::empty(), 10.0)
    };
    ui.ctx().data_mut(|d| {
        d.insert_temp(id, state);
        d.insert_temp(frame_id, frame);
    });
    let screen = |p| view.project(p, size).map(|p| rect.min + egui::vec2(p.x, p.y));
    let painter = ui.painter_at(rect);
    for (index, handle) in handles.iter().enumerate() {
        let color = if state.hovered() == Some(index) {
            ui.visuals().selection.stroke.color
        } else {
            ui.visuals().widgets.inactive.fg_stroke.color
        };
        if let Handle::Arrow { origin, dir } = *handle
            && let (Some(a), Some(b)) = (screen(origin), screen(origin + dir))
        {
            painter.line_segment([a, b], egui::Stroke::new(2.0, color));
            painter.circle_filled(b, 5.0, color);
        }
    }
    if let Some(event) = event {
        match event.phase {
            DragPhase::Hover => {}
            DragPhase::Begin => {
                world.get_resource_or_insert_with(UiState::default).drag =
                    Some((entity.to_bits(), brush.clone()));
            }
            DragPhase::Drag | DragPhase::End => {
                let point = transform.inverse().transform_point3(event.position);
                let mut next = brush;
                if event.handle < 3 {
                    next.size[event.handle] = (point[event.handle] * 2.0).clamp(0.001, 10_000.0);
                } else {
                    next.steps = (point.y * 10.0).round().clamp(1.0, 64.0) as u32;
                }
                if valid(&next) {
                    world.entity_mut(entity).insert(next);
                }
                if event.phase == DragPhase::End {
                    end_drag(world);
                }
            }
        }
    }
    state.dragging() || state.hovered().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        crate::agent_ext::install(app.world_mut());
        app
    }
    fn count(world: &mut World) -> usize {
        world.query::<&BlockoutBrush>().iter(world).count()
    }

    #[test]
    fn blockout_preview_debounces_and_idle_frames_start_no_jobs() {
        let mut app = app();
        let world = app.world_mut();
        let entity = add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
        tick(world);
        assert!(world.resource::<State>().pending.is_none());
        tick(world);
        assert!(world.resource::<State>().pending.is_none());
        tick(world);
        assert!(world.resource::<State>().pending.is_some());
        wait_for_worker(world);
        tick(world);
        assert!(world.resource::<State>().groups["Blockout"].preview.is_some());
        for _ in 0..8 {
            tick(world);
            assert!(world.resource::<State>().pending.is_none());
        }
        let mut changed = world.get::<BlockoutBrush>(entity).expect("brush").clone();
        changed.size.x = 3.0;
        edit(world, entity, changed).expect("edit");
        tick(world);
        tick(world);
        assert!(world.resource::<State>().pending.is_none());
        tick(world);
        assert!(world.resource::<State>().pending.is_some());
        wait_for_worker(world);
        tick(world);
        let preview = world.resource::<State>().groups["Blockout"]
            .preview
            .expect("preview");
        crate::visibility::ensure_scene_identity(world);
        assert!(
            world.get::<EntityGuid>(preview).is_none(),
            "preview is not saved content"
        );
        assert!(!crate::visibility::scene_content(world, &[]).contains(&preview));
        let mesh = &world.get::<MeshRenderer>(preview).expect("renderer").mesh;
        assert!(mesh.vertices.iter().any(|v| v.position[0] == 1.5));
    }

    #[test]
    fn blockout_successful_bake_publishes_and_undo_restores() {
        let root = regia_test_utils::unique_temp_dir("blockout_publish");
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
        bake(world, "Blockout", Some("assets/baked.glb"), true).expect("bake");
        wait_for_worker(world);
        tick(world);
        assert_eq!(count(world), 0);
        let bytes = std::fs::read(root.join("assets/baked.glb")).expect("published asset");
        let gltf = gltf::Gltf::from_slice(&bytes).expect("valid GLB");
        assert_eq!(gltf.meshes().count(), 1);
        assert_eq!(std::fs::read_dir(root.join("assets")).expect("files").count(), 1);
        assert!(crate::command::undo_world(world));
        assert_eq!(count(world), 1);
        assert!(root.join("assets/baked.glb").is_file(), "undo retains the asset");
        assert!(crate::command::redo_world(world));
        assert_eq!(count(world), 0);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn blockout_changed_bake_is_discarded_and_cleans_its_temp_file() {
        let root = regia_test_utils::unique_temp_dir("blockout_changed_bake");
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let entity = add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
        bake(world, "Blockout", Some("assets/changed.glb"), false).expect("bake");
        wait_for_worker(world);
        let temp = world
            .resource::<State>()
            .pending
            .as_ref()
            .expect("job")
            .bake
            .as_ref()
            .expect("bake")
            .temp
            .clone();
        assert!(temp.exists());
        assert_eq!(regia_asset::SourceKind::of(&temp), regia_asset::SourceKind::Other);
        let mut b = world.get::<BlockoutBrush>(entity).expect("brush").clone();
        b.size.y = 4.0;
        edit(world, entity, b).expect("edit");
        tick(world);
        assert_eq!(count(world), 1);
        assert!(!root.join("assets/changed.glb").exists());
        assert!(!temp.exists());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn blockout_cancelled_bake_never_publishes_and_existing_assets_are_preserved() {
        let root = regia_test_utils::unique_temp_dir("blockout_cancel_bake");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
        std::fs::write(root.join("assets/kept.glb"), b"existing asset").expect("existing");
        assert!(bake(world, "Blockout", Some("assets/kept.glb"), false).is_err());
        assert_eq!(
            std::fs::read(root.join("assets/kept.glb")).expect("kept"),
            b"existing asset"
        );
        let id = bake(world, "Blockout", Some("assets/cancelled.glb"), false).expect("bake");
        // Join first, then cancel: completion still must honor a cancel before publication.
        wait_for_worker(world);
        crate::editor_jobs::cancel_job(world, &json!({"id":id})).expect("cancel");
        tick(world);
        assert_eq!(count(world), 1);
        assert!(!root.join("assets/cancelled.glb").exists());
        assert_eq!(std::fs::read_dir(root.join("assets")).expect("files").count(), 1);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn blockout_bake_cannot_delete_a_non_group_child_added_during_the_job() {
        let root = regia_test_utils::unique_temp_dir("blockout_child_bake");
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(regia_agent::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let brush = add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
        bake(world, "Blockout", Some("assets/children.glb"), false).expect("bake");
        wait_for_worker(world);
        let child = world
            .spawn((EntityGuid::new_random(), Transform::default(), ChildOf(brush)))
            .id();
        tick(world);
        assert!(
            world.get_entity(child).is_ok(),
            "a group bake must preserve unrelated content"
        );
        assert_eq!(count(world), 1, "refuse this bake atomically");
        assert!(!root.join("assets/children.glb").exists());
        assert!(bake(world, "Blockout", Some("assets/children.glb"), false).is_err());
        assert!(world.get_entity(child).is_ok());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn blockout_fields_have_a_keyboard_resize_path_at_both_ui_scales() {
        for scale in [1.0, 1.75] {
            let mut app = app();
            let world = app.world_mut();
            let entity = add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(scale);
            let mut id = egui::Id::NULL;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                id = fields(ui, world, entity).expect("fields");
            });
            output.textures_delta.clear();
            ctx.memory_mut(|m| m.request_focus(id));
            let event = egui::Event::Key {
                key: egui::Key::ArrowUp,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![event],
                    ..Default::default()
                },
                |ui| {
                    fields(ui, world, entity).expect("keyboard fields");
                },
            );
            output.textures_delta.clear();
            assert!(
                world.get::<BlockoutBrush>(entity).expect("brush").size.x > 2.0,
                "scale {scale}"
            );
            assert!(crate::command::undo_world(world));
            assert_eq!(
                world.get::<BlockoutBrush>(entity).expect("undo brush").size.x,
                2.0
            );
        }
    }

    #[test]
    fn blockout_grip_drag_is_one_undo_step_even_with_two_ui_passes() {
        for target in [1.5, -1.0] {
            let mut app = app();
            let world = app.world_mut();
            let entity = add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
            world
                .get_resource_or_insert_with(crate::collider_handles::ColliderHandleTool::default)
                .enabled = true;
            let view = crate::views::ViewCamera::of(
                crate::views::ViewKind::Front,
                &crate::viewport::EditorCamera::default(),
            );
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
            let project = |point| {
                let p = view
                    .project(point, regia_core::math::Vec2::new(400.0, 300.0))
                    .expect("on screen");
                egui::pos2(p.x, p.y)
            };
            let start = project(Vec3::X);
            let end = project(Vec3::new(target, 0.0, 0.0));
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let ctx = egui::Context::default();
            for events in [
                vec![egui::Event::PointerMoved(start), button(start, true)],
                vec![egui::Event::PointerMoved(end)],
                vec![button(end, false)],
            ] {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(rect),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        viewport_input(ui, world, &view, rect, 0);
                        viewport_input(ui, world, &view, rect, 0);
                    },
                );
                output.textures_delta.clear();
            }
            let resized = world.get::<BlockoutBrush>(entity).expect("resized").size.x;
            if target > 0.0 {
                assert!((resized - 3.0).abs() < 0.01);
            } else {
                assert_eq!(
                    resized, 0.001,
                    "a positive-side grip clamps when dragged past its center"
                );
            }
            assert_eq!(world.resource::<crate::command::UndoStack>().undo_depth(), 2);
            assert!(crate::command::undo_world(world));
            assert_eq!(world.get::<BlockoutBrush>(entity).expect("restored").size.x, 2.0);
        }
    }

    #[test]
    fn blockout_invalid_edits_are_atomic_and_brushes_save_as_new_component_types() {
        let mut app = app();
        let world = app.world_mut();
        let entity = add(world, BlockoutBrush::default(), Vec3::ZERO).expect("brush");
        let old = world.get::<BlockoutBrush>(entity).expect("brush").clone();
        let mut bad = old.clone();
        bad.steps = 0;
        assert!(edit(world, entity, bad).is_err());
        assert_eq!(world.get::<BlockoutBrush>(entity), Some(&old));
        let file = crate::command::subtree_snapshot(world, &[entity.to_bits()]);
        let text = file.to_ron_string().expect("scene");
        assert!(text.contains("BlockoutBrush"));
        assert!(crate::command::undo_world(world));
        assert_eq!(count(world), 0);
        assert!(crate::command::redo_world(world));
        assert_eq!(count(world), 1);
    }
}
