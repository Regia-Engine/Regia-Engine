// SPDX-License-Identifier: MPL-2.0
//! Agent methods for video playback (specs/50 § 5, ADR-1670, book/agents/video-playback.md):
//! `video.probe`, `video.list`, `video.status`, `video.frame_hash` (read); `video.open`, `video.play`,
//! `video.pause`, `video.seek`, `video.step`, `video.set_rate`, `video.set_loop`, `video.set_volume`,
//! `video.select_track`, `video.skip`, `video.close` (play); `video.create_player` (edit).
//!
//! Media errors are `F0002` with `data.reason` = `video_` + the `MediaError` code. Without
//! `MediaPlugin`, `video.list` answers `installed: false` and the play methods, `video.status`
//! and `video.frame_hash` refuse with `video_not_installed`. `video.codecs` does not look at
//! the world. `video.probe` still opens the source and returns that file's error
//! (`video_source_not_found`, `video_container_unsupported`) rather than `video_not_installed`.

use regia_app::prelude::*;
use regia_media::plugin::{
    self, PlayerStatus, VideoPlayer, VideoPlayerState, VideoRegistry, command_open, command_seek,
    find_player, with_core,
};
use regia_media::reference::Region;
use regia_media::{ClockMode, LoopMode, MediaError, PlayState, SeekMode};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most frames one `video.step` call presents.
const MAX_STEP: u64 = 10_000;

fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn media(err: MediaError) -> RpcError {
    refused(&format!("video_{}", err.code()), err.to_string(), err.hint())
}

fn installed(world: &World) -> bool {
    world.contains_resource::<VideoRegistry>()
}

fn require_installed(world: &World) -> Result<(), RpcError> {
    if installed(world) {
        Ok(())
    } else {
        Err(refused(
            "video_not_installed",
            "no video playback in this app",
            "add regia_media::MediaPlugin",
        ))
    }
}

fn text<'a>(params: &'a Value, field: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a string"))),
    }
}

/// A whole-number parameter: a fraction is an error rather than truncated.
fn whole(params: &Value, field: &str) -> Result<Option<f64>, RpcError> {
    let n = number(params, field)?;
    if let Some(n) = n
        && n.fract() != 0.0
    {
        return Err(RpcError::invalid_params(format!(
            "`{field}` must be a whole number"
        )));
    }
    Ok(n)
}

fn number(params: &Value, field: &str) -> Result<Option<f64>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            if !n.is_finite() {
                return Err(RpcError::invalid_params(format!("`{field}` must be finite")));
            }
            Ok(Some(n))
        }
    }
}

/// The `player` parameter: entity id (number or string) or `Name`.
fn player(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    require_installed(world)?;
    let key = match params.get("player") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => {
            return Err(RpcError::invalid_params(
                "`player` (entity id or name) is required",
            ));
        }
    };
    find_player(world, &key).map_err(media)
}

fn status_json(s: &PlayerStatus) -> Value {
    json!({
        "player": s.player,
        "source": s.source,
        "state": s.state,
        "position": s.position,
        "duration": s.duration,
        "frame_index": s.frame_index,
        "rate": s.rate,
        "loop": loop_json(s.loop_mode),
        "loops": s.loops,
        "clock": s.clock,
        "audio_track": s.audio_track,
        "stats": s.stats,
        "last_error": s.last_error,
    })
}

fn loop_json(mode: LoopMode) -> Value {
    match mode {
        LoopMode::Off => json!({ "mode": "off" }),
        LoopMode::Loop => json!({ "mode": "loop" }),
        LoopMode::Count(n) => json!({ "mode": "count", "count": n }),
    }
}

fn parse_loop(params: &Value) -> Result<Option<LoopMode>, RpcError> {
    let Some(mode) = text(params, "loop")?.or(text(params, "mode")?) else {
        return Ok(None);
    };
    Ok(Some(match mode {
        "off" => LoopMode::Off,
        "loop" => LoopMode::Loop,
        "count" => {
            let n = whole(params, "count")?.ok_or_else(|| RpcError::invalid_params("`count` is required"))?;
            if !(1.0..=1_000_000.0).contains(&n) {
                return Err(RpcError::invalid_params("`count` must be in 1..=1000000"));
            }
            LoopMode::Count(n as u32)
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "loop mode `{other}` is not off, loop or count"
            )));
        }
    }))
}

