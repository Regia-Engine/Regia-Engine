// SPDX-License-Identifier: MPL-2.0
//! Agent methods for screen-space subsurface scattering (`book/agents/subsurface.md`, ADR-2049):
//! `render.subsurface` reads what the last frame did, `render.set_subsurface` switches the profile's
//! knob, `material.subsurface` reads a mesh entity's factors and `material.set_subsurface` edits them.
//!
//! The factors belong to the entity's material (`LabMaterial::subsurface`). Editing them gives the
//! entity its own copy of the material, so entities that shared the old one keep theirs.
//!
//! ADR-3170: `render.set_subsurface { kernel }` picks where the blur lays its kernel (`screen`, the
//! default, or `tangent`), and `render.subsurface` counts the scene's scattering materials by why
//! this path cannot scatter them (`materials.unsupported_by`, the codes of
//! `SubsurfaceUnsupported`).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::shading_model::{MaterialShading, ShadingModel};
use regia_render::{
    CpuMaterial, MeshRenderer, RenderSettings, RuntimeRenderer, SubsurfaceFactors, SubsurfaceKernel,
    SubsurfaceProfile, SubsurfaceStats, SubsurfaceUnsupported,
};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "subsurface scattering goes on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

/// Why this path cannot scatter this material (`null` when it can). What depends on the draw (a
/// masked instance, a lightmap receiver, a render override) or the frame (a lite tier) is counted in
/// the renderer's `unsupported` and `reason` instead. The codes are `SubsurfaceUnsupported`'s.
fn unsupported(material: &CpuMaterial, shading: Option<&MaterialShading>) -> Value {
    let eye = shading.is_some_and(|shading| matches!(shading.model, ShadingModel::Eye(_)));
    SubsurfaceUnsupported::of_material(material, eye).map_or(Value::Null, |reason| json!(reason.code()))
}

/// The scene's mesh entities whose material asks to scatter: how many, how many this path can
/// scatter, and the rest counted by why not (`{ code: count }`, only the codes that occur).
fn materials_json(world: &mut World) -> Value {
    let mut query = world.query::<(&MeshRenderer, Option<&MaterialShading>)>();
    let (mut asking, mut scatterable) = (0u64, 0u64);
    let mut by = [0u64; SubsurfaceUnsupported::COUNT];
    for (renderer, shading) in query.iter(world) {
        if !renderer.material.subsurface.is_on() {
            continue;
        }
        asking += 1;
        let eye = shading.is_some_and(|shading| matches!(shading.model, ShadingModel::Eye(_)));
        match SubsurfaceUnsupported::of_material(&renderer.material, eye) {
            None => scatterable += 1,
            Some(reason) => by[reason.index()] += 1,
        }
    }
    let unsupported_by: Map<String, Value> = SubsurfaceUnsupported::ALL
        .iter()
        .filter(|reason| by[reason.index()] > 0)
        .map(|reason| (reason.code().to_owned(), json!(by[reason.index()])))
        .collect();
    json!({ "asking": asking, "scatterable": scatterable, "unsupported_by": unsupported_by })
}

fn stats_json(stats: SubsurfaceStats) -> Value {
    json!({
        "requested": stats.requested,
        "drawn": stats.drawn,
        "fallback": stats.fallback,
        "unsupported": stats.unsupported,
        "unsupported_by": stats
            .unsupported_reasons()
            .map(|(reason, count)| (reason.code().to_owned(), json!(count)))
            .collect::<Map<String, Value>>(),
        "passes": stats.passes,
        "covered_permille": stats.covered_permille,
        "reason": stats.reason,
    })
}

/// One material's factors: its values, whether this path scatters it, and why not.
fn factors_json(material: &CpuMaterial, shading: Option<&MaterialShading>) -> Value {
    let factors = material.subsurface;
    let reason = unsupported(material, shading);
    json!({
        "enabled": factors.is_on(),
        "active": factors.is_on() && reason.is_null(),
        "unsupported": reason,
        "profile": factors.profile.name(),
        "radius_mm": factors.radius_mm,
        "reach_mm": factors.reach_mm(),
        "tint": factors.tint,
    })
}

