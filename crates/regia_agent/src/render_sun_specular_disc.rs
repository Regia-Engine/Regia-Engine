// SPDX-License-Identifier: MPL-2.0
//! The sun's specular as a disc on the agent protocol (`book/agents/sun-specular-disc.md`,
//! ADR-3190): `render.sun_specular_disc` reads the render profile's `sun_specular_disc` field and
//! the angular radius the runtime sun then carries; `render.set_sun_specular_disc { enabled }`
//! sets it. Off (the point sun) in every preset, every shipped frame and every golden.

use regia_app::prelude::*;
use regia_render::RenderSettings;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn missing() -> RpcError {
    RpcError::not_found(
        "no render settings: this app has no RenderSettings resource",
        "add regia_render::RenderPlugin (a host that builds FrameScene itself uses \
         DirectionalLightUniform::with_specular_disc)",
    )
}

fn state(settings: &RenderSettings) -> Value {
    let enabled = settings.profile.sun_specular_disc;
    let radius = if enabled {
        regia_render::shadow::SUN_PCSS_ANGULAR_RADIUS_TAN
    } else {
        0.0
    };
    json!({
        "enabled": enabled,
        "angular_radius": radius,
        "preset": settings.preset().map(|p| p.name()),
        "applies_to": "the runtime sun from the next extracted frame: each isotropic GGX lobe of the \
                       forward pass and the visibility-buffer resolve takes its mean over the sun's disc; \
                       anisotropic and hair lobes and the lite (Mobile) shader keep the point sun",
    })
}

/// `render.sun_specular_disc`: `{ enabled, angular_radius, preset, applies_to }`.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    Ok(state(settings))
}

/// `render.set_sun_specular_disc { enabled }`: sets the render profile's field and answers the new
/// state. The preset is still reported (`RenderSettings::preset` masks the field).
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "sun_specular_disc_bad_enabled",
            "`enabled` must be true or false",
            "use { \"enabled\": true } for the disc sun (ADR-3190) or false for the shipped point sun",
        )
    })?;
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    settings.profile.sun_specular_disc = enabled;
    Ok(state(&settings))
}

#[cfg(test)]
mod tests {
    use regia_render::RenderProfilePreset;

    use super::*;

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn reads_sets_and_keeps_the_preset() {
        let mut world = World::new();
        assert!(get(&mut world).is_err(), "no RenderSettings");
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let off = get(&mut world).expect("read");
        assert_eq!(off["enabled"], false, "{off}");
        assert_eq!(off["angular_radius"], 0.0, "{off}");
        let on = set(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(on["enabled"], true, "{on}");
        assert!(
            on["angular_radius"]
                .as_f64()
                .is_some_and(|r| r > 0.004 && r < 0.005),
            "{on}"
        );
        assert_eq!(on["preset"], off["preset"], "opting in keeps the preset");
        assert!(world.resource::<RenderSettings>().profile.sun_specular_disc);
        for bad in [json!({}), json!({ "enabled": "yes" }), json!({ "enabled": 1 })] {
            let err = set(&mut world, &bad).expect_err("bad enabled");
            assert_eq!(code(&err), Some("sun_specular_disc_bad_enabled"), "{bad}");
        }
        assert!(
            world.resource::<RenderSettings>().profile.sun_specular_disc,
            "an error changes nothing"
        );
        let back = set(&mut world, &json!({ "enabled": false })).expect("set off");
        assert_eq!(back["enabled"], false, "{back}");
    }
}