fn state_of(world: &mut World, e: Entity) -> Result<Value, RpcError> {
    with_core(world, e, |c| Ok(json!({ "state": c.state().name() }))).map_err(media)
}

fn one_status(world: &mut World, e: Entity) -> Result<Value, RpcError> {
    plugin::statuses(world)
        .iter()
        .find(|s| s.player == e.to_bits())
        .map(status_json)
        .ok_or_else(|| media(MediaError::PlayerNotFound(e.to_bits().to_string())))
}

/// `video.probe`.
pub(crate) fn probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let source = text(params, "source")?.ok_or_else(|| RpcError::invalid_params("`source` is required"))?;
    let info = plugin::probe_source(world, source).map_err(media)?;
    let reject = info.reject_reason();
    Ok(json!({
        "container": info.container,
        "duration": info.duration_ns.map(|d| d as f64 / 1e9),
        "tracks": info.tracks,
        "cue_count": info.cue_count,
        "markers": info.markers,
        "supported": reject.is_none(),
        "reject_reason": reject.map(|e| format!("video_{}", e.code())),
    }))
}

/// `video.list`.
pub(crate) fn list(world: &mut World) -> Result<Value, RpcError> {
    if !installed(world) {
        return Ok(json!({ "installed": false, "players": [] }));
    }
    let players: Vec<Value> = plugin::statuses(world)
        .iter()
        .map(|s| {
            json!({
                "player": s.player, "entity": s.player, "source": s.source, "state": s.state,
                "position": s.position, "duration": s.duration,
            })
        })
        .collect();
    let events: Vec<Value> = world
        .resource::<VideoRegistry>()
        .recent()
        .map(|(seq, e, ev)| json!({ "seq": seq, "player": e.to_bits(), "kind": ev.kind(), "event": ev }))
        .collect();
    Ok(json!({ "installed": true, "players": players, "events": events }))
}

/// `video.status`.
pub(crate) fn status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    one_status(world, e)
}

/// `video.frame_hash`.
pub(crate) fn frame_hash(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let region = match params.get("region") {
        None | Some(Value::Null) => None,
        Some(r) => {
            let field = |k: &str| {
                r.get(k)
                    .and_then(Value::as_u64)
                    .map(|v| v.min(u64::from(u32::MAX)) as u32)
            };
            match (field("x"), field("y"), field("w"), field("h")) {
                (Some(x), Some(y), Some(w), Some(h)) => Some(Region { x, y, w, h }),
                _ => return Err(RpcError::invalid_params("`region` is { x, y, w, h } in pixels")),
            }
        }
    };
    let (hash, width, height, time) = plugin::frame_hash(world, e, region).map_err(media)?;
    Ok(json!({
        "hash": format!("{hash:016x}"),
        "width": width,
        "height": height,
        "frame_time": time,
        "source": "cpu_reference",
    }))
}

/// `video.open`.
pub(crate) fn open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let source = text(params, "source")?.ok_or_else(|| RpcError::invalid_params("`source` is required"))?;
    let mut p = world.get::<VideoPlayer>(e).cloned().unwrap_or_default();
    p.source = source.to_string();
    if let Some(a) = crate::params::opt_bool(params, "autoplay")? {
        p.autoplay = a;
    }
    if let Some(c) = text(params, "clock")? {
        p.clock = ClockMode::parse(c)
            .ok_or_else(|| RpcError::invalid_params("`clock` is audio, game, external or lockstep"))?;
    }
    if let Some(o) = number(params, "phase_offset")? {
        p.phase_offset = o.max(0.0);
    }
    if let Some(l) = parse_loop(params)? {
        p.loop_mode = l;
    }
    let state = command_open(world, e, p).map_err(media)?;
    let last = with_core(world, e, |c| Ok(c.last_error().cloned())).map_err(media)?;
    if let Some(err) = last.filter(|_| state.name() == "failed") {
        return Err(media(err));
    }
    Ok(json!({ "state": state.name() }))
}

/// `video.play`.
pub(crate) fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    with_core(world, e, |c| c.play()).map_err(media)?;
    state_of(world, e)
}

/// `video.pause`.
pub(crate) fn pause(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    with_core(world, e, |c| c.pause()).map_err(media)?;
    state_of(world, e)
}

