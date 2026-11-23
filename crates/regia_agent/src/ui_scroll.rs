// SPDX-License-Identifier: MPL-2.0
//! `ui.scroll`: read and drive scroll containers (book `agents/ui-scroll.md`, ADR-520, UI design
//! series d-12): state (offset, range, velocity, nesting chain, settled), wheel-like scrolling
//! through nested views, flings, stop, scrollbar styles and the view's scroll settings.
//!
//! The target is the node id `ui.query` returns, as `entity` (or its alias `node`). Offsets and
//! deltas are physical pixels in offset space: positive `y` moves toward the end of the content.

use regia_ecs::prelude::*;
use regia_ui::scroll::{is_settled, max_offset, scroll_by, scroll_chain_in, start_fling, stop_scroll};
use regia_ui::scroll_motion::{fling_distance, overscroll_amount};
use regia_ui::{
    ComputedNode, Overscroll, ScrollAxes, ScrollChaining, ScrollMotion, ScrollOffset, ScrollVelocity,
    ScrollView, ScrollbarStyle, Scrollbars,
};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn unknown_entity() -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "unknown_entity",
        "the entity does not exist",
        "take entity ids from a ui.query result",
    )
}

fn not_scrollable() -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "not_scrollable",
        "the entity is not a scroll view",
        "find scroll views with ui.query { kind: \"ScrollView\" }",
    )
}

