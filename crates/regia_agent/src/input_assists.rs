// SPDX-License-Identifier: MPL-2.0
//! `input.assists`: read or change the player's per-action input assists (hold or toggle, a
//! minimum hold time, auto-repeat, an input cool-down; book `agents/input-assists.md`, ADR-2646,
//! ADR-3464).
//!
//! A change writes `GameSettings.accessibility.input_assists` (and the per-player or per-gamepad
//! maps) and the live `InputAssists` resource at once.

use std::collections::BTreeMap;

use regia_ecs::prelude::*;
use regia_framework::settings::GameSettings;
use regia_input::{ActionAssist, Actions, InputAssists, InputConfig, PressMode, RepeatRate};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// One assist as the method takes it. Every field is optional; a missing one is the neutral value.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssistParams {
    mode: Option<String>,
    min_hold_secs: Option<f32>,
    repeat: Option<RepeatParams>,
    /// Evaluation steps that must pass after an accepted press. Missing means `0`.
    cooldown_steps: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatParams {
    delay_secs: f32,
    interval_secs: f32,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Params {
    /// Action name → assist; `null` removes the action's assist.
    #[serde(default)]
    set: BTreeMap<String, Option<AssistParams>>,
    /// Remove every assist of the chosen set first.
    #[serde(default)]
    clear: bool,
    /// Local player whose overlay this call reads and writes. Not with `gamepad`.
    #[serde(default)]
    player: Option<u8>,
    /// Gamepad device id whose overlay this call reads and writes. Not with `player`.
    #[serde(default)]
    gamepad: Option<u64>,
}

fn bad_params(detail: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "input_assist_bad_params",
        format!("bad `input.assists` parameters: {detail}"),
        "pass { set: { \"<action>\": { mode: \"toggle\", min_hold_secs: 0.25, repeat: { delay_secs: 0.5, interval_secs: 0.1 }, cooldown_steps: 4 } | null }, clear?, player? | gamepad? }",
    )
}

fn bad_value(action: &str, what: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "input_assist_bad_value",
        format!("`{action}`: {what}"),
        format!(
            "mode is \"hold\" or \"toggle\"; min_hold_secs and delay_secs are 0 to {max}; interval_secs is {min} to {max}; cooldown_steps is 0 to {cool}",
            cool = ActionAssist::MAX_COOLDOWN_STEPS,
            max = ActionAssist::MAX_SECS,
            min = RepeatRate::MIN_INTERVAL_SECS
        ),
    )
}

fn unknown_action(action: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "input_assist_unknown_action",
        format!("no action `{action}` is declared in the input config"),
        "input.rebind_state lists the declared actions",
    )
}

fn in_range(v: f32, lo: f32) -> bool {
    v.is_finite() && (lo..=ActionAssist::MAX_SECS).contains(&v)
}

fn to_assist(action: &str, p: AssistParams) -> Result<ActionAssist, RpcError> {
    let mode = match p.mode.as_deref() {
        None => PressMode::Hold,
        Some(text) => PressMode::parse(text).ok_or_else(|| bad_value(action, "unknown mode"))?,
    };
    let min_hold_secs = p.min_hold_secs.unwrap_or(0.0);
    if !in_range(min_hold_secs, 0.0) {
        return Err(bad_value(action, "min_hold_secs out of range"));
    }
    let repeat = match p.repeat {
        None => None,
        Some(r) => {
            if !in_range(r.delay_secs, 0.0) || !in_range(r.interval_secs, RepeatRate::MIN_INTERVAL_SECS) {
                return Err(bad_value(action, "repeat out of range"));
            }
            Some(RepeatRate {
                delay_secs: r.delay_secs,
                interval_secs: r.interval_secs,
            })
        }
    };
    let cooldown_steps = p.cooldown_steps.unwrap_or(0);
    if cooldown_steps > ActionAssist::MAX_COOLDOWN_STEPS {
        return Err(bad_value(action, "cooldown_steps out of range"));
    }
    Ok(ActionAssist {
        mode,
        min_hold_secs,
        repeat,
        cooldown_steps,
    })
}

