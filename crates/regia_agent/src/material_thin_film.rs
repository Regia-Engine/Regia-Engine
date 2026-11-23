// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the thin film (`book/agents/thin-film.md`, ADR-1977): `material.thin_film`
//! reads a mesh entity's film and what the renderer did with films in the last frame,
//! `material.set_thin_film` edits it.
//!
//! The film belongs to the entity's material (`LabMaterial::thin_film`). Editing it gives the
//! entity its own copy of the material, so entities that shared the old one keep theirs.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::{CpuMaterial, MeshRenderer, ThinFilmFactors};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "a thin film goes on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

/// Why this material's own fields keep the forward pass from drawing its film (`null` when they
/// do not). A mask, a lightmap, an x-ray override and a Mobile lite opaque draw are instance or
/// frame state this check does not see, so `active` can stay true while that draw drops the film.
/// A transmissive material, including per-instance glass, is `transmissive`.
fn unsupported(material: &CpuMaterial) -> Value {
    if material.surface_graph.is_some() {
        json!("material_graph")
    } else if material.transmission > 0.0 {
        json!("transmissive")
    } else {
        Value::Null
    }
}

/// The film of one material: its values, whether it draws, and why not.
pub(crate) fn film_json(material: &CpuMaterial) -> Value {
    let film = material.thin_film;
    let reason = unsupported(material);
    json!({
        "enabled": film.is_on(),
        "active": film.is_on() && reason.is_null(),
        "unsupported": reason,
        "factor": film.factor,
        "ior": film.ior,
        "thickness_min": film.thickness_min,
        "thickness_max": film.thickness_max,
        "has_map": material.thin_film_map.is_some(),
    })
}

/// `material.thin_film { entity }`: the entity's film, plus the last frame's `renderer` counts
/// (`null` without a renderer).
pub fn get(world: &World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(renderer) = world.get::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let mut out = film_json(&renderer.material);
    if let Value::Object(map) = &mut out {
        map.insert("entity".into(), json!(entity.to_bits()));
        let stats = world
            .get_resource::<regia_render::RuntimeRenderer>()
            .map_or(Value::Null, |r| {
                let s = r.0.thin_film_stats();
                json!({
                    "materials": s.materials,
                    "drawn": s.drawn,
                    "fallback": s.fallback,
                    "unsupported": s.unsupported,
                })
            });
        map.insert("renderer".into(), stats);
    }
    Ok(out)
}

fn number(params: &Map<String, Value>, field: &str, current: f32) -> Result<f32, RpcError> {
    match params.get(field) {
        None => Ok(current),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .filter(|v| v.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a finite f32 number"))),
    }
}

/// The film `params` asks for on top of `current`.
fn edited(params: &Value, current: ThinFilmFactors) -> Result<ThinFilmFactors, RpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    let film = ThinFilmFactors {
        factor: number(object, "factor", current.factor)?,
        ior: number(object, "ior", current.ior)?,
        thickness_min: number(object, "thickness_min", current.thickness_min)?,
        thickness_max: number(object, "thickness_max", current.thickness_max)?,
    };
    if !(0.0..=1.0).contains(&film.factor) {
        return Err(RpcError::invalid_params("`factor` must be in 0..=1"));
    }
    if film.ior < 1.0 {
        return Err(RpcError::invalid_params("`ior` must be at least 1"));
    }
    if film.thickness_min < 0.0 || film.thickness_max < 0.0 {
        return Err(RpcError::invalid_params(
            "`thickness_min` and `thickness_max` are nanometres, at least 0",
        ));
    }
    Ok(film)
}

