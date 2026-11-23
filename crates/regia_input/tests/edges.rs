// SPDX-License-Identifier: MPL-2.0
//! Action-pipeline edges the other suites leave open: a Touch device assignment, an
//! equal-priority context tie, the exclusive 0.5 analog threshold, a binding with both
//! `Pressed` and `Down`, and a chord whose partner was never declared.
//!
//! Frames are manual 60 Hz. Nothing here sleeps or reads the wall clock.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_input::ActionState;
use regia_input::prelude::*;
use regia_platform::{
    ButtonState, GamepadAxisChanged, GamepadButtonChanged, GamepadConnected, KeyboardInput,
};

/// Builds a headless app with the input plugin and deterministic 60 Hz frames.
fn input_app(config: InputConfig) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app
}

fn send_key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_string(),
        logical_key: name.to_string(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn connect_pad(app: &mut App, id: u64) {
    app.world_mut().write_message(GamepadConnected {
        device: id,
        name: "Test Pad".into(),
    });
}

fn pad_button(app: &mut App, id: u64, button: GamepadButton, value: f32) {
    app.world_mut().write_message(GamepadButtonChanged {
        device: id,
        button,
        state: if value > 0.5 {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        },
        value,
    });
}

fn pad_axis(app: &mut App, id: u64, axis: GamepadAxis, value: f32) {
    app.world_mut().write_message(GamepadAxisChanged {
        device: id,
        axis,
        value,
    });
}

fn action<'a>(app: &'a App, player: u8, name: &str) -> &'a ActionState {
    app.world()
        .resource::<Actions>()
        .player(player)
        .states
        .get(name)
        .expect("declared action")
}

/// Touch assignment drops every device class. Injection still drives the action, and two
/// consuming contexts of equal priority resolve by name, not by stack order.
#[test]
fn touch_assignment_and_equal_priority_name_tie() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")));
    let mut app = input_app(config);
    app.world_mut()
        .resource_mut::<PlayerInputs>()
        .players
        .insert(1, DeviceAssignment::Touch);
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(1, "Gameplay");

    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(action(&app, 0, "Jump").actuated());
    assert_eq!(action(&app, 0, "Jump").phase, ActionPhase::Started);
    assert!(
        !action(&app, 1, "Jump").actuated(),
        "Touch assignment reads no keyboard"
    );
    assert_eq!(action(&app, 1, "Jump").phase, ActionPhase::Idle);

    app.world_mut()
        .resource_mut::<InputInjection>()
        .hold("Jump", Vec3::X);
    app.update();
    assert!(action(&app, 1, "Jump").actuated(), "injection ignores assignment");
    assert_eq!(action(&app, 1, "Jump").phase, ActionPhase::Started);
    assert_eq!(action(&app, 0, "Jump").phase, ActionPhase::Ongoing);

    // The key is still down. Player 1 loses the injection and completes; player 0 stays held.
    app.world_mut().resource_mut::<InputInjection>().release("Jump");
    app.update();
    assert!(!action(&app, 1, "Jump").actuated());
    assert_eq!(action(&app, 1, "Jump").phase, ActionPhase::Completed);
    assert!(action(&app, 0, "Jump").actuated());
    assert_eq!(action(&app, 0, "Jump").phase, ActionPhase::Ongoing);

    let tied = InputConfig::new()
        .action("FromZoo", ActionKind::Button)
        .action("FromAaa", ActionKind::Button)
        .context(
            "Zoo",
            InputContext::consuming(10).with_binding(Binding::new("FromZoo", InputSource::key("Escape"))),
        )
        .context(
            "Aaa",
            InputContext::consuming(10).with_binding(Binding::new("FromAaa", InputSource::key("Escape"))),
        );
    let mut tied_app = input_app(tied);
    // No Gameplay context, so the plugin left an empty stack. Replace it after the plugin.
    let mut contexts = InputContexts::default();
    contexts.set(0, ["Zoo", "Aaa"]);
    tied_app.insert_resource(contexts);
    send_key(&mut tied_app, "Escape", ButtonState::Pressed);
    tied_app.update();
    let actions = tied_app.world().resource::<Actions>();
    assert!(actions.pressed("FromAaa"), "Aaa sorts before Zoo");
    assert_eq!(actions.phase("FromAaa"), ActionPhase::Started);
    assert!(!actions.pressed("FromZoo"));
    assert_eq!(actions.phase("FromZoo"), ActionPhase::Idle);
}