fn assist_json(assist: &ActionAssist) -> Value {
    json!({
        "mode": assist.mode.as_str(),
        "min_hold_secs": assist.min_hold_secs,
        "repeat": assist.repeat.map(|r| json!({
            "delay_secs": r.delay_secs,
            "interval_secs": r.interval_secs,
        })),
        "cooldown_steps": assist.cooldown_steps,
    })
}

fn keyed_json<K: std::fmt::Display>(map: &BTreeMap<K, BTreeMap<String, ActionAssist>>) -> Value {
    let mut out = serde_json::Map::new();
    for (key, actions) in map {
        let mut inner = serde_json::Map::new();
        for (name, assist) in actions {
            inner.insert(name.clone(), assist_json(assist));
        }
        out.insert(key.to_string(), Value::Object(inner));
    }
    Value::Object(out)
}

fn snapshot(world: &World) -> Value {
    let live = world.get_resource::<InputAssists>();
    let assists: serde_json::Map<String, Value> = live
        .map(|l| {
            l.actions
                .iter()
                .map(|(name, a)| (name.clone(), assist_json(a)))
                .collect()
        })
        .unwrap_or_default();
    let toggled_on: Vec<&str> = world
        .get_resource::<Actions>()
        .map(|actions| {
            actions
                .player(0)
                .states
                .iter()
                .filter(|(_, s)| s.toggled_on())
                .map(|(name, _)| name.as_str())
                .collect()
        })
        .unwrap_or_default();
    json!({
        "assists": assists,
        "players": live.map(|l| keyed_json(&l.players)).unwrap_or(json!({})),
        "gamepads": live.map(|l| keyed_json(&l.gamepads)).unwrap_or(json!({})),
        "toggled_on": toggled_on,
        "live": live.is_some(),
        "persisted": world.contains_resource::<GameSettings>(),
    })
}

