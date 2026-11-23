// SPDX-License-Identifier: MPL-2.0
//! Agent methods of the video renderer half (specs/50 § 2.7, ADR-2044, book/agents/media-render.md):
//! `media.state` (read) and `media.play`, `media.pause`, `media.seek`, `media.set_loop` (play).
//!
//! The play methods queue a `regia_render::VideoControl` on the installed frame source, the same
//! path a `UiVideo` transport widget uses; the media crate applies it in the next `PreUpdate`, so the
//! reply says `queued` and the effect shows after one update. `media.state` reports what the
//! renderer side sees: texture size and serial, the fallback frame, the decode ring's memory, the
//! UI nodes and material bindings showing each player, the GPU convert queue and the fullscreen queue.
//!
//! Errors are `F0002` with `data.reason`: `media_not_installed` (no `VideoFramePlugin`),
//! `media_player_not_found`, `media_control_queue_full`.

use regia_app::prelude::*;
use regia_media::frame_source::{FullscreenVideos, VideoMaterialErrors};
use regia_media::frame_source::{
    GpuVideoJobs, MediaFrameSource, VideoFrameSettings, VideoMaterial, VideoMaterialBound,
};
use regia_media::plugin::find_player;
use regia_render::{VideoControl, VideoFrameSource};
use regia_ui::video::{UiVideo, UiVideoShown};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn source(world: &World) -> Result<MediaFrameSource, RpcError> {
    world.get_resource::<MediaFrameSource>().cloned().ok_or_else(|| {
        refused(
            "media_not_installed",
            "no video frame source in this app",
            "add regia_media::MediaPlugin and regia_media::VideoFramePlugin",
        )
    })
}

/// The `player` parameter (entity id or `Name`) as entity bits known to the source.
fn player(world: &mut World, params: &Value, source: &MediaFrameSource) -> Result<u64, RpcError> {
    let key = match params.get("player") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(RpcError::invalid_params(
                "`player` (entity id or name) is required",
            ));
        }
    };
    let not_found = || {
        refused(
            "media_player_not_found",
            format!("no video player `{key}` with a published frame"),
            "players appear in media.state one update after they are spawned",
        )
    };
    let entity = find_player(world, &key).map_err(|_| not_found())?;
    let bits = entity.to_bits();
    if source.players().contains(&bits) {
        Ok(bits)
    } else {
        Err(not_found())
    }
}

fn queue(world: &mut World, params: &Value, control: VideoControl) -> Result<Value, RpcError> {
    let source = source(world)?;
    let id = player(world, params, &source)?;
    source.control(id, control).map_err(|e| {
        refused(
            &format!("media_{}", e.code()),
            e.to_string(),
            "let one update run so queued controls apply",
        )
    })?;
    let pending = source
        .statuses()
        .into_iter()
        .find(|s| s.player == id)
        .map_or(0, |s| s.controls_pending);
    Ok(json!({ "player": id, "queued": true, "pending": pending }))
}

/// `media.play {player}`.
pub(crate) fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    queue(world, params, VideoControl::Play)
}

/// `media.pause {player}`.
pub(crate) fn pause(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    queue(world, params, VideoControl::Pause)
}

/// `media.seek {player, time}` (seconds; accurate seek, clamped to the clip).
pub(crate) fn seek(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let time = match params.get("time") {
        Some(v) => v
            .as_f64()
            .filter(|t| t.is_finite())
            .ok_or_else(|| RpcError::invalid_params("`time` must be a finite number of seconds"))?,
        None => return Err(RpcError::invalid_params("`time` (seconds) is required")),
    };
    queue(world, params, VideoControl::Seek(time))
}

/// `media.set_loop {player, loop}`.
pub(crate) fn set_loop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params
        .get("loop")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`loop` (boolean) is required"))?;
    queue(world, params, VideoControl::SetLoop(on))
}