/// `video.seek`.
pub(crate) fn seek(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let mode = match text(params, "mode")? {
        None | Some("accurate") => SeekMode::Accurate,
        Some("keyframe") => SeekMode::Keyframe,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "seek mode `{other}` is not accurate or keyframe"
            )));
        }
    };
    let time = number(params, "time")?;
    let frame = match whole(params, "frame")? {
        Some(f) if f >= 0.0 => Some(f as u64),
        Some(_) => return Err(RpcError::invalid_params("`frame` must not be negative")),
        None => None,
    };
    if time.is_none() && frame.is_none() {
        return Err(RpcError::invalid_params("give `time` (seconds) or `frame`"));
    }
    // Spec 50: `time` or `frame`. Frame wins in the player when both are set, and the reply
    // used to name `time`. Refuse the pair so the target matches the seek.
    if time.is_some() && frame.is_some() {
        return Err(RpcError::invalid_params(
            "give `time` (seconds) or `frame`, not both",
        ));
    }
    let state = command_seek(world, e, time, frame, mode).map_err(media)?;
    Ok(
        json!({ "state": state.name(), "target": time.map_or_else(|| json!({ "frame": frame }), |t| json!({ "time": t })) }),
    )
}

/// `video.step`.
pub(crate) fn step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let frames = whole(params, "frames")?.unwrap_or(1.0);
    if !(1.0..=MAX_STEP as f64).contains(&frames) {
        return Err(RpcError::invalid_params(format!(
            "`frames` must be in 1..={MAX_STEP}"
        )));
    }
    with_core(world, e, |c| {
        c.step_frames(frames as u32)?;
        Ok(json!({ "frame_index": c.frame_index(), "position": c.position(), "state": c.state().name() }))
    })
    .map_err(media)
}

/// `video.set_rate`.
pub(crate) fn set_rate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let rate = number(params, "rate")?.ok_or_else(|| RpcError::invalid_params("`rate` is required"))? as f32;
    with_core(world, e, |c| c.set_rate(rate)).map_err(media)?;
    if let Some(mut p) = world.get_mut::<VideoPlayer>(e)
        && rate > 0.0
    {
        p.rate = rate;
    }
    Ok(json!({ "rate": rate }))
}

/// `video.set_loop`.
pub(crate) fn set_loop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let mode = parse_loop(params)?.ok_or_else(|| RpcError::invalid_params("`mode` is required"))?;
    with_core(world, e, |c| {
        c.set_loop(mode);
        Ok(())
    })
    .map_err(media)?;
    if let Some(mut p) = world.get_mut::<VideoPlayer>(e) {
        p.loop_mode = mode;
    }
    Ok(json!({ "loop": loop_json(mode) }))
}

/// `video.set_volume`.
pub(crate) fn set_volume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let volume = number(params, "volume")?.ok_or_else(|| RpcError::invalid_params("`volume` is required"))?;
    if !(0.0..=4.0).contains(&volume) {
        return Err(RpcError::invalid_params("`volume` must be in 0..=4"));
    }
    if let Some(mut p) = world.get_mut::<VideoPlayer>(e) {
        p.volume = volume as f32;
    }
    if let Some(mut reg) = world.get_resource_mut::<regia_audio::media_feed::MediaAudioRegistry>() {
        reg.set_volume(e.to_bits(), volume as f32);
    }
    Ok(json!({ "volume": volume }))
}

