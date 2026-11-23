// SPDX-License-Identifier: MPL-2.0
//! Runtime lens effects on the agent protocol (`book/agents/lens-effects.md`, ADR-1863):
//!
//! - `render.lens_effects { entity?, light? }` reads a camera's lens effects: its `LensEffects`
//!   component, the strengths its view resolves to (project, camera, post volumes, preset gate),
//!   whether the preset allows the pass, the pass's counters in the runtime renderer, the traced
//!   ghost table of the built-in lens and, for a `light` position, where each ghost lands.
//! - `render.set_lens_effects { entity?, ... }` sets them (inserting the component), or removes
//!   the component with `remove: true`; every value is validated before anything changes.
//!
//! Error reasons (`data.reason`): `camera_missing`, `not_a_camera`, `unknown_field`,
//! `invalid_value`, `nothing_to_remove`.

use std::sync::atomic::Ordering;

use regia_app::prelude::*;
use regia_render::lens_effects::{
    CHROMATIC_ABERRATION_RANGE, DIRT_INTENSITY_RANGE, FLARE_INTENSITY_RANGE, FLARE_THRESHOLD_RANGE,
    LENS_EFFECTS_PASS_NAME, STARBURST_RANGE, default_ghost_table, lens_effects_counters,
};
use regia_render::post_volumes::{VolumeContribution, blend_post_volumes, gather_contributions};
use regia_render::{LensEffects, PostVolume, RenderSettings, RuntimeRenderer, gate_lens_effects};
use regia_scene::GlobalTransform;
use serde_json::{Map, Value, json};

use crate::post_volumes::{base_post, check_fields, error, lens_target, missing, object};
use crate::protocol::{self, RpcError};

/// The fields `render.set_lens_effects` accepts.
pub(crate) const SET_FIELDS: &[&str] = &[
    "entity",
    "flare_intensity",
    "flare_threshold",
    "coating",
    "dirt_intensity",
    "chromatic_aberration",
    "starburst",
    "after_dof",
    "remove",
];

fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(
        protocol::INVALID_PARAMS,
        "F0002",
        reason,
        message,
        "see book/agents/lens-effects.md",
    )
}

/// A finite number in `range` (`None` when absent or null).
fn ranged(map: &Map<String, Value>, key: &str, range: (f32, f32)) -> Result<Option<f32>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(v) if v.is_finite() && (f64::from(range.0)..=f64::from(range.1)).contains(&v) => {
                Ok(Some(v as f32))
            }
            _ => Err(invalid(
                "invalid_value",
                format!("`{key}` must be a number from {} to {}", range.0, range.1),
            )),
        },
    }
}

fn effects_json(effects: &LensEffects) -> Value {
    json!({
        "flare_intensity": effects.flare_intensity,
        "flare_threshold": effects.flare_threshold,
        "coating": effects.coating,
        "dirt_intensity": effects.dirt_intensity,
        "chromatic_aberration": effects.chromatic_aberration,
        "starburst": effects.starburst,
        "after_dof": effects.after_dof,
        "active": effects.is_active(),
        "flare": effects.flare_active(),
        "dirt": effects.dirt_active(),
        "aberration": effects.chromatic_aberration_active(),
        "starburst_active": effects.starburst_active(),
    })
}

/// The strengths the camera's view resolves to, as `extract_frame` builds them: the project's
/// (and the level look), the camera's component, the post volumes at its eye, then the gate.
fn resolved(world: &mut World, camera: Entity) -> (LensEffects, LensEffects) {
    let mut post = base_post(world);
    if let Some(effects) = world.get::<LensEffects>(camera) {
        effects.apply_camera(&mut post);
    }
    if let Some(eye) = world
        .get::<GlobalTransform>(camera)
        .map(|t| t.0.translation.to_array())
    {
        let mut query = world.query::<(Entity, &PostVolume, &GlobalTransform)>();
        let rows: Vec<(Entity, PostVolume, GlobalTransform)> =
            query.iter(world).map(|(e, v, t)| (e, v.clone(), *t)).collect();
        let mut contributions: Vec<VolumeContribution> = Vec::new();
        gather_contributions(rows.iter().map(|(e, v, t)| (*e, v, t)), eye, &mut contributions);
        blend_post_volumes(&mut post, &mut contributions);
    }
    let requested = post.lens_effects;
    if let Some(settings) = world.get_resource::<RenderSettings>() {
        gate_lens_effects(&mut post, &settings.profile);
    }
    (requested, post.lens_effects)
}

