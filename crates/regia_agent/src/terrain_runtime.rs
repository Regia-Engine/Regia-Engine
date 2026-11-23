// SPDX-License-Identifier: MPL-2.0
//! Runtime terrain changes on the agent protocol (ADR-1595, DO-4,
//! `book/agents/terrain-runtime.md`): `terrain.set_hole` opens, closes, adds and removes one named
//! hole, `terrain.patch_heights` queues a rectangle of new heights through the
//! [`TerrainHeightPatches`] queue, `terrain.get` reads the map, its holes and the queue, and
//! `terrain.save_holes` writes the holes in effect to a `<map>.holes.ron` (ADR-1853), and
//! `terrain.backdrop_status` reports the far backdrop ring (ADR-8459).

use regia_app::prelude::*;
use regia_terrain::{
    BackdropRing, BackdropState, BackdropStatus, HeightEdit, HeightEditError, HeightPatch, HeightPatchError,
    HoleShape, HolesError, HolesMask, MAX_PATCH_SAMPLES, NamedHole, Terrain, TerrainHeightEdits,
    TerrainHeightPatches, TerrainHoles, TerrainMeshLayers, terrain_holes,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Points one `terrain.get` call probes at most.
const PROBES_MAX: usize = 64;

fn hole_error(e: &HolesError) -> RpcError {
    let (code, reason) = match e {
        HolesError::BadId(_) => (protocol::INVALID_PARAMS, "hole_bad_id"),
        HolesError::UnknownId(_) => (protocol::NOT_FOUND, "hole_unknown_id"),
        HolesError::TooManyShapes(_) => (protocol::INVALID_PARAMS, "hole_too_many"),
        HolesError::FilledByLayer(_) => (protocol::INVALID_PARAMS, "hole_filled_by_layer"),
        _ => (protocol::INVALID_PARAMS, "F0002"),
    };
    RpcError::new(code, reason, e.to_string(), "see book/agents/terrain-runtime.md")
}

pub(crate) fn patch_error(e: &HeightPatchError) -> RpcError {
    let code = match e {
        HeightPatchError::NoTarget => protocol::NOT_FOUND,
        HeightPatchError::QueueFull { .. } => protocol::RATE_LIMITED,
        _ => protocol::INVALID_PARAMS,
    };
    RpcError::new(
        code,
        e.code(),
        e.to_string(),
        "see book/agents/terrain-runtime.md",
    )
}

/// The DO27 contract's refusals (`heights_mm`, ADR-7585): its own codes, or the patch codes.
fn edit_error(e: &HeightEditError) -> RpcError {
    match e {
        HeightEditError::Patch(inner) => patch_error(inner),
        HeightEditError::FrameFull { .. } => RpcError::new(
            protocol::RATE_LIMITED,
            e.code(),
            e.to_string(),
            "see book/agents/terrain-runtime.md",
        ),
        _ => RpcError::new(
            protocol::INVALID_PARAMS,
            e.code(),
            e.to_string(),
            "see book/agents/terrain-runtime.md",
        ),
    }
}

pub(crate) fn no_terrain() -> RpcError {
    RpcError::not_found(
        "no Terrain in this world and no linked terrain draw",
        "spawn an entity with regia_terrain::Terrain, or insert TerrainSharedDraw::holes_handle()",
    )
}

/// The terrain the calls act on: the lowest entity id (the one `terrain.stroke` edits).
pub(crate) fn first_terrain(world: &mut World) -> Option<Entity> {
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    query.iter(world).min_by_key(|e| e.to_bits())
}

/// The linked draw's holes handle, if the app has one.
fn linked_holes(world: &World) -> Option<TerrainHoles> {
    world
        .get_resource::<TerrainHoles>()
        .filter(|h| h.state().linked)
        .cloned()
}

fn named_json(hole: Option<&NamedHole>) -> Result<Value, RpcError> {
    serde_json::to_value(hole).map_err(|e| RpcError::internal(format!("hole serialization failed: {e}")))
}

fn segment_shape(seg: &Value) -> Result<HoleShape, RpcError> {
    let point = |key: &str| -> Result<[f32; 2], RpcError> {
        serde_json::from_value(seg.get(key).cloned().unwrap_or(Value::Null))
            .map_err(|_| RpcError::invalid_params(format!("segment `{key}` must be [x, z]")))
    };
    let width = seg
        .get("width_m")
        .and_then(Value::as_f64)
        .ok_or_else(|| RpcError::invalid_params("segment `width_m` must be a number"))?;
    Ok(HoleShape::segment(point("from")?, point("to")?, width as f32))
}

/// What one `terrain.set_hole` call does to a mask.
enum HoleEdit {
    /// Add or replace with this hole.
    Set(NamedHole),
    /// Open or close an existing hole.
    Open(bool),
    /// Remove the hole.
    Remove,
}

/// The hole before and after the edit.
type HoleChange = (Option<NamedHole>, Option<NamedHole>);

fn apply_edit(mask: &mut HolesMask, id: &str, edit: &HoleEdit) -> Result<HoleChange, HolesError> {
    let before = mask.named_hole(id).cloned();
    match edit {
        HoleEdit::Set(hole) => {
            mask.set_named(hole.clone())?;
        }
        HoleEdit::Open(open) => {
            mask.set_open(id, *open)?;
        }
        HoleEdit::Remove => {
            mask.remove_named(id)
                .ok_or_else(|| HolesError::UnknownId(id.to_owned()))?;
        }
    }
    let after = mask.named_hole(id).cloned();
    Ok((before, after))
}

/// `terrain.set_hole`: one named hole (`id`). With `shape` (a footprint, as in
/// `terrain.set_holes`) or `segment` (`{ from, to, width_m }`) it is added or replaced, open
/// unless `open: false`; with only `open` it is opened or closed; with `remove: true` it is
/// removed. A linked terrain draw cuts the result on its next frame; without one the first
/// `Terrain`'s own mask changes at once (headless apps: picks and colliders read it).
pub fn set_hole(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`id` (the hole's name) is required"))?
        .to_owned();
    let open = match params.get("open") {
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`open` must be a boolean"))?,
        ),
        None => None,
    };
    let remove = match params.get("remove") {
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`remove` must be a boolean"))?,
        None => false,
    };
    let shape = match (params.get("shape"), params.get("segment")) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params("pass `shape` or `segment`, not both"));
        }
        (Some(v), None) => Some(serde_json::from_value::<HoleShape>(v.clone()).map_err(|e| {
            RpcError::invalid_params(format!(
                "`shape` must be one of {{rect|polygon|ellipse|raster: {{...}}}}: {e}"
            ))
        })?),
        (None, Some(seg)) => Some(segment_shape(seg)?),
        (None, None) => None,
    };
    let edit = match (shape, open, remove) {
        (None, None, true) => HoleEdit::Remove,
        (_, _, true) => {
            return Err(RpcError::invalid_params("`remove: true` takes only `id`"));
        }
        (Some(shape), open, false) => HoleEdit::Set(NamedHole {
            id: id.clone(),
            open: open.unwrap_or(true),
            shape,
        }),
        (None, Some(open), false) => HoleEdit::Open(open),
        (None, None, false) => {
            return Err(RpcError::invalid_params(
                "pass `shape` or `segment` (add or replace), `open` (open or close), or `remove: true`",
            ));
        }
    };
    let fills = filled_ids(world);
    if let Some(handle) = linked_holes(world) {
        handle.set_filled_by_layer(&fills);
        let (before, after) = handle
            .edit(|mask| apply_edit(mask, &id, &edit))
            .map_err(|e| hole_error(&e))?;
        sync_layer_mouths(world, &id, &edit);
        return Ok(json!({
            "id": id,
            "before": named_json(before.as_ref())?,
            "after": named_json(after.as_ref())?,
            "applied_to": "draw",
            "pending": true,
        }));
    }
    let entity = first_terrain(world).ok_or_else(no_terrain)?;
    let (before, after) = {
        let mut terrain = world.get_mut::<Terrain>(entity).ok_or_else(no_terrain)?;
        let mut mask = terrain.holes.clone().unwrap_or_default();
        let before_mask = mask.clone();
        let (before, after) = apply_edit(&mut mask, &id, &edit).map_err(|e| hole_error(&e))?;
        regia_terrain::holes::filled_shape_change(&before_mask, &mask, &fills).map_err(|e| hole_error(&e))?;
        mask.validate().map_err(|e| hole_error(&e))?;
        terrain.holes = Some(mask);
        (before, after)
    };
    sync_layer_mouths(world, &id, &edit);
    Ok(json!({
        "id": id,
        "before": named_json(before.as_ref())?,
        "after": named_json(after.as_ref())?,
        "applied_to": "component",
        "entity": entity.to_bits(),
        "pending": false,
    }))
}

