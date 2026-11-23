// SPDX-License-Identifier: MPL-2.0
//! Tests written from the mutation run of `regia_input` (ADR-3389): each one pins a behavior a mutant broke
//! while the older tests still passed. Frame times are exact binary fractions so thresholds land on known
//! frames.
//!
//! Evaluation is per fixed step (ADR-4768): every `frame()` runs exactly one fixed step of the
//! frame's delta, so one `frame()` call is one evaluation with that delta — the arithmetic the
//! comments describe.

use std::time::Duration;

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_platform::{ButtonState, KeyboardInput};

const DT: f32 = 0.125;

fn app(config: InputConfig) -> App {
    let mut app = App::new();
    // One fixed step per update at the frame's delta: action evaluation advances one fixed step at
    // a time (ADR-4768), and these thresholds are written per evaluation.
    regia_app::enable_fixed_step(&mut app, 1.0 / f64::from(DT));
    app.add_plugins(InputPlugin::new(config));
    // The first update has no delta; run it before the test's frames.
    app.update();
    app
}

fn one(action: &str, trigger: Trigger) -> InputConfig {
    InputConfig::new()
        .action(action, ActionKind::Button)
        .binding(Binding::new(action, InputSource::key("Space")).with_trigger(trigger))
}

fn key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_string(),
        logical_key: name.to_string(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn frame(app: &mut App, dt: f32, action: &str) -> ActionPhase {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(dt)));
    // Keep exactly one fixed step of length `dt` in this frame (the deltas are binary fractions).
    app.world_mut().resource_mut::<FixedTime>().hz = 1.0 / f64::from(dt);
    app.update();
    app.world().resource::<Actions>().phase(action)
}

#[test]
fn released_fires_only_on_the_release_frame() {
    let mut app = app(one("Shoot", Trigger::Released));
    assert_eq!(
        frame(&mut app, DT, "Shoot"),
        ActionPhase::Idle,
        "idle frames do not fire"
    );
    key(&mut app, "Space", ButtonState::Pressed);
    assert_eq!(frame(&mut app, DT, "Shoot"), ActionPhase::Idle);
    assert_eq!(
        frame(&mut app, DT, "Shoot"),
        ActionPhase::Idle,
        "held frames do not fire"
    );
    key(&mut app, "Space", ButtonState::Released);
    assert_eq!(frame(&mut app, DT, "Shoot"), ActionPhase::Triggered);
    assert_eq!(frame(&mut app, DT, "Shoot"), ActionPhase::Idle);
}

#[test]
fn tap_starts_once_and_its_window_includes_the_limit() {
    // Tap(0.25) at 1/8 s frames: elapsed is 0, 0.125, 0.25 on the three held frames; 0.25 is still
    // inside the window, so the release on the fourth frame taps.
    let mut app = app(one("Dodge", Trigger::Tap(0.25)));
    key(&mut app, "Space", ButtonState::Pressed);
    assert_eq!(frame(&mut app, DT, "Dodge"), ActionPhase::Started);
    assert_eq!(frame(&mut app, DT, "Dodge"), ActionPhase::Ongoing);
    assert_eq!(frame(&mut app, DT, "Dodge"), ActionPhase::Ongoing);
    key(&mut app, "Space", ButtonState::Released);
    assert_eq!(frame(&mut app, DT, "Dodge"), ActionPhase::Triggered);
}

#[test]
fn pulse_carries_lateness_for_an_exact_average_rate() {
    // Interval 3/16 s at 1/8 s frames: after the press, 6 frames cover 0.75 s = 4 intervals.
    let mut app = app(one("Fire", Trigger::Pulse(0.1875)));
    key(&mut app, "Space", ButtonState::Pressed);
    let fired: String = (0..7)
        .map(|_| {
            if frame(&mut app, DT, "Fire") == ActionPhase::Triggered {
                '1'
            } else {
                '0'
            }
        })
        .collect();
    assert_eq!(fired, "1011011");
}