/// The pass's row in the runtime renderer's graph, `null` without a runtime renderer.
fn pass_json(world: &World) -> Value {
    let Some(renderer) = world.get_resource::<RuntimeRenderer>() else {
        return Value::Null;
    };
    let report = renderer.0.graph_report();
    match report
        .extensions
        .iter()
        .find(|p| p.name == LENS_EFFECTS_PASS_NAME)
    {
        Some(info) => json!({
            "registered": true,
            "runs": info.runs,
            "failures": info.failures,
            "last_frame": info.last_frame,
            "last_error": info.last_error,
            "last_cpu_ms": info.last_cpu_ms,
        }),
        None => json!({ "registered": false }),
    }
}

/// The fields `render.lens_effects` accepts.
pub(crate) const GET_FIELDS: &[&str] = &["entity", "light"];

/// `light`: a centred frame position `[x, y]` (x right, y down, -1..1 inside the frame; up to ±4
/// for lights beyond it), `None` when absent or null.
fn light_param(map: &Map<String, Value>) -> Result<Option<[f32; 2]>, RpcError> {
    match map.get("light") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(xy)) if xy.len() == 2 => {
            let coord = |v: &Value| v.as_f64().filter(|c| c.is_finite() && (-4.0..=4.0).contains(c));
            match (coord(&xy[0]), coord(&xy[1])) {
                (Some(x), Some(y)) => Ok(Some([x as f32, y as f32])),
                _ => Err(invalid("invalid_value", "`light` must be [x, y], each -4..4")),
            }
        }
        Some(_) => Err(invalid("invalid_value", "`light` must be [x, y], each -4..4")),
    }
}

/// `render.lens_effects { entity?, light? }`.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, GET_FIELDS)?;
    let light = light_param(map)?;
    let camera = lens_target(world, map)?;
    let component = world.get::<LensEffects>(camera).copied();
    let (requested, effective) = resolved(world, camera);
    let allowed = world
        .get_resource::<RenderSettings>()
        .is_none_or(|s| s.profile.lens_effects);
    let table = default_ghost_table(effective.coating).map_err(|err| {
        error(
            protocol::INTERNAL_ERROR,
            "F0001",
            "trace_failed",
            format!("lens trace: {err}"),
            "the built-in lens failed validation; report it",
        )
    })?;
    let ghosts: Vec<Value> = table
        .ghosts
        .iter()
        .map(|g| {
            json!({
                "first_bounce": g.first_bounce,
                "second_bounce": g.second_bounce,
                "energy": g.energy,
                "magnification": g.magnification,
                "radius": g.radius,
            })
        })
        .collect();
    // Where each ghost lands for a light at `light` (green channel; red and blue differ by the
    // ghost's dispersion), so a screenshot can be checked against it.
    let predicted: Option<Vec<Value>> = light.map(|c| {
        table
            .ghosts
            .iter()
            .map(|g| {
                let m = g.magnification[1];
                let at = [m * c[0], m * c[1]];
                json!({
                    "first_bounce": g.first_bounce,
                    "second_bounce": g.second_bounce,
                    "at": at,
                    "in_frame": at.iter().all(|v| v.abs() <= 1.0),
                    "radius": g.radius,
                })
            })
            .collect()
    });
    let counters = lens_effects_counters();
    Ok(json!({
        "entity": camera.to_bits(),
        "has_component": component.is_some(),
        "component": component.as_ref().map(effects_json),
        "requested": effects_json(&requested),
        "effective": effects_json(&effective),
        "preset_allows": allowed,
        "pass": pass_json(world),
        "counters": {
            "views_recorded": counters.views_recorded.load(Ordering::Relaxed),
            "views_idle": counters.views_idle.load(Ordering::Relaxed),
            "aberration_skipped": counters.aberration_skipped.load(Ordering::Relaxed),
            "last_ghosts": counters.last_ghosts.load(Ordering::Relaxed),
            "targets_built": counters.targets_built.load(Ordering::Relaxed),
            "tables_traced": counters.tables_traced.load(Ordering::Relaxed),
        },
        "lens": {
            "name": table.lens,
            "coating": table.coating,
            "ranked": table.ranked,
            "ghosts": ghosts,
        },
        "light": light,
        "predicted": predicted,
    }))
}

