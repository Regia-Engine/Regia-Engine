// SPDX-License-Identifier: MPL-2.0
//! End-to-end test of the Steam Input bridge (ADR-350) against the mock backend: action set
//! selection and switching, analog and digital actions held on `InputInjection`, the UI actions
//! reported as gamepad button messages, runtime disable, and the two agent methods.

use std::sync::Arc;

use serde_json::json;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::message::MessageCursor;
use regia_input::{
    ActionKind, Binding, InputConfig, InputContext, InputContexts, InputInjection, InputSource, Stick,
};
use regia_platform::{AppLifecycle, ButtonState, GamepadButton, GamepadButtonChanged};
use regia_steam::{
    MockSteam, STEAM_INPUT_PAD_DEVICE, SteamBackend, SteamInputSettings, SteamInputStatus, SteamPlugin,
};

fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action("Move", ActionKind::Axis2D)
        .action("Confirm", ActionKind::Button)
        .context("Gameplay", InputContext::new(0))
        .context("Menu", InputContext::consuming(10))
        .binding_in(
            "Gameplay",
            Binding::new("Jump", InputSource::gamepad_button(GamepadButton::South)),
        )
        .binding_in(
            "Gameplay",
            Binding::new("Move", InputSource::gamepad_stick(Stick::Left)),
        )
        .binding_in(
            "Menu",
            Binding::new("Confirm", InputSource::gamepad_button(GamepadButton::South)),
        )
}

/// An app on the mock backend with the input resources the bridge reads (no `InputPlugin`: the
/// bridge only needs the resources).
fn app_with(mock: &Arc<MockSteam>, enabled: bool) -> App {
    let mut app = App::new();
    app.insert_resource(AppLifecycle::default())
        .insert_resource(input_config())
        .insert_resource(InputContexts::default())
        .insert_resource(InputInjection::default())
        .insert_resource(SteamInputSettings { enabled, player: 0 });
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(0, "Gameplay");
    app.add_plugins(
        SteamPlugin::from_project_settings()
            .with_backend(Arc::clone(mock) as Arc<dyn SteamBackend>)
            .with_achievements(Default::default()),
    );
    app.finish_plugins();
    app
}

/// Runs one frame and returns the gamepad button messages it produced.
fn step(app: &mut App, cursor: &mut MessageCursor<GamepadButtonChanged>) -> Vec<GamepadButtonChanged> {
    app.update();
    cursor
        .read(app.world().resource::<Messages<GamepadButtonChanged>>())
        .copied()
        .collect()
}

fn injected(app: &App, action: &str) -> Option<Vec3> {
    app.world().resource::<InputInjection>().value_for(action, 0)
}

/// Steady Steam Input must resolve each action name once, then read the cached handle.
#[test]
fn a_stable_context_does_not_resolve_action_names_every_frame() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    mock.set_input_digital("gameplay_jump", true);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    let lookups = mock.input_name_lookups();
    assert!(lookups > 0, "the first frame resolves the action set");
    assert_eq!(
        app.world().resource::<SteamInputStatus>().driven_actions,
        vec!["Jump".to_owned()]
    );
    step(&mut app, &mut cursor);
    step(&mut app, &mut cursor);
    assert_eq!(
        mock.input_name_lookups(),
        lookups,
        "later frames must read cached handles, not format the action name again"
    );
    mock.set_input_digital("gameplay_jump", false);
    mock.set_input_analog("gameplay_move", (0.25, 0.5));
    step(&mut app, &mut cursor);
    assert_eq!(
        injected(&app, "Jump"),
        None,
        "a released button drops off the held set"
    );
    assert_eq!(injected(&app, "Move"), Some(Vec3::new(0.25, 0.5, 0.0)));
    assert_eq!(
        app.world().resource::<SteamInputStatus>().driven_actions,
        vec!["Move".to_owned()]
    );
    assert_eq!(
        mock.input_name_lookups(),
        lookups,
        "a value change still uses the cached handle"
    );
}

#[test]
fn disabled_bridge_does_nothing() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    mock.set_input_digital("gameplay_jump", true);
    let mut app = app_with(&mock, false);
    let mut cursor = MessageCursor::default();
    assert!(step(&mut app, &mut cursor).is_empty());
    assert_eq!(
        mock.input_init_calls(),
        0,
        "Steam Input is never initialized when off"
    );
    assert!(mock.input_activations().is_empty());
    assert_eq!(injected(&app, "Jump"), None);
    assert_eq!(
        *app.world().resource::<SteamInputStatus>(),
        SteamInputStatus::default()
    );
}

#[test]
fn a_rejected_action_set_activation_is_retried_until_it_succeeds() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    mock.set_input_activation_fails(true);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    step(&mut app, &mut cursor);
    assert!(mock.input_activations().is_empty(), "every attempt was rejected");
    mock.set_input_activation_fails(false);
    step(&mut app, &mut cursor);
    assert_eq!(mock.input_activations(), vec!["GAMEPLAY".to_owned()]);
    step(&mut app, &mut cursor);
    assert_eq!(
        mock.input_activations().len(),
        1,
        "once activated the set is not re-activated every frame"
    );
}

