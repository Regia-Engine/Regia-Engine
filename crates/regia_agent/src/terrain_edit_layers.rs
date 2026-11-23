// SPDX-License-Identifier: MPL-2.0
//! Non-destructive terrain edit layers over the agent protocol (P-236, ADR-7825, page
//! `book/agents/terrain-editing.md`): `terrain.edit_layers`, `terrain.edit_layer_create`,
//! `terrain.edit_layer_set`, `terrain.edit_layer_delete`, `terrain.edit_layer_reorder`,
//! `terrain.edit_layer_flatten`, `terrain.edit_layer_weights`, `terrain.edit_layer_paint_weights`,
//! `terrain.edit_layer_procgen`, and the `edit_layer` parameter of `terrain.stroke`.
//!
//! Every change goes through [`regia_terrain::apply_op`], which recomputes only the samples the
//! changed layer covers and logs the change; under the editor each one is an undo step.

use regia_app::prelude::*;
use regia_terrain::{
    EditLayerBlend, EditLayerError, EditLayerOp, EditLayerPatch, ProcgenLayerTarget, Terrain,
    TerrainEditLayers,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

const HINT: &str = "see book/agents/terrain-editing.md (edit layers)";

/// The agent error of an edit-layer refusal (its lowercase code is `data.code`).
pub(crate) fn layer_error(e: &EditLayerError) -> RpcError {
    let code = match e {
        EditLayerError::NotFound(_) | EditLayerError::NoTerrain(_) => protocol::NOT_FOUND,
        _ => protocol::INVALID_PARAMS,
    };
    RpcError::new(code, e.code(), e.to_string(), HINT)
}

/// The terrain a call targets: `entity`, else the lowest-id terrain.
pub(crate) fn terrain_entity(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    if let Some(bits) = crate::params::opt_u64(params, "entity")? {
        let entity = Entity::try_from_bits(bits)
            .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))?;
        if world.get::<Terrain>(entity).is_none() {
            return Err(layer_error(&EditLayerError::NoTerrain(bits)));
        }
        return Ok(entity);
    }
    let mut query = world.query_filtered::<Entity, With<Terrain>>();
    query.iter(world).min_by_key(|e| e.to_bits()).ok_or_else(|| {
        RpcError::not_found(
            "no Terrain in this world",
            "spawn an entity with regia_terrain::Terrain (the level's heightfield)",
        )
    })
}

/// A layer named by id (integer) or name (string) in `params[key]`; `"base"` is `Some(None)`.
/// `Ok(None)` when the parameter is absent.
pub(crate) fn layer_ref(
    world: &World,
    entity: Entity,
    params: &Value,
    key: &str,
) -> Result<Option<Option<u32>>, RpcError> {
    let Some(value) = params.get(key) else {
        return Ok(None);
    };
    if value.as_str() == Some("base") {
        return Ok(Some(None));
    }
    let stack = world.get::<TerrainEditLayers>(entity);
    let id = if let Some(id) = value.as_u64() {
        u32::try_from(id).map_err(|_| RpcError::invalid_params(format!("`{key}` {id} is not a layer id")))?
    } else if let Some(name) = value.as_str() {
        stack.and_then(|s| s.find(name)).map(|l| l.id).ok_or_else(|| {
            RpcError::new(
                protocol::NOT_FOUND,
                "edit_layer_not_found",
                format!("no edit layer named `{name}`"),
                HINT,
            )
        })?
    } else {
        return Err(RpcError::invalid_params(format!(
            "`{key}` is a layer id, a layer name or \"base\""
        )));
    };
    if stack.and_then(|s| s.layer(id)).is_none() {
        return Err(layer_error(&EditLayerError::NotFound(id)));
    }
    Ok(Some(Some(id)))
}

fn required_layer(world: &World, entity: Entity, params: &Value) -> Result<u32, RpcError> {
    match layer_ref(world, entity, params, "layer")? {
        Some(Some(id)) => Ok(id),
        Some(None) => Err(RpcError::invalid_params(
            "this method needs a layer, not \"base\"",
        )),
        None => Err(RpcError::invalid_params("`layer` (id or name) is required")),
    }
}

fn state(world: &World, entity: Entity) -> Value {
    let Some(stack) = world.get::<TerrainEditLayers>(entity) else {
        return json!({
            "entity": entity.to_bits(),
            "layers": [],
            "active": null,
            "recomputed_samples": 0,
            "unsaved": false,
        });
    };
    let layers: Vec<Value> = stack
        .layers()
        .iter()
        .enumerate()
        .map(|(index, l)| {
            json!({
                "id": l.id,
                "name": l.name,
                "index": index,
                "blend": l.blend.name(),
                "enabled": l.enabled,
                "locked": l.locked,
                "opacity": l.opacity,
                "procgen": l.procgen().map(|t| json!({ "graph": t.graph, "output": t.output })),
                "bounds": l.bounds().map(|b| json!({
                    "col_start": b.col_start, "col_end": b.col_end,
                    "row_start": b.row_start, "row_end": b.row_end,
                })),
            })
        })
        .collect();
    json!({
        "entity": entity.to_bits(),
        "layers": layers,
        "active": stack.active(),
        "recomputed_samples": stack.recomputed_samples(),
        "unsaved": stack.unsaved(),
    })
}

