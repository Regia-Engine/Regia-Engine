// SPDX-License-Identifier: MPL-2.0
//! The material clock on the agent protocol (`book/agents/material-clock.md`):
//! `render.material_clock` reads what the runtime renderer writes into time-reading material
//! graphs (wind sway and other animated offsets): the clock now, one frame earlier, their
//! difference and the lap length ([`regia_render::material_clock()`], ADR-1238).

use regia_app::prelude::*;
use regia_render::{MATERIAL_CLOCK_PERIOD_SECS, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `render.material_clock`: `{ set, now, prev, delta, period_secs, gameplay_secs }`. `now`,
/// `prev` and `delta` are `null` until the clock system ran once; `gameplay_secs` is `null`
/// without a [`Time`] resource.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(|| {
        RpcError::not_found(
            "no RuntimeRenderer resource in this app",
            "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
             FrameRenderer::material_clock)",
        )
    })?;
    let clock = renderer.0.material_clock();
    let gameplay = world
        .get_resource::<Time>()
        .map(|time| time.elapsed().as_secs_f64());
    Ok(json!({
        "set": clock.is_some(),
        "now": clock.map(|(now, _)| now),
        "prev": clock.map(|(_, prev)| prev),
        "delta": clock.map(|(now, prev)| now - prev),
        "period_secs": MATERIAL_CLOCK_PERIOD_SECS,
        "gameplay_secs": gameplay,
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use regia_render::FrameRenderer;

    #[test]
    fn reads_the_runtime_renderers_clock() {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer(FrameRenderer::new()));
        let got = get(&mut world).expect("read");
        assert_eq!(got["set"], false);
        assert!(got["now"].is_null());
        assert_eq!(got["period_secs"], MATERIAL_CLOCK_PERIOD_SECS);
        assert!(got["gameplay_secs"].is_null());

        let mut time = Time::default();
        time.advance(Duration::from_millis(1500));
        world.insert_resource(time);
        let (now, prev) =
            regia_render::material_clock(Duration::from_millis(1500), Duration::from_millis(20));
        world
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_material_clock(now, prev);
        let got = get(&mut world).expect("read");
        assert_eq!(got["set"], true);
        assert!((got["now"].as_f64().expect("now") - 1.5).abs() < 1.0e-6);
        assert!((got["delta"].as_f64().expect("delta") - 0.02).abs() < 1.0e-5);
        assert!((got["gameplay_secs"].as_f64().expect("gameplay") - 1.5).abs() < 1.0e-9);
    }

    #[test]
    fn without_a_renderer_the_call_says_what_to_add() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no renderer");
        assert!(format!("{err:?}").contains("RuntimeRenderer"), "{err:?}");
    }
}
