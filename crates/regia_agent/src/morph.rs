// SPDX-License-Identifier: MPL-2.0
//! Agent methods for morph weights (Tincal engine items B1/C2, `book/agents/morph-weights.md`):
//! `anim.morph_weights` reads a mesh entity's weights, target names, expression layers and the
//! renderer's morph counters; `anim.set_morph_weights` sets weights by name or index.
//!
//! A rig whose weights are composed every frame (it has an `Animator` or `MorphLayers`) takes
//! agent weights as an override layer (named `"agent"` unless `layer` says otherwise), so the
//! animation does not overwrite them; any other entity's `MorphWeights` are written directly.

use regia_animation::{
    AnimationClip, Animator, AnimatorSource, MorphLayer, MorphLayerMode, MorphLayers, MorphSet, MorphTarget,
    MorphTargetNames, MorphWeights, NodeMorphPlayer, SourceNode,
};
use regia_app::prelude::*;
use regia_render::MeshMorphTargets;
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

/// The layer agent weights go to on a composed rig when `layer` is not given.
const AGENT_LAYER: &str = "agent";

/// Highest target index (exclusive) the agent may address: weight lists grow to the largest
/// index, so an unbounded one (`"4294967295"`) would allocate gigabytes.
const MAX_MORPH_TARGETS: u32 = 4096;

fn existing_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "list morphed meshes with anim.morph_weights (no entity)",
        ));
    }
    Ok(entity)
}

/// Target names: the entity's `MorphTargetNames`, else its render targets' names.
fn names_of(world: &World, entity: Entity) -> Option<MorphTargetNames> {
    world.get::<MorphTargetNames>(entity).cloned().or_else(|| {
        world
            .get::<MeshMorphTargets>(entity)
            .map(|t| MorphTargetNames::new(t.targets.names().iter().cloned()))
    })
}

fn target_json(target: &MorphTarget) -> Value {
    match target {
        MorphTarget::Index(index) => json!(index),
        MorphTarget::Name(name) => json!(name),
    }
}

