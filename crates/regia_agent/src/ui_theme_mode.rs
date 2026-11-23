// SPDX-License-Identifier: MPL-2.0
//! `ui.theme_mode`: read or change how the UI picks its theme at runtime (book
//! `agents/ui-theme-switch.md`, ADR-865): the theme in use, the scheme preference
//! ([`ThemeMode`]), high contrast and reduced motion, and whether a cross-fade is running.
//!
//! A change applies on the next frame as one theme generation; `applies_at_generation` says which.
//! Wait for the fade with the `ui.theme_fading` atom (`{ predicate: "ui.theme_fading", is: false
//! }`), or `play.step` until `animating` is 0. `transition` shapes the swap fade (length, curve,
//! blend space; ADR-1309).

use regia_ecs::prelude::*;
use regia_ui::theme_file::Scheme;
use regia_ui::theme_runtime::{BlendSpace, MAX_SWAP_FADE_SECS, SchemePreference, ThemeMode, ThemeTransition};
use regia_ui::theme_tokens::{Easing, Expr, Value as TokenValue};
use regia_ui::{ActiveTheme, ThemeResolver, UiThemes};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn scheme_name(s: Scheme) -> &'static str {
    match s {
        Scheme::Dark => "dark",
        Scheme::Light => "light",
    }
}

fn flag(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| {
            invalid(
                "bad_flag",
                format!("`{key}` must be true or false"),
                "pass a JSON boolean",
            )
        }),
    }
}

/// A theme name parameter that must name a loaded theme.
fn theme_param(params: &Value, key: &str, world: &World) -> Result<Option<String>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => {
            let name = v.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                invalid(
                    "bad_theme",
                    format!("`{key}` must be a theme name"),
                    "list themes with ui.theme_mode {}",
                )
            })?;
            let loaded = world
                .get_resource::<UiThemes>()
                .is_some_and(|t| t.set.get(name).is_some());
            if !loaded {
                return Err(RpcError::new(
                    NOT_FOUND,
                    "unknown_theme",
                    format!("no theme named `{name}` is loaded"),
                    "`themes` in the ui.theme_mode answer lists the loaded ones",
                ));
            }
            Ok(Some(name.to_owned()))
        }
    }
}

/// An easing as the protocol writes it: `"linear"` or `[x1, y1, x2, y2]`.
fn ease_json(ease: Option<Easing>) -> Value {
    match ease {
        None | Some(Easing::Linear) => json!("linear"),
        Some(Easing::CubicBezier(p)) => json!(p),
    }
}

fn transition_json(t: &ThemeTransition) -> Value {
    json!({
        // Rounded to a microsecond: f32 seconds do not print as whole milliseconds.
        "duration_ms": t.duration.map(|s| (f64::from(s) * 1e6).round() / 1e3),
        "ease": t.ease.map_or_else(|| json!("theme"), |e| ease_json(Some(e))),
        "space": t.space.name(),
    })
}

fn bad_ease() -> RpcError {
    invalid(
        "bad_ease",
        "`transition.ease` must be \"theme\", \"linear\", \"cubic-bezier(x1, y1, x2, y2)\" or [x1, y1, x2, y2]",
        "x1 and x2 must lie in 0..1; \"theme\" uses the theme's ease.standard",
    )
}

/// A cubic Bézier from four finite numbers with both x control points in `0..=1`.
fn bezier(points: [f32; 4]) -> Result<Easing, RpcError> {
    let [x1, _, x2, _] = points;
    if points.iter().all(|v| v.is_finite()) && (0.0..=1.0).contains(&x1) && (0.0..=1.0).contains(&x2) {
        Ok(Easing::CubicBezier(points))
    } else {
        Err(bad_ease())
    }
}