#[test]
fn pulse_lateness_of_exactly_one_interval_restarts_a_full_interval() {
    let mut app = app(one("Fire", Trigger::Pulse(0.125)));
    key(&mut app, "Space", ButtonState::Pressed);
    assert_eq!(frame(&mut app, DT, "Fire"), ActionPhase::Triggered);
    assert_eq!(frame(&mut app, 0.0625, "Fire"), ActionPhase::Ongoing);
    // 0.0625 - 0.1875 = -0.125: one whole interval late. It fires once and waits a full interval.
    assert_eq!(frame(&mut app, 0.1875, "Fire"), ActionPhase::Triggered);
    assert_eq!(frame(&mut app, 0.0625, "Fire"), ActionPhase::Ongoing);
    assert_eq!(frame(&mut app, 0.0625, "Fire"), ActionPhase::Triggered);
}

/// The same at a constant fixed step (fixrev-12-k): lateness smaller than one interval
/// is carried into the next interval (the fire after an early fire comes sooner), while a step
/// a whole interval late fires exactly once and restarts a full interval — never a burst.
#[test]
fn pulse_lateness_carries_or_restarts_per_step() {
    // Carry: interval 3/16 s at 2/16 s steps. The third frame's step overshoots by 1/16 (< one
    // interval): it fires once and carries (timer restarts at 2/16, not 3/16), so the fires
    // come in pairs — 6 fires over 9 frames (every 1.5 steps), not every two.
    let mut carry = app(one("Fire", Trigger::Pulse(0.1875)));
    key(&mut carry, "Space", ButtonState::Pressed);
    let mut fired = String::new();
    for _ in 0..9 {
        fired.push(if frame(&mut carry, DT, "Fire") == ActionPhase::Triggered {
            '1'
        } else {
            '0'
        });
    }
    assert_eq!(fired, "101101101", "the 1/16 s lateness carries: {fired}");

    // A whole interval late: interval 1/8 s at 1/4 s steps. Every step overshoots by exactly
    // one interval: each fires exactly once and waits a full interval (the timer restarts at
    // `interval`, never below it — a burst within one step is the mutant being killed).
    let mut late = stepped(4.0, one("Fire", Trigger::Pulse(0.125)));
    key(&mut late, "Space", ButtonState::Pressed);
    let mut fired = String::new();
    for _ in 0..4 {
        fired.push(if frame(&mut late, 0.25, "Fire") == ActionPhase::Triggered {
            '1'
        } else {
            '0'
        });
    }
    assert_eq!(fired, "1111", "every late step fires exactly once: {fired}");
}

/// An app whose updates run exactly one fixed step of `1/hz` seconds (evaluation is per fixed
/// step, so `frame()` durations below the step run nothing).
fn stepped(hz: f64, config: InputConfig) -> App {
    let mut app = App::new();
    app.add_plugins(InputPlugin::new(config));
    enable_fixed_step(&mut app, hz);
    app.update();
    app
}

#[test]
fn a_chord_starts_once_then_stays_ongoing() {
    let config = InputConfig::new()
        .action("Ctrl", ActionKind::Button)
        .action("Save", ActionKind::Button)
        .binding(Binding::new("Ctrl", InputSource::key("ControlLeft")))
        .binding(Binding::new("Save", InputSource::key("KeyS")).with_trigger(Trigger::Chord("Ctrl".into())));
    let mut app = app(config);
    key(&mut app, "ControlLeft", ButtonState::Pressed);
    assert_eq!(frame(&mut app, DT, "Save"), ActionPhase::Idle);
    key(&mut app, "KeyS", ButtonState::Pressed);
    assert_eq!(frame(&mut app, DT, "Save"), ActionPhase::Started);
    assert_eq!(frame(&mut app, DT, "Save"), ActionPhase::Ongoing);
    assert_eq!(frame(&mut app, DT, "Save"), ActionPhase::Ongoing);
    key(&mut app, "KeyS", ButtonState::Released);
    assert_eq!(frame(&mut app, DT, "Save"), ActionPhase::Completed);
}

