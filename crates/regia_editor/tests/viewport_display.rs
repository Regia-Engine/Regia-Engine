// SPDX-License-Identifier: MPL-2.0
//! Viewport display and debug (lane w12-viewport-display, ADR-7770; `book/agents/viewport-tools.md`,
//! `book/agents/rendering-debug.md`). No golden: every image check compares two captures of the
//! same scene on lavapipe.
//!
//! - P-075: every View mode renders a frame that differs from Lit, and the pane's mode
//!   (`editor.set_viewport { view_mode }`, what the toolbar and Alt+1..8 set) draws exactly what
//!   `editor.capture_viewport { debug_view }` draws for that mode (one code path).
//! - P-082: the Exposure menu pins the pane's EV (fixed, level band).
//! - P-080: the screen percentage scales the internal render size; the picture keeps the pane size.
//! - P-078, P-081: `editor.viewport_badges` lists realtime off and the changed settings and resets
//!   them.
//! - P-085, P-091: `editor.hide` / `editor.isolate` never dirty the level, and picking skips what
//!   they hide.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::views::{ViewCamera, ViewKind};
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

const W: u32 = 240;
const H: u32 = 180;

fn gpu_or_panic() {
    if regia_render::RenderDevice::headless().is_err() {
        panic!("no GPU adapter (install Mesa lavapipe or use WARP)");
    }
}

/// An edit world with two red boxes side by side (the right one behind a smaller one, so overdraw
/// stacks) and a point light between them.
fn edit_app() -> (App, Entity, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<UndoStack>()
        .init_resource::<Selection>()
        .init_resource::<regia_editor::viewport::GizmoState>()
        .init_resource::<regia_editor::viewport::ViewportSettings>();
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut()
        .insert_resource(regia_editor::viewport::EditorCamera {
            target: Vec3::new(0.0, 0.5, 0.0),
            distance: 6.0,
            yaw: 0.3,
            pitch: 0.3,
            ..regia_editor::viewport::EditorCamera::default()
        });
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    let world = app.world_mut();
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let mesh = Arc::new(regia_render::CpuMesh { vertices, indices });
    let red = Arc::new(regia_render::LabMaterial {
        base_color: [0.8, 0.1, 0.1, 1.0],
        ..regia_render::LabMaterial::default()
    });
    let left = world
        .spawn((
            Transform::from_translation(Vec3::new(-1.0, 0.5, 0.0)),
            regia_render::MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&red)),
        ))
        .id();
    let right = world
        .spawn((
            Transform::from_translation(Vec3::new(1.0, 0.5, 0.0)),
            regia_render::MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&red)),
        ))
        .id();
    world.spawn((
        Transform {
            translation: Vec3::new(1.0, 0.5, 0.9),
            scale: Vec3::splat(0.5),
            ..Transform::default()
        },
        regia_render::MeshRenderer::new(mesh, red),
    ));
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, 1.2, 0.8)),
        regia_render::PointLight {
            intensity: 2000.0,
            radius: 6.0,
            ..regia_render::PointLight::default()
        },
    ));
    app.update();
    let mut undo = app.world_mut().resource_mut::<UndoStack>();
    undo.mark_saved();
    (app, left, right)
}

/// Calls an editor agent method; `Err` carries the error's `reason` (or message).
fn try_call(app: &mut App, name: &str, params: Value) -> Result<Value, String> {
    let Some((_, handler)) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method(name)
    else {
        return Err(format!("{name} is not registered"));
    };
    handler(app.world_mut(), &params).map_err(|e| {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason").or_else(|| d.get("code")))
            .and_then(Value::as_str)
            .map_or_else(|| e.message.clone(), str::to_owned)
    })
}

fn call(app: &mut App, name: &str, params: Value) -> Value {
    try_call(app, name, params).unwrap_or_else(|err| panic!("{name}: {err}"))
}