/// 0.5 is below the button threshold and still a live axis. Pressed outranks Down. A chord
/// with no partner stays idle.
#[test]
fn analog_threshold_combined_triggers_and_missing_chord() {
    let config = InputConfig::new()
        .action("StickY", ActionKind::Axis2D)
        .action("Face", ActionKind::Button)
        .action("Trigger", ActionKind::Axis1D)
        .action("Combo", ActionKind::Button)
        .action("Chorded", ActionKind::Button)
        .binding(Binding::new(
            "StickY",
            InputSource::gamepad_axis(GamepadAxis::LeftStickY),
        ))
        .binding(Binding::new(
            "Face",
            InputSource::gamepad_button(GamepadButton::South),
        ))
        .binding(Binding::new(
            "Trigger",
            InputSource::gamepad_button(GamepadButton::RightTrigger),
        ))
        .binding(
            Binding::new("Combo", InputSource::key("KeyC"))
                .with_trigger(Trigger::Pressed)
                .with_trigger(Trigger::Down),
        )
        .binding(
            Binding::new("Chorded", InputSource::key("KeyZ")).with_trigger(Trigger::Chord("Missing".into())),
        );
    let mut app = input_app(config);
    connect_pad(&mut app, 1);
    pad_axis(&mut app, 1, GamepadAxis::LeftStickY, 0.5);
    pad_button(&mut app, 1, GamepadButton::South, 0.5);
    pad_button(&mut app, 1, GamepadButton::RightTrigger, 0.5);
    send_key(&mut app, "KeyC", ButtonState::Pressed);
    send_key(&mut app, "KeyZ", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert_eq!(actions.axis2d("StickY"), Vec2::new(0.0, 0.5));
        assert!(!actions.pressed("Face"));
        assert_eq!(actions.phase("Face"), ActionPhase::Idle);
        assert_eq!(actions.axis1d("Trigger"), 0.5);
        assert!(actions.pressed("Combo") && actions.just_pressed("Combo"));
        assert_eq!(actions.phase("Combo"), ActionPhase::Triggered);
        assert!(!actions.pressed("Chorded"));
        assert_eq!(actions.phase("Chorded"), ActionPhase::Idle);
    }

    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Combo"));
        assert!(!actions.just_pressed("Combo"));
        assert_eq!(actions.phase("Combo"), ActionPhase::Ongoing);
        assert!(!actions.pressed("Chorded"));
        assert_eq!(actions.phase("Chorded"), ActionPhase::Idle);
    }

    send_key(&mut app, "KeyC", ButtonState::Released);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(!actions.pressed("Combo"));
        assert_eq!(actions.phase("Combo"), ActionPhase::Completed);
    }

    pad_button(&mut app, 1, GamepadButton::South, 0.75);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Face") && actions.just_pressed("Face"));
        assert_eq!(actions.phase("Face"), ActionPhase::Started);
        assert_eq!(actions.axis1d("Face"), 1.0);
    }
}

