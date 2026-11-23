// SPDX-License-Identifier: MPL-2.0
//! `ui.motion`: motion presets (enter, exit, hover, press) on UI nodes, the effective reduced-motion
//! state and the theme's motion tokens (book `agents/ui-motion.md`, ADR-522).
//!
//! The method only edits `UiMotion` / `UiMotionState` components; `regia_ui::motion` starts the
//! tweens and the tween system plays them on the game clock, so `play.step` reproduces them. Wait
//! for a preset with the `ui.tweening` atom (`book/agents/ui-animation.md`). Reduced motion itself
//! is the player's setting: `ui.accessibility { reduced_motion }`.

use regia_ecs::prelude::*;
use regia_ui::motion::{MotionContext, reduced_motion};
use regia_ui::{
    ActiveTheme, Ease, ExitAction, MotionPose, MotionSpeed, MotionTokens, RevealPreset, ThemeResolver,
    TweenValue, UiAccessibility, UiMotion, UiMotionState, UiTween,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most nodes one `query` without `entity` lists.
const MAX_LISTED: usize = 64;
/// Offset a `"slide"` reveal named without one uses, in UI px.
const DEFAULT_SLIDE: [f32; 2] = [0.0, 24.0];
/// Scale a `"zoom"` reveal named without one uses.
const DEFAULT_ZOOM: f32 = 0.96;

fn error(class: i64, code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(class, code, message, hint)
}

fn entity_param(params: &Value) -> Result<Option<Entity>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| {
                error(
                    INVALID_PARAMS,
                    "bad_entity",
                    "`entity` must be entity bits from ui.query",
                    "take entity ids from a ui.query result",
                )
            }),
    }
}

fn required_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = entity_param(params)?.ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "missing_entity",
            "`entity` is required",
            "pass the entity bits of the node, from ui.query",
        )
    })?;
    if world.get_entity(entity).is_err() {
        return Err(error(
            NOT_FOUND,
            "unknown_entity",
            "the entity does not exist",
            "take entity ids from a ui.query result",
        ));
    }
    Ok(entity)
}

fn numbers(v: &Value) -> Option<Vec<f32>> {
    let a = v.as_array()?;
    let n: Vec<f32> = a.iter().filter_map(Value::as_f64).map(|x| x as f32).collect();
    (n.len() == a.len() && n.iter().all(|x| x.is_finite())).then_some(n)
}

/// `"fade"`, `"slide"`, `"zoom"`, `{ "slide": [x, y] }` or `{ "zoom": scale }`.
fn reveal_of(v: &Value, key: &str) -> Result<RevealPreset, RpcError> {
    let bad = || {
        error(
            INVALID_PARAMS,
            "bad_reveal",
            format!(
                "`{key}` must be \"fade\", \"slide\", \"zoom\", {{\"slide\": [x, y]}} or {{\"zoom\": scale}}"
            ),
            "slide offsets are UI px from where the node rests; zoom is the scale at the hidden end",
        )
    };
    match v {
        Value::String(name) => match name.as_str() {
            "fade" => Ok(RevealPreset::Fade),
            "slide" => Ok(RevealPreset::Slide(DEFAULT_SLIDE)),
            "zoom" => Ok(RevealPreset::Zoom(DEFAULT_ZOOM)),
            _ => Err(bad()),
        },
        Value::Object(map) if map.len() == 1 => {
            if let Some(offset) = map.get("slide") {
                match numbers(offset).as_deref() {
                    Some([x, y]) => Ok(RevealPreset::Slide([*x, *y])),
                    _ => Err(bad()),
                }
            } else if let Some(scale) = map.get("zoom") {
                match scale.as_f64() {
                    Some(s) if s.is_finite() && s >= 0.0 => Ok(RevealPreset::Zoom(s as f32)),
                    _ => Err(bad()),
                }
            } else {
                Err(bad())
            }
        }
        _ => Err(bad()),
    }
}