/// `material.subsurface { entity }`: the entity's factors, plus the last frame's `renderer` counts
/// (`null` without a renderer).
pub(crate) fn get_material(world: &World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(renderer) = world.get::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let mut out = factors_json(&renderer.material, world.get::<MaterialShading>(entity));
    if let Value::Object(map) = &mut out {
        map.insert("entity".into(), json!(entity.to_bits()));
        let stats = world
            .get_resource::<RuntimeRenderer>()
            .map_or(Value::Null, |r| stats_json(r.0.subsurface_stats()));
        map.insert("renderer".into(), stats);
    }
    Ok(out)
}

fn tint(params: &Map<String, Value>, current: [f32; 3]) -> Result<[f32; 3], RpcError> {
    let Some(value) = params.get("tint") else {
        return Ok(current);
    };
    let bad = || RpcError::invalid_params("`tint` must be three numbers in 0..=1, linear RGB");
    let list = value.as_array().filter(|list| list.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0; 3];
    for (slot, item) in out.iter_mut().zip(list) {
        let channel = item
            .as_f64()
            .filter(|v| (0.0..=1.0).contains(v))
            .ok_or_else(bad)?;
        *slot = channel as f32;
    }
    Ok(out)
}

/// The factors `params` asks for on top of `current`.
fn edited(params: &Value, current: SubsurfaceFactors) -> Result<SubsurfaceFactors, RpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    let profile = match object.get("profile") {
        None => current.profile,
        Some(value) => value
            .as_str()
            .and_then(SubsurfaceProfile::from_name)
            .ok_or_else(|| RpcError::invalid_params("`profile` must be one of off, skin, foliage"))?,
    };
    let radius_mm = match object.get("radius_mm") {
        None => current.radius_mm,
        Some(value) => value
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or_else(|| RpcError::invalid_params("`radius_mm` must be a number, at least 0"))?
            as f32,
    };
    Ok(SubsurfaceFactors {
        profile,
        radius_mm,
        tint: tint(object, current.tint)?,
    })
}

/// `material.set_subsurface { entity, profile?, radius_mm?, tint? }`: edits the factors on the
/// entity's material and returns `{ entity, before, after }`. An omitted field keeps its value.
pub(crate) fn set_material(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let Some(mut renderer) = world.get_mut::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("mesh renderer vanished", "retry"));
    };
    let factors = edited(params, renderer.material.subsurface)?;
    let before_material = renderer.material.clone();
    if renderer.material.subsurface != factors {
        // A new `Arc` is a new material upload; the entities sharing the old one are unchanged.
        let mut material = (*renderer.material).clone();
        material.subsurface = factors;
        renderer.material = Arc::new(material);
    }
    let after_material = renderer.material.clone();
    let shading = world.get::<MaterialShading>(entity);
    let before = factors_json(&before_material, shading);
    let after = factors_json(&after_material, shading);
    Ok(json!({ "entity": entity.to_bits(), "before": before, "after": after }))
}

/// `render.subsurface`: the active profile's knob and kernel, the scene's scattering materials by
/// why they cannot scatter, and what the last frame did with them.
pub(crate) fn report(world: &mut World) -> Value {
    let (requested, kernel) =
        world
            .get_resource::<RenderSettings>()
            .map_or((false, SubsurfaceKernel::Screen), |settings| {
                (
                    settings.profile.subsurface_scattering,
                    settings.profile.subsurface_kernel,
                )
            });
    let materials = materials_json(world);
    json!({
        "enabled": requested,
        "kernel": kernel.name(),
        "materials": materials,
        "frame": world
            .get_resource::<RuntimeRenderer>()
            .map_or(Value::Null, |r| stats_json(r.0.subsurface_stats())),
    })
}

