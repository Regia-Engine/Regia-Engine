// SPDX-License-Identifier: MPL-2.0
//! `input.rebind_state`, `input.rebind`, `input.rebind_cancel`, `input.rebind_reset`,
//! `input.rebind_axis`, `input.rebind_save`, `input.rebind_load` and `input.rebind_profiles`: the
//! rebinding surface of the input layer (ADR-1801, ADR-2129, book `agents/input-rebind.md`). The
//! capture, override merge and save are `regia_input`'s; these methods start, cancel and reset them
//! and report the result.

use regia_ecs::prelude::*;
use regia_input::{
    ActionKind, AxisTuning, InputConfig, InputOverrides, InputRebinder, RebindError, RebindMode, RebindSlot,
    axis_tuning, binding_conflicts, binding_label, binding_label_in, cancel_rebind, has_dead_zone_axis,
    is_chord_helper, list_profiles, load_profile, reset_action, reset_all, save_profile, set_axis_tuning,
    start_rebind_slot,
};
use serde_json::{Value, json};

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn error(e: &RebindError) -> RpcError {
    let (code, regia, hint) = match e {
        RebindError::NotInstalled => (UNSUPPORTED, "F0013", "add regia_input::InputPlugin to the app"),
        RebindError::UnknownContext(_) => (
            NOT_FOUND,
            "F0003",
            "input.rebind_state lists the contexts and their actions",
        ),
        RebindError::UnknownAction(_) => (
            NOT_FOUND,
            "F0003",
            "input.rebind_state lists the actions a context binds",
        ),
        RebindError::NoDefault(_) => (
            NOT_FOUND,
            "F0003",
            "the project config binds no default for this action; rebind it instead",
        ),
        RebindError::BadMode { .. } => (
            INVALID_PARAMS,
            "F0002",
            "input.rebind_state lists the modes each action accepts",
        ),
        RebindError::NotPending => (INVALID_PARAMS, "F0002", "start a capture with input.rebind first"),
        RebindError::BadTuning(_) => (INVALID_PARAMS, "F0002", "dead_zone runs from 0 to 0.95"),
        RebindError::NotTunable(_) => (
            INVALID_PARAMS,
            "F0002",
            "input.rebind_state lists the tuning of each action per device slot",
        ),
        RebindError::BadProfile(_) => (
            INVALID_PARAMS,
            "F0002",
            "a profile name is 1 to 32 of a-z, 0-9, _ and -",
        ),
        RebindError::ProfileMissing(_) => (
            NOT_FOUND,
            "F0003",
            "input.rebind_profiles lists the saved profiles",
        ),
        RebindError::ProfileCorrupt(_) => {
            (INVALID_PARAMS, "F0002", "fix or delete the file; nothing changed")
        }
        RebindError::SaveFailed(_) => (
            INTERNAL_ERROR,
            "F0001",
            "input.rebind_state reports persist_blocked; a named profile can still be saved",
        ),
    };
    let mut out = RpcError::new(code, regia, e.to_string(), hint);
    if let Some(data) = out.data.as_mut() {
        data["code"] = json!(e.code());
    }
    out
}

fn kind_name(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Button => "button",
        ActionKind::Axis1D => "axis1d",
        ActionKind::Axis2D => "axis2d",
        ActionKind::Axis3D => "axis3d",
    }
}

fn slot_param(params: &Value) -> Result<RebindSlot, RpcError> {
    match params.get("slot") {
        None | Some(Value::Null) => Ok(RebindSlot::All),
        Some(Value::String(name)) => RebindSlot::parse(name)
            .ok_or_else(|| RpcError::invalid_params("`slot` must be one of all, keyboard_mouse, gamepad")),
        Some(_) => Err(RpcError::invalid_params("`slot` must be a string")),
    }
}