fn run(world: &mut World, entity: Entity, op: &EditLayerOp) -> Result<Value, RpcError> {
    let outcome = regia_terrain::apply_op(world, entity, op).map_err(|e| layer_error(&e))?;
    let mut out = state(world, entity);
    out["layer"] = json!(outcome.layer);
    out["recomputed"] = json!(outcome.recomputed);
    out["merged"] = json!(outcome.merged);
    Ok(out)
}

/// `terrain.edit_layers`: the stack, bottom first.
pub fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    Ok(state(world, entity))
}

/// `terrain.edit_layer_create { name, blend? }`: a layer on top, made active.
pub fn create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let name = crate::params::opt_str(params, "name")?
        .ok_or_else(|| RpcError::invalid_params("`name` is required"))?
        .to_owned();
    let blend = match crate::params::opt_str(params, "blend")? {
        None => EditLayerBlend::Add,
        Some(b) => EditLayerBlend::parse(b)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown blend `{b}`: add | set")))?,
    };
    run(world, entity, &EditLayerOp::Create { name, blend })
}

/// `terrain.edit_layer_set { layer, name?, enabled?, locked?, opacity?, active? }`.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let target = layer_ref(world, entity, params, "layer")?
        .ok_or_else(|| RpcError::invalid_params("`layer` (id, name or \"base\") is required"))?;
    let active = crate::params::opt_bool(params, "active")?;
    let patch = EditLayerPatch {
        name: crate::params::opt_str(params, "name")?.map(str::to_owned),
        enabled: crate::params::opt_bool(params, "enabled")?,
        locked: crate::params::opt_bool(params, "locked")?,
        opacity: crate::params::opt_f64(params, "opacity")?.map(|o| o as f32),
    };
    let changes = patch != EditLayerPatch::default();
    let mut out = match target {
        Some(id) if changes => run(world, entity, &EditLayerOp::Set { id, patch })?,
        None if changes => {
            return Err(RpcError::invalid_params(
                "the base has no name, visibility, lock or opacity; only `active` applies",
            ));
        }
        _ => state(world, entity),
    };
    if active == Some(true) {
        regia_terrain::set_active_layer(world, entity, target).map_err(|e| layer_error(&e))?;
        out["active"] = json!(target);
    }
    Ok(out)
}

/// `terrain.edit_layer_delete { layer }`.
pub fn delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let id = required_layer(world, entity, params)?;
    run(world, entity, &EditLayerOp::Delete { id })
}

/// `terrain.edit_layer_reorder { layer, index }`.
pub fn reorder(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let id = required_layer(world, entity, params)?;
    let index = crate::params::opt_u64(params, "index")?
        .ok_or_else(|| RpcError::invalid_params("`index` (0 is the bottom) is required"))?;
    run(
        world,
        entity,
        &EditLayerOp::Reorder {
            id,
            index: usize::try_from(index).unwrap_or(usize::MAX),
        },
    )
}

/// `terrain.edit_layer_flatten`: bakes every shown layer into the base.
pub fn flatten(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    run(world, entity, &EditLayerOp::Flatten)
}

fn sample_index(stack: &TerrainEditLayers, params: &Value) -> Result<(u32, u32, usize), RpcError> {
    let col = crate::params::opt_u64(params, "col")?
        .ok_or_else(|| RpcError::invalid_params("`col` is required"))?;
    let row = crate::params::opt_u64(params, "row")?
        .ok_or_else(|| RpcError::invalid_params("`row` is required"))?;
    let [width, height] = stack.size();
    let col = u32::try_from(col).map_err(|_| RpcError::invalid_params("`col` is out of range"))?;
    let row = u32::try_from(row).map_err(|_| RpcError::invalid_params("`row` is out of range"))?;
    if col >= width || row >= height {
        return Err(RpcError::invalid_params(format!(
            "sample ({col}, {row}) is outside the {width} x {height} grid"
        )));
    }
    Ok((col, row, row as usize * width as usize + col as usize))
}

fn weight_json(weights: &[f32]) -> Value {
    let sum: f32 = weights.iter().sum();
    json!({
        "weights": weights,
        "sum": sum,
    })
}

