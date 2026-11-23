// SPDX-License-Identifier: MPL-2.0
//! Render overrides (Tincal F5) are extracted before the frame renders, in the plugin's own
//! schedule, without running any system by hand. A probe system is ordered after
//! `render_texture_views` and before `extract_render_overrides`: if the plugin orders rendering
//! after the extraction, that closes a cycle and the schedule refuses to build; if the plugin
//! leaves the two unordered, the probe builds and this test fails.

use regia_app::prelude::*;
use regia_render::RenderPlugin;
use regia_render::render_override::extract_render_overrides;
use regia_render::runtime::render_texture_views;

fn probe() {}

fn render_schedule_builds_with_probe(headless: bool) -> bool {
    let mut app = App::new();
    app.add_plugins(RenderPlugin { headless });
    app.add_systems(
        Render,
        probe.after(render_texture_views).before(extract_render_overrides),
    );
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
fn rendering_is_ordered_after_override_extraction() {
    assert!(
        !render_schedule_builds_with_probe(true),
        "headless: render_texture_views must run after extract_render_overrides"
    );
}

#[test]
fn windowed_rendering_is_ordered_after_override_extraction() {
    assert!(
        !render_schedule_builds_with_probe(false),
        "windowed: render_texture_views must run after extract_render_overrides"
    );
}