/// Split-screen: each player owns its own `Smooth` state, so one player's stick never pulls the
/// other's smoothed value (the state used to be keyed by context and binding index only).
#[test]
fn smooth_state_is_per_player() {
    let config = InputConfig::new().action("Throttle", ActionKind::Axis1D).binding(
        Binding::new("Throttle", InputSource::gamepad_axis(GamepadAxis::LeftStickX))
            .with_modifier(Modifier::Smooth(10.0)),
    );
    let mut app = input_app(config);
    {
        let mut players = app.world_mut().resource_mut::<PlayerInputs>();
        players.players.insert(0, DeviceAssignment::Gamepad(1));
        players.players.insert(1, DeviceAssignment::Gamepad(2));
    }
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(1, "Gameplay");
    connect_pad(&mut app, 1);
    connect_pad(&mut app, 2);
    pad_axis(&mut app, 1, GamepadAxis::LeftStickX, 1.0);
    app.update();
    app.update();
    assert_eq!(action(&app, 0, "Throttle").value.x, 1.0);
    assert_eq!(
        action(&app, 1, "Throttle").value.x,
        0.0,
        "player 1's idle stick is not dragged by player 0's smoother"
    );
}

/// Mouse motion and wheel without any click count as "the player used the mouse".
#[test]
fn mouse_motion_and_wheel_update_the_last_used_device() {
    let mut app = input_app(InputConfig::new());
    send_key(&mut app, "KeyW", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<LastUsedDevice>().0,
        Some(regia_input::InputDevice::Keyboard)
    );
    app.world_mut().write_message(regia_platform::MouseMotion {
        delta: (3.0, 0.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    assert_eq!(
        app.world().resource::<LastUsedDevice>().0,
        Some(regia_input::InputDevice::Mouse)
    );
    send_key(&mut app, "KeyW", ButtonState::Released);
    app.update();
    app.world_mut().write_message(regia_platform::MouseWheel {
        window: Entity::PLACEHOLDER,
        delta: (0.0, 1.0),
        unit: regia_platform::WheelUnit::Line,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    assert_eq!(
        app.world().resource::<LastUsedDevice>().0,
        Some(regia_input::InputDevice::Mouse)
    );
}

/// A player dropped while holding a button reads as released afterwards.
#[test]
fn removed_player_does_not_keep_a_held_action() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new(
            "Jump",
            InputSource::gamepad_button(GamepadButton::South),
        ));
    let mut app = input_app(config);
    app.world_mut()
        .resource_mut::<PlayerInputs>()
        .players
        .insert(2, DeviceAssignment::Gamepad(5));
    app.world_mut()
        .resource_mut::<InputContexts>()
        .push(2, "Gameplay");
    connect_pad(&mut app, 5);
    pad_button(&mut app, 5, GamepadButton::South, 1.0);
    app.update();
    assert!(action(&app, 2, "Jump").actuated());
    app.world_mut().resource_mut::<PlayerInputs>().players.remove(&2);
    app.world_mut().resource_mut::<InputContexts>().remove_player(2);
    app.update();
    assert!(
        !app.world()
            .resource::<Actions>()
            .player(2)
            .states
            .contains_key("Jump"),
        "the removed player's states are dropped"
    );
}

/// Audit finding 1 (ADR-4768): a press edge must be true for exactly one fixed step, whether the
/// frame runs a single 1/60 s step or three (a 50 ms hitch at 60 Hz fixed). Before the fix the
/// actions were evaluated once per frame in `PreUpdate`, so every fixed step of the hitch frame
/// saw `just_pressed` — a jump impulse applied three times.
#[test]
fn press_edge_fires_for_exactly_one_fixed_step_per_frame() {
    #[derive(Resource, Default)]
    struct Counts {
        jumps: u32,
    }

    fn counter(actions: Res<Actions>, mut counts: ResMut<Counts>) {
        if actions.just_pressed("Jump") {
            counts.jumps += 1;
        }
    }

    let config = || {
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .binding(Binding::new("Jump", InputSource::key("Space")).with_trigger(Trigger::Pressed))
    };

    // Baseline: one 1/60 s frame runs one fixed step.
    let mut steady = input_app(config());
    steady.init_resource::<Counts>();
    steady.add_systems(FixedUpdate, counter.after(InputSet::Evaluation));
    send_key(&mut steady, "Space", ButtonState::Pressed);
    steady.update();
    assert_eq!(
        steady.world().resource::<Counts>().jumps,
        1,
        "one step sees the edge"
    );
    steady.update();
    steady.update();
    assert_eq!(
        steady.world().resource::<Counts>().jumps,
        1,
        "no re-fire while held"
    );

    // The hitch: one 50 ms frame runs three fixed steps (default max_substeps is 5).
    let mut hitch = input_app(config());
    hitch.init_resource::<Counts>();
    hitch.add_systems(FixedUpdate, counter.after(InputSet::Evaluation));
    hitch.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(0.05)));
    send_key(&mut hitch, "Space", ButtonState::Pressed);
    hitch.update();
    assert_eq!(
        hitch.world().resource::<Counts>().jumps,
        1,
        "three steps in one frame still see the press edge exactly once"
    );
    hitch.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    hitch.update();
    assert_eq!(
        hitch.world().resource::<Counts>().jumps,
        1,
        "still held: no second edge on the next frame"
    );
}

/// Audit finding 1 (ADR-4768): hold timers advance per fixed step, not per presentation frame.
/// `Actions::elapsed` must read the same on the first steps after a press whether the 0.05 s
/// mark is reached by three steady 1/60 s frames or by one 50 ms frame's three steps.
#[test]
fn hold_timer_advances_per_fixed_step_not_per_frame() {
    #[derive(Resource, Default)]
    struct Trace {
        steps: u32,
        past_threshold: u32,
    }

    fn trace(actions: Res<Actions>, mut t: ResMut<Trace>) {
        if t.steps < 4 && actions.elapsed("Charge") >= 0.05 {
            t.past_threshold += 1;
        }
        t.steps += 1;
    }

    let config = || {
        InputConfig::new()
            .action("Charge", ActionKind::Button)
            .binding(Binding::new("Charge", InputSource::key("Space")))
    };

    let mut steady = input_app(config());
    steady.init_resource::<Trace>();
    steady.add_systems(FixedUpdate, trace.after(InputSet::Evaluation));
    send_key(&mut steady, "Space", ButtonState::Pressed);
    for _ in 0..4 {
        steady.update();
    }
    let steady_count = steady.world().resource::<Trace>().past_threshold;

    let mut hitch = input_app(config());
    hitch.init_resource::<Trace>();
    hitch.add_systems(FixedUpdate, trace.after(InputSet::Evaluation));
    hitch.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(0.05)));
    send_key(&mut hitch, "Space", ButtonState::Pressed);
    hitch.update();
    hitch.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    for _ in 0..3 {
        hitch.update();
    }
    let hitch_count = hitch.world().resource::<Trace>().past_threshold;

    assert_eq!(
        steady_count, hitch_count,
        "elapsed crosses 0.05 s on the same fixed steps (steady {steady_count} vs hitch {hitch_count})"
    );
}

