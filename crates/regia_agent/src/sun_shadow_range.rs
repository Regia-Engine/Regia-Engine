// SPDX-License-Identifier: MPL-2.0
//! `render.get_sun_shadow_range` / `render.set_sun_shadow_range` (ADR-2486): cap how far the sun cascades reach.
//! A one-cascade tier (Mobile Low) off the lite path spreads its one shadow map over the whole shadow range, so the
//! trees of a forest cast no visible shadow; a cap of 60-150 m brings them back near the camera. Off by default.

use regia_app::prelude::*;
use regia_render::{FrameRenderer, RuntimeRenderer};
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
    Ok(json!({
        "range_m": renderer.0.sun_shadow_range(),
        "min_m": FrameRenderer::SUN_SHADOW_RANGE_MIN_M,
        "max_m": FrameRenderer::SUN_SHADOW_RANGE_MAX_M,
    }))
}

/// The `range_m` parameter: `null` clears the cap, a finite positive number sets it (clamped by the renderer).
fn range_param(params: &Value) -> Result<Option<f32>, RpcError> {
    match params.get("range_m") {
        Some(Value::Null) => Ok(None),
        Some(value) => {
            let range = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`range_m` must be a number of metres or null"))?;
            if !range.is_finite() || range <= 0.0 {
                return Err(RpcError::invalid_params(format!(
                    "`range_m` must be above 0 (got {range}); null clears the cap"
                )));
            }
            Ok(Some(range as f32))
        }
        None => Err(RpcError::invalid_params(
            "`range_m` (metres, or null to clear) is required",
        )),
    }
}

/// `render.get_sun_shadow_range`.
pub(crate) fn get(world: &World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_sun_shadow_range { range_m }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let range = range_param(params)?;
    let before = state(world)?;
    world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(missing)?
        .0
        .set_sun_shadow_range(range);
    let after = state(world)?;
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_world_without_a_renderer_answers_not_found() {
        let mut world = World::new();
        assert!(get(&world).is_err());
        let error = set(&mut world, &json!({ "range_m": 80.0 })).expect_err("no renderer");
        assert_ne!(error.code, crate::protocol::INVALID_PARAMS);
    }

    #[test]
    fn range_must_be_a_positive_number_or_null() {
        assert_eq!(range_param(&json!({ "range_m": null })).unwrap(), None);
        assert_eq!(range_param(&json!({ "range_m": 80 })).unwrap(), Some(80.0));
        for bad in [
            json!({}),
            json!({ "range_m": "far" }),
            json!({ "range_m": 0 }),
            json!({ "range_m": -5.0 }),
        ] {
            let error = range_param(&bad).expect_err("refused");
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn the_renderer_clamps_and_clears_the_range() {
        let mut renderer = FrameRenderer::default();
        assert_eq!(renderer.sun_shadow_range(), None, "off by default");
        renderer.set_sun_shadow_range(Some(80.0));
        assert_eq!(renderer.sun_shadow_range(), Some(80.0));
        renderer.set_sun_shadow_range(Some(1.0));
        assert_eq!(
            renderer.sun_shadow_range(),
            Some(FrameRenderer::SUN_SHADOW_RANGE_MIN_M)
        );
        renderer.set_sun_shadow_range(Some(1.0e9));
        assert_eq!(
            renderer.sun_shadow_range(),
            Some(FrameRenderer::SUN_SHADOW_RANGE_MAX_M)
        );
        renderer.set_sun_shadow_range(Some(f32::NAN));
        assert_eq!(renderer.sun_shadow_range(), None);
        renderer.set_sun_shadow_range(Some(80.0));
        renderer.set_sun_shadow_range(None);
        assert_eq!(renderer.sun_shadow_range(), None);
    }
}
