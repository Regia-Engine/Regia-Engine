// SPDX-License-Identifier: MPL-2.0
//! `render.set_light_profile` (ADR-2592, `book/agents/light-profiles.md`): adds an IES LM-63 profile to the
//! scene's `LightProfileLibrary` and optionally puts it on a point or spot light. The rect and tube lights
//! (`RectLight`, `TubeLight`) and the `LightProfile` fields are edited through the generic entity methods
//! (`scene.add_component`, `editor.set_property`).

use regia_app::prelude::*;
use regia_render::{LightProfile, LightProfileLibrary, PointLight, SpotLight};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use crate::scene::entity_from;

/// `render.set_light_profile { name, ies?, path?, entity?, file_intensity?, scale?, rotation? }`.
///
/// With `ies` (LM-63 text) or `path` (an `.ies` file) the profile is parsed, cooked and stored under `name`; without
/// either, `name` must already be in the library. With `entity` (a point or spot light) the entity takes a
/// `LightProfile` naming it. Returns the profile's peak and flux, the library's names and the entity's profile.
pub(crate) fn set_light_profile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad_name = || RpcError::invalid_params("`name` must be a non-empty string");
    let name = params.get("name").and_then(Value::as_str).ok_or_else(bad_name)?;
    if name.is_empty() {
        return Err(bad_name());
    }
    let text = match (params.get("ies"), params.get("path")) {
        (Some(_), Some(_)) => {
            return Err(RpcError::invalid_params("give `ies` or `path`, not both"));
        }
        (Some(ies), None) => Some(
            ies.as_str()
                .ok_or_else(|| RpcError::invalid_params("`ies` must be LM-63 text"))?
                .to_string(),
        ),
        (None, Some(path)) => {
            let path = path
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`path` must be a string"))?;
            // Project-relative only: no absolute path, no `..`, no symlink out of the project.
            let file = crate::audio::project_file(world, path)?;
            Some(std::fs::read_to_string(&file).map_err(|e| {
                RpcError::not_found(
                    format!("cannot read {path}: {e}"),
                    "pass a readable .ies file, or the LM-63 text as `ies`",
                )
            })?)
        }
        (None, None) => None,
    };
    let mut library = world.get_resource_or_insert_with(LightProfileLibrary::default);
    if let Some(text) = text {
        library
            .insert_ies_text(name, &text)
            .map_err(|e| RpcError::invalid_params(format!("{name}: {e}")))?;
    }
    let Some(profile) = library.get(name).cloned() else {
        return Err(RpcError::not_found(
            format!("no light profile `{name}`"),
            "pass its LM-63 text as `ies` or a file as `path`",
        ));
    };
    let names = library.names();
    let mut assigned = Value::Null;
    if params.get("entity").is_some() {
        let entity = entity_from(params)?;
        if world.get_entity(entity).is_err() {
            return Err(RpcError::not_found(
                format!("no entity {}", entity.to_bits()),
                "pick a point or spot light from scene.query_entities",
            ));
        }
        if world.get::<PointLight>(entity).is_none() && world.get::<SpotLight>(entity).is_none() {
            return Err(RpcError::not_found(
                format!("entity {} has no PointLight or SpotLight", entity.to_bits()),
                "a profile goes on a point or spot light",
            ));
        }
        let mut component = world.get::<LightProfile>(entity).cloned().unwrap_or_default();
        component.profile = name.to_string();
        if let Some(v) = params.get("file_intensity") {
            component.file_intensity = v
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params("`file_intensity` must be true or false"))?;
        }
        for (key, slot) in [
            ("scale", &mut component.scale),
            ("rotation", &mut component.rotation),
        ] {
            if let Some(v) = params.get(key) {
                let v = v
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a finite number")))?;
                *slot = v as f32;
            }
        }
        assigned = json!({
            "entity": entity.to_bits(),
            "profile": component.profile,
            "file_intensity": component.file_intensity,
            "scale": component.scale,
            "rotation": component.rotation,
        });
        world.entity_mut(entity).insert(component);
    }
    Ok(json!({
        "name": name,
        "peak_candela": profile.peak_candela,
        "flux_lm": profile.flux_lm,
        "vertical_range": profile.vertical_range,
        "profiles": names,
        "assigned": assigned,
        "note": "applies from the next frame on the forward lights of every tier, Mobile included (ADR-3247); the terrain pass and fog ignore profiles",
    }))
}

