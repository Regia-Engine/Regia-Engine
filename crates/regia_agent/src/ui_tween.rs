// SPDX-License-Identifier: MPL-2.0
//! `ui.tween`: start, stop and query UI property tweens, show and hide nodes, and set a node's
//! visual transform (book `agents/ui-animation.md`).
//!
//! Tweens run on the game clock (`Time`), so `play.step` and a paused game reproduce them. The
//! method only inserts and removes `UiTween` / `UiTransform` components; `regia_ui` does the
//! animating (ADR-273 lane 3, ADR-343). `wait_until` and `play.expect` can wait for a tween with
//! the `ui.tweening` atom.

use regia_ecs::prelude::*;
use regia_ui::{
    Ease, HitTestSpace, Reveal, Tween, TweenPlan, TweenProperty, TweenValue, UiColor, UiTransform, UiTween,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most tweens one `start` may combine.
const MAX_TWEENS: usize = 32;
/// Duration used when a tween names none, in seconds.
const DEFAULT_DURATION: f32 = 0.25;
/// Longest tween an agent may ask for, in seconds.
const MAX_SECONDS: f64 = 3600.0;

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

fn seconds(params: &Value, key: &str, default: f32) -> Result<f32, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => match v.as_f64() {
            Some(s) if s.is_finite() && (0.0..=MAX_SECONDS).contains(&s) => Ok(s as f32),
            _ => Err(error(
                INVALID_PARAMS,
                "bad_duration",
                format!("`{key}` must be a number of seconds between 0 and {MAX_SECONDS}"),
                "use seconds, for example 0.25",
            )),
        },
    }
}

fn ease_of(v: Option<&Value>) -> Result<Ease, RpcError> {
    let bad = || {
        error(
            INVALID_PARAMS,
            "bad_ease",
            "`ease` must be a curve name or [x1, y1, x2, y2] for a cubic bezier",
            "names: linear, quad_in, quad_out, quad_in_out, cubic_*, expo_*, back_*, bounce_*, elastic_*",
        )
    };
    match v {
        None | Some(Value::Null) => Ok(Ease::Linear),
        Some(Value::String(name)) => Ease::parse(name).ok_or_else(bad),
        Some(Value::Array(a)) if a.len() == 4 => {
            let n: Vec<f32> = a.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
            match n.as_slice() {
                [x1, y1, x2, y2] if n.iter().all(|x| x.is_finite()) => {
                    Ok(Ease::CubicBezier([*x1, *y1, *x2, *y2]))
                }
                _ => Err(bad()),
            }
        }
        Some(_) => Err(bad()),
    }
}

fn value_of(property: TweenProperty, v: &Value, key: &str) -> Result<TweenValue, RpcError> {
    let bad = |what: &str| {
        error(
            INVALID_PARAMS,
            "bad_value",
            format!("`{key}` for {} must be {what}", property.name()),
            "colours are \"#RRGGBB\", \"#RRGGBBAA\" or [r, g, b, a] linear floats; others are numbers",
        )
    };
    if property.is_color() {
        if let Some(hex) = v.as_str() {
            return UiColor::from_hex(hex)
                .map(TweenValue::Color)
                .ok_or_else(|| bad("a colour like \"#336699\""));
        }
        if let Some(arr) = v.as_array() {
            let n: Vec<f32> = arr.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
            if arr.len() == 4
                && let [r, g, b, a] = n.as_slice()
                && n.iter().all(|c| c.is_finite())
            {
                return Ok(TweenValue::Color(UiColor([*r, *g, *b, *a])));
            }
        }
        Err(bad("a colour like \"#336699\" or [r, g, b, a]"))
    } else {
        match v.as_f64() {
            Some(x) if x.is_finite() => Ok(TweenValue::Number(x as f32)),
            _ => Err(bad("a finite number")),
        }
    }
}