/// A press that lands on a frame whose fixed loop banks (a 16 Hz display feeding an 8 Hz
/// fixed loop: bank, step, bank, step — the 144 Hz-display/60 Hz-fixed pattern) must not be
/// consumed by the zero-step-frame fallback: the frame-level readers see the edge at once,
/// and the first later fixed step still sees it exactly once (fixrev review of the per-step
/// evaluation, finding 1). Frame durations are exact binary fractions of the step, so the
/// banking pattern is nanosecond-exact.
#[test]
fn zero_step_frame_press_reaches_the_next_fixed_step() {
    #[derive(Resource, Default)]
    struct Counts {
        jumps: u32,
    }

    fn counter(actions: Res<Actions>, mut counts: ResMut<Counts>) {
        if actions.just_pressed("Jump") {
            counts.jumps += 1;
        }
    }

    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")).with_trigger(Trigger::Pressed));
    let mut app = input_app(config);
    app.init_resource::<Counts>();
    app.add_systems(FixedUpdate, counter.after(InputSet::Evaluation));
    enable_fixed_step(&mut app, 8.0);
    // Half-step frames: every other frame banks without running a step.
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(62_500)));

    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert_eq!(
        app.world().resource::<Counts>().jumps,
        0,
        "the press frame banks: no fixed step runs on it"
    );
    assert!(
        app.world().resource::<Actions>().just_pressed_this_frame("Jump"),
        "the fallback gives the frame-level readers the edge at once"
    );
    app.update();
    assert_eq!(
        app.world().resource::<Counts>().jumps,
        1,
        "the banked frame's fixed step still sees the press"
    );
    app.update();
    assert_eq!(
        app.world().resource::<Counts>().jumps,
        1,
        "the next banked frame runs no step and adds nothing"
    );
    assert!(
        !app.world().resource::<Actions>().just_pressed_this_frame("Jump"),
        "the held press is not re-presented on a later banked frame"
    );
    app.update();
    assert_eq!(
        app.world().resource::<Counts>().jumps,
        1,
        "the key is held: the stepped frame does not re-fire"
    );
}

