// SPDX-License-Identifier: MPL-2.0
//! Viewport helpers in real captures (`editor.capture_viewport`, lavapipe; ADR-7468). No golden:
//! a control capture of the same scene with the helper switched off is the reference.
//!
//! - A player start placed with `editor.create_entity` shows helper-colored pixels near its
//!   projected position, none with the icons hidden.
//! - Show ▸ Navigation (`editor.set_viewport { navigation }`) over a baked floor changes pixels
//!   over the floor against the flag-off control.

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::views::{ViewCamera, ViewKind};
use serde_json::{Value, json};

/// The player start's helper color (`overlays::ICON_PLAYER_START`).
const PLAYER_START_RGB: [u8; 3] = [150, 255, 40];
const W: u32 = 320;
const H: u32 = 240;

fn edit_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    app.world_mut()
        .insert_resource(regia_editor::viewport::EditorCamera::default());
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

fn call(app: &mut App, name: &str, params: Value) -> Value {
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method(name)
        .unwrap_or_else(|| panic!("{name} is not registered"));
    handler(app.world_mut(), &params).unwrap_or_else(|err| panic!("{name}: {err:?}"))
}

/// The capture's pixels as RGBA8.
fn capture(app: &mut App) -> image::RgbaImage {
    let reply = call(app, "editor.capture_viewport", json!({ "width": W, "height": H }));
    let png =
        regia_editor::reference::base64_decode(reply["png_base64"].as_str().expect("png")).expect("base64");
    image::load_from_memory(&png).expect("png decodes").to_rgba8()
}

/// Helper-colored pixels within `radius` pixels of `at`, and in the whole image.
fn helper_pixels(image: &image::RgbaImage, at: Vec2, radius: f32) -> (usize, usize) {
    let mut near = 0;
    let mut all = 0;
    for (x, y, p) in image.enumerate_pixels() {
        if p.0[..3] == PLAYER_START_RGB {
            all += 1;
            if (Vec2::new(x as f32, y as f32) - at).length() <= radius {
                near += 1;
            }
        }
    }
    (near, all)
}

#[test]
fn player_start_helper_shows_in_the_capture_and_not_in_the_control() {
    if regia_render::RenderDevice::headless().is_err() {
        panic!("no GPU adapter (install Mesa lavapipe or use WARP)");
    }
    let mut app = edit_app();
    let at = Vec3::new(0.5, 0.0, 0.5);
    let reply = call(
        &mut app,
        "editor.create_entity",
        json!({ "kind": "player_start", "translation": [at.x, at.y, at.z] }),
    );
    let start = Entity::try_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
    // Nothing selected: the selection outline and gizmo stay out of both captures.
    app.world_mut().resource_mut::<Selection>().clear();
    app.update();
    assert!(app.world().get_entity(start).is_ok());

    call(
        &mut app,
        "editor.set_viewport",
        json!({ "icons": true, "grid": false }),
    );
    let shown = capture(&mut app);
    call(
        &mut app,
        "editor.set_viewport",
        json!({ "icons": false, "grid": false }),
    );
    let control = capture(&mut app);

    let camera = app
        .world()
        .resource::<regia_editor::viewport::EditorCamera>()
        .clone();
    let projected = ViewCamera::of(ViewKind::Perspective, &camera)
        .project(at, Vec2::new(W as f32, H as f32))
        .expect("the start is in view");
    let (near, all) = helper_pixels(&shown, projected, 60.0);
    let (control_near, control_all) = helper_pixels(&control, projected, 60.0);
    eprintln!("helper pixels: near {near} of {all}; control {control_near} of {control_all}; at {projected}");
    assert!(
        near > 30,
        "helper pixels near the player start: {near} (of {all})"
    );
    assert_eq!(control_all, 0, "the control capture (helpers hidden) has none");
    // Away from the helper the two captures agree (the helper is the only difference).
    let differing_far = shown
        .enumerate_pixels()
        .zip(control.pixels())
        .filter(|((x, y, a), b)| a.0 != b.0 && (Vec2::new(*x as f32, *y as f32) - projected).length() > 90.0)
        .count();
    assert!(
        differing_far < (W * H / 100) as usize,
        "only the helper differs: {differing_far} pixels far from it"
    );
}