/// `transition.ease`: `None` = the theme's curve.
fn ease_param(v: &Value) -> Result<Option<Easing>, RpcError> {
    if v.is_null() {
        return Ok(None);
    }
    if let Some(items) = v.as_array() {
        let numbers: Vec<f32> = items.iter().filter_map(Value::as_f64).map(|n| n as f32).collect();
        return match <[f32; 4]>::try_from(numbers.as_slice()) {
            Ok(points) if items.len() == 4 => bezier(points).map(Some),
            _ => Err(bad_ease()),
        };
    }
    match v.as_str() {
        Some("theme") => Ok(None),
        Some("linear") => Ok(Some(Easing::Linear)),
        Some(text) => match Expr::parse(text) {
            Ok(Expr::Literal(TokenValue::Easing(Easing::CubicBezier(points)))) => bezier(points).map(Some),
            _ => Err(bad_ease()),
        },
        None => Err(bad_ease()),
    }
}

/// The `transition` object applied over the current settings.
fn transition_param(params: &Value, current: ThemeTransition) -> Result<Option<ThemeTransition>, RpcError> {
    let Some(v) = params.get("transition").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(fields) = v.as_object() else {
        return Err(invalid(
            "bad_transition",
            "`transition` must be an object { duration_ms?, ease?, space? }",
            "omit a field to keep it; null resets duration_ms and ease to the theme's tokens",
        ));
    };
    if let Some(key) = fields
        .keys()
        .find(|k| !matches!(k.as_str(), "duration_ms" | "ease" | "space"))
    {
        return Err(invalid(
            "bad_transition",
            format!("unknown `transition` field `{key}`"),
            "the fields are duration_ms, ease and space",
        ));
    }
    let mut next = current;
    if let Some(d) = fields.get("duration_ms") {
        next.duration = if d.is_null() {
            None
        } else {
            let max_ms = f64::from(MAX_SWAP_FADE_SECS) * 1000.0;
            let ms = d
                .as_f64()
                .filter(|ms| ms.is_finite() && (0.0..=max_ms).contains(ms))
                .ok_or_else(|| {
                    invalid(
                        "bad_duration",
                        format!("`transition.duration_ms` must be a number from 0 to {max_ms}"),
                        "0 swaps instantly; null uses the theme's motion.base",
                    )
                })?;
            Some((ms / 1000.0) as f32)
        };
    }
    if let Some(e) = fields.get("ease") {
        next.ease = ease_param(e)?;
    }
    if let Some(s) = fields.get("space") {
        next.space = s.as_str().and_then(BlendSpace::parse).ok_or_else(|| {
            invalid(
                "bad_space",
                "`transition.space` must be \"oklab\" or \"linear\"",
                "oklab brightens evenly; linear is the plain linear-light blend",
            )
        })?;
    }
    Ok(Some(next))
}

fn snapshot(world: &World, changed: bool) -> Value {
    let resolver = world.get_resource::<ThemeResolver>();
    let active = world.get_resource::<ActiveTheme>().cloned().unwrap_or_default();
    let mode = world.get_resource::<ThemeMode>().cloned().unwrap_or_default();
    let transition = world
        .get_resource::<ThemeTransition>()
        .copied()
        .unwrap_or_default();
    let mut themes: Vec<String> = world
        .get_resource::<UiThemes>()
        .map(|t| t.set.names().map(str::to_owned).collect())
        .unwrap_or_default();
    themes.sort();
    let generation = resolver.map_or(0, ThemeResolver::generation);
    // The fade the next swap would run (the current theme's motion.base unless overridden).
    let fade = if active.reduced_motion {
        0.0
    } else {
        transition.fade_secs(
            resolver
                .and_then(ThemeResolver::resolved_theme)
                .and_then(|t| t.duration("motion.base")),
        )
    };
    json!({
        "active": resolver.map(|r| r.active().to_owned()),
        "requested": active.name,
        "generation": generation,
        "applies_at_generation": if changed { generation + 1 } else { generation },
        "animating": resolver.map_or(0, |r| r.stats().animating),
        "fade_seconds": fade,
        "previous": resolver.and_then(ThemeResolver::previous),
        "transition": transition_json(&transition),
        "fade": {
            "fading": resolver.is_some_and(ThemeResolver::fading),
            "nodes": resolver.map_or(0, ThemeResolver::fading_nodes),
            "seconds": resolver.map_or(0.0, ThemeResolver::fade_secs),
            "remaining_seconds": resolver.map_or(0.0, ThemeResolver::fade_remaining),
            "ease": ease_json(resolver.and_then(ThemeResolver::fade_ease)),
            "space": resolver.map(ThemeResolver::fade_space).unwrap_or_default().name(),
        },
        "follow": mode.follow.map(SchemePreference::name),
        "system_scheme": mode.system_scheme.map(scheme_name),
        "dark": mode.dark,
        "light": mode.light,
        "high_contrast": active.high_contrast,
        "reduced_motion": active.reduced_motion,
        "themes": themes,
    })
}