/// Steam answers action-handle lookups with `0` until the action manifest is loaded (the same
/// window in which the set activation is rejected). A handle resolved in that window must not be
/// cached as "absent" for the rest of the session: once the set activates, the actions work.
#[test]
fn actions_resolved_before_the_manifest_loads_work_once_it_loads() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    mock.set_input_activation_fails(true);
    mock.set_input_digital("gameplay_jump", true);
    mock.set_input_analog("gameplay_move", (0.5, 0.0));
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Jump"), None, "no manifest yet: nothing reads");
    mock.set_input_activation_fails(false);
    step(&mut app, &mut cursor);
    step(&mut app, &mut cursor);
    assert_eq!(
        injected(&app, "Jump"),
        Some(Vec3::X),
        "the button must work once the manifest loaded"
    );
    assert_eq!(injected(&app, "Move"), Some(Vec3::new(0.5, 0.0, 0.0)));
    let lookups = mock.input_name_lookups();
    step(&mut app, &mut cursor);
    assert_eq!(
        mock.input_name_lookups(),
        lookups,
        "after the reload the handles are cached again"
    );
}

#[test]
fn unavailable_steam_keeps_the_bridge_off() {
    let mock = MockSteam::shared_unavailable();
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    assert!(step(&mut app, &mut cursor).is_empty());
    assert_eq!(mock.input_init_calls(), 0);
    assert!(!app.world().resource::<SteamInputStatus>().active);
}

#[test]
fn context_picks_the_action_set_and_actions_are_held_then_released() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();

    step(&mut app, &mut cursor);
    assert_eq!(mock.input_init_calls(), 1);
    assert_eq!(mock.input_activations(), vec!["GAMEPLAY".to_owned()]);
    let status = app.world().resource::<SteamInputStatus>().clone();
    assert!(status.active);
    assert_eq!(status.controllers, 1);
    assert_eq!(status.action_set.as_deref(), Some("GAMEPLAY"));
    assert!(status.driven_actions.is_empty(), "nothing actuated yet");

    mock.set_input_analog("gameplay_move", (0.5, -0.25));
    mock.set_input_digital("gameplay_jump", true);
    step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Move"), Some(Vec3::new(0.5, -0.25, 0.0)));
    assert_eq!(injected(&app, "Jump"), Some(Vec3::X));
    assert_eq!(
        app.world().resource::<SteamInputStatus>().driven_actions,
        vec!["Jump".to_owned(), "Move".to_owned()]
    );
    assert_eq!(
        mock.input_activations().len(),
        1,
        "the set is not re-activated every frame"
    );
    assert_eq!(mock.input_init_calls(), 1);

    // Back to zero: the injection is released so other bindings of the action read devices again.
    mock.set_input_digital("gameplay_jump", false);
    step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Jump"), None);
    assert_eq!(injected(&app, "Move"), Some(Vec3::new(0.5, -0.25, 0.0)));
}

#[test]
fn context_switch_activates_the_new_set_and_releases_the_old_actions() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    mock.set_input_analog("gameplay_move", (1.0, 0.0));
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Move"), Some(Vec3::X));

    app.world_mut().resource_mut::<InputContexts>().push(0, "Menu");
    step(&mut app, &mut cursor);
    assert_eq!(
        mock.input_activations(),
        vec!["GAMEPLAY".to_owned(), "MENU".to_owned()]
    );
    assert_eq!(
        injected(&app, "Move"),
        None,
        "gameplay actions are not read while Menu is on top"
    );

    app.world_mut().resource_mut::<InputContexts>().pop(0, "Menu");
    step(&mut app, &mut cursor);
    assert_eq!(
        mock.input_activations().last().map(String::as_str),
        Some("GAMEPLAY")
    );
    assert_eq!(injected(&app, "Move"), Some(Vec3::X));
}

#[test]
fn non_finite_analog_does_not_hold() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();

    // A misbehaving driver can report a non-finite axis for an *active* analog action; it must
    // read as "not held", not drive the action with NaN/inf every frame.
    mock.set_input_analog("gameplay_move", (f32::NAN, 0.0));
    step(&mut app, &mut cursor);
    assert!(
        app.world()
            .resource::<SteamInputStatus>()
            .driven_actions
            .is_empty(),
        "a NaN axis must not hold the action"
    );
    assert_eq!(injected(&app, "Move"), None);

    mock.set_input_analog("gameplay_move", (f32::INFINITY, 0.0));
    step(&mut app, &mut cursor);
    assert!(
        app.world()
            .resource::<SteamInputStatus>()
            .driven_actions
            .is_empty(),
        "an infinite axis must not hold the action"
    );
    assert_eq!(injected(&app, "Move"), None);

    // A later finite value drives the action again.
    mock.set_input_analog("gameplay_move", (1.0, 0.0));
    step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Move"), Some(Vec3::new(1.0, 0.0, 0.0)));
}

