// SPDX-License-Identifier: MPL-2.0
//! `ui.localization` and `ui.subtitles` (`regia_ui::loc_*`, `regia_ui::subtitles`, ADR-1570,
//! book `agents/ui-localization.md`).
//!
//! `ui.localization` reports the loaded locales' key coverage and previews pseudo-localization of
//! a string. `ui.subtitles` lists, pushes, advances and cancels subtitle lines on the
//! [`SubtitleQueue`] resource; the queue has no clock of its own, so an agent advances it with
//! `advance { to }` or `advance { by }`.

use std::collections::BTreeSet;

use regia_app::prelude::*;
use regia_ui::Localization;
use regia_ui::loc_pseudo::{KEY_LOCALE, PSEUDO_LOCALE, PseudoOptions, pseudo_localize};
use regia_ui::subtitles::{SubtitleError, SubtitleEvent, SubtitleId, SubtitleLine, SubtitleQueue};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Longest text `ui.localization { action: "pseudo" }` accepts, in bytes.
const MAX_PSEUDO_BYTES: usize = 65_536;

/// Locale keys listed per locale in a report (the count is always exact).
const MAX_LISTED_KEYS: usize = 50;

/// Upper bound for `max_concurrent`.
const MAX_CONCURRENT_LIMIT: u64 = 64;

fn action_param<'a>(params: &'a Value, default: &'a str) -> Result<&'a str, RpcError> {
    match params.get("action") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`action` must be a string")),
    }
}

fn number_param(params: &Value, name: &str) -> Result<Option<f64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number"))),
    }
}

fn string_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a string"))),
    }
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

/// `ui.localization { action?, text?, expansion?, accents?, brackets? }`.
pub(crate) fn localization(world: &World, params: &Value) -> Result<Value, RpcError> {
    match action_param(params, "report")? {
        "report" => report(world),
        "pseudo" => pseudo(params),
        _ => Err(RpcError::invalid_params(
            "`action` must be \"report\" or \"pseudo\"",
        )),
    }
}

fn report(world: &World) -> Result<Value, RpcError> {
    let Some(loc) = world.get_resource::<Localization>() else {
        return Err(RpcError::new(
            NOT_FOUND,
            "no_localization",
            "the project has no localization files",
            "add assets://localization/<locale>/*.ftl",
        ));
    };
    let mut universe: BTreeSet<&str> = BTreeSet::new();
    for locale in &loc.available {
        universe.extend(loc.keys_in(locale));
    }
    let rows: Vec<Value> = loc
        .available
        .iter()
        .map(|locale| {
            let own: BTreeSet<&str> = loc.keys_in(locale).into_iter().collect();
            let missing: Vec<&str> = universe.difference(&own).copied().collect();
            let coverage = if universe.is_empty() {
                1.0
            } else {
                own.len() as f64 / universe.len() as f64
            };
            json!({
                "locale": locale,
                "defined": own.len(),
                "coverage": coverage,
                "missing_count": missing.len(),
                "missing": missing.iter().take(MAX_LISTED_KEYS).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "selected": loc.locale(),
        "fallback": loc.fallback(),
        "total_keys": universe.len(),
        "locales": rows,
        "debug_locales": { "pseudo": PSEUDO_LOCALE, "keys": KEY_LOCALE },
    }))
}

fn pseudo(params: &Value) -> Result<Value, RpcError> {
    let text = string_param(params, "text")?
        .ok_or_else(|| RpcError::invalid_params("`text` is required for action \"pseudo\""))?;
    if text.len() > MAX_PSEUDO_BYTES {
        return Err(RpcError::invalid_params(format!(
            "`text` is longer than {MAX_PSEUDO_BYTES} bytes"
        )));
    }
    let defaults = PseudoOptions::default();
    let options = PseudoOptions {
        expansion: number_param(params, "expansion")?.map_or(defaults.expansion, |e| e as f32),
        accents: bool_param(params, "accents")?.unwrap_or(defaults.accents),
        brackets: bool_param(params, "brackets")?.unwrap_or(defaults.brackets),
    };
    let out = pseudo_localize(text, &options);
    Ok(json!({
        "text": out,
        "input_chars": text.chars().count(),
        "output_chars": out.chars().count(),
    }))
}

fn subtitle_error(e: SubtitleError) -> RpcError {
    let hint = match e {
        SubtitleError::EmptyText => "give `text`, or a localization `key`",
        SubtitleError::BadStart => "`start` is seconds from 0 on the queue's clock",
        SubtitleError::BadDuration => "`duration` is seconds greater than 0",
        SubtitleError::QueueFull(_) => "advance time so lines expire, or cancel some",
        SubtitleError::BadTime => "`to` must not be earlier than `now` from ui.subtitles",
    };
    RpcError::new(INVALID_PARAMS, e.code(), e.to_string(), hint)
}

fn event_json(event: SubtitleEvent) -> Value {
    match event {
        SubtitleEvent::Shown(id) => json!({ "event": "shown", "id": id.0 }),
        SubtitleEvent::Hidden(id, reason) => {
            json!({ "event": "hidden", "id": id.0, "reason": reason.name() })
        }
    }
}

fn snapshot(world: &World) -> Value {
    let loc = world.get_resource::<Localization>();
    let Some(queue) = world.get_resource::<SubtitleQueue>() else {
        return json!({ "now": 0.0, "max_concurrent": 0, "count": 0, "queued": [], "visible": [] });
    };
    let queued: Vec<Value> = queue
        .queued()
        .into_iter()
        .map(|(id, line, visible)| {
            let shown = loc.map_or_else(|| line.text.clone(), |l| line.resolve(l));
            json!({
                "id": id.0,
                "speaker": line.speaker,
                "text": shown,
                "key": line.key,
                "start": line.start,
                "duration": line.duration,
                "priority": line.priority,
                "visible": visible,
            })
        })
        .collect();
    let visible: Vec<u64> = queue.visible().into_iter().map(|(id, _)| id.0).collect();
    json!({
        "now": queue.now(),
        "max_concurrent": queue.max_concurrent(),
        "count": queue.len(),
        "queued": queued,
        "visible": visible,
    })
}

fn args_param(params: &Value) -> Result<Vec<(String, String)>, RpcError> {
    let bad = || RpcError::invalid_params("`args` must map names to strings or numbers");
    match params.get("args") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                Value::Number(n) => Ok((k.clone(), n.to_string())),
                _ => Err(bad()),
            })
            .collect(),
        Some(_) => Err(bad()),
    }
}

