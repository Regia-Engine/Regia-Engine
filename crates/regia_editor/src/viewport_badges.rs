// SPDX-License-Identifier: MPL-2.0
//! Viewport pane badges (ADR-7770, parity rows P-078 and P-081): a "Realtime off" badge that
//! turns realtime back on, and a warning badge that lists every pane setting that differs from
//! its default (view mode, quality, exposure, screen percentage, show flags, icons) with a reset
//! per setting and "Reset all". The same model answers `editor.viewport_badges`, and the
//! `editor.set_viewport` pane-display parameters are parsed here.

use regia_agent::protocol::RpcError;
use regia_ecs::prelude::World;
use serde_json::{Value, json};

use crate::viewport::ViewportSettings;
use crate::views::{
    IconFilters, PaneDisplay, ShowFlags, ViewMode, ViewportExposure, ViewportLayout, ViewportQuality,
};

/// Every pane setting a badge can list, by stable name and label, in badge order.
pub const SETTINGS: [(&str, &str); 6] = [
    ("view_mode", "View mode"),
    ("quality", "Quality"),
    ("exposure", "Exposure"),
    ("screen_percentage", "Screen percentage"),
    ("show_flags", "Show flags"),
    ("icons", "Icons"),
];

/// The settings of pane `pane` that differ from their defaults, by [`SETTINGS`] name, in
/// [`SETTINGS`] order.
#[must_use]
pub fn changed_settings(layout: &ViewportLayout, pane: usize) -> Vec<&'static str> {
    let display = layout.display(pane);
    let defaults = PaneDisplay::default();
    let show = ShowFlags {
        icon_filter: IconFilters::default(),
        ..layout.show
    };
    SETTINGS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| match *name {
            "view_mode" => display.view_mode != defaults.view_mode,
            "quality" => layout.quality != ViewportQuality::default(),
            "exposure" => display.exposure != defaults.exposure,
            "screen_percentage" => display.screen_percentage() != defaults.screen_percentage,
            "show_flags" => show != ShowFlags::default(),
            "icons" => display.icons != defaults.icons,
            _ => false,
        })
        .collect()
}

/// The label of setting `name` ([`SETTINGS`]).
#[must_use]
pub fn label(name: &str) -> &'static str {
    SETTINGS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("Setting", |(_, label)| label)
}

/// Resets setting `name` of pane `pane` to its default. Returns false for an unknown name.
/// Quality and show flags belong to the whole viewport, so their reset applies to every pane.
pub fn reset_setting(layout: &mut ViewportLayout, pane: usize, name: &str) -> bool {
    let defaults = PaneDisplay::default();
    let display = layout.display_mut(pane);
    match name {
        "view_mode" => display.view_mode = defaults.view_mode,
        "exposure" => display.exposure = defaults.exposure,
        "screen_percentage" => display.screen_percentage = defaults.screen_percentage,
        "icons" => display.icons = defaults.icons,
        "quality" => layout.quality = ViewportQuality::default(),
        "show_flags" => layout.show = ShowFlags::default(),
        _ => return false,
    }
    true
}

/// Resets every changed setting of pane `pane` ("Reset all"). Returns how many were reset.
pub fn reset_all(layout: &mut ViewportLayout, pane: usize) -> usize {
    let changed = changed_settings(layout, pane);
    for name in &changed {
        reset_setting(layout, pane, name);
    }
    changed.len()
}

/// The icon filters as a JSON object of kind switches.
#[must_use]
pub fn icon_filters_json(icons: &IconFilters) -> Value {
    let map: serde_json::Map<String, Value> = IconFilters::KINDS
        .iter()
        .map(|(name, _)| ((*name).to_string(), Value::Bool(icons.kind(name))))
        .collect();
    Value::Object(map)
}

