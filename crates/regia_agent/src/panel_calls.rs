// SPDX-License-Identifier: MPL-2.0
//! The agent handlers the editor's gameplay panels call on the play world (ADR-2030). The agent
//! server lives on the edit world, so `statetree.*`, `smartobject.*` and `camera.*` reach that world
//! when a client calls them; a panel debugging a running game routes the same handlers here instead,
//! so a button, a panel method and a direct call behave and fail alike.

use regia_app::prelude::*;
use serde_json::Value;

use crate::protocol::RpcError;

/// The methods [`call`] accepts.
pub const PANEL_METHODS: &[&str] = &[
    "statetree.control",
    "statetree.reload",
    "statetree.debug",
    "ai.bt_live",
    "smartobject.claim",
    "smartobject.release",
    "smartobject.set_enabled",
    "camera.inspect",
    "camera.set_var",
    "camera.set_param",
    "camera.set_input",
    "camera.reset_input",
    "camera.push_overlay",
    "camera.add_additive",
    "camera.release_layer",
    "camera.hold_overlay",
    "camera.set_driver",
    "camera.load",
    "physics.vehicles",
    "physics.vehicle_telemetry",
    "physics.vehicle_tire_curve",
];

/// Runs one of [`PANEL_METHODS`] on `world`.
///
/// # Errors
/// What the handler answers; `invalid_params` for a method not in [`PANEL_METHODS`].
pub fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
    use crate::{camera_framework as camera, smart_object, smart_object_v2, state_tree};
    match method {
        "statetree.control" => state_tree::control(world, params),
        "statetree.reload" => state_tree::reload(world, params),
        "statetree.debug" => state_tree::debug(world, params),
        "ai.bt_live" => crate::ai_bt::live(world, params),
        "smartobject.claim" => smart_object::claim(world, params),
        "smartobject.release" => smart_object::release(world, params),
        "smartobject.set_enabled" => smart_object_v2::set_enabled_method(world, params),
        "camera.inspect" => camera::camera_inspect(world, params),
        "camera.set_var" => camera::camera_set_var(world, params),
        "camera.set_param" => camera::camera_set_param(world, params),
        "camera.set_input" => camera::camera_set_input(world, params),
        "camera.reset_input" => camera::camera_reset_input(world, params),
        "camera.push_overlay" => camera::camera_push_overlay(world, params),
        "camera.add_additive" => camera::camera_add_additive(world, params),
        "camera.release_layer" => camera::camera_release_layer(world, params),
        "camera.hold_overlay" => camera::camera_hold_overlay(world, params),
        "camera.set_driver" => camera::camera_set_driver(world, params),
        "camera.load" => camera::camera_load(world, params),
        // The Vehicle Tuning panel reads the play car and plots the tire as these do (ADR-9760).
        "physics.vehicles" => crate::vehicles::vehicles(world, params),
        "physics.vehicle_telemetry" => crate::vehicles::vehicle_telemetry(world, params),
        "physics.vehicle_tire_curve" => crate::vehicles::vehicle_tire_curve(world, params),
        other => Err(RpcError::invalid_params(format!(
            "{other} is not a method the panels forward"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn every_listed_method_reaches_a_handler_and_others_are_refused() {
        let mut world = World::new();
        for method in PANEL_METHODS {
            let answer = call(&mut world, method, &json!({}));
            let not_routed = answer
                .as_ref()
                .err()
                .is_some_and(|e| e.message.contains("not a method the panels forward"));
            assert!(!not_routed, "{method}");
        }
        let refused = call(&mut world, "scene.despawn", &json!({})).expect_err("refused");
        assert_eq!(refused.code, crate::protocol::INVALID_PARAMS);
    }
}
