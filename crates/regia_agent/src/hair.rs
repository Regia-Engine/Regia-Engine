// SPDX-License-Identifier: MPL-2.0
//! Agent methods for hair card shading (`book/agents/hair-cards.md`): `render.hair_get` reads an
//! entity's hair factors and `render.hair_set` patches them.
//!
//! The factors live on the entity's `CpuMaterial` (an `Arc` shared by every instance of it).
//! `render.hair_set` gives the entity its own copy of the material, patched, so the other
//! instances of the original keep their look; a second call on the same entity patches the copy
//! in place. The alpha mode follows the strategy (`HairFactors::apply_alpha_mode`); the mode
//! and cutoff the material had before `render.hair_set` turned hair on are kept on the entity
//! ([`HairPriorAlpha`]) and restored when it turns hair off.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_render::{HairAlpha, HairFactors, MeshRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

/// The alpha mode and cutoff an entity's material had before `render.hair_set` turned hair on,
/// restored (and removed) when a later call turns hair off.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub(crate) struct HairPriorAlpha {
    alpha_mode: u32,
    alpha_cutoff: f32,
}

fn hair_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_from(params)?;
    if world.get::<MeshRenderer>(entity).is_none() {
        return Err(RpcError::not_found(
            format!("entity {} has no MeshRenderer", entity.to_bits()),
            "pick a hair card mesh entity from scene.query_entities",
        ));
    }
    Ok(entity)
}

fn hair_json(entity: Entity, h: &HairFactors) -> Value {
    json!({
        "entity": entity.to_bits(),
        "enabled": h.enabled,
        "alpha": h.alpha.name(),
        "primary_shift": h.primary_shift,
        "secondary_shift": h.secondary_shift,
        "primary_exponent": h.primary_exponent,
        "secondary_exponent": h.secondary_exponent,
        "primary_strength": h.primary_strength,
        "secondary_strength": h.secondary_strength,
        "transmission": h.transmission,
        "shift_noise": h.shift_noise,
        "streak_density": h.streak_density,
        "root_color": h.root_color,
        "tip_color": h.tip_color,
        "root_alpha": h.root_alpha,
        "tip_alpha": h.tip_alpha,
        "ramp": [h.ramp_start, h.ramp_end],
    })
}

/// `render.hair_get { entity }`: the hair factors of the entity's material (hair off when it has
/// none), plus the alpha mode the renderer draws the material with.
pub fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = hair_entity(world, params)?;
    let Some(renderer) = world.get::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("entity lost its MeshRenderer", "retry"));
    };
    let material = &renderer.material;
    let mut out = hair_json(entity, &material.hair);
    out["alpha_mode"] = json!(regia_render::hair::effective_alpha_mode(material));
    Ok(out)
}

fn number(params: &Value, field: &str, lo: f64, hi: f64) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite() && (lo..=hi).contains(v))
            .map(|v| Some(v as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number in {lo}..={hi}"))),
    }
}

fn colour(params: &Value, field: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(v) = params.get(field) else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params(format!("`{field}` must be [r, g, b], linear, each 0..=4"));
    let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x
            .as_f64()
            .filter(|x| x.is_finite() && (0.0..=4.0).contains(x))
            .ok_or_else(bad)? as f32;
    }
    Ok(Some(out))
}

