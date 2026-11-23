// SPDX-License-Identifier: MPL-2.0
//! `cutscene.start`, `cutscene.skip`, `cutscene.stop`, `cutscene.state`: the cutscene flow around
//! a sequence player (`regia_framework::sequencer::cutscene`, ADR-2948; book `agents/cutscene.md`,
//! guide topic `cutscene`).
//!
//! A cutscene plays a `SequencePlayer` (see `sequence.player`) with gameplay input locked, the
//! HUD hidden, letterbox bars, a skip action and a hand-back through the camera takeover. Calls
//! land at a frame boundary; the cutscene system applies a skip's jump on the next frame.

use regia_app::prelude::*;
use regia_framework::sequencer::{
    CutsceneError, CutsceneSettings, Cutscenes, SequencePlayer, SkipSettings, cutscene_state, skip_cutscene,
    start_cutscene, stop_cutscene,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

const HINT: &str = "see book/agents/cutscene.md for the parameters";

fn bad(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, HINT)
}

fn present<'a>(params: &'a Value, field: &str) -> Option<&'a Value> {
    params.get(field).filter(|v| !v.is_null())
}

fn cutscene_error(error: CutsceneError) -> RpcError {
    let class = match error {
        CutsceneError::NoPlayer(_) | CutsceneError::NotRunning(_) | CutsceneError::UnknownName(_) => {
            NOT_FOUND
        }
        _ => INVALID_PARAMS,
    };
    RpcError::new(class, error.code(), error.to_string(), HINT)
}

/// The player a call names, or the only sequence player.
fn player(world: &mut World, params: &Value, running_only: bool) -> Result<Entity, RpcError> {
    if let Some(v) = present(params, "player") {
        return v
            .as_u64()
            .and_then(Entity::try_from_bits)
            .ok_or_else(|| bad("bad_entity", "`player` must be an entity id"));
    }
    let candidates: Vec<Entity> = if running_only {
        world
            .get_resource::<Cutscenes>()
            .map(|c| c.players().collect())
            .unwrap_or_default()
    } else {
        let mut query = world.query_filtered::<Entity, With<SequencePlayer>>();
        let mut list: Vec<Entity> = query.iter(world).collect();
        list.sort_unstable_by_key(|e| e.to_bits());
        list
    };
    match candidates.as_slice() {
        [only] => Ok(*only),
        [] if running_only => Err(cutscene_error(CutsceneError::NotRunning(Entity::PLACEHOLDER))),
        [] => Err(RpcError::new(
            NOT_FOUND,
            "cutscene_no_player",
            "no sequence player exists",
            "sequence.player { \"action\": \"load\", ... } creates one",
        )),
        many => Err(bad(
            "cutscene_several",
            format!("{} candidates exist; pass `player`", many.len()),
        )),
    }
}

fn f32_field(params: &Value, field: &str, default: f32) -> Result<f32, RpcError> {
    match present(params, field) {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| f as f32)
            .ok_or_else(|| bad("cutscene_bad_settings", format!("`{field}` must be a number"))),
    }
}

fn bool_field(params: &Value, field: &str, default: bool) -> Result<bool, RpcError> {
    match present(params, field) {
        None => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| {
            bad(
                "cutscene_bad_settings",
                format!("`{field}` must be true or false"),
            )
        }),
    }
}

fn string_field(params: &Value, field: &str, default: &str) -> Result<String, RpcError> {
    match present(params, field) {
        None => Ok(default.to_string()),
        Some(v) => v
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .map(str::to_string)
            .ok_or_else(|| {
                bad(
                    "cutscene_bad_settings",
                    format!("`{field}` must be a non-empty string"),
                )
            }),
    }
}