/// Applies the `editor.set_viewport` pane-display parameters (`view_mode`, `icon_filters`,
/// `icon_size`, `icon_opacity`, `exposure`, `exposure_ev`, `screen_percentage`) to pane `pane`.
///
/// # Errors
///
/// Invalid params for an unknown name or an out-of-range value; nothing changes then.
pub fn apply_display_params(
    layout: &mut ViewportLayout,
    pane: usize,
    params: &Value,
) -> Result<(), RpcError> {
    let mut display = *layout.display(pane);
    if let Some(value) = params.get("view_mode") {
        let name = value
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`view_mode` must be a string"))?;
        display.view_mode = ViewMode::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown view_mode `{name}` (lit, unlit, wireframe_only, lit_wireframe, lighting_only, \
                 light_complexity, overdraw, or a buffer view such as base_color)"
            ))
        })?;
    }
    if let Some(value) = params.get("icon_filters") {
        let object = value
            .as_object()
            .ok_or_else(|| RpcError::invalid_params("`icon_filters` must be an object of kind: bool"))?;
        for (kind, on) in object {
            let on = on
                .as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("icon_filters.{kind} must be a bool")))?;
            *display.icons.kind_mut(kind).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown icon kind `{kind}` (lights, cameras, audio, decals, player_starts, match_rules, colliders, \
                     captures, volumes, nav)"
                ))
            })? = on;
        }
    }
    let percent = |name: &str, range: std::ops::RangeInclusive<f64>| -> Result<Option<f64>, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_f64()
                .filter(|v| range.contains(v))
                .map(|v| Some(v.round()))
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`{name}` must be a number in {}..={}",
                        range.start(),
                        range.end()
                    ))
                }),
        }
    };
    if let Some(size) = percent("icon_size", 50.0..=200.0)? {
        display.icons.size_percent = size as u16;
    }
    if let Some(opacity) = percent("icon_opacity", 10.0..=100.0)? {
        display.icons.opacity_percent = opacity as u8;
    }
    if let Some(scale) = percent("screen_percentage", 25.0..=200.0)? {
        display.screen_percentage = scale as u16;
    }
    let ev = match params.get("exposure_ev") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|v| ViewportExposure::EV_RANGE.contains(&(*v as f32)))
                .ok_or_else(|| RpcError::invalid_params("`exposure_ev` must be a number in -4..=20"))?
                as f32,
        ),
    };
    match (params.get("exposure").and_then(Value::as_str), ev) {
        (Some("auto"), _) => display.exposure = ViewportExposure::Auto,
        (Some("level_band"), _) => display.exposure = ViewportExposure::LevelBand,
        (Some("fixed") | None, Some(ev)) => display.exposure = ViewportExposure::Fixed(ev),
        (Some("fixed"), None) => {
            let current = display
                .exposure
                .fixed_ev()
                .unwrap_or(crate::render::VIEWPORT_EXPOSURE_EV);
            display.exposure = ViewportExposure::Fixed(current);
        }
        (Some(other), _) => {
            return Err(RpcError::invalid_params(format!(
                "unknown exposure `{other}` (auto, fixed, level_band)"
            )));
        }
        (None, None) => {
            if params.get("exposure").is_some_and(|v| !v.is_null()) {
                return Err(RpcError::invalid_params("`exposure` must be a string"));
            }
        }
    }
    *layout.display_mut(pane) = display;
    Ok(())
}

/// The badges of pane `pane` as JSON.
#[must_use]
pub fn badges_json(layout: &ViewportLayout, settings: &ViewportSettings, pane: usize) -> Value {
    json!({
        "pane": pane,
        "realtime_off": !settings.realtime,
        "changed": changed_settings(layout, pane)
            .iter()
            .map(|name| json!({ "setting": name, "label": label(name) }))
            .collect::<Vec<_>>(),
    })
}

