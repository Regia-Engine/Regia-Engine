// SPDX-License-Identifier: MPL-2.0
//! `ui.accessibility`: read or change the player's accessibility choices as the UI sees them
//! (book `agents/ui-accessibility.md`, ADR-341).
//!
//! A change writes `GameSettings.accessibility` only when that resource is already in the world
//! (so it persists like `settings.set`). The live `UiAccessibility` resource is updated either
//! way; `persisted` is false when `GameSettings` is absent. The text re-lays out on the next
//! layout pass.

use regia_ecs::prelude::*;
use regia_framework::settings::GameSettings;
use regia_ui::style::locale_is_rtl;
use regia_ui::{Localization, UiAccessibility};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn bad_text_scale() -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "ui_bad_text_scale",
        format!(
            "`text_scale` must be a number from {} to {}",
            UiAccessibility::MIN_TEXT_SCALE,
            UiAccessibility::MAX_TEXT_SCALE
        ),
        "1.0 is the authored size; try 1.25 or 1.5",
    )
}

fn bad_flag(name: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "ui_bad_flag",
        format!("`{name}` must be true or false"),
        "pass a JSON boolean",
    )
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad_flag(name)),
    }
}

fn text_scale(params: &Value) -> Result<Option<f32>, RpcError> {
    match params.get("text_scale") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_f64().ok_or_else(bad_text_scale)? as f32;
            if n.is_finite()
                && (UiAccessibility::MIN_TEXT_SCALE..=UiAccessibility::MAX_TEXT_SCALE).contains(&n)
            {
                Ok(Some(n))
            } else {
                Err(bad_text_scale())
            }
        }
    }
}

fn snapshot(world: &World) -> Value {
    let live = world
        .get_resource::<UiAccessibility>()
        .copied()
        .unwrap_or_default();
    let locale = world
        .get_resource::<Localization>()
        .map(|l| l.locale().to_owned())
        .unwrap_or_default();
    json!({
        "text_scale": live.text_scale,
        "effective_text_scale": live.effective_text_scale(),
        "reduced_motion": live.reduced_motion,
        "high_contrast": live.high_contrast,
        "locale": locale,
        "locale_rtl": locale_is_rtl(&locale),
        "persisted": world.contains_resource::<GameSettings>(),
    })
}

/// `ui.accessibility { text_scale?, reduced_motion?, high_contrast? }`: with no parameters it
/// only reports.
pub(crate) fn accessibility(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let scale = text_scale(params)?;
    let motion = flag(params, "reduced_motion")?;
    let contrast = flag(params, "high_contrast")?;
    if scale.is_some() || motion.is_some() || contrast.is_some() {
        let mut next = world
            .get_resource::<GameSettings>()
            .map(|settings| UiAccessibility {
                text_scale: settings.accessibility.text_scale,
                reduced_motion: settings.accessibility.reduced_motion,
                high_contrast: settings.accessibility.high_contrast,
            })
            .or_else(|| world.get_resource::<UiAccessibility>().copied())
            .unwrap_or_default();
        if let Some(v) = scale {
            next.text_scale = v;
        }
        if let Some(v) = motion {
            next.reduced_motion = v;
        }
        if let Some(v) = contrast {
            next.high_contrast = v;
        }
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>() {
            settings.accessibility.text_scale = next.text_scale;
            settings.accessibility.reduced_motion = next.reduced_motion;
            settings.accessibility.high_contrast = next.high_contrast;
        }
        world.insert_resource(next);
    }
    Ok(snapshot(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_update_keeps_settings_and_live_accessibility_together() {
        for existing_live in [false, true] {
            let mut world = World::new();
            let mut settings = GameSettings::default();
            settings.accessibility.text_scale = 1.5;
            settings.accessibility.high_contrast = true;
            world.insert_resource(settings);
            if existing_live {
                world.insert_resource(UiAccessibility::default());
            }
            let out = accessibility(&mut world, &json!({ "reduced_motion": true })).expect("partial update");
            let saved = &world.resource::<GameSettings>().accessibility;
            let live = world.resource::<UiAccessibility>();
            assert_eq!(live.text_scale, saved.text_scale);
            assert_eq!(live.high_contrast, saved.high_contrast);
            assert_eq!(live.reduced_motion, saved.reduced_motion);
            assert_eq!(out["text_scale"], 1.5);
            assert_eq!(out["high_contrast"], true);
            assert_eq!(out["reduced_motion"], true);
            assert_eq!(out["persisted"], true);
        }
    }

    #[test]
    fn reports_defaults_without_changing_anything() {
        let mut world = World::new();
        let out = accessibility(&mut world, &json!({})).expect("report");
        assert_eq!(out["text_scale"], json!(1.0));
        assert_eq!(out["reduced_motion"], json!(false));
        assert_eq!(out["high_contrast"], json!(false));
        assert_eq!(out["locale_rtl"], json!(false));
        assert_eq!(out["persisted"], json!(false));
    }

    #[test]
    fn sets_the_live_resource_and_the_persisted_settings() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        let out = accessibility(
            &mut world,
            &json!({ "text_scale": 1.5, "reduced_motion": true, "high_contrast": true }),
        )
        .expect("set");
        assert_eq!(out["text_scale"], json!(1.5));
        assert_eq!(out["reduced_motion"], json!(true));
        assert_eq!(out["persisted"], json!(true));
        let settings = world.resource::<GameSettings>();
        assert_eq!(settings.accessibility.text_scale, 1.5);
        assert!(settings.accessibility.reduced_motion);
        assert!(settings.accessibility.high_contrast);
        // A partial change leaves the other fields alone.
        let out = accessibility(&mut world, &json!({ "reduced_motion": false })).expect("set");
        assert_eq!(out["reduced_motion"], json!(false));
        assert_eq!(out["text_scale"], json!(1.5));
        assert_eq!(out["high_contrast"], json!(true));
    }

    #[test]
    fn rejects_bad_values_and_changes_nothing() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        for bad in [
            json!({ "text_scale": 0.1 }),
            json!({ "text_scale": 9 }),
            json!({ "text_scale": "big" }),
        ] {
            let err = accessibility(&mut world, &bad).expect_err("bad text_scale");
            assert_eq!(err.data.expect("data")["code"], json!("ui_bad_text_scale"));
        }
        let err = accessibility(&mut world, &json!({ "high_contrast": 1 })).expect_err("bad flag");
        assert_eq!(err.data.expect("data")["code"], json!("ui_bad_flag"));
        assert_eq!(world.resource::<GameSettings>().accessibility.text_scale, 1.0);
        assert!(world.get_resource::<UiAccessibility>().is_none());
    }

    #[test]
    fn reports_an_rtl_locale() {
        let mut world = World::new();
        world.init_resource::<Localization>();
        world.resource_mut::<Localization>().selected = "ar-EG".to_owned();
        let out = accessibility(&mut world, &json!({})).expect("report");
        assert_eq!(out["locale"], json!("ar-EG"));
        assert_eq!(out["locale_rtl"], json!(true));
    }
}
