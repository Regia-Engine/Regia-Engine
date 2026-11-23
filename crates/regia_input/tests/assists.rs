// SPDX-License-Identifier: MPL-2.0
//! Player input assists end to end (ADR-2646, `book/agents/input-assists.md`): hold versus toggle,
//! a minimum hold time and auto-repeat, driven by synthetic keyboard event streams through
//! `InputPlugin` with fixed frame deltas.

use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_platform::{ButtonState, KeyboardInput};

fn app_with(assist: Option<ActionAssist>) -> App {
    let mut app = App::new();
    // One fixed step per update at the same 1/8 s: the streams below are written in frames,
    // and action evaluation advances one fixed step at a time (ADR-4768).
    regia_app::enable_fixed_step(&mut app, 8.0);
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Crouch", ActionKind::Button)
            .binding(Binding::new("Crouch", InputSource::key("KeyC"))),
    ));
    if let Some(assist) = assist {
        app.world_mut()
            .resource_mut::<InputAssists>()
            .set("Crouch", assist);
    }
    // The first update has no delta; run it before the stream starts.
    app.update();
    app
}

fn key(app: &mut App, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: "KeyC".to_string(),
        logical_key: "KeyC".to_string(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

/// Plays a stream of key levels (`1` held, `0` up), one frame each, sending only the edges.
/// Returns per frame (pressed, just_pressed).
fn play(app: &mut App, stream: &str) -> Vec<(bool, bool)> {
    let mut down = false;
    stream
        .chars()
        .map(|c| {
            let want = c == '1';
            if want != down {
                key(
                    app,
                    if want {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                );
                down = want;
            }
            app.update();
            let actions = app.world().resource::<Actions>();
            (actions.pressed("Crouch"), actions.just_pressed("Crouch"))
        })
        .collect()
}

fn pressed(frames: &[(bool, bool)]) -> String {
    frames.iter().map(|f| if f.0 { '1' } else { '0' }).collect()
}

fn fired(frames: &[(bool, bool)]) -> String {
    frames.iter().map(|f| if f.1 { '1' } else { '0' }).collect()
}

const STREAM: &str = "0110001110000100";

#[test]
fn without_an_assist_the_action_follows_the_key() {
    let mut plain = app_with(None);
    let seen = play(&mut plain, STREAM);
    assert_eq!(pressed(&seen), STREAM);
    // The neutral assist is the same stream, frame for frame.
    let mut neutral = app_with(Some(ActionAssist::default()));
    assert_eq!(play(&mut neutral, STREAM), seen);
}

#[test]
fn toggle_mode_latches_on_a_press_and_releases_on_the_next() {
    let mut app = app_with(Some(ActionAssist {
        mode: PressMode::Toggle,
        ..Default::default()
    }));
    let seen = play(&mut app, STREAM);
    // On at the first press, held through the gap, off at the second press, on at the third.
    assert_eq!(pressed(&seen), "0111110000000111");
    assert_eq!(fired(&seen), "0100000000000100");
    let state = app.world().resource::<Actions>().player(0).states["Crouch"];
    assert!(state.toggled_on());
    // A game can stand the player up (a cutscene): the latch drops on the next frame.
    assert!(
        app.world_mut()
            .resource_mut::<Actions>()
            .release_toggle(0, "Crouch")
    );
    let after = play(&mut app, "00");
    assert_eq!(pressed(&after), "00");
}

#[test]
fn a_minimum_hold_ignores_bumps() {
    let mut app = app_with(Some(ActionAssist {
        min_hold_secs: 0.25,
        ..Default::default()
    }));
    // A one-frame and a two-frame press are dropped; a four-frame press counts from its third.
    let seen = play(&mut app, "01001100111100");
    assert_eq!(pressed(&seen), "00000000001100");
    assert_eq!(fired(&seen), "00000000001000");
}

#[test]
fn repeat_turns_one_long_hold_into_regular_presses() {
    let mut app = app_with(Some(ActionAssist {
        repeat: Some(RepeatRate {
            delay_secs: 0.25,
            interval_secs: 0.25,
        }),
        ..Default::default()
    }));
    let seen = play(&mut app, "0111111100");
    // The press, then a repeat every two frames while held.
    assert_eq!(fired(&seen), "0101010100");
    assert_eq!(pressed(&seen), "0111111100");
}

#[test]
fn the_same_stream_gives_the_same_phases_every_run() {
    let assist = ActionAssist {
        mode: PressMode::Toggle,
        min_hold_secs: 0.125,
        repeat: Some(RepeatRate::DEFAULT),
        cooldown_steps: 0,
    };
    let stream = "0110111110010011111111000111";
    let mut first = app_with(Some(assist));
    let mut second = app_with(Some(assist));
    assert_eq!(play(&mut first, stream), play(&mut second, stream));
}

#[test]
fn cooldown_steps_drop_presses_inside_the_gap() {
    let mut app = app_with(Some(ActionAssist {
        cooldown_steps: 4,
        ..Default::default()
    }));
    assert_eq!(pressed(&play(&mut app, "101010101")), "100010001");
    let mut held = app_with(Some(ActionAssist {
        cooldown_steps: 4,
        ..Default::default()
    }));
    assert_eq!(pressed(&play(&mut held, "11011")), "11000");
}

#[test]
fn a_players_toggle_changes_only_that_players_stream() {
    let mut app = app_with(None);
    app.world_mut().resource_mut::<InputAssists>().set_player(
        1,
        "Crouch",
        ActionAssist {
            mode: PressMode::Toggle,
            ..Default::default()
        },
    );
    let mut contexts = InputContexts::default();
    contexts.set(0, ["Gameplay"]);
    contexts.set(1, ["Gameplay"]);
    app.insert_resource(contexts);
    let mut down = false;
    let mut player0 = String::new();
    let mut player1 = String::new();
    for c in "0110001110000100".chars() {
        let want = c == '1';
        if want != down {
            key(
                &mut app,
                if want {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            );
            down = want;
        }
        app.update();
        let actions = app.world().resource::<Actions>();
        let on = |player: u8| {
            actions
                .player(player)
                .states
                .get("Crouch")
                .is_some_and(|state| state.actuated())
        };
        player0.push(if on(0) { '1' } else { '0' });
        player1.push(if on(1) { '1' } else { '0' });
    }
    assert_eq!(player0, "0110001110000100");
    assert_eq!(player1, "0111110000000111");
}
