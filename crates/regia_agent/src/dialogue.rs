// SPDX-License-Identifier: MPL-2.0
//! `dialogue.*`: dialogue lines from `.dialogue.ron` (book `agents/dialogue-subtitles.md`, spec 36 § 10,
//! ADR-1882): list the loaded dialogues and voices, resolve a line without playing it, play and stop
//! lines. The subtitles of a playing line show on the caption board (`captions.state`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::dialogue::{
    DialogueError, DialogueHandle, DialogueLibrary, DialoguePlayback, DialogueRequest, play_line,
    resolve_line, stop_line,
};
use regia_ui::Localization;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn unavailable() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "dialogue_unavailable",
        "the dialogue player is not installed",
        "add regia_framework::dialogue::DialoguePlugin (DefaultPlugins with the ui feature has it)",
    )
}

fn error(e: &DialogueError) -> RpcError {
    let (kind, hint) = match e {
        DialogueError::UnknownAsset(_) => (NOT_FOUND, "dialogue.list names the loaded dialogues"),
        DialogueError::UnknownLine(..) => (NOT_FOUND, "dialogue.list { dialogue } lists its lines"),
        DialogueError::UnknownVoice(_) => (
            NOT_FOUND,
            "add a .voice.ron for it; dialogue.list lists the voices",
        ),
        DialogueError::NoVariant(..) => (
            INVALID_PARAMS,
            "pass a speaker or listeners a variant fits, or add a variant without speaker",
        ),
        DialogueError::BadHandle(_) => (
            NOT_FOUND,
            "take handles from dialogue.play; finished lines are gone",
        ),
    };
    RpcError::new(kind, e.code(), e.to_string(), hint)
}

fn invalid(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn library(world: &World) -> Result<&DialogueLibrary, RpcError> {
    world.get_resource::<DialogueLibrary>().ok_or_else(unavailable)
}

fn opt_str<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a string"))),
    }
}

fn request(params: &Value) -> Result<DialogueRequest, RpcError> {
    let dialogue = opt_str(params, "dialogue")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`dialogue` must be a non-empty string"))?;
    let line = opt_str(params, "line")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`line` must be a non-empty string"))?;
    let mut req = DialogueRequest::new(dialogue, line);
    req.speaker = opt_str(params, "speaker")?.map(str::to_owned);
    if let Some(listeners) = params.get("listeners").filter(|v| !v.is_null()) {
        let list = listeners
            .as_array()
            .ok_or_else(|| RpcError::invalid_params("`listeners` must be an array of voice ids"))?;
        let ids = list
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| RpcError::invalid_params("`listeners` must be an array of voice ids"))?;
        req.listeners = Some(ids);
    }
    if let Some(p) = params.get("priority").filter(|v| !v.is_null()) {
        req.priority = p
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`priority` must be an integer 0 to 255"))?;
    }
    if let Some(p) = params.get("position").filter(|v| !v.is_null()) {
        let xyz = p
            .as_array()
            .filter(|a| a.len() == 3)
            .and_then(|a| a.iter().map(Value::as_f64).collect::<Option<Vec<f64>>>())
            .ok_or_else(|| RpcError::invalid_params("`position` must be [x, y, z]"))?;
        // Positions are f32 in the engine.
        let position = Vec3::new(xyz[0] as f32, xyz[1] as f32, xyz[2] as f32);
        if !position.is_finite() {
            return Err(RpcError::invalid_params(
                "`position` must contain three finite f32 coordinates",
            ));
        }
        req.position = Some(position);
    }
    Ok(req)
}

