// SPDX-License-Identifier: MPL-2.0
//! Renderer toggles on the agent protocol (`book/agents/contact-shadows.md`,
//! `book/agents/translucent-shadows.md`, `book/agents/reflection-occlusion.md`):
//! `render.set_contact_shadows` switches the screen-space contact shadows,
//! `render.set_translucent_shadows` the translucent (glass) shadow casters and
//! `render.set_reflection_occlusion` the reflection occlusion of the active [`RenderSettings`]
//! profile, `render.set_bent_normals` its bent normals (`book/agents/bent-normals.md`), and
//! `render.set_near_sun_shadow` its near sun tile (`book/agents/near-sun-shadow.md`).

use regia_app::prelude::*;
use regia_render::{AntiAliasing, RenderSettings, SsaoMode};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource in this app",
        "add regia_render's RenderPlugin (it inserts RenderSettings)",
    )
}

/// `enabled` plus whether the frame can run the pass: it needs the profile's depth prepass
/// and a single-sample frame (no MSAA).
fn state(settings: &RenderSettings) -> Value {
    let profile = &settings.profile;
    let enabled = profile.contact_shadows;
    let active = enabled && profile.depth_prepass && !matches!(profile.anti_aliasing, AntiAliasing::Msaa4x);
    json!({ "enabled": enabled, "active": active })
}

fn enabled_param(params: &Value) -> Result<bool, RpcError> {
    params
        .get("enabled")
        .ok_or_else(|| RpcError::invalid_params("pass `enabled` (boolean)"))?
        .as_bool()
        .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))
}

/// `render.set_contact_shadows {enabled}`: turns the contact shadows on or off. Returns the
/// state before and after (`enabled`, `active`).
pub fn set_contact_shadows(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = enabled_param(params)?;
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    let before = state(&settings);
    settings.profile.contact_shadows = enabled;
    let after = state(&settings);
    Ok(json!({ "before": before, "after": after }))
}

fn translucent_state(settings: &RenderSettings) -> Value {
    let profile = &settings.profile;
    json!({
        "cascades": profile.translucent_shadow_cascades,
        "local": profile.translucent_local_shadows,
        // Cascades the frame can use: no more than the sun has.
        "active_cascades": profile.translucent_shadow_cascades.min(profile.csm_cascade_count),
    })
}

/// `render.set_translucent_shadows {cascades?, local?}`: how many sun cascades carry a
/// translucency map (0–4, 0 = glass casts no sun shadow) and whether local-light shadows see
/// translucent casters. At least one parameter. Returns the state before and after
/// (`cascades`, `local`, `active_cascades`).
pub fn set_translucent_shadows(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let cascades = match params.get("cascades") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|&n| n <= u64::from(regia_render::CASCADE_COUNT))
                .ok_or_else(|| RpcError::invalid_params("`cascades` must be an integer 0–4"))?
                as u32,
        ),
    };
    let local = match params.get("local") {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`local` must be a boolean"))?,
        ),
    };
    if cascades.is_none() && local.is_none() {
        return Err(RpcError::invalid_params(
            "pass `cascades` (0–4) and/or `local` (boolean)",
        ));
    }
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    let before = translucent_state(&settings);
    if let Some(n) = cascades {
        settings.profile.translucent_shadow_cascades = n;
    }
    if let Some(on) = local {
        settings.profile.translucent_local_shadows = on;
    }
    let after = translucent_state(&settings);
    Ok(json!({ "before": before, "after": after }))
}

/// `enabled` plus whether the frame can run it: it rides on the SSAO pass's GTAO rows, which
/// need the depth prepass and a single-sample frame.
fn reflection_state(settings: &RenderSettings) -> Value {
    let profile = &settings.profile;
    let enabled = profile.reflection_occlusion;
    let active = enabled
        && matches!(profile.ssao, SsaoMode::GtaoHalfRes | SsaoMode::GtaoFull)
        && profile.depth_prepass
        && !matches!(profile.anti_aliasing, AntiAliasing::Msaa4x);
    json!({ "enabled": enabled, "active": active })
}