/// `input.assists { set?, clear? }`: with no parameters it only reports.
pub(crate) fn assists(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let params: Params = match params {
        Value::Null => Params::default(),
        Value::Object(_) => serde_json::from_value(params.clone()).map_err(bad_params)?,
        _ => return Err(bad_params("expected an object")),
    };
    if params.player.is_some() && params.gamepad.is_some() {
        return Err(bad_params("pass player or gamepad, not both"));
    }
    // Validate everything before changing anything.
    let declared = world.get_resource::<InputConfig>().map(|config| {
        config
            .actions
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    });
    let mut changes = Vec::with_capacity(params.set.len());
    for (action, assist) in params.set {
        if let Some(declared) = &declared
            && !declared.contains(&action)
        {
            return Err(unknown_action(&action));
        }
        let assist = match assist {
            Some(p) => to_assist(&action, p)?,
            None => ActionAssist::default(),
        };
        changes.push((action, assist));
    }
    if params.clear || !changes.is_empty() {
        // The settings apply copies saved assists into `InputAssists` only when the settings
        // change. On the baseline frame the live maps stay empty while the save still holds the
        // player's assists, so start from those maps before a write replaces them.
        let saved = world.get_resource::<GameSettings>().map(|settings| {
            (
                settings.accessibility.input_assists.clone(),
                settings.accessibility.input_assists_by_player.clone(),
                settings.accessibility.input_assists_by_gamepad.clone(),
            )
        });
        let mut live = world.get_resource_or_insert_with(InputAssists::default);
        if let Some((actions, players, gamepads)) = saved {
            if live.actions.is_empty() {
                live.actions = actions;
            }
            if live.players.is_empty() {
                live.players = players;
            }
            if live.gamepads.is_empty() {
                live.gamepads = gamepads;
            }
        }
        if let Some(player) = params.player {
            if params.clear {
                live.clear_player(player);
            }
            for (action, assist) in &changes {
                live.set_player(player, action.clone(), *assist);
            }
        } else if let Some(device) = params.gamepad {
            if params.clear {
                live.clear_gamepad(device);
            }
            for (action, assist) in &changes {
                live.set_gamepad(device, action.clone(), *assist);
            }
        } else {
            if params.clear {
                live.actions.clear();
            }
            for (action, assist) in &changes {
                live.set(action.clone(), *assist);
            }
        }
        let actions = live.actions.clone();
        let players = live.players.clone();
        let gamepads = live.gamepads.clone();
        if let Some(mut settings) = world.get_resource_mut::<GameSettings>() {
            if settings.accessibility.input_assists != actions {
                settings.accessibility.input_assists = actions;
            }
            if settings.accessibility.input_assists_by_player != players {
                settings.accessibility.input_assists_by_player = players;
            }
            if settings.accessibility.input_assists_by_gamepad != gamepads {
                settings.accessibility.input_assists_by_gamepad = gamepads;
            }
        }
    }
    Ok(snapshot(world))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: RpcError) -> Value {
        err.data.expect("data")["code"].clone()
    }

    #[test]
    fn reports_an_empty_set_without_changing_anything() {
        let mut world = World::new();
        let out = assists(&mut world, &json!({})).expect("report");
        assert_eq!(out["assists"], json!({}));
        assert_eq!(out["live"], json!(false));
        assert_eq!(out["persisted"], json!(false));
        assert!(world.get_resource::<InputAssists>().is_none());
    }

    #[test]
    fn sets_live_and_persisted_assists_and_removes_with_null() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        let out = assists(
            &mut world,
            &json!({ "set": {
                "Crouch": { "mode": "toggle" },
                "Fire": { "repeat": { "delay_secs": 0.4, "interval_secs": 0.1 } },
                "Jump": { "min_hold_secs": 0.2 },
            }}),
        )
        .expect("set");
        assert_eq!(out["assists"]["Crouch"]["mode"], json!("toggle"));
        assert_eq!(out["assists"]["Fire"]["repeat"]["interval_secs"], json!(0.1f32));
        assert_eq!(out["persisted"], json!(true));
        let settings = world.resource::<GameSettings>();
        assert_eq!(settings.accessibility.input_assists.len(), 3);
        assert_eq!(
            settings.accessibility.input_assists["Crouch"].mode,
            PressMode::Toggle
        );
        // `null` removes one; a neutral assist removes it too.
        let out = assists(
            &mut world,
            &json!({ "set": { "Crouch": null, "Jump": { "mode": "hold" } } }),
        )
        .expect("set");
        assert_eq!(out["assists"].as_object().map(|o| o.len()), Some(1));
        assert_eq!(
            world.resource::<GameSettings>().accessibility.input_assists.len(),
            1
        );
        // `clear` empties the set.
        let out = assists(&mut world, &json!({ "clear": true })).expect("clear");
        assert_eq!(out["assists"], json!({}));
        assert!(
            world
                .resource::<GameSettings>()
                .accessibility
                .input_assists
                .is_empty()
        );
    }

    #[test]
    fn rejects_bad_shapes_values_and_unknown_actions_and_changes_nothing() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        world.insert_resource(InputConfig::new().action("Jump", regia_input::ActionKind::Button));
        for (bad, want) in [
            (
                json!({ "set": { "Jump": { "mode": "tap" } } }),
                "input_assist_bad_value",
            ),
            (
                json!({ "set": { "Jump": { "min_hold_secs": 9.0 } } }),
                "input_assist_bad_value",
            ),
            (
                json!({ "set": { "Jump": { "repeat": { "delay_secs": 0.1, "interval_secs": 0.0 } } } }),
                "input_assist_bad_value",
            ),
            (
                json!({ "set": { "Jump": { "speed": 1 } } }),
                "input_assist_bad_params",
            ),
            (json!({ "sett": {} }), "input_assist_bad_params"),
            (
                json!({ "set": { "Jump": { "min_hold_secs": "long" } } }),
                "input_assist_bad_params",
            ),
            (
                json!({ "set": { "Fly": { "mode": "toggle" } } }),
                "input_assist_unknown_action",
            ),
        ] {
            let err = assists(&mut world, &bad).expect_err("bad");
            assert_eq!(code(err), json!(want), "{bad}");
        }
        // A valid entry next to a bad one is not applied either.
        let err = assists(
            &mut world,
            &json!({ "set": { "Jump": { "mode": "toggle" }, "Fly": {} } }),
        )
        .expect_err("unknown");
        assert_eq!(code(err), json!("input_assist_unknown_action"));
        assert!(
            world
                .resource::<GameSettings>()
                .accessibility
                .input_assists
                .is_empty()
        );
        assert!(world.get_resource::<InputAssists>().is_none());
    }

    #[test]
    fn assists_set_cooldown_and_a_per_player_override() {
        let mut world = World::new();
        world.insert_resource(GameSettings::default());
        let out = assists(
            &mut world,
            &json!({ "set": { "Crouch": { "mode": "toggle", "cooldown_steps": 4 } } }),
        )
        .expect("default set");
        assert_eq!(out["assists"]["Crouch"]["cooldown_steps"], json!(4));
        let out = assists(
            &mut world,
            &json!({ "player": 1, "set": { "Crouch": { "mode": "hold", "cooldown_steps": 8 } } }),
        )
        .expect("player set");
        assert_eq!(out["players"]["1"]["Crouch"]["cooldown_steps"], json!(8));
        assert_eq!(out["assists"]["Crouch"]["mode"], json!("toggle"));
        let settings = world.resource::<GameSettings>();
        assert_eq!(settings.accessibility.input_assists["Crouch"].cooldown_steps, 4);
        assert_eq!(
            settings.accessibility.input_assists_by_player[&1]["Crouch"].cooldown_steps,
            8
        );
        let err = assists(
            &mut world,
            &json!({ "player": 1, "gamepad": 3, "set": { "Crouch": { "mode": "toggle" } } }),
        )
        .expect_err("both keys");
        assert_eq!(code(err), json!("input_assist_bad_params"));
        let err = assists(
            &mut world,
            &json!({ "set": { "Crouch": { "cooldown_steps": 1000 } } }),
        )
        .expect_err("too long");
        assert_eq!(code(err), json!("input_assist_bad_value"));
        // The failed calls left the stored assist alone.
        assert_eq!(
            world.resource::<GameSettings>().accessibility.input_assists["Crouch"].cooldown_steps,
            4
        );
        let out = assists(&mut world, &json!({ "player": 1, "clear": true })).expect("clear player");
        assert_eq!(out["players"], json!({}));
        assert_eq!(out["assists"]["Crouch"]["mode"], json!("toggle"));
    }

    #[test]
    fn set_keeps_saved_assists_the_live_map_has_not_copied() {
        let mut world = World::new();
        let mut settings = GameSettings::default();
        settings.accessibility.input_assists.insert(
            "Crouch".into(),
            ActionAssist {
                mode: PressMode::Toggle,
                ..Default::default()
            },
        );
        let mut jump = std::collections::BTreeMap::new();
        jump.insert(
            "Jump".into(),
            ActionAssist {
                mode: PressMode::Hold,
                ..Default::default()
            },
        );
        settings.accessibility.input_assists_by_player.insert(1, jump);
        world.insert_resource(settings);
        world.insert_resource(InputAssists::default());

        let out = assists(&mut world, &json!({ "set": { "Fire": { "mode": "toggle" } } })).expect("set");
        assert_eq!(out["assists"]["Crouch"]["mode"], json!("toggle"));
        assert_eq!(out["assists"]["Fire"]["mode"], json!("toggle"));
        assert_eq!(out["players"]["1"]["Jump"]["mode"], json!("hold"));
        let saved = &world.resource::<GameSettings>().accessibility;
        assert!(saved.input_assists.contains_key("Crouch"));
        assert!(saved.input_assists.contains_key("Fire"));
        assert!(saved.input_assists_by_player[&1].contains_key("Jump"));

        let out = assists(
            &mut world,
            &json!({ "player": 1, "set": { "Aim": { "mode": "toggle" } } }),
        )
        .expect("player set");
        assert_eq!(out["assists"]["Crouch"]["mode"], json!("toggle"), "{out}");
        assert!(
            world
                .resource::<GameSettings>()
                .accessibility
                .input_assists
                .contains_key("Crouch"),
            "a player overlay must not wipe the saved default set"
        );
        assert!(
            world
                .resource::<GameSettings>()
                .accessibility
                .input_assists_by_player[&1]
                .contains_key("Jump")
        );
    }
}