fn no_settings() -> RpcError {
    RpcError::not_found(
        "no render settings: this app has no RenderSettings resource",
        "add regia_render::RenderPlugin (a host that builds FrameScene itself appends \
         regia_render::light_profiles::area_light_fallback's lights to its local lights)",
    )
}

fn fallback_state(world: &World, settings: &regia_render::RenderSettings) -> Value {
    let budget = settings.quality().emissive_lights;
    let frame = world.get_resource::<regia_render::runtime::ExtractedFrame>();
    json!({
        "enabled": settings.profile.area_light_fallback,
        "emitter_budget": budget,
        "active": settings.profile.area_light_fallback && budget == 0,
        "stand_in_lights": frame.map_or(0, |f| f.light_extras.area_fallback_lights),
        "area_lights": frame.map_or(0, |f| f.light_extras.area_lights.len()),
        "preset": settings.preset().map(|p| p.name()),
        "applies_to": "rect and tube lights on a tier with no emitter budget (DesktopLow, MobileLow, MobileHigh): \
                       each rect face stands in as a cosine spot, each tube as three radial cosine spots, with the \
                       equal-area source size, from the next extracted frame; a tier with an emitter budget keeps the LTC emitters",
    })
}

/// `render.area_light_fallback`: `{ enabled, emitter_budget, active, stand_in_lights, area_lights, preset,
/// applies_to }` (ADR-3247). `stand_in_lights` is the last extracted frame's cost counter.
pub(crate) fn area_light_fallback(world: &mut World) -> Result<Value, RpcError> {
    let settings = world
        .get_resource::<regia_render::RenderSettings>()
        .ok_or_else(no_settings)?;
    Ok(fallback_state(world, settings))
}

/// `render.set_area_light_fallback { enabled }`: sets the render profile's `area_light_fallback` and answers the new
/// state. The preset is still reported (`RenderSettings::preset` masks the field).
pub(crate) fn set_area_light_fallback(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
        RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "area_light_fallback_bad_enabled",
            "`enabled` must be true or false",
            "use { \"enabled\": true } for the rect and tube stand-ins (ADR-3247) or false for the shipped look",
        )
    })?;
    world
        .get_resource_mut::<regia_render::RenderSettings>()
        .ok_or_else(no_settings)?
        .profile
        .area_light_fallback = enabled;
    let settings = world
        .get_resource::<regia_render::RenderSettings>()
        .ok_or_else(no_settings)?;
    Ok(fallback_state(world, settings))
}

fn sized_ltc_state(world: &World, settings: &regia_render::RenderSettings) -> Value {
    let frame = world.get_resource::<regia_render::runtime::ExtractedFrame>();
    json!({
        "enabled": settings.profile.sized_light_ltc,
        "marked_lights": frame.map_or(0, |f| f.light_extras.ltc_spheres.iter().filter(|m| **m).count()),
        "preset": settings.preset().map(|p| p.name()),
        "applies_to": "every point and spot light with a LightSourceSize, from the next extracted frame: its sphere \
                       shades through the LTC tables (as with a LightSourceLtc marker) on the forward pass of the \
                       Desktop tiers; standard materials only (clearcoat, sheen, anisotropic, skin and hair keep the \
                       representative point), and the Mobile lite and terrain passes keep it too",
    })
}

/// `render.sized_light_ltc`: `{ enabled, marked_lights, preset, applies_to }` (ADR-3247). `marked_lights` counts the
/// last extracted frame's lights shaded through the LTC tables (markers and the switch).
pub(crate) fn sized_light_ltc(world: &mut World) -> Result<Value, RpcError> {
    let settings = world
        .get_resource::<regia_render::RenderSettings>()
        .ok_or_else(no_settings)?;
    Ok(sized_ltc_state(world, settings))
}

