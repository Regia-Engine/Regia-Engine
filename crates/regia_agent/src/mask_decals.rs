// SPDX-License-Identifier: MPL-2.0
//! Agent methods for material mask recolor and decal slots (`book/agents/mask-decals.md`):
//! `render.mask_decals` reads an instance (or lists every masked instance with the renderer's
//! counters) and `render.set_mask_decals` edits its palette and decal slots.
//!
//! Colors are linear RGB `[r, g, b]` (decal colors `[r, g, b, a]`), or `"#RRGGBB"` strings, which
//! are sRGB and decoded. Textures come from the host: an instance the agent creates has no mask
//! or atlas, so its decals draw as solid rectangles until the game supplies them.

use regia_app::prelude::*;
use regia_render::{DecalBlend, DecalSlot, MAX_DECAL_SLOTS, MaskDecals, MaskPalette, MeshRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

fn mesh_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "masks and decals go on a mesh entity: pick one from scene.query_entities",
        ));
    }
    Ok(entity)
}

fn image_json(image: Option<&regia_render::SharedImage>) -> Value {
    image.map_or(Value::Null, |image| {
        let (w, h) = image.size();
        json!([w, h])
    })
}

fn palette_json(p: &MaskPalette) -> Value {
    json!({
        "primary": p.primary,
        "secondary": p.secondary,
        "trim": p.trim,
        "emissive": p.emissive,
        "emissive_intensity": p.emissive_intensity,
    })
}

fn slot_json(slot: &DecalSlot) -> Value {
    json!({
        "cell": slot.atlas_cell,
        "uv_rect": slot.uv_rect,
        "rotation": slot.rotation,
        "color": slot.color,
        "blend": slot.blend.name(),
        "emissive_intensity": slot.emissive_intensity,
    })
}

fn instance_json(entity: Entity, decals: &MaskDecals) -> Value {
    json!({
        "entity": entity.to_bits(),
        "palette": palette_json(&decals.palette),
        "decals": decals.decals.iter().map(slot_json).collect::<Vec<_>>(),
        "drawn_slots": decals.decals.len().min(MAX_DECAL_SLOTS),
        "mask": image_json(decals.textures.mask.as_ref()),
        "atlas": image_json(decals.textures.atlas.as_ref()),
        "atlas_grid": decals.textures.atlas_grid,
    })
}

fn stats_json(world: &World) -> Value {
    world
        .get_resource::<regia_render::RuntimeRenderer>()
        .map_or(Value::Null, |renderer| {
            let s = renderer.0.mask_decal_stats();
            json!({
                "instances": s.instances,
                "masked_draws": s.masked_draws,
                "fallback_draws": s.fallback_draws,
                "unsupported_draws": s.unsupported_draws,
                "gpu_entries": s.gpu_entries,
                "blocks_created": s.blocks_created,
                "block_writes": s.block_writes,
                "block_bytes": regia_render::mask_decals::BLOCK_BYTES,
            })
        })
}

/// `render.mask_decals { entity? }`: one instance, or every masked instance and the renderer's
/// counters for its last frame.
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if params.get("entity").is_some() {
        let entity = mesh_entity(world, params)?;
        return Ok(match world.get::<MaskDecals>(entity) {
            Some(decals) => instance_json(entity, decals),
            None => json!({ "entity": entity.to_bits(), "palette": Value::Null, "decals": [] }),
        });
    }
    let mut query = world.query::<(Entity, &MaskDecals)>();
    let instances: Vec<Value> = query
        .iter(world)
        .map(|(entity, decals)| instance_json(entity, decals))
        .collect();
    Ok(json!({ "instances": instances, "renderer": stats_json(world) }))
}

fn color(value: &Value, field: &str, alpha: bool) -> Result<Vec<f32>, RpcError> {
    if let Some(hex) = value.as_str() {
        let c = regia_core::math::Color::from_hex(hex)
            .map_err(|e| RpcError::invalid_params(format!("`{field}`: {e}")))?;
        return Ok(if alpha {
            vec![c.r, c.g, c.b, c.a]
        } else {
            vec![c.r, c.g, c.b]
        });
    }
    let want = if alpha { 4 } else { 3 };
    let shape = if alpha { "[r, g, b, a]" } else { "[r, g, b]" };
    let values: Vec<f32> = value
        .as_array()
        .filter(|a| a.len() == want)
        .ok_or_else(|| {
            RpcError::invalid_params(format!("`{field}` must be {shape} (linear) or \"#RRGGBB\""))
        })?
        .iter()
        .map(|v| {
            v.as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| v as f32)
        })
        .collect::<Option<_>>()
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must hold numbers >= 0")))?;
    Ok(values)
}