/// A capture's pixels (RGBA8) and its `info`.
fn capture(app: &mut App, extra: Value) -> (image::RgbaImage, Value) {
    let mut params = json!({ "width": W, "height": H });
    if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
        params.extend(extra.clone());
    }
    let reply = call(app, "editor.capture_viewport", params);
    let png =
        regia_editor::reference::base64_decode(reply["png_base64"].as_str().expect("png")).expect("base64");
    let image = image::load_from_memory(&png).expect("png decodes").to_rgba8();
    (image, reply["info"].clone())
}

/// The share of pixels whose channels differ by more than 8 between `a` and `b` (same size).
fn differing(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
    let n = a
        .pixels()
        .zip(b.pixels())
        .filter(|(p, q)| p.0.iter().zip(q.0.iter()).any(|(x, y)| x.abs_diff(*y) > 8))
        .count();
    n as f64 / f64::from(a.width() * a.height())
}

/// Two captures show the same picture (at most 0.2 % of pixels off by more than 8).
fn same(a: &image::RgbaImage, b: &image::RgbaImage) -> bool {
    differing(a, b) <= 0.002
}

fn mean_luma(image: &image::RgbaImage) -> f64 {
    let sum: f64 = image
        .pixels()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum();
    sum / f64::from(image.width() * image.height())
}

/// P-075: the eight keyed view modes each differ from Lit, and the pane's mode draws exactly what
/// the capture's `debug_view` draws.
#[test]
fn view_modes_differ_from_lit_and_match_the_capture() {
    gpu_or_panic();
    let (mut app, _, _) = edit_app();
    let (lit, _) = capture(&mut app, json!({}));
    for mode in [
        "unlit",
        "wireframe_only",
        "lit_wireframe",
        "lighting_only",
        "light_complexity",
        "overdraw",
        "base_color",
    ] {
        let (by_param, _) = capture(&mut app, json!({ "debug_view": mode }));
        assert!(differing(&by_param, &lit) > 0.01, "{mode} draws like lit");
        let reply = call(&mut app, "editor.set_viewport", json!({ "view_mode": mode }));
        assert_eq!(reply["view_mode"], json!(mode), "{reply}");
        let (by_pane, info) = capture(&mut app, json!({}));
        assert_eq!(info["view_mode"], json!(mode), "{info}");
        assert!(
            same(&by_pane, &by_param),
            "{mode}: the pane's mode and the capture's debug_view draw differently ({})",
            differing(&by_pane, &by_param)
        );
    }
    // The renderer's own `wireframe` name stays the lit wireframe.
    call(&mut app, "editor.set_viewport", json!({ "view_mode": "lit" }));
    let (wire, _) = capture(&mut app, json!({ "debug_view": "wireframe" }));
    let (lit_wire, _) = capture(&mut app, json!({ "debug_view": "lit_wireframe" }));
    assert!(same(&wire, &lit_wire));
    let err = try_call(&mut app, "editor.set_viewport", json!({ "view_mode": "nope" }));
    assert!(err.is_err(), "{err:?}");
}

/// P-082: a fixed EV pins the pane's exposure (lower EV is brighter); the level band's centre is
/// the same picture as that EV fixed.
#[test]
fn exposure_menu_pins_the_pane_ev() {
    gpu_or_panic();
    let (mut app, _, _) = edit_app();
    let (auto, info) = capture(&mut app, json!({}));
    assert_eq!(info["exposure_ev"].as_f64(), Some(14.5), "{info}");
    call(&mut app, "editor.set_viewport", json!({ "exposure_ev": 11.0 }));
    let (fixed, info) = capture(&mut app, json!({}));
    assert_eq!(info["exposure_ev"].as_f64(), Some(11.0), "{info}");
    let (bright, base) = (mean_luma(&fixed), mean_luma(&auto));
    assert!(
        bright > base + 0.5 && differing(&fixed, &auto) > 0.005,
        "EV 11 is brighter than EV 14.5: mean luma {bright} vs {base}, {} of pixels differ",
        differing(&fixed, &auto)
    );
    app.world_mut()
        .insert_resource(regia_scene::level::LevelSettings {
            look: regia_scene::level::LevelLook {
                min_ev100: Some(9.0),
                max_ev100: Some(13.0),
                ..Default::default()
            },
            ..Default::default()
        });
    let reply = call(
        &mut app,
        "editor.set_viewport",
        json!({ "exposure": "level_band" }),
    );
    assert_eq!(reply["exposure"], json!("level_band"));
    let (band, info) = capture(&mut app, json!({}));
    assert_eq!(info["exposure_ev"].as_f64(), Some(11.0), "{info}");
    assert!(same(&band, &fixed), "the band centre is EV 11");
    call(&mut app, "editor.set_viewport", json!({ "exposure": "auto" }));
    let (again, _) = capture(&mut app, json!({}));
    assert!(same(&again, &auto), "auto draws as before");
}

