// SPDX-License-Identifier: MPL-2.0
//! `render.color_vision`: read or change the color-vision filter (simulation for designers,
//! compensation for players; book `agents/color-vision.md`, ADR-2646). A change writes
//! `GameSettings.accessibility.color_vision` (persisted like `settings.set`) and the live
//! `ColorVision` resource at once; the next frame's views carry the composed LUT.

use regia_ecs::prelude::*;
use regia_framework::settings::GameSettings;
use regia_render::color_vision::{ColorVision, ColorVisionDeficiency, ColorVisionFilter, ColorVisionMode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Params {
    mode: Option<String>,
    deficiency: Option<String>,
    severity: Option<f32>,
}

fn bad(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    bad(
        "color_vision_bad_params",
        format!("bad `render.color_vision` parameters: {detail}"),
        "pass { mode?, deficiency?, severity? }",
    )
}

fn snapshot(world: &World) -> Value {
    let live = world.get_resource::<ColorVision>();
    let saved = world
        .get_resource::<GameSettings>()
        .map(|s| s.accessibility.color_vision);
    let filter = live.map(|v| v.filter).or(saved).unwrap_or_default();
    let stats = live.map(ColorVision::stats).unwrap_or_default();
    json!({
        "mode": filter.mode.as_str(),
        "deficiency": filter.deficiency.as_str(),
        "severity": filter.severity(),
        "active": !filter.is_off(),
        "views_last_frame": stats.views_last_frame,
        "composed": stats.composed,
        "cache_hits": stats.cache_hits,
        "live": live.is_some(),
        "persisted": saved.is_some(),
    })
}

/// `render.color_vision { mode?, deficiency?, severity? }`: with no parameters it only reports.
pub(crate) fn color_vision(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let params: Params = match params {
        Value::Null => Params::default(),
        Value::Object(_) => serde_json::from_value(params.clone()).map_err(bad_params)?,
        _ => return Err(bad_params("expected an object")),
    };
    let mode = params
        .mode
        .as_deref()
        .map(|m| {
            ColorVisionMode::parse(m).ok_or_else(|| {
                bad(
                    "color_vision_bad_mode",
                    format!("unknown mode `{m}`"),
                    "\"off\", \"simulate\" or \"compensate\"",
                )
            })
        })
        .transpose()?;
    let deficiency = params
        .deficiency
        .as_deref()
        .map(|d| {
            ColorVisionDeficiency::parse(d).ok_or_else(|| {
                bad(
                    "color_vision_bad_deficiency",
                    format!("unknown deficiency `{d}`"),
                    "\"protanopia\", \"deuteranopia\" or \"tritanopia\"",
                )
            })
        })
        .transpose()?;
    if let Some(s) = params.severity
        && !(s.is_finite() && (0.0..=1.0).contains(&s))
    {
        return Err(bad(
            "color_vision_bad_severity",
            "`severity` must be a number from 0 to 1",
            "1 is full dichromacy; 0.5 approximates a milder anomaly",
        ));
    }
    if mode.is_some() || deficiency.is_some() || params.severity.is_some() {
        let current = world
            .get_resource::<ColorVision>()
            .map(|v| v.filter)
            .or_else(|| {
                world
                    .get_resource::<GameSettings>()
                    .map(|s| s.accessibility.color_vision)
            })
            .unwrap_or_default();
        let filter = ColorVisionFilter {
            mode: mode.unwrap_or(current.mode),
            deficiency: deficiency.unwrap_or(current.deficiency),
            severity: params.severity.unwrap_or(current.severity),
        };
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>()
            && settings.accessibility.color_vision != filter
        {
            settings.accessibility.color_vision = filter;
        }
        world.get_resource_or_insert_with(ColorVision::default).filter = filter;
    }
    Ok(snapshot(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_off_by_default() {
        let mut world = World::new();
        let out = color_vision(&mut world, &json!({})).expect("report");
        assert_eq!(out["mode"], json!("off"));
        assert_eq!(out["active"], json!(false));
        assert_eq!(out["live"], json!(false));
        assert!(world.get_resource::<ColorVision>().is_none());
    }

    #[test]
    fn sets_the_live_filter_and_the_persisted_settings_field_by_field() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        let out = color_vision(
            &mut world,
            &json!({ "mode": "simulate", "deficiency": "protanopia" }),
        )
        .expect("set");
        assert_eq!(out["mode"], json!("simulate"));
        assert_eq!(out["deficiency"], json!("protanopia"));
        assert_eq!(out["severity"], json!(1.0));
        assert_eq!(out["active"], json!(true));
        // A partial change keeps the other fields.
        let out = color_vision(&mut world, &json!({ "severity": 0.5 })).expect("set");
        assert_eq!(out["mode"], json!("simulate"));
        assert_eq!(out["severity"], json!(0.5));
        let saved = world.resource::<GameSettings>().accessibility.color_vision;
        assert_eq!(saved.mode, ColorVisionMode::Simulate);
        assert_eq!(saved.deficiency, ColorVisionDeficiency::Protanopia);
        assert_eq!(saved.severity, 0.5);
        assert_eq!(world.resource::<ColorVision>().filter, saved);
        let out = color_vision(&mut world, &json!({ "mode": "off" })).expect("off");
        assert_eq!(out["active"], json!(false));
    }

    #[test]
    fn rejects_bad_values_and_changes_nothing() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        for (bad, want) in [
            (json!({ "mode": "invert" }), "color_vision_bad_mode"),
            (
                json!({ "deficiency": "achromatopsia" }),
                "color_vision_bad_deficiency",
            ),
            (json!({ "severity": 2.0 }), "color_vision_bad_severity"),
            (json!({ "severity": -0.1 }), "color_vision_bad_severity"),
            (json!({ "severity": "full" }), "color_vision_bad_params"),
            (json!({ "filter": "deuteranopia" }), "color_vision_bad_params"),
        ] {
            let err = color_vision(&mut world, &bad).expect_err("bad");
            assert_eq!(err.data.expect("data")["code"], json!(want), "{bad}");
        }
        assert!(
            world
                .resource::<GameSettings>()
                .accessibility
                .color_vision
                .is_off()
        );
        assert!(world.get_resource::<ColorVision>().is_none());
    }
}