fn crouch(mode: PressMode) -> App {
    let mut app = app(InputConfig::new()
        .action("Crouch", ActionKind::Button)
        .action("Jump", ActionKind::Axis1D)
        .binding(Binding::new("Crouch", InputSource::key("KeyC"))));
    app.world_mut().resource_mut::<InputAssists>().set(
        "Crouch",
        ActionAssist {
            mode,
            ..Default::default()
        },
    );
    app
}

#[test]
fn actions_report_their_kind_and_toggle_state() {
    let mut app = crouch(PressMode::Hold);
    let actions = app.world().resource::<Actions>();
    assert_eq!(actions.kind("Crouch"), Some(ActionKind::Button));
    assert_eq!(actions.kind("Jump"), Some(ActionKind::Axis1D));
    assert_eq!(actions.kind("Nope"), None);
    key(&mut app, "KeyC", ButtonState::Pressed);
    frame(&mut app, DT, "Crouch");
    let state = app.world().resource::<Actions>().player(0).states["Crouch"];
    assert!(state.actuated());
    assert!(
        !state.toggled_on(),
        "a held action without the toggle assist is not latched"
    );
}

#[test]
fn release_all_toggles_drops_every_latch() {
    let mut app = crouch(PressMode::Toggle);
    key(&mut app, "KeyC", ButtonState::Pressed);
    frame(&mut app, DT, "Crouch");
    key(&mut app, "KeyC", ButtonState::Released);
    frame(&mut app, DT, "Crouch");
    assert!(app.world().resource::<Actions>().player(0).states["Crouch"].toggled_on());
    app.world_mut().resource_mut::<Actions>().release_all_toggles();
    assert!(!app.world().resource::<Actions>().player(0).states["Crouch"].toggled_on());
    frame(&mut app, DT, "Crouch");
    assert!(
        !app.world().resource::<Actions>().pressed("Crouch"),
        "the latch is gone on the next frame"
    );
}

#[test]
fn press_modes_have_stable_names_that_parse_back() {
    assert_eq!(PressMode::Hold.as_str(), "hold");
    assert_eq!(PressMode::Toggle.as_str(), "toggle");
    for mode in [PressMode::Hold, PressMode::Toggle] {
        assert_eq!(PressMode::parse(mode.as_str()), Some(mode));
    }
    assert_eq!(PressMode::parse("HOLD"), Some(PressMode::Hold));
    assert_eq!(PressMode::parse("latch"), None);
}

#[test]
fn replace_swaps_every_assist_and_drops_neutral_ones() {
    let mut assists = InputAssists::default();
    assists.set(
        "Old",
        ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        },
    );
    let mut next = std::collections::BTreeMap::new();
    next.insert(
        "Crouch".to_string(),
        ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        },
    );
    next.insert("Neutral".to_string(), ActionAssist::default());
    assists.replace(&next);
    assert_eq!(assists.get("Old"), None, "replace clears what was there");
    assert_eq!(assists.get("Crouch").map(|a| a.mode), Some(PressMode::Toggle));
    assert_eq!(assists.get("Neutral"), None, "a neutral assist is not stored");
    assert_eq!(assists.actions.len(), 1);
}

fn repeating(delay_secs: f32, interval_secs: f32) -> App {
    let mut app = app(InputConfig::new()
        .action("Next", ActionKind::Button)
        .binding(Binding::new("Next", InputSource::key("KeyN"))));
    app.world_mut().resource_mut::<InputAssists>().set(
        "Next",
        ActionAssist {
            repeat: Some(RepeatRate {
                delay_secs,
                interval_secs,
            }),
            ..Default::default()
        },
    );
    key(&mut app, "KeyN", ButtonState::Pressed);
    app
}

#[test]
fn repeat_carries_lateness_for_an_even_rate() {
    // Delay 1/4 s, interval 3/16 s, 1/8 s frames: repeats on frames 3, 5, 6 and 8.
    let mut app = repeating(0.25, 0.1875);
    let fired: String = (0..8)
        .map(|_| {
            if frame(&mut app, DT, "Next") == ActionPhase::Triggered {
                '1'
            } else {
                '0'
            }
        })
        .collect();
    assert_eq!(fired, "00101101");
}