fn rgb(value: &Value, field: &str) -> Result<[f32; 3], RpcError> {
    let c = color(value, field, false)?;
    Ok([c[0], c[1], c[2]])
}

fn number(value: Option<&Value>, field: &str, default: f32) -> Result<f32, RpcError> {
    match value {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number"))),
    }
}

fn apply_palette(palette: &mut MaskPalette, value: &Value) -> Result<(), RpcError> {
    if !value.is_object() {
        return Err(RpcError::invalid_params("`palette` must be an object"));
    }
    for (field, slot) in [
        ("primary", &mut palette.primary),
        ("secondary", &mut palette.secondary),
        ("trim", &mut palette.trim),
        ("emissive", &mut palette.emissive),
    ] {
        if let Some(v) = value.get(field) {
            *slot = rgb(v, field)?;
        }
    }
    if let Some(v) = value.get("emissive_intensity") {
        palette.emissive_intensity = number(Some(v), "emissive_intensity", 0.0)?.max(0.0);
    }
    Ok(())
}

fn parse_slot(value: &Value) -> Result<DecalSlot, RpcError> {
    let uv_rect: Vec<f32> = crate::params::opt_array(value, "uv_rect")?
        .filter(|a| a.len() == 4)
        .and_then(|a| a.iter().map(|v| v.as_f64().map(|v| v as f32)).collect())
        .ok_or_else(|| {
            RpcError::invalid_params("each decal needs `uv_rect`: [min_u, min_v, max_u, max_v]")
        })?;
    let atlas_cell = match value.get("cell") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|c| u32::try_from(c).ok())
                .ok_or_else(|| RpcError::invalid_params("`cell` must be an atlas cell index >= 0"))?,
        ),
    };
    let blend = match value.get("blend") {
        None => DecalBlend::Alpha,
        Some(v) => v.as_str().and_then(DecalBlend::from_name).ok_or_else(|| {
            RpcError::invalid_params("`blend` must be \"alpha\", \"multiply\" or \"emissive\"")
        })?,
    };
    let color = match value.get("color") {
        None => [1.0; 4],
        Some(v) => {
            let c = color(v, "color", true)?;
            [c[0], c[1], c[2], c[3].min(1.0)]
        }
    };
    Ok(DecalSlot {
        atlas_cell,
        uv_rect: [uv_rect[0], uv_rect[1], uv_rect[2], uv_rect[3]],
        rotation: number(value.get("rotation"), "rotation", 0.0)?,
        color,
        blend,
        emissive_intensity: number(value.get("emissive_intensity"), "emissive_intensity", 0.0)?.max(0.0),
    })
}

