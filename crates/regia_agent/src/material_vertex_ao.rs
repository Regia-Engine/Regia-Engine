// SPDX-License-Identifier: MPL-2.0
//! Agent methods for per-vertex ambient occlusion (`book/agents/vertex-ao.md`, ADR-1512):
//! `material.vertex_ao` reads a mesh entity's flag and the occlusion range its mesh carries,
//! `material.set_vertex_ao` turns the flag on or off.
//!
//! The flag belongs to the entity's material (`LabMaterial::vertex_ao`). Turning it on or off gives
//! the entity its own copy of the material, so entities that shared the old one keep their
//! setting.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::MeshRenderer;
use regia_render::vertex_ao;
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "vertex occlusion goes on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

/// The state of one mesh entity: the flag, whether the renderer honors it, and the range of the
/// mesh's vertex color alpha (`null` for a mesh without vertices).
fn state(renderer: &MeshRenderer) -> Value {
    let alpha = vertex_ao::alpha_range(&renderer.mesh.vertices);
    json!({
        "enabled": renderer.material.vertex_ao,
        "active": vertex_ao::is_active(&renderer.material),
        "alpha_min": alpha.map(|r| r.0),
        "alpha_max": alpha.map(|r| r.1),
        "has_ramp": alpha.is_some_and(|(lo, hi)| hi > lo),
    })
}

/// `material.vertex_ao { entity }`: `{ entity, enabled, active, alpha_min, alpha_max,
/// has_ramp }`. `active` is false for a masked, blended, unlit, hair or transmissive
/// material, where the vertex alpha stays an opacity.
pub fn get(world: &World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(renderer) = world.get::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let mut out = state(renderer);
    if let Value::Object(map) = &mut out {
        map.insert("entity".into(), json!(entity.to_bits()));
    }
    Ok(out)
}

/// `material.set_vertex_ao { entity, enabled }`: sets the flag on the entity's material and
/// returns `{ entity, before, after }` (each as in [`get`]).
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let enabled = params
        .get("enabled")
        .ok_or_else(|| RpcError::invalid_params("pass `enabled` (boolean)"))?
        .as_bool()
        .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?;
    let Some(mut renderer) = world.get_mut::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let before = state(&renderer);
    if renderer.material.vertex_ao != enabled {
        // A new `Arc` is a new material upload; the entities sharing the old one are unchanged.
        let mut material = (*renderer.material).clone();
        material.vertex_ao = enabled;
        renderer.material = Arc::new(material);
    }
    let after = state(&renderer);
    Ok(json!({ "entity": entity.to_bits(), "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMaterial, CpuMesh, LabVertex};

    fn world_with_ramp() -> (World, Entity, Arc<CpuMaterial>) {
        let mut world = World::new();
        let shared = Arc::new(CpuMaterial::default());
        let mut vertices = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        vertices[0].color[3] = 0.25;
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

    #[test]
    fn set_round_trips_gives_the_entity_its_own_material_and_reports_the_ramp() {
        let (mut world, e, shared) = world_with_ramp();
        let got = get(&world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["enabled"], false);
        assert_eq!(got["active"], false);
        assert_eq!(got["alpha_min"], 0.25);
        assert_eq!(got["alpha_max"], 1.0);
        assert_eq!(got["has_ramp"], true);
        let out = set(&mut world, &json!({ "entity": e.to_bits(), "enabled": true })).expect("set");
        assert_eq!(out["before"]["enabled"], false);
        assert_eq!(out["after"]["enabled"], true);
        assert_eq!(out["after"]["active"], true);
        // The shared material is untouched; the entity holds a copy.
        assert!(!shared.vertex_ao);
        assert!(world.get::<MeshRenderer>(e).expect("renderer").material.vertex_ao);
        // Setting the same value again keeps the Arc.
        let before = world.get::<MeshRenderer>(e).expect("renderer").material.clone();
        set(&mut world, &json!({ "entity": e.to_bits(), "enabled": true })).expect("again");
        assert!(Arc::ptr_eq(
            &before,
            &world.get::<MeshRenderer>(e).expect("renderer").material
        ));
    }

    #[test]
    fn bad_params_are_rejected() {
        let (mut world, e, _) = world_with_ramp();
        assert!(set(&mut world, &json!({ "entity": e.to_bits() })).is_err());
        assert!(set(&mut world, &json!({ "entity": e.to_bits(), "enabled": "yes" })).is_err());
        assert!(get(&world, &json!({})).is_err());
        assert!(get(&world, &json!({ "entity": 4_294_967_301u64 })).is_err());
    }
}