/// `render.set_reflection_occlusion {enabled}`: turns the reflection occlusion (enclosed
/// surfaces reflect the enclosure, not the sky) on or off. Returns the state before and after
/// (`enabled`, `active`).
pub fn set_reflection_occlusion(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = enabled_param(params)?;
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    let before = reflection_state(&settings);
    settings.profile.reflection_occlusion = enabled;
    let after = reflection_state(&settings);
    Ok(json!({ "before": before, "after": after }))
}

/// `enabled` plus whether the frame can run them: the bent normals come from the SSAO pass's
/// GTAO rows, which need the depth prepass and a single-sample frame.
fn bent_state(settings: &RenderSettings) -> Value {
    let profile = &settings.profile;
    let enabled = profile.bent_normals;
    let active = enabled
        && matches!(profile.ssao, SsaoMode::GtaoHalfRes | SsaoMode::GtaoFull)
        && profile.depth_prepass
        && !matches!(profile.anti_aliasing, AntiAliasing::Msaa4x);
    json!({ "enabled": enabled, "active": active })
}

/// `render.set_bent_normals {enabled}`: turns the bent normals (indirect light along the mean
/// unoccluded direction, specular occlusion from the visibility cone; ADR-1375) on or off.
/// Returns the state before and after (`enabled`, `active`).
pub fn set_bent_normals(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = enabled_param(params)?;
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    let before = bent_state(&settings);
    settings.profile.bent_normals = enabled;
    let after = bent_state(&settings);
    Ok(json!({ "before": before, "after": after }))
}

fn near_sun_state(settings: &RenderSettings) -> Value {
    let profile = &settings.profile;
    json!({
        "tile": profile.near_sun_tile,
        "range_m": regia_render::NEAR_SUN_RANGE_M,
        // The tile lives in the local shadow atlas and is sampled with the sun cascades.
        "active": profile.near_sun_tile > 0 && profile.csm_cascade_count > 0,
    })
}

