// SPDX-License-Identifier: MPL-2.0
//! `terrain.get_ground_look` / `terrain.set_ground_look`: ground variation and late-winter
//! surfaces of the photoreal terrain (ADR-1408, book `agents/terrain-late-winter.md`, guide
//! topic `ground_look`).
//!
//! The host inserts [`TerrainGroundLookControl`] with its terrain draw's handle; the draw applies
//! a new look at its next prepare. Without the resource both methods answer `F0003`.

use regia_ecs::prelude::*;
use regia_terrain::{FillCanyonScope, TerrainGroundLook, TerrainGroundLookControl};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no terrain ground look control in this app",
        "the host inserts TerrainGroundLookControl(draw.ground_look_handle()) with its terrain draw",
    )
}

fn look_json(look: &TerrainGroundLook) -> Value {
    json!({
        "on": look.is_on(),
        "variation": look.variation,
        "variation_scale_m": look.variation_scale_m,
        "remix": look.remix,
        "remix_scale": look.remix_scale,
        "snow": look.snow,
        "snow_line_m": look.snow_line_m,
        "snow_aspect": look.snow_aspect,
        "melt_edge": look.melt_edge,
        "snow_patch_m": look.snow_patch_m,
        "mud_wetness": look.mud_wetness,
        "hoarfrost": look.hoarfrost,
    })
}

/// `terrain.get_ground_look`: the look the terrain draw uses, its generation and the presets.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let control = world
        .get_resource::<TerrainGroundLookControl>()
        .ok_or_else(missing)?;
    let (generation, look) = control.0.get();
    let layer_albedo: Vec<Value> = control
        .0
        .layer_albedo()
        .iter()
        .map(|l| {
            json!({
                "name": l.name,
                "tint": l.tint,
                "texture_mean": l.texture_mean,
                "effective": l.effective,
                "warning": l.warning,
            })
        })
        .collect();
    Ok(json!({
        "look": look_json(&look),
        "generation": generation,
        "presets": TerrainGroundLook::PRESETS,
        "layer_albedo": layer_albedo,
        "fill_canyon_scope": control.0.fill_canyon_scope().name(),
        "fill_canyon_scopes": FillCanyonScope::NAMES,
    }))
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