/// `render.set_mask_decals { entity, palette?, decals?, remove? }`: edits (or creates) the
/// instance's palette and decal slots; `remove: true` drops the component.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = mesh_entity(world, params)?;
    let before = world.get::<MaskDecals>(entity).cloned();
    let before_json = before
        .as_ref()
        .map_or(Value::Null, |decals| instance_json(entity, decals));
    if crate::params::opt_bool(params, "remove")? == Some(true) {
        world.entity_mut(entity).remove::<MaskDecals>();
        return Ok(json!({ "before": before_json, "after": Value::Null }));
    }
    let mut after = before.unwrap_or_default();
    if let Some(palette) = params.get("palette") {
        apply_palette(&mut after.palette, palette)?;
    }
    if let Some(decals) = params.get("decals") {
        let list = decals
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`decals` must be an array of slots"))?;
        if list.len() > MAX_DECAL_SLOTS {
            return Err(RpcError::invalid_params(format!(
                "at most {MAX_DECAL_SLOTS} decal slots draw; got {}",
                list.len()
            )));
        }
        after.decals = list.iter().map(parse_slot).collect::<Result<_, _>>()?;
    }
    let after_json = instance_json(entity, &after);
    world.entity_mut(entity).insert(after);
    Ok(json!({ "before": before_json, "after": after_json }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{CpuMaterial, CpuMesh, MaskDecalTextures};
    use std::sync::Arc;

    fn world_with_mesh() -> (World, Entity) {
        let mut world = World::new();
        let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
        let entity = world
            .spawn(MeshRenderer::new(
                Arc::new(CpuMesh { vertices, indices }),
                Arc::new(CpuMaterial::default()),
            ))
            .id();
        (world, entity)
    }

    #[test]
    fn set_creates_edits_and_removes_an_instance() {
        let (mut world, e) = world_with_mesh();
        let got = get(&mut world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["palette"], Value::Null);
        let set_reply = set(
            &mut world,
            &json!({
                "entity": e.to_bits(),
                "palette": { "primary": "#FF0000", "trim": [0.1, 0.2, 0.3], "emissive_intensity": 2 },
                "decals": [
                    { "cell": 7, "uv_rect": [0.4, 0.3, 0.6, 0.5], "rotation": 0.5 },
                    { "uv_rect": [0, 0, 1, 1], "color": [0.2, 0.3, 0.4, 0.5], "blend": "multiply" }
                ]
            }),
        )
        .expect("set");
        assert_eq!(set_reply["before"], Value::Null);
        let decals = world.get::<MaskDecals>(e).expect("component inserted");
        assert_eq!(decals.palette.primary, [1.0, 0.0, 0.0]);
        assert_eq!(decals.palette.trim, [0.1, 0.2, 0.3]);
        assert_eq!(
            decals.palette.secondary, [1.0; 3],
            "untouched entries stay neutral"
        );
        assert_eq!(decals.decals.len(), 2);
        assert_eq!(decals.decals[0].atlas_cell, Some(7));
        assert_eq!(decals.decals[1].blend, DecalBlend::Multiply);
        assert_eq!(decals.decals[1].atlas_cell, None);
        // A palette-only edit keeps the slots.
        set(
            &mut world,
            &json!({ "entity": e.to_bits(), "palette": { "secondary": [0.0, 1.0, 0.0] } }),
        )
        .expect("palette edit");
        let decals = world.get::<MaskDecals>(e).expect("component");
        assert_eq!(decals.decals.len(), 2);
        assert_eq!(decals.palette.secondary, [0.0, 1.0, 0.0]);
        let listed = get(&mut world, &json!({})).expect("list");
        assert_eq!(listed["instances"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["renderer"], Value::Null, "no renderer in this world");
        let removed = set(&mut world, &json!({ "entity": e.to_bits(), "remove": true })).expect("remove");
        assert_eq!(removed["after"], Value::Null);
        assert!(world.get::<MaskDecals>(e).is_none());
    }

    #[test]
    fn host_textures_survive_agent_edits() {
        let (mut world, e) = world_with_mesh();
        let mask = regia_render::SharedImage::rgba(2, 2, vec![255; 16], false).expect("mask");
        let textures = Arc::new(MaskDecalTextures {
            mask: Some(mask),
            atlas: None,
            atlas_grid: [4, 4],
        });
        world
            .entity_mut(e)
            .insert(MaskDecals::new(textures.clone(), MaskPalette::NEUTRAL));
        set(
            &mut world,
            &json!({ "entity": e.to_bits(), "palette": { "primary": [0.5, 0.5, 0.5] } }),
        )
        .expect("set");
        let decals = world.get::<MaskDecals>(e).expect("component");
        assert!(Arc::ptr_eq(&decals.textures, &textures));
        let got = get(&mut world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["mask"], json!([2, 2]));
        assert_eq!(got["atlas_grid"], json!([4, 4]));
    }

    #[test]
    fn bad_params_are_rejected() {
        let (mut world, e) = world_with_mesh();
        let bad = [
            json!({ "entity": e.to_bits(), "palette": { "primary": [1.0, 0.0] } }),
            json!({ "entity": e.to_bits(), "palette": { "primary": "#GG0000" } }),
            json!({ "entity": e.to_bits(), "decals": [{ "cell": 1 }] }),
            json!({ "entity": e.to_bits(), "decals": [{ "uv_rect": [0, 0, 1, 1], "blend": "screen" }] }),
            json!({ "entity": e.to_bits(), "decals": vec![json!({ "uv_rect": [0, 0, 1, 1] }); 9] }),
        ];
        for params in bad {
            assert!(set(&mut world, &params).is_err(), "{params}");
        }
        let other = world.spawn_empty().id();
        assert!(get(&mut world, &json!({ "entity": other.to_bits() })).is_err());
    }
}