fn tween_of(spec: &Value) -> Result<Tween, RpcError> {
    let name = spec.get("property").and_then(Value::as_str).ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "missing_property",
            "each tween needs a `property`",
            "one of: opacity, background, border_color, text_color, inset_left, inset_top, inset_right, \
             inset_bottom, width, height, translate_x, translate_y, scale",
        )
    })?;
    let property = TweenProperty::parse(name).ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "unknown_property",
            format!("`{name}` is not a tweenable property"),
            "one of: opacity, background, border_color, text_color, inset_left, inset_top, inset_right, \
             inset_bottom, width, height, translate_x, translate_y, scale",
        )
    })?;
    let to = spec.get("to").filter(|v| !v.is_null()).ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "bad_value",
            "each tween needs a `to` value",
            "pass the end value: a number, or a colour for background, border_color and text_color",
        )
    })?;
    let mut tween = Tween::to(
        property,
        value_of(property, to, "to")?,
        seconds(spec, "duration", DEFAULT_DURATION)?,
    )
    .delay(seconds(spec, "delay", 0.0)?)
    .ease(ease_of(spec.get("ease"))?);
    if let Some(from) = spec.get("from").filter(|v| !v.is_null()) {
        tween = tween.from(value_of(property, from, "from")?);
    }
    Ok(tween)
}

fn plan_of(params: &Value) -> Result<TweenPlan, RpcError> {
    let Some(list) = params.get("tweens").filter(|v| !v.is_null()) else {
        return Ok(TweenPlan::Tween(tween_of(params)?));
    };
    let list = list.as_array().ok_or_else(|| {
        error(
            INVALID_PARAMS,
            "no_tweens",
            "`tweens` must be an array of tween objects",
            "each is { property, to, from?, duration?, delay?, ease? }",
        )
    })?;
    if list.is_empty() {
        return Err(error(
            INVALID_PARAMS,
            "no_tweens",
            "`tweens` is empty",
            "pass at least one tween object",
        ));
    }
    if list.len() > MAX_TWEENS {
        return Err(error(
            INVALID_PARAMS,
            "too_many_tweens",
            format!("at most {MAX_TWEENS} tweens per call"),
            "split the animation across several calls",
        ));
    }
    let children: Vec<TweenPlan> = list
        .iter()
        .map(|s| tween_of(s).map(TweenPlan::Tween))
        .collect::<Result<_, _>>()?;
    match crate::params::opt_str(params, "mode")? {
        None | Some("parallel") => Ok(TweenPlan::Parallel(children)),
        Some("sequence") => Ok(TweenPlan::Sequence(children)),
        Some(_) => Err(error(
            INVALID_PARAMS,
            "bad_mode",
            "`mode` must be \"parallel\" or \"sequence\"",
            "parallel runs the tweens together, sequence one after another",
        )),
    }
}

fn reveal_of(params: &Value) -> Result<Reveal, RpcError> {
    match crate::params::opt_str(params, "kind")? {
        None | Some("fade") => Ok(Reveal::Fade),
        Some("slide") => {
            let offset = match crate::params::opt_array(params, "offset")? {
                None => [0.0, 24.0],
                Some(a) => {
                    let n: Vec<f32> = a.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
                    match n.as_slice() {
                        [x, y] if a.len() == 2 && x.is_finite() && y.is_finite() => [*x, *y],
                        _ => {
                            return Err(error(
                                INVALID_PARAMS,
                                "bad_value",
                                "`offset` must be [x, y] in UI px",
                                "for example [0, 24] slides in from 24 px below",
                            ));
                        }
                    }
                }
            };
            Ok(Reveal::Slide(offset))
        }
        Some(_) => Err(error(
            INVALID_PARAMS,
            "bad_kind",
            "`kind` must be \"fade\" or \"slide\"",
            "fade changes opacity; slide also moves the node by `offset`",
        )),
    }
}

fn describe(world: &World, entity: Entity) -> Value {
    let tween = world.get::<UiTween>(entity).map(|t| {
        json!({
            "tweening": true,
            "elapsed": t.elapsed,
            "duration": t.duration(),
            "progress": t.progress(),
            "properties": t.plan.properties().iter().map(|p| p.name()).collect::<Vec<_>>(),
            "shows": t.show_on_start,
            "hides": t.hide_on_finish,
        })
    });
    let transform = world.get::<UiTransform>(entity).map(|t| {
        json!({
            "translate": t.translate,
            "scale": t.scale,
            "pivot_ratio": t.pivot_ratio,
            "hit_test": match t.hit_test {
                HitTestSpace::Transformed => "transformed",
                HitTestSpace::Layout => "layout",
            },
        })
    });
    json!({
        "entity": entity.to_bits(),
        "tweening": tween.is_some(),
        "tween": tween,
        "transform": transform,
        "opacity": world.get::<regia_ui::Opacity>(entity).map(|o| o.0),
    })
}

