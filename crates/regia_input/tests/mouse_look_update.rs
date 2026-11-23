// SPDX-License-Identifier: MPL-2.0
//! Mouse look read in `Update` on frames that run several fixed steps (ADR-5599).
//!
//! `Mouse::delta` is per frame and must be applied once per frame. Applied on the frame's first
//! fixed step, the later steps evaluated a zero look and `Update` readers (the game templates'
//! `move_player`, the showcase fly camera), which see the last step's `Actions`, read zero on
//! every frame that ran two or more steps: below the fixed rate, mouse look stopped.
//!
//! Frames are manual durations. Nothing here sleeps or reads the wall clock.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_input::prelude::*;

/// What `Update` and `FixedUpdate` read of `Look`.
#[derive(Resource, Default)]
struct Seen {
    /// `Update`'s reading of the last frame.
    update: Vec2,
    /// `FixedUpdate` readings, summed over every step so far.
    fixed_sum: Vec2,
    /// Fixed steps that read a non-zero look.
    fixed_nonzero_steps: u32,
}

fn read_update(actions: Res<Actions>, mut seen: ResMut<Seen>) {
    seen.update = actions.axis2d("Look");
}

fn read_fixed(actions: Res<Actions>, mut seen: ResMut<Seen>) {
    let look = actions.axis2d("Look");
    seen.fixed_sum += look;
    if look != Vec2::ZERO {
        seen.fixed_nonzero_steps += 1;
    }
}

fn look_app() -> App {
    let config = InputConfig::new()
        .action("Look", ActionKind::Axis2D)
        .binding(Binding::new("Look", InputSource::MouseDelta));
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    app.add_plugins(InputPlugin::new(config));
    app.init_resource::<Seen>();
    app.add_systems(Update, read_update);
    app.add_systems(FixedUpdate, read_fixed.after(InputSet::Evaluation));
    // A first frame so `FixedTime` exists and the bank starts from a known state.
    app.update();
    app
}

fn move_mouse(app: &mut App, dx: f64, dy: f64) {
    app.world_mut().write_message(regia_platform::MouseMotion {
        delta: (dx, dy),
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn frame(app: &mut App, secs: f64) {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(secs)));
    app.update();
}

/// A 50 ms frame at the 60 Hz fixed rate runs three steps. `Update` sees the frame's motion, and
/// the fixed steps apply it exactly once between them.
#[test]
fn update_mouse_look_sees_the_motion_of_a_three_step_frame() {
    let mut app = look_app();
    let before = app.world().resource::<Seen>().fixed_sum;
    move_mouse(&mut app, 10.0, -5.0);
    frame(&mut app, 0.05);
    let seen = app.world().resource::<Seen>();
    assert_eq!(
        seen.update,
        Vec2::new(10.0, -5.0),
        "Update reads the frame's motion, not the zero of a later step"
    );
    assert_eq!(
        seen.fixed_sum - before,
        Vec2::new(10.0, -5.0),
        "the fixed steps apply the motion once"
    );
    assert_eq!(seen.fixed_nonzero_steps, 1, "on one step only");
}

/// Every frame of a 30 fps run (two steps each) keeps its own motion: none is dropped, none is
/// carried into the next frame.
#[test]
fn update_mouse_look_follows_every_frame_of_a_thirty_fps_run() {
    let mut app = look_app();
    for i in 1..=6 {
        let dx = f64::from(i);
        move_mouse(&mut app, dx, 0.0);
        frame(&mut app, 1.0 / 30.0);
        let seen = app.world().resource::<Seen>();
        assert_eq!(
            seen.update,
            Vec2::new(dx as f32, 0.0),
            "frame {i}: Update reads this frame's motion"
        );
    }
    let seen = app.world().resource::<Seen>();
    assert_eq!(seen.fixed_sum, Vec2::new(21.0, 0.0), "1 + 2 + .. + 6, once each");
    assert_eq!(seen.fixed_nonzero_steps, 6);
}

/// A frame shorter than a step runs no fixed step: the `PreUpdate` fallback still shows the
/// motion to `Update`, and no motion leaks into the next frame's steps.
#[test]
fn a_zero_step_frame_still_shows_its_motion_and_does_not_leak_it() {
    let mut app = look_app();
    // An empty bank, so the 1 ms frame below cannot complete a step.
    app.world_mut().resource_mut::<FixedTime>().clear_accumulator();
    let before = app.world().resource::<Seen>().fixed_sum;
    move_mouse(&mut app, 3.0, 4.0);
    frame(&mut app, 0.001);
    assert_eq!(app.world().resource::<Seen>().update, Vec2::new(3.0, 4.0));
    frame(&mut app, 1.0 / 60.0);
    let seen = app.world().resource::<Seen>();
    assert_eq!(seen.update, Vec2::ZERO, "no new motion next frame");
    assert_eq!(
        seen.fixed_sum - before,
        Vec2::ZERO,
        "the zero-step frame's motion is not replayed by the next frame's step"
    );
}
