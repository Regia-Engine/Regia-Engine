// SPDX-License-Identifier: MPL-2.0
//! `subtitle_sets.*`: cue sets on the caption board (book `agents/dialogue-subtitles.md`, spec 36
//! § 10, ADR-1882): queue timed cues without a sound or a conversation, watch them and stop them.
//! `captions.state` shows the same captions with their layout.

use regia_app::caption::CaptionId;
use regia_app::prelude::*;
use regia_asset::dialogue::Cue;
use regia_ui::captions::{
    CaptionBoard, CaptionClock, CaptionOptions, SetRequest, SubtitleKind, SubtitleSets, queue_set,
    stop_all_sets, stop_set, sync_set,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn unavailable() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "subtitle_sets_unavailable",
        "the caption board is not installed",
        "add regia_ui::captions::CaptionPlugin (DefaultPlugins with the ui feature has it)",
    )
}

fn bad_cues(message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "subtitle_sets_bad_cues",
        message,
        "cues are [{ at, until?, text_key | text }] in time order; duration is above 0",
    )
}

fn installed(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<SubtitleSets>() && world.contains_resource::<CaptionBoard>() {
        Ok(())
    } else {
        Err(unavailable())
    }
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name).filter(|v| !v.is_null()) {
        None => Ok(None),
        // Times are f32 seconds in the cue format.
        Some(v) => v
            .as_f64()
            .map(|n| Some(n as f32))
            .ok_or_else(|| bad_cues(format!("`{name}` must be a number"))),
    }
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be true or false"))),
    }
}

fn request(params: &Value) -> Result<SetRequest, RpcError> {
    let cues = params
        .get("cues")
        .filter(|v| !v.is_null())
        .ok_or_else(|| bad_cues("`cues` is required"))?;
    let cues: Vec<Cue> =
        serde_json::from_value(cues.clone()).map_err(|e| bad_cues(format!("`cues`: {e}")))?;
    let mut req = SetRequest {
        cues,
        duration: number(params, "duration")?.ok_or_else(|| bad_cues("`duration` is required"))?,
        ..SetRequest::default()
    };
    req.start_delay = number(params, "start_delay")?.unwrap_or(0.0);
    req.keep_breaks = flag(params, "keep_breaks")?.unwrap_or(false);
    req.one_at_a_time = flag(params, "one_at_a_time")?.unwrap_or(false);
    if let Some(p) = params.get("priority").filter(|v| !v.is_null()) {
        req.priority = p
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| bad_cues("`priority` must be an integer"))?;
    }
    req.kind = match crate::params::opt_str(params, "kind")? {
        None | Some("dialogue") => SubtitleKind::Dialogue,
        Some("caption") => SubtitleKind::Caption,
        Some(other) => return Err(bad_cues(format!("`kind` {other:?} is not dialogue or caption"))),
    };
    req.speaker = crate::params::opt_str(params, "speaker")?.map(str::to_owned);
    if let Some(bits) = params.get("owner").filter(|v| !v.is_null()) {
        req.owner = Some(
            bits.as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`owner` must be entity bits"))?,
        );
    }
    Ok(req)
}

/// `subtitle_sets.status {}`.
pub(crate) fn status(world: &World) -> Result<Value, RpcError> {
    installed(world)?;
    let sets = world.resource::<SubtitleSets>();
    let snapshot = world.resource::<CaptionBoard>().snapshot();
    let options = world.get_resource::<CaptionOptions>();
    let enabled = options.is_none_or(|o| o.enabled);
    let mode = match options {
        Some(o) if !o.enabled => "off",
        Some(o) if o.caption_mode => "all",
        _ => "dialogue",
    };
    let clock = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    let active: Vec<Value> = sets
        .iter()
        .map(|set| {
            json!({
                "id": set.id.0,
                "priority": set.priority,
                "owner": set.owner.map(Entity::to_bits),
                "elapsed": clock.saturating_sub(set.start_us) as f64 / 1_000_000.0,
            })
        })
        .collect();
    let entry = |id: CaptionId| {
        snapshot
            .entries
            .iter()
            .find(|e| e["id"] == id.0 && e["state"] == "Visible")
    };
    let current = sets
        .iter()
        .filter_map(|set| entry(set.id).map(|e| (set.priority, e)))
        // The highest priority; ties to the set queued first.
        .min_by_key(|(priority, _)| std::cmp::Reverse(*priority))
        .map(|(_, e)| json!({ "lines": e["lines"], "page": e["page"], "speaker": e["speaker"] }));
    Ok(json!({ "enabled": enabled, "mode": mode, "active": active, "current": current }))
}

