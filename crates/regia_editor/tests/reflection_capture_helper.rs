// SPDX-License-Identifier: MPL-2.0
//! The selected reflection capture's box helper in a real capture (`editor.capture_viewport`,
//! lavapipe; ADR-7592). No golden: a control capture of the same scene with Show ▸ Bounds off is
//! the reference, and the two frames may differ only inside the projected blend-margin box.

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::views::{ViewCamera, ViewKind};
use serde_json::{Value, json};

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
        .insert_resource(regia_editor::viewport::EditorCamera {
            target: Vec3::new(0.0, 1.0, 0.0),
            distance: 20.0,
            ..Default::default()
        });
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

#[test]
fn capture_box_helper_changes_pixels_only_inside_the_projected_box() {
    if regia_render::RenderDevice::headless().is_err() {
        panic!("no GPU adapter (install Mesa lavapipe or use WARP)");
    }
    let mut app = edit_app();
    let at = Vec3::new(0.0, 1.0, 0.0);
    // Create ▸ Reflection capture selects the new capture, so its helper is the selection's.
    let reply = call(
        &mut app,
        "editor.create_entity",
        json!({ "kind": "reflection_capture", "translation": [at.x, at.y, at.z] }),
    );
    let entity = Entity::try_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
    app.update();
    let capture_component = *app
        .world()
        .get::<regia_render::LocalCapture>(entity)
        .expect("a LocalCapture");
    assert_eq!(
        app.world().resource::<Selection>().primary(),
        Some(entity),
        "the new capture is selected"
    );

    call(
        &mut app,
        "editor.set_viewport",
        json!({ "bounds": false, "grid": false }),
    );
    let control = capture(&mut app);
    let flags = call(
        &mut app,
        "editor.set_viewport",
        json!({ "bounds": true, "grid": false }),
    );
    assert_eq!(flags["bounds"], true);
    let shown = capture(&mut app);

    // The screen rectangle of the blend-margin box (the outer one), with a line's width of slack.
    let camera = app
        .world()
        .resource::<regia_editor::viewport::EditorCamera>()
        .clone();
    let view = ViewCamera::of(ViewKind::Perspective, &camera);
    let size = Vec2::new(W as f32, H as f32);
    let half = Vec3::from_array(capture_component.half_extent) + Vec3::splat(capture_component.fade);
    let corners: Vec<Vec2> = regia_editor::capture_helper::box_edges(at, half)
        .iter()
        .flat_map(|(a, b)| [*a, *b])
        .filter_map(|p| view.project(p, size))
        .collect();
    assert_eq!(corners.len(), 24, "the whole box is in view");
    let min = corners.iter().fold(Vec2::splat(f32::MAX), |m, p| m.min(*p)) - Vec2::splat(3.0);
    let max = corners.iter().fold(Vec2::splat(f32::MIN), |m, p| m.max(*p)) + Vec2::splat(3.0);
    let inside = |x: u32, y: u32| {
        let p = Vec2::new(x as f32, y as f32);
        p.x >= min.x && p.y >= min.y && p.x <= max.x && p.y <= max.y
    };
    let box_rgb = regia_editor::capture_helper::CAPTURE_BOX.to_array();
    let mut changed_inside = 0;
    let mut changed_outside = 0;
    let mut box_colored = 0;
    for ((x, y, on), off) in shown.enumerate_pixels().zip(control.pixels()) {
        if on.0 != off.0 {
            if inside(x, y) {
                changed_inside += 1;
            } else {
                changed_outside += 1;
            }
        }
        if on.0[..3] == box_rgb[..3] {
            box_colored += 1;
        }
    }
    eprintln!(
        "changed inside {changed_inside}, outside {changed_outside}; box-colored {box_colored}; box {min}..{max}"
    );
    assert!(changed_inside > 100, "the helper draws: {changed_inside} pixels");
    assert!(box_colored > 50, "in the box color: {box_colored}");
    assert_eq!(changed_outside, 0, "nothing changes outside the projected box");
}

