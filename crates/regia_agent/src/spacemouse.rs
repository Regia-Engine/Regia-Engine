// SPDX-License-Identifier: MPL-2.0
//! `input.spacemouse_status` / `input.spacemouse_inject` / `input.spacemouse_nav_defaults`:
//! the SpaceMouse agent surface (`docs/specs/29-spacemouse.md` § 4, guide topic `spacemouse`,
//! book `agents/spacemouse.md`). The device layer is `regia_input::spacemouse` (desktop only;
//! on Android every call answers "unsupported").

use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{RpcError, UNSUPPORTED};

#[cfg(target_os = "android")]
fn unsupported() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "F0013",
        "the SpaceMouse is desktop-only",
        "run the app on Linux or Windows",
    )
}

#[cfg(not(target_os = "android"))]
fn no_plugin() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "F0013",
        "no SpaceMousePlugin in this app",
        "add regia_input::InputPlugin (it installs the SpaceMouse plugin on desktop)",
    )
}

/// `input.spacemouse_status`: devices, connection state, the last merged sample, the user
/// config and the runtime toggles.
#[cfg(not(target_os = "android"))]
pub(crate) fn status(world: &mut World) -> Result<Value, RpcError> {
    use regia_input::spacemouse::{
        LINUX_UDEV_RULE, SpaceMouseDeviceStatus, SpaceMouseDevices, SpaceMouseInjection, SpaceMouseNav,
        SpaceMouseState,
    };
    let devices = world.get_resource::<SpaceMouseDevices>().ok_or_else(no_plugin)?;
    let list: Vec<Value> = devices
        .devices
        .iter()
        .map(|d| {
            let (status, reason) = match &d.status {
                SpaceMouseDeviceStatus::Connected => ("connected", Value::Null),
                SpaceMouseDeviceStatus::Unavailable(reason) => ("unavailable", json!(reason)),
            };
            json!({
                "id": d.info.id,
                "name": d.info.name,
                "vendor_id": format!("{:04x}", d.info.vendor_id),
                "product_id": format!("{:04x}", d.info.product_id),
                "source": d.info.source,
                "status": status,
                "reason": reason,
            })
        })
        .collect();
    let connected = devices.connected();
    let backend = devices.backend;
    let state = world
        .get_resource::<SpaceMouseState>()
        .copied()
        .unwrap_or_default();
    let nav = world.get_resource::<SpaceMouseNav>().copied().unwrap_or_default();
    let injected = world
        .get_resource::<SpaceMouseInjection>()
        .map_or(0, SpaceMouseInjection::remaining);
    let config = world
        .get_resource::<regia_input::InputConfig>()
        .map(|c| c.spacemouse.sanitized())
        .unwrap_or_default();
    let unavailable = list.len() - connected;
    Ok(json!({
        "backend": backend,
        "connected": connected,
        "devices": list,
        "state": {
            "translation": state.translation,
            "rotation": state.rotation,
            "buttons": state.buttons,
        },
        "injected_frames_left": injected,
        "config": {
            "enabled": config.enabled,
            "sensitivity": config.sensitivity,
            "invert": config.invert,
        },
        "toggles": {
            "horizon_lock": nav.horizon_lock,
            "dominant": nav.dominant,
            "object_tool": nav.object_tool,
        },
        "linux_udev_rule": (cfg!(target_os = "linux") && unavailable > 0).then_some(LINUX_UDEV_RULE),
    }))
}

#[cfg(not(target_os = "android"))]
fn axes(params: &Value, name: &str) -> Result<[f32; 3], RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok([0.0; 3]),
        Some(Value::Array(values)) if values.len() == 3 => {
            let mut out = [0.0; 3];
            for (slot, value) in out.iter_mut().zip(values) {
                let v = value.as_f64().ok_or_else(|| {
                    RpcError::invalid_params(format!("`{name}` must be three numbers in -1..1"))
                })?;
                *slot = (v as f32).clamp(-1.0, 1.0);
            }
            Ok(out)
        }
        Some(_) => Err(RpcError::invalid_params(format!(
            "`{name}` must be an array of three numbers in -1..1"
        ))),
    }
}

#[cfg(not(target_os = "android"))]
fn buttons(params: &Value) -> Result<u32, RpcError> {
    match params.get("buttons") {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(mask)) => mask
            .as_u64()
            .and_then(|m| u32::try_from(m).ok())
            .ok_or_else(|| RpcError::invalid_params("`buttons` bitmask must fit in 32 bits")),
        Some(Value::Array(list)) => list.iter().try_fold(0u32, |mask, b| match b.as_u64() {
            Some(n @ 1..=32) => Ok(mask | 1 << (n - 1)),
            _ => Err(RpcError::invalid_params(
                "`buttons` entries are button numbers 1..32",
            )),
        }),
        Some(_) => Err(RpcError::invalid_params(
            "`buttons` is a bitmask number or an array of button numbers",
        )),
    }
}