/// A press and a release inside one frame's batch read as actuated for the rest of that
/// frame: the device keeps the edge until the next frame's batch, so every fixed step of a
/// multi-step frame sees the click (the documented duration consequence of the batch-edge
/// rule; fixrev review, finding 7).
#[test]
fn batch_click_reads_held_for_every_step_of_the_frame() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")));
    let mut app = input_app(config);
    #[derive(Resource, Default)]
    struct Trace(Vec<bool>);
    app.init_resource::<Trace>();
    app.add_systems(
        FixedUpdate,
        (|actions: Res<Actions>, mut t: ResMut<Trace>| {
            t.0.push(actions.pressed("Jump"));
        })
        .after(InputSet::Evaluation),
    );
    // An 8 Hz fixed loop and a three-step hitch frame (375 ms = 3 × 125 ms exactly).
    let mut fixed = FixedTime::default();
    fixed.hz = 8.0;
    app.insert_resource(fixed);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(375)));
    send_key(&mut app, "Space", ButtonState::Pressed);
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    let trace = &app.world().resource::<Trace>().0;
    assert_eq!(trace.len(), 3, "the hitch frame runs three steps");
    assert!(
        trace.iter().all(|&held| held),
        "the batch click reads as held for every step of the frame: {trace:?}"
    );
}

/// With the simulation clock halted (paused), the fixed loop runs zero steps: the `PreUpdate`
/// fallback still evaluates actions with a zero delta, so UI reading actions keeps working and
/// edges refresh exactly once.
#[test]
fn edges_refresh_while_the_sim_clock_is_halted() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")));
    let mut app = input_app(config);
    app.world_mut().resource_mut::<Time>().paused = true;

    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(
        app.world().resource::<Actions>().just_pressed("Jump"),
        "the halted-clock fallback evaluates the press"
    );
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(
        actions.pressed("Jump") && !actions.just_pressed("Jump"),
        "held but not re-fired while paused"
    );
    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    assert!(
        !app.world().resource::<Actions>().pressed("Jump"),
        "the release lands too"
    );
    assert_eq!(
        app.world().resource::<Actions>().elapsed("Jump"),
        0.0,
        "a zero delta never advances the hold timer"
    );
}

