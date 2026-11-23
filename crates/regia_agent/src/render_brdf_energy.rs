// SPDX-License-Identifier: MPL-2.0
//! The direct BRDF's energy model on the agent protocol (`book/agents/brdf-energy.md`,
//! ADR-2632): `render.brdf_energy` reads the process-wide model the forward modules compose
//! with, the `REGIA_BRDF_ENERGY` value and the render profile's field; `render.set_brdf_energy`
//! sets the model (`legacy`, the default and every shipped frame; `compensated`, Kulla & Conty's
//! energy-conserving form).

use regia_app::prelude::*;
use regia_render::{BRDF_ENERGY_ENV, BrdfEnergy, RenderSettings};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn state(world: &World) -> Value {
    let settings = world.get_resource::<RenderSettings>();
    json!({
        "mode": regia_render::process_brdf_energy().name(),
        "env": regia_core::brand::env_var(BRDF_ENERGY_ENV).ok(),
        "profile": settings.map(|s| s.profile.brdf_energy.name()),
        "preset": settings.and_then(RenderSettings::preset).map(|p| p.name()),
        "applies_to": "forward modules composed after the change (a renderer's built pipelines keep \
                       their model until rebuilt) and path-tracer scenes whose brdf_energy is set",
    })
}

/// `render.brdf_energy`: `{ mode, env, profile, preset, applies_to }`. Never fails: the model is
/// process-wide, and `profile`/`preset` are `null` without a `RenderSettings` resource.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    Ok(state(world))
}

/// `render.set_brdf_energy { mode }`: sets the process-wide model and the render profile's field
/// (when the app has one), and answers the new state.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params.get("mode").and_then(Value::as_str).unwrap_or_default();
    let mode = BrdfEnergy::from_name(name).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "brdf_energy_bad_mode",
            format!("unknown BRDF energy model `{name}`"),
            "use \"legacy\" (the shipped BRDF) or \"compensated\" (energy-conserving, ADR-2632)",
        )
    })?;
    regia_render::set_process_brdf_energy(mode);
    if let Some(mut settings) = world.get_resource_mut::<RenderSettings>() {
        settings.profile.brdf_energy = mode;
    }
    Ok(state(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    // Setting `compensated` changes a process-wide value other tests' forward modules compose
    // with, so it is exercised in its own process (`tests/brdf_energy.rs`); here only the read,
    // `legacy` (the value every other test expects) and the errors.

    #[test]
    fn reads_the_model_and_legacy_keeps_the_preset() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(
            regia_render::RenderProfilePreset::DesktopHigh,
        ));
        let got = get(&mut world).expect("read");
        assert_eq!(got["profile"], "legacy");
        assert_eq!(
            got["preset"],
            regia_render::RenderProfilePreset::DesktopHigh.name()
        );
        let got = set(&mut world, &json!({ "mode": " Legacy " })).expect("legacy");
        assert_eq!(got["profile"], "legacy");
        let mut bare = World::new();
        let got = get(&mut bare).expect("read without settings");
        assert!(got["profile"].is_null() && got["preset"].is_null(), "{got}");
    }

    #[test]
    fn errors_carry_their_codes() {
        let mut world = World::new();
        for params in [json!({ "mode": "kulla" }), json!({}), json!({ "mode": 1 })] {
            let err = set(&mut world, &params).expect_err("bad mode");
            assert_eq!(code(&err), Some("brdf_energy_bad_mode"), "{params}");
        }
    }
}