/// `ui.theme_mode { theme?, follow?, system_scheme?, dark?, light?, high_contrast?, reduced_motion?,
/// transition? }`.
pub(crate) fn theme_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<ActiveTheme>() || !world.contains_resource::<ThemeMode>() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "ui_not_installed",
            "this app has no runtime UI themes",
            "the app needs regia_ui::UiPlugin",
        ));
    }
    let theme = theme_param(params, "theme", world)?;
    let dark = theme_param(params, "dark", world)?;
    let light = theme_param(params, "light", world)?;
    let follow = match params.get("follow").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => match v.as_str() {
            Some("off") => Some(None),
            Some(name) => Some(Some(SchemePreference::parse(name).ok_or_else(|| {
                invalid(
                    "bad_follow",
                    "`follow` must be \"dark\", \"light\", \"system\" or \"off\"",
                    "system follows `system_scheme`",
                )
            })?)),
            None => {
                return Err(invalid(
                    "bad_follow",
                    "`follow` must be \"dark\", \"light\", \"system\" or \"off\"",
                    "system follows `system_scheme`",
                ));
            }
        },
    };
    let system_scheme = match params.get("system_scheme").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(match v.as_str() {
            Some("dark") => Some(Scheme::Dark),
            Some("light") => Some(Scheme::Light),
            Some("unknown") => None,
            _ => {
                return Err(invalid(
                    "bad_scheme",
                    "`system_scheme` must be \"dark\", \"light\" or \"unknown\"",
                    "it stands in for the OS setting the host reports",
                ));
            }
        }),
    };
    let high_contrast = flag(params, "high_contrast")?;
    let reduced_motion = flag(params, "reduced_motion")?;
    let current_transition = world
        .get_resource::<ThemeTransition>()
        .copied()
        .unwrap_or_default();
    let transition = transition_param(params, current_transition)?;
    if theme.is_some() && matches!(follow, Some(Some(_))) {
        return Err(invalid(
            "theme_and_follow",
            "pass either `theme` (select one) or `follow` (follow a scheme), not both",
            "a selected theme turns following off",
        ));
    }

    if let Some(t) = transition
        && t != current_transition
    {
        world.insert_resource(t);
    }
    let before = world.resource::<ActiveTheme>().clone();
    {
        let mut mode = world.resource_mut::<ThemeMode>();
        let mut next = mode.clone();
        if let Some(f) = follow {
            next.follow = f;
        }
        if theme.is_some() {
            next.follow = None;
        }
        if let Some(s) = system_scheme {
            next.system_scheme = s;
        }
        if let Some(d) = dark {
            next.dark = d;
        }
        if let Some(l) = light {
            next.light = l;
        }
        if *mode != next {
            *mode = next;
        }
    }
    let wanted = world
        .resource::<ThemeMode>()
        .wanted_theme()
        .map(str::to_owned)
        .or(theme);
    let mut next = before.clone();
    if let Some(name) = wanted {
        next.name = name;
    }
    if let Some(v) = high_contrast {
        next.high_contrast = v;
    }
    if let Some(v) = reduced_motion {
        next.reduced_motion = v;
    }
    let changed = next != before;
    if changed {
        *world.resource_mut::<ActiveTheme>() = next;
    }
    Ok(snapshot(world, changed))
}