fn profile_param(params: &Value) -> Result<Option<&str>, RpcError> {
    match params.get("profile") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => Ok(Some(name)),
        Some(_) => Err(RpcError::invalid_params("`profile` must be a string")),
    }
}

fn tuning_json(tuning: AxisTuning) -> Value {
    json!({
        "dead_zone": tuning.dead_zone,
        "invert_x": tuning.invert_x,
        "invert_y": tuning.invert_y,
    })
}

/// One action's bindings, tuning and conflicts per device slot, for `input.rebind_state`.
fn slot_rows(config: &InputConfig, context: &str, action: &str) -> Value {
    let mut out = serde_json::Map::new();
    for slot in [RebindSlot::KeyboardMouse, RebindSlot::Gamepad] {
        let tuning = axis_tuning(config, context, action, slot);
        out.insert(
            slot.name().to_owned(),
            json!({
                "binding": binding_label_in(config, context, action, slot),
                "tuning": tuning.map(tuning_json),
                "has_dead_zone": has_dead_zone_axis(config, context, action, slot),
            }),
        );
    }
    Value::Object(out)
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (string) is required")))
}

/// `input.rebind_state { context? }`: the capture in progress, the last outcome, and every
/// context's actions with their current binding label.
pub(crate) fn state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let config = world
        .get_resource::<InputConfig>()
        .ok_or_else(|| error(&RebindError::NotInstalled))?;
    let only = crate::params::opt_str(params, "context")?;
    if let Some(name) = only
        && !config.contexts.contains_key(name)
    {
        return Err(error(&RebindError::UnknownContext(name.to_string())));
    }
    let overrides = world.get_resource::<InputOverrides>();
    let mut contexts = serde_json::Map::new();
    for (name, ctx) in &config.contexts {
        if only.is_some_and(|o| o != name) {
            continue;
        }
        let mut actions: Vec<&String> = ctx
            .bindings
            .iter()
            .map(|b| &b.action)
            .filter(|a| !is_chord_helper(a))
            .collect();
        actions.sort();
        actions.dedup();
        let rows: Vec<Value> = actions
            .into_iter()
            .map(|action| {
                let kind = config.kind(action);
                let modes: Vec<&str> = [
                    RebindMode::Single,
                    RebindMode::Keys1D,
                    RebindMode::Keys2D,
                    RebindMode::Chord,
                ]
                .into_iter()
                .filter(|m| kind.is_some_and(|k| m.fits(k)))
                .map(RebindMode::name)
                .collect();
                json!({
                    "action": action,
                    "kind": kind.map(kind_name),
                    "binding": binding_label(config, name, action),
                    "overridden": overrides.is_some_and(|o| {
                        o.contexts.get(name).is_some_and(|c| c.contains_key(action.as_str()))
                            || o.slots.get(name).is_some_and(|c| c.contains_key(action.as_str()))
                    }),
                    "modes": modes,
                    "slots": slot_rows(config, name, action),
                    "conflicts": binding_conflicts(config, name, action),
                })
            })
            .collect();
        contexts.insert(name.clone(), Value::Array(rows));
    }
    let rebinder = world.get_resource::<InputRebinder>();
    let pending = rebinder.and_then(|r| {
        let (context, action) = r.pending()?;
        let progress = r.progress()?;
        Some(json!({
            "context": context,
            "action": action,
            "mode": progress.mode.name(),
            "slot": r.slot().name(),
            "step": progress.step,
            "total": progress.total,
            "prompt": progress.prompt,
            "captured": progress.captured,
        }))
    });
    let outcome = rebinder.and_then(|r| r.outcome.as_ref()).map(|o| {
        json!({
            "context": o.context,
            "action": o.action,
            "input": regia_input::source_label(&o.input),
            "conflicts": o.conflicts,
            "slot": o.slot.name(),
        })
    });
    Ok(json!({
        "pending": pending,
        "outcome": outcome,
        "persist_error": rebinder.and_then(|r| r.persist_error.clone()),
        "persist_blocked": rebinder.is_some_and(InputRebinder::persist_blocked),
        "contexts": contexts,
    }))
}