#[test]
fn ui_actions_become_standard_layout_button_edges() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    assert!(step(&mut app, &mut cursor).is_empty());

    mock.set_input_digital("gameplay_ui_accept", true);
    mock.set_input_digital("gameplay_ui_up", true);
    let pressed = step(&mut app, &mut cursor);
    assert_eq!(pressed.len(), 2);
    assert!(
        pressed
            .iter()
            .all(|m| m.device == STEAM_INPUT_PAD_DEVICE && m.state == ButtonState::Pressed && m.value == 1.0)
    );
    let buttons: Vec<_> = pressed.iter().map(|m| m.button).collect();
    assert!(buttons.contains(&GamepadButton::South) && buttons.contains(&GamepadButton::DPadUp));
    assert_eq!(
        app.world().resource::<SteamInputStatus>().ui_down,
        vec!["ui_up".to_owned(), "ui_accept".to_owned()]
    );

    assert!(
        step(&mut app, &mut cursor).is_empty(),
        "held buttons report edges only"
    );

    mock.set_input_digital("gameplay_ui_accept", false);
    let released = step(&mut app, &mut cursor);
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].button, GamepadButton::South);
    assert_eq!(released[0].state, ButtonState::Released);
    assert_eq!(released[0].value, 0.0);
}

/// A UI button held while the player's last context goes away (a cutscene pops every context)
/// must be released: with no context the bridge caches no UI slots, and the button used to stay
/// down in `GamepadButtonChanged` and `SteamInputStatus::ui_down` until a context came back.
#[test]
fn a_ui_button_held_when_the_context_goes_away_is_released() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    mock.set_input_digital("gameplay_ui_accept", true);
    let pressed = step(&mut app, &mut cursor);
    assert_eq!(pressed.len(), 1);
    assert_eq!(pressed[0].state, ButtonState::Pressed);

    assert!(app.world_mut().resource_mut::<InputContexts>().pop(0, "Gameplay"));
    let released = step(&mut app, &mut cursor);
    assert_eq!(released.len(), 1, "the held button is released: {released:?}");
    assert_eq!(released[0].button, GamepadButton::South);
    assert_eq!(released[0].state, ButtonState::Released);
    assert!(app.world().resource::<SteamInputStatus>().ui_down.is_empty());
}

#[test]
fn controller_connecting_later_activates_the_set() {
    let mock = MockSteam::shared();
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    assert!(
        mock.input_activations().is_empty(),
        "nothing to activate without a controller"
    );
    assert!(app.world().resource::<SteamInputStatus>().active);

    mock.set_input_controllers(1);
    step(&mut app, &mut cursor);
    assert_eq!(mock.input_activations(), vec!["GAMEPLAY".to_owned()]);
}

#[test]
fn disabling_at_runtime_releases_everything() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(1);
    mock.set_input_digital("gameplay_jump", true);
    mock.set_input_digital("gameplay_ui_cancel", true);
    let mut app = app_with(&mock, true);
    let mut cursor = MessageCursor::default();
    step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Jump"), Some(Vec3::X));

    app.world_mut().resource_mut::<SteamInputSettings>().enabled = false;
    let released = step(&mut app, &mut cursor);
    assert_eq!(injected(&app, "Jump"), None);
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].button, GamepadButton::East);
    assert_eq!(released[0].state, ButtonState::Released);
    assert_eq!(
        *app.world().resource::<SteamInputStatus>(),
        SteamInputStatus::default()
    );
}

#[test]
fn agent_methods_report_the_bridge_and_the_manifest() {
    let mock = MockSteam::shared();
    mock.set_input_controllers(2);
    let mut app = app_with(&mock, true);
    app.update();
    let extension = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .clone();

    let (schema, handler) = extension.method("steam.input").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    let result = handler(app.world_mut(), &json!({})).expect("steam.input");
    assert_eq!(result["plugin"], true);
    assert_eq!(result["enabled"], true);
    assert_eq!(result["active"], true);
    assert_eq!(result["controllers"], 2);
    assert_eq!(result["action_set"], "GAMEPLAY");

    let (schema, handler) = extension.method("steam.input_manifest").expect("registered");
    assert_eq!(schema.permission.name(), "read");
    let result = handler(app.world_mut(), &json!({})).expect("steam.input_manifest");
    assert_eq!(result["ok"], true);
    let manifest = result["manifest"].as_str().expect("manifest text");
    assert!(manifest.contains("\"gameplay_move\"") && manifest.contains("\"menu_confirm\""));

    // A config the manifest cannot express is reported in the result, not as a protocol error.
    app.world_mut().insert_resource(
        InputConfig::new()
            .action("Fly", ActionKind::Axis3D)
            .binding(Binding::new("Fly", InputSource::gamepad_stick(Stick::Left))),
    );
    let result = handler(app.world_mut(), &json!({})).expect("steam.input_manifest");
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().is_some_and(|e| e.contains("Fly")));
}
