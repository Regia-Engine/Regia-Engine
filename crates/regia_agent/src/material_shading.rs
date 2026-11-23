// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the character shading models (`book/agents/shading-models.md`):
//! `material.shading` reads a mesh entity's model (or lists every shaded entity with the frame's
//! table and the renderer's count) and `material.set_shading` sets the skin, eye or standard
//! model, its parameters and a fabric sheen lobe, or removes the component.
//!
//! The model belongs to the entity's material: entities that share its `Arc` draw with the first
//! one's shading, and `material.shading` lists them under `shares_material_with`.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::MeshRenderer;
use regia_render::shading_model::{
    ExtractedMaterialShading, EyeShading, MaterialShading, ShadingModel, SkinShading,
};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "a shading model goes on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

fn skin_json(s: &SkinShading) -> Value {
    json!({
        "scatter_scale": s.scatter_scale,
        "curvature_radius_mm": s.curvature_radius_mm,
        "normal_blur": s.normal_blur,
        "transmission": s.transmission,
        "thickness_scale": s.thickness_scale,
        "secondary_roughness": s.secondary_roughness,
        "secondary_weight": s.secondary_weight,
        "mm_per_unit": s.mm_per_unit,
    })
}

fn eye_json(e: &EyeShading) -> Value {
    json!({
        "iris_center": e.iris_center,
        "iris_radius": e.iris_radius,
        "limbus_width": e.limbus_width,
        "limbus_darkening": e.limbus_darkening,
        "iris_depth": e.iris_depth,
        "cornea_ior": e.cornea_ior,
        "cornea_roughness": e.cornea_roughness,
        "socket_occlusion": e.socket_occlusion,
        "occlusion_start": e.occlusion_start,
        "occlusion_end": e.occlusion_end,
    })
}

fn shading_json(shading: Option<&MaterialShading>) -> Value {
    let Some(s) = shading else {
        return json!({ "model": "standard", "sheen": Value::Null });
    };
    let mut out = Map::new();
    out.insert("model".into(), json!(s.model.name()));
    match &s.model {
        ShadingModel::Standard => {}
        ShadingModel::Skin(skin) => {
            out.insert("skin".into(), skin_json(skin));
        }
        ShadingModel::Eye(eye) => {
            out.insert("eye".into(), eye_json(eye));
        }
    }
    out.insert(
        "sheen".into(),
        s.sheen.map_or(
            Value::Null,
            |sheen| json!({ "color": sheen.color, "roughness": sheen.roughness }),
        ),
    );
    Value::Object(out)
}

fn entity_json(world: &World, entity: Entity) -> Value {
    let shading = world.get::<MaterialShading>(entity);
    let mut out = shading_json(shading);
    let material = world.get::<MeshRenderer>(entity).map(|r| r.material.clone());
    if let (Value::Object(map), Some(material)) = (&mut out, material) {
        map.insert("entity".into(), json!(entity.to_bits()));
        map.insert(
            "thin_film".into(),
            crate::material_thin_film::film_json(&material),
        );
        map.insert(
            "material_key".into(),
            json!(regia_render::shading_model::material_key(&material)),
        );
        let mut others = Vec::new();
        let mut query = world.try_query::<(Entity, &MeshRenderer)>();
        if let Some(query) = query.as_mut() {
            for (other, renderer) in query.iter(world) {
                if other != entity && Arc::ptr_eq(&renderer.material, &material) {
                    others.push(other.to_bits());
                }
            }
        }
        others.sort_unstable();
        map.insert("shares_material_with".into(), json!(others));
    }
    out
}

/// `material.shading { entity? }`: one mesh entity's shading model, or every entity with a
/// [`MaterialShading`] plus the last frame's table and the renderer's count.
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("entity").is_some() {
        let entity = mesh_entity(world, params)?;
        return Ok(entity_json(world, entity));
    }
    let mut query = world.query::<(Entity, &MaterialShading)>();
    let mut entities: Vec<Entity> = query.iter(world).map(|(entity, _)| entity).collect();
    entities.sort_unstable_by_key(|e| e.to_bits());
    let listed: Vec<Value> = entities.iter().map(|&e| entity_json(world, e)).collect();
    let table = world.get_resource::<ExtractedMaterialShading>().map_or(
        Value::Null,
        |t| json!({ "materials": t.entries.len(), "shaded_draws": t.shaded_draws, "conflicts": t.conflicts }),
    );
    let renderer = world.get_resource::<regia_render::RuntimeRenderer>().map_or(
        Value::Null,
        |r| json!({ "materials": r.0.material_shading_count() }),
    );
    Ok(json!({ "entities": listed, "table": table, "renderer": renderer }))
}

fn number(object: &Map<String, Value>, field: &str, current: f32) -> Result<f32, RpcError> {
    match object.get(field) {
        None => Ok(current),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number"))),
    }
}

