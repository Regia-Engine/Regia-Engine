// SPDX-License-Identifier: MPL-2.0
//! The color-vision filter (ADR-2646) is composed into the extracted views after `extract_frame`
//! fills them and before any view renders, in the plugin's own schedule. Each probe closes a cycle
//! only if the plugin orders the system that way: a probe ordered after the filter and before the
//! extraction, or after rendering and before the filter, must make the schedule refuse to build.

use regia_app::prelude::*;
use regia_render::RenderPlugin;
use regia_render::color_vision::{ColorVision, apply_color_vision};
use regia_render::runtime::{extract_frame, render_texture_views};

fn probe() {}

fn builds(headless: bool, add: impl FnOnce(&mut App)) -> bool {
    let mut app = App::new();
    app.add_plugins(RenderPlugin { headless });
    assert!(
        app.world().contains_resource::<ColorVision>(),
        "the plugin inserts the ColorVision resource"
    );
    add(&mut app);
    let world = app.world_mut();
    let mut schedules = world
        .remove_resource::<regia_ecs::ecs::schedule::Schedules>()
        .expect("schedules");
    let built = schedules
        .get_mut(Render)
        .expect("render schedule")
        .initialize(world)
        .is_ok();
    world.insert_resource(schedules);
    built
}

#[test]
fn the_filter_runs_after_extraction() {
    for headless in [true, false] {
        assert!(
            !builds(headless, |app| {
                app.add_systems(Render, probe.after(apply_color_vision).before(extract_frame));
            }),
            "headless {headless}: apply_color_vision must run after extract_frame"
        );
    }
}

#[test]
fn the_filter_runs_before_rendering() {
    for headless in [true, false] {
        assert!(
            !builds(headless, |app| {
                app.add_systems(
                    Render,
                    probe.after(render_texture_views).before(apply_color_vision),
                );
            }),
            "headless {headless}: render_texture_views must run after apply_color_vision"
        );
    }
}

#[test]
fn an_unrelated_probe_builds() {
    // The control: the same schedule with a probe that closes no cycle builds.
    assert!(builds(true, |app| {
        app.add_systems(Render, probe.after(apply_color_vision));
    }));
}