/// A multi-step frame applies the frame's mouse motion on one fixed step only (its last, ADR-5599):
/// the raw delta is per frame, and the earlier steps of the frame evaluate without it.
/// The motion is not repeated on the later steps of the frame (that would triple the look).
#[test]
fn mouse_delta_applies_once_across_a_multi_step_frame() {
    #[derive(Resource, Default)]
    struct Accumulated(Vec2);

    fn accumulate(actions: Res<Actions>, mut sum: ResMut<Accumulated>) {
        sum.0 += actions.axis2d("Look");
    }

    let config = InputConfig::new()
        .action("Look", ActionKind::Axis2D)
        .binding(Binding::new("Look", InputSource::MouseDelta));
    let mut app = input_app(config);
    app.init_resource::<Accumulated>();
    app.add_systems(FixedUpdate, accumulate.after(InputSet::Evaluation));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(0.05)));
    app.world_mut().write_message(regia_platform::MouseMotion {
        delta: (10.0, -5.0),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
    app.update();
    assert_eq!(
        app.world().resource::<Accumulated>().0,
        Vec2::new(10.0, -5.0),
        "three steps share one frame of motion: the frame total is one motion, not three"
    );
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.update();
    assert_eq!(
        app.world().resource::<Accumulated>().0,
        Vec2::new(10.0, -5.0),
        "no new motion: nothing more applied"
    );
}

/// Presentation-side readers (`Update` systems, quicksave, agent polling) run once per frame
/// and must see the press edge even when a multi-step frame consumed the step-level phase on
/// its first step: `just_pressed_this_frame` is the frame-level view.
#[test]
fn frame_level_edge_view_survives_a_multi_step_frame() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")).with_trigger(Trigger::Pressed));
    let mut app = input_app(config);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(0.05)));
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(
        !actions.just_pressed("Jump"),
        "the step-level phase was consumed by the first of the frame's three steps"
    );
    assert!(
        actions.just_pressed_this_frame("Jump"),
        "the frame-level view still reports the press"
    );
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(
        !actions.just_pressed_this_frame("Jump"),
        "a new frame clears the frame-level edge"
    );
}

/// An explicit trigger on one binding must not replace another binding's default
/// `Down`, and that `Pressed` binding keeps its own edge.
///
/// Hold Space (no triggers, so `Down`). Fire stays active for the whole hold.
/// Press KeyF (`Pressed`) while Space is already down: that press still fires.
#[test]
fn empty_trigger_binding_stays_down_beside_an_explicit_pressed_binding() {
    let config = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::key("KeyF")).with_trigger(Trigger::Pressed))
        .binding(Binding::new("Fire", InputSource::key("Space")));
    let mut app = input_app(config);

    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.pressed("Fire") && actions.just_pressed("Fire"),
            "the default Down binding starts the action"
        );
    }
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.pressed("Fire") && !actions.just_pressed("Fire"),
            "Space is still held, so Fire stays down"
        );
        assert_eq!(actions.phase("Fire"), ActionPhase::Ongoing);
    }

    send_key(&mut app, "KeyF", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(
            actions.pressed("Fire") && actions.just_pressed("Fire"),
            "KeyF's Pressed trigger fires even though Space is already down"
        );
    }
}

/// Two default `Down` bindings (the spec's Jump shape) are one action. The second
/// key going down while the first is held does not emit another press edge.
#[test]
fn a_second_down_binding_does_not_restart_a_held_action() {
    let config = InputConfig::new()
        .action("Jump", ActionKind::Button)
        .binding(Binding::new("Jump", InputSource::key("Space")))
        .binding(Binding::new("Jump", InputSource::key("KeyJ")));
    let mut app = input_app(config);
    send_key(&mut app, "Space", ButtonState::Pressed);
    app.update();
    assert!(app.world().resource::<Actions>().just_pressed("Jump"));
    app.update();
    send_key(&mut app, "KeyJ", ButtonState::Pressed);
    app.update();
    {
        let actions = app.world().resource::<Actions>();
        assert!(actions.pressed("Jump"));
        assert!(
            !actions.just_pressed("Jump"),
            "the action was already down; the second key does not start it again"
        );
        assert_eq!(actions.phase("Jump"), ActionPhase::Ongoing);
    }

    send_key(&mut app, "Space", ButtonState::Released);
    app.update();
    let actions = app.world().resource::<Actions>();
    assert!(
        actions.pressed("Jump") && !actions.just_pressed("Jump"),
        "KeyJ is still held, so releasing Space does not end Jump"
    );
    assert_eq!(actions.phase("Jump"), ActionPhase::Ongoing);
}