/// A pose name or `{ "translate": [x, y]?, "scale": s? }`.
fn pose_of(v: &Value, key: &str) -> Result<MotionPose, RpcError> {
    let bad = || {
        error(
            INVALID_PARAMS,
            "bad_pose",
            format!("`{key}` must be a pose name or {{\"translate\": [x, y], \"scale\": s}}"),
            "names: rest, lift, grow, sink, shrink",
        )
    };
    match v {
        Value::String(name) => MotionPose::parse(name).ok_or_else(bad),
        Value::Object(map) => {
            if map.keys().any(|k| k != "translate" && k != "scale") {
                return Err(bad());
            }
            let mut pose = MotionPose::REST;
            if let Some(t) = map.get("translate") {
                match numbers(t).as_deref() {
                    Some([x, y]) => pose.translate = [*x, *y],
                    _ => return Err(bad()),
                }
            }
            if let Some(s) = map.get("scale") {
                match s.as_f64() {
                    Some(s) if s.is_finite() && s >= 0.0 => pose.scale = s as f32,
                    _ => return Err(bad()),
                }
            }
            Ok(pose)
        }
        _ => Err(bad()),
    }
}

fn ease_json(ease: Ease) -> Value {
    match ease {
        Ease::CubicBezier(points) => json!(points),
        named => json!(named.name()),
    }
}

fn reveal_json(preset: Option<RevealPreset>) -> Value {
    match preset {
        None => Value::Null,
        Some(RevealPreset::Fade) => json!("fade"),
        Some(RevealPreset::Slide(offset)) => json!({ "slide": offset }),
        Some(RevealPreset::Zoom(scale)) => json!({ "zoom": scale }),
    }
}

fn pose_json(pose: Option<MotionPose>) -> Value {
    pose.map_or(
        Value::Null,
        |p| json!({ "name": p.name(), "translate": p.translate, "scale": p.scale }),
    )
}

fn context(world: &World) -> MotionContext {
    MotionContext {
        tokens: MotionTokens::from_resolver(world.get_resource::<ThemeResolver>()),
        reduced: reduced_motion(
            world.get_resource::<ActiveTheme>(),
            world.get_resource::<UiAccessibility>(),
        ),
    }
}

fn plan_json(tween: &UiTween) -> Value {
    let number = |v: TweenValue| match v {
        TweenValue::Number(n) => json!(n),
        TweenValue::Color(c) => json!(c.0),
    };
    let parts: Vec<Value> = tween
        .plan
        .tweens()
        .into_iter()
        .map(|t| {
            json!({
                "property": t.property.name(),
                "from": t.from.map_or(Value::Null, number),
                "to": number(t.to),
                "duration": t.duration,
                "ease": ease_json(t.ease),
            })
        })
        .collect();
    json!({ "duration": tween.duration(), "parts": parts })
}

fn describe(world: &World, entity: Entity) -> Value {
    let ctx = context(world);
    let motion = world.get::<UiMotion>(entity);
    let state = world.get::<UiMotionState>(entity);
    let plans = motion.map(|m| {
        json!({
            "enter": m.enter.map(|p| plan_json(&ctx.enter_tween(p, m.speed, true))),
            "exit": m.exit.map(|p| plan_json(&ctx.exit_tween(p, m.speed, m.exit_action))),
            "hover": m.hover.map(|p| plan_json(&ctx.pose_tween(p))),
            "press": m.press.map(|p| plan_json(&ctx.pose_tween(p))),
        })
    });
    json!({
        "entity": entity.to_bits(),
        "motion": motion.map(|m| json!({
            "enter": reveal_json(m.enter),
            "exit": reveal_json(m.exit),
            "hover": pose_json(m.hover),
            "press": pose_json(m.press),
            "speed": m.speed.name(),
            "exit_action": m.exit_action.name(),
            "enter_on_add": m.enter_on_add,
        })),
        "state": state.map(|s| json!({
            "phase": s.phase.name(),
            "pose": s.pose.name(),
            "pending": s.pending().map(|t| t.name()),
            "running": s.running().map(|t| t.name()),
        })),
        "tweening": world.get::<UiTween>(entity).is_some(),
        "plans": plans,
        "reduced_motion": ctx.reduced,
    })
}

