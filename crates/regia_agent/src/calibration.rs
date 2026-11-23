// SPDX-License-Identifier: MPL-2.0
//! `render.get_calibration` / `render.set_calibration`: the player's display calibration
//! (ADR-092, book `user-guide/display-calibration.md`, guide topic `calibration`).
//!
//! The values live in the renderer's [`DisplayCalibration`] resource; when the app has the
//! framework's [`GameSettings`], `set` also writes `graphics.calibration` so the change persists
//! (the settings plugin re-applies it on the next update). Screenshots stay comparable with
//! `seeing.screenshot { "calibration": "neutral" }`.

use regia_ecs::prelude::*;
use regia_framework::settings::{CalibrationSettings, GameSettings};
use regia_render::{DisplayCalibration, DisplayLuminance, DisplayPlatform, HdrOutput, OutputSettings};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn platform_name(platform: DisplayPlatform) -> &'static str {
    match platform {
        DisplayPlatform::Desktop => "desktop",
        DisplayPlatform::Mobile => "mobile",
        DisplayPlatform::Deck => "deck",
    }
}

fn platform(world: &World) -> DisplayPlatform {
    world
        .get_resource::<DisplayPlatform>()
        .copied()
        .unwrap_or_else(DisplayPlatform::current)
}

fn calibration_json(c: &DisplayCalibration) -> Value {
    json!({
        "black_level": c.black_level,
        "gamma": c.gamma,
        "paper_white_nits": c.paper_white_nits,
        "peak_nits": c.peak_nits,
        "min_nits": c.min_nits,
    })
}

fn current(world: &World) -> DisplayCalibration {
    world
        .get_resource::<DisplayCalibration>()
        .copied()
        .unwrap_or_else(|| DisplayCalibration::platform_default(platform(world)))
}

/// `render.get_calibration`: the live calibration, whether it is neutral, the platform default,
/// what the display reports, and whether HDR output is requested.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let live = current(world);
    let platform = platform(world);
    let display = world
        .get_resource::<DisplayLuminance>()
        .copied()
        .unwrap_or_default();
    let hdr = world
        .get_resource::<OutputSettings>()
        .is_some_and(|o| o.hdr != HdrOutput::Sdr);
    let player_set = world
        .get_resource::<GameSettings>()
        .map(|s| s.graphics.calibration.is_some());
    Ok(json!({
        "calibration": calibration_json(&live),
        "sdr_neutral": live.is_sdr_neutral(),
        "platform": platform_name(platform),
        "platform_default": calibration_json(&DisplayCalibration::platform_default(platform)),
        "display": { "peak_nits": display.peak_nits, "min_nits": display.min_nits },
        "hdr_requested": hdr,
        "persisted": player_set,
    }))
}

fn field(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number"))),
    }
}

