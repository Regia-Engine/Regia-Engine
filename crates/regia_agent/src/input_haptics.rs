// SPDX-License-Identifier: MPL-2.0
//! `input.haptics`: read or change the player's gamepad rumble intensity and phone vibration
//! scale (book `agents/input-assists.md`, ADR-2646, ADR-3464). A change writes
//! `GameSettings.accessibility` (persisted like `settings.set`) and the live `Gamepads` or
//! `PhoneVibration` resource at once.

use regia_ecs::prelude::*;
use regia_framework::settings::GameSettings;
use regia_input::{Gamepads, PhoneVibration};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Params {
    rumble_scale: Option<f32>,
    vibration_scale: Option<f32>,
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "input_haptics_bad_params",
        format!("bad `input.haptics` parameters: {detail}"),
        "pass { rumble_scale: 0.5, vibration_scale: 0.5 }, or nothing to read",
    )
}

fn snapshot(world: &World) -> Value {
    let pads = world.get_resource::<Gamepads>().map(Gamepads::rumble_scale);
    let phone = world.get_resource::<PhoneVibration>().map(PhoneVibration::scale);
    let settings = world.get_resource::<GameSettings>();
    let saved_rumble = settings.map(|s| s.accessibility.rumble_scale);
    let saved_vibration = settings.map(|s| {
        let scale = s.accessibility.vibration_scale;
        if scale.is_nan() {
            1.0
        } else {
            scale.clamp(0.0, 1.0)
        }
    });
    json!({
        "rumble_scale": pads.or(saved_rumble).unwrap_or(1.0),
        "vibration_scale": phone.or(saved_vibration).unwrap_or(1.0),
        "live": pads.is_some() || phone.is_some(),
        "persisted": settings.is_some(),
    })
}

fn in_unit(scale: f32) -> bool {
    scale.is_finite() && (0.0..=1.0).contains(&scale)
}

/// `input.haptics { rumble_scale? }`: with no parameters it only reports.
pub(crate) fn haptics(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let params: Params = match params {
        Value::Null => Params::default(),
        Value::Object(_) => serde_json::from_value(params.clone()).map_err(bad_params)?,
        _ => return Err(bad_params("expected an object")),
    };
    if let Some(scale) = params.rumble_scale
        && !in_unit(scale)
    {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "input_haptics_bad_scale",
            "`rumble_scale` must be a number from 0 to 1",
            "0 turns rumble off, 1 plays it as the game asks",
        ));
    }
    if let Some(scale) = params.vibration_scale
        && !in_unit(scale)
    {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "input_haptics_bad_scale",
            "`vibration_scale` must be a number from 0 to 1",
            "0 turns phone vibration off, 1 plays it as the game asks",
        ));
    }
    if let Some(scale) = params.rumble_scale {
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>()
            && settings.accessibility.rumble_scale != scale
        {
            settings.accessibility.rumble_scale = scale;
        }
        if let Some(mut pads) = world.get_resource_mut::<Gamepads>() {
            pads.set_rumble_scale(scale);
        }
    }
    if let Some(scale) = params.vibration_scale {
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>()
            && settings.accessibility.vibration_scale != scale
        {
            settings.accessibility.vibration_scale = scale;
        }
        world.get_resource_or_insert_with(PhoneVibration::default);
        world.resource_mut::<PhoneVibration>().set_scale(scale);
    }
    Ok(snapshot(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_full_strength_by_default() {
        let mut world = World::new();
        let out = haptics(&mut world, &json!({})).expect("report");
        assert_eq!(
            out,
            json!({ "rumble_scale": 1.0, "vibration_scale": 1.0, "live": false, "persisted": false })
        );
    }

    #[test]
    fn sets_the_live_scale_and_the_persisted_settings() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        world.init_resource::<Gamepads>();
        let out = haptics(&mut world, &json!({ "rumble_scale": 0.25 })).expect("set");
        assert_eq!(
            out,
            json!({ "rumble_scale": 0.25, "vibration_scale": 1.0, "live": true, "persisted": true })
        );
        assert_eq!(world.resource::<Gamepads>().rumble_scale(), 0.25);
        assert_eq!(world.resource::<GameSettings>().accessibility.rumble_scale, 0.25);
    }

    #[test]
    fn rejects_bad_values_and_changes_nothing() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        for (bad, want) in [
            (json!({ "rumble_scale": 1.5 }), "input_haptics_bad_scale"),
            (json!({ "rumble_scale": -0.5 }), "input_haptics_bad_scale"),
            (json!({ "rumble_scale": "off" }), "input_haptics_bad_params"),
            (json!({ "strength": 0.5 }), "input_haptics_bad_params"),
            (json!({ "vibration_scale": 1.5 }), "input_haptics_bad_scale"),
        ] {
            let err = haptics(&mut world, &bad).expect_err("bad");
            assert_eq!(err.data.expect("data")["code"], json!(want), "{bad}");
        }
        assert_eq!(world.resource::<GameSettings>().accessibility.rumble_scale, 1.0);
        assert_eq!(
            world.resource::<GameSettings>().accessibility.vibration_scale,
            1.0
        );
    }

    #[test]
    fn haptics_sets_phone_vibration_scale() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        let out = haptics(&mut world, &json!({ "vibration_scale": 0.25 })).expect("set");
        assert_eq!(out["vibration_scale"], json!(0.25));
        assert_eq!(out["rumble_scale"], json!(1.0));
        assert_eq!(world.resource::<PhoneVibration>().scale(), 0.25);
        assert_eq!(
            world.resource::<GameSettings>().accessibility.vibration_scale,
            0.25
        );
        assert_eq!(world.resource::<GameSettings>().accessibility.rumble_scale, 1.0);
    }
}
