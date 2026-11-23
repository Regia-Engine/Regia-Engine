// SPDX-License-Identifier: MPL-2.0
//! Foliage wind and the dynamic-caster shadow class on the agent protocol
//! (`book/agents/foliage-wind.md`, `specs/88`, ADR-2028): `render.foliage_wind` reads the scene
//! wind the renderer writes into time-reading material graphs and what the dynamic-caster class
//! did on the last frame; `render.set_foliage_wind` and `render.set_dynamic_caster_shadows` set
//! them on the runtime renderer.

use regia_app::prelude::*;
use regia_render::{DynamicCasterShadows, FOLIAGE_WIND_MAX_M_PER_S, FrameRenderer, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself calls \
         FrameRenderer::set_foliage_wind and set_dynamic_caster_shadows)",
    )
}

fn mode_json(mode: DynamicCasterShadows) -> Value {
    match mode {
        DynamicCasterShadows::On {
            cascades,
            local_lights,
        } => {
            json!({ "mode": mode.name(), "cascades": cascades, "local_lights": local_lights })
        }
        _ => json!({ "mode": mode.name(), "cascades": null, "local_lights": null }),
    }
}

fn wind_json(renderer: &FrameRenderer) -> Value {
    let wind = renderer.foliage_wind();
    json!({
        "wind_m_per_s": wind,
        "speed_m_per_s": wind.map(|[x, z]| (x * x + z * z).sqrt()),
        "max_m_per_s": FOLIAGE_WIND_MAX_M_PER_S,
    })
}

/// `render.foliage_wind`: `{ wind_m_per_s, speed_m_per_s, max_m_per_s, dynamic_casters: { mode,
/// cascades, local_lights }, last_frame: { paged, cascade_mask, refreshed_cascades,
/// local_lights, animated_casters, skinned_casters, shared_boxes, dynamic_pages,
/// dynamic_atlas_tiles } }`. `skinned_casters` counts the bounded skinned casters among
/// `animated_casters` (ADR-5418).
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let renderer = &world.get_resource::<RuntimeRenderer>().ok_or_else(missing)?.0;
    let report = renderer.dynamic_caster_report();
    let mut out = wind_json(renderer);
    out["dynamic_casters"] = mode_json(renderer.dynamic_caster_shadows());
    out["last_frame"] = json!({
        "paged": !report.policy.unpaged,
        "cascade_mask": report.policy.cascade_mask,
        "refreshed_cascades": report.policy.cascade_count(),
        "local_lights": report.policy.local_lights,
        "animated_casters": report.animated_casters,
        "skinned_casters": report.skinned_casters,
        "shared_boxes": report.shared_boxes,
        "dynamic_pages": report.dynamic_pages,
        "dynamic_atlas_tiles": report.dynamic_atlas_tiles,
    });
    Ok(out)
}

/// `render.set_foliage_wind { wind_m_per_s: [x, z] | null }` → `{ before, after }` (each
/// `{ wind_m_per_s, speed_m_per_s, max_m_per_s }`). A vector longer than the maximum is
/// shortened to it; `null` leaves graph slot 0 `zw` as each material carries it.
pub(crate) fn set_wind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let raw = params
        .get("wind_m_per_s")
        .ok_or_else(|| RpcError::invalid_params("pass `wind_m_per_s`: [x, z] in m/s, or null"))?;
    let wind = if raw.is_null() {
        None
    } else {
        let pair = raw
            .as_array()
            .filter(|a| a.len() == 2)
            .and_then(|a| Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32]))
            .filter(|w| w.iter().all(|v| v.is_finite()))
            .ok_or_else(|| {
                RpcError::invalid_params("`wind_m_per_s` must be [x, z] (two finite numbers) or null")
            })?;
        Some(pair)
    };
    let mut renderer = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(missing)?;
    let before = wind_json(&renderer.0);
    renderer.0.set_foliage_wind(wind);
    let after = wind_json(&renderer.0);
    // With a `SceneWind` the call writes it, so the weather does not overwrite it next frame
    // (spec 99 § 3.3): `Manual` with the value, `Off` for null.
    if let Some(mut scene_wind) = world.get_resource_mut::<regia_scene::weather::SceneWind>() {
        scene_wind.set_explicitly(wind);
    }
    Ok(json!({ "before": before, "after": after }))
}

