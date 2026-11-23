// SPDX-License-Identifier: MPL-2.0
//! Agent methods for per-material foliage sway (`book/agents/foliage-wind.md`, spec 88 § 2.5,
//! ADR-2304): `material.sway` reads a mesh entity's sway, `material.set_sway` gives it one
//! (`regia_render::FoliageSway::adopt`), changes it, or takes it away.
//!
//! Like the other `material.*` setters, a change gives the entity its own copy of the material,
//! so entities that shared the old one keep theirs.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::{FOLIAGE_WIND_MAX_M_PER_S, FoliageSway, MeshRenderer};
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};
use crate::scene::entity_from;

const HINT: &str = "see book/agents/foliage-wind.md";

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "foliage sway goes on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

fn sway_json(sway: &FoliageSway) -> Value {
    json!({
        "stiffness": sway.stiffness,
        "height_m": sway.height_m,
        "flutter_m": sway.flutter_m,
        "gust_period_s": sway.gust_period_s,
        "max_wind_m_per_s": sway.max_wind_m_per_s,
        "max_offset_m": sway.max_offset(),
        "style": sway.style.name(),
    })
}

/// The sway state of one material: `{ sway, graph, vertex_stage, animated }`.
fn state(material: &regia_render::LabMaterial) -> Value {
    let graph = material.surface_graph.as_deref();
    let sway = FoliageSway::of_material(material);
    json!({
        "sway": sway.as_ref().map(sway_json),
        "graph": graph.is_some(),
        "vertex_stage": graph.is_some_and(|g| g.vertex_wgsl.is_some()),
        "animated": graph.is_some_and(|g| g.vertex_wgsl.is_some() && g.uses_time),
    })
}

/// `material.sway { entity }`: `{ entity, sway: { stiffness, height_m, flutter_m, gust_period_s,
/// max_wind_m_per_s, max_offset_m, style } | null, graph, vertex_stage, animated }`. `animated` is the
/// dynamic-caster class's test (a time-reading vertex stage).
pub fn get(world: &World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(renderer) = world.get::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let mut out = state(&renderer.material);
    if let Value::Object(map) = &mut out {
        map.insert("entity".into(), json!(entity.to_bits()));
    }
    Ok(out)
}

fn field(fields: &Map<String, Value>, name: &str, max: f32, default: f32) -> Result<f32, RpcError> {
    let Some(value) = fields.get(name) else {
        return Ok(default);
    };
    let v = value
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`sway.{name}` must be a number")))?
        as f32;
    if !v.is_finite() || !(0.0..=max).contains(&v) {
        return Err(RpcError::invalid_params(format!(
            "`sway.{name}` must be within 0..{max}"
        )));
    }
    Ok(v)
}

/// The sway a `material.set_sway` call asks for: fields default to the material's current sway,
/// else to [`FoliageSway::default`].
fn sway_of(fields: &Map<String, Value>, base: FoliageSway) -> Result<FoliageSway, RpcError> {
    const KNOWN: [&str; 6] = [
        "style",
        "stiffness",
        "height_m",
        "flutter_m",
        "gust_period_s",
        "max_wind_m_per_s",
    ];
    if let Some(unknown) = fields.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(RpcError::invalid_params(format!(
            "unknown sway field `{unknown}`: {}",
            KNOWN.join(" | ")
        )));
    }
    let sway = FoliageSway {
        stiffness: field(fields, "stiffness", 1.0, base.stiffness)?,
        height_m: field(fields, "height_m", 200.0, base.height_m)?,
        flutter_m: field(fields, "flutter_m", 1.0, base.flutter_m)?,
        gust_period_s: field(fields, "gust_period_s", 3600.0, base.gust_period_s)?,
        max_wind_m_per_s: field(
            fields,
            "max_wind_m_per_s",
            FOLIAGE_WIND_MAX_M_PER_S,
            base.max_wind_m_per_s,
        )?,
        style: match fields.get("style") {
            None => base.style,
            Some(Value::String(name)) => regia_render::WindStyle::from_name(name)
                .ok_or_else(|| RpcError::invalid_params("`sway.style` must be \"classic\" or \"natural\""))?,
            Some(_) => {
                return Err(RpcError::invalid_params(
                    "`sway.style` must be \"classic\" or \"natural\"",
                ));
            }
        },
    };
    if sway.height_m <= 0.0 {
        return Err(RpcError::invalid_params("`sway.height_m` must be above 0"));
    }
    if sway.gust_period_s < 0.1 {
        return Err(RpcError::invalid_params(
            "`sway.gust_period_s` must be at least 0.1",
        ));
    }
    Ok(sway)
}