/// The settings of a `cutscene.start` (every field optional; the defaults are
/// `CutsceneSettings::default()`).
fn settings_from(params: &Value) -> Result<CutsceneSettings, RpcError> {
    let d = CutsceneSettings::default();
    let players = match present(params, "players") {
        None => d.players.clone(),
        Some(v) => v
            .as_array()
            .filter(|a| a.len() <= 8)
            .and_then(|a| {
                a.iter()
                    .map(|p| p.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect::<Option<Vec<u8>>>()
            })
            .ok_or_else(|| {
                bad(
                    "cutscene_bad_settings",
                    "`players` must be up to 8 player numbers 0..=255",
                )
            })?,
    };
    let hide_hud = match present(params, "hide_hud") {
        None => d.hide_hud.clone(),
        Some(v) => v
            .as_array()
            .filter(|a| a.len() <= 64)
            .and_then(|a| {
                a.iter()
                    .map(|n| n.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                    .collect::<Option<Vec<String>>>()
            })
            .ok_or_else(|| bad("cutscene_bad_settings", "`hide_hud` must be up to 64 layer names"))?,
    };
    let skip = present(params, "skip").cloned().unwrap_or(Value::Null);
    let skip = SkipSettings {
        enabled: bool_field(&skip, "enabled", d.skip.enabled)?,
        action: string_field(&skip, "action", &d.skip.action)?,
        hold_secs: f32_field(&skip, "hold_secs", d.skip.hold_secs)?,
        fade_secs: f32_field(&skip, "fade_secs", d.skip.fade_secs)?,
    };
    let hand_back_blend = match present(params, "hand_back_blend") {
        None => None,
        Some(_) => Some(f32_field(params, "hand_back_blend", 0.0)?),
    };
    let settings = CutsceneSettings {
        lock_input: bool_field(params, "lock_input", d.lock_input)?,
        players,
        input_context: string_field(params, "input_context", &d.input_context)?,
        letterbox: f32_field(params, "letterbox", d.letterbox)?,
        letterbox_secs: f32_field(params, "letterbox_secs", d.letterbox_secs)?,
        hide_hud,
        skip,
        hand_back_blend,
        stop_dialogue_on_skip: bool_field(params, "stop_dialogue_on_skip", d.stop_dialogue_on_skip)?,
        dialogue_fade_ms: d.dialogue_fade_ms,
    };
    settings.validate().map_err(cutscene_error)?;
    Ok(settings)
}

fn state_json(world: &World, player: Entity) -> Value {
    let Some(state) = cutscene_state(world, player) else {
        return json!({ "player": player.to_bits(), "running": false });
    };
    let p = world.get::<SequencePlayer>(player);
    json!({
        "player": player.to_bits(),
        "running": true,
        "phase": state.phase.name(),
        "skipped": state.skipped,
        "skip_held": state.skip_held,
        "skip_hold_secs": state.settings.skip.hold_secs,
        "skip_enabled": state.settings.skip.enabled,
        "skip_action": state.settings.skip.action,
        "fade": state.fade,
        "letterbox": state.letterbox,
        "letterbox_height": state.settings.letterbox,
        "input_locked": state.input_locked,
        "input_context": state.settings.input_context,
        "players": state.settings.players,
        "hud_hidden": state.hud_hidden,
        "start_tick": state.start_tick,
        "time": p.map(SequencePlayer::time),
        "duration": p.map(SequencePlayer::duration),
        "status": p.map(|p| p.status().name()),
        "prerolling": p.map(SequencePlayer::is_prerolling),
        "markers_skipped": p.map(SequencePlayer::markers_skipped),
    })
}

/// `cutscene.start { player?, lock_input?, players?, input_context?, letterbox?, letterbox_secs?,
/// hide_hud?, skip?: { enabled?, action?, hold_secs?, fade_secs? }, hand_back_blend?,
/// stop_dialogue_on_skip? }`.
pub(crate) fn start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let settings = settings_from(params)?;
    let player = player(world, params, false)?;
    start_cutscene(world, player, settings).map_err(cutscene_error)?;
    Ok(state_json(world, player))
}

/// `cutscene.skip { player? }`: skips as the skip action would (with its fade).
pub(crate) fn skip(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let player = player(world, params, true)?;
    let began = skip_cutscene(world, player).map_err(cutscene_error)?;
    let mut out = state_json(world, player);
    if let Some(object) = out.as_object_mut() {
        object.insert("began".into(), json!(began));
    }
    Ok(out)
}

/// `cutscene.stop { player? }`: stops at once (input, HUD and camera back; also in the pre-roll).
pub(crate) fn stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let player = player(world, params, true)?;
    stop_cutscene(world, player).map_err(cutscene_error)?;
    Ok(state_json(world, player))
}