/// `dialogue.list { dialogue? }`.
pub(crate) fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let lib = library(world)?;
    let only = opt_str(params, "dialogue")?;
    if let Some(name) = only
        && !lib.dialogues.contains_key(name)
    {
        return Err(error(&DialogueError::UnknownAsset(name.to_owned())));
    }
    let dialogues: Vec<Value> = lib
        .dialogues
        .iter()
        .filter(|(name, _)| only.is_none_or(|o| o == name.as_str()))
        .map(|(name, d)| {
            json!({
                "name": name,
                "lines": d.lines.iter().map(|l| json!({
                    "id": l.id,
                    "speaker": l.speaker,
                    "listeners": l.listeners,
                    "text_key": l.text_key(name),
                    "display_key": l.display_key,
                    "mature": l.mature,
                    "variants": l.variants.len(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let voices: Vec<Value> = lib
        .voices
        .values()
        .map(|v| {
            json!({
                "id": v.id, "name_key": v.name_key,
                "gender": v.gender.as_str(), "number": v.number.as_str(),
            })
        })
        .collect();
    Ok(json!({
        "dialogues": dialogues,
        "voices": voices,
        "subtitle_sets": lib.subtitles.keys().collect::<Vec<_>>(),
        "errors": lib.errors,
    }))
}

/// `dialogue.resolve { dialogue, line, speaker?, listeners? }`.
pub(crate) fn resolve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let req = request(params)?;
    let lib = library(world)?;
    let r = resolve_line(lib, world.get_resource::<Localization>(), &req).map_err(|e| error(&e))?;
    let args: serde_json::Map<String, Value> = r.args.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    Ok(json!({
        "variant": r.variant,
        "sound": r.sound,
        "resolved_sound": r.resolved_sound,
        "text": r.text,
        "display_text": r.display_text,
        "args": args,
        "speaker": r.speaker,
        "listeners": r.listeners,
        "locale": r.locale,
        "cues": r.cues.iter().map(|c| json!({
            "start_us": c.start_us, "end_us": c.end_us, "text_key": c.text_key, "text": c.text,
        })).collect::<Vec<_>>(),
    }))
}

/// `dialogue.play { dialogue, line, speaker?, listeners?, priority?, position? }`.
pub(crate) fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let req = request(params)?;
    library(world)?;
    let handle = play_line(world, &req).map_err(|e| error(&e))?;
    let playing = world
        .get_resource::<DialoguePlayback>()
        .and_then(|p| p.active.get(&handle).cloned());
    Ok(json!({
        "handle": handle.0,
        "sound": playing.as_ref().map(|p| p.sound.clone()),
        "caption": playing.as_ref().and_then(|p| p.caption.map(|c| c.0)),
        "start_us": playing.as_ref().map(|p| p.start_us),
        "end_us": playing.as_ref().map(|p| p.end_us),
    }))
}

/// `dialogue.stop { handle?, fade_ms? }` (absent handle: every line).
pub(crate) fn stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    library(world)?;
    let handle = match params.get("handle").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(DialogueHandle(v.as_u64().ok_or_else(|| {
            invalid(
                "dialogue_bad_handle",
                "`handle` must be a playback handle",
                "take handles from dialogue.play",
            )
        })?)),
    };
    let fade_ms = match params.get("fade_ms").filter(|v| !v.is_null()) {
        None => 0,
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| RpcError::invalid_params("`fade_ms` must be a non-negative integer"))?,
    };
    let stopped = stop_line(world, handle, fade_ms).map_err(|e| error(&e))?;
    Ok(json!({ "stopped": stopped }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_position_is_rejected() {
        let mut world = world();
        for position in [[1.0e39, 0.0, 0.0], [0.0, -1.0e39, 0.0], [0.0, 0.0, 1.0e39]] {
            let err = play(
                &mut world,
                &json!({
                    "dialogue": "intro", "line": "hello", "position": position,
                }),
            )
            .expect_err("position outside f32");
            assert_eq!(err.code, INVALID_PARAMS);
            assert!(err.message.contains("position"));
            assert!(
                !world.contains_resource::<DialoguePlayback>(),
                "no playback was started"
            );
        }
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{err:?}"))
    }

    fn world() -> World {
        let root = regia_test_utils::unique_temp_dir("agent-dialogue");
        std::fs::create_dir_all(root.join("dialogue")).expect("mkdir");
        std::fs::write(
            root.join("dialogue/intro.dialogue.ron"),
            r#"(format_version: 1, name: "intro", lines: [
                (id: "hello", speaker: "ranger", variants: [(sound: "audio/hello.ogg")]),
                (id: "guard_only", speaker: "ranger", variants: [(speaker: Some("guard"), sound: "audio/g.ogg")]),
            ])"#,
        )
        .expect("write");
        std::fs::write(
            root.join("dialogue/ranger.voice.ron"),
            r#"(format_version: 1, id: "ranger", name_key: "voice-ranger", gender: Feminine)"#,
        )
        .expect("write");
        let lib = DialogueLibrary::load_dir(&root);
        let _ = std::fs::remove_dir_all(&root);
        let mut world = World::new();
        world.insert_resource(lib);
        world.insert_resource(
            Localization::new(
                vec![(
                    "en".to_owned(),
                    "dlg-intro-hello = Hi from { $speaker }.\nvoice-ranger = Ranger\n".to_owned(),
                )],
                Some("en".to_owned()),
            )
            .expect("loc"),
        );
        world
    }

    #[test]
    fn list_resolve_play_and_stop() {
        let mut w = world();
        let out = list(&mut w, &json!({})).expect("list");
        assert_eq!(out["dialogues"][0]["name"], "intro");
        assert_eq!(out["dialogues"][0]["lines"][0]["text_key"], "dlg-intro-hello");
        assert_eq!(out["voices"][0]["gender"], "feminine");
        let out = resolve(&mut w, &json!({ "dialogue": "intro", "line": "hello" })).expect("resolve");
        assert_eq!(out["text"], "Hi from Ranger.");
        assert_eq!(out["args"]["speaker_gender"], "feminine");
        let out = play(
            &mut w,
            &json!({ "dialogue": "intro", "line": "hello", "position": [1, 2, 3] }),
        )
        .expect("play");
        let handle = out["handle"].as_u64().expect("handle");
        assert!(out["caption"].is_u64());
        let out = stop(&mut w, &json!({ "handle": handle })).expect("stop");
        assert_eq!(out["stopped"], 1);
        let err = stop(&mut w, &json!({ "handle": handle })).expect_err("gone");
        assert_eq!(code(&err), "dialogue_bad_handle");
        assert_eq!(stop(&mut w, &json!({})).expect("all")["stopped"], 0);
    }

    #[test]
    fn errors_carry_their_codes() {
        let mut w = world();
        let code_of = |w: &mut World, p: Value| code(&resolve(w, &p).expect_err("an error"));
        assert_eq!(
            code_of(&mut w, json!({ "dialogue": "nope", "line": "x" })),
            "dialogue_unknown_asset"
        );
        assert_eq!(
            code_of(&mut w, json!({ "dialogue": "intro", "line": "x" })),
            "dialogue_unknown_line"
        );
        assert_eq!(
            code_of(
                &mut w,
                json!({ "dialogue": "intro", "line": "hello", "speaker": "ghost" })
            ),
            "dialogue_unknown_voice"
        );
        assert_eq!(
            code_of(&mut w, json!({ "dialogue": "intro", "line": "guard_only" })),
            "dialogue_no_variant"
        );
        assert_eq!(
            code(&list(&mut w, &json!({ "dialogue": "nope" })).expect_err("unknown")),
            "dialogue_unknown_asset"
        );
        let mut empty = World::new();
        assert_eq!(
            code(&list(&mut empty, &json!({})).expect_err("none")),
            "dialogue_unavailable"
        );
        assert_eq!(
            code(&stop(&mut empty, &json!({})).expect_err("none")),
            "dialogue_unavailable"
        );
    }
}