/// `input.spacemouse_inject`: replaces the device state with the given one for `frames`
/// frames (default 1). Values are normalised (-1..1, § 2.1 frame), before the dead zone.
#[cfg(not(target_os = "android"))]
pub(crate) fn inject(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_input::spacemouse::{SpaceMouseInjection, SpaceMouseState};
    let state = SpaceMouseState {
        translation: axes(params, "translation")?,
        rotation: axes(params, "rotation")?,
        buttons: buttons(params)?,
    };
    let frames = match params.get("frames") {
        None | Some(Value::Null) => 1,
        Some(v) => v
            .as_u64()
            .and_then(|f| u32::try_from(f).ok())
            .filter(|f| *f >= 1)
            .ok_or_else(|| RpcError::invalid_params("`frames` must be a positive integer"))?,
    };
    let frame = world
        .get_resource::<regia_app::prelude::Time>()
        .map_or(0, regia_app::prelude::Time::frame_count);
    let mut injection = world
        .get_resource_mut::<SpaceMouseInjection>()
        .ok_or_else(no_plugin)?;
    injection.inject(state, frames);
    Ok(json!({
        "injected": {
            "translation": state.translation,
            "rotation": state.rotation,
            "buttons": state.buttons,
        },
        "frames": frames,
        "from_frame": frame + 1,
    }))
}

/// `input.spacemouse_nav_defaults`: the fixed § 2 constants every app shares.
#[cfg(not(target_os = "android"))]
pub(crate) fn nav_defaults() -> Value {
    use regia_input::SpaceMouseConfig;
    use regia_input::spacemouse::{NavigationDefaults as D, button_action};
    json!({
        "raw_full_scale": D::RAW_FULL_SCALE,
        "dead_zone": D::DEAD_ZONE,
        "curve_exponent": D::CURVE_EXPONENT,
        "orbit": {
            "pan_rate_distances_per_s": D::PAN_RATE,
            "zoom_rate": D::ZOOM_RATE,
            "rotation_rate_rad_per_s": D::ROTATION_RATE,
            "roll_with_horizon_lock": false,
        },
        "fly": {
            "speed_scene_scales_per_s": D::FLY_SPEED,
            "rotation_rate_rad_per_s": D::ROTATION_RATE,
        },
        "turntable": "the orbit delta (yaw, pitch, pan, zoom) applied to the object",
        "view2d": {
            "pan_rate_extents_per_s": D::VIEW2D_PAN_RATE,
            "zoom_rate": D::ZOOM_RATE,
        },
        "timeline": {
            "scrub_rate_spans_per_s": D::TIMELINE_SCRUB_RATE,
            "scrub_axis": "ry (twist counter-clockwise = forward)",
            "zoom_rate": D::ZOOM_RATE,
        },
        "step": {
            "threshold": D::STEP_THRESHOLD,
            "repeat_s": D::STEP_REPEAT_S,
            "axis": "rz (tilt right = next, left = previous)",
        },
        "object_tool": {
            "default": false,
            "toggle_action": regia_input::spacemouse::actions::OBJECT_TOOL_TOGGLE,
            "gesture_rest_s": D::GESTURE_REST_S,
        },
        "pitch_limit_deg": D::PITCH_LIMIT_DEG,
        "horizon_lock": D::HORIZON_LOCK,
        "dominant_axis": D::DOMINANT_AXIS,
        "max_dt_s": D::MAX_DT,
        "default_mode": "orbit",
        "frame": "+x right, +y up, +z toward the viewer; rx tilt away, ry twist counter-clockwise from above, rz tilt left",
        "buttons": (1..=5u8).map(|b| json!({ "button": b, "action": button_action(b) })).collect::<Vec<_>>(),
        "user_overrides": {
            "enabled": true,
            "sensitivity": { "default": 1.0, "min": SpaceMouseConfig::SENSITIVITY_MIN, "max": SpaceMouseConfig::SENSITIVITY_MAX },
            "invert": [false, false, false, false, false, false],
        },
    })
}

#[cfg(target_os = "android")]
pub(crate) fn status(_world: &mut World) -> Result<Value, RpcError> {
    Err(unsupported())
}

#[cfg(target_os = "android")]
pub(crate) fn inject(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Err(unsupported())
}

