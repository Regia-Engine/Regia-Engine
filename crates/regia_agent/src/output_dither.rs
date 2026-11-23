// SPDX-License-Identifier: MPL-2.0
//! `render.get_output_dither` / `render.set_output_dither` (ADR-2486): whether the 8-bit display image is
//! dithered before it is stored, so smooth gradients (a clear sky at 4K) do not band. Off by default.
//! `render.get_star_flux` / `render.set_star_flux`: whether a star keeps its light at every output height, so the
//! star field does not fade at 4K. Off by default.

use regia_app::prelude::*;
use regia_render::RuntimeRenderer;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no runtime renderer: this app has no RuntimeRenderer",
        "add regia_render::RenderPlugin",
    )
}

fn state(world: &World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(missing)?;
    Ok(json!({ "enabled": renderer.0.output_dither(), "code_values": 1 }))
}

/// `render.get_output_dither`.
pub(crate) fn get_output_dither(world: &World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_output_dither { enabled }`.
pub(crate) fn set_output_dither(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`enabled` (a boolean) is required"))?;
    let before = state(world)?;
    world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(missing)?
        .0
        .set_output_dither(enabled);
    let after = state(world)?;
    Ok(json!({ "before": before, "after": after }))
}

fn star_state(world: &World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(missing)?;
    Ok(json!({ "enabled": renderer.0.star_flux(), "reference_height": 1080 }))
}

/// `render.get_star_flux`.
pub(crate) fn get_star_flux(world: &World) -> Result<Value, RpcError> {
    star_state(world)
}

/// `render.set_star_flux { enabled }`.
pub(crate) fn set_star_flux(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`enabled` (a boolean) is required"))?;
    let before = star_state(world)?;
    world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(missing)?
        .0
        .set_star_flux(enabled);
    let after = star_state(world)?;
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_world_without_a_renderer_answers_not_found() {
        let mut world = World::new();
        assert!(get_output_dither(&world).is_err());
        assert!(set_output_dither(&mut world, &json!({ "enabled": true })).is_err());
    }

    #[test]
    fn enabled_must_be_a_boolean() {
        let mut world = World::new();
        let error = set_output_dither(&mut world, &json!({ "enabled": "yes" })).expect_err("type");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        let error = set_output_dither(&mut world, &json!({})).expect_err("missing");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        let error = set_star_flux(&mut world, &json!({ "enabled": 1 })).expect_err("type");
        assert_eq!(error.code, crate::protocol::INVALID_PARAMS);
        assert!(get_star_flux(&world).is_err());
    }
}
