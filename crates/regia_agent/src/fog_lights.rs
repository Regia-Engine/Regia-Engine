// SPDX-License-Identifier: MPL-2.0
//! `render.fog_lights`: the local lights the froxel fog picked for the last extracted frame, their
//! scores (`regia_render::FogLightPick`) and whether the last drawn frame shadowed each one from
//! the local shadow atlas (`FrameRenderer::fog_light_shadowed`; ADR-1365,
//! `book/agents/volumetric-fog.md`).

use regia_app::prelude::*;
use regia_render::{FOG_LIGHT_SLOTS, FOG_PICK_HYSTERESIS, FogLightChoice, FogLightPick, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn choice_json(c: &FogLightChoice, shadowed: bool) -> Value {
    let light = &c.light;
    let mut v = json!({
        "entity": c.key,
        "kind": if light.cone.is_some() { "spot" } else { "point" },
        "position": light.position,
        "color": light.color,
        "intensity_cd": light.intensity,
        "radius": light.radius,
        "score": c.score,
        "held": c.held,
        "shadowed": shadowed,
    });
    if let Some(cone) = light.cone {
        v["direction"] = json!(cone.direction);
        v["cos_inner"] = json!(cone.cos_inner);
        v["cos_outer"] = json!(cone.cos_outer);
    }
    v
}

/// The JSON for a pick: `{ slots, hysteresis, candidates, eye, lights: [...] }`. `shadowed` is
/// per slot, from the last drawn frame.
fn pick_json(pick: &FogLightPick, shadowed: [bool; 4]) -> Value {
    let lights: Vec<Value> = pick
        .chosen
        .iter()
        .enumerate()
        .map(|(slot, c)| choice_json(c, shadowed.get(slot).copied().unwrap_or(false)))
        .collect();
    json!({
        "slots": FOG_LIGHT_SLOTS,
        "hysteresis": FOG_PICK_HYSTERESIS,
        "candidates": pick.candidates,
        "eye": pick.eye,
        "lights": lights,
    })
}

/// `render.fog_lights`: `{ slots, hysteresis, candidates, eye, lights: [{ entity, kind, position,
/// color, intensity_cd, radius, score, held, shadowed, direction?, cos_inner?, cos_outer? }] }` in
/// slot order. `shadowed` is false without a `RuntimeRenderer`.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let pick = world.get_resource::<FogLightPick>().ok_or_else(|| {
        RpcError::not_found(
            "no FogLightPick resource in this app",
            "add regia_render::RenderPlugin (it inserts FogLightPick; extract_frame fills it)",
        )
    })?;
    let shadowed = world
        .get_resource::<RuntimeRenderer>()
        .map_or([false; 4], |renderer| renderer.0.fog_light_shadowed());
    Ok(pick_json(pick, shadowed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::FogLight;

    #[test]
    fn fog_lights_lists_the_pick_in_slot_order() {
        let mut world = World::new();
        assert!(get(&mut world).is_err(), "no pick resource");
        world.insert_resource(FogLightPick {
            chosen: vec![
                FogLightChoice {
                    key: 7,
                    light: FogLight::point([1.0, 2.0, 3.0], [1.0; 3], 500.0, 6.0),
                    score: 12.5,
                    held: true,
                },
                FogLightChoice {
                    key: 3,
                    light: FogLight::spot([0.0; 3], [0.0, 0.0, -1.0], [1.0; 3], 1_000.0, 20.0, 0.2, 0.4),
                    score: 2.5,
                    held: false,
                },
            ],
            candidates: 5,
            eye: [0.0, 1.5, 0.0],
        });
        let v = get(&mut world).expect("pick");
        assert_eq!(v["slots"], json!(4));
        assert_eq!(v["candidates"], json!(5));
        assert_eq!(v["lights"].as_array().map(Vec::len), Some(2));
        assert_eq!(v["lights"][0]["entity"], json!(7));
        assert_eq!(v["lights"][0]["kind"], json!("point"));
        assert_eq!(v["lights"][0]["held"], json!(true));
        assert_eq!(v["lights"][0]["shadowed"], json!(false), "no renderer, no shadow");
        assert!(v["lights"][0].get("cos_outer").is_none());
        assert_eq!(v["lights"][1]["kind"], json!("spot"));
        assert_eq!(v["lights"][1]["intensity_cd"], json!(1_000.0));
        let cos_outer = v["lights"][1]["cos_outer"].as_f64().unwrap_or(f64::NAN);
        assert!((cos_outer - 0.4_f64.cos()).abs() < 1.0e-6, "{cos_outer}");
    }
}