fn set_transform(world: &mut World, entity: Entity, params: &Value) -> Result<Value, RpcError> {
    if crate::params::opt_bool(params, "reset")? == Some(true) {
        world.entity_mut(entity).remove::<UiTransform>();
        return Ok(json!({ "entity": entity.to_bits(), "transform": Value::Null }));
    }
    let mut t = world.get::<UiTransform>(entity).copied().unwrap_or_default();
    let pair = |key: &str| -> Result<Option<[f32; 2]>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => {
                let n: Option<Vec<f32>> = v
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .map(|a| a.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect());
                match n.as_deref() {
                    Some([x, y]) if x.is_finite() && y.is_finite() => Ok(Some([*x, *y])),
                    _ => Err(error(
                        INVALID_PARAMS,
                        "bad_value",
                        format!("`{key}` must be [x, y]"),
                        "two finite numbers; translate in UI px, pivot_ratio as fractions of the node",
                    )),
                }
            }
        }
    };
    if let Some(v) = pair("translate")? {
        t.translate = v;
    }
    if let Some(v) = pair("pivot_ratio")? {
        t.pivot_ratio = v;
    }
    match params.get("scale") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_f64() {
            Some(s) if s.is_finite() && s >= 0.0 => t.scale = s as f32,
            _ => {
                return Err(error(
                    INVALID_PARAMS,
                    "bad_value",
                    "`scale` must be a number >= 0",
                    "1 is no scaling",
                ));
            }
        },
    }
    match crate::params::opt_str(params, "hit_test")? {
        None => {}
        Some("transformed") => t.hit_test = HitTestSpace::Transformed,
        Some("layout") => t.hit_test = HitTestSpace::Layout,
        Some(_) => {
            return Err(error(
                INVALID_PARAMS,
                "bad_value",
                "`hit_test` must be \"transformed\" or \"layout\"",
                "transformed hits where the node is drawn; layout hits where layout put it",
            ));
        }
    }
    world.entity_mut(entity).insert(t);
    Ok(describe(world, entity))
}

/// `ui.tween { action, entity, ... }`.
pub(crate) fn tween(world: &mut World, params: &Value) -> Result<Value, RpcError> {
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
            None => {
                let mut rows = world.query::<(Entity, &UiTween)>();
                let mut running: Vec<Entity> = rows.iter(world).map(|(e, _)| e).collect();
                running.sort();
                let out: Vec<Value> = running.into_iter().map(|e| describe(world, e)).collect();
                Ok(json!({ "count": out.len(), "tweens": out }))
            }
        },
        "start" => {
            let entity = required_entity(world, params)?;
            let plan = plan_of(params)?;
            world.entity_mut(entity).insert(UiTween::new(plan));
            Ok(describe(world, entity))
        }
        "show" | "hide" => {
            let entity = required_entity(world, params)?;
            let how = reveal_of(params)?;
            let duration = seconds(params, "duration", DEFAULT_DURATION)?;
            let ease = ease_of(params.get("ease"))?;
            let tween = if action == "show" {
                UiTween::show(how, duration, ease)
            } else {
                UiTween::hide(how, duration, ease)
            };
            world.entity_mut(entity).insert(tween);
            Ok(describe(world, entity))
        }
        "stop" => {
            let entity = required_entity(world, params)?;
            let was = world.entity_mut(entity).take::<UiTween>().is_some();
            let mut out = describe(world, entity);
            out["stopped"] = json!(was);
            Ok(out)
        }
        "set_transform" => {
            let entity = required_entity(world, params)?;
            set_transform(world, entity, params)
        }
        _ => Err(error(
            INVALID_PARAMS,
            "bad_action",
            "`action` must be \"query\", \"start\", \"show\", \"hide\", \"stop\" or \"set_transform\"",
            "see the ui-animation guide page",
        )),
    }
}