/// `terrain.edit_layer_weights { col, row }`: the folded material weights at one sample.
pub fn weights(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let Some(stack) = world.get::<TerrainEditLayers>(entity) else {
        return Err(layer_error(&EditLayerError::NotFound(0)));
    };
    let (col, row, index) = sample_index(stack, params)?;
    let folded = stack.folded_weights(index);
    let mut out = weight_json(&folded);
    out["entity"] = json!(entity.to_bits());
    out["col"] = json!(col);
    out["row"] = json!(row);
    Ok(out)
}

fn channel_array(params: &Value) -> Result<[f32; 8], RpcError> {
    let Some(values) = params.get("channels").and_then(Value::as_array) else {
        return Err(RpcError::invalid_params("`channels` is an array of numbers"));
    };
    if values.len() > 8 {
        return Err(layer_error(&EditLayerError::BadWeight));
    }
    let mut channels = [0.0; 8];
    for (slot, value) in channels.iter_mut().zip(values) {
        let number = value
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("`channels` is an array of numbers"))?;
        let value = number as f32;
        if !value.is_finite() {
            return Err(layer_error(&EditLayerError::BadWeight));
        }
        *slot = value;
    }
    Ok(channels)
}

/// `terrain.edit_layer_paint_weights { layer?, col, row, channels }`.
///
/// `layer` omitted paints the active layer (or the base when none is active). `"base"` paints the
/// base. An owned layer is refused with `edit_layer_owned`.
pub fn paint_weights(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let named = layer_ref(world, entity, params, "layer")?;
    let layer = match named {
        Some(layer) => layer,
        None => world
            .get::<TerrainEditLayers>(entity)
            .and_then(|stack| stack.active()),
    };
    let channels = channel_array(params)?;
    let (width, index) = if let Some(stack) = world.get::<TerrainEditLayers>(entity) {
        let (_col, _row, index) = sample_index(stack, params)?;
        (stack.size()[0], index as u32)
    } else if let Some(id) = layer {
        return Err(layer_error(&EditLayerError::NotFound(id)));
    } else {
        let terrain = world
            .get::<Terrain>(entity)
            .ok_or_else(|| layer_error(&EditLayerError::NoTerrain(entity.to_bits())))?;
        let width = terrain.map.width();
        let height = terrain.map.height();
        let col = crate::params::opt_u64(params, "col")?
            .ok_or_else(|| RpcError::invalid_params("`col` is required"))?;
        let row = crate::params::opt_u64(params, "row")?
            .ok_or_else(|| RpcError::invalid_params("`row` is required"))?;
        let col = u32::try_from(col).map_err(|_| RpcError::invalid_params("`col` is out of range"))?;
        let row = u32::try_from(row).map_err(|_| RpcError::invalid_params("`row` is out of range"))?;
        if col >= width || row >= height {
            return Err(RpcError::invalid_params(format!(
                "sample ({col}, {row}) is outside the {width} x {height} grid"
            )));
        }
        (width, row * width + col)
    };
    let mut out = run(
        world,
        entity,
        &EditLayerOp::PaintWeights {
            layer,
            index,
            channels,
        },
    )?;
    if let Some(stack) = world.get::<TerrainEditLayers>(entity) {
        let folded = stack.folded_weights(index as usize);
        out["weights"] = json!(folded);
        out["sum"] = json!(folded.iter().sum::<f32>());
        out["col"] = json!(index % width);
        out["row"] = json!(index / width);
    }
    Ok(out)
}

/// `terrain.edit_layer_procgen { layer, graph?, output?, detach? }`.
///
/// `detach: true` clears the target. Otherwise `graph` and `output` attach one. Undoable.
pub fn procgen(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = terrain_entity(world, params)?;
    let id = required_layer(world, entity, params)?;
    let detach = crate::params::opt_bool(params, "detach")?;
    let target = if detach == Some(true) {
        None
    } else {
        let graph = crate::params::opt_str(params, "graph")?
            .ok_or_else(|| RpcError::invalid_params("`graph` is required unless `detach` is true"))?
            .to_owned();
        let output = crate::params::opt_str(params, "output")?
            .ok_or_else(|| RpcError::invalid_params("`output` is required unless `detach` is true"))?
            .to_owned();
        Some(ProcgenLayerTarget { graph, output })
    };
    run(world, entity, &EditLayerOp::SetProcgen { id, target })
}

#[cfg(test)]
mod tests {
    use regia_terrain::cdlod::CdlodConfig;
    use regia_terrain::layers::TerrainLayers;
    use regia_terrain::{TerrainEditLayerLog, TerrainMap};

    use super::*;

