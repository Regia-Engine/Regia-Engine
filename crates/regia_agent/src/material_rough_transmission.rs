// SPDX-License-Identifier: MPL-2.0
//! Agent methods for rough transmission (`book/agents/rough-transmission.md`, ADR-2075):
//! `material.rough_transmission` reads how a mesh entity's material spreads what it shows behind it,
//! `material.set_rough_transmission` edits it.
//!
//! The roughness belongs to the entity's material (`LabMaterial::transmission_roughness`). Editing it
//! gives the entity its own copy of the material, so entities that shared the old one keep theirs.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::rough_transmission::{FORWARD_TAPS, effective_roughness, spread_radians};
use regia_render::{CpuMaterial, MeshRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "rough transmission goes on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

/// Why the material's roughness does nothing (`null` when it draws).
fn unsupported(material: &CpuMaterial) -> Value {
    if material.transmission <= 0.0 {
        json!("no_transmission")
    } else if material.hair.enabled {
        json!("hair")
    } else if material.surface_graph.is_some() {
        json!("material_graph")
    } else {
        Value::Null
    }
}

fn state_json(material: &CpuMaterial) -> Value {
    let roughness = effective_roughness(material);
    json!({
        "roughness": material.transmission_roughness,
        "active": roughness > 0.0,
        "unsupported": unsupported(material),
        "ior": material.ior,
        "spread_radians": spread_radians(roughness, material.ior),
        "forward_taps": FORWARD_TAPS,
    })
}

/// `material.rough_transmission { entity }`: the roughness, whether it draws and why not, and the
/// lobe's size. The frame also needs `FrameQuality::screen_space_refraction`.
pub fn get(world: &World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(renderer) = world.get::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let mut out = state_json(&renderer.material);
    if let Value::Object(map) = &mut out {
        map.insert("entity".into(), json!(entity.to_bits()));
    }
    Ok(out)
}

/// `material.set_rough_transmission { entity, roughness }`: edits the roughness and returns
/// `{ entity, before, after }`.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let roughness = crate::params::opt_f64(params, "roughness")?
        .filter(|v| (0.0..=1.0).contains(v))
        .ok_or_else(|| RpcError::invalid_params("`roughness` must be a number in 0..=1"))?
        as f32;
    let Some(mut renderer) = world.get_mut::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let before = state_json(&renderer.material);
    if renderer.material.transmission_roughness != roughness {
        // A new `Arc` is a new material upload; the entities sharing the old one are unchanged.
        let mut material = (*renderer.material).clone();
        material.transmission_roughness = roughness;
        renderer.material = Arc::new(material);
    }
    let after = state_json(&renderer.material);
    Ok(json!({ "entity": entity.to_bits(), "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMesh, LabVertex};

    fn world_with_mesh(material: CpuMaterial) -> (World, Entity, Arc<CpuMaterial>) {
        let mut world = World::new();
        let shared = Arc::new(material);
        let vertices = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        let entity = world
            .spawn(MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices,
                    indices: vec![0, 1, 2],
                }),
                shared.clone(),
            ))
            .id();
        (world, entity, shared)
    }

    fn glass() -> CpuMaterial {
        CpuMaterial {
            transmission: 1.0,
            ..CpuMaterial::default()
        }
    }

    #[test]
    fn set_edits_a_copy_and_reads_back() {
        let (mut world, e, shared) = world_with_mesh(glass());
        let params = json!({ "entity": e.to_bits(), "roughness": 0.4 });
        let out = set(&mut world, &params).expect("set");
        assert_eq!(out["before"]["active"], false);
        assert_eq!(out["after"]["active"], true);
        assert_eq!(
            shared.transmission_roughness, 0.0,
            "the shared material is untouched"
        );
        let read = get(&world, &json!({ "entity": e.to_bits() })).expect("get");
        assert!(read["spread_radians"].as_f64().expect("number") > 0.05);
        assert!(read["unsupported"].is_null());
    }

    #[test]
    fn unsupported_names_the_reason_and_bad_values_are_refused() {
        let (mut world, e, _) = world_with_mesh(CpuMaterial::default());
        let read = get(&world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(read["unsupported"], "no_transmission");
        for bad in [json!(1.5), json!(-0.1), json!("x"), Value::Null] {
            let params = json!({ "entity": e.to_bits(), "roughness": bad });
            assert!(set(&mut world, &params).is_err());
        }
        assert!(get(&world, &json!({ "entity": 12345 })).is_err());
    }
}
