// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the game pause (ADR-10104; `book/agents/game-pause.md`): `game.pause`,
//! `game.resume` and `game.pause_state`.
//!
//! `game.pause` pauses the way the player's pause key does (gameplay time frozen, gameplay input
//! ignored, cursor released, the game's pause screen shown); `play.pause` stays the raw time freeze
//! for tests and shows nothing. Errors are `F0002` with `data.reason`: `game_pause_not_installed` (no
//! `GamePausePlugin` on this process), `game_pause_off` (the game did not turn the pause on) and
//! `networked_session` (a networked session runs: a peer cannot freeze the other players' worlds).
//! A render-out owns the clock: both writes answer `F0018` while one runs.

use regia_app::prelude::*;
use regia_framework::game_pause::{
    GamePause, PauseRefusal, PauseSource, TOGGLE_PAUSE_ACTION, gamepad_button_name, networked_session,
    pause_binding_label, pause_now, pause_refusal, resume_now,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn reason(reason: &str, message: &str, hint: &str) -> RpcError {
    let mut r = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = r.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    r
}

fn not_installed() -> RpcError {
    reason(
        "game_pause_not_installed",
        "the game pause is not installed on this process",
        "regia::DefaultPlugins adds GamePausePlugin; a custom app adds regia_framework::game_pause::GamePausePlugin",
    )
}

fn refusal(r: PauseRefusal) -> RpcError {
    let hint = match r {
        PauseRefusal::Disabled => "set `[game] pause = true` in Regia.toml (or GamePauseConfig::enabled())",
        PauseRefusal::NetworkedSession => "play.pause freezes this process only, for tests",
    };
    reason(r.code(), r.message(), hint)
}

/// A render-out owns the clock until it ends (same rule as `play.pause`).
fn refuse_during_render_out(world: &World) -> Result<(), RpcError> {
    if crate::render_out::job_running(world) {
        return Err(RpcError::new(
            crate::protocol::INVALID_REQUEST,
            "F0018",
            "a render-out is running",
            "render.out_progress shows it; render.out_cancel stops it",
        ));
    }
    Ok(())
}

fn frame(world: &World) -> u64 {
    world.get_resource::<Time>().map_or(0, Time::frame_count)
}

/// `game.pause`: pauses like the pause key. `changed` is false when the game was already paused.
pub(crate) fn pause(world: &mut World) -> Result<Value, RpcError> {
    refuse_during_render_out(world)?;
    if !world.contains_resource::<GamePause>() {
        return Err(not_installed());
    }
    let changed = pause_now(world, PauseSource::Agent).map_err(refusal)?;
    let screen = world.resource::<GamePause>().screen_shown();
    Ok(json!({ "paused": true, "changed": changed, "frame": frame(world), "screen": screen }))
}

/// `game.resume`: ends the pause. `changed` is false when the game was not paused.
pub(crate) fn resume(world: &mut World) -> Result<Value, RpcError> {
    refuse_during_render_out(world)?;
    if !world.contains_resource::<GamePause>() {
        return Err(not_installed());
    }
    let changed = resume_now(world, PauseSource::Agent);
    Ok(json!({ "paused": false, "changed": changed, "frame": frame(world) }))
}

/// `game.pause_state`: the settings, the state and what a pause would do now.
pub(crate) fn pause_state(world: &mut World) -> Result<Value, RpcError> {
    let Some(pause) = world.get_resource::<GamePause>() else {
        return Err(not_installed());
    };
    let config = pause.config();
    let (pauses, resumes, refusals) = pause.counts();
    let would_refuse = if pause.is_paused() {
        None
    } else {
        pause_refusal(world).map(PauseRefusal::code)
    };
    Ok(json!({
        "enabled": config.enabled,
        "paused": pause.is_paused(),
        "source": pause.source().map(PauseSource::name),
        "since_frame": pause.since_frame(),
        "frame": frame(world),
        "time_paused": world.get_resource::<Time>().is_some_and(|t| t.paused),
        "action": TOGGLE_PAUSE_ACTION,
        "key": config.key,
        "gamepad": config.gamepad.map(gamepad_button_name),
        "binding_label": pause_binding_label(world),
        "screen": config.screen.kind(),
        "screen_setting": config.screen.setting(),
        "screen_shown": pause.screen_shown(),
        "screen_error": pause.screen_error(),
        "networked": networked_session(world),
        "would_refuse": would_refuse,
        "last_refusal": pause.last_refusal().map(PauseRefusal::code),
        "pauses": pauses,
        "resumes": resumes,
        "refusals": refusals,
    }))
}

#[cfg(test)]
mod tests {
    use regia_framework::game_pause::{GamePauseConfig, GamePausePlugin, NetworkedSession};

    use super::*;

    fn reason_of(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn app(config: GamePauseConfig) -> App {
        let mut app = App::new();
        app.add_plugins(GamePausePlugin {
            config: Some(config),
            ..Default::default()
        });
        app.update();
        app
    }

    #[test]
    fn pause_resume_and_state() {
        let mut app = app(GamePauseConfig::enabled());
        let state = pause_state(app.world_mut()).expect("state");
        assert_eq!(state["enabled"], true);
        assert_eq!(state["paused"], false);
        assert_eq!(state["key"], "Pause");
        assert_eq!(state["gamepad"], Value::Null);
        assert_eq!(state["screen"], "default");
        assert_eq!(state["would_refuse"], Value::Null);
        let paused = pause(app.world_mut()).expect("pauses");
        assert_eq!(paused["changed"], true);
        assert_eq!(paused["screen"], "default");
        assert!(app.world().resource::<Time>().paused);
        let again = pause(app.world_mut()).expect("idempotent");
        assert_eq!(again["changed"], false);
        let state = pause_state(app.world_mut()).expect("state");
        assert_eq!(state["paused"], true);
        assert_eq!(state["source"], "agent");
        assert_eq!(state["screen_shown"], "default");
        assert_eq!(state["time_paused"], true);
        let resumed = resume(app.world_mut()).expect("resumes");
        assert_eq!(resumed["changed"], true);
        assert!(!app.world().resource::<Time>().paused);
        assert_eq!(resume(app.world_mut()).expect("idempotent")["changed"], false);
        let state = pause_state(app.world_mut()).expect("state");
        assert_eq!(
            (state["pauses"].as_u64(), state["resumes"].as_u64()),
            (Some(1), Some(1))
        );
    }

    #[test]
    fn refusals_name_their_reason() {
        let mut off = app(GamePauseConfig::default());
        let e = pause(off.world_mut()).expect_err("off");
        assert_eq!(reason_of(&e).as_deref(), Some("game_pause_off"));
        assert_eq!(
            pause_state(off.world_mut()).expect("state")["would_refuse"],
            "game_pause_off"
        );

        let mut net = app(GamePauseConfig::enabled());
        net.insert_resource(NetworkedSession(true));
        let e = pause(net.world_mut()).expect_err("networked");
        assert_eq!(reason_of(&e).as_deref(), Some("networked_session"));
        assert!(!net.world().resource::<Time>().paused);
        let state = pause_state(net.world_mut()).expect("state");
        assert_eq!(state["networked"], true);
        assert_eq!(state["last_refusal"], "networked_session");
        assert_eq!(state["refusals"], 1);

        let mut bare = App::new();
        bare.update();
        for e in [
            pause(bare.world_mut()).expect_err("no plugin"),
            resume(bare.world_mut()).expect_err("no plugin"),
            pause_state(bare.world_mut()).expect_err("no plugin"),
        ] {
            assert_eq!(reason_of(&e).as_deref(), Some("game_pause_not_installed"));
        }
    }
}