/// `render.set_dynamic_caster_shadows { mode: "tier" | "off" | "on", cascades?, local_lights? }`
/// → `{ before, after }` (each `{ mode, cascades, local_lights }`). `on` takes `cascades` 1–4
/// (default 2) and `local_lights` (default true).
pub(crate) fn set_dynamic_casters(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("pass `mode`: \"tier\", \"off\" or \"on\""))?;
    let mode = match mode {
        "tier" => DynamicCasterShadows::Tier,
        "off" => DynamicCasterShadows::Off,
        "on" => {
            let cascades = match params.get("cascades") {
                None | Some(Value::Null) => 2,
                Some(v) => v
                    .as_u64()
                    .filter(|n| (1..=4).contains(n))
                    .ok_or_else(|| RpcError::invalid_params("`cascades` must be an integer 1–4"))?
                    as u8,
            };
            let local_lights = match params.get("local_lights") {
                None | Some(Value::Null) => true,
                Some(v) => v
                    .as_bool()
                    .ok_or_else(|| RpcError::invalid_params("`local_lights` must be true or false"))?,
            };
            DynamicCasterShadows::On {
                cascades,
                local_lights,
            }
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown mode {other:?}: use \"tier\", \"off\" or \"on\""
            )));
        }
    };
    let mut renderer = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(missing)?;
    let before = mode_json(renderer.0.dynamic_caster_shadows());
    renderer.0.set_dynamic_caster_shadows(mode);
    let after = mode_json(renderer.0.dynamic_caster_shadows());
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        world
    }

    #[test]
    fn reads_the_defaults() {
        let mut world = world();
        let got = get(&mut world).expect("read");
        assert!(got["wind_m_per_s"].is_null());
        assert!(got["speed_m_per_s"].is_null());
        assert_eq!(got["dynamic_casters"]["mode"], "tier");
        assert_eq!(got["last_frame"]["animated_casters"], 0);
        assert_eq!(got["last_frame"]["skinned_casters"], 0);
        assert_eq!(got["last_frame"]["dynamic_pages"], 0);
        assert_eq!(got["last_frame"]["shared_boxes"], 0);
    }

    #[test]
    fn sets_and_clamps_the_wind() {
        let mut world = world();
        let out = set_wind(&mut world, &json!({ "wind_m_per_s": [3.0, 4.0] })).expect("set");
        assert!(out["before"]["wind_m_per_s"].is_null());
        assert_eq!(out["after"]["wind_m_per_s"], json!([3.0, 4.0]));
        assert!((out["after"]["speed_m_per_s"].as_f64().expect("speed") - 5.0).abs() < 1.0e-6);
        let out = set_wind(&mut world, &json!({ "wind_m_per_s": [300.0, 400.0] })).expect("storm");
        let speed = out["after"]["speed_m_per_s"].as_f64().expect("speed");
        assert!(
            (speed - f64::from(FOLIAGE_WIND_MAX_M_PER_S)).abs() < 1.0e-3,
            "{speed}"
        );
        let out = set_wind(&mut world, &json!({ "wind_m_per_s": null })).expect("clear");
        assert!(out["after"]["wind_m_per_s"].is_null());
        assert!(world.resource::<RuntimeRenderer>().0.foliage_wind().is_none());
    }

    #[test]
    fn rejects_bad_wind() {
        let mut world = world();
        for params in [
            json!({}),
            json!({ "wind_m_per_s": [1.0] }),
            json!({ "wind_m_per_s": "east" }),
            json!({ "wind_m_per_s": [1.0, "x"] }),
        ] {
            let err = set_wind(&mut world, &params).expect_err("bad wind");
            assert!(format!("{err:?}").contains("F0002"), "{params}: {err:?}");
        }
    }

    #[test]
    fn sets_the_dynamic_caster_mode() {
        let mut world = world();
        let out = set_dynamic_casters(
            &mut world,
            &json!({ "mode": "on", "cascades": 3, "local_lights": false }),
        )
        .expect("on");
        assert_eq!(out["before"]["mode"], "tier");
        assert_eq!(
            out["after"],
            json!({ "mode": "on", "cascades": 3, "local_lights": false })
        );
        assert_eq!(
            world.resource::<RuntimeRenderer>().0.dynamic_caster_shadows(),
            DynamicCasterShadows::On {
                cascades: 3,
                local_lights: false
            }
        );
        let out = set_dynamic_casters(&mut world, &json!({ "mode": "on" })).expect("defaults");
        assert_eq!(
            out["after"],
            json!({ "mode": "on", "cascades": 2, "local_lights": true })
        );
        let out = set_dynamic_casters(&mut world, &json!({ "mode": "off" })).expect("off");
        assert_eq!(out["after"]["mode"], "off");
        for params in [
            json!({}),
            json!({ "mode": "sometimes" }),
            json!({ "mode": "on", "cascades": 0 }),
            json!({ "mode": "on", "cascades": 5 }),
            json!({ "mode": "on", "local_lights": 1 }),
        ] {
            let err = set_dynamic_casters(&mut world, &params).expect_err("bad mode");
            assert!(format!("{err:?}").contains("F0002"), "{params}: {err:?}");
        }
    }

    #[test]
    fn without_a_renderer_the_calls_say_what_to_add() {
        let mut world = World::new();
        for err in [
            get(&mut world).expect_err("get"),
            set_wind(&mut world, &json!({ "wind_m_per_s": [1.0, 0.0] })).expect_err("set wind"),
            set_dynamic_casters(&mut world, &json!({ "mode": "off" })).expect_err("set mode"),
        ] {
            assert!(format!("{err:?}").contains("F0003"), "{err:?}");
        }
    }
}

