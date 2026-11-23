// SPDX-License-Identifier: MPL-2.0
//! The in-game performance overlay on the agent protocol (`book/agents/perf-overlay.md`, guide
//! topic `perf_overlay`). `perf_overlay.get` returns the active mode, where it came from and a
//! fresh snapshot of every number the overlay shows (from the engine sources, so an agent reads
//! what a tester sees); `perf_overlay.toggle` shows, hides or cycles it.

use regia_app::prelude::*;
use regia_framework::perf_overlay::{
    PerfOverlayState, PerformanceOverlay, PerformanceOverlayMode, PerformanceOverlayPlacement, active_mode,
    set_override, set_player_mode, snapshot,
};
use regia_framework::settings::GameSettings;
use serde_json::{Value, json};

use crate::protocol::RpcError;

const MODES: &str = "\"off\", \"compact\", \"detailed\" or \"default\" (drop the override)";

fn state(world: &World) -> Value {
    let (mode, source) = active_mode(world);
    let host_default = world
        .get_resource::<PerformanceOverlay>()
        .map_or(PerformanceOverlayMode::Off, |o| o.default);
    json!({
        "mode": mode.name(),
        "source": source.name(),
        "host_default": host_default.name(),
        "setting": world
            .get_resource::<GameSettings>()
            .map(|s| s.performance_overlay.map_or("default", PerformanceOverlayMode::name)),
        "shown": world
            .get_resource::<PerfOverlayState>()
            .map_or("Off", |s| s.shown().name()),
        "plugin": world.contains_resource::<PerformanceOverlay>(),
        "placement": world.get_resource::<PerformanceOverlayPlacement>().copied().unwrap_or_default().normalized(),
    })
}

/// `perf_overlay.get`: the mode and its source plus `stats`, a fresh snapshot of the values
/// the overlay shows. The window statistics (p50, stutters) cover the last ~5 s while the
/// overlay is visible and are 0 while it is hidden.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let mut out = state(world);
    let stats = snapshot(world);
    out["stats"] = serde_json::to_value(&stats).unwrap_or(Value::Null);
    out["top_passes"] = json!(stats.top_passes(5));
    Ok(out)
}

/// `perf_overlay.toggle { mode?, persist? }`: `mode` = off / compact / detailed / default;
/// without it the mode cycles Off → Compact → Detailed → Off. By default the change is a runtime
/// override (not saved); `persist: true` writes it as the player's setting
/// (`GameSettings.performance_overlay`, saved by the settings plugin).
pub(crate) fn toggle(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<PerformanceOverlay>() {
        return Err(RpcError::not_found(
            "the performance overlay plugin is not installed",
            "add regia::DefaultPlugins (feature `ui`) or regia_framework::perf_overlay::PerfOverlayPlugin",
        ));
    }
    let persist = match params.get("persist") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(RpcError::invalid_params("`persist` must be a boolean")),
    };
    let requested = match params.get("mode") {
        None | Some(Value::Null) if params.get("placement").is_some() => None,
        None | Some(Value::Null) => Some(active_mode(world).0.next()),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("default") => None,
        Some(Value::String(s)) => Some(
            PerformanceOverlayMode::from_name(s)
                .ok_or_else(|| RpcError::invalid_params(format!("unknown mode {s:?}: use {MODES}")))?,
        ),
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "`mode` must be a string: {MODES}"
            )));
        }
    };
    let placement = params.get("placement").map(|value| {
        let placement: PerformanceOverlayPlacement = serde_json::from_value(value.clone())
            .map_err(|_| RpcError::invalid_params("`placement` must be {corner: top_left|top_right|bottom_left|bottom_right, offset: [x, y]}"))?;
        if placement.offset.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Err(RpcError::invalid_params("placement offsets must be finite and nonnegative"));
        }
        Ok(placement)
    }).transpose()?;
    let before = state(world);
    if let Some(placement) = placement {
        world.insert_resource(placement);
    }
    let placement_only = params.get("placement").is_some() && params.get("mode").is_none_or(Value::is_null);
    if !placement_only {
        match (requested, persist) {
            (Some(mode), true) => set_player_mode(world, mode),
            (Some(mode), false) => set_override(world, Some(mode)),
            (None, true) => {
                set_override(world, None);
                if let Some(mut s) = world.get_resource_mut::<GameSettings>() {
                    s.performance_overlay = None;
                }
            }
            (None, false) => set_override(world, None),
        }
    }
    Ok(json!({
        "before": before,
        "after": state(world),
        "note": "drawn from the next frame; the window statistics fill over ~5 s",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(PerformanceOverlay::default());
        world
    }

    #[test]
    fn toggle_cycles_and_overrides_without_persisting() {
        let mut world = world();
        world.insert_resource(GameSettings::default());
        let out = toggle(&mut world, &json!({})).expect("toggle");
        assert_eq!(out["before"]["mode"], "Off");
        assert_eq!(out["after"]["mode"], "Compact");
        assert_eq!(out["after"]["source"], "override");
        assert_eq!(world.resource::<GameSettings>().performance_overlay, None);
        toggle(&mut world, &json!({ "mode": "detailed", "persist": true })).expect("persist");
        assert_eq!(
            world.resource::<GameSettings>().performance_overlay,
            Some(PerformanceOverlayMode::Detailed)
        );
        let got = get(&mut world).expect("get");
        assert_eq!(got["mode"], "Detailed");
        assert_eq!(got["source"], "player");
        assert_eq!(got["setting"], "Detailed");
        assert!(got["stats"].is_object());
    }

    #[test]
    fn host_default_applies_until_the_player_chooses() {
        let mut world = World::new();
        world.insert_resource(PerformanceOverlay::default_mode(PerformanceOverlayMode::Compact));
        world.insert_resource(GameSettings::default());
        let got = get(&mut world).expect("get");
        assert_eq!(got["mode"], "Compact");
        assert_eq!(got["source"], "host_default");
        toggle(&mut world, &json!({ "mode": "off", "persist": true })).expect("off");
        assert_eq!(get(&mut world).expect("get")["mode"], "Off");
    }

    #[test]
    fn rejects_bad_params_and_a_missing_plugin() {
        let mut world = world();
        assert!(toggle(&mut world, &json!({ "mode": "huge" })).is_err());
        assert!(toggle(&mut world, &json!({ "persist": "yes" })).is_err());
        assert!(toggle(&mut World::new(), &json!({})).is_err());
    }
}