/// `input.rebind { context, action, mode?, slot? }`: starts a capture; the next input (or inputs,
/// for a group mode) rebinds the action. With a `slot` only that device's inputs count, and only
/// that device's bindings are replaced.
pub(crate) fn start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let context = required_str(params, "context")?;
    let action = required_str(params, "action")?;
    let mode = match params.get("mode") {
        None | Some(Value::Null) => RebindMode::Single,
        Some(Value::String(name)) => RebindMode::parse(name)
            .ok_or_else(|| RpcError::invalid_params("`mode` must be one of single, keys1d, keys2d, chord"))?,
        Some(_) => return Err(RpcError::invalid_params("`mode` must be a string")),
    };
    let slot = slot_param(params)?;
    start_rebind_slot(world, context, action, mode, slot).map_err(|e| error(&e))?;
    let steps = mode.steps();
    Ok(json!({
        "context": context,
        "action": action,
        "mode": mode.name(),
        "slot": slot.name(),
        "steps": steps,
    }))
}

/// `input.rebind_cancel {}`: drops the pending capture.
pub(crate) fn cancel(world: &mut World) -> Result<Value, RpcError> {
    cancel_rebind(world).map_err(|e| error(&e))?;
    Ok(json!({ "cancelled": true }))
}

/// `input.rebind_reset { context?, action? }`: resets one action, or everything when `action` is
/// omitted, to the project bindings and saves.
pub(crate) fn reset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match crate::params::opt_str(params, "action")? {
        Some(action) => {
            let context = required_str(params, "context")?;
            let removed = reset_action(world, context, action).map_err(|e| error(&e))?;
            Ok(json!({ "context": context, "action": action, "reset": removed }))
        }
        None => {
            let count = reset_all(world).map_err(|e| error(&e))?;
            Ok(json!({ "reset": count }))
        }
    }
}

/// `input.rebind_axis { context, action, slot?, dead_zone?, invert_x?, invert_y? }`: changes the
/// dead zone and invert of an action's axis bindings and saves. A field left out keeps its value;
/// `dead_zone: null` removes the dead zone.
pub(crate) fn axis(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let context = required_str(params, "context")?;
    let action = required_str(params, "action")?;
    let slot = slot_param(params)?;
    let flag = |key: &str, current: bool| match params.get(key) {
        None => Ok(current),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    };
    let current = {
        let config = world
            .get_resource::<InputConfig>()
            .ok_or_else(|| error(&RebindError::NotInstalled))?;
        if !config.contexts.contains_key(context) {
            return Err(error(&RebindError::UnknownContext(context.to_string())));
        }
        axis_tuning(config, context, action, slot).unwrap_or_default()
    };
    let dead_zone = match params.get("dead_zone") {
        None => current.dead_zone,
        Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`dead_zone` must be a number or null"))?
                as f32,
        ),
    };
    let tuning = AxisTuning {
        dead_zone,
        invert_x: flag("invert_x", current.invert_x)?,
        invert_y: flag("invert_y", current.invert_y)?,
    };
    let now = set_axis_tuning(world, context, action, slot, tuning).map_err(|e| error(&e))?;
    Ok(json!({
        "context": context,
        "action": action,
        "slot": slot.name(),
        "tuning": tuning_json(now),
    }))
}

/// `input.rebind_save { profile? }`: writes the live overrides to a named profile, or to the active
/// file when `profile` is left out.
pub(crate) fn save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let profile = profile_param(params)?;
    let path = save_profile(world, profile).map_err(|e| error(&e))?;
    Ok(json!({ "profile": profile, "path": path }))
}

/// `input.rebind_load { profile? }`: replaces the live overrides with a named profile (it becomes
/// the active one), or reloads the active file when `profile` is left out.
pub(crate) fn load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let profile = profile_param(params)?;
    let count = load_profile(world, profile).map_err(|e| error(&e))?;
    Ok(json!({ "profile": profile, "overrides": count }))
}