fn entity_param(params: &Value) -> Result<Entity, RpcError> {
    let bad = || RpcError::invalid_params("`entity` (or `node`) must be the node id from ui.query");
    let value = params
        .get("entity")
        .or_else(|| params.get("node"))
        .filter(|v| !v.is_null())
        .ok_or_else(|| RpcError::invalid_params("`entity` (the node id from ui.query) is required"))?;
    value.as_u64().and_then(Entity::try_from_bits).ok_or_else(bad)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

fn f32_param(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| Some(n as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

/// `[x, y]`, both finite numbers.
fn pair_param(params: &Value, key: &str) -> Result<[f32; 2], RpcError> {
    let bad = || RpcError::invalid_params(format!("`{key}` must be [x, y] (two numbers)"));
    let items = params
        .get(key)
        .filter(|v| !v.is_null())
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` ([x, y]) is required")))?
        .as_array()
        .ok_or_else(bad)?;
    match items.as_slice() {
        [x, y] => {
            let x = x.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
            let y = y.as_f64().filter(|n| n.is_finite()).ok_or_else(bad)?;
            Ok([x as f32, y as f32])
        }
        _ => Err(bad()),
    }
}

/// The `scrollbars` field: the preset name, `"custom"`, or null without bars.
fn bars_name(world: &World, entity: Entity) -> Value {
    match world.get::<Scrollbars>(entity) {
        None => Value::Null,
        Some(bars) => json!(ScrollbarStyle::matching(bars).map_or("custom", ScrollbarStyle::name)),
    }
}

fn state_json(world: &World, entity: Entity) -> Result<Value, RpcError> {
    let view = world.get::<ScrollView>(entity).ok_or_else(not_scrollable)?;
    let computed = world.get::<ComputedNode>(entity).copied().unwrap_or_default();
    let offset = world.get::<ScrollOffset>(entity).copied().unwrap_or_default();
    let velocity = world.get::<ScrollVelocity>(entity).copied().unwrap_or_default();
    let motion = world.get::<ScrollMotion>(entity).cloned().unwrap_or_default();
    let max = max_offset(&computed);
    let chain: Vec<u64> = scroll_chain_in(world, entity)
        .iter()
        .map(|e| e.to_bits())
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "offset": offset.0,
        "max": max,
        "viewport": [computed.rect[2], computed.rect[3]],
        "content": computed.content_size,
        "velocity": velocity.v,
        "overscrolled": [overscroll_amount(offset.0[0], max[0]), overscroll_amount(offset.0[1], max[1])],
        "axes": view.axes.name(),
        "overscroll": view.overscroll.name(),
        "chaining": view.chaining.name(),
        "inertia": view.inertia,
        "wheel_smoothing": view.wheel_smoothing,
        "dragging": motion.dragging(),
        "held": motion.held(),
        "flings": motion.flings(),
        "settled": is_settled(&computed, &offset, &velocity, &motion),
        "scrollbars": bars_name(world, entity),
        "chain": chain,
    }))
}

/// `ui.scroll { entity, action?, ... }`.
pub(crate) fn scroll(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(params)?;
    if world.get_entity(entity).is_err() {
        return Err(unknown_entity());
    }
    if world.get::<ScrollView>(entity).is_none() {
        return Err(not_scrollable());
    }
    let action = str_param(params, "action")?.unwrap_or("state");
    match action {
        "state" => state_json(world, entity),
        "wheel" => {
            let delta = pair_param(params, "delta")?;
            let moved: Vec<Value> = scroll_by(world, entity, delta)
                .into_iter()
                .map(|(e, offset)| json!({ "entity": e.to_bits(), "offset": offset }))
                .collect();
            let mut out = state_json(world, entity)?;
            out["moved"] = Value::Array(moved);
            Ok(out)
        }
        "fling" => {
            let wanted = pair_param(params, "velocity")?;
            let applied = start_fling(world, entity, wanted).ok_or_else(not_scrollable)?;
            let tau = world.get::<ScrollView>(entity).map_or(0.0, |v| v.inertia);
            let mut out = state_json(world, entity)?;
            out["distance"] = json!([fling_distance(applied[0], tau), fling_distance(applied[1], tau)]);
            Ok(out)
        }
        "stop" => {
            stop_scroll(world, entity);
            state_json(world, entity)
        }
        "style" => {
            let name = str_param(params, "style")?
                .ok_or_else(|| RpcError::invalid_params("`style` is required for the style action"))?;
            if name == "none" {
                world.entity_mut(entity).remove::<Scrollbars>();
                return state_json(world, entity);
            }
            let style = ScrollbarStyle::parse(name).ok_or_else(|| {
                RpcError::new(
                    INVALID_PARAMS,
                    "unknown_scroll_style",
                    format!("no scrollbar style named {name:?}"),
                    "use \"overlay\", \"compact\", \"classic\", \"hidden\" or \"none\"",
                )
            })?;
            if world.get::<ChildOf>(entity).is_none() {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "no_bar_parent",
                    "scrollbars are spawned beside the view, and this view has no parent node",
                    "wrap the scroll view in a plain node, then set the style",
                ));
            }
            world.entity_mut(entity).insert(Scrollbars::styled(style));
            state_json(world, entity)
        }
        "config" => {
            let axes = match str_param(params, "axes")? {
                None => None,
                Some(name) => Some(ScrollAxes::parse(name).ok_or_else(|| {
                    RpcError::invalid_params("`axes` must be \"both\", \"vertical\" or \"horizontal\"")
                })?),
            };
            let overscroll = match str_param(params, "overscroll")? {
                None => None,
                Some(name) => Some(Overscroll::parse(name).ok_or_else(|| {
                    RpcError::invalid_params("`overscroll` must be \"clamp\" or \"bounce\"")
                })?),
            };
            let chaining = match str_param(params, "chaining")? {
                None => None,
                Some(name) => Some(ScrollChaining::parse(name).ok_or_else(|| {
                    RpcError::invalid_params("`chaining` must be \"chain\" or \"contain\"")
                })?),
            };
            let inertia = f32_param(params, "inertia")?;
            let smoothing = f32_param(params, "wheel_smoothing")?;
            if inertia.is_some_and(|t| t < 0.0) || smoothing.is_some_and(|t| t < 0.0) {
                return Err(RpcError::invalid_params(
                    "`inertia` and `wheel_smoothing` are seconds, 0 or more",
                ));
            }
            if let Some(mut view) = world.get_mut::<ScrollView>(entity) {
                if let Some(axes) = axes {
                    view.axes = axes;
                }
                if let Some(overscroll) = overscroll {
                    view.overscroll = overscroll;
                }
                if let Some(chaining) = chaining {
                    view.chaining = chaining;
                }
                if let Some(inertia) = inertia {
                    view.inertia = inertia;
                }
                if let Some(smoothing) = smoothing {
                    view.wheel_smoothing = smoothing;
                }
            }
            state_json(world, entity)
        }
        _ => Err(RpcError::invalid_params(
            "`action` must be \"state\", \"wheel\", \"fling\", \"stop\", \"style\" or \"config\"",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<regia_ui::ScrollDirty>();
        world
    }

    fn computed(rect: [f32; 4], content: [f32; 2]) -> ComputedNode {
        ComputedNode {
            rect,
            clip_rect: rect,
            content_size: content,
            ..Default::default()
        }
    }

    /// An outer vertical view (range 1700) holding an inner one (range 250).
    fn nested(world: &mut World) -> (Entity, Entity) {
        let outer = world
            .spawn((
                ScrollView::vertical(),
                computed([0.0, 0.0, 400.0, 300.0], [400.0, 2000.0]),
            ))
            .id();
        let inner = world
            .spawn((
                ScrollView::vertical(),
                computed([0.0, 0.0, 300.0, 150.0], [300.0, 400.0]),
                ChildOf(outer),
            ))
            .id();
        (outer, inner)
    }

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        scroll(world, &params)
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn state_reports_range_settings_and_chain() {
        let mut world = world();
        let (outer, inner) = nested(&mut world);
        let out = call(&mut world, json!({ "entity": inner.to_bits() })).expect("state");
        assert_eq!(out["max"], json!([0.0, 250.0]));
        assert_eq!(out["axes"], json!("vertical"));
        assert_eq!(out["overscroll"], json!("clamp"));
        assert_eq!(out["chaining"], json!("chain"));
        assert_eq!(out["settled"], json!(true));
        assert_eq!(out["scrollbars"], Value::Null);
        assert_eq!(out["chain"], json!([inner.to_bits(), outer.to_bits()]));
    }

    #[test]
    fn wheel_fills_the_inner_view_and_chains() {
        let mut world = world();
        let (outer, inner) = nested(&mut world);
        let out = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "wheel", "delta": [0, 300] }),
        )
        .expect("wheel");
        assert_eq!(out["offset"], json!([0.0, 250.0]));
        assert_eq!(
            world.get::<ScrollOffset>(outer).expect("outer offset").0,
            [0.0, 50.0]
        );
        assert_eq!(out["moved"].as_array().map(Vec::len), Some(2));
        assert!(world.resource::<regia_ui::ScrollDirty>().0);
    }

    #[test]
    fn fling_respects_axes_and_reports_its_distance() {
        let mut world = world();
        let (_, inner) = nested(&mut world);
        let out = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "fling", "velocity": [900, 1000] }),
        )
        .expect("fling");
        assert_eq!(
            out["velocity"],
            json!([0.0, 1000.0]),
            "the view does not scroll x"
        );
        assert_eq!(out["flings"], json!(1));
        assert_eq!(out["settled"], json!(false));
        let tau = f64::from(ScrollView::default().inertia);
        let distance = out["distance"][1].as_f64().expect("distance");
        assert!((distance - 1000.0 * tau).abs() < 1e-3);
        let stopped = call(&mut world, json!({ "entity": inner.to_bits(), "action": "stop" })).expect("stop");
        assert_eq!(stopped["velocity"], json!([0.0, 0.0]));
        assert_eq!(stopped["settled"], json!(true));
    }

    #[test]
    fn stop_brings_an_overscrolled_view_back_to_its_edge() {
        let mut world = world();
        let (_, inner) = nested(&mut world);
        world.get_mut::<ScrollOffset>(inner).expect("inner offset").0 = [0.0, -40.0];
        let state = call(&mut world, json!({ "entity": inner.to_bits() })).expect("state");
        assert_eq!(state["overscrolled"], json!([0.0, -40.0]));
        let out = call(&mut world, json!({ "entity": inner.to_bits(), "action": "stop" })).expect("stop");
        assert_eq!(out["offset"], json!([0.0, 0.0]));
    }

    #[test]
    fn style_sets_and_removes_bars() {
        let mut world = world();
        let (outer, inner) = nested(&mut world);
        let out = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "style", "style": "overlay" }),
        )
        .expect("style");
        assert_eq!(out["scrollbars"], json!("overlay"));
        assert_eq!(world.get::<Scrollbars>(inner), Some(&Scrollbars::overlay()));
        let out = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "style", "style": "none" }),
        )
        .expect("none");
        assert_eq!(out["scrollbars"], Value::Null);
        // The outer view has no parent to hold its bars.
        let err = call(
            &mut world,
            json!({ "entity": outer.to_bits(), "action": "style", "style": "classic" }),
        )
        .expect_err("refused");
        assert_eq!(code(&err), "no_bar_parent");
        crate::contract::assert_listed("ui.scroll", "no_bar_parent");
        let err = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "style", "style": "chunky" }),
        )
        .expect_err("refused");
        assert_eq!(code(&err), "unknown_scroll_style");
        crate::contract::assert_listed("ui.scroll", "unknown_scroll_style");
    }

    #[test]
    fn config_edits_the_view() {
        let mut world = world();
        let (_, inner) = nested(&mut world);
        let out = call(
            &mut world,
            json!({
                "entity": inner.to_bits(),
                "action": "config",
                "axes": "both",
                "overscroll": "bounce",
                "chaining": "contain",
                "inertia": 0.4,
                "wheel_smoothing": 0.05,
            }),
        )
        .expect("config");
        assert_eq!(out["axes"], json!("both"));
        assert_eq!(out["overscroll"], json!("bounce"));
        assert_eq!(out["chaining"], json!("contain"));
        let view = world.get::<ScrollView>(inner).expect("scroll view");
        assert_eq!((view.inertia, view.wheel_smoothing), (0.4, 0.05));
        let err = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "config", "axes": "diagonal" }),
        )
        .expect_err("refused");
        assert_eq!(err.code, INVALID_PARAMS);
        let err = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "config", "inertia": -1 }),
        )
        .expect_err("refused");
        assert_eq!(err.code, INVALID_PARAMS);
    }

    #[test]
    fn errors_are_stable() {
        let mut world = world();
        let plain = world.spawn(computed([0.0; 4], [0.0; 2])).id();
        let err = call(&mut world, json!({ "entity": plain.to_bits() })).expect_err("refused");
        assert_eq!(code(&err), "not_scrollable");
        crate::contract::assert_listed("ui.scroll", "not_scrollable");
        world.despawn(plain);
        let err = call(&mut world, json!({ "entity": plain.to_bits() })).expect_err("refused");
        assert_eq!(code(&err), "unknown_entity");
        let (_, inner) = nested(&mut world);
        let err =
            call(&mut world, json!({ "entity": inner.to_bits(), "action": "spin" })).expect_err("refused");
        assert_eq!(err.code, INVALID_PARAMS);
        let err = call(
            &mut world,
            json!({ "entity": inner.to_bits(), "action": "wheel", "delta": [1] }),
        )
        .expect_err("refused");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(call(&mut world, json!({})).is_err());
    }
}