#[cfg(target_os = "android")]
pub(crate) fn nav_defaults() -> Value {
    json!({ "unsupported": "the SpaceMouse is desktop-only" })
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_input::spacemouse::{FakeSpaceMouse, SpaceMousePlugin, SpaceMouseState};

    #[test]
    fn inject_drives_the_state_and_status_reports_it() {
        let fake = FakeSpaceMouse::default();
        let mut app = App::new();
        app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()));
        app.add_plugins(regia_input::InputPlugin::default());
        fake.connect(3, "SpaceMouse Compact");
        app.update();
        let out = inject(
            app.world_mut(),
            &json!({ "translation": [0.5, 0.0, -2.0], "buttons": [2], "frames": 2 }),
        )
        .expect("inject");
        assert_eq!(out["frames"], json!(2));
        app.update();
        let state = *app.world().resource::<SpaceMouseState>();
        assert_eq!(state.translation, [0.5, 0.0, -1.0], "clamped");
        assert_eq!(state.buttons, 0b10);
        let status = status(app.world_mut()).expect("status");
        assert_eq!(status["connected"], json!(1));
        assert_eq!(status["backend"], json!("fake"));
        assert_eq!(status["devices"][0]["name"], json!("SpaceMouse Compact"));
        assert_eq!(status["devices"][0]["source"], json!("fake"));
        assert_eq!(status["injected_frames_left"], json!(1));
        assert_eq!(status["toggles"]["horizon_lock"], json!(true));
        assert!(inject(app.world_mut(), &json!({ "rotation": [1, 2] })).is_err());
        assert!(inject(app.world_mut(), &json!({ "buttons": [0] })).is_err());
        assert!(inject(app.world_mut(), &json!({ "frames": 0 })).is_err());
    }

    #[test]
    fn nav_defaults_quote_the_spec() {
        let d = nav_defaults();
        assert_eq!(d["dead_zone"], json!(0.04_f32));
        assert_eq!(d["orbit"]["zoom_rate"], json!(1.5_f32));
        assert_eq!(d["buttons"][1]["action"], json!("view.home"));
        assert_eq!(d["step"]["repeat_s"], json!(0.35_f32));
        assert!(
            status(&mut World::new()).is_err(),
            "no plugin: a structured error"
        );
    }

    #[test]
    fn inject_bounds_on_buttons_axes_and_frames_and_a_refusal_injects_nothing() {
        let fake = FakeSpaceMouse::default();
        let mut app = App::new();
        app.add_plugins(SpaceMousePlugin::with_backend(fake.backend()));
        app.add_plugins(regia_input::InputPlugin::default());
        app.update();
        let left = |app: &mut App| status(app.world_mut()).expect("status")["injected_frames_left"].clone();
        for bad in [
            json!({ "buttons": [33] }),
            json!({ "buttons": [1, 0] }),
            json!({ "buttons": [1.5] }),
            json!({ "buttons": ["1"] }),
            json!({ "buttons": 4_294_967_296_u64 }),
            json!({ "buttons": -1 }),
            json!({ "buttons": true }),
            json!({ "translation": [0, 0, "x"] }),
            json!({ "translation": [0, 0, 0, 0] }),
            json!({ "rotation": "none" }),
            json!({ "frames": 4_294_967_296_u64 }),
            json!({ "frames": -1 }),
            json!({ "frames": "2" }),
            json!({ "frames": 1.5 }),
        ] {
            let err = inject(app.world_mut(), &bad).expect_err("refused");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{bad}");
            assert_eq!(left(&mut app), json!(0), "{bad} must inject nothing");
        }
        // The boundaries themselves are accepted.
        let top = inject(app.world_mut(), &json!({ "buttons": [32, 1, 1] })).expect("32 and a duplicate");
        assert_eq!(top["injected"]["buttons"], json!(0x8000_0001_u32));
        let all = inject(app.world_mut(), &json!({ "buttons": 4_294_967_295_u64 })).expect("full mask");
        assert_eq!(all["injected"]["buttons"], json!(u32::MAX));
        let long = inject(app.world_mut(), &json!({ "frames": u32::MAX })).expect("max frames");
        assert_eq!(long["frames"], json!(u32::MAX));
        // Huge axis values saturate at +-1 rather than becoming NaN; nulls are zero.
        let sat = inject(
            app.world_mut(),
            &json!({ "translation": [1.0e300, -1.0e300, 0.5], "rotation": null }),
        )
        .expect("saturate");
        assert_eq!(sat["injected"]["translation"], json!([1.0, -1.0, 0.5]));
        assert_eq!(sat["injected"]["rotation"], json!([0.0, 0.0, 0.0]));
    }

    #[test]
    fn without_the_plugin_inject_and_status_report_the_stable_unsupported_code() {
        let mut world = World::new();
        for err in [
            status(&mut world).expect_err("status"),
            inject(&mut world, &json!({})).expect_err("inject"),
        ] {
            assert_eq!(err.code, UNSUPPORTED);
            assert_eq!(err.data.as_ref().and_then(|d| d["code"].as_str()), Some("F0013"));
        }
        // Parameter errors win over the missing plugin, so the caller learns about both in order.
        assert_eq!(
            inject(&mut world, &json!({ "frames": 0 }))
                .expect_err("frames")
                .code,
            crate::protocol::INVALID_PARAMS
        );
    }
}