/// `input.rebind_profiles {}`: the saved profile names.
pub(crate) fn profiles(world: &mut World) -> Result<Value, RpcError> {
    if world.get_resource::<InputConfig>().is_none() {
        return Err(error(&RebindError::NotInstalled));
    }
    Ok(json!({ "profiles": list_profiles(world) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::*;
    use regia_input::prelude::*;
    use regia_platform::ButtonState;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(InputPlugin::new(
            InputConfig::new()
                .action("Jump", ActionKind::Button)
                .action("Move", ActionKind::Axis2D)
                .binding(Binding::new("Jump", InputSource::key("Space")))
                .binding(Binding::new(
                    "Move",
                    InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
                )),
        ));
        app.update();
        app
    }

    fn tap(app: &mut App, key: &str) {
        for state in [ButtonState::Pressed, ButtonState::Released] {
            app.world_mut().write_message(regia_platform::KeyboardInput {
                window: Entity::PLACEHOLDER,
                physical_key: key.to_string(),
                logical_key: key.to_string(),
                state,
                repeat: false,
                timestamp: regia_platform::InputTimestamp::ZERO,
            });
            app.update();
        }
    }

    fn code(e: RpcError) -> String {
        e.data.expect("data")["code"].as_str().expect("code").to_owned()
    }

    #[test]
    fn state_lists_actions_with_labels_kinds_and_modes() {
        let mut app = app();
        let out = state(app.world_mut(), &json!({})).expect("state");
        let rows = out["contexts"]["Gameplay"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["action"], json!("Jump"));
        assert_eq!(rows[0]["binding"], json!("Space"));
        assert_eq!(rows[0]["modes"], json!(["single", "chord"]));
        assert_eq!(rows[1]["binding"], json!("W/S/A/D"));
        assert_eq!(rows[1]["modes"], json!(["single", "keys2d"]));
        assert_eq!(rows[1]["overridden"], json!(false));
        assert_eq!(out["pending"], Value::Null);
        let err = state(app.world_mut(), &json!({ "context": "Menu" })).expect_err("context");
        assert_eq!(code(err), "rebind_unknown_context");
    }

    #[test]
    fn rebind_captures_a_chord_and_reset_restores_it() {
        let mut app = app();
        let started = start(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Jump", "mode": "chord" }),
        )
        .expect("start");
        assert_eq!(started["steps"], json!(["modifier", "key"]));
        let pending = state(app.world_mut(), &json!({})).expect("state")["pending"].clone();
        assert_eq!(pending["prompt"], json!("modifier"));
        tap(&mut app, "ControlLeft");
        tap(&mut app, "KeyJ");
        let out = state(app.world_mut(), &json!({})).expect("state");
        assert_eq!(out["pending"], Value::Null);
        assert_eq!(out["outcome"]["action"], json!("Jump"));
        assert_eq!(
            out["contexts"]["Gameplay"][0]["binding"],
            json!("Left Control + J")
        );
        assert_eq!(out["contexts"]["Gameplay"][0]["overridden"], json!(true));

        let reset_out = reset(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Jump" }),
        )
        .expect("reset");
        assert_eq!(reset_out["reset"], json!(true));
        let out = state(app.world_mut(), &json!({})).expect("state");
        assert_eq!(out["contexts"]["Gameplay"][0]["binding"], json!("Space"));
        assert_eq!(
            reset(app.world_mut(), &json!({})).expect("all")["reset"],
            json!(0)
        );
    }

    #[test]
    fn cancel_and_errors_carry_lowercase_codes() {
        let mut app = app();
        assert_eq!(
            code(cancel(app.world_mut()).expect_err("idle")),
            "rebind_not_pending"
        );
        start(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Move" }),
        )
        .expect("start");
        assert_eq!(cancel(app.world_mut()).expect("cancel")["cancelled"], json!(true));
        let w = app.world_mut();
        assert_eq!(
            code(
                start(
                    w,
                    &json!({ "context": "Gameplay", "action": "Jump", "mode": "keys2d" })
                )
                .expect_err("mode")
            ),
            "rebind_bad_mode"
        );
        assert_eq!(
            code(start(w, &json!({ "context": "Gameplay", "action": "Nope" })).expect_err("action")),
            "rebind_unknown_action"
        );
        assert_eq!(
            code(reset(w, &json!({ "context": "Gameplay", "action": "Nope" })).expect_err("default")),
            "rebind_no_default"
        );
        assert!(start(w, &json!({ "action": "Jump" })).is_err());
        assert!(
            start(
                w,
                &json!({ "context": "Gameplay", "action": "Jump", "mode": "zigzag" })
            )
            .is_err()
        );
        let mut bare = World::new();
        assert_eq!(
            code(state(&mut bare, &json!({})).expect_err("none")),
            "input_not_installed"
        );
    }

    #[test]
    fn a_slot_capture_keeps_the_other_device_and_state_reports_both() {
        let mut app = App::new();
        app.add_plugins(InputPlugin::new(
            InputConfig::new()
                .action("Jump", ActionKind::Button)
                .action("Move", ActionKind::Axis2D)
                .binding(Binding::new("Jump", InputSource::key("Space")))
                .binding(Binding::new(
                    "Jump",
                    InputSource::gamepad_button(regia_platform::GamepadButton::South),
                ))
                .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left))),
        ));
        app.update();
        let started = start(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Jump", "slot": "keyboard_mouse" }),
        )
        .expect("start");
        assert_eq!(started["slot"], json!("keyboard_mouse"));
        assert_eq!(
            state(app.world_mut(), &json!({})).expect("state")["pending"]["slot"],
            json!("keyboard_mouse")
        );
        tap(&mut app, "KeyJ");
        let out = state(app.world_mut(), &json!({})).expect("state");
        let jump = &out["contexts"]["Gameplay"][0];
        assert_eq!(jump["slots"]["keyboard_mouse"]["binding"], json!("J"));
        assert_eq!(jump["slots"]["gamepad"]["binding"], json!("South"));
        assert_eq!(jump["overridden"], json!(true));
        assert_eq!(out["outcome"]["slot"], json!("keyboard_mouse"));
        assert!(
            start(
                app.world_mut(),
                &json!({ "context": "Gameplay", "action": "Jump", "slot": "toes" })
            )
            .is_err()
        );
    }

    #[test]
    fn axis_tuning_changes_only_the_named_fields_and_reports_errors() {
        let mut app = App::new();
        app.add_plugins(InputPlugin::new(
            InputConfig::new()
                .action("Jump", ActionKind::Button)
                .action("Move", ActionKind::Axis2D)
                .binding(Binding::new("Jump", InputSource::key("Space")))
                .binding(
                    Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                        .with_modifier(DeadZone::radial(0.25, 1.0)),
                ),
        ));
        app.update();
        let out = axis(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Move", "slot": "gamepad", "invert_y": true }),
        )
        .expect("axis");
        assert_eq!(out["tuning"]["dead_zone"], json!(0.25));
        assert_eq!(out["tuning"]["invert_y"], json!(true));
        let out = axis(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Move", "slot": "gamepad", "dead_zone": 0.5 }),
        )
        .expect("axis");
        assert_eq!(out["tuning"]["dead_zone"], json!(0.5));
        assert_eq!(out["tuning"]["invert_y"], json!(true), "left out, so kept");
        let out = axis(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Move", "slot": "gamepad", "dead_zone": null }),
        )
        .expect("axis");
        assert_eq!(out["tuning"]["dead_zone"], Value::Null);
        let row = state(app.world_mut(), &json!({})).expect("state")["contexts"]["Gameplay"][1].clone();
        assert_eq!(row["slots"]["gamepad"]["tuning"]["invert_y"], json!(true));
        assert_eq!(row["slots"]["keyboard_mouse"]["tuning"], Value::Null);

        let w = app.world_mut();
        assert_eq!(
            code(
                axis(
                    w,
                    &json!({ "context": "Gameplay", "action": "Move", "dead_zone": 2.0 })
                )
                .expect_err("range")
            ),
            "rebind_bad_tuning"
        );
        assert_eq!(
            code(axis(w, &json!({ "context": "Gameplay", "action": "Jump" })).expect_err("no axis")),
            "rebind_not_tunable"
        );
        assert_eq!(
            code(axis(w, &json!({ "context": "Gameplay", "action": "Nope" })).expect_err("action")),
            "rebind_unknown_action"
        );
        assert_eq!(
            code(axis(w, &json!({ "context": "Menu", "action": "Move" })).expect_err("context")),
            "rebind_unknown_context"
        );
        assert!(
            axis(
                w,
                &json!({ "context": "Gameplay", "action": "Move", "invert_x": 1 })
            )
            .is_err()
        );
        assert!(
            axis(
                w,
                &json!({ "context": "Gameplay", "action": "Move", "dead_zone": "a" })
            )
            .is_err()
        );
        let mut bare = World::new();
        assert_eq!(
            code(axis(&mut bare, &json!({ "context": "a", "action": "b" })).expect_err("none")),
            "input_not_installed"
        );
        assert_eq!(
            code(profiles(&mut bare).expect_err("none")),
            "input_not_installed"
        );
    }

    #[test]
    fn profile_methods_round_trip_through_the_user_mount() {
        let dir = std::env::temp_dir().join(format!(
            "regia-agent-rebind-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let mut vfs = regia_platform::Vfs::new();
        vfs.mount(
            regia_platform::MountPoint::User,
            regia_platform::DirMount::writable(dir.clone()),
        );
        let mut app = App::new();
        app.insert_resource(vfs);
        app.add_plugins(InputPlugin::new(
            InputConfig::new()
                .action("Jump", ActionKind::Button)
                .binding(Binding::new("Jump", InputSource::key("Space"))),
        ));
        app.update();
        start(
            app.world_mut(),
            &json!({ "context": "Gameplay", "action": "Jump" }),
        )
        .expect("start");
        tap(&mut app, "KeyK");
        let saved = save(app.world_mut(), &json!({ "profile": "mine" })).expect("save");
        assert_eq!(saved["path"], json!("user://input_profiles/mine.ron"));
        assert_eq!(
            profiles(app.world_mut()).expect("list")["profiles"],
            json!(["mine"])
        );
        reset(app.world_mut(), &json!({})).expect("reset");
        assert_eq!(
            load(app.world_mut(), &json!({ "profile": "mine" })).expect("load")["overrides"],
            json!(1)
        );
        let row = state(app.world_mut(), &json!({})).expect("state")["contexts"]["Gameplay"][0].clone();
        assert_eq!(row["binding"], json!("K"));
        assert_eq!(
            code(load(app.world_mut(), &json!({ "profile": "other" })).expect_err("missing")),
            "rebind_profile_missing"
        );
        assert_eq!(
            code(save(app.world_mut(), &json!({ "profile": "A B" })).expect_err("name")),
            "rebind_bad_profile"
        );
        assert!(save(app.world_mut(), &json!({ "profile": 3 })).is_err());
        assert_eq!(
            save(app.world_mut(), &json!({})).expect("active")["profile"],
            Value::Null
        );
        std::fs::write(dir.join("input_profiles/bad.ron"), "((").expect("write");
        assert_eq!(
            code(load(app.world_mut(), &json!({ "profile": "bad" })).expect_err("corrupt")),
            "rebind_profile_corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