/// integ-v048c (merge of corr-input-world's per-binding `Hold`/`Tap` timers with the
/// zero-step-frame fallback): a binding's own hold time is step state, so a release that only
/// a banked frame sees (the key is down again before the next fixed step) must not restart it.
/// The fixed steps never see that release, so the `Hold` fires on the same step as for a key
/// held without a break. Half-step frames at 8 Hz: frame 0 banks, frame 1 steps, and so on.
#[test]
fn a_release_seen_only_by_a_banked_frame_keeps_the_binding_hold_timer() {
    #[derive(Resource, Default)]
    struct Steps {
        count: u32,
        fired_on: Option<u32>,
    }

    fn record(actions: Res<Actions>, mut steps: ResMut<Steps>) {
        steps.count += 1;
        if steps.fired_on.is_none() && actions.triggered("Charge") {
            steps.fired_on = Some(steps.count);
        }
    }

    let run = |blip: bool| {
        let config = InputConfig::new()
            .action("Charge", ActionKind::Button)
            .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(0.5)));
        let mut app = input_app(config);
        app.init_resource::<Steps>();
        app.add_systems(FixedUpdate, record.after(InputSet::Evaluation));
        enable_fixed_step(&mut app, 8.0);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(62_500)));
        send_key(&mut app, "KeyA", ButtonState::Pressed);
        for frame in 0..24 {
            if blip && frame == 2 {
                send_key(&mut app, "KeyA", ButtonState::Released);
            }
            if blip && frame == 3 {
                send_key(&mut app, "KeyA", ButtonState::Pressed);
            }
            app.update();
        }
        app.world().resource::<Steps>().fired_on
    };
    let steady = run(false);
    assert!(steady.is_some(), "Hold(0.5) fires within twelve 8 Hz steps");
    assert_eq!(
        run(true),
        steady,
        "a release only the zero-step fallback saw must not restart the binding's hold time"
    );
}

/// integ-v048c (corr-input-world review, finding 1): a `Tap` pressed on a stepped frame and
/// released on a banked (zero-step) frame must still `Trigger` on the next fixed step. The
/// fallback runs the tap for the frame-level readers, but the per-binding tap window is step
/// state: if the fallback persisted `binding_tap_open = false`, the next fixed step would read
/// the release edge with the window already closed and the simulation would never see the tap.
/// Half-step frames at 8 Hz: frame 0 banks, frame 1 steps, frame 2 banks, frame 3 steps.
#[test]
fn a_tap_released_on_a_banked_frame_triggers_on_the_next_fixed_step() {
    #[derive(Resource, Default)]
    struct Steps {
        count: u32,
        tap_steps: Vec<u32>,
    }

    fn record(actions: Res<Actions>, mut steps: ResMut<Steps>) {
        steps.count += 1;
        let count = steps.count;
        if actions.triggered("Use") {
            steps.tap_steps.push(count);
        }
    }

    let config = InputConfig::new()
        .action("Use", ActionKind::Button)
        .binding(Binding::new("Use", InputSource::key("KeyA")).with_trigger(Trigger::Tap(0.5)));
    let mut app = input_app(config);
    app.init_resource::<Steps>();
    app.add_systems(FixedUpdate, record.after(InputSet::Evaluation));
    enable_fixed_step(&mut app, 8.0);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(62_500)));

    send_key(&mut app, "KeyA", ButtonState::Pressed);
    app.update(); // frame 0 banks: the fallback presents the press to the frame-level readers
    app.update(); // frame 1 steps: the press edge opens the tap window in step state
    send_key(&mut app, "KeyA", ButtonState::Released);
    app.update(); // frame 2 banks: the fallback runs the tap for the frame-level readers
    assert_eq!(
        app.world().resource::<Actions>().phase("Use"),
        ActionPhase::Triggered,
        "the frame-level readers see the tap on the banked release frame"
    );
    app.update(); // frame 3 steps: the release edge must still find the window open
    assert_eq!(
        app.world().resource::<Steps>().tap_steps,
        vec![2],
        "the fixed step sees the tap exactly once"
    );
}