/// Hole ids a [`TerrainMeshLayers`] mouth fills, sorted and unique.
fn filled_ids(world: &mut World) -> Vec<String> {
    let _ = world.register_component::<TerrainMeshLayers>();
    let mut ids = Vec::new();
    let mut query = world.query::<&TerrainMeshLayers>();
    for layers in query.iter(world) {
        ids.extend(layers.filled_holes());
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Opens, closes or drops the presentation mouth after a hole edit that was allowed.
fn sync_layer_mouths(world: &mut World, id: &str, edit: &HoleEdit) {
    let _ = world.register_component::<TerrainMeshLayers>();
    let mut query = world.query::<&mut TerrainMeshLayers>();
    for mut layers in query.iter_mut(world) {
        match edit {
            HoleEdit::Open(open) => {
                let _ = layers.set_open(id, *open);
            }
            HoleEdit::Remove => {
                let _ = layers.remove_mouth(id);
            }
            HoleEdit::Set(hole) => {
                let _ = layers.set_open(id, hole.open);
            }
        }
    }
}

/// `terrain.save_holes`: writes the mask in effect (a linked draw's next mask, pending edits
/// included, else the first `Terrain`'s) to the `<map>.holes.ron` of `path` under the project
/// root, atomically, replacing the file.
pub fn save_holes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (relative to the project root) is required"))?;
    let name = if crate::terrain::has_holes_suffix(path) {
        path.to_owned()
    } else {
        // The reply names the file with `/` on every host (`holes_ron_path` joins with the host's separator).
        regia_terrain::holes_ron_path(std::path::Path::new(path))
            .to_string_lossy()
            .replace('\\', "/")
    };
    let file = crate::terrain_heights::project_file(world, &name)?;
    let mask = if let Some(handle) = linked_holes(world) {
        let state = handle.state();
        state.requested.unwrap_or(state.applied)
    } else {
        let entity = first_terrain(world).ok_or_else(no_terrain)?;
        world
            .get::<Terrain>(entity)
            .and_then(|t| t.holes.clone())
            .unwrap_or_default()
    };
    let hint = "see book/agents/terrain-runtime.md";
    let failed = |message: String| RpcError::new(protocol::INTERNAL_ERROR, "holes_write", message, hint);
    let text = mask.to_holes_ron().map_err(|e| failed(e.to_string()))?;
    crate::bake_export::write_atomic(&file, text.as_bytes()).map_err(|e| failed(format!("{name}: {e}")))?;
    Ok(json!({
        "path": name,
        "bytes": text.len(),
        "shapes": mask.shape_count(),
        "named": mask.named.len(),
    }))
}

fn u32_param(params: &Value, name: &str) -> Result<Option<u32>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| {
                RpcError::invalid_params(format!("`{name}` must be a whole number of samples"))
            })
        })
        .transpose()
}