/// `render.hair_set { entity, enabled?, alpha?, ... }`: patch the hair factors; omitted fields
/// keep their value. Any field other than `enabled: false` turns hair on.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = hair_entity(world, params)?;
    let before = world
        .get::<MeshRenderer>(entity)
        .map(|r| r.material.hair)
        .unwrap_or_default();
    let mut h = before;
    let mut touched = false;
    if let Some(v) = params.get("alpha") {
        h.alpha = v.as_str().and_then(HairAlpha::from_name).ok_or_else(|| {
            RpcError::invalid_params("`alpha` must be \"hashed\", \"sorted\" or \"coverage\"")
        })?;
        touched = true;
    }
    let numbers: [(&str, f64, f64, &mut f32); 11] = [
        ("primary_shift", -1.0, 1.0, &mut h.primary_shift),
        ("secondary_shift", -1.0, 1.0, &mut h.secondary_shift),
        ("primary_exponent", 1.0, 2048.0, &mut h.primary_exponent),
        ("secondary_exponent", 1.0, 2048.0, &mut h.secondary_exponent),
        ("primary_strength", 0.0, 8.0, &mut h.primary_strength),
        ("secondary_strength", 0.0, 8.0, &mut h.secondary_strength),
        ("transmission", 0.0, 1.0, &mut h.transmission),
        ("shift_noise", 0.0, 1.0, &mut h.shift_noise),
        ("streak_density", 1.0, 1024.0, &mut h.streak_density),
        ("root_alpha", 0.0, 1.0, &mut h.root_alpha),
        ("tip_alpha", 0.0, 1.0, &mut h.tip_alpha),
    ];
    for (field, lo, hi, slot) in numbers {
        if let Some(v) = number(params, field, lo, hi)? {
            *slot = v;
            touched = true;
        }
    }
    if let Some(c) = colour(params, "root_color")? {
        h.root_color = c;
        touched = true;
    }
    if let Some(c) = colour(params, "tip_color")? {
        h.tip_color = c;
        touched = true;
    }
    if let Some(v) = params.get("ramp") {
        let bad = || RpcError::invalid_params("`ramp` must be [start_v, end_v], each -8..=8");
        let a = v.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
        let get = |i: usize| {
            a[i].as_f64()
                .filter(|x| x.is_finite() && x.abs() <= 8.0)
                .ok_or_else(bad)
        };
        h.ramp_start = get(0)? as f32;
        h.ramp_end = get(1)? as f32;
        touched = true;
    }
    match params.get("enabled") {
        None => h.enabled |= touched,
        Some(v) => {
            h.enabled = v
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?
        }
    }
    let prior = world.get::<HairPriorAlpha>(entity).copied();
    let Some(mut renderer) = world.get_mut::<MeshRenderer>(entity) else {
        return Err(RpcError::not_found("entity lost its MeshRenderer", "retry"));
    };
    let mut material = (*renderer.material).clone();
    // Turning hair on remembers the material's own alpha mode; turning it off puts it back (hair
    // the host turned on has no record, so its mode stays as the host wrote it).
    let remember = h.enabled && !before.enabled && prior.is_none();
    let saved = HairPriorAlpha {
        alpha_mode: material.alpha_mode,
        alpha_cutoff: material.alpha_cutoff,
    };
    let restore = !h.enabled && before.enabled;
    if restore && let Some(p) = prior {
        material.alpha_mode = p.alpha_mode;
        material.alpha_cutoff = p.alpha_cutoff;
    }
    material.hair = h;
    h.apply_alpha_mode(&mut material.alpha_mode, &mut material.alpha_cutoff);
    renderer.material = Arc::new(material);
    if remember {
        world.entity_mut(entity).insert(saved);
    } else if restore && prior.is_some() {
        world.entity_mut(entity).remove::<HairPriorAlpha>();
    }
    Ok(json!({ "before": hair_json(entity, &before), "after": hair_json(entity, &h) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::{ALPHA_BLEND, ALPHA_MASK, CpuMaterial, CpuMesh};

    fn world_with_card() -> (World, Entity, Arc<CpuMaterial>) {
        let mut world = World::new();
        let shared = Arc::new(CpuMaterial::default());
        let entity = world
            .spawn(MeshRenderer::new(
                Arc::new(CpuMesh {
                    vertices: vec![],
                    indices: vec![],
                }),
                shared.clone(),
            ))
            .id();
        (world, entity, shared)
    }

    #[test]
    fn get_set_round_trip_and_alpha_mode() {
        let (mut world, e, shared) = world_with_card();
        let got = get(&mut world, &json!({ "entity": e.to_bits() })).expect("get");
        assert_eq!(got["enabled"], false);
        let set = set(
            &mut world,
            &json!({
                "entity": e.to_bits(),
                "tip_color": [0.3, 0.2, 0.1],
                "tip_alpha": 0.25,
                "ramp": [0.2, 0.9],
                "primary_exponent": 200.0,
            }),
        )
        .expect("set");
        assert_eq!(set["before"]["enabled"], false);
        assert_eq!(set["after"]["enabled"], true, "a patch turns hair on");
        assert_eq!(set["after"]["alpha"], "hashed");
        let material = world.get::<MeshRenderer>(e).expect("renderer").material.clone();
        assert_eq!(material.hair.tip_alpha, 0.25);
        assert_eq!(material.hair.ramp_start, 0.2);
        assert_eq!(material.alpha_mode, ALPHA_MASK);
        assert!(!shared.hair.enabled, "the shared material is untouched");
        let sorted =
            super::set(&mut world, &json!({ "entity": e.to_bits(), "alpha": "sorted" })).expect("sorted");
        assert_eq!(sorted["after"]["alpha"], "sorted");
        assert_eq!(
            world
                .get::<MeshRenderer>(e)
                .expect("renderer")
                .material
                .alpha_mode,
            ALPHA_BLEND
        );
        let off = super::set(&mut world, &json!({ "entity": e.to_bits(), "enabled": false })).expect("off");
        assert_eq!(off["after"]["enabled"], false);
        assert_eq!(
            off["after"]["tip_alpha"], 0.25,
            "the tuning survives switching off"
        );
    }

    /// Review w11 #1: turning hair off puts back the alpha mode and cutoff the material had
    /// before hair turned it on (opaque stays opaque, a masked material keeps its own cutoff).
    #[test]
    fn switching_hair_off_restores_the_prior_alpha_mode() {
        use regia_render::ALPHA_OPAQUE;
        let (mut world, e, _) = world_with_card();
        let mode = |world: &World| {
            let m = &world.get::<MeshRenderer>(e).expect("renderer").material;
            (m.alpha_mode, m.alpha_cutoff)
        };
        let opaque = mode(&world);
        assert_eq!(opaque.0, ALPHA_OPAQUE);
        for strategy in ["sorted", "hashed"] {
            set(&mut world, &json!({ "entity": e.to_bits(), "alpha": strategy })).expect("on");
            assert_ne!(mode(&world).0, ALPHA_OPAQUE, "{strategy}: hair changes the mode");
            // A second patch while on must not overwrite the remembered mode.
            set(&mut world, &json!({ "entity": e.to_bits(), "alpha": "sorted" })).expect("again");
            set(&mut world, &json!({ "entity": e.to_bits(), "enabled": false })).expect("off");
            assert_eq!(mode(&world), opaque, "{strategy}: off restores the opaque mode");
            assert!(world.get::<HairPriorAlpha>(e).is_none());
        }
        // A masked material with its own cutoff keeps both through a sorted round trip.
        {
            let mut r = world.get_mut::<MeshRenderer>(e).expect("renderer");
            let mut m = (*r.material).clone();
            m.alpha_mode = ALPHA_MASK;
            m.alpha_cutoff = 0.3;
            r.material = Arc::new(m);
        }
        set(&mut world, &json!({ "entity": e.to_bits(), "alpha": "sorted" })).expect("on");
        assert_eq!(mode(&world).0, ALPHA_BLEND);
        set(&mut world, &json!({ "entity": e.to_bits(), "enabled": false })).expect("off");
        assert_eq!(mode(&world), (ALPHA_MASK, 0.3));
        // Off while already off changes nothing.
        set(&mut world, &json!({ "entity": e.to_bits(), "enabled": false })).expect("off again");
        assert_eq!(mode(&world), (ALPHA_MASK, 0.3));
    }

    #[test]
    fn bad_requests_are_rejected() {
        let (mut world, e, _) = world_with_card();
        for bad in [
            json!({ "entity": e.to_bits(), "alpha": "dither" }),
            json!({ "entity": e.to_bits(), "tip_alpha": 2.0 }),
            json!({ "entity": e.to_bits(), "tip_color": [1.0, 2.0] }),
            json!({ "entity": e.to_bits(), "ramp": [0.0] }),
            json!({ "entity": e.to_bits(), "enabled": "yes" }),
            json!({}),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
        }
        let plain = world.spawn_empty().id();
        assert!(get(&mut world, &json!({ "entity": plain.to_bits() })).is_err());
    }
}