/// `render.set_near_sun_shadow {tile}`: the edge (texels, 256–8192) of the near sun tile, the
/// high-resolution sun shadow over the first `range_m` of the view that mesh receivers there
/// sample instead of cascade 0 (ADR-164); 0 turns it off. Returns the state before and after
/// (`tile`, `range_m`, `active`).
pub fn set_near_sun_shadow(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tile = params
        .get("tile")
        .ok_or_else(|| RpcError::invalid_params("pass `tile` (0, or 256–8192 texels)"))?
        .as_u64()
        .filter(|&n| n == 0 || (256..=8192).contains(&n))
        .ok_or_else(|| RpcError::invalid_params("`tile` must be 0 or an integer 256–8192"))?
        as u32;
    let mut settings = world.get_resource_mut::<RenderSettings>().ok_or_else(missing)?;
    let before = near_sun_state(&settings);
    settings.profile.near_sun_tile = tile;
    let after = near_sun_state(&settings);
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderProfilePreset;

    #[test]
    fn translucent_shadows_round_trip() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let out =
            set_translucent_shadows(&mut world, &json!({ "cascades": 0, "local": false })).expect("off");
        assert_eq!(
            out["before"],
            json!({ "cascades": 4, "local": true, "active_cascades": 4 })
        );
        assert_eq!(
            out["after"],
            json!({ "cascades": 0, "local": false, "active_cascades": 0 })
        );
        let out = set_translucent_shadows(&mut world, &json!({ "cascades": 2 })).expect("two");
        assert_eq!(out["after"]["cascades"], json!(2));
        assert_eq!(out["after"]["local"], json!(false));
        let profile = &world.resource::<RenderSettings>().profile;
        assert_eq!(profile.translucent_shadow_cascades, 2);
        assert!(!profile.translucent_local_shadows);
    }

    #[test]
    fn translucent_cascades_clamp_to_the_sun_cascades() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileLow));
        let out = set_translucent_shadows(&mut world, &json!({ "cascades": 4 })).expect("on");
        assert_eq!(out["after"]["active_cascades"], json!(1));
    }

    #[test]
    fn translucent_bad_params_are_rejected() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        assert!(set_translucent_shadows(&mut world, &json!({})).is_err());
        assert!(set_translucent_shadows(&mut world, &json!({ "cascades": 5 })).is_err());
        assert!(set_translucent_shadows(&mut world, &json!({ "cascades": -1 })).is_err());
        assert!(set_translucent_shadows(&mut world, &json!({ "local": 1 })).is_err());
        assert!(set_translucent_shadows(&mut World::new(), &json!({ "local": true })).is_err());
    }

    #[test]
    fn toggles_round_trip() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let out = set_contact_shadows(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(out["before"], json!({ "enabled": true, "active": true }));
        assert_eq!(out["after"], json!({ "enabled": false, "active": false }));
        assert!(!world.resource::<RenderSettings>().profile.contact_shadows);
        let out = set_contact_shadows(&mut world, &json!({ "enabled": true })).expect("on");
        assert_eq!(out["after"]["enabled"], json!(true));
    }

    #[test]
    fn inactive_without_a_depth_prepass() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileLow));
        let out = set_contact_shadows(&mut world, &json!({ "enabled": true })).expect("on");
        assert_eq!(out["after"], json!({ "enabled": true, "active": false }));
    }

    #[test]
    fn bad_params_are_rejected() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        assert!(set_contact_shadows(&mut world, &json!({})).is_err());
        assert!(set_contact_shadows(&mut world, &json!({ "enabled": "yes" })).is_err());
        assert!(set_contact_shadows(&mut World::new(), &json!({ "enabled": true })).is_err());
    }

    #[test]
    fn reflection_occlusion_round_trips_and_needs_gtao() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let out = set_reflection_occlusion(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(out["before"], json!({ "enabled": true, "active": true }));
        assert_eq!(out["after"], json!({ "enabled": false, "active": false }));
        assert!(!world.resource::<RenderSettings>().profile.reflection_occlusion);
        // Deck's half-res AO row has no reflection visibility: enabled but not active.
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::Deck));
        let out = set_reflection_occlusion(&mut world, &json!({ "enabled": true })).expect("on");
        assert_eq!(out["after"], json!({ "enabled": true, "active": false }));
        assert!(set_reflection_occlusion(&mut world, &json!({ "enabled": 1 })).is_err());
        assert!(set_reflection_occlusion(&mut World::new(), &json!({ "enabled": true })).is_err());
    }

    #[test]
    fn bent_normals_round_trip_and_need_gtao() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let out = set_bent_normals(&mut world, &json!({ "enabled": false })).expect("off");
        assert_eq!(out["before"], json!({ "enabled": true, "active": true }));
        assert_eq!(out["after"], json!({ "enabled": false, "active": false }));
        assert!(!world.resource::<RenderSettings>().profile.bent_normals);
        let out = set_bent_normals(&mut world, &json!({ "enabled": true })).expect("on");
        assert_eq!(out["after"], json!({ "enabled": true, "active": true }));
        // Off by default on the Deck, and its half-res AO row has no bent normals: enabled but
        // not active.
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::Deck));
        let out = set_bent_normals(&mut world, &json!({ "enabled": true })).expect("on");
        assert_eq!(out["before"], json!({ "enabled": false, "active": false }));
        assert_eq!(out["after"], json!({ "enabled": true, "active": false }));
        assert!(set_bent_normals(&mut world, &json!({ "enabled": "yes" })).is_err());
        assert!(set_bent_normals(&mut world, &json!({})).is_err());
        assert!(set_bent_normals(&mut World::new(), &json!({ "enabled": true })).is_err());
    }

    #[test]
    fn near_sun_shadow_round_trip() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let out = set_near_sun_shadow(&mut world, &json!({ "tile": 0 })).expect("off");
        assert_eq!(out["before"]["tile"], json!(2048));
        assert_eq!(out["before"]["active"], json!(true));
        assert_eq!(out["after"]["tile"], json!(0));
        assert_eq!(out["after"]["active"], json!(false));
        assert!(set_near_sun_shadow(&mut world, &json!({ "tile": 100 })).is_err());
        assert!(set_near_sun_shadow(&mut world, &json!({})).is_err());
    }
}