/// P-080: the screen percentage scales the internal size, the picture keeps the pane's size.
#[test]
fn screen_percentage_scales_the_internal_size() {
    gpu_or_panic();
    let (mut app, _, _) = edit_app();
    let (full, info) = capture(&mut app, json!({}));
    assert_eq!(info["render_size"], json!([W, H]), "{info}");
    call(
        &mut app,
        "editor.set_viewport",
        json!({ "screen_percentage": 50 }),
    );
    let (half, info) = capture(&mut app, json!({}));
    assert_eq!(info["render_size"], json!([W / 2, H / 2]), "{info}");
    assert_eq!(info["screen_percentage"], json!(50));
    assert_eq!(
        (half.width(), half.height()),
        (W, H),
        "the picture keeps the pane size"
    );
    assert!(differing(&half, &full) > 0.0, "a half-scale render is softer");
    call(
        &mut app,
        "editor.set_viewport",
        json!({ "screen_percentage": 200 }),
    );
    let (double, info) = capture(&mut app, json!({}));
    assert_eq!(info["render_size"], json!([W * 2, H * 2]), "{info}");
    assert_eq!((double.width(), double.height()), (W, H));
    assert!(
        try_call(
            &mut app,
            "editor.set_viewport",
            json!({ "screen_percentage": 10 })
        )
        .is_err()
    );
}

/// P-078, P-081: the badges list realtime off and every changed pane setting; reset clears one or
/// all, and the realtime badge's click turns realtime back on.
#[test]
fn badges_list_changed_settings_and_reset_them() {
    let (mut app, _, _) = edit_app();
    let reply = call(&mut app, "editor.viewport_badges", json!({}));
    assert_eq!(reply["realtime_off"], json!(false));
    assert_eq!(reply["changed"], json!([]));
    call(
        &mut app,
        "editor.set_viewport",
        json!({ "realtime": false, "view_mode": "unlit", "screen_percentage": 50, "icon_filters": { "cameras": false } }),
    );
    let reply = call(&mut app, "editor.viewport_badges", json!({}));
    assert_eq!(reply["realtime_off"], json!(true));
    let names: Vec<&str> = reply["changed"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|c| c["setting"].as_str())
        .collect();
    assert_eq!(names, ["view_mode", "screen_percentage", "icons"]);
    let reply = call(
        &mut app,
        "editor.viewport_badges",
        json!({ "reset": "view_mode" }),
    );
    assert_eq!(reply["changed"].as_array().map(Vec::len), Some(2));
    let reply = call(
        &mut app,
        "editor.viewport_badges",
        json!({ "reset": "all", "restore_realtime": true }),
    );
    assert_eq!(reply["realtime_off"], json!(false));
    assert_eq!(reply["changed"], json!([]));
    assert_eq!(
        try_call(&mut app, "editor.viewport_badges", json!({ "reset": "nope" })),
        Err("viewport_badge_unknown_setting".to_owned())
    );
}

/// The pixel of `entity`'s centre in the perspective capture.
fn pixel_of(app: &App, entity: Entity) -> Vec2 {
    let camera = app
        .world()
        .resource::<regia_editor::viewport::EditorCamera>()
        .clone();
    let position = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    ViewCamera::of(ViewKind::Perspective, &camera)
        .project(position, Vec2::new(W as f32, H as f32))
        .expect("in view")
}