pub(crate) fn patch_handle(world: &World) -> Result<TerrainHeightPatches, RpcError> {
    world
        .get_resource::<TerrainHeightPatches>()
        .cloned()
        .ok_or_else(|| {
            RpcError::not_found(
                "no TerrainHeightPatches resource in this app",
                "add regia_terrain::TerrainPlugin (and insert the draw's height_patches_handle())",
            )
        })
}

pub(crate) fn queue_json(handle: &TerrainHeightPatches) -> Value {
    let state = handle.state();
    json!({
        "queued_patches": state.queued_patches,
        "queued_samples": state.queued_samples,
        "budget": state.budget,
        "next_id": state.next_id,
        "completed": state.completed,
        "applied_samples": state.applied_samples,
        "generation": state.generation,
        "last_frame": {
            "samples": state.last_frame.samples,
            "bands": state.last_frame.bands,
            "completed": state.last_frame.completed,
        },
        "draw_linked": state.draw_linked,
        "draw_pending": state.draw_pending,
        "edits_this_frame": TerrainHeightEdits::new(handle.clone()).pushed_this_frame(),
    })
}

/// `terrain.patch_heights`: queues `width × height` samples at sample `(col, row)`: `heights`
/// (metres, row-major, X fastest), `samples` (16-bit values), `fill` (one height for all) or
/// `heights_mm` (the DO27 contract's integer millimetres through [`TerrainHeightEdits`], with
/// its limits: 128 samples a side, 16 edits a frame; ADR-7585).
/// `budget` sets the samples written per frame (alone, it only sets the budget). Heights are
/// clamped and quantised to the map's 16-bit grid. Returns the patch id; wait until
/// `terrain.get`'s `patches.completed` reaches it.
pub fn patch_heights(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let handle = patch_handle(world)?;
    let budget = u32_param(params, "budget")?;
    if budget == Some(0) {
        return Err(RpcError::invalid_params("`budget` must be at least 1 sample"));
    }
    let width = u32_param(params, "width")?;
    let height = u32_param(params, "height")?;
    if let (None, None, Some(b)) = (width, height, budget) {
        handle.set_budget(b);
        return Ok(json!({ "patch": null, "queue": queue_json(&handle) }));
    }
    let (Some(width), Some(height)) = (width, height) else {
        return Err(RpcError::invalid_params(
            "`width` and `height` (samples) are required",
        ));
    };
    let col = u32_param(params, "col")?.unwrap_or(0);
    let row = u32_param(params, "row")?.unwrap_or(0);
    if handle.target().is_none() {
        // No draw linked and the plugin has not run yet: take the map from the first terrain.
        if let Some(entity) = first_terrain(world)
            && let Some(terrain) = world.get::<Terrain>(entity)
        {
            handle.set_target(regia_terrain::PatchTarget::of(&terrain.map));
        }
    }
    let count = u64::from(width) * u64::from(height);
    let given = ["heights", "samples", "fill", "heights_mm"]
        .iter()
        .filter(|k| params.get(**k).is_some())
        .count();
    if given != 1 {
        return Err(RpcError::invalid_params(
            "pass exactly one of `heights`, `samples`, `fill` or `heights_mm`",
        ));
    }
    if let Some(mm) = params.get("heights_mm") {
        let heights_mm = mm
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`heights_mm` must be an array of whole millimetres"))?
            .iter()
            .map(|v| v.as_i64().and_then(|n| i32::try_from(n).ok()))
            .collect::<Option<Vec<i32>>>()
            .ok_or_else(|| RpcError::invalid_params("`heights_mm` must hold 32-bit whole numbers"))?;
        let id = TerrainHeightEdits::new(handle.clone())
            .push(HeightEdit {
                col,
                row,
                width,
                height,
                heights_mm,
            })
            .map_err(|e| edit_error(&e))?;
        if let Some(b) = budget {
            handle.set_budget(b);
        }
        return Ok(json!({ "patch": id, "queue": queue_json(&handle) }));
    }
    let queued = if let Some(samples) = params.get("samples") {
        let samples = samples
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`samples` must be an array of 0..=65535"))?
            .iter()
            .map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
            .collect::<Option<Vec<u16>>>()
            .ok_or_else(|| RpcError::invalid_params("`samples` must hold whole numbers 0..=65535"))?;
        handle.request_samples(col, row, width, height, samples)
    } else {
        let heights = if let Some(fill) = params.get("fill") {
            let fill = fill
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`fill` must be a number of metres"))?;
            if count > MAX_PATCH_SAMPLES {
                return Err(patch_error(&HeightPatchError::TooLarge { samples: count }));
            }
            vec![fill as f32; count as usize]
        } else {
            params
                .get("heights")
                .and_then(Value::as_array)
                .ok_or_else(|| RpcError::invalid_params("`heights` must be an array of metres"))?
                .iter()
                .map(|v| v.as_f64().map(|h| h as f32))
                .collect::<Option<Vec<f32>>>()
                .ok_or_else(|| RpcError::invalid_params("`heights` must hold numbers"))?
        };
        handle.request(HeightPatch {
            col,
            row,
            width,
            height,
            heights,
        })
    };
    let id = queued.map_err(|e| patch_error(&e))?;
    if let Some(b) = budget {
        handle.set_budget(b);
    }
    Ok(json!({ "patch": id, "queue": queue_json(&handle) }))
}