/// The `wait_until` / `play.expect` atom `{ "predicate": "ui.tweening", entity?, is? }`: holds when
/// the node (or, without `entity`, any node) has a running tween equals `is` (default `true`).
pub(crate) fn predicate(world: &mut World, atom: &Value) -> Result<(bool, String), String> {
    let want = crate::params::opt_bool(atom, "is")
        .map_err(|e| e.message)?
        .unwrap_or(true);
    let running = match entity_param(atom).map_err(|e| e.message)? {
        Some(entity) => {
            if world.get_entity(entity).is_err() {
                return Err("unknown_entity: the entity does not exist".to_owned());
            }
            world.get::<UiTween>(entity).is_some()
        }
        None => world.query::<&UiTween>().iter(world).next().is_some(),
    };
    Ok((running == want, format!("tweening {running}, wanted {want}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    fn node(world: &mut World) -> Entity {
        world.spawn(regia_ui::Opacity(0.0)).id()
    }

    #[test]
    fn start_inserts_a_tween_and_query_reports_it() {
        let mut world = World::new();
        let e = node(&mut world);
        let out = tween(
            &mut world,
            &json!({
                "action": "start", "entity": e.to_bits(), "property": "opacity",
                "from": 0.0, "to": 1.0, "duration": 0.5, "ease": "quad_out"
            }),
        )
        .expect("start");
        assert_eq!(out["tweening"], json!(true));
        assert_eq!(out["tween"]["duration"], json!(0.5));
        assert_eq!(out["tween"]["properties"], json!(["opacity"]));
        assert!(world.get::<UiTween>(e).is_some());
        let all = tween(&mut world, &json!({})).expect("query");
        assert_eq!(all["count"], json!(1));
        assert_eq!(all["tweens"][0]["entity"], json!(e.to_bits()));
        let one = tween(&mut world, &json!({ "entity": e.to_bits() })).expect("query one");
        assert_eq!(one["tweening"], json!(true));
    }

    #[test]
    fn start_accepts_many_tweens_colours_and_beziers() {
        let mut world = World::new();
        let e = node(&mut world);
        let out = tween(
            &mut world,
            &json!({
                "action": "start", "entity": e.to_bits(), "mode": "sequence",
                "tweens": [
                    { "property": "background", "to": "#336699", "duration": 1.0, "ease": [0.25, 0.1, 0.25, 1.0] },
                    { "property": "scale", "from": 1.0, "to": 1.2, "duration": 0.5, "delay": 0.25 },
                    { "property": "border_color", "to": [1.0, 0.0, 0.0, 1.0] }
                ]
            }),
        )
        .expect("start");
        assert_eq!(out["tween"]["duration"], json!(1.0 + 0.75 + 0.25));
        assert_eq!(
            out["tween"]["properties"],
            json!(["background", "scale", "border_color"])
        );
    }

    #[test]
    fn stop_removes_the_tween_and_reports_whether_one_ran() {
        let mut world = World::new();
        let e = node(&mut world);
        let out = tween(&mut world, &json!({ "action": "stop", "entity": e.to_bits() })).expect("stop");
        assert_eq!(out["stopped"], json!(false));
        tween(
            &mut world,
            &json!({ "action": "start", "entity": e.to_bits(), "property": "width", "to": 10 }),
        )
        .expect("start");
        let out = tween(&mut world, &json!({ "action": "stop", "entity": e.to_bits() })).expect("stop");
        assert_eq!(out["stopped"], json!(true));
        assert!(world.get::<UiTween>(e).is_none());
    }

    #[test]
    fn show_and_hide_build_flagged_tweens() {
        let mut world = World::new();
        let e = node(&mut world);
        let out = tween(
            &mut world,
            &json!({ "action": "hide", "entity": e.to_bits(), "kind": "slide", "offset": [0, 30], "duration": 0.4 }),
        )
        .expect("hide");
        assert_eq!(out["tween"]["hides"], json!(true));
        assert_eq!(
            out["tween"]["properties"],
            json!(["opacity", "translate_x", "translate_y"])
        );
        let out = tween(&mut world, &json!({ "action": "show", "entity": e.to_bits() })).expect("show");
        assert_eq!(out["tween"]["shows"], json!(true));
        assert_eq!(out["tween"]["properties"], json!(["opacity"]));
    }

    #[test]
    fn set_transform_edits_and_resets() {
        let mut world = World::new();
        let e = node(&mut world);
        let out = tween(
            &mut world,
            &json!({
                "action": "set_transform", "entity": e.to_bits(),
                "translate": [10, -5], "scale": 1.5, "hit_test": "layout"
            }),
        )
        .expect("set");
        assert_eq!(out["transform"]["translate"], json!([10.0, -5.0]));
        assert_eq!(out["transform"]["scale"], json!(1.5));
        assert_eq!(out["transform"]["pivot_ratio"], json!([0.5, 0.5]));
        assert_eq!(out["transform"]["hit_test"], json!("layout"));
        let out = tween(
            &mut world,
            &json!({ "action": "set_transform", "entity": e.to_bits(), "pivot_ratio": [0, 0] }),
        )
        .expect("pivot");
        assert_eq!(out["transform"]["scale"], json!(1.5), "unnamed fields are kept");
        assert_eq!(out["transform"]["pivot_ratio"], json!([0.0, 0.0]));
        tween(
            &mut world,
            &json!({ "action": "set_transform", "entity": e.to_bits(), "reset": true }),
        )
        .expect("reset");
        assert!(world.get::<UiTransform>(e).is_none());
    }

    #[test]
    fn errors_carry_lowercase_codes() {
        let mut world = World::new();
        let e = node(&mut world);
        let bits = e.to_bits();
        let start = |world: &mut World, extra: Value| {
            let mut p = json!({ "action": "start", "entity": bits });
            for (k, v) in extra.as_object().expect("object") {
                p[k] = v.clone();
            }
            tween(world, &p)
        };
        assert_eq!(
            code(tween(&mut world, &json!({ "action": "start" })).expect_err("rejected")),
            "missing_entity"
        );
        assert_eq!(
            code(tween(&mut world, &json!({ "action": "start", "entity": "x" })).expect_err("rejected")),
            "bad_entity"
        );
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        assert_eq!(
            code(
                tween(&mut world, &json!({ "action": "stop", "entity": gone.to_bits() }))
                    .expect_err("rejected")
            ),
            "unknown_entity"
        );
        assert_eq!(
            code(start(&mut world, json!({})).expect_err("rejected")),
            "missing_property"
        );
        assert_eq!(
            code(start(&mut world, json!({ "property": "colour", "to": 1 })).expect_err("rejected")),
            "unknown_property"
        );
        assert_eq!(
            code(start(&mut world, json!({ "property": "opacity" })).expect_err("rejected")),
            "bad_value"
        );
        assert_eq!(
            code(start(&mut world, json!({ "property": "opacity", "to": "red" })).expect_err("rejected")),
            "bad_value"
        );
        assert_eq!(
            code(start(&mut world, json!({ "property": "background", "to": 1 })).expect_err("rejected")),
            "bad_value"
        );
        assert_eq!(
            code(
                start(
                    &mut world,
                    json!({ "property": "opacity", "to": 1, "duration": -1 })
                )
                .expect_err("rejected")
            ),
            "bad_duration"
        );
        assert_eq!(
            code(
                start(
                    &mut world,
                    json!({ "property": "opacity", "to": 1, "ease": "wobble" })
                )
                .expect_err("rejected")
            ),
            "bad_ease"
        );
        assert_eq!(
            code(start(&mut world, json!({ "tweens": [] })).expect_err("rejected")),
            "no_tweens"
        );
        let many: Vec<Value> = (0..40)
            .map(|_| json!({ "property": "opacity", "to": 1 }))
            .collect();
        assert_eq!(
            code(start(&mut world, json!({ "tweens": many })).expect_err("rejected")),
            "too_many_tweens"
        );
        assert_eq!(
            code(
                start(
                    &mut world,
                    json!({ "tweens": [{ "property": "opacity", "to": 1 }], "mode": "loop" })
                )
                .expect_err("rejected")
            ),
            "bad_mode"
        );
        assert_eq!(
            code(
                tween(
                    &mut world,
                    &json!({ "action": "hide", "entity": bits, "kind": "spin" })
                )
                .expect_err("rejected")
            ),
            "bad_kind"
        );
        assert_eq!(
            code(tween(&mut world, &json!({ "action": "dance" })).expect_err("rejected")),
            "bad_action"
        );
        assert_eq!(
            code(
                tween(
                    &mut world,
                    &json!({ "action": "set_transform", "entity": bits, "scale": -1 })
                )
                .expect_err("rejected")
            ),
            "bad_value"
        );
        assert!(
            world.get::<UiTween>(e).is_none(),
            "a rejected call changes nothing"
        );
    }

    #[test]
    fn the_tweening_predicate_follows_the_component() {
        let mut world = World::new();
        let e = node(&mut world);
        let atom = json!({ "entity": e.to_bits(), "is": false });
        assert!(predicate(&mut world, &atom).expect("evaluates").0);
        tween(
            &mut world,
            &json!({ "action": "start", "entity": e.to_bits(), "property": "opacity", "to": 1 }),
        )
        .expect("start");
        assert!(!predicate(&mut world, &atom).expect("evaluates").0);
        assert!(
            predicate(&mut world, &json!({})).expect("evaluates").0,
            "any tween, default is: true"
        );
        assert!(
            predicate(&mut world, &json!({ "entity": e.to_bits() }))
                .expect("evaluates")
                .0
        );
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        assert!(predicate(&mut world, &json!({ "entity": gone.to_bits() })).is_err());
    }
}
