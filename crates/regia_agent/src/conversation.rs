// SPDX-License-Identifier: MPL-2.0
//! `conversation.*` and `captions.*` (`regia_framework::conversation`, `regia_ui::captions`,
//! ADR-1697, book `agents/conversation.md`).
//!
//! `conversation.play`, `.say`, `.choose` and `.stop` drive the [`ConversationRunner`];
//! `captions.state` and `captions.set_options` read and change the caption board and the
//! player's options; `conversation.validate` runs the `conv_*` validator in-process. Every method
//! answers `plugin_missing` when the caption or conversation plugin is not installed.

use regia_app::caption::{CaptionIds, CaptionKind};
use regia_app::prelude::*;
use regia_framework::conversation::{
    BusyPolicy, ConversationClock, ConversationError, ConversationLibrary, ConversationRunner,
    LibraryValidateEnv, PlayRequest, RunId, SayRequest, ValidateInput, WorldEnv, validate, with_localization,
    with_voice_player,
};
use regia_ui::Localization;
use regia_ui::captions::{CaptionBoard, CaptionClock, CaptionDeviceClock, CaptionOptions, OptionError};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn plugin_missing(name: &str) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "plugin_missing",
        format!("{name} is not installed"),
        "add regia_ui::captions::CaptionPlugin and regia_framework::conversation::ConversationPlugin to the app",
    )
}

fn error(e: &ConversationError) -> RpcError {
    let hint = match e {
        ConversationError::NotFound(_) => "list conversations with conversation.validate",
        ConversationError::LocaleNotLoaded(_) => "use a locale from ui.localization",
        ConversationError::LabelNotFound(_) => "name a Label step of the conversation",
        ConversationError::ChannelBusy(_) => "pass busy: replace or queue, or another channel",
        ConversationError::RunNotFound(_) => "take run ids from conversation.play",
        ConversationError::NoChoicePending => "wait for the run to offer a choice",
        ConversationError::ChoiceOutOfRange => "index into the offered options",
        ConversationError::TextTooLong => "keep `text` under 512 graphemes",
        ConversationError::SpeakerNotFound(_) => "add the speaker to speakers.ron",
        ConversationError::LineNotFound(_) => "use `<conversation>#<line>`",
        _ => "see the message",
    };
    let code = if matches!(
        e,
        ConversationError::NotFound(_)
            | ConversationError::RunNotFound(_)
            | ConversationError::LineNotFound(_)
            | ConversationError::SpeakerNotFound(_)
            | ConversationError::LabelNotFound(_)
    ) {
        NOT_FOUND
    } else {
        INVALID_PARAMS
    };
    RpcError::new(code, e.code(), e.to_string(), hint)
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a string"))),
    }
}

fn uint_param(params: &Value, name: &str) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a non-negative integer"))),
    }
}

/// An unsigned parameter that must fit `T`; an overflow is `invalid_params`, not saturated.
fn fit_param<T: TryFrom<u64>>(params: &Value, name: &str) -> Result<Option<T>, RpcError> {
    uint_param(params, name)?
        .map(|v| T::try_from(v).map_err(|_| RpcError::invalid_params(format!("`{name}` is out of range"))))
        .transpose()
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be true or false"))),
    }
}

fn installed(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<ConversationRunner>().is_none() {
        return Err(plugin_missing("the conversation plugin"));
    }
    if world.get_resource::<CaptionBoard>().is_none() {
        return Err(plugin_missing("the caption plugin"));
    }
    Ok(())
}