/// The navigation overlay's color (`overlays::NAVIGATION`).
const NAVIGATION_RGB: [u8; 3] = [26, 191, 255];

#[test]
fn navigation_flag_changes_pixels_over_a_baked_floor() {
    if regia_render::RenderDevice::headless().is_err() {
        panic!("no GPU adapter (install Mesa lavapipe or use WARP)");
    }
    let mut app = edit_app();
    // A floor (a plane shape, 8 m) and its baked nav mesh.
    let reply = call(
        &mut app,
        "editor.create_entity",
        json!({ "kind": "plane", "translation": [0.0, 0.0, 0.0] }),
    );
    let floor = Entity::try_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
    if let Some(mut transform) = app.world_mut().get_mut::<regia_scene::prelude::Transform>(floor) {
        transform.scale = Vec3::new(8.0, 1.0, 8.0);
    }
    let (a, b, c, d) = (
        Vec3::new(-3.0, 0.0, -3.0),
        Vec3::new(3.0, 0.0, -3.0),
        Vec3::new(3.0, 0.0, 3.0),
        Vec3::new(-3.0, 0.0, 3.0),
    );
    let mesh = regia_nav::NavMeshBuilder::new(regia_nav::NavMeshBuildParams::default())
        .build(&[[a, c, b], [a, d, c]])
        .expect("bake");
    app.world_mut().insert_resource(mesh);
    app.world_mut().resource_mut::<Selection>().clear();
    app.update();

    let flags = call(&mut app, "editor.set_viewport", json!({ "grid": false }));
    assert_eq!(flags["navigation"], false, "off by default");
    let control = capture(&mut app);
    let flags = call(
        &mut app,
        "editor.set_viewport",
        json!({ "navigation": true, "grid": false }),
    );
    assert_eq!(flags["navigation"], true);
    let shown = capture(&mut app);

    // The floor's screen rectangle: the projected corners of the baked area.
    let camera = app
        .world()
        .resource::<regia_editor::viewport::EditorCamera>()
        .clone();
    let view = ViewCamera::of(ViewKind::Perspective, &camera);
    let size = Vec2::new(W as f32, H as f32);
    let corners: Vec<Vec2> = [a, b, c, d]
        .iter()
        .filter_map(|p| view.project(*p, size))
        .collect();
    assert_eq!(corners.len(), 4, "the floor is in view");
    let min = corners.iter().fold(Vec2::splat(f32::MAX), |m, p| m.min(*p)) - Vec2::splat(4.0);
    let max = corners.iter().fold(Vec2::splat(f32::MIN), |m, p| m.max(*p)) + Vec2::splat(4.0);
    let over_floor = |x: u32, y: u32| {
        let p = Vec2::new(x as f32, y as f32);
        p.x >= min.x && p.y >= min.y && p.x <= max.x && p.y <= max.y
    };
    let mut changed_over = 0;
    let mut changed_elsewhere = 0;
    let mut nav_colored = 0;
    for ((x, y, on), off) in shown.enumerate_pixels().zip(control.pixels()) {
        if on.0 != off.0 {
            if over_floor(x, y) {
                changed_over += 1;
            } else {
                changed_elsewhere += 1;
            }
        }
        if on.0[..3] == NAVIGATION_RGB {
            nav_colored += 1;
        }
    }
    eprintln!(
        "navigation: {changed_over} changed over the floor, {changed_elsewhere} elsewhere, {nav_colored} nav-colored"
    );
    assert!(
        changed_over > 60,
        "the nav polygons draw over the floor: {changed_over}"
    );
    assert!(nav_colored > 60, "in the navigation color: {nav_colored}");
    assert!(
        changed_elsewhere * 10 < changed_over,
        "the change is over the floor: {changed_elsewhere} elsewhere vs {changed_over}"
    );
    assert_eq!(
        control.pixels().filter(|p| p.0[..3] == NAVIGATION_RGB).count(),
        0,
        "the control has none"
    );
}