/// `render.set_sized_light_ltc { enabled }`: sets the render profile's `sized_light_ltc` and answers the new state.
pub(crate) fn set_sized_light_ltc(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
        RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "sized_light_ltc_bad_enabled",
            "`enabled` must be true or false",
            "use { \"enabled\": true } to shade every sized light through the LTC tables (ADR-3247) or false for the \
             representative point",
        )
    })?;
    world
        .get_resource_mut::<regia_render::RenderSettings>()
        .ok_or_else(no_settings)?
        .profile
        .sized_light_ltc = enabled;
    let settings = world
        .get_resource::<regia_render::RenderSettings>()
        .ok_or_else(no_settings)?;
    Ok(sized_ltc_state(world, settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-3247: with `sized_light_ltc` on, the extraction marks every sized point and spot light for the LTC sphere
    /// and leaves an unsized one alone; off, only the marker does.
    #[test]
    fn the_sized_light_ltc_switch_marks_every_sized_light() {
        use regia_ecs::ecs::system::RunSystemOnce;
        use regia_render::{LightSourceSize, PointLight, RenderProfilePreset, RenderSettings, SpotLight};
        use regia_scene::prelude::{GlobalTransform, Transform};
        let mut world = World::new();
        world.init_resource::<regia_render::runtime::ExtractedFrame>();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let at = |x: f32| {
            let t = Transform::from_translation(regia_core::math::Vec3::new(x, 2.0, 0.0));
            (t, GlobalTransform(t.compute_affine()))
        };
        let (t, g) = at(0.0);
        world.spawn((t, g, PointLight::default(), LightSourceSize { radius_m: 0.2 }));
        let (t, g) = at(3.0);
        world.spawn((t, g, PointLight::default()));
        let (t, g) = at(6.0);
        world.spawn((t, g, SpotLight::default(), LightSourceSize { radius_m: 0.1 }));
        let marked = |world: &mut World| {
            world
                .run_system_once(regia_render::extract_frame)
                .expect("extract");
            let frame = world.resource::<regia_render::runtime::ExtractedFrame>();
            let mut out: Vec<(f32, bool)> = frame
                .local_lights
                .iter()
                .enumerate()
                .map(|(i, light)| {
                    let x = match light {
                        regia_render::LabLocalLight::Point(p) => p.position[0],
                        regia_render::LabLocalLight::Spot(s) => s.position[0],
                    };
                    (x, frame.light_extras.ltc_spheres.get(i).copied().unwrap_or(false))
                })
                .collect();
            out.sort_by(|a, b| a.0.total_cmp(&b.0));
            out
        };
        assert_eq!(marked(&mut world), vec![(0.0, false), (3.0, false), (6.0, false)]);
        assert_eq!(sized_light_ltc(&mut world).expect("read")["enabled"], false);
        let err = set_sized_light_ltc(&mut world, &json!({ "enabled": "yes" })).expect_err("bad");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str),
            Some("sized_light_ltc_bad_enabled")
        );
        let on = set_sized_light_ltc(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(on["preset"], "DesktopHigh", "{on}");
        assert_eq!(marked(&mut world), vec![(0.0, true), (3.0, false), (6.0, true)]);
        assert_eq!(sized_light_ltc(&mut world).expect("read")["marked_lights"], 2);
    }

    /// ADR-3247: the switch is off by default, sets the profile field and keeps the preset; on a tier with no
    /// emitter budget the next extraction appends a stand-in per rect face and per tube and counts them, and with
    /// the switch off (the shipped look) or on a tier with a budget it appends none.
    #[test]
    fn the_area_light_fallback_switch_drives_the_extraction() {
        use regia_ecs::ecs::system::RunSystemOnce;
        use regia_render::{RectLight, RenderProfilePreset, RenderSettings, TubeLight};
        use regia_scene::prelude::{GlobalTransform, Transform};
        let code = |err: &RpcError| {
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let mut world = World::new();
        assert!(area_light_fallback(&mut world).is_err(), "no RenderSettings");
        world.init_resource::<regia_render::runtime::ExtractedFrame>();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileHigh));
        let at = |x: f32| {
            let t = Transform::from_translation(regia_core::math::Vec3::new(x, 2.0, 0.0));
            (t, GlobalTransform(t.compute_affine()))
        };
        let (t, g) = at(0.0);
        world.spawn((
            t,
            g,
            RectLight {
                two_sided: true,
                ..RectLight::default()
            },
        ));
        let (t, g) = at(3.0);
        world.spawn((t, g, RectLight::default()));
        let (t, g) = at(6.0);
        world.spawn((t, g, TubeLight::default()));
        let extract = |world: &mut World| {
            world
                .run_system_once(regia_render::extract_frame)
                .expect("extract");
            let frame = world.resource::<regia_render::runtime::ExtractedFrame>();
            (frame.local_lights.len(), frame.light_extras.area_fallback_lights)
        };
        let off = area_light_fallback(&mut world).expect("read");
        assert_eq!(off["enabled"], false, "{off}");
        assert_eq!(off["emitter_budget"], 0, "{off}");
        assert_eq!(extract(&mut world), (0, 0), "off: the shipped look");
        for bad in [json!({}), json!({ "enabled": 1 })] {
            let err = set_area_light_fallback(&mut world, &bad).expect_err("bad");
            assert_eq!(
                code(&err).as_deref(),
                Some("area_light_fallback_bad_enabled"),
                "{bad}"
            );
        }
        let on = set_area_light_fallback(&mut world, &json!({ "enabled": true })).expect("set");
        assert_eq!(
            (on["enabled"].clone(), on["active"].clone()),
            (json!(true), json!(true)),
            "{on}"
        );
        assert_eq!(on["preset"], "MobileHigh", "{on}");
        // Two faces of the two-sided rect, one of the other, three radial lobes of the tube.
        assert_eq!(extract(&mut world), (6, 6));
        let read = area_light_fallback(&mut world).expect("read");
        assert_eq!(read["stand_in_lights"], 6, "{read}");
        assert_eq!(read["area_lights"], 3, "{read}");
        // A tier with an emitter budget keeps the LTC emitters: no stand-ins even with the switch on.
        world
            .resource_mut::<RenderSettings>()
            .apply_preset(RenderProfilePreset::DesktopUltra);
        world.resource_mut::<RenderSettings>().profile.area_light_fallback = true;
        assert_eq!(extract(&mut world), (0, 0));
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn a_profile_is_cooked_stored_and_assigned() -> Result<(), RpcError> {
        let mut world = World::new();
        let spot = world.spawn(SpotLight::default()).id();
        let plain = world.spawn_empty().id();
        let text = regia_asset::ies::example_text();
        let out = set_light_profile(&mut world, &json!({ "name": "dl60", "ies": text }))?;
        assert_eq!(out["peak_candela"], json!(1000.0));
        assert_eq!(out["profiles"], json!(["dl60"]));
        let out = set_light_profile(
            &mut world,
            &json!({ "name": "dl60", "entity": spot.to_bits(), "scale": 2.0, "file_intensity": true }),
        )?;
        assert_eq!(out["assigned"]["profile"], json!("dl60"));
        let component = world.get::<LightProfile>(spot).cloned().unwrap_or_default();
        assert_eq!(component.profile, "dl60");
        assert_eq!(component.scale, 2.0);
        assert!(component.file_intensity);
        // Errors: unknown name, a non-light entity, bad text, both sources, a bad number.
        let unknown = set_light_profile(&mut world, &json!({ "name": "nope" })).expect_err("unknown");
        assert_eq!(code(&unknown), "F0003");
        let not_light = set_light_profile(&mut world, &json!({ "name": "dl60", "entity": plain.to_bits() }))
            .expect_err("plain");
        assert_eq!(code(&not_light), "F0003");
        let bad =
            set_light_profile(&mut world, &json!({ "name": "x", "ies": "TILT=NONE\n1 2" })).expect_err("bad");
        assert_eq!(code(&bad), "F0002");
        let both = set_light_profile(&mut world, &json!({ "name": "x", "ies": "", "path": "a.ies" }))
            .expect_err("both");
        assert_eq!(code(&both), "F0002");
        // A path outside the project is refused before any read.
        let outside =
            set_light_profile(&mut world, &json!({ "name": "x", "path": "/etc/passwd" })).expect_err("abs");
        assert_eq!(code(&outside), "F0002");
        let up = set_light_profile(&mut world, &json!({ "name": "x", "path": "../a.ies" })).expect_err("up");
        assert_eq!(code(&up), "F0002");
        let nan = set_light_profile(
            &mut world,
            &json!({ "name": "dl60", "entity": spot.to_bits(), "scale": "big" }),
        )
        .expect_err("scale");
        assert_eq!(code(&nan), "F0002");
        Ok(())
    }
}