fn pair(object: &Map<String, Value>, field: &str, current: [f32; 2]) -> Result<[f32; 2], RpcError> {
    match object.get(field) {
        None => Ok(current),
        Some(v) => {
            let values: Option<Vec<f32>> = v.as_array().filter(|a| a.len() == 2).and_then(|a| {
                a.iter()
                    .map(|x| x.as_f64().filter(|x| x.is_finite()).map(|x| x as f32))
                    .collect()
            });
            values
                .map(|v| [v[0], v[1]])
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be [u, v]")))
        }
    }
}

fn rgb(value: &Value) -> Result<[f32; 3], RpcError> {
    if let Some(hex) = value.as_str() {
        let c = regia_core::math::Color::from_hex(hex)
            .map_err(|e| RpcError::invalid_params(format!("`sheen.color`: {e}")))?;
        return Ok([c.r, c.g, c.b]);
    }
    let values: Option<Vec<f32>> = value.as_array().filter(|a| a.len() == 3).and_then(|a| {
        a.iter()
            .map(|x| {
                x.as_f64()
                    .filter(|x| x.is_finite() && *x >= 0.0)
                    .map(|x| x as f32)
            })
            .collect()
    });
    values.map(|v| [v[0], v[1], v[2]]).ok_or_else(|| {
        RpcError::invalid_params("`sheen.color` must be [r, g, b] (linear, ≥ 0) or \"#RRGGBB\"")
    })
}

fn object<'a>(params: &'a Value, field: &str) -> Result<Option<&'a Map<String, Value>>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_object()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an object"))),
    }
}

fn edit_skin(mut s: SkinShading, o: &Map<String, Value>) -> Result<SkinShading, RpcError> {
    s.scatter_scale = number(o, "scatter_scale", s.scatter_scale)?;
    s.curvature_radius_mm = number(o, "curvature_radius_mm", s.curvature_radius_mm)?;
    s.normal_blur = number(o, "normal_blur", s.normal_blur)?;
    s.transmission = number(o, "transmission", s.transmission)?;
    s.thickness_scale = number(o, "thickness_scale", s.thickness_scale)?;
    s.secondary_roughness = number(o, "secondary_roughness", s.secondary_roughness)?;
    s.secondary_weight = number(o, "secondary_weight", s.secondary_weight)?;
    s.mm_per_unit = number(o, "mm_per_unit", s.mm_per_unit)?;
    Ok(s)
}

fn edit_eye(mut e: EyeShading, o: &Map<String, Value>) -> Result<EyeShading, RpcError> {
    e.iris_center = pair(o, "iris_center", e.iris_center)?;
    e.iris_radius = number(o, "iris_radius", e.iris_radius)?;
    e.limbus_width = number(o, "limbus_width", e.limbus_width)?;
    e.limbus_darkening = number(o, "limbus_darkening", e.limbus_darkening)?;
    e.iris_depth = number(o, "iris_depth", e.iris_depth)?;
    e.cornea_ior = number(o, "cornea_ior", e.cornea_ior)?;
    e.cornea_roughness = number(o, "cornea_roughness", e.cornea_roughness)?;
    e.socket_occlusion = number(o, "socket_occlusion", e.socket_occlusion)?;
    e.occlusion_start = number(o, "occlusion_start", e.occlusion_start)?;
    e.occlusion_end = number(o, "occlusion_end", e.occlusion_end)?;
    Ok(e)
}

/// The shading `params` describe, starting from `current`.
fn edited(current: MaterialShading, params: &Value) -> Result<MaterialShading, RpcError> {
    let skin = object(params, "skin")?;
    let eye = object(params, "eye")?;
    let model_name =
        match params.get("model") {
            None => None,
            Some(v) => Some(v.as_str().ok_or_else(|| {
                RpcError::invalid_params("`model` must be \"standard\", \"skin\" or \"eye\"")
            })?),
        };
    let model_name = model_name.unwrap_or(match (skin.is_some(), eye.is_some()) {
        (true, false) => "skin",
        (false, true) => "eye",
        _ => current.model.name(),
    });
    let model = match model_name {
        "standard" => ShadingModel::Standard,
        "skin" => {
            let base = match current.model {
                ShadingModel::Skin(s) => s,
                _ => SkinShading::default(),
            };
            ShadingModel::Skin(match skin {
                Some(o) => edit_skin(base, o)?,
                None => base,
            })
        }
        "eye" => {
            let base = match current.model {
                ShadingModel::Eye(e) => e,
                _ => EyeShading::default(),
            };
            ShadingModel::Eye(match eye {
                Some(o) => edit_eye(base, o)?,
                None => base,
            })
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown model `{other}`: standard, skin or eye"
            )));
        }
    };
    let sheen = match params.get("sheen") {
        None => current.sheen,
        Some(Value::Null) => None,
        Some(v) => {
            let o = v
                .as_object()
                .ok_or_else(|| RpcError::invalid_params("`sheen` must be { color?, roughness? } or null"))?;
            let mut s = current.sheen.unwrap_or_default();
            if let Some(color) = o.get("color") {
                s.color = rgb(color)?;
            }
            s.roughness = number(o, "roughness", s.roughness)?;
            Some(s)
        }
    };
    Ok(MaterialShading { model, sheen })
}