#[cfg(test)]
mod scene_wind_tests {
    use regia_render::RuntimeRenderer;
    use regia_scene::weather::{SceneWind, SceneWindMode};
    use serde_json::json;

    use super::*;

    #[test]
    fn missing_renderer_does_not_change_scene_wind() {
        let mut world = World::new();
        let before = SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [5.0, 0.0],
            mean_m_per_s: [4.0, 1.0],
        };
        world.insert_resource(before);
        for wind in [json!([1.0, 2.0]), Value::Null] {
            let err = set_wind(&mut world, &json!({ "wind_m_per_s": wind })).expect_err("no renderer");
            assert_eq!(err.code, crate::protocol::NOT_FOUND);
            assert_eq!(*world.resource::<SceneWind>(), before);
        }
    }

    /// Spec 99 § 3.3: with a `SceneWind`, the call writes it.
    #[test]
    fn the_call_writes_the_scene_wind_when_it_exists() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.insert_resource(SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [5.0, 0.0],
            mean_m_per_s: [5.0, 0.0],
        });
        set_wind(&mut world, &json!({ "wind_m_per_s": [1.0, 2.0] })).expect("set");
        assert_eq!(*world.resource::<SceneWind>(), SceneWind::manual([1.0, 2.0]));
        set_wind(&mut world, &json!({ "wind_m_per_s": null })).expect("clear");
        assert_eq!(world.resource::<SceneWind>().mode, SceneWindMode::Off);
    }

    /// A call that answers `F0003` (no `RuntimeRenderer`) changes nothing: the `SceneWind` the
    /// weather drives (and the audio bed hears, ADR-2442) keeps its mode and value.
    #[test]
    fn a_refused_call_leaves_the_scene_wind_alone() {
        let mut world = World::new();
        let weather = SceneWind {
            mode: SceneWindMode::Weather,
            velocity_m_per_s: [5.0, 0.0],
            mean_m_per_s: [5.0, 0.0],
        };
        world.insert_resource(weather);
        for params in [
            json!({ "wind_m_per_s": [1.0, 2.0] }),
            json!({ "wind_m_per_s": null }),
        ] {
            let err = set_wind(&mut world, &params).expect_err("no renderer");
            assert!(format!("{err:?}").contains("F0003"), "{err:?}");
            assert_eq!(*world.resource::<SceneWind>(), weather, "{params}");
        }
    }
}