/// `cutscene.state { player? }`: one cutscene's state, or every running one.
pub(crate) fn state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if present(params, "player").is_some() {
        let player = player(world, params, false)?;
        return Ok(state_json(world, player));
    }
    let players: Vec<Entity> = world
        .get_resource::<Cutscenes>()
        .map(|c| c.players().collect())
        .unwrap_or_default();
    let (started, skipped) = world
        .get_resource::<Cutscenes>()
        .map_or((0, 0), Cutscenes::counts);
    let list: Vec<Value> = players.iter().map(|&p| state_json(world, p)).collect();
    Ok(json!({ "count": list.len(), "cutscenes": list, "started": started, "skipped": skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::sequencer::{Sequence, SequencerPlugin};
    use std::time::Duration;

    fn app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)))
            .init_resource::<regia_input::InputContexts>();
        app.world_mut()
            .resource_mut::<regia_input::InputContexts>()
            .push(0, "Gameplay");
        let seq =
            Sequence::from_ron(r#"(version: 5, markers: [(name: "a", time: 0.2), (name: "b", time: 1.0)])"#)
                .expect("valid");
        let player = app.world_mut().spawn(SequencePlayer::new(seq)).id();
        app.update();
        (app, player)
    }

    fn code(e: RpcError) -> String {
        e.data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    #[test]
    fn start_skip_state_and_stop_round_trip() {
        let (mut app, player) = app();
        let w = app.world_mut();
        let out = start(w, &json!({ "letterbox": 0.1, "skip": { "hold_secs": 0.0 } })).expect("start");
        assert_eq!(out["running"], true);
        assert_eq!(out["input_locked"], true);
        assert_eq!(code(start(w, &json!({})).expect_err("twice")), "cutscene_running");
        app.update();
        let out = skip(app.world_mut(), &json!({})).expect("skip");
        assert_eq!(out["began"], true);
        assert_eq!(out["skipped"], true);
        for _ in 0..10 {
            app.update();
        }
        let all = state(app.world_mut(), &json!({})).expect("state");
        assert_eq!(all["count"], 0, "handed back");
        assert_eq!(all["skipped"], 1);
        let w = app.world_mut();
        assert_eq!(
            code(skip(w, &json!({})).expect_err("none")),
            "cutscene_not_running"
        );
        start(w, &json!({ "player": player.to_bits() })).expect("again");
        let out = stop(w, &json!({})).expect("stop");
        assert_eq!(out["running"], false);
        assert!(
            w.resource::<regia_input::InputContexts>()
                .is_enabled(0, "Gameplay")
        );
    }

    #[test]
    fn bad_settings_are_refused_with_codes() {
        let (mut app, _) = app();
        let w = app.world_mut();
        assert_eq!(
            code(start(w, &json!({ "letterbox": 0.9 })).expect_err("big")),
            "cutscene_bad_settings"
        );
        assert_eq!(
            code(start(w, &json!({ "players": [300] })).expect_err("player")),
            "cutscene_bad_settings"
        );
        assert_eq!(
            code(start(w, &json!({ "skip": { "fade_secs": -1 } })).expect_err("fade")),
            "cutscene_bad_settings"
        );
        assert_eq!(
            code(start(w, &json!({ "player": 12345678 })).expect_err("nobody")),
            "cutscene_no_player"
        );
        assert_eq!(
            code(start(w, &json!({ "player": "x" })).expect_err("type")),
            "bad_entity"
        );
        let out = state(w, &json!({})).expect("state");
        assert_eq!(out["count"], 0);
    }
}