/// Runs `f` with the runner, an environment over the world, and the caption id counter.
///
/// `now` is the runner clock: [`ConversationClock`] whenever the app has a [`Time`] (always, for
/// an `App`: `App::new` installs it), else the caption clock — the same selection
/// `conversation_system` makes (Muse read-only review of fix-framework2-determinism, high
/// finding 3: feeding the runner the caption/device clock stamps agent-driven lines on a clock
/// the runner never ticks on). The voice player gets the caption clock, so an unvoiced line's
/// caption is stamped on the clock the board reads while deadlines stay on `now`
/// (corr-framework-a F4, the split `conversation_system` makes).
fn with_env<R>(
    world: &mut World,
    f: impl FnOnce(&mut ConversationRunner, &mut WorldEnv<'_>, &mut CaptionIds, u64) -> R,
) -> R {
    let caption_clock = world.get_resource::<CaptionClock>().map_or(0, |c| c.0);
    let now = if world.contains_resource::<Time>() {
        world.get_resource::<ConversationClock>().map_or(0, |c| c.0)
    } else {
        caption_clock
    };
    world.resource_scope(|world, mut runner: Mut<ConversationRunner>| {
        world.resource_scope(|world, mut ids: Mut<CaptionIds>| {
            world.resource_scope(|world, library: Mut<ConversationLibrary>| {
                with_localization(world, |world, loc| {
                    with_voice_player(world, caption_clock, |voice| {
                        let mut env = WorldEnv {
                            library: &library,
                            loc,
                            voice,
                        };
                        f(&mut runner, &mut env, &mut ids, now)
                    })
                })
            })
        })
    })
}

/// `conversation.play { conversation, locale?, from?, channel?, busy? }`.
pub(crate) fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let conversation = str_param(params, "conversation")?
        .ok_or_else(|| RpcError::invalid_params("`conversation` is required"))?;
    let mut request = PlayRequest::new(conversation);
    request.locale = str_param(params, "locale")?.map(str::to_string);
    request.from = str_param(params, "from")?.map(str::to_string);
    if let Some(channel) = str_param(params, "channel")? {
        request.channel = channel.to_string();
    }
    request.busy = match str_param(params, "busy")? {
        None | Some("fail") => BusyPolicy::Fail,
        Some("replace") => BusyPolicy::Replace,
        Some("queue") => BusyPolicy::Queue,
        Some(_) => return Err(RpcError::invalid_params("`busy` must be fail, replace or queue")),
    };
    let started = with_env(world, |runner, env, _, _| runner.play(request, &*env)).map_err(|e| error(&e))?;
    Ok(json!({
        "run": started.run.map(|r| r.0),
        "queued": started.run.is_none(),
        "locale": started.locale,
    }))
}

/// `conversation.say { line | text, speaker?, kind?, duration_ms?, priority?, voice?, locale? }`.
pub(crate) fn say(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let kind = match str_param(params, "kind")? {
        None | Some("speech") => None,
        Some("narration") => Some(CaptionKind::Narration),
        Some("sound") => Some(CaptionKind::Sound),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`kind` must be speech, narration or sound",
            ));
        }
    };
    let request = SayRequest {
        line: str_param(params, "line")?.map(str::to_string),
        text: str_param(params, "text")?.map(str::to_string),
        speaker: str_param(params, "speaker")?.map(str::to_string),
        kind,
        duration_ms: fit_param(params, "duration_ms")?,
        priority: fit_param(params, "priority")?,
        voice: str_param(params, "voice")?.map(str::to_string),
        locale: str_param(params, "locale")?.map(str::to_string),
    };
    let caption =
        with_env(world, |runner, env, ids, now| runner.say(request, now, env, ids)).map_err(|e| error(&e))?;
    let start_us = match caption.start {
        regia_app::caption::CaptionTime::AtMicros(t) => t,
        regia_app::caption::CaptionTime::Now => world.get_resource::<CaptionClock>().map_or(0, |c| c.0),
    };
    Ok(json!({
        "caption": caption.id.0,
        "start_us": start_us,
        "planned_end_us": start_us.saturating_add(caption.planned_us),
        "pieces": caption.pieces.len(),
    }))
}

/// `conversation.choose { run, index }`.
pub(crate) fn choose(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let run = uint_param(params, "run")?.ok_or_else(|| RpcError::invalid_params("`run` is required"))?;
    let index =
        uint_param(params, "index")?.ok_or_else(|| RpcError::invalid_params("`index` is required"))?;
    with_env(world, |runner, env, _, _| {
        runner.choose(RunId(run), index as usize, &*env)
    })
    .map_err(|e| error(&e))?;
    Ok(json!({ "run": run, "state": "running" }))
}

