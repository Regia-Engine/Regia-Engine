// SPDX-License-Identifier: MPL-2.0
//! Agent methods of the spec 50 remainder (ADR-2410, book/agents/video-boot-and-cues.md):
//! `media.boot_state`, `media.cues`, `media.decoders`, `media.validate` (read) and
//! `media.boot_skip`, `media.set_visible` (play).
//!
//! Errors are `F0002` with `data.reason`: `media_player_not_found`, `media_not_installed` (no
//! `MediaPlugin`; `media.boot_state` answers `installed: false` without a sequence), and for
//! `media.validate` the file's own protocol code prefixed `video_` (for example
//! `video_container_invalid` for a truncated file).

use regia_app::prelude::*;
use regia_media::boot::{BootMovies, SkipVideo};
use regia_media::plugin::{MediaFiles, MediaSettings, find_player};
use regia_media::{VideoPlayer, VideoPlayerState, VideoVisible};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn player(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    let key = match params.get("player") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(RpcError::invalid_params(
                "`player` (entity id or name) is required",
            ));
        }
    };
    find_player(world, &key).map_err(|_| {
        refused(
            "media_player_not_found",
            format!("no video player `{key}`"),
            "video.list lists the players",
        )
    })
}

fn installed(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<MediaSettings>() {
        Ok(())
    } else {
        Err(refused(
            "media_not_installed",
            "no video player in this app",
            "add regia_media::MediaPlugin",
        ))
    }
}

/// `media.boot_state`: the boot movie sequence.
pub(crate) fn boot_state(world: &mut World) -> Result<Value, RpcError> {
    let origin = world.get_resource::<regia_framework::boot_movies::BootMoviesOrigin>();
    let configuration = json!({
        "source": origin.map_or_else(
            || if world.contains_resource::<BootMovies>() { "code" } else { "default" },
            |origin| origin.source,
        ),
        "location": origin.and_then(|origin| origin.location.as_deref()),
    });
    let Some(boot) = world.get_resource::<BootMovies>() else {
        return Ok(json!({ "installed": false, "configuration": configuration }));
    };
    let records: Vec<Value> = boot
        .records()
        .iter()
        .map(|r| {
            let code = match &r.outcome {
                regia_media::boot::BootOutcome::Failed(code) => Some(code.clone()),
                _ => None,
            };
            json!({ "index": r.index, "source": r.source, "outcome": r.outcome.name(), "code": code })
        })
        .collect();
    let current = boot.current();
    let position = current
        .and_then(|(_, e)| world.get::<VideoPlayerState>(e))
        .map(|s| s.core().position());
    Ok(json!({
        "installed": true,
        "configuration": configuration,
        "movies": boot.movies,
        "skippable_after": boot.skippable_after,
        "done": boot.is_done(),
        "current": current.map(|(i, e)| json!({ "index": i, "player": e.to_bits(), "position": position })),
        "records": records,
        "refused_skips": boot.refused_skips(),
    }))
}

/// `media.boot_skip`: the same as a `skip_video` press (applied in the next `PreUpdate`).
pub(crate) fn boot_skip(world: &mut World) -> Result<Value, RpcError> {
    installed(world)?;
    let boot = world.contains_resource::<BootMovies>();
    match world.get_resource_mut::<Messages<SkipVideo>>() {
        Some(mut m) => {
            m.write(SkipVideo);
            Ok(json!({ "queued": true, "boot": boot }))
        }
        None => Err(refused(
            "media_not_installed",
            "the SkipVideo message is not registered",
            "add regia_media::MediaPlugin",
        )),
    }
}

/// `media.set_visible`: writes the player's `VideoVisible` (its policy decides what pauses).
pub(crate) fn set_visible(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = player(world, params)?;
    let visible = params
        .get("visible")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`visible` (boolean) is required"))?;
    let policy = world
        .get::<VideoPlayer>(entity)
        .map(|p| format!("{:?}", p.visibility));
    world.entity_mut(entity).insert(VideoVisible(visible));
    Ok(json!({ "player": entity.to_bits(), "visible": visible, "policy": policy }))
}