/// `material.set_sway { entity, sway: { stiffness?, height_m?, flutter_m?, gust_period_s?,
/// max_wind_m_per_s?, style? } | null }`: gives the entity's material the sway (missing fields keep the
/// current sway's, else the default shrub's), or takes it away with `null` (the surface stays).
/// Returns `{ entity, before, after }` (each as in [`get`]). A material graph with its own vertex
/// stage answers `sway_vertex_stage_taken`; one that reads parameter slots 14-15 answers
/// `sway_slots_taken`.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let request = params
        .get("sway")
        .ok_or_else(|| RpcError::invalid_params("pass `sway` (an object, or null to remove it)"))?;
    let Some(mut renderer) = world.get_mut::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let before = state(&renderer.material);
    let current = FoliageSway::of_material(&renderer.material);
    let next =
        match request {
            Value::Null => current.map(|_| FoliageSway::still(&renderer.material)),
            Value::Object(fields) => {
                let sway = sway_of(fields, current.unwrap_or_default())?;
                if current == Some(sway) {
                    None
                } else {
                    Some(sway.adopt(&renderer.material).map_err(|e| {
                        RpcError::new(protocol::INVALID_PARAMS, e.code(), e.to_string(), HINT)
                    })?)
                }
            }
            _ => return Err(RpcError::invalid_params("`sway` must be an object or null")),
        };
    if let Some(material) = next {
        // A new `Arc` is a new material upload; the entities sharing the old one are unchanged.
        renderer.material = Arc::new(material);
    }
    let after = state(&renderer.material);
    Ok(json!({ "entity": entity.to_bits(), "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMaterial, CpuMesh, LabVertex};

    fn world_with_leaf() -> (World, Entity, Arc<CpuMaterial>) {
        let mut world = World::new();
        let shared = Arc::new(CpuMaterial {
            base_color: [0.2, 0.5, 0.1, 1.0],
            ..CpuMaterial::default()
        });
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

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn set_adopts_changes_and_removes_the_sway() {
        let (mut world, e, shared) = world_with_leaf();
        let id = e.to_bits();
        let got = get(&world, &json!({ "entity": id })).expect("get");
        assert_eq!(got["sway"], Value::Null);
        assert_eq!(got["graph"], false);
        assert_eq!(got["animated"], false);

        let out = set(&mut world, &json!({ "entity": id, "sway": { "height_m": 6.0 } })).expect("set");
        assert_eq!(out["before"]["sway"], Value::Null);
        assert_eq!(out["after"]["sway"]["height_m"], 6.0);
        assert_eq!(
            out["after"]["sway"]["stiffness"],
            f64::from(FoliageSway::default().stiffness)
        );
        assert_eq!(out["after"]["animated"], true);
        assert!(
            out["after"]["sway"]["max_offset_m"]
                .as_f64()
                .is_some_and(|m| m > 0.0)
        );
        // The shared material is untouched; the entity holds a copy.
        assert!(shared.surface_graph.is_none());

        // A partial change keeps the other fields; the same values keep the Arc.
        set(
            &mut world,
            &json!({ "entity": id, "sway": { "stiffness": 0.01 } }),
        )
        .expect("change");
        let held = world.get::<MeshRenderer>(e).expect("renderer").material.clone();
        let sway = FoliageSway::of_material(&held).expect("sways");
        assert_eq!((sway.stiffness, sway.height_m), (0.01, 6.0));
        set(
            &mut world,
            &json!({ "entity": id, "sway": { "stiffness": 0.01 } }),
        )
        .expect("same");
        assert!(Arc::ptr_eq(
            &held,
            &world.get::<MeshRenderer>(e).expect("renderer").material
        ));

        let out = set(&mut world, &json!({ "entity": id, "sway": null })).expect("remove");
        assert_eq!(out["after"]["sway"], Value::Null);
        assert_eq!(out["after"]["graph"], true, "the surface stays a graph");
        assert_eq!(out["after"]["animated"], false);
    }

    #[test]
    fn style_is_classic_until_asked_and_survives_other_changes() {
        let (mut world, e, _) = world_with_leaf();
        let id = e.to_bits();
        let out = set(&mut world, &json!({ "entity": id, "sway": {} })).expect("adopt");
        assert_eq!(out["after"]["sway"]["style"], "classic");
        let out = set(
            &mut world,
            &json!({ "entity": id, "sway": { "style": "natural" } }),
        )
        .expect("natural");
        assert_eq!(out["after"]["sway"]["style"], "natural");
        let out = set(&mut world, &json!({ "entity": id, "sway": { "height_m": 4.0 } })).expect("height");
        assert_eq!(
            out["after"]["sway"]["style"], "natural",
            "other fields keep the style"
        );
        for bad in [json!("wild"), json!(2)] {
            let err = set(&mut world, &json!({ "entity": id, "sway": { "style": bad } })).expect_err("bad");
            assert_eq!(reason(&err), "F0002");
        }
    }

    #[test]
    fn bad_params_are_rejected_with_codes() {
        let (mut world, e, _) = world_with_leaf();
        let id = e.to_bits();
        for bad in [
            json!({ "entity": id }),
            json!({ "entity": id, "sway": 3 }),
            json!({ "entity": id, "sway": { "stiffness": -1.0 } }),
            json!({ "entity": id, "sway": { "height_m": 0.0 } }),
            json!({ "entity": id, "sway": { "gust_period_s": 0.01 } }),
            json!({ "entity": id, "sway": { "max_wind_m_per_s": 99.0 } }),
            json!({ "entity": id, "sway": { "bend": 1.0 } }),
        ] {
            let err = set(&mut world, &bad).expect_err("bad sway");
            assert_eq!(reason(&err), "F0002", "{bad}");
        }
        assert_eq!(reason(&get(&world, &json!({})).expect_err("no entity")), "F0002");
        assert_eq!(
            reason(&get(&world, &json!({ "entity": 4_294_967_301u64 })).expect_err("no mesh")),
            "F0003"
        );
    }

    #[test]
    fn a_graph_with_its_own_vertex_stage_is_refused() {
        let (mut world, e, _) = world_with_leaf();
        let code = regia_render::GraphCode {
            surface: "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface { return regia::pbr::surface::default_surface(); }".into(),
            vertex: Some("fn material_vertex_offset(in: regia::pbr::surface::VertexOffsetInput) -> vec3<f32> { return vec3<f32>(0.0); }".into()),
            uv1: false,
        };
        world.get_mut::<MeshRenderer>(e).expect("renderer").material = Arc::new(CpuMaterial {
            surface_graph: Some(Arc::new(regia_render::SurfaceGraph::from_code(&code))),
            ..CpuMaterial::default()
        });
        let err = set(&mut world, &json!({ "entity": e.to_bits(), "sway": {} })).expect_err("taken");
        assert_eq!(reason(&err), "sway_vertex_stage_taken");
    }

    /// Spec 88 § 4: a graph whose surface reads parameter slot 14 or 15 (where the sway keeps
    /// its values) answers `sway_slots_taken`, and the entity keeps its material.
    #[test]
    fn a_graph_reading_the_sway_slots_is_refused() {
        let (mut world, e, _) = world_with_leaf();
        for slot in ["graph[14]", "graph[15]"] {
            let surface = format!(
                "fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {{ \
                 var s = regia::pbr::surface::default_surface(); s.roughness = material.{slot}.x; return s; }}"
            );
            let graph = Arc::new(regia_render::SurfaceGraph::new(surface));
            let material = Arc::new(CpuMaterial {
                surface_graph: Some(graph),
                ..CpuMaterial::default()
            });
            world.get_mut::<MeshRenderer>(e).expect("renderer").material = material.clone();
            let err = set(&mut world, &json!({ "entity": e.to_bits(), "sway": {} })).expect_err("taken");
            assert_eq!(reason(&err), "sway_slots_taken", "{slot}");
            let kept = &world.get::<MeshRenderer>(e).expect("renderer").material;
            assert!(Arc::ptr_eq(kept, &material), "{slot}: the material is unchanged");
        }
    }
}