/// `media.state {player?}`.
pub(crate) fn state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(source) = world.get_resource::<MediaFrameSource>().cloned() else {
        return Ok(json!({ "installed": false, "players": [] }));
    };
    let only = match params.get("player") {
        None | Some(Value::Null) => None,
        Some(_) => Some(player(world, params, &source)?),
    };
    let settings = world
        .get_resource::<VideoFrameSettings>()
        .cloned()
        .unwrap_or_default();
    let mut ui_nodes: Vec<(u64, Value)> = {
        let mut q = world.query::<(Entity, &UiVideo, Option<&UiVideoShown>)>();
        q.iter(world)
            .map(|(e, v, shown)| {
                let s = shown.copied().unwrap_or_default();
                (
                    v.player,
                    json!({
                        "node": e.to_bits(),
                        "image": s.image,
                        "serial": s.serial,
                        "frame_time": s.pts_ns.map(|t| t as f64 / 1e9),
                        "fallback": s.fallback,
                        "uploads": s.uploads,
                    }),
                )
            })
            .collect()
    };
    ui_nodes.sort_by_key(|(p, v)| (*p, v["node"].as_u64()));
    let errors = world
        .get_resource::<VideoMaterialErrors>()
        .cloned()
        .unwrap_or_default();
    let mut materials: Vec<(u64, Value)> = {
        let mut q = world.query::<(Entity, &VideoMaterial, Option<&VideoMaterialBound>)>();
        q.iter(world)
            .map(|(e, m, bound)| {
                (
                    m.player.to_bits(),
                    json!({
                        "entity": e.to_bits(),
                        "slot": m.slot.name(),
                        "bound": bound.is_some(),
                        "error": errors.0.get(&e.to_bits()),
                    }),
                )
            })
            .collect()
    };
    materials.sort_by_key(|(p, v)| (*p, v["entity"].as_u64()));
    let players: Vec<Value> = source
        .statuses()
        .into_iter()
        .filter(|s| only.is_none_or(|o| o == s.player))
        .map(|s| {
            let mut v = serde_json::to_value(&s).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut v {
                let ui: Vec<Value> = ui_nodes
                    .iter()
                    .filter(|(p, _)| *p == s.player)
                    .map(|(_, n)| n.clone())
                    .collect();
                let mats: Vec<Value> = materials
                    .iter()
                    .filter(|(p, _)| *p == s.player)
                    .map(|(_, n)| n.clone())
                    .collect();
                map.insert("ui_nodes".into(), Value::Array(ui));
                map.insert("materials".into(), Value::Array(mats));
            }
            v
        })
        .collect();
    let gpu = world.get_resource::<GpuVideoJobs>().map(|jobs| {
        let s = jobs.0.stats();
        json!({
            "converted": s.converted,
            "replaced": s.replaced,
            "failed": s.failed,
            "pending": s.pending,
            "gpu_bytes": s.gpu_bytes,
        })
    });
    let fullscreen = world.get_resource::<FullscreenVideos>().map(|q| {
        let s = q.0.stats();
        let players: Vec<u64> = q.0.videos().iter().map(|v| v.player).collect();
        json!({
            "players": players,
            "queued": s.queued,
            "drawn": s.drawn,
            "uploads": s.uploads,
            "over_limit": s.over_limit,
        })
    });
    Ok(json!({
        "installed": true,
        "stall_ticks": settings.stall_ticks,
        "gpu_convert": settings.gpu_convert && gpu.is_some(),
        "ring": { "max_frames": settings.ring.max_frames, "max_bytes": settings.ring.max_bytes },
        "players": players,
        "gpu": gpu,
        "fullscreen": fullscreen,
    }))
}

#[cfg(test)]
mod tests {
    use regia_media::mux::PatternClip;
    use regia_media::plugin::MediaFiles;
    use regia_media::{ClockMode, MediaPlugin, VideoFramePlugin, VideoPlayer};

    use super::*;

    fn reason(err: &RpcError) -> Option<String> {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn app() -> (App, Entity) {
        let mut app = App::new();
        let mut files = MediaFiles::default();
        files.insert("movies/a.webm", PatternClip::new(16, 8, 20, 25).to_webm());
        app.add_plugins(MinimalPlugins)
            .insert_resource(files)
            .add_plugins((MediaPlugin, VideoFramePlugin));
        let e = app
            .world_mut()
            .spawn((
                VideoPlayer {
                    autoplay: false,
                    clock: ClockMode::Lockstep,
                    ..VideoPlayer::new("movies/a.webm")
                },
                Name::new("wall"),
            ))
            .id();
        app.update();
        (app, e)
    }

    #[test]
    fn controls_round_trip_through_the_source() {
        let (mut app, e) = app();
        let w = app.world_mut();
        let s = state(w, &json!({})).expect("state");
        assert_eq!(s["installed"], true);
        assert_eq!(s["players"][0]["player"], e.to_bits());
        assert_eq!(s["players"][0]["state"], "ready");
        assert_eq!(s["players"][0]["width"], 16);
        assert_eq!(s["players"][0]["fallback"], false);
        assert!(s["fullscreen"].is_null(), "no fullscreen queue installed");
        let q = play(w, &json!({ "player": "wall" })).expect("play");
        assert_eq!(q["queued"], true);
        assert_eq!(q["pending"], 1);
        app.update();
        let s = state(app.world_mut(), &json!({ "player": "wall" })).expect("state");
        assert_eq!(s["players"][0]["state"], "playing");
        assert_eq!(s["players"][0]["controls_applied"], 1);
        let w = app.world_mut();
        seek(w, &json!({ "player": e.to_bits(), "time": 0.4 })).expect("seek");
        pause(w, &json!({ "player": "wall" })).expect("pause");
        set_loop(w, &json!({ "player": "wall", "loop": true })).expect("loop");
        app.update();
        let s = state(app.world_mut(), &json!({ "player": "wall" })).expect("state");
        assert_eq!(s["players"][0]["state"], "paused");
        assert_eq!(s["players"][0]["looping"], true);
        assert_eq!(s["players"][0]["frame_time"], 0.4);
        assert_eq!(s["players"][0]["controls_applied"], 4);
    }

    #[test]
    fn error_reasons() {
        let (mut app, _) = app();
        let w = app.world_mut();
        let err = play(w, &json!({ "player": "ghost" })).expect_err("missing");
        assert_eq!(reason(&err).as_deref(), Some("media_player_not_found"));
        assert!(seek(w, &json!({ "player": "wall" })).is_err(), "time is required");
        assert!(set_loop(w, &json!({ "player": "wall", "loop": "yes" })).is_err());
        for _ in 0..regia_render::video::VIDEO_CONTROL_QUEUE {
            pause(w, &json!({ "player": "wall" })).expect("queued");
        }
        let err = pause(w, &json!({ "player": "wall" })).expect_err("full");
        assert_eq!(reason(&err).as_deref(), Some("media_control_queue_full"));

        let mut bare = App::new();
        bare.add_plugins(MinimalPlugins);
        let s = state(bare.world_mut(), &json!({})).expect("state");
        assert_eq!(s["installed"], false);
        let err = play(bare.world_mut(), &json!({ "player": "wall" })).expect_err("bare");
        assert_eq!(reason(&err).as_deref(), Some("media_not_installed"));
    }
}