/// `material.set_thin_film { entity, factor?, ior?, thickness_min?, thickness_max? }`: edits the
/// film on the entity's material and returns `{ entity, before, after }` (each as in [`get`],
/// without the renderer's counts). An omitted field keeps its value.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(mut renderer) = world.get_mut::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let film = edited(params, renderer.material.thin_film)?;
    let before = film_json(&renderer.material);
    if renderer.material.thin_film != film {
        // A new `Arc` is a new material upload; the entities sharing the old one are unchanged.
        let mut material = (*renderer.material).clone();
        material.thin_film = film;
        renderer.material = Arc::new(material);
    }
    let after = film_json(&renderer.material);
    Ok(json!({ "entity": entity.to_bits(), "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMesh, LabVertex};

    #[test]
    fn huge_thickness_is_rejected_and_keeps_the_material() {
        let (mut world, entity, shared) = world_with_mesh(CpuMaterial::default());
        let before = get(&world, &json!({ "entity": entity.to_bits() })).expect("before");
        for field in ["thickness_min", "thickness_max", "ior", "factor"] {
            let mut params = json!({ "entity": entity.to_bits(), "factor": 1.0 });
            params[field] = json!(1.0e39);
            let err = set(&mut world, &params).expect_err("overflow");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
            assert!(Arc::ptr_eq(
                &shared,
                &world.get::<MeshRenderer>(entity).expect("mesh").material
            ));
            assert_eq!(
                get(&world, &json!({ "entity": entity.to_bits() })).expect("after"),
                before
            );
        }
    }

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

    #[test]
    fn set_edits_a_copy_keeps_omitted_fields_and_reads_back() {
        let (mut world, e, shared) = world_with_mesh(CpuMaterial::default());
        let id = e.to_bits();
        let got = get(&world, &json!({ "entity": id })).expect("get");
        assert_eq!(got["enabled"], false);
        assert_eq!(got["active"], false);
        assert_eq!(got["unsupported"], Value::Null);
        assert_eq!(got["ior"], f64::from(1.3f32));
        assert_eq!(got["thickness_max"], 400.0);
        assert_eq!(got["has_map"], false);
        assert_eq!(got["renderer"], Value::Null);
        let out = set(
            &mut world,
            &json!({ "entity": id, "factor": 1.0, "thickness_max": 520.0 }),
        )
        .expect("set");
        assert_eq!(out["before"]["enabled"], false);
        assert_eq!(out["after"]["enabled"], true);
        assert_eq!(out["after"]["active"], true);
        assert_eq!(out["after"]["thickness_max"], 520.0);
        assert_eq!(
            out["after"]["thickness_min"], 100.0,
            "omitted fields keep their value"
        );
        // The shared material is untouched; the entity holds a copy.
        assert_eq!(shared.thin_film, ThinFilmFactors::OFF);
        let held = world.get::<MeshRenderer>(e).expect("renderer").material.clone();
        assert_eq!(held.thin_film.thickness_max, 520.0);
        // Setting the same values again keeps the `Arc`.
        set(&mut world, &json!({ "entity": id, "factor": 1.0 })).expect("again");
        assert!(Arc::ptr_eq(
            &held,
            &world.get::<MeshRenderer>(e).expect("renderer").material
        ));
    }

    #[test]
    fn a_material_the_variant_cannot_draw_says_why() {
        let (mut world, e, _) = world_with_mesh(CpuMaterial {
            transmission: 0.5,
            thin_film: ThinFilmFactors {
                factor: 1.0,
                ..ThinFilmFactors::OFF
            },
            ..CpuMaterial::default()
        });
        let got = get(&world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["enabled"], true);
        assert_eq!(got["active"], false);
        assert_eq!(got["unsupported"], "transmissive");
        let out = set(&mut world, &json!({ "entity": e.to_bits(), "factor": 0.0 })).expect("off");
        assert_eq!(out["after"]["enabled"], false);
    }

    #[test]
    fn bad_params_are_rejected() {
        let (mut world, e, shared) = world_with_mesh(CpuMaterial::default());
        let id = e.to_bits();
        for bad in [
            json!({ "entity": id, "factor": 1.5 }),
            json!({ "entity": id, "factor": "lots" }),
            json!({ "entity": id, "ior": 0.5 }),
            json!({ "entity": id, "thickness_min": -1.0 }),
            json!({ "entity": id, "thickness_max": null }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world.get::<MeshRenderer>(e).expect("r").material.thin_film,
            shared.thin_film
        );
        assert!(get(&world, &json!({})).is_err());
        assert!(get(&world, &json!({ "entity": 4_294_967_301u64 })).is_err());
    }
}
