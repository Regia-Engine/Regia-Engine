// SPDX-License-Identifier: MPL-2.0
//! `render.get_contact_calibration` / `render.set_contact_calibration` (ADR-3202,
//! `book/agents/contact-calibration.md`): the two contact-scale opt-ins of the active
//! [`RenderSettings`] profile, the horizon AO kernel (`RenderProfile::horizon_ao`) and the sun
//! penumbra by the sun's angular size (`RenderProfile::angular_sun_penumbra`). Both are off in
//! every preset, and turning them on keeps the preset recognised (`RenderSettings::preset`).

use regia_app::prelude::*;
use regia_render::{AntiAliasing, RenderSettings, ShadowFilter, SsaoMode};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource in this app",
        "add regia_render's RenderPlugin (it inserts RenderSettings)",
    )
}

/// Both switches and whether the frame can use them: the horizon kernel replaces the GTAO rows
/// of the SSAO (a depth prepass, no MSAA), the angular penumbra rides on the PCSS-lite filter.
fn state(settings: &RenderSettings) -> Value {
    let p = &settings.profile;
    let ssao_runs = p.depth_prepass && !matches!(p.anti_aliasing, AntiAliasing::Msaa4x);
    json!({
        "horizon_ao": p.horizon_ao,
        "horizon_ao_active": p.horizon_ao && ssao_runs && matches!(p.ssao, SsaoMode::GtaoHalfRes | SsaoMode::GtaoFull),
        "angular_sun_penumbra": p.angular_sun_penumbra,
        "angular_sun_penumbra_active": p.angular_sun_penumbra && p.shadow_filter == ShadowFilter::PcssLite,
    })
}

/// The optional boolean `name`; any other field is refused.
fn params_of(params: &Value) -> Result<(Option<bool>, Option<bool>), RpcError> {
    let object = match params {
        Value::Object(object) => object,
        Value::Null => {
            return Err(RpcError::invalid_params(
                "pass `horizon_ao` or `angular_sun_penumbra` (boolean)",
            ));
        }
        _ => return Err(RpcError::invalid_params("params must be an object")),
    };
    if let Some(unknown) = object
        .keys()
        .find(|k| *k != "horizon_ao" && *k != "angular_sun_penumbra")
    {
        return Err(RpcError::invalid_params(format!(
            "unknown field `{unknown}`: the fields are horizon_ao and angular_sun_penumbra"
        )));
    }
    let flag = |name: &str| -> Result<Option<bool>, RpcError> {
        object
            .get(name)
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean")))
            })
            .transpose()
    };
    let (horizon, penumbra) = (flag("horizon_ao")?, flag("angular_sun_penumbra")?);
    if horizon.is_none() && penumbra.is_none() {
        return Err(RpcError::invalid_params(
            "pass `horizon_ao` or `angular_sun_penumbra` (boolean)",
        ));
    }
    Ok((horizon, penumbra))
}

/// `render.get_contact_calibration`.
pub(crate) fn get(world: &World) -> Result<Value, RpcError> {
    Ok(state(world.get_resource::<RenderSettings>().ok_or_else(missing)?))
}

/// `render.set_contact_calibration { horizon_ao?, angular_sun_penumbra? }`: at least one. Returns
/// the state before and after.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (horizon, penumbra) = params_of(params)?;
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    let before = state(&settings);
    if let Some(on) = horizon {
        settings.profile.horizon_ao = on;
    }
    if let Some(on) = penumbra {
        settings.profile.angular_sun_penumbra = on;
    }
    let after = state(&settings);
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use regia_render::RenderProfilePreset;

    fn world(preset: RenderProfilePreset) -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(preset));
        world
    }

    #[test]
    fn off_by_default_and_on_keeps_the_preset() {
        let mut world = world(RenderProfilePreset::DesktopHigh);
        let got = get(&world).unwrap();
        assert_eq!(got["horizon_ao"], false);
        assert_eq!(got["angular_sun_penumbra"], false);
        let out = set(
            &mut world,
            &json!({ "horizon_ao": true, "angular_sun_penumbra": true }),
        )
        .unwrap();
        assert_eq!(out["after"]["horizon_ao_active"], true);
        assert_eq!(out["after"]["angular_sun_penumbra_active"], true);
        let settings = world.resource::<RenderSettings>();
        assert_eq!(settings.preset(), Some(RenderProfilePreset::DesktopHigh));
        assert_eq!(
            regia_render::FrameQuality::from_profile(&settings.profile).shadow_filter_mode(),
            4
        );
        set(&mut world, &json!({ "horizon_ao": false })).unwrap();
        let got = get(&world).unwrap();
        assert_eq!(got["horizon_ao"], false);
        assert_eq!(got["angular_sun_penumbra"], true);
    }

    #[test]
    fn inactive_where_the_tier_has_no_gtao_or_pcss() {
        let mut world = world(RenderProfilePreset::MobileLow);
        let out = set(
            &mut world,
            &json!({ "horizon_ao": true, "angular_sun_penumbra": true }),
        )
        .unwrap();
        assert_eq!(out["after"]["horizon_ao"], true);
        assert_eq!(out["after"]["horizon_ao_active"], false);
        assert_eq!(out["after"]["angular_sun_penumbra_active"], false);
    }

    #[test]
    fn bad_params_are_refused_and_change_nothing() {
        let mut world = world(RenderProfilePreset::DesktopUltra);
        for bad in [
            json!({}),
            json!(null),
            json!([true]),
            json!({ "horizon_ao": 1 }),
            json!({ "angular_sun_penumbra": "on" }),
            json!({ "horizon_ao": true, "radius": 2 }),
        ] {
            let error = set(&mut world, &bad).expect_err("refused");
            assert_eq!(error.code, crate::protocol::INVALID_PARAMS, "{bad}");
        }
        assert_eq!(get(&world).unwrap()["horizon_ao"], false);
        let mut empty = World::new();
        assert!(get(&empty).is_err());
        assert!(set(&mut empty, &json!({ "horizon_ao": true })).is_err());
    }
}
