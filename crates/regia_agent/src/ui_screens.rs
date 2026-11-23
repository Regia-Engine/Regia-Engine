// SPDX-License-Identifier: MPL-2.0
//! `ui.screens`: list, push, pop and show the UI screen stack (`regia_ui::screens`, ADR-1563 l,
//! book `agents/ui-screens.md`).
//!
//! Only the top screen is shown and holds the input layer. Push, pop and show apply at once, so
//! the answer already shows the new stack and focus.

use regia_app::prelude::*;
use regia_ui::ScreenStack;
use regia_ui::focus::FocusState;
use regia_ui::screens::{ScreenError, pop_screen, push_screen, screen_list, show_screen};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn error(e: ScreenError) -> RpcError {
    match e {
        ScreenError::UnknownEntity => RpcError::new(
            NOT_FOUND,
            e.code(),
            "the entity does not exist",
            "take entity ids from ui.screens or a ui.query result",
        ),
        ScreenError::NotAScreen => RpcError::new(
            INVALID_PARAMS,
            e.code(),
            "the entity has no UiScreen",
            "put regia_ui::UiScreen on the screen's root node",
        ),
        ScreenError::AlreadyOnStack => RpcError::new(
            INVALID_PARAMS,
            e.code(),
            "the screen is already on the stack",
            "use ui.screens { action: \"show\", entity } to bring it back to the top",
        ),
        ScreenError::EmptyStack => RpcError::new(
            NOT_FOUND,
            e.code(),
            "there is no screen to pop",
            "push one with ui.screens { action: \"push\", entity }",
        ),
        ScreenError::NotOnTop => RpcError::new(
            INVALID_PARAMS,
            e.code(),
            "the screen is on the stack but not on top",
            "pop without `entity`, or use action \"show\" to jump back to it",
        ),
    }
}

fn entity_param(params: &Value) -> Result<Option<Entity>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params("`entity` must be entity bits from ui.screens or ui.query")
            }),
    }
}

fn required_entity(params: &Value, action: &str) -> Result<Entity, RpcError> {
    entity_param(params)?
        .ok_or_else(|| RpcError::invalid_params(format!("`entity` is required to {action} a screen")))
}

fn snapshot(world: &World) -> Value {
    let rows: Vec<Value> = screen_list(world)
        .into_iter()
        .map(|s| json!({ "entity": s.root.to_bits(), "id": s.id, "visible": s.visible,
            "back": world.get::<regia_ui::screens::ScreenBack>(s.root).map(|back| if *back == regia_ui::screens::ScreenBack::Pop { "pop" } else { "ignore" }) }))
        .collect();
    let stack = world.get_resource::<ScreenStack>();
    let focused = world
        .get_resource::<FocusState>()
        .and_then(|f| f.focused)
        .map(Entity::to_bits);
    json!({
        "screens": rows,
        "back": if stack.is_some_and(ScreenStack::back_pop) { "pop" } else { "ignore" },
        "depth": stack.map_or(0, ScreenStack::depth),
        "top": stack.and_then(ScreenStack::top).map(Entity::to_bits),
        "top_id": stack.and_then(ScreenStack::top_id),
        "focused": focused,
    })
}