fn pick(app: &mut App, at: Vec2) -> Option<u64> {
    let reply = call(
        app,
        "editor.pick",
        json!({ "x": at.x, "y": at.y, "width": W, "height": H }),
    );
    reply["entity"].as_u64()
}

/// P-085, P-091: hiding and isolating never dirty the level or add an undo step, and picking
/// skips what they hide; showing again makes it pickable.
#[test]
fn hide_and_isolate_never_dirty_and_picking_skips_them() {
    gpu_or_panic();
    let (mut app, left, right) = edit_app();
    let at = pixel_of(&app, left);
    assert_eq!(pick(&mut app, at), Some(left.to_bits()), "the control");
    let reply = call(&mut app, "editor.hide", json!({ "entities": [left.to_bits()] }));
    assert_eq!(reply["changed"], json!(1));
    assert_ne!(
        pick(&mut app, at),
        Some(left.to_bits()),
        "a hidden entity is not picked"
    );
    let undo = app.world().resource::<UndoStack>();
    assert!(
        !undo.is_dirty() && undo.undo_descriptions().is_empty(),
        "hide is editor-only"
    );
    call(&mut app, "editor.hide", json!({ "all": true, "hidden": false }));
    assert_eq!(pick(&mut app, at), Some(left.to_bits()));
    // Isolate the right box: the left one hides; isolating again restores it.
    app.world_mut()
        .resource_mut::<Selection>()
        .select(right, regia_editor::selection::SelectMode::Replace);
    let reply = call(&mut app, "editor.isolate", json!({ "on": true }));
    assert_eq!(reply["isolating"], json!(true));
    assert_ne!(pick(&mut app, at), Some(left.to_bits()));
    let reply = call(&mut app, "editor.isolate", json!({}));
    assert_eq!(reply["isolating"], json!(false));
    assert_eq!(pick(&mut app, at), Some(left.to_bits()));
    let undo = app.world().resource::<UndoStack>();
    assert!(
        !undo.is_dirty() && undo.undo_descriptions().is_empty(),
        "isolate is editor-only"
    );
    app.world_mut().resource_mut::<Selection>().clear();
    assert_eq!(
        try_call(&mut app, "editor.isolate", json!({ "on": true })),
        Err("isolate_empty_selection".to_owned())
    );
}

/// P-076: Show ▸ Icons by kind hides a kind's icons in the pane, and the icon size scales them
/// about their anchor (CPU overlays, no GPU).
#[test]
fn icon_filters_hide_a_kind_and_scale_icons() {
    use regia_editor::overlays::{ICON_LIGHT, OverlayShape, build_overlays};
    use regia_editor::views::ShowFlags;
    let mut world = World::new();
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
        regia_render::PointLight::default(),
    ));
    let view = ViewCamera::of(
        ViewKind::Perspective,
        &regia_editor::viewport::EditorCamera::default(),
    );
    let size = Vec2::new(320.0, 240.0);
    let radius = |show: ShowFlags| {
        build_overlays(&world, &view, ViewKind::Perspective, size, show)
            .iter()
            .find_map(|s| match s {
                OverlayShape::Circle { radius, color, .. } if *color == ICON_LIGHT => Some(*radius),
                _ => None,
            })
    };
    let on = ShowFlags {
        grid: false,
        ..ShowFlags::default()
    };
    let base = radius(on).expect("the light icon draws");
    let mut big = on;
    big.icon_filter.size_percent = 200;
    let scaled = radius(big).expect("still drawn");
    assert!((scaled - 2.0 * base).abs() < 1e-3, "{scaled} vs {base}");
    let mut off = on;
    off.icon_filter.lights = false;
    assert_eq!(radius(off), None, "lights off hides the light icon");
    let mut cameras_off = on;
    cameras_off.icon_filter.cameras = false;
    assert_eq!(radius(cameras_off), Some(base), "another kind's switch leaves it");
}