/// Tools ▸ Bake reflection captures with the real renderer (lavapipe): `editor.bake_captures`
/// bakes the capture with the edit world's `RuntimeRenderer`, one capture per tick, and writes a
/// `.fcap` that reads back with the capture's id and box, named in the level.
#[test]
fn bake_reflection_captures_writes_the_level_fcap_with_the_gpu_bake() {
    let Ok(device) = regia_render::RenderDevice::shared_headless() else {
        panic!("no GPU adapter (install Mesa lavapipe or use WARP)");
    };
    let root = regia_test_utils::unique_temp_dir("capture-bake-gpu");
    std::fs::create_dir_all(root.join("assets/maps")).expect("assets");
    let mut app = edit_app();
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
    app.world_mut().init_resource::<regia_render::RuntimeRenderer>();
    app.world_mut().insert_resource(device);
    app.world_mut()
        .init_resource::<regia_scene::level::LevelSettings>();
    call(
        &mut app,
        "editor.create_entity",
        json!({ "kind": "plane", "translation": [0.0, 0.0, 0.0] }),
    );
    let reply = call(
        &mut app,
        "editor.create_entity",
        json!({ "kind": "reflection_capture", "translation": [0.0, 1.0, 0.0] }),
    );
    let entity = Entity::try_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
    app.update();
    let capture_component = *app
        .world()
        .get::<regia_render::LocalCapture>(entity)
        .expect("a LocalCapture");
    let started = call(
        &mut app,
        "editor.bake_captures",
        json!({ "file": "maps/gpu.fcap" }),
    );
    assert_eq!(started["captures"], 1, "{started}");
    // One capture per tick; a second tick is a no-op once done.
    regia_editor::capture_bake::tick(app.world_mut());
    regia_editor::capture_bake::tick(app.world_mut());
    let status = call(&mut app, "editor.bake_captures_status", json!({}));
    assert_eq!(status["state"], "done", "{status}");
    let path = root.join("assets/maps/gpu.fcap");
    let file = regia_asset::local_capture_bake::FcapFile::read(&path).expect("the bake reads");
    assert_eq!(file.captures.len(), 1);
    let baked = &file.captures[0];
    assert_eq!(baked.id, capture_component.id);
    assert_eq!(baked.center, [0.0, 1.0, 0.0]);
    assert_eq!(baked.half_extent, capture_component.half_extent);
    assert_eq!(baked.mips.len(), regia_render::local_capture_mip_count() as usize);
    assert!((0.0..=1.0).contains(&baked.coverage), "{}", baked.coverage);
    assert_eq!(status["bytes"], std::fs::metadata(&path).expect("meta").len());
    assert_eq!(
        app.world()
            .resource::<regia_scene::level::LevelSettings>()
            .look
            .captures
            .as_ref()
            .map(|c| c.file.as_str()),
        Some("maps/gpu.fcap")
    );
    // ADR-7663: the next editor frame draws the bake in the viewport: the capture is placed on the
    // edit world's renderer with the file's map, and the capture selection (which the offscreen
    // viewport render does not run) puts it in a layer and uploads the map on this device.
    regia_editor::capture_bake::tick(app.world_mut());
    let infos = app
        .world()
        .resource::<regia_render::RuntimeRenderer>()
        .0
        .local_captures();
    assert_eq!(infos.len(), 1, "{infos:?}");
    let drawn = &infos[0];
    assert_eq!(drawn.desc.id, capture_component.id);
    assert!(drawn.baked, "placed with its bake");
    assert_eq!(
        drawn.desc.bake.as_ref().map(|b| b.mips.clone()),
        Some(baked.mips.clone()),
        "the file's map"
    );
    assert!(drawn.layer.is_some(), "selected into a layer: {drawn:?}");
    assert!(drawn.uploads >= 1, "its map was written to the layer: {drawn:?}");
    let status = call(&mut app, "editor.bake_captures_status", json!({}));
    assert_eq!(
        status["viewport"]["drawn"],
        json!([capture_component.id]),
        "{status}"
    );
    std::fs::remove_dir_all(&root).ok();
}