fn overview(world: &mut World) -> Value {
    let ctx = context(world);
    let theme = world
        .get_resource::<ActiveTheme>()
        .is_some_and(|t| t.reduced_motion);
    let access = world
        .get_resource::<UiAccessibility>()
        .is_some_and(|a| a.reduced_motion);
    let mut rows = world.query::<(Entity, &UiMotion)>();
    let mut nodes: Vec<Entity> = rows.iter(world).map(|(e, _)| e).collect();
    nodes.sort();
    let count = nodes.len();
    let listed: Vec<Value> = nodes
        .into_iter()
        .take(MAX_LISTED)
        .map(|e| describe(world, e))
        .collect();
    let t = ctx.tokens;
    json!({
        "reduced_motion": { "effective": ctx.reduced, "theme": theme, "accessibility": access },
        "tokens": {
            "fast": t.fast, "base": t.base, "slow": t.slow, "page": t.page,
            "ease_standard": ease_json(t.ease_standard),
            "ease_enter": ease_json(t.ease_enter),
            "ease_exit": ease_json(t.ease_exit),
        },
        "presets": UiMotion::PRESETS,
        "poses": MotionPose::NAMED.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        "count": count,
        "truncated": count > MAX_LISTED,
        "nodes": listed,
    })
}

fn set(world: &mut World, entity: Entity, params: &Value) -> Result<Value, RpcError> {
    let existing = world.get::<UiMotion>(entity).cloned();
    let had_motion = existing.is_some();
    let mut motion = match params.get("preset") {
        None | Some(Value::Null) => existing.unwrap_or_default(),
        Some(v) => v.as_str().and_then(UiMotion::preset).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_preset",
                "`preset` must name a preset set",
                format!("one of: {}", UiMotion::PRESETS.join(", ")),
            )
        })?,
    };
    if !had_motion {
        // A node the agent adds motion to is already on screen: do not replay its enter unless asked.
        motion.enter_on_add = false;
    }
    for (key, slot) in [("enter", &mut motion.enter), ("exit", &mut motion.exit)] {
        match params.get(key) {
            None => {}
            Some(Value::Null) => *slot = None,
            Some(v) => *slot = Some(reveal_of(v, key)?),
        }
    }
    for (key, slot) in [("hover", &mut motion.hover), ("press", &mut motion.press)] {
        match params.get(key) {
            None => {}
            Some(Value::Null) => *slot = None,
            Some(v) => *slot = Some(pose_of(v, key)?),
        }
    }
    if let Some(v) = params.get("speed").filter(|v| !v.is_null()) {
        motion.speed = v.as_str().and_then(MotionSpeed::parse).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_speed",
                "`speed` must be \"fast\", \"base\", \"slow\" or \"page\"",
                "the motion.* token enter and exit use",
            )
        })?;
    }
    if let Some(v) = params.get("exit_action").filter(|v| !v.is_null()) {
        motion.exit_action = v.as_str().and_then(ExitAction::parse).ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_exit_action",
                "`exit_action` must be \"hide\" or \"despawn\"",
                "hide sets Display::None at the end of the exit; despawn removes the node",
            )
        })?;
    }
    if let Some(v) = params.get("enter_on_add").filter(|v| !v.is_null()) {
        motion.enter_on_add = v.as_bool().ok_or_else(|| {
            error(
                INVALID_PARAMS,
                "bad_value",
                "`enter_on_add` must be a boolean",
                "true plays the enter when the motion is first added",
            )
        })?;
    }
    world.entity_mut(entity).insert(motion);
    Ok(describe(world, entity))
}

fn play(world: &mut World, entity: Entity, params: &Value) -> Result<Value, RpcError> {
    let enter = match crate::params::opt_str(params, "trigger")? {
        Some("enter") => true,
        Some("exit") => false,
        _ => {
            return Err(error(
                INVALID_PARAMS,
                "bad_trigger",
                "`trigger` must be \"enter\" or \"exit\"",
                "hover and press follow the pointer; set them with action set",
            ));
        }
    };
    if world.get::<UiMotion>(entity).is_none() {
        return Err(error(
            INVALID_PARAMS,
            "no_motion",
            "the node has no motion presets",
            "add them first with { action: \"set\", entity, preset }",
        ));
    }
    let mut entity_mut = world.entity_mut(entity);
    if entity_mut.get::<UiMotionState>().is_none() {
        entity_mut.insert(UiMotionState::default());
    }
    if let Some(mut state) = entity_mut.get_mut::<UiMotionState>() {
        if enter {
            state.request_enter();
        } else {
            state.request_exit();
        }
    }
    Ok(describe(world, entity))
}