/// `video.select_track`: reopens the session with the chosen audio track (or none).
pub(crate) fn select_track(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    match text(params, "kind")? {
        Some("audio") => {}
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "track kind `{other}` cannot be selected (audio)"
            )));
        }
        None => return Err(RpcError::invalid_params("`kind` is required (audio)")),
    }
    let index = match params.get("index") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|i| *i < 64)
                .ok_or_else(|| RpcError::invalid_params("`index` is a track index below 64, or null"))?
                as u32,
        ),
    };
    let mut p = world
        .get::<VideoPlayer>(e)
        .cloned()
        .ok_or_else(|| media(MediaError::PlayerNotFound(e.to_bits().to_string())))?;
    if let Some(i) = index {
        let info = with_core(world, e, |c| Ok(c.info().cloned())).map_err(media)?;
        let audio = info.as_ref().is_some_and(|inf| {
            inf.tracks
                .iter()
                .any(|t| t.index == i && t.kind == regia_media::TrackKind::Audio)
        });
        if !audio {
            return Err(media(MediaError::TrackNotFound(i)));
        }
    }
    let position = with_core(world, e, |c| Ok(c.position())).map_err(media)?;
    let was_open = world
        .get::<VideoPlayerState>(e)
        .is_some_and(|s| !matches!(s.core().state(), PlayState::Closed | PlayState::Failed));
    if let Some(mut state) = world.get_mut::<VideoPlayerState>(e) {
        state.set_audio_disabled(index.is_none());
    }
    if index.is_none() && p.clock == ClockMode::Audio {
        p.clock = ClockMode::Game;
    }
    p.audio_track = index;
    // `phase_offset` is the game's initial seek, so the reopen borrows it for a moment instead of
    // keeping the position: a later source change by component must start the new clip at the
    // game's offset, not where this clip was when the track changed (corr-2d-media LOW 7).
    let initial_seek = p.phase_offset;
    p.phase_offset = position;
    // A playlist keeps its entry: only the clip that plays now is reopened.
    let opened = plugin::command_reopen_tracks(world, e, p).map_err(media)?;
    if let Some(mut live) = world.get_mut::<VideoPlayer>(e) {
        live.phase_offset = initial_seek;
    }
    if was_open && opened != PlayState::Failed {
        with_core(world, e, |c| {
            c.note_tracks_changed();
            Ok(())
        })
        .map_err(media)?;
    }
    let tracks = with_core(world, e, |c| {
        Ok(c.info().map(|i| i.tracks.clone()).unwrap_or_default())
    })
    .map_err(media)?;
    Ok(json!({ "tracks": tracks, "audio_track": index }))
}

/// `video.skip`.
pub(crate) fn skip(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    let skipped = with_core(world, e, |c| Ok(c.skip())).map_err(media)?;
    Ok(json!({ "skipped": skipped }))
}

/// `video.close`.
pub(crate) fn close(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = player(world, params)?;
    with_core(world, e, |c| {
        c.close();
        Ok(())
    })
    .map_err(media)?;
    state_of(world, e)
}

/// `video.create_player`: adds a `VideoPlayer` to an entity (or a new named entity).
pub(crate) fn create_player(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_installed(world)?;
    let source = text(params, "source")?.ok_or_else(|| RpcError::invalid_params("`source` is required"))?;
    let mut p = VideoPlayer::new(source);
    if let Some(a) = crate::params::opt_bool(params, "autoplay")? {
        p.autoplay = a;
    }
    if let Some(c) = text(params, "clock")? {
        p.clock = ClockMode::parse(c)
            .ok_or_else(|| RpcError::invalid_params("`clock` is audio, game, external or lockstep"))?;
    }
    if let Some(l) = parse_loop(params)? {
        p.loop_mode = l;
    }
    if let Some(r) = number(params, "rate")? {
        if !(0.25..=4.0).contains(&r) {
            return Err(media(MediaError::RateUnsupported(r.to_string())));
        }
        p.rate = r as f32;
    }
    let entity = match crate::params::opt_u64(params, "entity")? {
        Some(bits) => {
            let e = Entity::try_from_bits(bits)
                .filter(|e| world.get_entity(*e).is_ok())
                .ok_or_else(|| RpcError::invalid_params(format!("entity {bits} does not exist")))?;
            world.entity_mut(e).insert(p);
            e
        }
        None => {
            let name = text(params, "name")?.unwrap_or("video player").to_string();
            world.spawn((Name::new(name), p)).id()
        }
    };
    if world.get::<VideoPlayerState>(entity).is_none() {
        world.entity_mut(entity).insert(VideoPlayerState::default());
    }
    Ok(json!({ "entity": entity.to_bits() }))
}

/// `video.codecs`: the codec rule's allowed codecs and the decoders built into this binary (ADR-1869).
/// Works without `MediaPlugin`: it describes the build, not a player.
pub(crate) fn codecs() -> Result<Value, RpcError> {
    let rows = regia_media::codec::codecs();
    let decodes = |id: &str| rows.iter().any(|r| r.codec_id == id && r.decoder.is_some());
    let av1 = decodes(regia_media::codec::AV1);
    let cores = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);
    let av1_thread_budget =
        regia_media::isolate::DecodeCaps::av1_process_budget(cfg!(target_os = "android"), cores);
    Ok(json!({
        "codecs": rows,
        "media_codecs": av1 && decodes(regia_media::codec::OPUS) && decodes(regia_media::codec::VORBIS),
        "av1_assembly": regia_media::codec::av1_assembly(),
        "av1_thread_budget": av1_thread_budget,
        "containers": ["webm", "mkv"],
    }))
}