/// `subtitle_sets.queue { cues, duration, priority?, start_delay?, keep_breaks?, one_at_a_time?, kind?, speaker?, owner? }`.
pub(crate) fn queue(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let req = request(params)?;
    let id = queue_set(world, &req).map_err(bad_cues)?;
    Ok(json!({ "id": id.map(|id| id.0) }))
}

fn unknown(id: &Value) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "subtitle_sets_unknown_id",
        format!("no live set {id}"),
        "subtitle_sets.status lists the live sets; finished ones are gone",
    )
}

/// `subtitle_sets.stop { id? }` (absent id: every set).
pub(crate) fn stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let Some(id) = params.get("id").filter(|v| !v.is_null()) else {
        return Ok(json!({ "stopped": stop_all_sets(world) }));
    };
    let bits = id.as_u64().ok_or_else(|| unknown(id))?;
    if stop_set(world, CaptionId(bits)) {
        Ok(json!({ "stopped": 1 }))
    } else {
        Err(unknown(id))
    }
}

/// `subtitle_sets.sync { id, playhead }`: sets a live set's time to a voice playhead (seconds of cue
/// time). Within 40 ms nothing changes (`in_sync`); otherwise the set is sent again from the
/// playhead under the same id (`rebased`); at or past its duration it stops (`finished`).
pub(crate) fn sync(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let id = params
        .get("id")
        .filter(|v| !v.is_null())
        .ok_or_else(|| RpcError::invalid_params("`id` is required"))?;
    let bad_playhead = |message: String| {
        RpcError::new(
            INVALID_PARAMS,
            "subtitle_sets_bad_playhead",
            message,
            "playhead is the voice position in seconds of cue time, 0 or more",
        )
    };
    let playhead = params
        .get("playhead")
        .filter(|v| !v.is_null())
        .ok_or_else(|| bad_playhead("`playhead` is required".to_owned()))?
        .as_f64()
        .ok_or_else(|| bad_playhead("`playhead` must be a number".to_owned()))?;
    let bits = id.as_u64().ok_or_else(|| unknown(id))?;
    // Times are f32 seconds in the cue format.
    match sync_set(world, CaptionId(bits), playhead as f32).map_err(bad_playhead)? {
        Some(outcome) => Ok(json!({ "outcome": outcome.name() })),
        None => Err(unknown(id)),
    }
}

/// `subtitle_sets.set_enabled { on }`: the board's `enabled` option. Off drops speech rows, including rows
/// already on screen, so their sets end (spec 56 § 2.8, ADR-5778).
pub(crate) fn set_enabled(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let on = flag(params, "on")?.ok_or_else(|| RpcError::invalid_params("`on` is required"))?;
    let mut options = world
        .get_resource_mut::<CaptionOptions>()
        .ok_or_else(unavailable)?;
    options.enabled = on;
    Ok(json!({ "enabled": on }))
}

#[cfg(test)]
mod tests {
    use regia_ui::captions::CaptionPlugin;

