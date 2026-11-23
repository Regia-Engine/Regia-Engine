// SPDX-License-Identifier: MPL-2.0
//! `ui.shortcuts`, `ui.shortcut_trigger` and `ui.shortcut_contexts`: inspect and drive the UI
//! shortcut registry (book `agents/ui-shortcuts.md`, ADR-273 lane 7, ADR-374).

use regia_ecs::prelude::*;
use regia_ui::shortcuts::{ShortcutError, UiShortcuts, trigger_action};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn error(e: &ShortcutError) -> RpcError {
    let (code, hint) = match e {
        ShortcutError::UnknownAction(_) => (NOT_FOUND, "list the actions with ui.shortcuts"),
        ShortcutError::Inactive(_) => (
            INVALID_PARAMS,
            "turn its context on with ui.shortcut_contexts, or check which shortcut shadows it in ui.shortcuts",
        ),
        ShortcutError::Ambiguous(_) => (
            INVALID_PARAMS,
            "ui.shortcuts lists the conflicts; rebind one of the actions",
        ),
        ShortcutError::NotInstalled => (UNSUPPORTED, "the app needs regia_ui::UiPlugin"),
        ShortcutError::BadChord(_) | ShortcutError::Duplicate(_) => (INVALID_PARAMS, "see agent.schema"),
    };
    let regia = if code == NOT_FOUND {
        "F0003"
    } else if code == UNSUPPORTED {
        "F0013"
    } else {
        "F0002"
    };
    let mut out = RpcError::new(code, regia, e.to_string(), hint);
    if let Some(data) = out.data.as_mut() {
        data["code"] = json!(e.code());
    }
    out
}

fn registry(world: &World) -> Result<&UiShortcuts, RpcError> {
    world
        .get_resource::<UiShortcuts>()
        .ok_or_else(|| error(&ShortcutError::NotInstalled))
}

/// `ui.shortcuts {}`: every shortcut, the conflicts and the active contexts.
pub(crate) fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let reg = registry(world)?;
    let shortcuts: Vec<Value> = reg
        .shortcuts()
        .iter()
        .map(|s| {
            json!({
                "action": s.action,
                "chord": s.chord.canonical(),
                "display": s.chord.platform_display(),
                "scope": s.scope.name(),
                "label": s.label,
                "active": reg.activation(&s.action).is_ok(),
            })
        })
        .collect();
    let conflicts: Vec<Value> = reg
        .conflicts()
        .iter()
        .map(|c| {
            json!({
                "chord": c.chord.canonical(),
                "kind": c.kind.name(),
                "actions": c.actions,
                "scopes": [c.scopes[0].name(), c.scopes[1].name()],
            })
        })
        .collect();
    Ok(json!({
        "shortcuts": shortcuts,
        "conflicts": conflicts,
        "contexts": reg.contexts(),
    }))
}

/// `ui.shortcut_trigger { action }`: fires the action as if its keys were pressed.
pub(crate) fn trigger(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`action` (string) is required"))?;
    let fired = trigger_action(world, action).map_err(|e| error(&e))?;
    Ok(json!({
        "action": fired.action,
        "chord": fired.chord.canonical(),
        "scope": fired.scope.name(),
    }))
}

/// `ui.shortcut_contexts { contexts }`: replaces the active contexts.
pub(crate) fn contexts(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let list = params
        .get("contexts")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`contexts` (array of strings) is required"))?;
    let names = list
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| RpcError::invalid_params("`contexts` must hold strings"))?;
    let mut reg = world
        .get_resource_mut::<UiShortcuts>()
        .ok_or_else(|| error(&ShortcutError::NotInstalled))?;
    reg.set_contexts(names);
    Ok(json!({ "contexts": reg.contexts() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ui::shortcuts::{Chord, ShortcutScope, UiShortcut};

    fn world() -> World {
        let mut world = World::new();
        let mut reg = UiShortcuts::default();
        reg.register(UiShortcut::new(Chord::parse("Ctrl+S").expect("chord"), "save").labeled("Save"))
            .expect("registers");
        reg.register(
            UiShortcut::new(Chord::parse("Ctrl+B").expect("chord"), "bold")
                .in_scope(ShortcutScope::Context("editor".into())),
        )
        .expect("registers");
        reg.register(UiShortcut::new(Chord::parse("F2").expect("chord"), "a"))
            .expect("registers");
        reg.register(UiShortcut::new(Chord::parse("F2").expect("chord"), "b"))
            .expect("registers");
        world.insert_resource(reg);
        world
    }

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    #[test]
    fn lists_shortcuts_conflicts_and_contexts() {
        let mut world = world();
        let out = list(&mut world, &json!({})).expect("list");
        assert_eq!(out["shortcuts"].as_array().expect("array").len(), 4);
        assert_eq!(out["shortcuts"][0]["chord"], json!("Ctrl+S"));
        assert_eq!(out["shortcuts"][0]["active"], json!(true));
        assert_eq!(out["shortcuts"][1]["active"], json!(false), "context off");
        assert_eq!(out["shortcuts"][2]["active"], json!(false), "ambiguous");
        assert_eq!(out["conflicts"][0]["kind"], json!("same_scope"));
        assert_eq!(out["contexts"], json!([]));
    }

    #[test]
    fn contexts_switch_scoped_shortcuts_on() {
        let mut world = world();
        assert_eq!(
            code(trigger(&mut world, &json!({ "action": "bold" })).expect_err("off")),
            "shortcut_inactive"
        );
        crate::contract::assert_listed("ui.shortcut_trigger", "shortcut_inactive");
        let out = contexts(&mut world, &json!({ "contexts": ["editor"] })).expect("set");
        assert_eq!(out["contexts"], json!(["editor"]));
        let fired = trigger(&mut world, &json!({ "action": "bold" })).expect("live");
        assert_eq!(fired["chord"], json!("Ctrl+B"));
        assert_eq!(fired["scope"], json!("editor"));
    }

    #[test]
    fn errors_carry_lowercase_codes() {
        let mut world = world();
        assert_eq!(
            code(trigger(&mut world, &json!({ "action": "nope" })).expect_err("unknown")),
            "unknown_action"
        );
        crate::contract::assert_listed("ui.shortcut_trigger", "unknown_action");
        assert_eq!(
            code(trigger(&mut world, &json!({ "action": "a" })).expect_err("ambiguous")),
            "shortcut_ambiguous"
        );
        crate::contract::assert_listed("ui.shortcut_trigger", "shortcut_ambiguous");
        assert!(trigger(&mut world, &json!({})).is_err());
        assert!(contexts(&mut world, &json!({ "contexts": [1] })).is_err());
        let mut bare = World::new();
        assert_eq!(
            code(list(&mut bare, &json!({})).expect_err("none")),
            "shortcuts_not_installed"
        );
    }
}
