// SPDX-License-Identifier: MPL-2.0
//! A whole controls screen as a game builds one (the First Person template's `controls.rs`): a
//! `RebindPanel` with its options inside a frosted panel over a game frame, drawn through the real
//! `UiPlugin` extract and overlay. Checks the blur through the UI path and how the frame behind
//! shows; `REGIA_UI_SHOTS=<dir>` also writes a PNG for review by eye. Needs a software adapter.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use regia_app::prelude::*;
use regia_input::RebindSlot;
use regia_input::prelude::*;
use regia_platform::GamepadButton;
use regia_render::RenderDevice;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::style::{Align, FlexDirection};
use regia_ui::{
    BackdropBlur, BackgroundKind, ComputedNode, LayoutDirty, NodeName, OffscreenTarget, RebindPanel,
    RebindPanelOptions, UiScales,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;

fn app(blur: f32) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
        1.0 / 60.0,
    )));
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::ImeEvent>();
    app.add_plugins(InputPlugin::new(
        InputConfig::new()
            .action("Jump", ActionKind::Button)
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new(
                "Jump",
                InputSource::gamepad_button(GamepadButton::South),
            ))
            .binding(Binding::new(
                "Move",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(
                Binding::new("Move", InputSource::gamepad_stick(Stick::Left))
                    .with_modifier(DeadZone::radial(0.15, 1.0)),
            )
            .binding(Binding::new("Look", InputSource::MouseDelta))
            .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right))),
    ));
    let world = app.world_mut();
    world.insert_resource(UiScales {
        window_size: [WIDTH as f32, HEIGHT as f32],
        scale_factor: 1.0,
    });
    let root = world
        .spawn((
            UiRoot::default(),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                justify_content: JustifyContent::Center,
                align_items: Align::Center,
                ..Default::default()
            }),
        ))
        .id();
    let glass = world
        .spawn((
            UiNode,
            NodeName("controls.panel".to_owned()),
            Background {
                kind: BackgroundKind::Color(UiColor([0.05, 0.07, 0.1, 0.55])),
            },
            Border {
                radius: [12.0; 4],
                ..Default::default()
            },
            NodeStyle::new(Style {
                direction: FlexDirection::Column,
                ..Default::default()
            }),
        ))
        .id();
    if blur > 0.0 {
        world.entity_mut(glass).insert(BackdropBlur { radius: blur });
    }
    let panel = world
        .spawn((
            UiNode,
            RebindPanel {
                context: "Gameplay".into(),
                actions: Vec::new(),
            },
            RebindPanelOptions {
                per_device: true,
                axis_tuning: true,
                profiles: vec!["player1".into()],
            },
        ))
        .id();
    world.entity_mut(glass).add_child(panel);
    world.entity_mut(root).add_child(glass);
    for _ in 0..3 {
        app.update();
        app.world_mut().resource_mut::<LayoutDirty>().mark();
    }
    app.update();
    app
}

/// Diagonal stripes of 8 px: fine detail that a panel in front of it must melt.
fn game_frame() -> Vec<u8> {
    (0..WIDTH * HEIGHT)
        .flat_map(|i| {
            let (x, y) = (i % WIDTH, i / WIDTH);
            if ((x + y) / 8).is_multiple_of(2) {
                [200, 170, 120, 255]
            } else {
                [30, 60, 110, 255]
            }
        })
        .collect()
}

fn render(app: &App, device: &RenderDevice, frame: &[u8]) -> Vec<u8> {
    let shared = app.world().resource::<SharedUiRenderer>().clone();
    let target = OffscreenTarget::new(device, WIDTH, HEIGHT);
    assert!(target.upload(device, frame));
    target.draw_ui(device, &mut shared.lock());
    target.read(device).expect("readback")
}

fn panel_rect(app: &mut App) -> [f32; 4] {
    let entity = app
        .world_mut()
        .query::<(Entity, &NodeName)>()
        .iter(app.world())
        .find(|(_, n)| n.0 == "controls.panel")
        .map(|(e, _)| e)
        .expect("panel");
    app.world().get::<ComputedNode>(entity).expect("laid out").rect
}

/// How much neighboring pixels differ along a row, on average: fine detail scores high.
fn roughness(image: &[u8], rect: [u32; 4]) -> f32 {
    let (mut sum, mut count) = (0u32, 0u32);
    for y in rect[1]..rect[1] + rect[3] {
        for x in rect[0]..rect[0] + rect[2] - 1 {
            let at = ((y * WIDTH + x) * 4) as usize;
            sum += u32::from(image[at].abs_diff(image[at + 4]));
            count += 1;
        }
    }
    sum as f32 / count as f32
}

#[test]
fn the_frosted_controls_screen_melts_the_frame_behind_it_and_only_there() {
    let device = match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe): {err}"),
    };
    let frame = game_frame();
    let mut plain_app = app(0.0);
    let plain = render(&plain_app, &device, &frame);
    let mut glass_app = app(12.0);
    let glass = render(&glass_app, &device, &frame);
    let stats = glass_app
        .world()
        .resource::<SharedUiRenderer>()
        .lock()
        .backdrop_stats();
    assert_eq!(
        (stats.quads, stats.blurred, stats.skipped),
        (1, 1, 0),
        "{stats:?}"
    );
    let no_blur = plain_app
        .world()
        .resource::<SharedUiRenderer>()
        .lock()
        .backdrop_stats();
    assert_eq!(no_blur.quads, 0, "no flag, no blur");

    if let Ok(dir) = std::env::var("REGIA_UI_SHOTS") {
        std::fs::create_dir_all(&dir).expect("dir");
        for (name, pixels) in [("controls_plain.png", &plain), ("controls_frosted.png", &glass)] {
            image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels.clone())
                .expect("size")
                .save(std::path::Path::new(&dir).join(name))
                .expect("png");
        }
    }

    let rect = panel_rect(&mut glass_app);
    assert!(rect[2] > 200.0 && rect[3] > 100.0, "the panel has rows: {rect:?}");
    assert_eq!(
        rect,
        panel_rect(&mut plain_app),
        "the flag does not move the layout"
    );
    // A strip inside the panel's top padding: only tint and frame there, no widgets.
    let strip = [rect[0] as u32 + 20, rect[1] as u32 + 3, rect[2] as u32 - 40, 8];
    let (rough_plain, rough_glass) = (roughness(&plain, strip), roughness(&glass, strip));
    assert!(
        rough_plain > 5.0,
        "the stripes show through the plain tint: {rough_plain}"
    );
    assert!(
        rough_glass < rough_plain / 5.0,
        "melted: {rough_glass} against {rough_plain}"
    );
    // Away from the panel both frames are the game frame.
    for (x, y) in [
        (5u32, 5u32),
        (WIDTH - 5, HEIGHT - 5),
        (5, HEIGHT - 5),
        (WIDTH - 5, 5),
    ] {
        let at = ((y * WIDTH + x) * 4) as usize;
        assert_eq!(glass[at..at + 3], frame[at..at + 3], "{x},{y}");
        assert_eq!(plain[at..at + 3], frame[at..at + 3], "{x},{y}");
    }
    // The panel still sees the input layer it is drawn from.
    let label = regia_input::binding_label_in(
        glass_app.world().resource::<InputConfig>(),
        "Gameplay",
        "Jump",
        RebindSlot::Gamepad,
    );
    assert_eq!(label.as_deref(), Some("South"));
}