/// `ui.motion { action, entity, ... }`.
pub(crate) fn motion(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = match params.get("action") {
        None | Some(Value::Null) => "query",
        Some(v) => v
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`action` must be a string"))?,
    };
    match action {
        "query" => match entity_param(params)? {
            Some(_) => {
                let entity = required_entity(world, params)?;
                Ok(describe(world, entity))
            }
            None => Ok(overview(world)),
        },
        "set" => {
            let entity = required_entity(world, params)?;
            set(world, entity, params)
        }
        "play" => {
            let entity = required_entity(world, params)?;
            play(world, entity, params)
        }
        "clear" => {
            let entity = required_entity(world, params)?;
            let mut entity_mut = world.entity_mut(entity);
            let cleared = entity_mut.take::<UiMotion>().is_some();
            entity_mut.remove::<UiMotionState>();
            Ok(json!({ "entity": entity.to_bits(), "cleared": cleared }))
        }
        _ => Err(error(
            INVALID_PARAMS,
            "bad_action",
            "`action` must be \"query\", \"set\", \"play\" or \"clear\"",
            "see the ui-motion guide page",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    fn node(world: &mut World) -> Entity {
        world.spawn(regia_ui::Opacity(1.0)).id()
    }

    #[test]
    fn overview_reports_tokens_and_both_reduced_motion_sources() {
        let mut world = World::new();
        let out = motion(&mut world, &json!({})).expect("query");
        assert_eq!(out["reduced_motion"]["effective"], json!(false));
        assert_eq!(out["tokens"]["fast"], json!(0.08f32));
        assert_eq!(out["tokens"]["ease_enter"], json!([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(out["count"], json!(0));
        assert_eq!(out["presets"][0], json!("button"));
        world.insert_resource(UiAccessibility {
            reduced_motion: true,
            ..UiAccessibility::default()
        });
        let out = motion(&mut world, &json!({ "action": "query" })).expect("query");
        assert_eq!(
            out["reduced_motion"],
            json!({ "effective": true, "theme": false, "accessibility": true })
        );
    }

    #[test]
    fn set_with_a_preset_adds_motion_without_replaying_the_enter() {
        let mut world = World::new();
        let e = node(&mut world);
        let out = motion(
            &mut world,
            &json!({ "action": "set", "entity": e.to_bits(), "preset": "dialog" }),
        )
        .expect("set");
        assert_eq!(out["motion"]["enter"], json!({ "slide": [0.0, 12.0] }));
        assert_eq!(out["motion"]["enter_on_add"], json!(false));
        assert_eq!(out["state"]["phase"], json!("shown"));
        assert_eq!(out["plans"]["enter"]["duration"], json!(0.24f32));
        assert_eq!(out["plans"]["exit"]["parts"][0]["property"], json!("opacity"));
        assert_eq!(out["plans"]["hover"], Value::Null);
        assert!(world.get::<UiMotion>(e).is_some());
        assert!(world.get::<UiMotionState>(e).is_some(), "required state added");
        let listed = motion(&mut world, &json!({})).expect("query");
        assert_eq!(listed["count"], json!(1));
        assert_eq!(listed["nodes"][0]["entity"], json!(e.to_bits()));
    }

    #[test]
    fn set_edits_fields_and_keeps_the_rest() {
        let mut world = World::new();
        let e = node(&mut world);
        motion(
            &mut world,
            &json!({ "action": "set", "entity": e.to_bits(), "preset": "card" }),
        )
        .expect("card");
        let out = motion(
            &mut world,
            &json!({
                "action": "set", "entity": e.to_bits(),
                "enter": { "zoom": 0.9 }, "exit": "slide", "hover": "grow",
                "press": { "translate": [0, 2] }, "speed": "page",
                "exit_action": "despawn", "enter_on_add": true
            }),
        )
        .expect("edit");
        let m = &out["motion"];
        assert_eq!(m["enter"], json!({ "zoom": 0.9f32 }));
        assert_eq!(m["exit"], json!({ "slide": [0.0, 24.0] }));
        assert_eq!(m["hover"]["name"], json!("grow"));
        assert_eq!(m["press"]["translate"], json!([0.0, 2.0]));
        assert_eq!(m["press"]["name"], json!("custom"));
        assert_eq!(m["speed"], json!("page"));
        assert_eq!(m["exit_action"], json!("despawn"));
        assert_eq!(m["enter_on_add"], json!(true));
        let out = motion(
            &mut world,
            &json!({ "action": "set", "entity": e.to_bits(), "hover": null }),
        )
        .expect("clear hover");
        assert_eq!(out["motion"]["hover"], Value::Null);
        assert_eq!(out["motion"]["speed"], json!("page"), "unnamed fields are kept");
    }

    #[test]
    fn plans_follow_reduced_motion() {
        let mut world = World::new();
        world.insert_resource(ActiveTheme {
            reduced_motion: true,
            ..ActiveTheme::default()
        });
        let e = node(&mut world);
        let out = motion(
            &mut world,
            &json!({ "action": "set", "entity": e.to_bits(), "preset": "toast" }),
        )
        .expect("set");
        assert_eq!(out["reduced_motion"], json!(true));
        let parts = &out["plans"]["enter"]["parts"];
        assert_eq!(parts[0]["duration"], json!(0.08f32), "fade capped at motion.fast");
        assert_eq!(parts[1]["duration"], json!(0.0), "slide jumps");
    }

    #[test]
    fn play_queues_a_request_and_clear_removes_the_motion() {
        let mut world = World::new();
        let e = node(&mut world);
        motion(
            &mut world,
            &json!({ "action": "set", "entity": e.to_bits(), "preset": "popup" }),
        )
        .expect("set");
        let out = motion(
            &mut world,
            &json!({ "action": "play", "entity": e.to_bits(), "trigger": "exit" }),
        )
        .expect("play");
        assert_eq!(out["state"]["pending"], json!("exit"));
        let out = motion(&mut world, &json!({ "action": "clear", "entity": e.to_bits() })).expect("clear");
        assert_eq!(out["cleared"], json!(true));
        assert!(world.get::<UiMotion>(e).is_none());
        assert!(world.get::<UiMotionState>(e).is_none());
        let out = motion(&mut world, &json!({ "action": "clear", "entity": e.to_bits() })).expect("again");
        assert_eq!(out["cleared"], json!(false));
    }

    #[test]
    fn errors_carry_lowercase_codes_and_change_nothing() {
        let mut world = World::new();
        let e = node(&mut world);
        let bits = e.to_bits();
        let call = |world: &mut World, p: Value| code(motion(world, &p).expect_err("rejected"));
        assert_eq!(call(&mut world, json!({ "action": "set" })), "missing_entity");
        assert_eq!(
            call(&mut world, json!({ "action": "set", "entity": "x" })),
            "bad_entity"
        );
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        assert_eq!(
            call(
                &mut world,
                json!({ "action": "play", "entity": gone.to_bits(), "trigger": "enter" })
            ),
            "unknown_entity"
        );
        let set = |extra: Value| {
            let mut p = json!({ "action": "set", "entity": bits });
            for (k, v) in extra.as_object().expect("object") {
                p[k] = v.clone();
            }
            p
        };
        assert_eq!(call(&mut world, set(json!({ "preset": "wobble" }))), "bad_preset");
        assert_eq!(call(&mut world, set(json!({ "enter": "spin" }))), "bad_reveal");
        assert_eq!(
            call(&mut world, set(json!({ "exit": { "slide": [1] } }))),
            "bad_reveal"
        );
        assert_eq!(
            call(&mut world, set(json!({ "enter": { "zoom": -1 } }))),
            "bad_reveal"
        );
        assert_eq!(call(&mut world, set(json!({ "hover": "wiggle" }))), "bad_pose");
        assert_eq!(
            call(&mut world, set(json!({ "press": { "tilt": 3 } }))),
            "bad_pose"
        );
        assert_eq!(call(&mut world, set(json!({ "speed": "warp" }))), "bad_speed");
        assert_eq!(
            call(&mut world, set(json!({ "exit_action": "explode" }))),
            "bad_exit_action"
        );
        assert_eq!(
            call(&mut world, set(json!({ "enter_on_add": "yes" }))),
            "bad_value"
        );
        assert!(
            world.get::<UiMotion>(e).is_none(),
            "a rejected set changes nothing"
        );
        assert_eq!(
            call(
                &mut world,
                json!({ "action": "play", "entity": bits, "trigger": "enter" })
            ),
            "no_motion"
        );
        assert_eq!(
            call(
                &mut world,
                json!({ "action": "play", "entity": bits, "trigger": "hover" })
            ),
            "bad_trigger"
        );
        assert_eq!(call(&mut world, json!({ "action": "dance" })), "bad_action");
    }
}
