// SPDX-License-Identifier: MPL-2.0
//! `ui.layers`: list, push and pop UI input layers (book `agents/ui-input.md`).
//!
//! Only the top layer receives pointer and navigation input (ADR-273 lane 4, ADR-293). Push and
//! pop apply at once, so the answer already shows the new stack and focus.

use regia_ecs::prelude::*;
use regia_ui::focus::FocusState;
use regia_ui::layers::{LayerError, pop_layer, push_layer};
use regia_ui::{InputLayers, LayerInputMode};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

fn error(e: LayerError) -> RpcError {
    match e {
        LayerError::UnknownEntity => RpcError::new(
            NOT_FOUND,
            e.code(),
            "the entity does not exist",
            "take entity ids from ui.layers list or a ui.query result",
        ),
        LayerError::AlreadyLayer => RpcError::new(
            INVALID_PARAMS,
            e.code(),
            "the entity is already an input layer",
            "pop it first, or push a different node",
        ),
        LayerError::NotALayer => RpcError::new(
            NOT_FOUND,
            e.code(),
            "the entity is not an input layer",
            "list the layers with ui.layers",
        ),
        LayerError::NoLayers => RpcError::new(
            NOT_FOUND,
            e.code(),
            "there is no input layer to pop",
            "push one with ui.layers { action: \"push\", entity }",
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
            .ok_or_else(|| RpcError::invalid_params("`entity` must be entity bits from ui.layers list")),
    }
}

fn snapshot(world: &World) -> Value {
    let (rows, depth, mode) = match world.get_resource::<InputLayers>() {
        Some(layers) => (
            layers
                .entries()
                .iter()
                .map(|l| {
                    json!({
                        "entity": l.root.to_bits(),
                        "mode": l.mode.name(),
                        "previous_mode": l.previous_mode.name(),
                        "restore_focus": l.restore_focus.map(Entity::to_bits),
                    })
                })
                .collect::<Vec<_>>(),
            layers.depth(),
            layers.mode(),
        ),
        None => (Vec::new(), 0, LayerInputMode::UiAndGame),
    };
    let focused = world
        .get_resource::<FocusState>()
        .and_then(|f| f.focused)
        .map(Entity::to_bits);
    json!({ "layers": rows, "depth": depth, "mode": mode.name(), "focused": focused })
}

/// `ui.layers { action?, entity?, mode? }`.
pub(crate) fn layers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = match params.get("action") {
        None | Some(Value::Null) => "list",
        Some(v) => v
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`action` must be a string"))?,
    };
    match action {
        "list" => {
            // Nothing changed: `applied: false` keeps a read out of the session's change log.
            let mut out = snapshot(world);
            out["applied"] = json!(false);
            Ok(out)
        }
        "push" => {
            let entity = entity_param(params)?
                .ok_or_else(|| RpcError::invalid_params("`entity` is required to push a layer"))?;
            let mode = match params.get("mode") {
                None | Some(Value::Null) => LayerInputMode::UiAndGame,
                Some(v) => v.as_str().and_then(LayerInputMode::parse).ok_or_else(|| {
                    RpcError::invalid_params("`mode` must be \"ui_only\", \"ui_and_game\" or \"game_only\"")
                })?,
            };
            push_layer(world, entity, mode).map_err(error)?;
            Ok(snapshot(world))
        }
        "pop" => {
            let popped = pop_layer(world, entity_param(params)?).map_err(error)?;
            let mut out = snapshot(world);
            out["popped"] = json!(popped.to_bits());
            Ok(out)
        }
        _ => Err(RpcError::invalid_params(
            "`action` must be \"list\", \"push\" or \"pop\"",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::InputLayer;

    fn world() -> World {
        let mut world = World::new();
        world.init_resource::<InputLayers>();
        world.init_resource::<FocusState>();
        world
    }

    #[test]
    fn a_list_is_marked_not_applied_so_it_is_not_audited_as_a_change() {
        let mut world = world();
        let listed = layers(&mut world, &json!({ "action": "list" })).expect("list");
        assert_eq!(listed["applied"], false);
        let root = world.spawn_empty().id();
        let pushed =
            layers(&mut world, &json!({ "action": "push", "entity": root.to_bits() })).expect("push");
        assert!(pushed.get("applied").is_none(), "a push is a real change");
    }

    #[test]
    fn lists_pushes_and_pops() {
        let mut world = world();
        let out = layers(&mut world, &json!({})).expect("list");
        assert_eq!(out["depth"], json!(0));
        assert_eq!(out["mode"], json!("ui_and_game"));
        let root = world.spawn_empty().id();
        let out = layers(
            &mut world,
            &json!({ "action": "push", "entity": root.to_bits(), "mode": "ui_only" }),
        )
        .expect("push");
        assert_eq!(out["depth"], json!(1));
        assert_eq!(out["mode"], json!("ui_only"));
        assert_eq!(out["layers"][0]["entity"], json!(root.to_bits()));
        assert_eq!(out["layers"][0]["previous_mode"], json!("ui_and_game"));
        assert!(world.get::<InputLayer>(root).is_some());
        let out = layers(&mut world, &json!({ "action": "pop" })).expect("pop");
        assert_eq!(out["depth"], json!(0));
        assert_eq!(out["popped"], json!(root.to_bits()));
        assert_eq!(out["mode"], json!("ui_and_game"), "previous mode restored");
    }

    #[test]
    fn errors_carry_lowercase_codes() {
        let mut world = world();
        let code = |e: RpcError| e.data.expect("data")["code"].as_str().expect("code").to_owned();
        let err = layers(&mut world, &json!({ "action": "pop" })).expect_err("empty");
        assert_eq!(code(err), "no_layers");
        let root = world.spawn_empty().id();
        let err = layers(&mut world, &json!({ "action": "pop", "entity": root.to_bits() }))
            .expect_err("not a layer");
        assert_eq!(code(err), "not_a_layer");
        layers(&mut world, &json!({ "action": "push", "entity": root.to_bits() })).expect("push");
        let err =
            layers(&mut world, &json!({ "action": "push", "entity": root.to_bits() })).expect_err("twice");
        assert_eq!(code(err), "already_layer");
        assert!(
            layers(&mut world, &json!({ "action": "push" })).is_err(),
            "entity required"
        );
        assert!(
            layers(
                &mut world,
                &json!({ "action": "push", "entity": root.to_bits(), "mode": "x" })
            )
            .is_err()
        );
        assert!(layers(&mut world, &json!({ "action": "spin" })).is_err());
    }
}