/// `terrain.get`: the lowest-entity-id terrain's map (grid, world width, height mapping, height range),
/// its holes (count, named holes with their state, generation, pending), the height patch queue,
/// and the ground at up to 64 `at` points (`height` null and `hole` true inside a hole).
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points: Vec<[f32; 2]> = match params.get("at") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.len() <= PROBES_MAX => items
            .iter()
            .map(|v| {
                serde_json::from_value::<[f32; 2]>(v.clone())
                    .ok()
                    .filter(|p| p.iter().all(|c| c.is_finite()))
                    .ok_or_else(|| RpcError::invalid_params("`at` points must be finite [x, z]"))
            })
            .collect::<Result<_, _>>()?,
        Some(Value::Array(_)) => {
            return Err(RpcError::invalid_params(format!(
                "`at` holds at most {PROBES_MAX} points"
            )));
        }
        Some(_) => return Err(RpcError::invalid_params("`at` must be an array of [x, z] points")),
    };
    let terrains = world.query::<&Terrain>().iter(world).count();
    let entity = first_terrain(world);
    let patches = world.get_resource::<TerrainHeightPatches>().map(queue_json);
    let holes_state = linked_holes(world).map(|h| h.state());
    let Some(entity) = entity else {
        if patches.is_none() && holes_state.is_none() {
            return Err(no_terrain());
        }
        return Ok(
            json!({ "terrains": 0, "entity": null, "map": null, "holes": null, "patches": patches, "probes": [], "anisotropy_path": anisotropy_path(), "anisotropy_lod_offset": anisotropy_lod_offset() }),
        );
    };
    let world_ref: &World = world;
    let terrain = world_ref.get::<Terrain>(entity).ok_or_else(no_terrain)?;
    let map = &terrain.map;
    let (lo, hi) = map.min_max_height();
    let mask = terrain_holes(world_ref, terrain);
    // The named holes the draw cuts with when one is linked, else the component's.
    let named_of = |m: &HolesMask| -> Vec<Value> {
        m.named
            .iter()
            .map(|n| json!({ "id": n.id, "open": n.open }))
            .collect()
    };
    let named = match &holes_state {
        Some(state) => named_of(&state.applied),
        None => terrain.holes.as_ref().map(named_of).unwrap_or_default(),
    };
    let probes: Vec<Value> = points
        .iter()
        .map(|&[x, z]| {
            let hole = mask.as_deref().is_some_and(|m| m.contains(x, z));
            // The map's world rectangle, moved by its grid origin (ADR-8179, ADR-8369).
            let ([x0, z0], [x1, z1]) = (map.world_min(), map.world_max());
            let inside = (x0..=x1).contains(&x) && (z0..=z1).contains(&z);
            let height = (!hole && inside).then(|| map.sample_bilinear(x, z));
            json!({ "at": [x, z], "height": height, "hole": hole, "inside": inside })
        })
        .collect();
    let holes = json!({
        "count": mask.as_deref().map_or(0, HolesMask::shape_count),
        "named": named,
        "linked": holes_state.as_ref().is_some_and(|s| s.linked),
        "generation": holes_state.as_ref().map_or(0, |s| s.generation),
        "pending": holes_state.as_ref().is_some_and(|s| s.requested.is_some()),
    });
    Ok(json!({
        "terrains": terrains,
        "entity": entity.to_bits(),
        "map": {
            "width": map.width(),
            "height": map.height(),
            "world_width": map.world_width(),
            "world_depth": map.world_depth(),
            "grid_origin": map.grid_origin(),
            "sample_spacing": map.sample_spacing(),
            "sample_spacing_z": map.sample_spacing_z(),
            "sample_layout": map.sample_layout().as_str(),
            "cell_extent": map.cell_extent(),
            "height_scale": map.height_scale(),
            "height_offset": map.height_offset(),
            "min_height": lo,
            "max_height": hi,
        },
        "holes": holes,
        "patches": patches,
        "probes": probes,
        "anisotropy_path": anisotropy_path(),
        "anisotropy_lod_offset": anisotropy_lod_offset(),
    }))
}

/// `terrain.backdrop_status`: the far backdrop ring (ADR-8459): what the host's last
/// `regia_terrain::build_visual_map` did (`state`: built, disabled, tier_off, no_room, or
/// not_built before any build), the inner (collider) and drawn map sizes, the ring width and the
/// settings with the ring profile. `configured` is false when the app has no backdrop settings.
pub fn backdrop_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let (status, configured) = match (
        world.get_resource::<BackdropStatus>(),
        world.get_resource::<BackdropRing>(),
    ) {
        (Some(status), _) => (status.clone(), true),
        (None, Some(ring)) => (BackdropStatus::not_built(ring.clone()), true),
        (None, None) => (BackdropStatus::not_built(BackdropRing::default()), false),
    };
    let mut out = serde_json::to_value(&status).map_err(|e| RpcError::internal(e.to_string()))?;
    if let Value::Object(map) = &mut out {
        map.insert("enabled".into(), json!(status.state == BackdropState::Built));
        map.insert("configured".into(), json!(configured));
        map.insert(
            "collider".into(),
            json!("inner: the collider, nav and simulation keep the inner map"),
        );
    }
    Ok(out)
}

/// The anisotropic tap path of the process's terrain draw (`sampler` / `footprint`, null before
/// a draw was created; ADR-3957).
fn anisotropy_path() -> Value {
    regia_terrain::anisotropy_path().map_or(Value::Null, |p| Value::from(p.name()))
}