/// `media.cues`: the text cues read so far and the ones on screen.
pub(crate) fn cues(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = player(world, params)?;
    let state = world.get::<VideoPlayerState>(entity).ok_or_else(|| {
        refused(
            "media_player_not_found",
            "player has no state",
            "let one update run",
        )
    })?;
    let core = state.core();
    let Some(store) = core.cue_store() else {
        return Ok(json!({ "player": entity.to_bits(), "cues": [], "active": [], "dropped": 0 }));
    };
    let cues: Vec<Value> = store
        .cues()
        .iter()
        .map(|c| {
            json!({
                "track": c.track,
                "start": c.start_ns as f64 / 1e9,
                "end": c.end_ns.map(|e| e as f64 / 1e9),
                "text": c.text,
            })
        })
        .collect();
    let active: Vec<Value> = core
        .active_cues()
        .iter()
        .map(|(start, track)| json!({ "track": track, "start": *start as f64 / 1e9 }))
        .collect();
    Ok(json!({
        "player": entity.to_bits(),
        "cues": cues,
        "active": active,
        "dropped": store.dropped(),
    }))
}

/// `media.decoders`: the decode pool.
pub(crate) fn decoders(world: &mut World) -> Result<Value, RpcError> {
    installed(world)?;
    let settings = world.resource::<MediaSettings>().clone();
    let mut q = world.query::<(Entity, &VideoPlayerState)>();
    let mut holders: Vec<Value> = Vec::new();
    let mut paused = 0;
    for (e, s) in q.iter(world) {
        let core = s.core();
        if core.holds_decoder() {
            if core.decode_paused() {
                paused += 1;
            }
            holders.push(json!({
                "player": e.to_bits(),
                "state": core.state().name(),
                "decode_paused": core.decode_paused(),
                "half_rate": core.half_rate(),
            }));
        }
    }
    Ok(json!({
        "max_decoders": settings.max_decoders,
        "max_players": settings.max_players,
        "in_use": holders.len(),
        "decode_paused": paused,
        "thermal_throttle": settings.thermal_throttle,
        "holders": holders,
    }))
}

/// `media.validate`: the import and cook checks on a media file (no player needed).
pub(crate) fn validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let source = params
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`source` (media file path) is required"))?;
    let files = world.get_resource::<MediaFiles>().cloned().unwrap_or_default();
    let fail = |e: regia_media::MediaError| refused(&format!("video_{}", e.code()), e.to_string(), e.hint());
    let (file, _) = files.resolve(source).map_err(fail)?;
    let reader = files.open(&file).map_err(fail)?;
    let report = regia_media::validate(reader).map_err(fail)?;
    serde_json::to_value(&report)
        .map(|v| json!({ "source": source, "file": file, "valid": true, "report": v }))
        .map_err(|e| RpcError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::boot_movies::BootMoviesPlugin;

    #[test]
    fn packaged_boot_state_reports_origin_and_explicit_sequence_wins() {
        let root = regia_test_utils::unique_temp_dir("agent-packaged-boot");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(
            root.join("build_info.json"),
            r#"{"boot":{"movies":["movies/intro.webm"]}}"#,
        )
        .expect("metadata");
        for explicit in [false, true] {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, regia_media::MediaPlugin));
            if explicit {
                app.insert_resource(BootMovies::new(["code.webm"]));
            }
            app.add_plugins(BootMoviesPlugin {
                project_root: Some(root.clone()),
                config: None,
            });
            let state = boot_state(app.world_mut()).expect("status");
            assert_eq!(
                state["configuration"]["source"],
                if explicit { "code" } else { "packaged" }
            );
            assert_eq!(
                state["movies"],
                json!([if explicit {
                    "code.webm"
                } else {
                    "movies/intro.webm"
                }])
            );
        }
        for (manifest, source) in [
            ("[boot]\nmovies = []\n", "manifest"),
            ("[boot]\nmovies = 7\n", "invalid"),
        ] {
            std::fs::write(root.join("Regia.toml"), manifest).expect("loose manifest");
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                BootMoviesPlugin {
                    project_root: Some(root.clone()),
                    config: None,
                },
            ));
            let state = boot_state(app.world_mut()).expect("empty status");
            assert_eq!(state["installed"], false);
            assert_eq!(state["configuration"]["source"], source);
            assert!(
                state["configuration"]["location"]
                    .as_str()
                    .expect("location")
                    .ends_with("Regia.toml")
            );
        }
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
