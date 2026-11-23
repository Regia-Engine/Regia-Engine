// SPDX-License-Identifier: MIT-0
//! What the per-frame screen-reader sync costs on the First Person template's controls screen as a
//! phone shows it (ADR-5379): the tree build, the diff and the whole `sync_screen_reader`, each
//! timed over a few hundred unchanged frames. Prints only; the device bar (release, TalkBack on,
//! under 1 ms mean and p99) is checked on the phone through `ui.a11y_tree` `sync_cost`.
//!
//! `cargo test --release -p ui_gallery --test a11y_sync_cost -- --nocapture`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_ui::a11y::{NullScreenReader, ScreenReader, SyncStage, build_a11y_tree, sync_screen_reader};
use regia_ui::text::TextSystem;
use regia_ui::theme_resolve::ActiveTheme;

#[path = "../../../crates/regia_templates/templates/first_person/game/src/controls.rs"]
mod template_controls;

const ROUNDS: usize = 300;

fn key(app: &mut App, name: &str) {
    for state in [ButtonState::Pressed, ButtonState::Released] {
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.to_owned(),
            logical_key: name.to_owned(),
            state,
            repeat: false,
            timestamp: InputTimestamp::ZERO,
        });
        app.update();
    }
}

/// The controls screen open in the touch layout on the TEST Pixel 9 Pro's window.
fn phone_controls() -> App {
    let config = regia_input::InputConfig::new()
        .action("Look", regia_input::ActionKind::Axis2D)
        .binding(regia_input::Binding::new(
            "Look",
            regia_input::InputSource::gamepad_stick(regia_input::Stick::Right),
        ));
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only())
        .add_message::<WindowResized>()
        .add_message::<WindowScaleFactorChanged>()
        .add_message::<ImeEvent>()
        .add_plugins(regia_input::InputPlugin::new(config))
        .insert_resource(template_controls::ControlsLayout::Touch)
        .add_plugins(template_controls::ControlsPlugin);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app.insert_resource(TextSystem::embedded_only());
    app.update();
    app.insert_resource(regia_ui::UiScales {
        window_size: [2142.0, 960.0],
        scale_factor: 2.25,
    });
    key(&mut app, "F10");
    assert!(app.world().resource::<template_controls::ControlsOpen>().0);
    app.insert_resource(ActiveTheme::named("regia-dark"));
    for _ in 0..24 {
        app.update();
    }
    app
}

fn per_round(total: Duration) -> f64 {
    total.as_secs_f64() * 1e6 / ROUNDS as f64
}

#[test]
fn print_the_sync_stages_on_the_phone_controls_screen() {
    let mut app = phone_controls();
    let world = app.world_mut();
    let entities = world.query::<&regia_ui::ComputedNode>().iter(world).count();
    let mut tree = build_a11y_tree(world);
    let started = Instant::now();
    for _ in 0..ROUNDS {
        tree = std::hint::black_box(build_a11y_tree(world));
    }
    let build = per_round(started.elapsed());
    let previous = tree.clone();
    let started = Instant::now();
    for _ in 0..ROUNDS {
        std::hint::black_box(tree.diff(&previous));
    }
    let diff = per_round(started.elapsed());
    let mut reader = ScreenReader::with_backend(Box::new(NullScreenReader));
    reader.set_exporting(true);
    world.insert_resource(reader);
    sync_screen_reader(world);
    let started = Instant::now();
    for _ in 0..ROUNDS {
        sync_screen_reader(world);
    }
    let sync = per_round(started.elapsed());
    let cost = world.resource::<ScreenReader>().sync_cost();
    println!(
        "a11y sync on the phone controls screen: {} nodes of {entities} UI entities; build {build:.1} us, \
         diff {diff:.1} us, sync {sync:.1} us (sync_cost mean {:.4} ms p99 {:.4} ms, stages {:?} ms)",
        tree.nodes.len(),
        cost.mean_ms(),
        cost.percentile_ms(99.0),
        SyncStage::ALL.map(|s| cost.stage_mean_ms(s))
    );
    assert!(!tree.nodes.is_empty());
}