/// `render.set_lens_effects { entity?, flare_intensity?, flare_threshold?, coating?,
/// dirt_intensity?, chromatic_aberration?, starburst?, after_dof?, remove? }`: returns `{ entity, created, removed,
/// before, after }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, SET_FIELDS)?;
    let camera = lens_target(world, map)?;
    let existing = world.get::<LensEffects>(camera).copied();
    let remove = match map.get("remove") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("invalid_value", "`remove` must be true or false")),
    };
    if remove {
        if map.keys().any(|k| k != "remove" && k != "entity") {
            return Err(invalid(
                "invalid_value",
                "`remove: true` takes no strengths; remove, then set",
            ));
        }
        if existing.is_none() {
            return Err(missing(
                "nothing_to_remove",
                format!("camera {} has no LensEffects", camera.to_bits()),
                "the camera already uses the project's lens effects",
            ));
        }
        world.entity_mut(camera).remove::<LensEffects>();
        return Ok(json!({
            "entity": camera.to_bits(),
            "created": false,
            "removed": true,
            "before": existing.as_ref().map(effects_json),
            "after": Value::Null,
        }));
    }
    let flare = ranged(map, "flare_intensity", FLARE_INTENSITY_RANGE)?;
    let threshold = ranged(map, "flare_threshold", FLARE_THRESHOLD_RANGE)?;
    let coating = ranged(map, "coating", (0.0, 1.0))?;
    let dirt = ranged(map, "dirt_intensity", DIRT_INTENSITY_RANGE)?;
    let aberration = ranged(map, "chromatic_aberration", CHROMATIC_ABERRATION_RANGE)?;
    let starburst = ranged(map, "starburst", STARBURST_RANGE)?;
    let after_dof = match map.get("after_dof") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(invalid("invalid_value", "`after_dof` must be true or false")),
    };
    let mut effects = existing.unwrap_or_default();
    if let Some(v) = starburst {
        effects.starburst = v;
    }
    if let Some(v) = after_dof {
        effects.after_dof = v;
    }
    if let Some(v) = flare {
        effects.flare_intensity = v;
    }
    if let Some(v) = threshold {
        effects.flare_threshold = v;
    }
    if let Some(v) = coating {
        effects.coating = v;
    }
    if let Some(v) = dirt {
        effects.dirt_intensity = v;
    }
    if let Some(v) = aberration {
        effects.chromatic_aberration = v;
    }
    world.entity_mut(camera).insert(effects);
    Ok(json!({
        "entity": camera.to_bits(),
        "created": existing.is_none(),
        "removed": false,
        "before": existing.as_ref().map(effects_json),
        "after": effects_json(&effects),
    }))
}

#[cfg(test)]
mod tests {
    use regia_render::{Camera, RenderProfile, RenderProfilePreset};
    use regia_scene::Transform;

    use super::*;

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn world_with_camera(preset: RenderProfilePreset) -> (World, Entity) {
        let mut world = World::new();
        let settings = RenderSettings {
            profile: RenderProfile::from_preset(preset),
            ..RenderSettings::default()
        };
        world.insert_resource(settings);
        let camera = world
            .spawn((
                Camera::default(),
                GlobalTransform(Transform::default().compute_affine()),
            ))
            .id();
        (world, camera)
    }