/// `editor.viewport_badges`.
fn viewport_badges(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut layout = world.get_resource_or_insert_with(ViewportLayout::default).clone();
    let mut settings = world
        .get_resource_or_insert_with(ViewportSettings::default)
        .clone();
    let pane = match params.get("pane") {
        None | Some(Value::Null) => layout.active.min(3),
        Some(value) => value
            .as_u64()
            .filter(|p| *p < 4)
            .ok_or_else(|| RpcError::invalid_params("`pane` must be 0..=3"))? as usize,
    };
    if params.get("restore_realtime").and_then(Value::as_bool) == Some(true) {
        settings.realtime = true;
    }
    match params.get("reset") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) if name == "all" => {
            reset_all(&mut layout, pane);
        }
        Some(Value::String(name)) => {
            if !reset_setting(&mut layout, pane, name) {
                return Err(RpcError::new(
                    regia_agent::protocol::INVALID_PARAMS,
                    "viewport_badge_unknown_setting",
                    format!("unknown setting `{name}`"),
                    "one of all, view_mode, quality, exposure, screen_percentage, show_flags, icons",
                ));
            }
        }
        Some(_) => return Err(RpcError::invalid_params("`reset` must be a string")),
    }
    let out = badges_json(&layout, &settings, pane);
    world.insert_resource(layout);
    world.insert_resource(settings);
    Ok(out)
}

/// The methods of this module (`editor.viewport_badges`).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.viewport_badges",
            tool: "viewport_badges",
            description: "The badges of a viewport pane (ADR-7770): realtime_off when the viewport renders only on \
                change, and every pane setting that differs from its default (view_mode, quality, exposure, \
                screen_percentage, show_flags, icons). reset resets one setting or all; restore_realtime turns \
                realtime back on (the badge's click). Error viewport_badge_unknown_setting.",
            params: &[
                ParamSchema {
                    name: "pane",
                    kind: "integer",
                    required: false,
                    description: "The pane (0-3); default the active pane.",
                },
                ParamSchema {
                    name: "reset",
                    kind: "string",
                    required: false,
                    description: "all, or one setting name to reset to its default.",
                },
                ParamSchema {
                    name: "restore_realtime",
                    kind: "boolean",
                    required: false,
                    description: "Turn realtime rendering back on.",
                },
            ],
            result: "{ pane, realtime_off, changed: [{ setting, label }] }",
            mutating: false,
            permission: Permission::Read,
        },
        viewport_badges as crate::viewport_agent::Handler,
    )]
}

/// What a click in the badge strip asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BadgeActions {
    /// "Realtime off" was clicked.
    pub restore_realtime: bool,
    /// Settings to reset (`all` for Reset all).
    pub reset: Vec<&'static str>,
}

/// Draws pane `pane`'s badges at the bottom left of `rect` and returns what was clicked. Buttons
/// carry their text (named for screen readers), are keyboard focusable, take their colors from
/// the theme's `warn` token and scale with the UI scale like every egui widget.
pub fn draw_badges(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    layout: &ViewportLayout,
    realtime: bool,
    pane: usize,
    warn: egui::Color32,
) -> BadgeActions {
    let mut actions = BadgeActions::default();
    let changed = changed_settings(layout, pane);
    if realtime && changed.is_empty() {
        return actions;
    }
    let area = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 4.0, rect.bottom() - 26.0),
        egui::pos2(rect.right() - 4.0, rect.bottom() - 2.0),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(area)
            .id_salt(("viewport-badges", pane))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    if !realtime
        && child
            .button(egui::RichText::new("Realtime off").color(warn))
            .on_hover_text("The viewport renders only on change; click to render every frame")
            .clicked()
    {
        actions.restore_realtime = true;
    }
    if !changed.is_empty() {
        let title = format!("⚠ {} changed", changed.len());
        child
            .menu_button(egui::RichText::new(title).color(warn), |ui| {
                for name in &changed {
                    ui.horizontal(|ui| {
                        ui.label(label(name));
                        if ui
                            .button("Reset")
                            .on_hover_text(format!("Reset {} to its default", label(name).to_lowercase()))
                            .clicked()
                        {
                            actions.reset.push(*name);
                            ui.close();
                        }
                    });
                }
                ui.separator();
                if ui.button("Reset all").clicked() {
                    actions.reset.push("all");
                    ui.close();
                }
            })
            .response
            .on_hover_text("Pane settings that differ from their defaults");
    }
    actions
}