/// `material.set_shading { entity, model?, skin?, eye?, sheen?, remove? }`: edits the entity's
/// [`MaterialShading`] (omitted parameters keep their value; a new model starts from its
/// defaults) or removes it.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let before = shading_json(world.get::<MaterialShading>(entity));
    if crate::params::opt_bool(params, "remove")? == Some(true) {
        world.entity_mut(entity).remove::<MaterialShading>();
        return Ok(json!({ "before": before, "after": shading_json(None) }));
    }
    let current = world.get::<MaterialShading>(entity).copied().unwrap_or_default();
    let next = edited(current, params)?;
    world.entity_mut(entity).insert(next);
    Ok(json!({ "before": before, "after": entity_json(world, entity) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMaterial, CpuMesh};

    fn world_with_meshes() -> (World, Entity, Entity) {
        let mut world = World::new();
        let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
        let mesh = Arc::new(CpuMesh { vertices, indices });
        let material = Arc::new(CpuMaterial::default());
        let a = world
            .spawn(MeshRenderer::new(mesh.clone(), material.clone()))
            .id();
        let b = world.spawn(MeshRenderer::new(mesh, material)).id();
        (world, a, b)
    }

    #[test]
    fn set_switches_models_edits_params_and_removes() {
        let (mut world, a, b) = world_with_meshes();
        let got = get(&mut world, &json!({ "entity": a.to_bits() })).expect("get");
        assert_eq!(got["model"], "standard");
        assert_eq!(got["shares_material_with"], json!([b.to_bits()]));
        set(
            &mut world,
            &json!({ "entity": a.to_bits(), "skin": { "scatter_scale": 2.0, "transmission": 0.5 } }),
        )
        .expect("skin");
        let s = *world.get::<MaterialShading>(a).expect("inserted");
        let ShadingModel::Skin(skin) = s.model else {
            panic!("not skin: {s:?}")
        };
        assert_eq!(skin.scatter_scale, 2.0);
        assert_eq!(skin.transmission, 0.5);
        assert_eq!(skin.normal_blur, SkinShading::default().normal_blur);
        // A sheen edit keeps the model.
        set(
            &mut world,
            &json!({ "entity": a.to_bits(), "sheen": { "color": "#FF8080", "roughness": 0.3 } }),
        )
        .expect("sheen");
        let s = *world.get::<MaterialShading>(a).expect("component");
        assert!(matches!(s.model, ShadingModel::Skin(k) if k.scatter_scale == 2.0));
        let sheen = s.sheen.expect("sheen");
        assert_eq!(sheen.roughness, 0.3);
        assert!((sheen.color[0] - 1.0).abs() < 1e-6 && sheen.color[1] < 0.5);
        // Switching to the eye starts from the eye defaults.
        let reply = set(
            &mut world,
            &json!({ "entity": a.to_bits(), "model": "eye", "eye": { "iris_depth": 0.0 } }),
        )
        .expect("eye");
        assert_eq!(reply["before"]["model"], "skin");
        assert_eq!(reply["after"]["model"], "eye");
        assert_eq!(reply["after"]["eye"]["iris_depth"], 0.0);
        assert_eq!(
            reply["after"]["eye"]["iris_radius"].as_f64().map(|v| v as f32),
            Some(EyeShading::default().iris_radius)
        );
        let listed = get(&mut world, &json!({})).expect("list");
        assert_eq!(listed["entities"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["renderer"], Value::Null, "no renderer in this world");
        let removed = set(&mut world, &json!({ "entity": a.to_bits(), "remove": true })).expect("remove");
        assert_eq!(removed["after"]["model"], "standard");
        assert!(world.get::<MaterialShading>(a).is_none());
    }

    #[test]
    fn bad_params_are_rejected() {
        let (mut world, a, _) = world_with_meshes();
        assert!(set(&mut world, &json!({ "entity": a.to_bits(), "model": "hair" })).is_err());
        assert!(
            set(
                &mut world,
                &json!({ "entity": a.to_bits(), "skin": { "normal_blur": "x" } })
            )
            .is_err()
        );
        assert!(
            set(
                &mut world,
                &json!({ "entity": a.to_bits(), "sheen": { "color": [1, 2] } })
            )
            .is_err()
        );
        let empty = world.spawn_empty().id();
        assert!(get(&mut world, &json!({ "entity": empty.to_bits() })).is_err());
        assert!(
            world.get::<MaterialShading>(a).is_none(),
            "a rejected edit changes nothing"
        );
    }
}