    use super::*;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{err:?}"))
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(CaptionPlugin);
        app
    }

    fn cues() -> Value {
        json!({ "cues": [{ "at": 0.0, "until": 1.0, "text": "One" }, { "at": 2.0, "text": "Two" }],
                "duration": 3.0, "priority": 2, "speaker": "guard" })
    }

    #[test]
    fn queue_status_and_stop() {
        let mut app = app();
        let id = queue(app.world_mut(), &cues()).expect("queue")["id"]
            .as_u64()
            .expect("id");
        app.update();
        let out = status(app.world()).expect("status");
        assert_eq!(out["enabled"], true);
        assert_eq!(out["mode"], "dialogue");
        assert_eq!(out["active"][0]["id"], id);
        assert_eq!(out["active"][0]["priority"], 2);
        assert_eq!(out["current"]["lines"][0], "One");
        assert_eq!(out["current"]["speaker"], "guard");
        app.world_mut().resource_mut::<CaptionClock>().0 = 1_500_000;
        app.update();
        let out = status(app.world()).expect("status");
        assert_eq!(out["current"]["lines"][0], "", "a gap between cues");
        let out = stop(app.world_mut(), &json!({ "id": id })).expect("stop");
        assert_eq!(out["stopped"], 1);
        let err = stop(app.world_mut(), &json!({ "id": id })).expect_err("gone");
        assert_eq!(code(&err), "subtitle_sets_unknown_id");
        assert_eq!(stop(app.world_mut(), &json!({})).expect("all")["stopped"], 0);
        app.update();
        assert_eq!(status(app.world()).expect("status")["current"], Value::Null);
    }

    #[test]
    fn sync_rebases_a_set_and_reports_each_outcome() {
        let mut app = app();
        let id = queue(app.world_mut(), &cues()).expect("queue")["id"]
            .as_u64()
            .expect("id");
        app.update();
        let call = |app: &mut App, playhead: Value| {
            sync(app.world_mut(), &json!({ "id": id, "playhead": playhead }))
        };
        assert_eq!(call(&mut app, json!(0.0)).expect("sync")["outcome"], "in_sync");
        assert_eq!(call(&mut app, json!(2.5)).expect("sync")["outcome"], "rebased");
        app.update();
        let out = status(app.world()).expect("status");
        assert_eq!(out["current"]["lines"][0], "Two");
        assert_eq!(out["active"][0]["id"], id, "the same id after a re-base");
        assert_eq!(
            code(&call(&mut app, json!(-1.0)).expect_err("negative")),
            "subtitle_sets_bad_playhead"
        );
        assert_eq!(
            code(&call(&mut app, json!("soon")).expect_err("text")),
            "subtitle_sets_bad_playhead"
        );
        assert_eq!(
            code(&sync(app.world_mut(), &json!({ "id": id })).expect_err("missing")),
            "subtitle_sets_bad_playhead"
        );
        assert!(
            sync(app.world_mut(), &json!({ "playhead": 1.0 })).is_err(),
            "id is required"
        );
        assert_eq!(call(&mut app, json!(3.0)).expect("sync")["outcome"], "finished");
        assert_eq!(
            code(&call(&mut app, json!(1.0)).expect_err("gone")),
            "subtitle_sets_unknown_id"
        );
        let mut empty = World::new();
        assert_eq!(
            code(&sync(&mut empty, &json!({ "id": 1, "playhead": 0.0 })).expect_err("none")),
            "subtitle_sets_unavailable"
        );
    }

    #[test]
    fn queue_takes_one_at_a_time() {
        let mut app = app();
        let mut params = cues();
        params["one_at_a_time"] = json!(true);
        queue(app.world_mut(), &params).expect("queue");
        app.update();
        let snapshot = app.world().resource::<CaptionBoard>().snapshot();
        assert_eq!(snapshot.entries[0]["one_at_a_time"], true);
        let mut bad = cues();
        bad["one_at_a_time"] = json!("yes");
        assert!(queue(app.world_mut(), &bad).is_err());
    }

    #[test]
    fn set_enabled_and_mode() {
        let mut app = app();
        assert_eq!(
            set_enabled(app.world_mut(), &json!({ "on": false })).expect("off")["enabled"],
            false
        );
        assert_eq!(status(app.world()).expect("status")["mode"], "off");
        assert!(set_enabled(app.world_mut(), &json!({})).is_err());
        app.world_mut().resource_mut::<CaptionOptions>().enabled = true;
        app.world_mut().resource_mut::<CaptionOptions>().caption_mode = true;
        assert_eq!(status(app.world()).expect("status")["mode"], "all");
    }

    #[test]
    fn bad_requests_carry_their_codes() {
        let mut app = app();
        let bad = |app: &mut App, params: Value| code(&queue(app.world_mut(), &params).expect_err("bad"));
        for params in [
            json!({ "duration": 1.0 }),
            json!({ "cues": [] }),
            json!({ "cues": [], "duration": 0 }),
            json!({ "cues": "x", "duration": 1 }),
            json!({ "cues": [{ "at": 0.0 }], "duration": 1 }),
            json!({ "cues": [{ "at": 0.0, "text": "a", "extra": 1 }], "duration": 1 }),
            json!({ "cues": [], "duration": 1 }),
            json!({ "cues": [{ "at": 0.0, "text": "a" }], "duration": 1, "kind": "music" }),
        ] {
            assert_eq!(bad(&mut app, params), "subtitle_sets_bad_cues");
        }
        let never = json!({ "cues": [{ "at": 0.0, "text": "a" }], "duration": 1, "priority": 0 });
        let out = queue(app.world_mut(), &never).expect("zero");
        assert_eq!(out["id"], Value::Null, "priority 0 is never shown");
        let mut empty = World::new();
        assert_eq!(
            code(&status(&empty).expect_err("none")),
            "subtitle_sets_unavailable"
        );
        assert_eq!(
            code(&stop(&mut empty, &json!({})).expect_err("none")),
            "subtitle_sets_unavailable"
        );
    }
}
