// SPDX-License-Identifier: MPL-2.0
//! `ui.choice_state`, `ui.choice_pick`, `captions.anchor_state` and `captions.set_dialogue_style`
//! (`regia_ui::choice`, `regia_ui::captions::{anchor, karaoke}`, ADR-3234, book
//! `agents/dialogue-ui.md`).
//!
//! `ui.choice_state` lists the open choice widgets with their options, focus and timer;
//! `ui.choice_pick` queues a pick or a navigation input for one (applied on the next frame, like a
//! player's input); `captions.anchor_state` reports where anchored captions went;
//! `captions.set_dialogue_style` turns karaoke highlighting and anchored placement on or off.

use regia_app::caption::CaptionId;
use regia_app::prelude::*;
use regia_ui::captions::{
    AnchorMode, CaptionAnchorState, CaptionAnchors, CaptionPlacement, DialogueCaptionSettings,
};
use regia_ui::choice::{ChoiceBoard, ChoiceInput, ChoiceLayout};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn plugin_missing(name: &str) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "plugin_missing",
        format!("{name} is not installed"),
        "add regia_ui::UiPlugin (choice widgets) and regia_ui::captions::CaptionPlugin to the app",
    )
}

fn rect_json(r: regia_ui::captions::Rect) -> Value {
    json!({"x": r.x, "y": r.y, "w": r.w, "h": r.h})
}

/// `ui.choice_state { widget? }`.
pub(crate) fn choice_state(world: &World, params: &Value) -> Result<Value, RpcError> {
    let Some(board) = world.get_resource::<ChoiceBoard>() else {
        return Err(plugin_missing("the choice plugin"));
    };
    let only = crate::params::opt_u64(params, "widget")?;
    let widgets: Vec<Value> = board
        .open_ids()
        .into_iter()
        .filter(|id| only.is_none_or(|w| w == *id))
        .filter_map(|id| board.get(id).map(|w| (id, w)))
        .map(|(id, w)| {
            let layout = w.layout();
            let options: Vec<Value> = w
                .options()
                .iter()
                .zip(&layout.parts)
                .map(|(o, part)| {
                    json!({
                        "index": part.index,
                        "label": o.label,
                        "enabled": o.enabled(),
                        "reason": o.disabled_reason,
                        "rect": rect_json(part.rect),
                        "classes": part.classes,
                        "a11y_name": part.a11y_name,
                    })
                })
                .collect();
            json!({
                "widget": id,
                "kind": match w.kind() { ChoiceLayout::List => "list", ChoiceLayout::Wheel => "wheel" },
                "prompt": w.prompt(),
                "rtl": w.rtl(),
                "focus": w.focus(),
                "hover": w.hover(),
                "timer": w.timer().map(|t| json!({"remaining": t.remaining, "total": t.total, "default": t.default_index})),
                "a11y_name": layout.a11y_name,
                "options": options,
            })
        })
        .collect();
    if let Some(id) = only
        && widgets.is_empty()
    {
        return Err(RpcError::new(
            NOT_FOUND,
            "widget_not_found",
            format!("no open choice widget {id}"),
            "list the open widgets with ui.choice_state",
        ));
    }
    Ok(json!({ "widgets": widgets }))
}

fn input_named(name: &str) -> Option<ChoiceInput> {
    Some(match name {
        "up" => ChoiceInput::Up,
        "down" => ChoiceInput::Down,
        "left" => ChoiceInput::Left,
        "right" => ChoiceInput::Right,
        "next" => ChoiceInput::Next,
        "prev" => ChoiceInput::Prev,
        "submit" => ChoiceInput::Submit,
        "cancel" => ChoiceInput::Cancel,
        _ => return None,
    })
}

/// `ui.choice_pick { widget?, index? | input? }`.
pub(crate) fn choice_pick(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(mut board) = world.get_resource_mut::<ChoiceBoard>() else {
        return Err(plugin_missing("the choice plugin"));
    };
    let ids = board.open_ids();
    let id = match params.get("widget").filter(|v| !v.is_null()) {
        Some(v) => v
            .as_u64()
            .ok_or_else(|| RpcError::invalid_params("`widget` must be a non-negative integer"))?,
        None => *ids.last().ok_or_else(|| {
            RpcError::new(
                NOT_FOUND,
                "widget_not_found",
                "no choice widget is open",
                "wait for a conversation choice or open one with ChoiceBoard::show",
            )
        })?,
    };
    let Some(widget) = board.get(id) else {
        return Err(RpcError::new(
            NOT_FOUND,
            "widget_not_found",
            format!("no open choice widget {id}"),
            "list the open widgets with ui.choice_state",
        ));
    };
    let input = match (
        params.get("index").filter(|v| !v.is_null()),
        crate::params::opt_str(params, "input")?,
    ) {
        (Some(i), _) => {
            let i = i
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`index` must be a non-negative integer"))?
                as usize;
            let option = widget.options().get(i).ok_or_else(|| {
                RpcError::new(
                    INVALID_PARAMS,
                    "option_out_of_range",
                    format!("the widget has {} options", widget.options().len()),
                    "index from 0 to the option count minus 1",
                )
            })?;
            if let Some(reason) = &option.disabled_reason {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "option_disabled",
                    format!("option {i} is disabled: {reason}"),
                    "pick an enabled option (ui.choice_state shows enabled and reason)",
                ));
            }
            ChoiceInput::Digit(i + 1)
        }
        (None, Some(name)) => input_named(name).ok_or_else(|| {
            RpcError::invalid_params("`input` is one of up, down, left, right, next, prev, submit, cancel")
        })?,
        (None, None) => return Err(RpcError::invalid_params("give `index` or `input`")),
    };
    board.queue_input(id, input);
    Ok(json!({ "widget": id, "queued": true }))
}

