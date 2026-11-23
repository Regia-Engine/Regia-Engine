// SPDX-License-Identifier: MPL-2.0
//! Acceptance (quick-app lane): the same `Quick` code runs as a quick app's `update` and inside an
//! ordinary Regia Engine system, and both see the same input, time and window and draw the same things.

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_input::InputPlugin;
use regia_platform::{ButtonState, CursorMoved, KeyboardInput, PrimaryWindow, Window};
use regia_quick::frame::{FrameParams, QuickRenderer};
use regia_quick::{Draw, Model, Quick, app};
use regia_render::ImmediateBatch;

#[derive(Reflect, Default, Debug, Clone, PartialEq)]
struct Ball {
    pos: Vec2,
    presses: u32,
    frames: u32,
}

/// Game code written once against `&Quick`.
fn step(q: &Quick, ball: &mut Ball) {
    ball.frames += 1;
    if q.key_pressed("Space") {
        ball.presses += 1;
    }
    if q.key_down("ArrowRight") {
        ball.pos.x += 10.0;
    }
    ball.pos.y = q.mouse().y;
    q.draw().ellipse().xy(ball.pos).radius(5.0);
}

/// The same state, owned by a plain resource and advanced by a plain system.
#[derive(Resource, Default)]
struct Plain(Ball);

fn plain_system(q: Quick, mut ball: ResMut<Plain>) {
    step(&q, &mut ball.0);
}

fn base_app() -> (App, Entity) {
    let mut game = App::new();
    game.add_plugins(InputPlugin::default())
        .insert_resource(Draw::default());
    let window = game
        .world_mut()
        .spawn((
            Window {
                physical_size: (800, 600),
                scale_factor: 1.0,
                ..Window::default()
            },
            PrimaryWindow,
        ))
        .id();
    (game, window)
}

fn key(window: Entity, name: &str, state: ButtonState) -> KeyboardInput {
    KeyboardInput {
        window,
        physical_key: name.into(),
        logical_key: name.into(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    }
}

/// Drives an app through the same scripted input: Space tapped on frame 1, ArrowRight held on frames
/// 2–4, the cursor moving down the window.
fn drive(game: &mut App, window: Entity) {
    for frame in 0..6u32 {
        let world = game.world_mut();
        world.write_message(CursorMoved {
            window,
            position: (400.0, 100.0 * f64::from(frame)),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        let keys: &[(&str, ButtonState)] = match frame {
            1 => &[("Space", ButtonState::Pressed)],
            2 => &[
                ("Space", ButtonState::Released),
                ("ArrowRight", ButtonState::Pressed),
            ],
            5 => &[("ArrowRight", ButtonState::Released)],
            _ => &[],
        };
        for (name, state) in keys {
            world.write_message(key(window, name, *state));
        }
        game.update();
    }
}

#[test]
fn the_same_quick_code_runs_as_an_app_and_as_a_plain_system() {
    let (mut quick_app, window) = base_app();
    quick_app.add_plugins(app(|_: &Quick| Ball::default()).update(step));
    drive(&mut quick_app, window);

    let (mut system_app, window) = base_app();
    system_app
        .init_resource::<Plain>()
        .add_systems(Update, plain_system);
    drive(&mut system_app, window);

    let from_app = &quick_app.world().resource::<Model<Ball>>().model;
    let from_system = &system_app.world().resource::<Plain>().0;
    assert_eq!(from_app, from_system);
    assert_eq!(from_app.frames, 6);
    assert_eq!(from_app.presses, 1, "Space was pressed once");
    assert_eq!(from_app.pos.x, 30.0, "ArrowRight held on three frames");
    assert_eq!(
        from_app.pos.y,
        300.0 - 500.0,
        "cursor at y = 500 px is 200 below the center"
    );
    // Both drew into the frame's `Draw` (nothing extracts it without a renderer), and drew the
    // same shapes: tessellating each gives identical vertices.
    let params = FrameParams {
        size: Vec2::new(800.0, 600.0),
        scale_factor: 1.0,
    };
    let tessellate = |app: &App| {
        let mut batch = ImmediateBatch::new();
        QuickRenderer::new().build(app.world().resource::<Draw>(), params, None, &mut batch);
        batch
    };
    let (from_app_draw, from_system_draw) = (tessellate(&quick_app), tessellate(&system_app));
    assert!(!from_app_draw.vertices().is_empty());
    assert_eq!(from_app_draw.vertices().len(), from_system_draw.vertices().len());
    for (a, b) in from_app_draw.vertices().iter().zip(from_system_draw.vertices()) {
        assert_eq!(a.position, b.position);
        assert_eq!(a.color, b.color);
    }
    assert_eq!(from_app_draw.indices(), from_system_draw.indices());
}
