// SPDX-License-Identifier: MPL-2.0
//! Spec 11 § 7 end to end (ADR-5610): with `UiPlugin` and `InputPlugin` in one app, a finger on a
//! `VirtualButton` makes `actions.pressed("Jump")` true and a finger dragged on a `VirtualStick`
//! moves `actions.axis2d("Move")`, with no glue code; lifting the finger hands both actions back to
//! their (idle) device bindings. Headless: no window, no GPU, fixed 100 ms steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::TimeUpdateStrategy;
use regia_app::prelude::*;
use regia_input::prelude::*;
use regia_platform::TouchPhase;
use regia_ui::prelude::*;
use regia_ui::widgets::{VirtualButton, VirtualStick};

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
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
    app
}

fn computed(rect: [f32; 4]) -> ComputedNode {
    ComputedNode {
        rect,
        clip_rect: rect,
        scale: 1.0,
        ..Default::default()
    }
}

fn touch(app: &mut App, id: u64, phase: TouchPhase, x: f64, y: f64) {
    app.world_mut().write_message(regia_platform::TouchInput {
        window: Entity::PLACEHOLDER,
        id,
        phase,
        position: (x, y),
        timestamp: Default::default(),
    });
}

fn frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

#[test]
fn a_touch_button_presses_its_action() {
    let mut app = app();
    app.world_mut().spawn((
        Interactable::default(),
        computed([0.0, 0.0, 100.0, 100.0]),
        VirtualButton {
            action: "Jump".into(),
        },
    ));
    frames(&mut app, 2);
    assert!(!app.world().resource::<Actions>().pressed("Jump"));

    touch(&mut app, 1, TouchPhase::Started, 50.0, 50.0);
    frames(&mut app, 3);
    assert_eq!(
        app.world().resource::<regia_ui::VirtualButtons>().0.get("Jump"),
        Some(&true),
        "the widget is pressed"
    );
    assert!(
        app.world().resource::<Actions>().pressed("Jump"),
        "the touch button drives its action"
    );

    touch(&mut app, 1, TouchPhase::Ended, 50.0, 50.0);
    frames(&mut app, 4);
    assert!(
        !app.world().resource::<Actions>().pressed("Jump"),
        "lifting the finger releases the action"
    );
}

#[test]
fn a_touch_stick_moves_its_axis() {
    let mut app = app();
    // `VirtualStick` has a crate-private field, so set the public ones on a default value.
    let mut stick = VirtualStick::default();
    stick.action = "Move".into();
    stick.radius = 50.0;
    app.world_mut()
        .spawn((Interactable::default(), computed([0.0, 0.0, 100.0, 100.0]), stick));
    frames(&mut app, 2);
    touch(&mut app, 3, TouchPhase::Started, 50.0, 50.0);
    frames(&mut app, 2);
    // Drag 25 px right and 25 px up from the center: half deflection on both axes.
    touch(&mut app, 3, TouchPhase::Moved, 75.0, 25.0);
    frames(&mut app, 3);
    let axes = app.world().resource::<regia_ui::VirtualAxes>().0["Move"];
    assert_eq!(axes, [0.5, 0.5], "the widget follows the finger");
    let moved = app.world().resource::<Actions>().axis2d("Move");
    assert!(
        (moved.x - 0.5).abs() < 1e-5 && (moved.y - 0.5).abs() < 1e-5,
        "the touch stick drives its action: {moved:?}"
    );

    touch(&mut app, 3, TouchPhase::Ended, 75.0, 25.0);
    frames(&mut app, 4);
    assert_eq!(
        app.world().resource::<Actions>().axis2d("Move"),
        regia_core::math::Vec2::ZERO,
        "lifting the finger releases the action"
    );
}