#[test]
fn repeat_lateness_of_exactly_one_interval_restarts_a_full_interval() {
    let mut app = repeating(0.125, 0.125);
    assert_eq!(frame(&mut app, DT, "Next"), ActionPhase::Started);
    assert_eq!(frame(&mut app, 0.0625, "Next"), ActionPhase::Ongoing);
    // 0.0625 - 0.1875 = -0.125: one whole interval late.
    assert_eq!(frame(&mut app, 0.1875, "Next"), ActionPhase::Triggered);
    assert_eq!(frame(&mut app, 0.0625, "Next"), ActionPhase::Ongoing);
    assert_eq!(frame(&mut app, 0.0625, "Next"), ActionPhase::Triggered);
}

/// The same at a constant fixed step (fixrev-12-k): repeat lateness smaller than one
/// interval is carried (the next repeat comes sooner), while a step a whole interval late
/// repeats exactly once and restarts a full interval — never a burst.
#[test]
fn repeat_lateness_carries_or_restarts_per_step() {
    // Carry: delay 1/8 s, interval 3/16 s at 2/16 s steps. The delay ends exactly on the
    // second frame; the fourth frame's step overshoots the interval by 1/16 and carries, so
    // repeats come every 1.5 steps.
    let mut carry = repeating(0.125, 0.1875);
    let mut phases = Vec::new();
    for _ in 0..7 {
        phases.push(frame(&mut carry, DT, "Next"));
    }
    assert_eq!(
        phases,
        [
            ActionPhase::Started,
            ActionPhase::Triggered,
            ActionPhase::Ongoing,
            ActionPhase::Triggered,
            ActionPhase::Triggered,
            ActionPhase::Ongoing,
            ActionPhase::Triggered,
        ],
        "the 1/16 s lateness carries into the next interval"
    );

    // A whole interval late: delay and interval 1/8 s at 1/4 s steps. Every step overshoots
    // by exactly one interval: each repeats exactly once and waits a full interval.
    let mut late = stepped(
        4.0,
        InputConfig::new()
            .action("Next", ActionKind::Button)
            .binding(Binding::new("Next", InputSource::key("KeyN"))),
    );
    late.world_mut().resource_mut::<InputAssists>().set(
        "Next",
        ActionAssist {
            repeat: Some(RepeatRate {
                delay_secs: 0.125,
                interval_secs: 0.125,
            }),
            ..Default::default()
        },
    );
    key(&mut late, "KeyN", ButtonState::Pressed);
    let mut phases = Vec::new();
    for _ in 0..4 {
        phases.push(frame(&mut late, 0.25, "Next"));
    }
    assert_eq!(
        phases,
        [
            ActionPhase::Started,
            ActionPhase::Triggered,
            ActionPhase::Triggered,
            ActionPhase::Triggered,
        ],
        "every late step repeats exactly once"
    );
}

#[test]
fn a_button_is_on_whatever_sign_or_scale_its_bindings_give() {
    let negate = Modifier::Negate {
        x: true,
        y: false,
        z: false,
    };
    let config = InputConfig::new()
        .action("Use", ActionKind::Button)
        .binding(Binding::new("Use", InputSource::key("KeyE")))
        .binding(Binding::new("Use", InputSource::key("KeyF")).with_modifier(negate));
    let mut app = app(config);
    // The negated binding alone: a button is 0 or 1, never -1.
    key(&mut app, "KeyF", ButtonState::Pressed);
    frame(&mut app, DT, "Use");
    assert_eq!(app.world().resource::<Actions>().axis1d("Use"), 1.0);
    // Both at once: buttons OR, they do not add up to zero.
    key(&mut app, "KeyE", ButtonState::Pressed);
    frame(&mut app, DT, "Use");
    let actions = app.world().resource::<Actions>();
    assert!(actions.pressed("Use"));
    assert_eq!(actions.axis1d("Use"), 1.0);
}