/// The `wait_until` / `play.expect` atom `{ "predicate": "ui.theme_fading", is? }`: holds when
/// whether a theme-swap cross-fade is running equals `is` (default `true`). Wait for a swap to
/// settle with `is: false`.
pub(crate) fn predicate(world: &World, atom: &Value) -> Result<(bool, String), String> {
    let want = match atom.get("is").filter(|v| !v.is_null()) {
        None => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| "bad_flag: `is` must be true or false".to_owned())?,
    };
    let Some(resolver) = world.get_resource::<ThemeResolver>() else {
        return Err("ui_not_installed: this app has no runtime UI themes".to_owned());
    };
    let fading = resolver.fading();
    Ok((
        fading == want,
        format!(
            "theme fading {fading} ({} nodes, generation {}), wanted {want}",
            resolver.fading_nodes(),
            resolver.generation()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<ActiveTheme>();
        world.init_resource::<ThemeMode>();
        world.init_resource::<UiThemes>();
        world
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_owned()
    }

    #[test]
    fn reports_and_selects() {
        let mut world = world();
        let out = theme_mode(&mut world, &json!({})).expect("report");
        assert_eq!(out["requested"], json!("regia-dark"));
        assert_eq!(out["follow"], Value::Null);
        let themes: Vec<&str> = out["themes"]
            .as_array()
            .expect("themes")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            themes.contains(&"regia-light") && themes.contains(&"regia-high-contrast"),
            "{themes:?}"
        );

        let out = theme_mode(&mut world, &json!({ "theme": "regia-light" })).expect("select");
        assert_eq!(out["requested"], json!("regia-light"));
        assert_eq!(out["applies_at_generation"], json!(1));
        assert_eq!(world.resource::<ActiveTheme>().name, "regia-light");

        let out = theme_mode(
            &mut world,
            &json!({ "follow": "system", "system_scheme": "dark" }),
        )
        .expect("follow");
        assert_eq!(out["follow"], json!("system"));
        assert_eq!(out["requested"], json!("regia-dark"));
        let out = theme_mode(
            &mut world,
            &json!({ "high_contrast": true, "reduced_motion": true }),
        )
        .expect("flags");
        assert_eq!(out["high_contrast"], json!(true));
        assert_eq!(out["fade_seconds"], json!(0.0));
        // Selecting a theme turns following off.
        let out = theme_mode(&mut world, &json!({ "theme": "regia-light" })).expect("select");
        assert_eq!(out["follow"], Value::Null);
        // No change: the generation it applies at is the current one.
        let out = theme_mode(&mut world, &json!({ "theme": "regia-light" })).expect("same");
        assert_eq!(out["applies_at_generation"], out["generation"]);
    }

    #[test]
    fn validates_and_changes_nothing_on_error() {
        let mut world = world();
        for (params, expected) in [
            (json!({ "theme": "sepia" }), "unknown_theme"),
            (json!({ "theme": 3 }), "bad_theme"),
            (json!({ "dark": "" }), "bad_theme"),
            (json!({ "follow": "sometimes" }), "bad_follow"),
            (json!({ "follow": 1 }), "bad_follow"),
            (json!({ "system_scheme": "sepia" }), "bad_scheme"),
            (json!({ "high_contrast": "yes" }), "bad_flag"),
            (
                json!({ "theme": "regia-light", "follow": "dark" }),
                "theme_and_follow",
            ),
        ] {
            let err = theme_mode(&mut world, &params).expect_err("invalid");
            assert_eq!(code(&err), expected, "{params}");
            crate::contract::assert_listed("ui.theme_mode", expected);
        }
        assert_eq!(world.resource::<ActiveTheme>().name, "regia-dark");
        assert_eq!(world.resource::<ThemeMode>().follow, None);
        let err = theme_mode(&mut World::new(), &json!({})).expect_err("no ui");
        assert_eq!(code(&err), "ui_not_installed");
    }

    #[test]
    fn transition_sets_length_curve_and_space() {
        let mut world = world();
        let out = theme_mode(&mut world, &json!({})).expect("report");
        assert_eq!(
            out["transition"],
            json!({ "duration_ms": null, "ease": "theme", "space": "oklab" }),
            "defaults: the theme's tokens, blended in OKLab"
        );
        assert_eq!(out["fade"]["fading"], json!(false));

        let out = theme_mode(
            &mut world,
            &json!({ "transition": { "duration_ms": 400, "ease": "cubic-bezier(0.4, 0, 0.2, 1)", "space": "linear" } }),
        )
        .expect("set");
        let t = *world.resource::<ThemeTransition>();
        assert_eq!(t.duration, Some(0.4));
        assert_eq!(t.ease, Some(Easing::CubicBezier([0.4, 0.0, 0.2, 1.0])));
        assert_eq!(t.space, BlendSpace::Linear);
        assert_eq!(
            out["transition"]["ease"],
            json!([0.4_f32, 0.0_f32, 0.2_f32, 1.0_f32])
        );
        assert_eq!(out["fade_seconds"], json!(0.4_f32), "the next swap's fade");
        assert_eq!(out["applies_at_generation"], out["generation"], "no theme change");

        // Fields left out are kept; null goes back to the theme's token.
        theme_mode(
            &mut world,
            &json!({ "transition": { "ease": [0.0, 0.0, 1.0, 1.0] } }),
        )
        .expect("ease");
        let t = *world.resource::<ThemeTransition>();
        assert_eq!(t.duration, Some(0.4));
        assert_eq!(t.space, BlendSpace::Linear);
        theme_mode(
            &mut world,
            &json!({ "transition": { "duration_ms": null, "ease": "linear" } }),
        )
        .expect("reset");
        let t = *world.resource::<ThemeTransition>();
        assert_eq!(t.duration, None);
        assert_eq!(t.ease, Some(Easing::Linear));
    }

    #[test]
    fn bad_transitions_change_nothing() {
        let mut world = world();
        for (transition, expected) in [
            (json!(3), "bad_transition"),
            (json!({ "speed": 2 }), "bad_transition"),
            (json!({ "duration_ms": -1 }), "bad_duration"),
            (json!({ "duration_ms": 10_001 }), "bad_duration"),
            (json!({ "duration_ms": "fast" }), "bad_duration"),
            (json!({ "ease": "bouncy" }), "bad_ease"),
            (json!({ "ease": [1.5, 0, 0, 1] }), "bad_ease"),
            (json!({ "ease": [0.2, 0, 1] }), "bad_ease"),
            (json!({ "ease": "cubic-bezier(0.2, 0, 2, 1)" }), "bad_ease"),
            (json!({ "space": "hsl" }), "bad_space"),
        ] {
            let params = json!({ "transition": transition, "theme": "regia-light" });
            let err = theme_mode(&mut world, &params).expect_err("invalid");
            assert_eq!(code(&err), expected, "{params}");
            crate::contract::assert_listed("ui.theme_mode", expected);
        }
        assert_eq!(
            world.get_resource::<ThemeTransition>(),
            None,
            "nothing was written"
        );
        assert_eq!(
            world.resource::<ActiveTheme>().name,
            "regia-dark",
            "no half-applied call"
        );
    }

    #[test]
    fn the_fading_atom_follows_the_resolver() {
        let mut world = world();
        let err = predicate(&World::new(), &json!({ "predicate": "ui.theme_fading" })).expect_err("no ui");
        assert!(err.starts_with("ui_not_installed"), "{err}");
        world.init_resource::<ThemeResolver>();
        let (holds, detail) = predicate(&world, &json!({ "predicate": "ui.theme_fading" })).expect("atom");
        assert!(!holds, "nothing fades before the first swap: {detail}");
        let (holds, _) =
            predicate(&world, &json!({ "predicate": "ui.theme_fading", "is": false })).expect("atom");
        assert!(holds);
        let err = predicate(&world, &json!({ "predicate": "ui.theme_fading", "is": "no" })).expect_err("bad");
        assert!(err.starts_with("bad_flag"), "{err}");
    }
}