    #[test]
    fn set_then_get_round_trips_and_the_preset_gates() -> Result<(), RpcError> {
        let (mut world, camera) = world_with_camera(RenderProfilePreset::DesktopHigh);
        let before = get(&mut world, &json!({}))?;
        assert_eq!(before["has_component"], false);
        assert_eq!(before["effective"]["active"], false);
        assert_eq!(before["preset_allows"], true);
        assert!(before["lens"]["ghosts"].as_array().is_some_and(|g| !g.is_empty()));
        assert!(before["predicted"].is_null());
        let predicted = get(&mut world, &json!({ "light": [0.5, -0.25] }))?;
        let rows = predicted["predicted"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            rows.len(),
            predicted["lens"]["ghosts"].as_array().map_or(0, Vec::len)
        );
        let m = predicted["lens"]["ghosts"][0]["magnification"][1]
            .as_f64()
            .unwrap_or(f64::NAN);
        let x = rows[0]["at"][0].as_f64().unwrap_or(f64::NAN);
        assert!((x - m * 0.5).abs() < 1e-5, "{x} vs {}", m * 0.5);
        let set_result = set(
            &mut world,
            &json!({ "flare_intensity": 2.0, "chromatic_aberration": 0.004 }),
        )?;
        assert_eq!(set_result["created"], true);
        assert_eq!(set_result["after"]["flare_intensity"], 2.0);
        assert_eq!(
            world.get::<LensEffects>(camera).map(|e| e.flare_intensity),
            Some(2.0)
        );
        let after = get(&mut world, &json!({ "entity": camera.to_bits() }))?;
        assert_eq!(after["effective"]["flare"], true);
        assert_eq!(after["effective"]["aberration"], true);
        assert_eq!(after["effective"]["dirt"], false);

        // A preset that does not allow the pass: requested stays, effective is off.
        world.insert_resource(RenderSettings {
            profile: RenderProfile::from_preset(RenderProfilePreset::MobileHigh),
            ..RenderSettings::default()
        });
        let gated = get(&mut world, &json!({}))?;
        assert_eq!(gated["preset_allows"], false);
        assert_eq!(gated["requested"]["active"], true);
        assert_eq!(gated["effective"]["active"], false);

        let removed = set(&mut world, &json!({ "remove": true }))?;
        assert_eq!(removed["removed"], true);
        assert!(world.get::<LensEffects>(camera).is_none());
        Ok(())
    }

    #[test]
    fn errors_carry_reasons() {
        let (mut world, camera) = world_with_camera(RenderProfilePreset::DesktopUltra);
        let cases = [
            (json!({ "bogus": 1 }), "unknown_field"),
            (json!({ "flare_intensity": -1.0 }), "invalid_value"),
            (json!({ "chromatic_aberration": 0.5 }), "invalid_value"),
            (json!({ "coating": "full" }), "invalid_value"),
            (json!({ "remove": "yes" }), "invalid_value"),
            (json!({ "remove": true }), "nothing_to_remove"),
        ];
        for bad in [
            json!({ "light": [0.0] }),
            json!({ "light": [9.0, 0.0] }),
            json!({ "light": "centre" }),
        ] {
            let err = get(&mut world, &bad).expect_err("bad light");
            assert_eq!(reason(&err), "invalid_value", "{bad}");
        }
        for (params, expected) in cases {
            let err = set(&mut world, &params).expect_err("refused");
            assert_eq!(reason(&err), expected, "{params}");
        }
        assert!(world.get::<LensEffects>(camera).is_none(), "nothing applied");
        let not_camera = world.spawn_empty().id();
        let err = get(&mut world, &json!({ "entity": not_camera.to_bits() })).expect_err("not a camera");
        assert_eq!(reason(&err), "not_a_camera");
        let mut empty = World::new();
        let err = get(&mut empty, &json!({})).expect_err("no camera");
        assert_eq!(reason(&err), "camera_missing");
    }

    #[test]
    fn book_and_guide_name_every_field_and_reason() {
        let book = include_str!("../../../book/agents/lens-effects.md");
        for name in SET_FIELDS.iter().chain(GET_FIELDS) {
            assert!(book.contains(&format!("`{name}`")), "book lacks `{name}`");
        }
        for reason in [
            "camera_missing",
            "not_a_camera",
            "unknown_field",
            "invalid_value",
            "nothing_to_remove",
            "trace_failed",
        ] {
            assert!(
                book.contains(&format!("`{reason}`")),
                "book lacks reason `{reason}`"
            );
        }
        let guide = crate::discovery::guide(&json!({ "topic": "lens-effects" }))
            .expect("lens-effects topic exists") // invariant: test-only (cfg(test))
            .to_string();
        for method in ["render.lens_effects", "render.set_lens_effects"] {
            assert!(guide.contains(method), "guide lacks {method}");
        }
    }
}
