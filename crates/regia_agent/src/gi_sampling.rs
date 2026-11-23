// SPDX-License-Identifier: MPL-2.0
//! How the baked GI lookup evaluates its probes' SH, on the agent protocol (ADR-1377,
//! `book/agents/gi-sampling.md`).
//!
//! `render.set_gi_sampling {zh3}` switches the baked volume's irradiance evaluation between the
//! hallucinated ZH3 term (`true`: directional contrast under a window or a lit patch, the same
//! `.fgiv`) and L1 (`false`, the default, de-ringed as `BakedGiSettings::deringing` says), for an
//! A/B of the same frame. `render.get_gi_mode` reports the setting in
//! `baked.lookup.zh3`, and `render.sample_baked_gi` takes `zh3` as a per-call override.

use regia_app::prelude::*;
use regia_render::RuntimeRenderer;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (it inserts RuntimeRenderer)",
    )
}

/// `render.set_gi_sampling`: `zh3` (required, a boolean). Returns the setting before and after
/// and whether a baked volume is installed.
pub(crate) fn set_gi_sampling(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let zh3 = params.get("zh3").and_then(Value::as_bool).ok_or_else(|| {
        RpcError::invalid_params(
            "`zh3` is required: true (the ZH3 evaluation of the baked SH) or false (L1, the \
             default)",
        )
    })?;
    let mut runtime = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(missing)?;
    let mut settings = runtime.0.baked_gi_settings();
    let before = settings.zh3;
    settings.zh3 = zh3;
    runtime.0.set_baked_gi_settings(settings);
    let status = runtime.0.baked_gi_status();
    Ok(json!({
        "before": { "zh3": before },
        "after": { "zh3": zh3 },
        "deringing": settings.deringing,
        "volume_installed": status.installed,
        "note": "applies from the next frame while a baked GI volume lights it; with zh3 false the \
                 L1 evaluation is de-ringed as baked.lookup.deringing says. The host's own \
                 FrameRenderer::set_baked_gi_settings calls replace it",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_zh3_off_and_on_and_refuses_bad_params() {
        let mut world = World::new();
        assert!(
            set_gi_sampling(&mut world, &json!({ "zh3": false })).is_err(),
            "no renderer"
        );
        world.insert_resource(RuntimeRenderer::default());
        assert!(
            !world.resource::<RuntimeRenderer>().0.baked_gi_settings().zh3,
            "off by default"
        );
        let set = set_gi_sampling(&mut world, &json!({ "zh3": true })).expect("on");
        assert_eq!(set["before"]["zh3"], false);
        assert_eq!(set["after"]["zh3"], true);
        assert_eq!(set["volume_installed"], false);
        let settings = world.resource::<RuntimeRenderer>().0.baked_gi_settings();
        assert!(settings.zh3);
        // Only the evaluation changed.
        assert_eq!(
            settings,
            regia_render::BakedGiSettings {
                zh3: true,
                ..regia_render::BakedGiSettings::default()
            }
        );
        let set = set_gi_sampling(&mut world, &json!({ "zh3": false })).expect("off");
        assert_eq!(set["before"]["zh3"], true);
        assert!(!world.resource::<RuntimeRenderer>().0.baked_gi_settings().zh3);
        for bad in [json!({}), json!({ "zh3": 1 }), json!({ "zh3": "yes" })] {
            let err = set_gi_sampling(&mut world, &bad).expect_err("bad params");
            assert!(
                err.message.contains("`zh3` is required"),
                "{bad}: {}",
                err.message
            );
        }
    }
}