/// `conversation.stop { run, fade_ms? }`.
pub(crate) fn stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let run = uint_param(params, "run")?.ok_or_else(|| RpcError::invalid_params("`run` is required"))?;
    let fade: u32 = fit_param(params, "fade_ms")?.unwrap_or(0);
    let known = world
        .resource::<ConversationRunner>()
        .state()
        .iter()
        .any(|r| r.run.0 == run);
    if !known {
        return Err(error(&ConversationError::RunNotFound(run)));
    }
    world.resource_mut::<ConversationRunner>().stop(RunId(run), fade);
    Ok(json!({ "stopped": true }))
}

/// `captions.state { include_layout? }`.
pub(crate) fn state(world: &World, params: &Value) -> Result<Value, RpcError> {
    let Some(board) = world.get_resource::<CaptionBoard>() else {
        return Err(plugin_missing("the caption plugin"));
    };
    let include = bool_param(params, "include_layout")?.unwrap_or(true);
    let mut out = board.snapshot().to_json(include);
    if let Some(object) = out.as_object_mut() {
        let options = world
            .get_resource::<CaptionOptions>()
            .map_or(Value::Null, CaptionOptions::to_json);
        object.insert("options".into(), options);
        let on_device = world
            .get_resource::<CaptionDeviceClock>()
            .is_some_and(CaptionDeviceClock::following);
        object.insert(
            "clock_source".into(),
            json!(if on_device { "device" } else { "fixed" }),
        );
        object.insert("system_captions".into(), json!(regia_platform::system_captions()));
        let runs: Vec<Value> = world
            .get_resource::<ConversationRunner>()
            .map(|r| {
                r.state()
                    .iter()
                    .map(|run| {
                        json!({
                            "run": run.run.0,
                            "channel": run.channel,
                            "conversation": run.conversation,
                            "state": run.state,
                            "caption": run.caption.map(|c| c.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        object.insert("runs".into(), Value::Array(runs));
    }
    Ok(out)
}

fn option_error(e: &OptionError) -> RpcError {
    match e {
        OptionError::Unknown(name) => RpcError::new(
            INVALID_PARAMS,
            "invalid_option",
            e.to_string(),
            format!("`{name}` is not an option; see captions.state options"),
        ),
        OptionError::OutOfRange { name, min, max } => RpcError {
            code: INVALID_PARAMS,
            message: e.to_string(),
            data: Some(json!({
                "code": "out_of_range",
                "hint": format!("`{name}` accepts {min} to {max}"),
                "min": min,
                "max": max,
            })),
        },
        OptionError::InvalidColor(_) => RpcError::new(
            INVALID_PARAMS,
            "invalid_color",
            e.to_string(),
            "use white, yellow or #RRGGBB",
        ),
        OptionError::InvalidValue(_) => RpcError::invalid_params(e.to_string()),
    }
}

/// `captions.set_options { <any option>..., persist? }`.
pub(crate) fn set_options(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<CaptionOptions>().is_none() {
        return Err(plugin_missing("the caption plugin"));
    }
    let mut patch = params.as_object().cloned().unwrap_or_default();
    let persist = match patch.remove("persist").filter(|v| !v.is_null()) {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "bad_flag",
                "`persist` must be true or false",
                "pass a JSON boolean",
            )
        })?,
    };
    // All or nothing: a persist request without saved settings changes no option.
    if persist && !world.contains_resource::<regia_framework::settings::GameSettings>() {
        return Err(RpcError::new(
            UNSUPPORTED,
            "no_settings",
            "this app has no saved settings to write the caption options to",
            "add the framework SettingsPlugin, or call without `persist`",
        ));
    }
    let mut options = world.resource_mut::<CaptionOptions>();
    let changed = options.apply_patch(&patch).map_err(|e| option_error(&e))?;
    let options = options.clone();
    if persist {
        // The settings plugin writes the file on its next apply (spec 56 § 2.8).
        world
            .resource_mut::<regia_framework::settings::GameSettings>()
            .store_caption_options(&options);
    }
    Ok(json!({
        "options": options.to_json(),
        "changed": changed,
        "persisted": persist,
        "persist_requested": persist,
    }))
}

/// `conversation.validate { conversation?, locale?, strict? }`.
pub(crate) fn validate_method(world: &World, params: &Value) -> Result<Value, RpcError> {
    let Some(library) = world.get_resource::<ConversationLibrary>() else {
        return Err(plugin_missing("the conversation plugin"));
    };
    let Some(loc) = world.get_resource::<Localization>() else {
        return Err(RpcError::new(
            NOT_FOUND,
            "locale_not_loaded",
            "the project has no localization files",
            "add assets://localization/<locale>/*.ftl",
        ));
    };
    let only = str_param(params, "conversation")?;
    let locale = str_param(params, "locale")?;
    let strict = bool_param(params, "strict")?.unwrap_or(false);
    let conversations: Vec<_> = library
        .conversations
        .values()
        .filter(|c| only.is_none_or(|id| id == c.id))
        .cloned()
        .collect();
    if let Some(id) = only
        && conversations.is_empty()
    {
        return Err(error(&ConversationError::NotFound(id.to_string())));
    }
    let locales: Vec<String> = match locale {
        Some(l) if loc.available.iter().any(|a| a == l) => vec![l.to_string()],
        Some(l) => return Err(error(&ConversationError::LocaleNotLoaded(l.to_string()))),
        None => loc.available.clone(),
    };
    let env = LibraryValidateEnv { library, loc };
    let report = validate(
        &ValidateInput {
            conversations: &conversations,
            speakers: &library.speakers,
            settings: &library.settings,
            locales: &locales,
            source_locale: loc.fallback(),
            strict,
        },
        &env,
    );
    Ok(report.to_json())
}

#[cfg(test)]
mod tests {
    use regia_framework::conversation::{
        ConversationAsset, ConversationLine, ConversationPlugin, DurationSource, Speaker, VoiceSpec,
    };
    use regia_ui::captions::CaptionPlugin;

    use super::*;

    fn code(e: RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    fn line(id: &str) -> ConversationLine {
        ConversationLine {
            id: id.into(),
            speaker: "guard".into(),
            text: format!("k-{id}"),
            note: String::new(),
            emotion: None,
            tags: Vec::new(),
            voice: VoiceSpec::None,
            duration: DurationSource::Fixed(1500),
            cues: Vec::new(),
            priority: None,
            keep_breaks: false,
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((CaptionPlugin, ConversationPlugin));
        let mut library = ConversationLibrary::default();
        library
            .speakers
            .speakers
            .insert("guard".into(), Speaker::default());
        library.conversations.insert(
            "intro".into(),
            ConversationAsset {
                format_version: 1,
                id: "intro".into(),
                notes: String::new(),
                lines: vec![line("a"), line("b")],
                steps: Vec::new(),
            },
        );
        *app.world_mut().resource_mut::<ConversationLibrary>() = library;
        app
    }

    fn step_to(app: &mut App, micros: u64) {
        app.world_mut().resource_mut::<CaptionClock>().0 = micros;
        // The runner now uses fixed simulation time; the caption board uses presentation time.
        // This test driver advances both clocks explicitly without a time plugin.
        app.world_mut()
            .resource_mut::<regia_framework::conversation::ConversationClock>()
            .0 = micros;
        app.update();
    }

    #[test]
    fn every_method_answers_plugin_missing_without_the_plugins() {
        let mut world = World::new();
        let none = json!({});
        assert_eq!(
            code(play(&mut world, &json!({"conversation": "x"})).expect_err("an error")),
            "plugin_missing"
        );
        assert_eq!(
            code(say(&mut world, &json!({"text": "x"})).expect_err("an error")),
            "plugin_missing"
        );
        assert_eq!(
            code(choose(&mut world, &json!({"run": 1, "index": 0})).expect_err("an error")),
            "plugin_missing"
        );
        assert_eq!(
            code(stop(&mut world, &json!({"run": 1})).expect_err("an error")),
            "plugin_missing"
        );
        assert_eq!(
            code(state(&world, &none).expect_err("an error")),
            "plugin_missing"
        );
        assert_eq!(
            code(set_options(&mut world, &none).expect_err("an error")),
            "plugin_missing"
        );
        assert_eq!(
            code(validate_method(&world, &none).expect_err("an error")),
            "plugin_missing"
        );
    }

    #[test]
    fn say_refuses_priority_and_duration_that_overflow() {
        let mut app = app();
        assert!(say(app.world_mut(), &json!({"text": "x", "priority": 1000})).is_err());
        assert!(
            say(
                app.world_mut(),
                &json!({"text": "x", "duration_ms": 5_000_000_000_u64})
            )
            .is_err()
        );
        assert!(say(app.world_mut(), &json!({"text": "x", "priority": 255})).is_ok());
    }

    /// fixrev2-v048c-merge-audit (corr-framework-a F4 on the agent path): with engine audio the
    /// voice player stamps an unvoiced caption with its clock, which must be the caption clock
    /// the board reads, not the runner's `ConversationClock`. `with_env` passed the runner clock,
    /// so a `conversation.say` while the caption clock ran ahead was stamped seconds in the past.
    #[test]
    fn say_with_engine_audio_stamps_the_caption_clock() {
        let mut app = app();
        app.insert_resource(regia_audio::Audio::new(&regia_audio::AudioSettings {
            headless: true,
            ..regia_audio::AudioSettings::default()
        }));
        app.world_mut().resource_mut::<CaptionClock>().0 = 30_000_000;
        assert_eq!(
            app.world()
                .resource::<regia_framework::conversation::ConversationClock>()
                .0,
            0,
            "the runner clock stays behind the caption clock"
        );
        let out = say(
            app.world_mut(),
            &json!({"text": "Hello there", "duration_ms": 1500}),
        )
        .expect("say");
        assert_eq!(out["start_us"], 30_000_000, "{out}");
        assert_eq!(out["planned_end_us"], 31_500_000, "{out}");
    }

    #[test]
    fn say_then_steps_show_visible_then_done() {
        let mut app = app();
        let out = say(
            app.world_mut(),
            &json!({"text": "Hello there", "duration_ms": 1500}),
        )
        .expect("say");
        assert_eq!(out["planned_end_us"], 1_500_000);
        step_to(&mut app, 0);
        let snap = state(app.world(), &json!({})).expect("state");
        assert_eq!(snap["entries"][0]["state"], "Visible");
        assert_eq!(snap["entries"][0]["lines"][0], "Hello there");
        assert_eq!(snap["options"]["text_scale"], 1.0);
        step_to(&mut app, 1_600_000);
        let snap = state(app.world(), &json!({})).expect("state");
        assert_eq!(snap["entries"][0]["state"], "Done");
        step_to(&mut app, 1_700_000);
        let snap = state(app.world(), &json!({"include_layout": false})).expect("state");
        assert_eq!(snap["entries"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn play_runs_the_lines_and_a_busy_channel_is_refused() {
        let mut app = app();
        let out = play(app.world_mut(), &json!({"conversation": "intro"})).expect("play");
        assert_eq!(out["run"], 1);
        assert_eq!(out["queued"], false);
        assert_eq!(out["locale"], "en");
        let keys: Vec<&str> = out
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        // Exactly these three keys. serde_json is built without `preserve_order`, so a `Map`
        // iterates in key order whatever order `json!` wrote them in.
        assert_eq!(keys, ["locale", "queued", "run"]);
        let queued = play(
            app.world_mut(),
            &json!({"conversation": "intro", "busy": "queue"}),
        )
        .expect("queue");
        assert!(queued["run"].is_null());
        assert_eq!(queued["queued"], true);
        assert_eq!(queued["locale"], "en");
        let err = play(app.world_mut(), &json!({"conversation": "intro"})).expect_err("an error");
        assert_eq!(code(err), "channel_busy");
        step_to(&mut app, 0);
        let snap = state(app.world(), &json!({})).expect("state");
        assert_eq!(snap["entries"][0]["source"], "dialogue:1:intro#a");
        assert_eq!(snap["runs"][0]["state"], "line");
        step_to(&mut app, 1_500_000);
        let snap = state(app.world(), &json!({})).expect("state");
        let sources: Vec<_> = snap["entries"]
            .as_array()
            .map(|e| e.iter().map(|x| x["source"].clone()).collect())
            .unwrap_or_default();
        assert!(sources.contains(&json!("dialogue:1:intro#b")), "{sources:?}");
        let stopped = stop(app.world_mut(), &json!({"run": 1})).expect("stop");
        assert_eq!(stopped["stopped"], true);
        assert_eq!(
            code(stop(app.world_mut(), &json!({"run": 9})).expect_err("an error")),
            "run_not_found"
        );
    }

    #[test]
    fn play_and_choose_errors_carry_their_codes() {
        let mut app = app();
        assert_eq!(
            code(play(app.world_mut(), &json!({"conversation": "nope"})).expect_err("an error")),
            "conversation_not_found"
        );
        assert_eq!(
            code(
                play(
                    app.world_mut(),
                    &json!({"conversation": "intro", "from": "ghost"})
                )
                .expect_err("an error")
            ),
            "label_not_found"
        );
        assert_eq!(
            code(choose(app.world_mut(), &json!({"run": 5, "index": 0})).expect_err("an error")),
            "run_not_found"
        );
        play(app.world_mut(), &json!({"conversation": "intro"})).expect("play");
        assert_eq!(
            code(choose(app.world_mut(), &json!({"run": 1, "index": 0})).expect_err("an error")),
            "no_choice_pending"
        );
        assert_eq!(
            code(say(app.world_mut(), &json!({"text": "x", "speaker": "ghost"})).expect_err("an error")),
            "speaker_not_found"
        );
        assert_eq!(
            code(say(app.world_mut(), &json!({"line": "intro#zzz"})).expect_err("an error")),
            "line_not_found"
        );
        let long = "x".repeat(513);
        assert_eq!(
            code(say(app.world_mut(), &json!({"text": long})).expect_err("an error")),
            "text_too_long"
        );
    }

    /// Spec 56 § 5: `conversation.say` answers `invalid_params` without `line` or `text`, or with a
    /// `line` that is not `<id>#<line>`; `conversation.choose` answers `choice_out_of_range` for an
    /// index past the offered options.
    #[test]
    fn say_invalid_params_and_choose_out_of_range_carry_their_codes() {
        use regia_framework::conversation::{ChoiceOption, Step};

        let mut app = app();
        assert_eq!(
            code(say(app.world_mut(), &json!({})).expect_err("neither line nor text")),
            "invalid_params"
        );
        assert_eq!(
            code(say(app.world_mut(), &json!({"line": "no-hash"})).expect_err("bad line id")),
            "invalid_params"
        );

        app.world_mut()
            .resource_mut::<ConversationLibrary>()
            .conversations
            .insert(
                "pick".into(),
                ConversationAsset {
                    format_version: 1,
                    id: "pick".into(),
                    notes: String::new(),
                    lines: vec![line("a"), line("b")],
                    steps: vec![
                        Step::Choice {
                            prompt: None,
                            options: vec![
                                ChoiceOption {
                                    text: "opt-a".into(),
                                    cond: None,
                                    goto: "la".into(),
                                },
                                ChoiceOption {
                                    text: "opt-b".into(),
                                    cond: None,
                                    goto: "lb".into(),
                                },
                            ],
                        },
                        Step::Label("la".into()),
                        Step::Line("a".into()),
                        Step::End,
                        Step::Label("lb".into()),
                        Step::Line("b".into()),
                        Step::End,
                    ],
                },
            );
        let out = play(app.world_mut(), &json!({"conversation": "pick"})).expect("play");
        let run = out["run"].as_u64().expect("run id");
        step_to(&mut app, 0);
        assert_eq!(
            code(choose(app.world_mut(), &json!({"run": run, "index": 2})).expect_err("out of range")),
            "choice_out_of_range"
        );
        // A valid index still works after the refusal.
        let chosen = choose(app.world_mut(), &json!({"run": run, "index": 1})).expect("choose");
        assert_eq!(chosen["run"], run);
    }

    #[test]
    fn set_options_validates_ranges_and_colors() {
        let mut app = app();
        let err = set_options(app.world_mut(), &json!({"text_scale": 3})).expect_err("an error");
        assert_eq!(err.data.as_ref().and_then(|d| d.get("min")), Some(&json!(0.75)));
        assert_eq!(code(err), "out_of_range");
        assert_eq!(
            code(set_options(app.world_mut(), &json!({"bogus": 1})).expect_err("an error")),
            "invalid_option"
        );
        assert_eq!(
            code(set_options(app.world_mut(), &json!({"text_color": "#12"})).expect_err("an error")),
            "invalid_color"
        );
        let out =
            set_options(app.world_mut(), &json!({"text_scale": 1.5, "caption_mode": true})).expect("set");
        assert_eq!(out["changed"], json!(["caption_mode", "text_scale"]));
        assert_eq!(out["options"]["caption_mode"], true);
    }

    #[test]
    fn persist_writes_the_options_into_the_settings() {
        let mut app = app();
        assert_eq!(
            code(
                set_options(app.world_mut(), &json!({"text_scale": 1.25, "persist": true}))
                    .expect_err("no settings")
            ),
            "no_settings"
        );
        assert_eq!(
            app.world().resource::<CaptionOptions>().text_scale,
            1.0,
            "all or nothing"
        );
        assert_eq!(
            code(set_options(app.world_mut(), &json!({"persist": "yes"})).expect_err("bad flag")),
            "bad_flag"
        );
        app.world_mut()
            .insert_resource(regia_framework::settings::GameSettings::default());
        let out = set_options(
            app.world_mut(),
            &json!({"text_scale": 1.25, "caption_mode": true, "persist": true}),
        )
        .expect("persist");
        assert_eq!(out["persisted"], true);
        let settings = app.world().resource::<regia_framework::settings::GameSettings>();
        assert_eq!(settings.captions.as_ref().map(|c| c.text_scale), Some(1.25));
        assert_eq!(
            settings.accessibility.effective_subtitle_mode(),
            regia_framework::settings::SubtitleMode::All
        );
        let out = set_options(app.world_mut(), &json!({"text_scale": 1.5})).expect("no persist");
        assert_eq!(out["persisted"], false);
        assert_eq!(
            app.world()
                .resource::<regia_framework::settings::GameSettings>()
                .captions
                .as_ref()
                .map(|c| c.text_scale),
            Some(1.25),
            "a call without persist leaves the saved options"
        );
    }

    #[test]
    fn validate_returns_the_validator_issues() {
        let mut app = app();
        // No Localization: the method says so with a code instead of crashing.
        assert_eq!(
            code(validate_method(app.world(), &json!({})).expect_err("an error")),
            "locale_not_loaded"
        );
        let loc = Localization::new(
            vec![("en".into(), "k-a = Hello\nk-b = World\n".into())],
            Some("en".into()),
        )
        .expect("loc");
        app.world_mut().insert_resource(loc);
        let out = validate_method(app.world(), &json!({})).expect("validate");
        assert!(out["counts"]["total"].is_number());
        assert_eq!(
            code(validate_method(app.world(), &json!({"conversation": "nope"})).expect_err("an error")),
            "conversation_not_found"
        );
        assert_eq!(
            code(validate_method(app.world(), &json!({"locale": "xx"})).expect_err("an error")),
            "locale_not_loaded"
        );
    }
}