/// The sampler path's LOD offset of the process's terrain draw, in mips (0 on most adapters,
/// 0.25 on RADV; null before a draw was created; ADR-5459).
fn anisotropy_lod_offset() -> Value {
    regia_terrain::anisotropy_lod_offset().map_or(Value::Null, Value::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_terrain() -> (World, Entity) {
        let mut world = World::new();
        let n = 65u32;
        let hf = regia_asset::Heightfield {
            width: n,
            height: n,
            samples: vec![32768; (n * n) as usize],
        };
        let terrain = Terrain::new(
            &hf,
            64.0,
            100.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            3,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let e = world.spawn(terrain).id();
        world.insert_resource(TerrainHeightPatches::default());
        (world, e)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn a_patch_heights_edit_survives_a_layer_recompute() {
        let (mut world, e) = world_with_terrain();
        world.init_resource::<Time>();
        let call = |world: &mut World, method: &str, params: Value| {
            crate::dispatch::call_in_process(world, method, params)
                .expect("synchronous reply")
                .unwrap_or_else(|err| panic!("{method}: {err:?}"))
        };
        let created = call(&mut world, "terrain.edit_layer_create", json!({"name": "Hills"}));
        let layer = created["layer"].clone();
        call(
            &mut world,
            "terrain.stroke",
            json!({
                "mode": "raise", "strength": 2.0, "radius": 6.0, "center": [0.0, 0.0],
            }),
        );
        call(
            &mut world,
            "terrain.patch_heights",
            json!({
                "col":32, "row":32, "width":1, "height":1, "fill":12.5
            }),
        );
        world.init_resource::<Messages<regia_terrain::TerrainHeightsChanged>>();
        let mut schedule = Schedule::default();
        schedule.add_systems(regia_terrain::apply_terrain_height_patches);
        schedule.run(&mut world);
        let i = 32 * 65 + 32;
        let patched = world.get::<Terrain>(e).expect("terrain").map.heights()[i];
        call(
            &mut world,
            "terrain.edit_layer_set",
            json!({"layer":layer, "enabled":false}),
        );
        call(
            &mut world,
            "terrain.edit_layer_set",
            json!({"layer":layer, "enabled":true}),
        );
        assert_eq!(
            world.get::<Terrain>(e).expect("terrain").map.heights()[i],
            patched
        );
    }

    /// `terrain.save_holes` writes what `terrain.set_hole` built, under the map's name, and it
    /// loads back; a closed hole keeps its state.
    #[test]
    fn save_holes_writes_a_file_that_loads_back() {
        let root = regia_test_utils::unique_temp_dir("agent_terrain_save_holes");
        let (mut world, _) = world_with_terrain();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: root.clone(),
            platform: "linux".into(),
            content_dir: None,
        })
        .expect("asset server");
        world.insert_resource(server);
        let hole = |id: &str, open: bool| {
            json!({ "id": id, "open": open,
                "shape": { "rect": { "center": [4.0, 0.0], "half_extents": [2.0, 2.0], "rotation_radians": 0.0 } } })
        };
        set_hole(&mut world, &hole("gate", false)).expect("closed hole");
        set_hole(&mut world, &hole("mouth", true)).expect("open hole");

        let out = save_holes(&mut world, &json!({ "path": "maps/arena.level.ron" })).expect("save");
        assert_eq!(out["path"], json!("maps/arena.holes.ron"), "{out}");
        assert_eq!(out["named"], json!(2), "{out}");
        assert_eq!(out["shapes"], json!(1), "only the open hole cuts: {out}");
        let back = regia_terrain::load_holes_ron(&root.join("maps/arena.holes.ron"))
            .expect("read")
            .expect("file");
        assert_eq!(back.named_hole("gate").map(|h| h.open), Some(false));
        assert_eq!(back.named_hole("mouth").map(|h| h.open), Some(true));

        // A `.holes.ron` name is taken as it is; a file replaces the old one.
        set_hole(&mut world, &json!({ "id": "gate", "remove": true })).expect("remove");
        let out = save_holes(&mut world, &json!({ "path": "maps/arena.holes.ron" })).expect("save");
        assert_eq!(out["path"], json!("maps/arena.holes.ron"), "{out}");
        assert_eq!(out["named"], json!(1), "{out}");

        assert!(save_holes(&mut world, &json!({})).is_err());
        assert!(save_holes(&mut world, &json!({ "path": "../up.level.ron" })).is_err());
        assert!(save_holes(&mut world, &json!({ "path": "/abs.level.ron" })).is_err());

        // A file where a directory has to be: the write fails with its code.
        std::fs::write(root.join("blocker"), b"file").expect("write");
        let err = save_holes(&mut world, &json!({ "path": "blocker/arena.level.ron" })).expect_err("blocked");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("holes_write")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `terrain.get` reports the map's grid origin (ADR-8179) through the dispatcher: `[0, 0]`
    /// for a default map, the host's origin for a moved one, and probes are inside the moved
    /// square. Patch rectangles stay in sample indices.
    #[test]
    fn get_reports_the_grid_origin() {
        let (mut world, e) = world_with_terrain();
        let call = |w: &mut World, method: &str, params: Value| {
            crate::dispatch::call_in_process(w, method, params)
                .expect("answered this frame")
                .expect("ok")
        };
        let read = call(&mut world, "terrain.get", json!({}));
        assert_eq!(read["map"]["grid_origin"], json!([0.0, 0.0]), "{read}");
        {
            let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
            let moved = terrain
                .map
                .clone()
                .with_grid_origin([-0.5, -0.5])
                .expect("origin");
            terrain.map = moved;
        }
        // 64 m wide: the moved square spans -32.5 ..= 31.5.
        let read = call(
            &mut world,
            "terrain.get",
            json!({ "at": [[-32.25, 0.0], [31.75, 0.0]] }),
        );
        assert_eq!(read["map"]["grid_origin"], json!([-0.5, -0.5]), "{read}");
        assert_eq!(read["probes"][0]["inside"], json!(true), "{read}");
        assert_eq!(read["probes"][1]["inside"], json!(false), "{read}");
        // A patch at sample (0, 0) writes the sample at the moved corner.
        let out = patch_heights(
            &mut world,
            &json!({ "col": 0, "row": 0, "width": 1, "height": 1, "fill": 10.0 }),
        )
        .expect("queued");
        assert!(out["patch"].is_u64(), "{out}");
        let target = world.resource::<TerrainHeightPatches>().target().expect("target");
        assert_eq!(target.grid_origin, [-0.5, -0.5]);
        assert_eq!(
            target
                .footprint(&regia_terrain::DirtyRect {
                    col_start: 0,
                    col_end: 1,
                    row_start: 0,
                    row_end: 1,
                })
                .0,
            [-32.5, -32.5]
        );
    }

    /// `terrain.get` reports a rectangular map's Z extent (ADR-8369). A default map reports
    /// `world_depth == world_width`. After `with_world_depth` a probe past the short side is
    /// outside the map.
    #[test]
    fn get_reports_the_world_depth() {
        let (mut world, e) = world_with_terrain();
        let call = |w: &mut World, params: Value| {
            crate::dispatch::call_in_process(w, "terrain.get", params)
                .expect("answered this frame")
                .expect("ok")
        };
        let read = call(&mut world, json!({}));
        assert_eq!(read["map"]["world_width"], json!(64.0), "{read}");
        assert_eq!(read["map"]["world_depth"], json!(64.0), "{read}");
        assert_eq!(
            read["map"]["sample_spacing_z"], read["map"]["sample_spacing"],
            "{read}"
        );
        {
            let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
            let rect = terrain.map.clone().with_world_depth(16.0).expect("depth");
            terrain.map = rect;
        }
        // 64 m on X, 16 m on Z: half-depth is 8 m, so z = 10 is outside and x = 0 is inside.
        let read = call(&mut world, json!({ "at": [[0.0, 10.0], [0.0, 0.0]] }));
        assert_eq!(read["map"]["world_depth"], json!(16.0), "{read}");
        assert_eq!(read["map"]["sample_spacing_z"], json!(0.25), "{read}");
        assert_eq!(read["probes"][0]["inside"], json!(false), "{read}");
        assert_eq!(read["probes"][1]["inside"], json!(true), "{read}");
        let target = regia_terrain::PatchTarget::of(&world.get::<Terrain>(e).expect("terrain").map);
        assert_eq!(target.world_depth, 16.0);
    }

    /// ADR-10079: `terrain.get` reports the map's sample layout and its cells' extent; a
    /// cell-centered map's `world_width` is the span between its outer samples.
    #[test]
    fn get_reports_the_sample_layout() {
        let (mut world, e) = world_with_terrain();
        let call = |w: &mut World| {
            crate::dispatch::call_in_process(w, "terrain.get", json!({}))
                .expect("answered this frame")
                .expect("ok")
        };
        let read = call(&mut world);
        assert_eq!(read["map"]["sample_layout"], json!("VertexCentered"), "{read}");
        assert_eq!(read["map"]["cell_extent"], json!([64.0, 64.0]), "{read}");
        let n = {
            let mut terrain = world.get_mut::<Terrain>(e).expect("terrain");
            let cell = terrain
                .map
                .clone()
                .with_sample_layout(regia_terrain::sampler::SampleLayout::CellCentered);
            terrain.map = cell;
            terrain.map.width()
        };
        let read = call(&mut world);
        assert_eq!(read["map"]["sample_layout"], json!("CellCentered"), "{read}");
        assert_eq!(read["map"]["cell_extent"], json!([64.0, 64.0]), "{read}");
        let span = read["map"]["world_width"].as_f64().expect("width");
        let want = 64.0 * f64::from(n - 1) / f64::from(n);
        assert!((span - want).abs() < 1e-4, "{read}");
    }

    /// `terrain.get` reports the anisotropic tap path as one of the documented names, or null
    /// while no terrain draw exists in the process (ADR-3957).
    #[test]
    fn get_reports_the_anisotropy_path() {
        let (mut world, _) = world_with_terrain();
        let read = get(&mut world, &json!({})).expect("get");
        let path = read
            .get("anisotropy_path")
            .unwrap_or_else(|| panic!("terrain.get has no anisotropy_path key: {read}"));
        assert!(
            path.is_null() || *path == json!("sampler") || *path == json!("footprint"),
            "{read}"
        );
    }

    /// `terrain.get` reports the sampler path's LOD offset as a number of mips in the documented
    /// range, or null while no terrain draw exists in the process (ADR-5459).
    #[test]
    fn get_reports_the_anisotropy_lod_offset() {
        let (mut world, _) = world_with_terrain();
        let read = get(&mut world, &json!({})).expect("get");
        let offset = read
            .get("anisotropy_lod_offset")
            .unwrap_or_else(|| panic!("terrain.get has no anisotropy_lod_offset key: {read}"));
        let max = f64::from(regia_terrain::MAX_ANISO_LOD_OFFSET);
        assert!(
            offset.is_null() || offset.as_f64().is_some_and(|v| (0.0..=max).contains(&v)),
            "{read}"
        );
    }

    /// Headless (no linked draw): `terrain.set_hole` edits the terrain's own mask at once, and a
    /// ray or a probe falls through an open hole only.
    #[test]
    fn set_hole_opens_and_closes_a_named_hole_on_the_component() {
        let (mut world, e) = world_with_terrain();
        let mouth = json!({ "id": "tunnel_a", "open": false,
            "shape": { "rect": { "center": [10.0, 0.0], "half_extents": [4.0, 3.0], "rotation_radians": 0.0 } } });
        let out = set_hole(&mut world, &mouth).expect("add closed");
        assert_eq!(out["applied_to"], json!("component"));
        assert!(
            out["before"].is_null() && out["after"]["open"] == json!(false),
            "{out}"
        );
        let probe = json!({ "at": [[10.0, 0.0], [-10.0, 0.0]] });
        let read = get(&mut world, &probe).expect("get");
        assert_eq!(read["probes"][0]["hole"], json!(false), "closed: ground");
        assert_eq!(
            read["holes"]["named"],
            json!([{ "id": "tunnel_a", "open": false }])
        );
        set_hole(&mut world, &json!({ "id": "tunnel_a", "open": true })).expect("open");
        let read = get(&mut world, &probe).expect("get");
        assert_eq!(read["probes"][0]["hole"], json!(true), "{read}");
        assert!(read["probes"][0]["height"].is_null());
        assert_eq!(read["probes"][1]["hole"], json!(false));
        assert_eq!(read["holes"]["count"], json!(1));
        let ray = regia_terrain::pick_terrain(
            &world,
            regia_core::math::Vec3::new(10.0, 80.0, 0.0),
            regia_core::math::Vec3::new(0.0, -1.0, 0.0),
            1000.0,
        );
        assert!(ray.is_none(), "the ray falls through the open hole");
        // Errors carry their codes.
        let err = set_hole(&mut world, &json!({ "id": "nope", "open": true })).expect_err("unknown");
        assert_eq!(reason(&err), "hole_unknown_id");
        let err = set_hole(
            &mut world,
            &json!({ "id": "bad id", "segment": { "from": [0.0, 0.0], "to": [5.0, 0.0], "width_m": 3.0 } }),
        )
        .expect_err("bad id");
        assert_eq!(reason(&err), "hole_bad_id");
        assert!(set_hole(&mut world, &json!({ "id": "x" })).is_err());
        assert!(set_hole(&mut world, &json!({ "id": "x", "remove": true, "open": true })).is_err());
        set_hole(&mut world, &json!({ "id": "tunnel_a", "remove": true })).expect("remove");
        assert!(
            world
                .get::<Terrain>(e)
                .and_then(|t| t.holes.as_ref())
                .is_some_and(HolesMask::is_empty)
        );
    }

    /// With a linked draw the edits stack on its pending request.
    #[test]
    fn set_hole_requests_on_a_linked_draw() {
        let mut world = World::new();
        assert!(
            set_hole(&mut world, &json!({ "id": "a", "open": true })).is_err(),
            "nothing to edit"
        );
        // A linked handle is only made by a draw; the component path covers the headless case
        // and `regia_terrain`'s tests cover `TerrainHoles::edit`.
        world.insert_resource(TerrainHoles::default());
        assert!(
            set_hole(&mut world, &json!({ "id": "a", "open": true })).is_err(),
            "unlinked, no terrain"
        );
    }

    /// `terrain.patch_heights` validates, quantises and queues; the plugin's system applies the
    /// patch within the budget and the map follows.
    #[test]
    fn patch_heights_queues_and_the_system_applies_it() {
        let (mut world, e) = world_with_terrain();
        let err = |w: &mut World, p: Value| reason(&patch_heights(w, &p).expect_err("refused"));
        assert_eq!(
            err(
                &mut world,
                json!({ "col": 60, "width": 10, "height": 2, "fill": 1.0 })
            ),
            "patch_out_of_bounds"
        );
        assert_eq!(
            err(
                &mut world,
                json!({ "width": 2, "height": 2, "heights": [1.0, 2.0] })
            ),
            "patch_bad_length"
        );
        assert_eq!(
            err(&mut world, json!({ "width": 0, "height": 2, "fill": 1.0 })),
            "patch_empty"
        );
        assert!(patch_heights(&mut world, &json!({ "width": 2, "height": 2 })).is_err());
        assert!(
            patch_heights(
                &mut world,
                &json!({ "width": 2, "height": 2, "fill": 1.0, "samples": [1, 2, 3, 4] })
            )
            .is_err()
        );
        let out = patch_heights(&mut world, &json!({ "budget": 64 })).expect("budget only");
        assert_eq!(out["queue"]["budget"], json!(64));
        let out = patch_heights(
            &mut world,
            &json!({ "col": 8, "row": 8, "width": 16, "height": 8, "fill": 12.5 }),
        )
        .expect("queued");
        assert_eq!(out["patch"], json!(1));
        assert_eq!(out["queue"]["queued_samples"], json!(128));
        // Two frames of the system at 64 samples a frame.
        let mut schedule = Schedule::default();
        schedule.add_systems(regia_terrain::apply_terrain_height_patches);
        world.init_resource::<Messages<regia_terrain::TerrainHeightsChanged>>();
        schedule.run(&mut world);
        let read = get(&mut world, &json!({})).expect("get");
        assert_eq!(read["patches"]["completed"], json!(0), "{read}");
        assert_eq!(read["patches"]["last_frame"]["samples"], json!(64));
        schedule.run(&mut world);
        let read = get(&mut world, &json!({ "at": [[-16.0, -16.0]] })).expect("get");
        assert_eq!(read["patches"]["completed"], json!(1), "{read}");
        let map = &world.get::<Terrain>(e).expect("terrain").map;
        let at = |c: u32, r: u32| map.heights()[(r * 65 + c) as usize];
        assert!((at(8, 8) - 12.5).abs() < 2e-3 && (at(23, 15) - 12.5).abs() < 2e-3);
        assert!((at(7, 8) - 50.0).abs() < 2e-3, "outside the rectangle: unchanged");
        let messages = world.resource::<Messages<regia_terrain::TerrainHeightsChanged>>();
        assert_eq!(messages.len(), 2, "one message per band");
    }

    /// `heights_mm` (ADR-7585): the DO27 contract's integer millimetres go through
    /// `TerrainHeightEdits`, land on the 16-bit grid, and its limits answer with their codes.
    #[test]
    fn patch_heights_takes_the_contracts_millimetres_and_limits() {
        let (mut world, e) = world_with_terrain();
        let err = |w: &mut World, p: Value| reason(&patch_heights(w, &p).expect_err("refused"));
        let big = vec![json!(1000); 129];
        assert_eq!(
            err(
                &mut world,
                json!({ "width": 129, "height": 1, "heights_mm": big })
            ),
            "edit_too_large"
        );
        assert_eq!(
            err(
                &mut world,
                json!({ "width": 1, "height": 1, "heights_mm": [5_000_000] })
            ),
            "edit_out_of_range"
        );
        assert_eq!(
            err(
                &mut world,
                json!({ "width": 1, "height": 1, "heights_mm": [1.5] })
            ),
            "F0002"
        );
        let out = patch_heights(
            &mut world,
            &json!({ "col": 4, "row": 5, "width": 2, "height": 1, "heights_mm": [12_345, -0] }),
        )
        .expect("queued");
        assert_eq!(out["patch"], json!(1));
        assert_eq!(out["queue"]["edits_this_frame"], json!(1));
        for _ in 1..16 {
            patch_heights(
                &mut world,
                &json!({ "width": 1, "height": 1, "heights_mm": [1_000] }),
            )
            .expect("within the frame limit");
        }
        assert_eq!(
            err(
                &mut world,
                json!({ "width": 1, "height": 1, "heights_mm": [1_000] })
            ),
            "edit_frame_full"
        );
        let mut schedule = Schedule::default();
        schedule.add_systems(regia_terrain::apply_terrain_height_patches);
        world.init_resource::<Messages<regia_terrain::TerrainHeightsChanged>>();
        schedule.run(&mut world);
        let map = &world.get::<Terrain>(e).expect("terrain").map;
        let step = map.height_scale() / 65535.0;
        let h = map.heights()[(5 * 65 + 4) as usize];
        assert!((h - 12.345).abs() <= step, "{h}");
        patch_heights(
            &mut world,
            &json!({ "width": 1, "height": 1, "heights_mm": [1_000] }),
        )
        .expect("a new frame takes edits again");
    }

    /// `terrain.backdrop_status` through the dispatcher (ADR-8459): no settings, settings
    /// before a build, a built ring and the lowest tier's skip, each with its sizes.
    #[test]
    fn backdrop_status_reports_the_ring() {
        let (mut world, e) = world_with_terrain();
        let call = |w: &mut World| {
            crate::dispatch::call_in_process(w, "terrain.backdrop_status", json!({}))
                .expect("answered this frame")
                .expect("ok")
        };
        let read = call(&mut world);
        assert_eq!(read["configured"], json!(false), "{read}");
        assert_eq!(read["state"], json!("not_built"), "{read}");
        assert_eq!(read["enabled"], json!(false), "{read}");
        assert_eq!(read["settings"]["off_tiers"], json!(["MobileLow"]), "{read}");
        assert_eq!(read["settings"]["max_side"], json!(8192), "{read}");
        assert_eq!(read["settings"]["profile"]["landform"], json!("none"), "{read}");

        world.insert_resource(BackdropRing::default());
        let read = call(&mut world);
        assert_eq!(read["configured"], json!(true), "{read}");
        assert_eq!(read["state"], json!("not_built"), "{read}");

        // A host build on High: the 65-sample map gets a 32-sample ring.
        let inner = world.get::<Terrain>(e).expect("terrain").map.clone();
        let ring = regia_terrain::BackdropRing {
            min_ring_texels: 16,
            ..BackdropRing::default()
        };
        let build = regia_terrain::build_visual_map(&inner, &ring, Some(regia_render::QualityTier::High))
            .expect("build");
        world.insert_resource(build.status);
        let read = call(&mut world);
        assert_eq!(read["state"], json!("built"), "{read}");
        assert_eq!(read["enabled"], json!(true), "{read}");
        assert_eq!(read["tier"], json!("High"), "{read}");
        assert_eq!(read["inner_size"], json!([65, 65]), "{read}");
        assert_eq!(read["visual_size"], json!([129, 129]), "{read}");
        assert_eq!(read["ring_texels"], json!(32), "{read}");
        assert_eq!(read["inner_world"], json!([64.0, 64.0]), "{read}");
        assert_eq!(read["visual_world"], json!([128.0, 128.0]), "{read}");

        // The lowest tier skips it: the drawn map is the inner one.
        let build =
            regia_terrain::build_visual_map(&inner, &ring, Some(regia_render::QualityTier::MobileLow))
                .expect("build");
        world.insert_resource(build.status);
        let read = call(&mut world);
        assert_eq!(read["state"], json!("tier_off"), "{read}");
        assert_eq!(read["enabled"], json!(false), "{read}");
        assert_eq!(read["visual_size"], json!([65, 65]), "{read}");
        assert_eq!(read["ring_texels"], json!(0), "{read}");
    }
}