/// `render.set_subsurface { enabled?, kernel? }`: switches the active profile's knob and picks the
/// blur's kernel (`screen` or `tangent`). At least one is needed. Returns the knob before and after
/// (`before`, `after`) and the kernel's (`kernel.before`, `kernel.after`).
pub(crate) fn set_enabled(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = crate::params::opt_bool(params, "enabled")?;
    let kernel = match crate::params::opt_str(params, "kernel")? {
        None => None,
        Some(name) => Some(
            SubsurfaceKernel::from_name(name)
                .ok_or_else(|| RpcError::invalid_params("`kernel` must be screen or tangent"))?,
        ),
    };
    if enabled.is_none() && kernel.is_none() {
        return Err(RpcError::invalid_params(
            "give `enabled` (true or false), `kernel` (screen or tangent) or both",
        ));
    }
    let Some(mut settings) = world.get_resource_mut::<RenderSettings>() else {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself sets \
             FrameQuality::subsurface_scattering and FrameQuality::subsurface_kernel)",
        ));
    };
    let before = settings.profile.subsurface_scattering;
    let kernel_before = settings.profile.subsurface_kernel;
    let after = enabled.unwrap_or(before);
    let kernel_after = kernel.unwrap_or(kernel_before);
    if before != after {
        settings.profile.subsurface_scattering = after;
    }
    if kernel_before != kernel_after {
        settings.profile.subsurface_kernel = kernel_after;
    }
    Ok(json!({
        "before": before,
        "after": after,
        "kernel": { "before": kernel_before.name(), "after": kernel_after.name() },
        "note": "applies from the next frame; read render.subsurface. Only materials with the factors \
                 scatter (material.set_subsurface); terrain grass takes the foliage transmission with \
                 the switch. A preset change (render.set_profile) replaces the profile and turns it off",
    }))
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

    #[test]
    fn set_edits_a_copy_keeps_omitted_fields_and_reads_back() {
        let (mut world, e, shared) = world_with_mesh(CpuMaterial::default());
        let id = e.to_bits();
        let got = get_material(&world, &json!({ "entity": id })).expect("get");
        assert_eq!(got["enabled"], false);
        assert_eq!(got["active"], false);
        assert_eq!(got["profile"], "off");
        assert_eq!(got["renderer"], Value::Null);
        let out = set_material(
            &mut world,
            &json!({ "entity": id, "profile": "skin", "tint": [1.0, 0.5, 0.25] }),
        )
        .expect("set");
        assert_eq!(out["before"]["enabled"], false);
        assert_eq!(out["after"]["enabled"], true);
        assert_eq!(out["after"]["active"], true);
        assert_eq!(out["after"]["profile"], "skin");
        assert_eq!(out["after"]["tint"], json!([1.0, 0.5, 0.25]));
        assert_eq!(out["after"]["radius_mm"], 0.0, "omitted fields keep their value");
        // The shared material is untouched; the entity holds a copy.
        assert_eq!(shared.subsurface, SubsurfaceFactors::OFF);
        let held = world.get::<MeshRenderer>(e).expect("renderer").material.clone();
        assert_eq!(held.subsurface.profile, SubsurfaceProfile::Skin);
        // Setting the same values again keeps the `Arc`.
        set_material(&mut world, &json!({ "entity": id, "profile": "skin" })).expect("again");
        assert!(Arc::ptr_eq(
            &held,
            &world.get::<MeshRenderer>(e).expect("renderer").material
        ));
    }

    #[test]
    fn a_material_this_path_cannot_scatter_says_why() {
        let (world, e, _) = world_with_mesh(CpuMaterial {
            transmission: 0.5,
            subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
            ..CpuMaterial::default()
        });
        let got = get_material(&world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["enabled"], true);
        assert_eq!(got["active"], false);
        assert_eq!(got["unsupported"], "transmissive");
    }

    #[test]
    fn an_eye_does_not_scatter_and_says_so() {
        let (mut world, e, _) = world_with_mesh(CpuMaterial {
            subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
            ..CpuMaterial::default()
        });
        world.entity_mut(e).insert(MaterialShading::eye(
            regia_render::shading_model::EyeShading::default(),
        ));
        let got = get_material(&world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["active"], false);
        assert_eq!(got["unsupported"], "eye");
    }

    #[test]
    fn bad_values_are_refused_and_change_nothing() {
        let (mut world, e, _) = world_with_mesh(CpuMaterial::default());
        let id = e.to_bits();
        for bad in [
            json!({ "entity": id, "profile": "wax" }),
            json!({ "entity": id, "profile": 3 }),
            json!({ "entity": id, "radius_mm": -1.0 }),
            json!({ "entity": id, "radius_mm": "far" }),
            json!({ "entity": id, "tint": [1.0, 1.0] }),
            json!({ "entity": id, "tint": [1.0, 1.0, 2.0] }),
        ] {
            assert!(set_material(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world
                .get::<MeshRenderer>(e)
                .expect("renderer")
                .material
                .subsurface,
            SubsurfaceFactors::OFF
        );
        assert!(set_material(&mut world, &json!({ "entity": 12345 })).is_err());
    }

    #[test]
    fn the_knob_needs_settings_and_reads_back() {
        let mut world = World::new();
        assert!(set_enabled(&mut world, &json!({ "enabled": true })).is_err());
        world.insert_resource(RenderSettings::default());
        assert!(set_enabled(&mut world, &json!({ "enabled": "yes" })).is_err());
        assert_eq!(report(&mut world)["enabled"], false);
        let out = set_enabled(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(out["before"], false);
        assert_eq!(out["after"], true);
        assert_eq!(report(&mut world)["enabled"], true);
        assert_eq!(report(&mut world)["frame"], Value::Null);
    }

    /// ADR-3170: the kernel switch, alone or with the knob; bad values change nothing.
    #[test]
    fn the_kernel_switches_and_reads_back() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        assert_eq!(report(&mut world)["kernel"], "screen");
        for bad in [
            json!({}),
            json!({ "kernel": "world" }),
            json!({ "kernel": 1 }),
            json!({ "enabled": true, "kernel": "wide" }),
        ] {
            assert!(set_enabled(&mut world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            report(&mut world)["enabled"],
            false,
            "a refused call changes nothing"
        );
        let out = set_enabled(&mut world, &json!({ "kernel": "tangent" })).expect("kernel");
        assert_eq!(out["kernel"], json!({ "before": "screen", "after": "tangent" }));
        assert_eq!(
            (out["before"].clone(), out["after"].clone()),
            (json!(false), json!(false))
        );
        let settings = world.resource::<RenderSettings>();
        assert_eq!(settings.profile.subsurface_kernel, SubsurfaceKernel::Tangent);
        assert!(settings.preset().is_some(), "the kernel keeps the preset");
        let both = set_enabled(&mut world, &json!({ "enabled": true, "kernel": "screen" })).expect("both");
        assert_eq!(both["after"], true);
        assert_eq!(both["kernel"]["after"], "screen");
        assert_eq!(report(&mut world)["kernel"], "screen");
    }

    /// ADR-3170: `render.subsurface` counts the scene's scattering materials by why they cannot.
    #[test]
    fn the_report_counts_materials_by_reason() {
        let (mut world, _, _) = world_with_mesh(CpuMaterial {
            subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
            ..CpuMaterial::default()
        });
        let vertices = vec![LabVertex::from_pos_normal_uv([0.0; 3], [0.0, 1.0, 0.0], [0.0; 2]); 3];
        let mesh = Arc::new(CpuMesh {
            vertices,
            indices: vec![0, 1, 2],
        });
        for material in [
            CpuMaterial {
                transmission: 0.5,
                subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Foliage),
                ..CpuMaterial::default()
            },
            CpuMaterial {
                transmission: 0.2,
                subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
                ..CpuMaterial::default()
            },
            CpuMaterial::default(),
        ] {
            world.spawn(MeshRenderer::new(mesh.clone(), Arc::new(material)));
        }
        let eye = world
            .spawn(MeshRenderer::new(
                mesh,
                Arc::new(CpuMaterial {
                    subsurface: SubsurfaceFactors::new(SubsurfaceProfile::Skin),
                    ..CpuMaterial::default()
                }),
            ))
            .id();
        world.entity_mut(eye).insert(MaterialShading::eye(
            regia_render::shading_model::EyeShading::default(),
        ));
        let got = report(&mut world);
        assert_eq!(
            got["materials"],
            json!({ "asking": 4, "scatterable": 1, "unsupported_by": { "eye": 1, "transmissive": 2 } })
        );
    }

    #[test]
    fn the_frame_counts_reach_the_reply_by_code() {
        let mut unsupported_by = [0; SubsurfaceUnsupported::COUNT];
        unsupported_by[SubsurfaceUnsupported::Transmissive.index()] = 2;
        unsupported_by[SubsurfaceUnsupported::Unlit.index()] = 1;
        let stats = SubsurfaceStats {
            unsupported: 3,
            unsupported_by,
            ..SubsurfaceStats::default()
        };
        assert_eq!(
            stats_json(stats)["unsupported_by"],
            json!({ "transmissive": 2, "unlit": 1 })
        );
        assert_eq!(
            stats_json(SubsurfaceStats::default())["unsupported_by"],
            json!({}),
            "a frame with nothing refused reports an empty map"
        );
    }
}