/// Applies `actions` to the pane and the viewport settings. Returns true when something changed.
pub fn apply_actions(
    actions: &BadgeActions,
    layout: &mut ViewportLayout,
    settings: &mut ViewportSettings,
    pane: usize,
) -> bool {
    let mut changed = false;
    if actions.restore_realtime && !settings.realtime {
        settings.realtime = true;
        changed = true;
    }
    for name in &actions.reset {
        changed |= if *name == "all" {
            reset_all(layout, pane) > 0
        } else {
            reset_setting(layout, pane, name)
        };
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P-081: a default pane has no badge; each changed setting is listed, its reset clears only
    /// it, and Reset all clears the rest.
    #[test]
    fn changed_settings_list_and_reset() {
        let mut layout = ViewportLayout::default();
        assert!(changed_settings(&layout, 0).is_empty());
        layout.display_mut(0).view_mode = ViewMode::Unlit;
        layout.display_mut(0).exposure = ViewportExposure::Fixed(10.0);
        layout.display_mut(0).screen_percentage = 50;
        layout.show.grid = false;
        layout.quality = ViewportQuality::Game;
        layout.display_mut(0).icons.cameras = false;
        assert_eq!(
            changed_settings(&layout, 0),
            vec![
                "view_mode",
                "quality",
                "exposure",
                "screen_percentage",
                "show_flags",
                "icons"
            ]
        );
        // Pane 1 shares quality and show flags only.
        assert_eq!(changed_settings(&layout, 1), vec!["quality", "show_flags"]);
        assert!(reset_setting(&mut layout, 0, "exposure"));
        assert!(!changed_settings(&layout, 0).contains(&"exposure"));
        assert!(!reset_setting(&mut layout, 0, "nope"));
        assert_eq!(reset_all(&mut layout, 0), 5);
        assert!(changed_settings(&layout, 0).is_empty());
    }

    /// The `editor.set_viewport` pane-display parameters, and their range checks.
    #[test]
    fn display_params_apply_and_reject_bad_values() {
        let mut layout = ViewportLayout::default();
        let params = json!({
            "view_mode": "lighting_only",
            "icon_filters": { "lights": false, "nav": false },
            "icon_size": 150,
            "icon_opacity": 40,
            "exposure_ev": 9.5,
            "screen_percentage": 50,
        });
        apply_display_params(&mut layout, 2, &params).expect("valid");
        let display = layout.display(2);
        assert_eq!(display.view_mode, ViewMode::LightingOnly);
        assert!(!display.icons.lights && !display.icons.nav && display.icons.cameras);
        assert_eq!(
            (display.icons.size_percent, display.icons.opacity_percent),
            (150, 40)
        );
        assert_eq!(display.exposure, ViewportExposure::Fixed(9.5));
        assert_eq!(display.screen_percentage, 50);
        assert_eq!(
            *layout.display(0),
            PaneDisplay::default(),
            "other panes keep theirs"
        );
        for bad in [
            json!({ "view_mode": "nope" }),
            json!({ "icon_filters": { "splines": true } }),
            json!({ "icon_size": 400 }),
            json!({ "screen_percentage": 10 }),
            json!({ "exposure": "manual" }),
            json!({ "exposure_ev": 40 }),
        ] {
            let before = *layout.display(2);
            assert!(apply_display_params(&mut layout, 2, &bad).is_err(), "{bad}");
            assert_eq!(*layout.display(2), before, "{bad} changed nothing");
        }
        apply_display_params(&mut layout, 2, &json!({ "exposure": "level_band" })).expect("valid");
        assert_eq!(layout.display(2).exposure, ViewportExposure::LevelBand);
    }

    /// P-078: the badge actions turn realtime back on and reset settings.
    #[test]
    fn badge_actions_restore_realtime_and_reset() {
        let mut layout = ViewportLayout::default();
        layout.display_mut(0).view_mode = ViewMode::Overdraw;
        let mut settings = ViewportSettings {
            realtime: false,
            ..ViewportSettings::default()
        };
        let actions = BadgeActions {
            restore_realtime: true,
            reset: vec!["all"],
        };
        assert!(apply_actions(&actions, &mut layout, &mut settings, 0));
        assert!(settings.realtime);
        assert!(changed_settings(&layout, 0).is_empty());
        assert!(
            !apply_actions(&actions, &mut layout, &mut settings, 0),
            "nothing left"
        );
    }
}