    fn world() -> (World, Entity) {
        let mut world = World::new();
        let n = 65u32;
        let heights = (0..n * n).map(|i| (i % n) as f32 * 0.1).collect();
        let map = TerrainMap::from_heights(n, n, heights, 64.0, 10.0, 0.0).expect("map");
        let terrain = Terrain::new(
            &map.to_heightfield(),
            64.0,
            10.0,
            0.0,
            CdlodConfig::default(),
            1,
            TerrainLayers::default(),
        )
        .expect("terrain");
        let e = world.spawn(terrain).id();
        (world, e)
    }

    fn heights(world: &World, e: Entity) -> Vec<u32> {
        world
            .get::<Terrain>(e)
            .expect("t")
            .map
            .heights()
            .iter()
            .map(|h| h.to_bits())
            .collect()
    }

    fn stroke(world: &mut World, extra: Value) -> Value {
        let mut params = json!({ "mode": "raise", "center": [0.0, 0.0], "radius": 6.0, "strength": 2.0 });
        if let (Some(p), Some(x)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(x.clone());
        }
        crate::terrain::stroke(world, &params).expect("stroke")
    }

    #[test]
    fn layers_add_up_toggle_exactly_and_log_undo_entries() {
        let (mut world, e) = world();
        let original = heights(&world, e);
        let a = create(&mut world, &json!({ "name": "A" })).expect("A");
        assert_eq!(a["layer"], 1);
        stroke(&mut world, json!({}));
        let only_a = heights(&world, e);
        create(&mut world, &json!({ "name": "B" })).expect("B");
        let s = stroke(&mut world, json!({ "edit_layer": "B" }));
        assert!(s["height_after"].as_f64() > s["height_before"].as_f64());
        let off = set(&mut world, &json!({ "layer": "B", "enabled": false })).expect("off");
        assert!(off["recomputed"].as_u64() > Some(0));
        assert_eq!(heights(&world, e), only_a, "B off restores A's result exactly");
        set(&mut world, &json!({ "layer": 1, "enabled": false })).expect("off");
        assert_eq!(heights(&world, e), original);
        let log = world.resource::<TerrainEditLayerLog>();
        let strokes = log
            .entries
            .iter()
            .filter(|e| matches!(e, regia_terrain::EditLayerLogEntry::Stroke { .. }))
            .count();
        assert_eq!(strokes, 2, "each layered stroke is one log entry");
        assert_eq!(log.entries.len(), 6, "create, stroke, create, stroke, set, set");
    }

    #[test]
    fn errors_have_lowercase_codes() {
        let (mut world, _) = world();
        let reason = |r: Result<Value, RpcError>| {
            r.expect_err("refused")
                .data
                .and_then(|d| d.get("code").and_then(Value::as_str).map(str::to_owned))
        };
        assert_eq!(
            reason(set(&mut world, &json!({ "layer": 3, "enabled": false }))).as_deref(),
            Some("edit_layer_not_found")
        );
        create(&mut world, &json!({ "name": "A" })).expect("A");
        set(&mut world, &json!({ "layer": "A", "locked": true })).expect("lock");
        assert_eq!(
            reason(delete(&mut world, &json!({ "layer": "A" }))).as_deref(),
            Some("edit_layer_locked")
        );
        let refused = crate::terrain::stroke(
            &mut world,
            &json!({ "mode": "raise", "center": [0.0, 0.0], "radius": 6.0, "edit_layer": "A" }),
        );
        assert_eq!(reason(refused).as_deref(), Some("edit_layer_locked"));
        assert_eq!(
            reason(create(&mut world, &json!({ "name": "" }))).as_deref(),
            Some("edit_layer_bad_name")
        );
        assert_eq!(
            reason(set(&mut world, &json!({ "layer": "A", "opacity": 2.0 }))).as_deref(),
            Some("edit_layer_bad_opacity")
        );
    }

    #[test]
    fn reorder_and_flatten_keep_the_map_for_add_layers() {
        let (mut world, e) = world();
        create(&mut world, &json!({ "name": "A" })).expect("A");
        stroke(&mut world, json!({}));
        create(&mut world, &json!({ "name": "B" })).expect("B");
        stroke(&mut world, json!({ "center": [3.0, 1.0], "strength": 0.37 }));
        let before = heights(&world, e);
        let moved = reorder(&mut world, &json!({ "layer": "B", "index": 0 })).expect("reorder");
        assert_eq!(moved["layers"][0]["name"], "B");
        assert_eq!(heights(&world, e), before, "Add layers commute");
        let flat = flatten(&mut world, &json!({})).expect("flatten");
        assert_eq!(flat["merged"], 2);
        assert_eq!(flat["layers"].as_array().map(Vec::len), Some(0));
        assert_eq!(heights(&world, e), before, "flatten keeps every bit");
        // The base takes strokes once there is no layer.
        set(&mut world, &json!({ "layer": "base", "active": true })).expect("base");
        stroke(&mut world, json!({ "edit_layer": "base" }));
        assert_ne!(heights(&world, e), before);
    }
}