fn line_from(params: &Value, now: f64) -> Result<SubtitleLine, RpcError> {
    let speaker = string_param(params, "speaker")?.unwrap_or("");
    let text = string_param(params, "text")?.unwrap_or("");
    let duration = number_param(params, "duration")?
        .ok_or_else(|| RpcError::invalid_params("`duration` is required to push a subtitle"))?;
    let start = number_param(params, "start")?.unwrap_or(now);
    let priority = match params.get("priority") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_i64()
            .map(|p| i32::try_from(p).unwrap_or(if p < 0 { i32::MIN } else { i32::MAX }))
            .ok_or_else(|| RpcError::invalid_params("`priority` must be an integer"))?,
    };
    let mut line = SubtitleLine::new(speaker, text, start, duration)
        .with_priority(priority)
        .with_args(args_param(params)?);
    line.key = string_param(params, "key")?.map(str::to_owned);
    Ok(line)
}

fn with_queue<R>(world: &mut World, f: impl FnOnce(&mut SubtitleQueue) -> R) -> Result<R, RpcError> {
    let mut queue = world
        .get_resource_mut::<SubtitleQueue>()
        .ok_or_else(|| RpcError::internal("the SubtitleQueue resource is missing"))?;
    Ok(f(&mut queue))
}

/// `ui.subtitles { action?, speaker?, text?, key?, args?, start?, duration?, priority?, to?, by?,
/// id?, max_concurrent? }`.
pub(crate) fn subtitles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    world.init_resource::<SubtitleQueue>();
    match action_param(params, "list")? {
        "list" => {
            let mut out = snapshot(world);
            out["applied"] = json!(false);
            Ok(out)
        }
        "push" => {
            let now = with_queue(world, |q| q.now())?;
            let line = line_from(params, now)?;
            let id = with_queue(world, |q| q.push(line))?.map_err(subtitle_error)?;
            let mut out = snapshot(world);
            out["id"] = json!(id.0);
            Ok(out)
        }
        "advance" => {
            let now = with_queue(world, |q| q.now())?;
            let to = match (number_param(params, "to")?, number_param(params, "by")?) {
                (Some(to), None) => to,
                (None, Some(by)) => now + by,
                _ => {
                    return Err(RpcError::invalid_params(
                        "give exactly one of `to` (absolute seconds) or `by` (seconds from now)",
                    ));
                }
            };
            let events = with_queue(world, |q| q.advance_to(to))?.map_err(subtitle_error)?;
            let mut out = snapshot(world);
            out["events"] = Value::Array(events.into_iter().map(event_json).collect());
            Ok(out)
        }
        "cancel" => {
            let id = params
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| RpcError::invalid_params("`id` (an integer from ui.subtitles) is required"))?;
            if with_queue(world, |q| q.cancel(SubtitleId(id)))?.is_none() {
                return Err(RpcError::new(
                    NOT_FOUND,
                    "unknown_subtitle",
                    "no queued subtitle has that id",
                    "take ids from ui.subtitles `queued`; expired lines are gone",
                ));
            }
            Ok(snapshot(world))
        }
        "clear" => {
            let removed = with_queue(world, SubtitleQueue::clear)?;
            let mut out = snapshot(world);
            out["removed"] = json!(removed);
            Ok(out)
        }
        "limit" => {
            let n = params
                .get("max_concurrent")
                .and_then(Value::as_u64)
                .ok_or_else(|| RpcError::invalid_params("`max_concurrent` (an integer) is required"))?;
            let n = usize::try_from(n.min(MAX_CONCURRENT_LIMIT)).unwrap_or(1);
            with_queue(world, |q| q.set_max_concurrent(n))?;
            Ok(snapshot(world))
        }
        _ => Err(RpcError::invalid_params(
            "`action` must be \"list\", \"push\", \"advance\", \"cancel\", \"clear\" or \"limit\"",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: RpcError) -> String {
        e.data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn world_with_locales() -> World {
        let mut world = World::new();
        let loc = Localization::new(
            vec![
                ("en".to_owned(), "a = A\nb = B\nc = C\n".to_owned()),
                ("fr".to_owned(), "a = Á\n".to_owned()),
            ],
            Some("en".to_owned()),
        )
        .expect("localization");
        world.insert_resource(loc);
        world
    }

    #[test]
    fn report_lists_coverage_and_missing_keys() {
        let world = world_with_locales();
        let out = localization(&world, &json!({})).expect("report");
        assert_eq!(out["total_keys"], 3);
        assert_eq!(out["fallback"], "en");
        let fr = out["locales"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["locale"] == "fr"))
            .expect("fr row");
        assert_eq!(fr["defined"], 1);
        assert_eq!(fr["missing_count"], 2);
        assert_eq!(fr["missing"], json!(["b", "c"]));
        assert!((fr["coverage"].as_f64().unwrap_or(0.0) - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(out["debug_locales"]["pseudo"], PSEUDO_LOCALE);
    }

    #[test]
    fn report_without_localization_is_a_coded_error() {
        let world = World::new();
        let err = localization(&world, &json!({ "action": "report" })).expect_err("none");
        assert_eq!(code(err), "no_localization");
    }

    #[test]
    fn pseudo_previews_a_string() {
        let world = World::new();
        let out = localization(
            &world,
            &json!({ "action": "pseudo", "text": "Hello { $n }", "expansion": 0.0, "brackets": false }),
        )
        .expect("pseudo");
        assert_eq!(out["text"], "Ĥéļļó { $n }");
        assert_eq!(out["input_chars"], 12);
        let out = localization(&world, &json!({ "action": "pseudo", "text": "ab" })).expect("defaults");
        assert_eq!(out["text"], "«áƀ~»");
    }

    #[test]
    fn localization_rejects_bad_params_without_panicking() {
        let world = World::new();
        for params in [
            json!({ "action": 3 }),
            json!({ "action": "spin" }),
            json!({ "action": "pseudo" }),
            json!({ "action": "pseudo", "text": 5 }),
            json!({ "action": "pseudo", "text": "x", "expansion": "big" }),
            json!({ "action": "pseudo", "text": "x", "accents": 1 }),
            json!({ "action": "pseudo", "text": "x".repeat(MAX_PSEUDO_BYTES + 1) }),
        ] {
            assert!(localization(&world, &params).is_err(), "{params}");
        }
        // Extreme numbers are clamped, not errors.
        let out = localization(
            &world,
            &json!({ "action": "pseudo", "text": "ab", "expansion": 1e308 }),
        )
        .expect("clamped");
        assert!(out["output_chars"].as_u64().unwrap_or(0) < 20);
    }

    #[test]
    fn subtitles_push_advance_cancel_clear() {
        let mut world = World::new();
        let out = subtitles(&mut world, &json!({})).expect("list");
        assert_eq!(out["applied"], false);
        assert_eq!(out["count"], 0);
        let out = subtitles(
            &mut world,
            &json!({ "action": "push", "speaker": "Ana", "text": "Hi", "duration": 2.0 }),
        )
        .expect("push");
        let first = out["id"].as_u64().expect("id");
        subtitles(
            &mut world,
            &json!({ "action": "push", "speaker": "Bo", "text": "Alarm", "duration": 1.0, "priority": 9 }),
        )
        .expect("push");
        let out = subtitles(&mut world, &json!({ "action": "advance", "to": 0.0 })).expect("advance");
        assert_eq!(out["events"].as_array().map(Vec::len), Some(2));
        assert_eq!(out["visible"].as_array().map(Vec::len), Some(2));
        let out = subtitles(&mut world, &json!({ "action": "advance", "by": 1.5 })).expect("by");
        assert_eq!(out["now"], 1.5);
        assert_eq!(out["events"][0]["reason"], "expired");
        let out = subtitles(&mut world, &json!({ "action": "cancel", "id": first })).expect("cancel");
        assert_eq!(out["count"], 0);
        let err = subtitles(&mut world, &json!({ "action": "cancel", "id": first })).expect_err("gone");
        assert_eq!(code(err), "unknown_subtitle");
        subtitles(
            &mut world,
            &json!({ "action": "push", "text": "x", "duration": 5.0 }),
        )
        .expect("push");
        let out = subtitles(&mut world, &json!({ "action": "clear" })).expect("clear");
        assert_eq!(out["removed"], 1);
    }

    #[test]
    fn subtitles_resolve_keys_and_report_the_limit() {
        let mut world = world_with_locales();
        let out = subtitles(
            &mut world,
            &json!({ "action": "push", "key": "a", "text": "fallback", "duration": 1.0, "args": { "n": 3, "w": "x" } }),
        )
        .expect("push");
        assert_eq!(out["queued"][0]["text"], "A");
        let out = subtitles(&mut world, &json!({ "action": "limit", "max_concurrent": 2 })).expect("limit");
        assert_eq!(out["max_concurrent"], 2);
    }

    #[test]
    fn subtitle_errors_carry_their_codes() {
        let mut world = World::new();
        let cases = [
            (json!({ "action": "push", "text": "x" }), None),
            (
                json!({ "action": "push", "text": "", "duration": 1.0 }),
                Some("subtitle_empty_text"),
            ),
            (
                json!({ "action": "push", "text": "x", "duration": 0.0 }),
                Some("subtitle_bad_duration"),
            ),
            (
                json!({ "action": "push", "text": "x", "duration": 1.0, "start": -3.0 }),
                Some("subtitle_bad_start"),
            ),
        ];
        for (params, want) in cases {
            let err = subtitles(&mut world, &params).expect_err("refused");
            if let Some(want) = want {
                assert_eq!(code(err), want, "{params}");
            }
        }
        subtitles(&mut world, &json!({ "action": "advance", "to": 5.0 })).expect("advance");
        let err = subtitles(&mut world, &json!({ "action": "advance", "to": 1.0 })).expect_err("back");
        assert_eq!(code(err), "subtitle_bad_time");
        for params in [
            json!({ "action": "advance" }),
            json!({ "action": "advance", "to": 1.0, "by": 1.0 }),
            json!({ "action": "cancel" }),
            json!({ "action": "limit" }),
            json!({ "action": "push", "text": "x", "duration": 1.0, "args": [1] }),
            json!({ "action": "push", "text": "x", "duration": 1.0, "priority": 1.5 }),
            json!({ "action": 3 }),
            json!({ "action": "spin" }),
        ] {
            assert!(subtitles(&mut world, &params).is_err(), "{params}");
        }
    }
}