/// `Hold` times the binding that lists it. A sibling `Down` key that has already been
/// held past the threshold must not make the `Hold` binding fire on its first frame.
#[test]
fn a_held_sibling_does_not_complete_another_bindings_hold() {
    let config = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(0.5)))
        .binding(Binding::new("Charge", InputSource::key("KeyB")));
    let mut app = input_app(config);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    for _ in 0..40 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Ongoing,
        "KeyB is a default Down binding"
    );
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    app.update();
    assert_ne!(
        app.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Triggered,
        "KeyB's hold time must not satisfy KeyA's Hold"
    );
}

/// Two `Tap` bindings are alternatives. An idle sibling must not close the other's window.
#[test]
fn an_idle_sibling_tap_does_not_cancel_a_quick_tap() {
    let config = InputConfig::new()
        .action("Dodge", ActionKind::Button)
        .binding(Binding::new("Dodge", InputSource::key("KeyB")).with_trigger(Trigger::Tap(0.2)))
        .binding(Binding::new("Dodge", InputSource::key("KeyA")).with_trigger(Trigger::Tap(0.2)));
    let mut app = input_app(config);
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    app.update();
    app.update();
    send_key(&mut app, "KeyA", ButtonState::Released);
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Dodge"),
        ActionPhase::Triggered,
        "a quick tap on KeyA fires even though KeyB's Tap is idle"
    );
}

/// Releasing one `Hold` binding must not clear the other's fired bit and make it fire again.
#[test]
fn releasing_a_second_hold_binding_does_not_rearm_the_first() {
    let config = InputConfig::new()
        .action("Charge", ActionKind::Button)
        .binding(Binding::new("Charge", InputSource::key("KeyA")).with_trigger(Trigger::Hold(0.25)))
        .binding(Binding::new("Charge", InputSource::key("KeyB")).with_trigger(Trigger::Hold(0.25)));
    let mut app = input_app(config);
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    let mut fired = false;
    for _ in 0..30 {
        app.update();
        if app.world().resource::<Actions>().triggered("Charge") {
            fired = true;
            break;
        }
    }
    assert!(fired, "KeyA's hold should fire");
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Ongoing
    );
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    app.update();
    send_key(&mut app, "KeyB", ButtonState::Released);
    app.update();
    app.update();
    assert_eq!(
        app.world().resource::<Actions>().phase("Charge"),
        ActionPhase::Ongoing,
        "KeyB releasing must not clear KeyA's hold and fire it again"
    );
}

/// Two held `Pulse` bindings are one action. Each advances its own interval, so holding
/// both does not fire twice as often as holding one.
#[test]
fn two_held_pulse_bindings_do_not_double_the_rate() {
    let config = InputConfig::new()
        .action("Fire", ActionKind::Button)
        .binding(Binding::new("Fire", InputSource::key("KeyA")).with_trigger(Trigger::Pulse(0.5)))
        .binding(Binding::new("Fire", InputSource::key("KeyB")).with_trigger(Trigger::Pulse(0.5)));
    let mut app = input_app(config);
    send_key(&mut app, "KeyA", ButtonState::Pressed);
    send_key(&mut app, "KeyB", ButtonState::Pressed);
    let mut fired = 0;
    for _ in 0..20 {
        app.update();
        if app.world().resource::<Actions>().triggered("Fire") {
            fired += 1;
        }
    }
    assert_eq!(
        fired, 1,
        "20 frames at 60 Hz stays inside one 0.5 s interval after the press; fired {fired}"
    );
}