/// `terrain.set_ground_look`: `preset` (off, varied, late_winter) and/or single fields over the
/// look in effect; values are clamped to their ranges. Applied at the draw's next prepare.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let handle = world
        .get_resource::<TerrainGroundLookControl>()
        .ok_or_else(missing)?
        .0
        .clone();
    let (_, before) = handle.get();
    let scope = match params.get("fill_canyon_scope") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(FillCanyonScope::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown fill_canyon_scope `{name}`; one of {}",
                FillCanyonScope::NAMES.join(", ")
            ))
        })?),
        Some(_) => return Err(RpcError::invalid_params("`fill_canyon_scope` must be a string")),
    };
    let mut after = match params.get("preset") {
        None | Some(Value::Null) => before,
        Some(Value::String(name)) => TerrainGroundLook::preset(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown ground look preset `{name}`; one of {}",
                TerrainGroundLook::PRESETS.join(", ")
            ))
        })?,
        Some(_) => return Err(RpcError::invalid_params("`preset` must be a string")),
    };
    let fields: [(&str, &mut f32); 11] = [
        ("variation", &mut after.variation),
        ("variation_scale_m", &mut after.variation_scale_m),
        ("remix", &mut after.remix),
        ("remix_scale", &mut after.remix_scale),
        ("snow", &mut after.snow),
        ("snow_line_m", &mut after.snow_line_m),
        ("snow_aspect", &mut after.snow_aspect),
        ("melt_edge", &mut after.melt_edge),
        ("snow_patch_m", &mut after.snow_patch_m),
        ("mud_wetness", &mut after.mud_wetness),
        ("hoarfrost", &mut after.hoarfrost),
    ];
    let mut values = Vec::with_capacity(fields.len());
    for (name, _) in &fields {
        values.push(number(params, name)?);
    }
    for ((_, slot), value) in fields.into_iter().zip(values) {
        if let Some(v) = value {
            *slot = v;
        }
    }
    let after = after.sanitized();
    handle.set(after);
    if let Some(scope) = scope {
        handle.set_fill_canyon_scope(scope);
    }
    let (generation, _) = handle.get();
    Ok(json!({
        "before": look_json(&before),
        "after": look_json(&after),
        "generation": generation,
        "fill_canyon_scope": handle.fill_canyon_scope().name(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_the_control_both_answer_not_found() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no control");
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0003")));
        assert!(set(&mut world, &json!({ "preset": "late_winter" })).is_err());
    }

    #[test]
    fn set_applies_presets_and_fields_and_refuses_bad_values() {
        let mut world = World::new();
        let control = TerrainGroundLookControl::default();
        world.insert_resource(control.clone());
        let got = get(&mut world).expect("get");
        assert_eq!(got["look"]["on"], false);
        assert_eq!(got["generation"], 0);
        let out = set(
            &mut world,
            &json!({ "preset": "late_winter", "snow": 0.9, "remix": 4.0 }),
        )
        .expect("set");
        assert_eq!(out["after"]["on"], true);
        let (generation, look) = control.0.get();
        assert_eq!(generation, 1);
        assert!((look.snow - 0.9).abs() < 1e-6);
        assert_eq!(look.remix, 1.0, "clamped");
        assert_eq!(look.hoarfrost, TerrainGroundLook::LATE_WINTER.hoarfrost);
        assert!(set(&mut world, &json!({ "preset": "summer" })).is_err());
        assert!(set(&mut world, &json!({ "preset": 3 })).is_err());
        assert!(set(&mut world, &json!({ "snow": "deep" })).is_err());
        assert_eq!(control.0.get().0, 1, "a refused call changes nothing");
        set(&mut world, &json!({ "preset": "off" })).expect("off");
        assert!(!control.0.get().1.is_on());
    }

    #[test]
    fn the_fill_canyon_scope_is_reported_and_set() {
        let mut world = World::new();
        let control = TerrainGroundLookControl::default();
        world.insert_resource(control.clone());
        let got = get(&mut world).expect("get");
        assert_eq!(got["fill_canyon_scope"], "off", "off by default");
        assert_eq!(
            got["fill_canyon_scopes"],
            json!(["off", "map_wide", "near_occluders"])
        );
        let out = set(&mut world, &json!({ "fill_canyon_scope": "near_occluders" })).expect("set");
        assert_eq!(out["fill_canyon_scope"], "near_occluders");
        assert_eq!(control.0.fill_canyon_scope(), FillCanyonScope::NearOccluders);
        assert_eq!(
            get(&mut world).expect("get")["fill_canyon_scope"],
            "near_occluders"
        );
        // The look is untouched by a scope-only call; a refused scope changes nothing.
        assert!(!control.0.get().1.is_on());
        assert!(set(&mut world, &json!({ "fill_canyon_scope": "everywhere" })).is_err());
        assert!(set(&mut world, &json!({ "fill_canyon_scope": 2, "snow": 0.5 })).is_err());
        assert_eq!(control.0.fill_canyon_scope(), FillCanyonScope::NearOccluders);
        assert!(!control.0.get().1.is_on());
        // A host that set the scope on the draw shows it through the same handle.
        control.0.set_fill_canyon_scope(FillCanyonScope::MapWide);
        assert_eq!(get(&mut world).expect("get")["fill_canyon_scope"], "map_wide");
    }

    #[test]
    fn the_dispatcher_reports_and_sets_the_fill_canyon_scope() {
        let mut world = World::new();
        // A mutating call stamps its change record with the frame.
        world.insert_resource(regia_app::Time::default());
        let control = TerrainGroundLookControl::default();
        world.insert_resource(control.clone());
        let call = |w: &mut World, method: &str, params: Value| {
            crate::dispatch::call_in_process(w, method, params).expect("answered this frame")
        };
        let read = call(&mut world, "terrain.get_ground_look", json!({})).expect("get");
        assert_eq!(read["fill_canyon_scope"], "off", "{read}");
        let out = call(
            &mut world,
            "terrain.set_ground_look",
            json!({ "fill_canyon_scope": "near_occluders" }),
        )
        .expect("set");
        assert_eq!(out["fill_canyon_scope"], "near_occluders", "{out}");
        assert_eq!(control.0.fill_canyon_scope(), FillCanyonScope::NearOccluders);
        let refused = call(
            &mut world,
            "terrain.set_ground_look",
            json!({ "fill_canyon_scope": "all" }),
        );
        assert!(refused.is_err(), "an unknown scope is refused");
        let read = call(&mut world, "terrain.get_ground_look", json!({})).expect("get");
        assert_eq!(read["fill_canyon_scope"], "near_occluders", "{read}");
    }
}