fn layers_json(layers: &MorphLayers) -> Value {
    json!({
        "layers": layers.layers.iter().map(|l| json!({
            "name": l.name,
            "mode": l.mode.as_str(),
            "weight": l.weight,
            "weight_param": l.weight_param,
            "values": l.values.iter().map(|(t, v)| json!({ "target": target_json(t), "value": v })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "clamp": layers.clamp,
        "normalized_sets": layers.normalized_sets.iter().map(|s| json!({
            "name": s.name,
            "targets": s.targets.iter().map(target_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// A clip's morph tracks for the agent: the node each names (`null` for a track that applies to
/// every morph mesh), its target index and key count.
pub(crate) fn morph_tracks_json(clip: &AnimationClip) -> Value {
    Value::Array(
        clip.morph_tracks
            .iter()
            .map(|track| {
                json!({
                    "node": track.node.as_ref().map(|n| json!({ "index": n.index, "name": n.name })),
                    "target": track.target,
                    "keys": track.weights.key_count(),
                })
            })
            .collect(),
    )
}

/// What drives a node entity's per-node morph tracks: the nearest [`NodeMorphPlayer`] or
/// [`Animator`] on it or an ancestor.
fn node_driver_json(world: &World, entity: Entity) -> Value {
    let mut cursor = Some(entity);
    let mut hops = 0usize;
    while let Some(current) = cursor {
        if let Some(player) = world.get::<NodeMorphPlayer>(current) {
            return json!({
                "kind": "player",
                "entity": current.to_bits(),
                "clip": player.clip.name,
                "time": player.time,
                "playing": player.playing,
                "tracks": morph_tracks_json(&player.clip),
            });
        }
        if let Some(animator) = world.get::<Animator>(current) {
            return match &animator.source {
                AnimatorSource::SingleClip { clip, .. } => json!({
                    "kind": "animator",
                    "entity": current.to_bits(),
                    "clip": clip.name,
                    "tracks": morph_tracks_json(clip),
                }),
                AnimatorSource::Graph { .. } => json!({ "kind": "animator", "entity": current.to_bits() }),
            };
        }
        hops += 1;
        if hops > 1024 {
            break;
        }
        cursor = world.get::<ChildOf>(current).map(ChildOf::parent);
    }
    Value::Null
}

fn entity_json(world: &World, entity: Entity) -> Value {
    let names = names_of(world, entity);
    let targets = world.get::<MeshMorphTargets>(entity).map_or(Value::Null, |t| {
        json!({
            "count": t.targets.len(),
            "vertex_count": t.targets.vertex_count(),
            "moved_vertices": t.targets.moved_vertex_count(),
            "gpu_bytes": t.targets.gpu_bytes(),
            "mode": match t.mode {
                regia_render::MorphMode::Game => "game",
                regia_render::MorphMode::Creator => "creator",
            },
        })
    });
    json!({
        "entity": entity.to_bits(),
        "names": names.map(|n| n.0.to_vec()),
        "weights": world.get::<MorphWeights>(entity).map(|w| w.0.clone()),
        "animated": world.get::<Animator>(entity).is_some(),
        "source_node": world
            .get::<SourceNode>(entity)
            .map(|n| json!({ "index": n.index, "name": n.name })),
        "node_driver": node_driver_json(world, entity),
        "layers": world.get::<MorphLayers>(entity).map_or(Value::Null, layers_json),
        "targets": targets,
    })
}

fn stats_json(world: &World) -> Value {
    world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map_or(Value::Null, |renderer| {
            let s = renderer.0.morph_stats();
            json!({
                "instances": s.instances,
                "drawn": s.drawn,
                "dispatched": s.dispatched,
                "dispatched_vertices": s.dispatched_vertices,
                "mismatched": s.mismatched,
                "waiting_pipeline": s.waiting_pipeline,
                "gpu_bytes": s.gpu_bytes,
                "buffers_created": s.buffers_created,
                "motion_instances": s.motion_instances,
                "counted": s.counted,
            })
        })
}

/// `anim.morph_weights { entity? }`: one entity's morph state, or every entity with morph
/// weights, targets or layers plus the renderer's counters for its last frame.
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("entity").is_some() {
        let entity = existing_entity(world, params)?;
        return Ok(entity_json(world, entity));
    }
    let mut query =
        world.query_filtered::<Entity, Or<(With<MorphWeights>, With<MeshMorphTargets>, With<MorphLayers>)>>();
    let mut entities: Vec<Entity> = query.iter(world).collect();
    entities.sort_by_key(|e| e.to_bits());
    let world: &World = world;
    let instances: Vec<Value> = entities.into_iter().map(|e| entity_json(world, e)).collect();
    Ok(json!({ "count": instances.len(), "instances": instances, "renderer": stats_json(world) }))
}

/// `(target, value)` pairs from `weights`: an object (`{"jawOpen": 0.4, "3": 1}`) or an array
/// (index order). Names are checked against `names`.
fn parse_weights(
    value: &Value,
    names: Option<&MorphTargetNames>,
) -> Result<Vec<(MorphTarget, f32)>, RpcError> {
    let number = |v: &Value, key: &str| -> Result<f32, RpcError> {
        v.as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("weight `{key}` must be a finite number")))
    };
    let mut out = Vec::new();
    match value {
        Value::Array(items) => {
            if items.len() > MAX_MORPH_TARGETS as usize {
                return Err(RpcError::invalid_params(format!(
                    "at most {MAX_MORPH_TARGETS} weights; got {}",
                    items.len()
                )));
            }
            for (i, v) in items.iter().enumerate() {
                out.push((MorphTarget::Index(i as u32), number(v, &i.to_string())?));
            }
        }
        Value::Object(map) => {
            for (key, v) in map {
                let target = match key.parse::<u32>() {
                    Ok(index) if index >= MAX_MORPH_TARGETS => {
                        return Err(RpcError::invalid_params(format!(
                            "morph target index {index} is out of range (at most {})",
                            MAX_MORPH_TARGETS - 1
                        )));
                    }
                    Ok(index) => MorphTarget::Index(index),
                    Err(_) => {
                        let known = names.is_some_and(|n| n.index_of(key).is_some());
                        if !known {
                            let list = names.map(|n| n.0.join(", ")).unwrap_or_default();
                            return Err(RpcError::invalid_params(format!(
                                "unknown morph target `{key}` (targets: [{list}])"
                            )));
                        }
                        MorphTarget::Name(key.clone())
                    }
                };
                out.push((target, number(v, key)?));
            }
        }
        _ => {
            return Err(RpcError::invalid_params(
                "`weights` must be an object (name or index → weight) or an array",
            ));
        }
    }
    Ok(out)
}

fn parse_target(value: &Value) -> Result<MorphTarget, RpcError> {
    match value {
        Value::String(name) => Ok(MorphTarget::Name(name.clone())),
        Value::Number(n) => n
            .as_u64()
            .and_then(|i| u32::try_from(i).ok())
            .filter(|i| *i < MAX_MORPH_TARGETS)
            .map(MorphTarget::Index)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "a target index must be an integer in 0..{MAX_MORPH_TARGETS}"
                ))
            }),
        _ => Err(RpcError::invalid_params("a target is a name or an index")),
    }
}

/// `anim.set_morph_weights { entity, weights?, layer?, mode?, layer_weight?, weight_param?,
/// remove_layer?, clamp?, normalized_sets? }`.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = existing_entity(world, params)?;
    let before = entity_json(world, entity);
    let names = names_of(world, entity);
    let weights = params
        .get("weights")
        .map(|w| parse_weights(w, names.as_ref()))
        .transpose()?;
    let layer_name = crate::params::opt_str(params, "layer")?;
    // A node entity under a per-node morph driver is rewritten every frame too.
    let node_driven = world.get::<SourceNode>(entity).is_some() && !node_driver_json(world, entity).is_null();
    let composed =
        world.get::<Animator>(entity).is_some() || world.get::<MorphLayers>(entity).is_some() || node_driven;
    let wants_layers = layer_name.is_some()
        || [
            "remove_layer",
            "clamp",
            "normalized_sets",
            "layer_weight",
            "weight_param",
            "mode",
        ]
        .iter()
        .any(|key| params.get(*key).is_some());
    let path = if composed || wants_layers {
        let mut layers = world.get::<MorphLayers>(entity).cloned().unwrap_or_default();
        let name = layer_name.unwrap_or(AGENT_LAYER);
        if crate::params::opt_bool(params, "remove_layer")? == Some(true) {
            layers.layers.retain(|l| l.name != name);
        } else {
            let mode = match crate::params::opt_str(params, "mode")? {
                None | Some("override") => MorphLayerMode::Override,
                Some("additive") => MorphLayerMode::Additive,
                Some(other) => {
                    return Err(RpcError::invalid_params(format!(
                        "`mode` is override or additive, not `{other}`"
                    )));
                }
            };
            let layer: &mut MorphLayer = layers.layer_mut(name, mode);
            if params.get("mode").is_some() {
                layer.mode = mode;
            }
            if let Some(w) = crate::params::opt_f64(params, "layer_weight")? {
                layer.weight = (w as f32).clamp(0.0, 1.0);
            }
            match params.get("weight_param") {
                Some(Value::String(param)) => layer.weight_param = Some(param.clone()),
                Some(Value::Null) => layer.weight_param = None,
                _ => {}
            }
            for (target, value) in weights.unwrap_or_default() {
                layer.set(target, value);
            }
        }
        match params.get("clamp") {
            Some(Value::Null) => layers.clamp = None,
            Some(Value::Array(pair)) if pair.len() == 2 => {
                let lo = pair[0].as_f64().unwrap_or(0.0) as f32;
                let hi = pair[1].as_f64().unwrap_or(1.0) as f32;
                layers.clamp = Some([lo, hi]);
            }
            Some(_) => return Err(RpcError::invalid_params("`clamp` is [min, max] or null")),
            None => {}
        }
        if let Some(sets) = params.get("normalized_sets") {
            let sets = sets
                .as_array()
                .ok_or_else(|| RpcError::invalid_params("`normalized_sets` is an array"))?;
            layers.normalized_sets = sets
                .iter()
                .map(|set| {
                    let targets = set
                        .get("targets")
                        .and_then(Value::as_array)
                        .ok_or_else(|| RpcError::invalid_params("a normalized set needs `targets`"))?
                        .iter()
                        .map(parse_target)
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(MorphSet {
                        name: crate::params::opt_str(set, "name")?
                            .unwrap_or_default()
                            .to_string(),
                        targets,
                    })
                })
                .collect::<Result<Vec<_>, RpcError>>()?;
        }
        world.entity_mut(entity).insert(layers);
        "layer"
    } else {
        let Some(weights) = weights else {
            return Err(RpcError::invalid_params("give `weights` (or a layer edit)"));
        };
        let mut current = world.get::<MorphWeights>(entity).cloned().unwrap_or_default();
        if let Some(count) = names.as_ref().map(MorphTargetNames::len)
            && current.0.len() < count
        {
            current.0.resize(count, 0.0);
        }
        for (target, value) in weights {
            if let Some(index) = target.resolve(names.as_ref()) {
                current.set(index, value);
            }
        }
        world.entity_mut(entity).insert(current);
        "weights"
    };
    Ok(json!({
        "path": path,
        "before": before,
        "after": entity_json(world, entity),
        "note": if path == "layer" { "layers compose into weights at the next frame's animation apply" } else { "" },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_target_indices_are_refused_before_any_allocation() {
        let (mut world, entity) = world_with_mesh();
        let bits = entity.to_bits();
        for weights in [
            json!({ "4294967295": 1.0 }),
            json!({ "4096": 1.0 }),
            json!(vec![0.5; 4097]),
        ] {
            let error = set(&mut world, &json!({ "entity": bits, "weights": weights }))
                .expect_err("index past the cap");
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        }
        assert_eq!(
            world.get::<MorphWeights>(entity).map(|w| w.0.len()),
            Some(3),
            "weights untouched"
        );
        assert!(parse_target(&json!(4096)).is_err());
        assert!(parse_target(&json!(4095)).is_ok());
    }

    fn world_with_mesh() -> (World, Entity) {
        let mut world = World::new();
        let entity = world
            .spawn((
                MorphTargetNames::new(["aa", "oh", "blink"]),
                MorphWeights(vec![0.0; 3]),
            ))
            .id();
        (world, entity)
    }

    #[test]
    fn plain_entities_get_weights_by_name_and_index() {
        let (mut world, entity) = world_with_mesh();
        let out = set(
            &mut world,
            &json!({ "entity": entity.to_bits(), "weights": { "oh": 0.5, "2": 1.0 } }),
        )
        .expect("set");
        assert_eq!(out["path"], "weights");
        assert_eq!(
            world.get::<MorphWeights>(entity).expect("weights").0,
            vec![0.0, 0.5, 1.0]
        );
        let err = set(
            &mut world,
            &json!({ "entity": entity.to_bits(), "weights": { "smile": 1 } }),
        );
        assert!(err.is_err(), "unknown names are refused");
        let read = get(&mut world, &json!({ "entity": entity.to_bits() })).expect("get");
        assert_eq!(read["names"], json!(["aa", "oh", "blink"]));
        let list = get(&mut world, &json!({})).expect("list");
        assert_eq!(list["count"], 1);
    }

    #[test]
    fn layer_edits_build_morph_layers() {
        let (mut world, entity) = world_with_mesh();
        let out = set(
            &mut world,
            &json!({
                "entity": entity.to_bits(),
                "layer": "happy",
                "mode": "additive",
                "layer_weight": 0.5,
                "weights": { "aa": 1.0 },
                "normalized_sets": [{ "name": "visemes", "targets": ["aa", "oh"] }],
            }),
        )
        .expect("layer");
        assert_eq!(out["path"], "layer");
        let layers = world.get::<MorphLayers>(entity).expect("layers");
        let layer = layers.layer("happy").expect("happy");
        assert_eq!(layer.mode, MorphLayerMode::Additive);
        assert_eq!(layer.weight, 0.5);
        assert_eq!(layer.values, vec![(MorphTarget::Name("aa".into()), 1.0)]);
        assert_eq!(layers.normalized_sets.len(), 1);
        set(
            &mut world,
            &json!({ "entity": entity.to_bits(), "layer": "happy", "remove_layer": true }),
        )
        .expect("remove");
        assert!(
            world
                .get::<MorphLayers>(entity)
                .expect("layers")
                .layer("happy")
                .is_none()
        );
    }

    #[test]
    fn node_entities_report_their_node_and_driver() {
        use regia_animation::clip::{MorphNode, MorphTrack, TrackCurve};
        use std::sync::Arc;

        let mut clip = AnimationClip::empty("both", 1.0);
        let node = MorphNode {
            index: 4,
            name: "head".into(),
        };
        clip.morph_tracks
            .push(MorphTrack::for_node(node, 1, TrackCurve::Static(0.5)));
        let mut world = World::new();
        let root = world.spawn(NodeMorphPlayer::new(Arc::new(clip), true)).id();
        let mesh = world
            .spawn((
                SourceNode {
                    index: 4,
                    name: "head".into(),
                },
                MorphTargetNames::new(["aa", "oh"]),
                MorphWeights(vec![0.0; 2]),
                ChildOf(root),
            ))
            .id();
        let read = get(&mut world, &json!({ "entity": mesh.to_bits() })).expect("get");
        assert_eq!(read["source_node"], json!({ "index": 4, "name": "head" }));
        assert_eq!(read["node_driver"]["kind"], "player");
        assert_eq!(read["node_driver"]["clip"], "both");
        assert_eq!(
            read["node_driver"]["tracks"],
            json!([{ "node": { "index": 4, "name": "head" }, "target": 1, "keys": 1 }])
        );
        // The player rewrites the weights every frame, so agent weights go to a layer.
        let out = set(
            &mut world,
            &json!({ "entity": mesh.to_bits(), "weights": { "aa": 1.0 } }),
        )
        .expect("set");
        assert_eq!(out["path"], "layer");
    }
}