/// Registers an in-memory clip for tests and examples.
#[cfg(test)]
pub(crate) fn insert_test_clip(world: &mut World, path: &str, bytes: Vec<u8>) {
    let mut files = world
        .get_resource::<plugin::MediaFiles>()
        .cloned()
        .unwrap_or_default();
    files.insert(path, bytes);
    world.insert_resource(files);
}

#[cfg(test)]
mod tests {
    use regia_media::MediaPlugin;
    use regia_media::mux::PatternClip;

    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, MediaPlugin));
        insert_test_clip(
            app.world_mut(),
            "movies/a.webm",
            PatternClip::new(16, 8, 20, 25).to_webm(),
        );
        app
    }

    fn reason(err: &RpcError) -> Option<String> {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn whole_number_params_reject_fractions() {
        assert_eq!(whole(&json!({ "frames": 3 }), "frames").expect("int"), Some(3.0));
        assert!(whole(&json!({ "frames": 2.9 }), "frames").is_err());
        assert_eq!(whole(&json!({}), "frames").expect("absent"), None);
    }

    #[test]
    fn codecs_lists_the_build() {
        let v = codecs().expect("codecs");
        let rows = v["codecs"].as_array().expect("rows");
        assert_eq!(rows.len(), 6);
        let row = |id: &str| rows.iter().find(|r| r["codec_id"] == id).expect("row").clone();
        assert_eq!(row("V_UNCOMPRESSED")["decoder"], "raw");
        assert_eq!(row("V_VP9")["decoder"], Value::Null);
        let av1_built = regia_media::codec::classify("V_AV1") == regia_media::codec::CodecVerdict::Supported;
        assert_eq!(row("V_AV1")["decoder"].is_null(), !av1_built);
        assert_eq!(v["media_codecs"], av1_built);
        assert_eq!(row("V_AV1")["feature"], "av1");
        assert_eq!(row("V_AV1")["deterministic"], av1_built);
        assert_eq!(v["containers"][0], "webm");
        assert_eq!(v["av1_assembly"], regia_media::codec::av1_assembly());
        let cores = std::thread::available_parallelism()
            .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
            .unwrap_or(1);
        assert_eq!(
            v["av1_thread_budget"],
            regia_media::isolate::DecodeCaps::av1_process_budget(cfg!(target_os = "android"), cores)
        );
    }

    #[test]
    fn lifecycle_round_trip() {
        let mut app = app();
        let w = app.world_mut();
        let created = create_player(
            w,
            &json!({ "source": "movies/a.webm", "name": "wall", "autoplay": false, "clock": "lockstep" }),
        )
        .expect("created");
        let id = created["entity"].as_u64().expect("id");
        let opened = open(
            w,
            &json!({ "player": "wall", "source": "movies/a.webm", "clock": "lockstep", "autoplay": false }),
        )
        .expect("open");
        assert_eq!(opened["state"], "ready");
        let s = seek(w, &json!({ "player": id, "frame": 7 })).expect("seek");
        assert_eq!(s["state"], "paused");
        assert_eq!(s["target"]["frame"], 7);
        let both = seek(w, &json!({ "player": id, "time": 0.5, "frame": 7 })).expect_err("both");
        assert!(both.message.contains("not both"), "{}", both.message);
        let st = step(w, &json!({ "player": "wall", "frames": 3 })).expect("step");
        assert_eq!(st["frame_index"], 10);
        let h1 = frame_hash(w, &json!({ "player": "wall" })).expect("hash");
        let h2 = frame_hash(
            w,
            &json!({ "player": "wall", "region": { "x": 0, "y": 0, "w": 4, "h": 4 } }),
        )
        .expect("hash");
        assert_ne!(h1["hash"], h2["hash"]);
        assert_eq!(h1["width"], 16);
        assert_eq!(
            play(w, &json!({ "player": "wall" })).expect("play")["state"],
            "playing"
        );
        assert_eq!(
            pause(w, &json!({ "player": "wall" })).expect("pause")["state"],
            "paused"
        );
        assert_eq!(
            set_rate(w, &json!({ "player": "wall", "rate": 2.0 })).expect("rate")["rate"],
            2.0
        );
        assert_eq!(
            set_loop(w, &json!({ "player": "wall", "mode": "count", "count": 3 })).expect("loop")["loop"]["count"],
            3
        );
        assert_eq!(
            set_volume(w, &json!({ "player": "wall", "volume": 0.5 })).expect("volume")["volume"],
            0.5
        );
        let status_v = status(w, &json!({ "player": "wall" })).expect("status");
        assert_eq!(status_v["clock"], "lockstep");
        assert_eq!(status_v["loop"]["mode"], "count");
        let listed = list(w).expect("list");
        assert_eq!(listed["players"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            skip(w, &json!({ "player": "wall" })).expect("skip")["skipped"],
            true
        );
        assert_eq!(
            close(w, &json!({ "player": "wall" })).expect("close")["state"],
            "closed"
        );
        let probed = probe(w, &json!({ "source": "movies/a.webm" })).expect("probe");
        assert_eq!(probed["supported"], true);
        assert_eq!(probed["tracks"][0]["codec_id"], "V_UNCOMPRESSED");
    }

    #[test]
    fn error_reasons() {
        let mut app = app();
        let w = app.world_mut();
        let err = status(w, &json!({ "player": "ghost" })).expect_err("missing");
        assert_eq!(reason(&err).as_deref(), Some("video_player_not_found"));
        let err = probe(w, &json!({ "source": "movies/none.webm" })).expect_err("missing");
        assert_eq!(reason(&err).as_deref(), Some("video_source_not_found"));
        let err = probe(w, &json!({ "source": "movies/a.mp4" })).expect_err("mp4");
        assert_eq!(reason(&err).as_deref(), Some("video_container_unsupported"));
        create_player(w, &json!({ "source": "", "name": "p" })).expect("created");
        let err = play(w, &json!({ "player": "p" })).expect_err("closed");
        assert_eq!(reason(&err).as_deref(), Some("video_bad_state"));
        let err = open(w, &json!({ "player": "p", "source": "movies/none.webm" })).expect_err("missing");
        assert_eq!(reason(&err).as_deref(), Some("video_source_not_found"));
        open(
            w,
            &json!({ "player": "p", "source": "movies/a.webm", "clock": "lockstep" }),
        )
        .expect("open");
        let err = set_rate(w, &json!({ "player": "p", "rate": 9.0 })).expect_err("rate");
        assert_eq!(reason(&err).as_deref(), Some("video_rate_unsupported"));
        let err = seek(w, &json!({ "player": "p", "frame": 999 })).expect_err("range");
        assert_eq!(reason(&err).as_deref(), Some("video_seek_out_of_range"));
        let err = select_track(w, &json!({ "player": "p", "kind": "audio", "index": 5 })).expect_err("track");
        assert_eq!(reason(&err).as_deref(), Some("video_track_not_found"));
    }

    #[test]
    fn select_track_null_mutes_and_reports_tracks_changed() {
        use regia_media::mux::ToneAudio;
        let mut app = app();
        let mut clip = PatternClip::new(16, 8, 8, 25);
        clip.audio = Some(ToneAudio {
            rate: 48_000,
            channels: 1,
            freq_hz: 440.0,
            amplitude: 0.2,
        });
        insert_test_clip(app.world_mut(), "movies/tone.webm", clip.to_webm());
        let w = app.world_mut();
        create_player(
            w,
            &json!({ "source": "movies/tone.webm", "name": "tone", "clock": "audio", "autoplay": false }),
        )
        .expect("created");
        open(
            w,
            &json!({ "player": "tone", "source": "movies/tone.webm", "clock": "audio", "autoplay": false }),
        )
        .expect("open");
        select_track(w, &json!({ "player": "tone", "kind": "audio", "index": null })).expect("mute");
        let st = status(w, &json!({ "player": "tone" })).expect("status");
        assert_eq!(st["clock"], "game");
        assert_eq!(st["audio_track"], Value::Null);
        app.update();
        let listed = list(app.world_mut()).expect("list");
        let kinds: Vec<&str> = listed["events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|ev| ev["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"tracks_changed"), "{kinds:?}");
        let w = app.world_mut();
        select_track(w, &json!({ "player": "tone", "kind": "audio", "index": 1 })).expect("restore");
        let entity = find_player(w, "tone").expect("tone");
        assert!(!w.get::<VideoPlayerState>(entity).expect("state").audio_disabled());
    }

    /// `select_track` on a `.videolist.ron` reopens the entry that plays now, not entry 0.
    #[test]
    fn select_track_on_a_playlist_keeps_the_current_entry() {
        use regia_media::mux::ToneAudio;
        let mut app = app();
        let tone = Some(ToneAudio {
            rate: 48_000,
            channels: 1,
            freq_hz: 440.0,
            amplitude: 0.2,
        });
        let mut a = PatternClip::new(8, 8, 6, 60);
        a.audio = tone;
        let mut b = PatternClip::new(16, 8, 30, 60);
        b.audio = tone;
        let w = app.world_mut();
        insert_test_clip(w, "movies/la.webm", a.to_webm());
        insert_test_clip(w, "movies/lb.webm", b.to_webm());
        insert_test_clip(
            w,
            "movies/l.videolist.ron",
            br#"(format_version: 1, entries: ["movies/la.webm", "movies/lb.webm"], loop_list: false)"#
                .to_vec(),
        );
        create_player(
            w,
            &json!({ "source": "movies/l.videolist.ron", "name": "list", "clock": "lockstep" }),
        )
        .expect("created");
        let entity = find_player(app.world_mut(), "list").expect("list");
        let playing = |app: &App| {
            app.world()
                .get::<VideoPlayerState>(entity)
                .and_then(|s| s.playing().map(str::to_string))
        };
        for _ in 0..40 {
            if playing(&app).as_deref() == Some("movies/lb.webm") {
                break;
            }
            app.update();
        }
        assert_eq!(playing(&app).as_deref(), Some("movies/lb.webm"));
        app.update();
        select_track(
            app.world_mut(),
            &json!({ "player": "list", "kind": "audio", "index": 1 }),
        )
        .expect("select");
        assert_eq!(
            playing(&app).as_deref(),
            Some("movies/lb.webm"),
            "select_track restarted the playlist"
        );
        let width = app
            .world_mut()
            .get_mut::<VideoPlayerState>(entity)
            .and_then(|mut s| s.core_mut().presented().map(|p| p.width));
        assert_eq!(width, Some(16), "entry b stays on screen");
    }

    /// `select_track` reopens at the current position, but the component's `phase_offset` (the
    /// initial seek) is the game's: a later source change by component must start the new clip at
    /// the game's offset, not where the old clip was when the track changed (corr-2d-media LOW 7).
    #[test]
    fn select_track_keeps_the_components_initial_seek() {
        use regia_media::mux::ToneAudio;
        let mut app = app();
        let mut tone = PatternClip::new(16, 8, 60, 30);
        tone.audio = Some(ToneAudio {
            rate: 48_000,
            channels: 1,
            freq_hz: 440.0,
            amplitude: 0.2,
        });
        let w = app.world_mut();
        insert_test_clip(w, "movies/tone2.webm", tone.to_webm());
        insert_test_clip(w, "movies/c.webm", PatternClip::new(16, 8, 60, 30).to_webm());
        create_player(
            w,
            &json!({ "source": "movies/tone2.webm", "name": "t", "clock": "lockstep" }),
        )
        .expect("created");
        app.update();
        seek(app.world_mut(), &json!({ "player": "t", "time": 1.0 })).expect("seek");
        app.update();
        let entity = find_player(app.world_mut(), "t").expect("t");
        let position =
            |app: &mut App| with_core(app.world_mut(), entity, |c| Ok(c.position())).expect("position");
        assert!(
            position(&mut app) > 0.9,
            "the seek landed: {}",
            position(&mut app)
        );
        select_track(
            app.world_mut(),
            &json!({ "player": "t", "kind": "audio", "index": 1 }),
        )
        .expect("select");
        assert!(
            position(&mut app) > 0.9,
            "the track switch keeps the position: {}",
            position(&mut app)
        );
        let offset = app.world().get::<VideoPlayer>(entity).map(|p| p.phase_offset);
        assert_eq!(offset, Some(0.0), "the component's initial seek is unchanged");
        if let Some(mut p) = app.world_mut().get_mut::<VideoPlayer>(entity) {
            p.source = "movies/c.webm".to_string();
        }
        app.update();
        assert!(
            position(&mut app) < 0.5,
            "a new clip starts at the game's offset (0), got {}",
            position(&mut app)
        );
    }

    #[test]
    fn without_the_plugin() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let w = app.world_mut();
        assert_eq!(list(w).expect("list")["installed"], false);
        let err = status(w, &json!({ "player": 1 })).expect_err("refused");
        assert_eq!(reason(&err).as_deref(), Some("video_not_installed"));
        let err = create_player(w, &json!({ "source": "a.webm" })).expect_err("refused");
        assert_eq!(reason(&err).as_deref(), Some("video_not_installed"));
    }

    /// Spec 50 § 2.3: switching the audio track reopens the session at the current position.
    #[test]
    fn select_track_keeps_the_position() {
        use regia_media::mux::ToneAudio;
        let mut app = app();
        let mut clip = PatternClip::new(16, 8, 20, 25);
        clip.audio = Some(ToneAudio {
            rate: 48_000,
            channels: 1,
            freq_hz: 440.0,
            amplitude: 0.2,
        });
        insert_test_clip(app.world_mut(), "movies/tone.webm", clip.to_webm());
        let w = app.world_mut();
        create_player(
            w,
            &json!({ "source": "movies/tone.webm", "name": "tone", "clock": "lockstep", "autoplay": false }),
        )
        .expect("created");
        open(
            w,
            &json!({ "player": "tone", "source": "movies/tone.webm", "clock": "lockstep", "autoplay": false }),
        )
        .expect("open");
        seek(w, &json!({ "player": "tone", "frame": 10 })).expect("seek");
        assert_eq!(
            status(w, &json!({ "player": "tone" })).expect("status")["frame_index"],
            10
        );
        select_track(w, &json!({ "player": "tone", "kind": "audio", "index": null })).expect("mute");
        let st = status(w, &json!({ "player": "tone" })).expect("status");
        assert_eq!(st["frame_index"], 10, "reopened at the same frame: {st}");
        assert_eq!(st["audio_track"], Value::Null);
    }

    /// Spec 50 § 5.3: every protocol code has an `explain_error` entry under its wire name
    /// (`video_` + the code), including the warning-only `audio_bus_missing` and
    /// `material_missing_video_slot` that no method contract lists.
    #[test]
    fn explain_error_knows_every_video_code() {
        for code in MediaError::CODES.iter().chain(&["not_installed"]) {
            let wire = format!("video_{code}");
            let answer = crate::discovery::errors_explain(&json!({ "code": wire }))
                .unwrap_or_else(|e| panic!("{wire} has no explanation: {e:?}"));
            assert_eq!(answer["code"], wire.as_str());
            assert!(answer["fix"].as_str().is_some_and(|f| !f.is_empty()), "{wire}");
        }
    }

    /// Spec 50 § 5.3 / test 13: the open-time refusals reach the wire with their codes (patented,
    /// no decoder, broken container, over the picture cap).
    #[test]
    fn open_refusals_carry_their_wire_codes() {
        let mut app = app();
        let mut h264 = PatternClip::new(16, 8, 3, 25);
        h264.video_codec = "V_MPEG4/ISO/AVC".into();
        let mut vp9 = PatternClip::new(16, 8, 3, 25);
        vp9.video_codec = "V_VP9".into();
        let mut truncated = PatternClip::new(16, 8, 3, 25).to_webm();
        truncated.truncate(12);
        insert_test_clip(app.world_mut(), "movies/h264.webm", h264.to_webm());
        insert_test_clip(app.world_mut(), "movies/vp9.webm", vp9.to_webm());
        insert_test_clip(app.world_mut(), "movies/cut.webm", truncated);
        insert_test_clip(
            app.world_mut(),
            "movies/big.webm",
            PatternClip::new(64, 64, 3, 25).to_webm(),
        );
        app.world_mut()
            .resource_mut::<regia_media::plugin::MediaSettings>()
            .caps
            .max_width = 32;
        let w = app.world_mut();
        create_player(w, &json!({ "source": "", "name": "p" })).expect("created");
        for (source, wire) in [
            ("movies/h264.webm", "video_codec_patented"),
            ("movies/vp9.webm", "video_codec_unsupported"),
            ("movies/cut.webm", "video_container_invalid"),
            ("movies/big.webm", "video_limit_exceeded"),
        ] {
            let err = open(
                w,
                &json!({ "player": "p", "source": source, "clock": "lockstep" }),
            )
            .expect_err(source);
            assert_eq!(reason(&err).as_deref(), Some(wire), "{source}");
        }
        let probed = probe(w, &json!({ "source": "movies/h264.webm" })).expect("probe reports");
        assert_eq!(probed["supported"], false);
    }
}