/// `ui.screens { action?, entity?, back? }`.
pub(crate) fn screens(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = match params.get("action") {
        None | Some(Value::Null) => "list",
        Some(v) => v
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`action` must be a string"))?,
    };
    // Read `back` for every action: a wrong-typed value is refused even where it is unused
    // (ADR-2622), never dropped as if absent.
    let back = crate::params::opt_str(params, "back")?;
    match action {
        "set_back" => {
            let back = match back {
                Some("pop") => regia_ui::screens::ScreenBack::Pop,
                Some("ignore") => regia_ui::screens::ScreenBack::Ignore,
                _ => return Err(RpcError::invalid_params("back must be pop or ignore")),
            };
            if let Some(entity) = entity_param(params)? {
                if world.get_entity(entity).is_err() {
                    return Err(error(ScreenError::UnknownEntity));
                }
                if world.get::<regia_ui::UiScreen>(entity).is_none() {
                    return Err(error(ScreenError::NotAScreen));
                }
                world.entity_mut(entity).insert(back);
            } else {
                world
                    .get_resource_or_insert_with(ScreenStack::default)
                    .set_back_pop(back == regia_ui::screens::ScreenBack::Pop);
            }
            Ok(snapshot(world))
        }
        "back" => {
            let scope = world
                .get_resource::<regia_ui::layers::InputLayers>()
                .and_then(|layers| layers.top_root());
            let changed = scope.is_some_and(|scope| regia_ui::screens::handle_screen_back(world, scope));
            let mut out = snapshot(world);
            out["changed"] = json!(changed);
            out["applied"] = json!(changed);
            Ok(out)
        }
        "list" => {
            // Nothing changed: `applied: false` keeps a read out of the session's change log.
            let mut out = snapshot(world);
            out["applied"] = json!(false);
            Ok(out)
        }
        "push" => {
            let entity = required_entity(params, "push")?;
            push_screen(world, entity).map_err(error)?;
            Ok(snapshot(world))
        }
        "pop" => {
            let popped = pop_screen(world, entity_param(params)?).map_err(error)?;
            let mut out = snapshot(world);
            out["popped"] = json!(popped.to_bits());
            Ok(out)
        }
        "show" => {
            let entity = required_entity(params, "show")?;
            let changed = show_screen(world, entity).map_err(error)?;
            let mut out = snapshot(world);
            out["changed"] = json!(changed);
            Ok(out)
        }
        _ => Err(RpcError::invalid_params(
            "`action` must be list, push, pop, show, back or set_back",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::layers::InputLayers;
    use regia_ui::{ScreenChanged, UiScreen};

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<InputLayers>();
        world.init_resource::<FocusState>();
        world.init_resource::<Messages<ScreenChanged>>();
        world
    }

    fn code(e: RpcError) -> String {
        e.data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    #[test]
    fn lists_pushes_shows_and_pops() {
        let mut world = world();
        let listed = screens(&mut world, &json!({})).expect("list");
        assert_eq!(listed["applied"], false);
        assert_eq!(listed["depth"], 0);
        assert_eq!(listed["top"], Value::Null);
        let title = world.spawn(UiScreen::new("title")).id();
        let options = world.spawn(UiScreen::new("options")).id();
        let out = screens(
            &mut world,
            &json!({ "action": "push", "entity": title.to_bits() }),
        )
        .expect("push");
        assert!(out.get("applied").is_none(), "a push is a real change");
        assert_eq!(out["top_id"], "title");
        let out = screens(
            &mut world,
            &json!({ "action": "push", "entity": options.to_bits() }),
        )
        .expect("push");
        assert_eq!(out["depth"], 2);
        assert_eq!(out["screens"][0]["visible"], false);
        assert_eq!(out["screens"][1]["visible"], true);
        let out = screens(
            &mut world,
            &json!({ "action": "show", "entity": title.to_bits() }),
        )
        .expect("show");
        assert_eq!(out["changed"], true);
        assert_eq!(out["depth"], 1);
        assert_eq!(out["top"], json!(title.to_bits()));
        let out = screens(
            &mut world,
            &json!({ "action": "show", "entity": title.to_bits() }),
        )
        .expect("show");
        assert_eq!(out["changed"], false, "already on top");
        let out = screens(&mut world, &json!({ "action": "pop" })).expect("pop");
        assert_eq!(out["popped"], json!(title.to_bits()));
        assert_eq!(out["depth"], 0);
    }

    #[test]
    fn errors_carry_the_lowercase_screen_codes() {
        let mut world = world();
        let err = screens(&mut world, &json!({ "action": "pop" })).expect_err("empty");
        assert_eq!(code(err), "empty_stack");
        let plain = world.spawn_empty().id();
        let err = screens(
            &mut world,
            &json!({ "action": "push", "entity": plain.to_bits() }),
        )
        .expect_err("no UiScreen");
        assert_eq!(code(err), "not_a_screen");
        let a = world.spawn(UiScreen::new("a")).id();
        let b = world.spawn(UiScreen::new("b")).id();
        screens(&mut world, &json!({ "action": "push", "entity": a.to_bits() })).expect("push a");
        let err =
            screens(&mut world, &json!({ "action": "push", "entity": a.to_bits() })).expect_err("twice");
        assert_eq!(code(err), "already_on_stack");
        screens(&mut world, &json!({ "action": "push", "entity": b.to_bits() })).expect("push b");
        let err =
            screens(&mut world, &json!({ "action": "pop", "entity": a.to_bits() })).expect_err("buried");
        assert_eq!(code(err), "not_on_top");
        assert!(world.despawn(plain), "despawned");
        let err = screens(
            &mut world,
            &json!({ "action": "show", "entity": plain.to_bits() }),
        )
        .expect_err("gone");
        assert_eq!(code(err), "unknown_entity");
        assert!(
            screens(&mut world, &json!({ "action": "push" })).is_err(),
            "entity required"
        );
        assert!(screens(&mut world, &json!({ "action": "spin" })).is_err());
        assert!(screens(&mut world, &json!({ "action": 3 })).is_err());
    }
    #[test]
    fn back_policy_runs_through_the_dispatcher() {
        let mut world = World::new();
        let first = world.spawn((regia_ui::UiNode, UiScreen::new("first"))).id();
        let second = world.spawn((regia_ui::UiNode, UiScreen::new("second"))).id();
        let call = |world: &mut World, params| {
            crate::dispatch::call_in_process(world, "ui.screens", params)
                .expect("sync")
                .expect("screens")
        };
        call(&mut world, json!({"action":"push","entity":first.to_bits()}));
        call(&mut world, json!({"action":"push","entity":second.to_bits()}));
        assert_eq!(call(&mut world, json!({"action":"back"}))["changed"], false);
        assert_eq!(
            call(&mut world, json!({"action":"set_back","back":"pop"}))["back"],
            "pop"
        );
        assert_eq!(call(&mut world, json!({"action":"back"}))["top"], first.to_bits());
        call(
            &mut world,
            json!({"action":"set_back","entity":first.to_bits(),"back":"ignore"}),
        );
        assert_eq!(call(&mut world, json!({"action":"back"}))["changed"], false);
    }

    #[test]
    fn a_wrong_typed_back_is_refused_by_every_action() {
        let mut world = world();
        for action in ["list", "back", "set_back", "pop"] {
            let err = screens(&mut world, &json!({ "action": action, "back": 5 }))
                .expect_err("a numeric back is not pop or ignore");
            assert_eq!(err.code, INVALID_PARAMS, "{action}");
            assert_eq!(
                err.data.as_ref().and_then(|d| d.get("param")),
                Some(&json!("back")),
                "{action}: the error names the parameter"
            );
        }
        assert!(
            screens(&mut world, &json!({ "back": null })).is_ok(),
            "null is absent"
        );
    }
}