/// `render.set_calibration`: changes the given fields (others keep their value), or
/// `{ "reset": true }` back to the platform default. Values are clamped to the player ranges.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let before = current(world);
    let flag = |name: &str| -> Result<Option<bool>, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be a boolean"))),
        }
    };
    let reset = flag("reset")?.unwrap_or(false);
    let hdr_param = flag("hdr")?;
    let after = if reset {
        let default = DisplayCalibration::platform_default(platform(world));
        world
            .get_resource::<DisplayLuminance>()
            .map_or(default, |d| default.with_display(d))
    } else {
        let mut c = before;
        if let Some(v) = field(params, "black_level")? {
            c.black_level = v;
        }
        if let Some(v) = field(params, "gamma")? {
            c.gamma = v;
        }
        if let Some(v) = field(params, "paper_white_nits")? {
            c.paper_white_nits = v;
        }
        if let Some(v) = field(params, "peak_nits")? {
            c.peak_nits = v;
        }
        if let Some(v) = field(params, "min_nits")? {
            c.min_nits = v;
        }
        c.clamped()
    };
    world.insert_resource(after);
    let hdr_now = hdr_param.unwrap_or_else(|| {
        world
            .get_resource::<OutputSettings>()
            .is_some_and(|o| o.hdr != HdrOutput::Sdr)
    });
    if let Some(mut output) = world.get_resource_mut::<OutputSettings>()
        && let Some(hdr) = hdr_param
    {
        output.hdr = if hdr { HdrOutput::Hdr10 } else { HdrOutput::Sdr };
    }
    let persisted = if let Some(mut settings) = world.get_resource_mut::<GameSettings>() {
        settings.graphics.calibration =
            (!reset).then(|| CalibrationSettings::from_calibration(&after, hdr_now));
        true
    } else {
        false
    };
    Ok(json!({
        "before": calibration_json(&before),
        "after": calibration_json(&after),
        "sdr_neutral": after.is_sdr_neutral(),
        "persisted": persisted,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clamps_merges_and_resets() {
        let mut world = World::new();
        world.insert_resource(DisplayPlatform::Desktop);
        world.insert_resource(DisplayCalibration::NEUTRAL);
        world.insert_resource(GameSettings::default());
        let out = set(&mut world, &json!({ "black_level": 0.4, "gamma": 9.0 })).expect("set");
        assert_eq!(out["after"]["black_level"], json!(0.4_f32));
        assert_eq!(out["after"]["gamma"], json!(2.6_f32), "clamped");
        assert_eq!(out["persisted"], json!(true));
        let stored = world.resource::<GameSettings>().graphics.calibration.clone();
        assert_eq!(stored.map(|c| c.black_level), Some(0.4));
        let got = get(&mut world).expect("get");
        assert_eq!(got["sdr_neutral"], json!(false));
        assert_eq!(got["platform"], json!("desktop"));
        let reset = set(&mut world, &json!({ "reset": true })).expect("reset");
        assert_eq!(reset["sdr_neutral"], json!(true));
        assert_eq!(world.resource::<GameSettings>().graphics.calibration, None);
        assert!(set(&mut world, &json!({ "gamma": "dark" })).is_err());
    }

    #[test]
    fn flags_must_be_booleans_and_a_refused_call_changes_nothing() {
        let mut world = World::new();
        world.insert_resource(DisplayPlatform::Desktop);
        world.insert_resource(DisplayCalibration::NEUTRAL);
        world.insert_resource(GameSettings::default());
        set(&mut world, &json!({ "black_level": 0.3 })).expect("set");
        let live = *world.resource::<DisplayCalibration>();
        // `reset: "true"` used to be ignored, leaving the calibration unreset.
        for bad in [
            json!({ "reset": "true" }),
            json!({ "reset": 1 }),
            json!({ "hdr": "on" }),
            json!({ "black_level": 0.9, "hdr": [] }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "{bad}");
            assert_eq!(*world.resource::<DisplayCalibration>(), live, "{bad}");
        }
        assert_eq!(
            world
                .resource::<GameSettings>()
                .graphics
                .calibration
                .as_ref()
                .map(|c| c.black_level),
            Some(0.3)
        );
        // Nulls mean absent.
        set(&mut world, &json!({ "reset": null, "hdr": null, "gamma": null })).expect("nulls");
        assert_eq!(*world.resource::<DisplayCalibration>(), live);
    }

    #[test]
    fn out_of_range_numbers_clamp_to_the_player_range_and_repeat_identically() {
        let mut world = World::new();
        world.insert_resource(DisplayPlatform::Desktop);
        world.insert_resource(DisplayCalibration::NEUTRAL);
        // 1e300 narrows to infinity; the clamp must still give a finite value.
        let big = json!({ "black_level": 1.0e300, "gamma": -1.0e300, "paper_white_nits": 1.0e300, "peak_nits": 1.0e300, "min_nits": -5.0 });
        let a = set(&mut world, &big).expect("big");
        for key in [
            "black_level",
            "gamma",
            "paper_white_nits",
            "peak_nits",
            "min_nits",
        ] {
            let v = a["after"][key].as_f64().unwrap_or(f64::NAN);
            assert!(v.is_finite(), "{key} = {v}");
        }
        assert_eq!(
            set(&mut world, &big).expect("again")["after"],
            a["after"],
            "the clamp is idempotent"
        );
        assert_eq!(a["persisted"], json!(false), "no GameSettings, nothing persisted");
        // No resources at all: the live calibration falls back to the platform default.
        let mut bare = World::new();
        let got = get(&mut bare).expect("bare get");
        assert_eq!(got["calibration"], got["platform_default"]);
        assert_eq!(got["persisted"], Value::Null);
    }

    /// `hdr: true` is stored on GameSettings, and it changes OutputSettings only when that
    /// resource exists. Without it, `hdr_requested` stays false.
    #[test]
    fn set_calibration_hdr_without_output_settings_stays_unrequested() {
        let mut world = World::new();
        world.insert_resource(DisplayPlatform::Desktop);
        world.insert_resource(DisplayCalibration::NEUTRAL);
        world.insert_resource(GameSettings::default());
        let out = set(&mut world, &json!({ "hdr": true })).expect("set");
        assert_eq!(out["persisted"], json!(true));
        assert!(world.get_resource::<OutputSettings>().is_none());
        let stored = world.resource::<GameSettings>().graphics.calibration.clone();
        assert_eq!(stored.map(|c| c.hdr), Some(true));
        let got = get(&mut world).expect("get");
        assert_eq!(got["hdr_requested"], json!(false));
    }
}