/// `captions.anchor_state`.
pub(crate) fn anchor_state(world: &World, _params: &Value) -> Result<Value, RpcError> {
    let Some(settings) = world.get_resource::<DialogueCaptionSettings>() else {
        return Err(plugin_missing("the caption plugin"));
    };
    let state = world
        .get_resource::<CaptionAnchorState>()
        .cloned()
        .unwrap_or_default();
    let anchors = world.get_resource::<CaptionAnchors>().map_or(0, |a| a.0.len());
    let placements: Vec<Value> = state
        .placements
        .iter()
        .map(|p| {
            let (mode, arrow) = match p.mode {
                AnchorMode::OnScreen => ("on_screen", Value::Null),
                AnchorMode::BottomBand => ("bottom_band", Value::Null),
                AnchorMode::EdgeArrow(a) => (
                    "edge_arrow",
                    json!({"x": a.pos[0], "y": a.pos[1], "angle": a.angle}),
                ),
            };
            json!({ "caption": p.id.0, "mode": mode, "arrow": arrow, "rect": rect_json(p.rect) })
        })
        .collect();
    Ok(json!({
        "placement": settings.placement.as_str(),
        "karaoke": settings.karaoke,
        "active": state.active,
        "anchors": anchors,
        "placements": placements,
    }))
}

/// `captions.set_dialogue_style { karaoke?, placement?, anchor? }`. `anchor` is `{ caption, world:
/// [x, y, z] }` and pins a caption to a world point (a test hook; games use `CaptionAnchors`).
pub(crate) fn set_dialogue_style(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<DialogueCaptionSettings>().is_none() {
        return Err(plugin_missing("the caption plugin"));
    }
    let karaoke = match params.get("karaoke").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`karaoke` must be true or false"))?,
        ),
    };
    let placement = match params.get("placement").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(v.as_str().and_then(CaptionPlacement::parse).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "invalid_placement",
                "placement is `bottom_band` or `anchored`",
                "use bottom_band or anchored",
            )
        })?),
    };
    if let Some(a) = params.get("anchor").filter(|v| !v.is_null()) {
        let caption = a
            .get("caption")
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::invalid_params("`anchor.caption` must be a caption id"))?;
        let w = crate::params::opt_array(a, "world")?
            .filter(|w| w.len() == 3)
            .and_then(|w| Some([w[0].as_f64()?, w[1].as_f64()?, w[2].as_f64()?]))
            .ok_or_else(|| RpcError::invalid_params("`anchor.world` must be [x, y, z] numbers"))?;
        let p = regia_core::glam::Vec3::new(w[0] as f32, w[1] as f32, w[2] as f32);
        world
            .get_resource_or_insert_with(CaptionAnchors::default)
            .0
            .insert(CaptionId(caption), regia_ui::captions::AnchorTarget::World(p));
    }
    let mut s = world.resource_mut::<DialogueCaptionSettings>();
    if let Some(k) = karaoke {
        s.karaoke = k;
    }
    if let Some(p) = placement {
        s.placement = p;
    }
    Ok(json!({ "karaoke": s.karaoke, "placement": s.placement.as_str() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::choice::{ChoiceOption, ChoicePlugin, ChoiceWidget};

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(ChoicePlugin);
        app.init_resource::<DialogueCaptionSettings>();
        app.init_resource::<CaptionAnchorState>();
        app
    }

    fn code(e: RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn choice_state_and_pick_round_trip() {
        let mut app = app();
        let id = app
            .world_mut()
            .resource_mut::<ChoiceBoard>()
            .show(ChoiceWidget::open(
                ChoiceLayout::List,
                Some("Go?".into()),
                vec![
                    ChoiceOption::new("yes"),
                    ChoiceOption::disabled("no", "locked"),
                    ChoiceOption::new("maybe"),
                ],
                false,
            ));
        let state = choice_state(app.world(), &json!({})).expect("state");
        let w = &state["widgets"][0];
        assert_eq!(w["widget"], id);
        assert_eq!(w["kind"], "list");
        assert_eq!(w["focus"], 0);
        assert_eq!(w["options"][1]["enabled"], false);
        assert_eq!(w["options"][1]["reason"], "locked");
        assert_eq!(
            code(choice_pick(app.world_mut(), &json!({"index": 1})).expect_err("disabled")),
            "option_disabled"
        );
        assert_eq!(
            code(choice_pick(app.world_mut(), &json!({"index": 9})).expect_err("range")),
            "option_out_of_range"
        );
        assert_eq!(
            code(choice_pick(app.world_mut(), &json!({"widget": 99, "index": 0})).expect_err("nf")),
            "widget_not_found"
        );
        choice_pick(app.world_mut(), &json!({"input": "down"})).expect("nav");
        choice_pick(app.world_mut(), &json!({"index": 2})).expect("pick");
        app.update();
        assert!(
            choice_state(app.world(), &json!({})).expect("state")["widgets"]
                .as_array()
                .expect("array")
                .is_empty()
        );
    }

    #[test]
    fn dialogue_style_sets_and_reports() {
        let mut app = app();
        let out = set_dialogue_style(
            app.world_mut(),
            &json!({"karaoke": true, "placement": "anchored", "anchor": {"caption": 3, "world": [1.0, 2.0, 3.0]}}),
        )
        .expect("set");
        assert_eq!(out["placement"], "anchored");
        let st = anchor_state(app.world(), &json!({})).expect("state");
        assert_eq!(st["karaoke"], true);
        assert_eq!(st["anchors"], 1);
        assert_eq!(
            code(set_dialogue_style(app.world_mut(), &json!({"placement": "x"})).expect_err("bad")),
            "invalid_placement"
        );
    }
}
