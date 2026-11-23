// SPDX-License-Identifier: MPL-2.0
//! Viewport Show flags and the navigation preview (lane w9-viewport-flags-nav, ADR-7593;
//! `book/agents/viewport-flags.md`). No golden: every image check diffs a capture against a
//! control capture of the same scene with the flag off (lavapipe).
//!
//! - `editor.nav_preview { action: "build" }` bakes the level's static colliders into an
//!   editor-only `regia_nav::NavMesh` (polys > 0); the level file's bytes do not change;
//!   `clear` removes it. Show ▸ Navigation then changes pixels over the floor.
//! - Show ▸ Post volumes (`editor.set_viewport { post_volumes }`) outlines every post volume and
//!   changes pixels only over the volume.
//! - The Colliders, Light radii and Post volumes toggles round-trip through `editor.set_viewport`.

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_editor::command::UndoStack;
use regia_editor::selection::Selection;
use regia_editor::views::{ViewCamera, ViewKind, ViewportLayout};
use regia_scene::GlobalTransform;
use regia_scene::prelude::Transform;
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
        .insert_resource(regia_editor::viewport::EditorCamera::default());
    let open = regia_editor::level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    regia_editor::agent_ext::install(app.world_mut());
    app.update();
    app
}

/// Calls an editor agent method; `Err` carries the error's `reason` (or message).
fn try_call(app: &mut App, name: &str, params: Value) -> Result<Value, String> {
    let (_, handler) = app
        .world()
        .resource::<regia_agent::extension::AgentExtension>()
        .method(name)
        .unwrap_or_else(|| panic!("{name} is not registered"));
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

/// The capture's pixels as RGBA8.
fn capture(app: &mut App) -> image::RgbaImage {
    let reply = call(app, "editor.capture_viewport", json!({ "width": W, "height": H }));
    let png =
        regia_editor::reference::base64_decode(reply["png_base64"].as_str().expect("png")).expect("base64");
    image::load_from_memory(&png).expect("png decodes").to_rgba8()
}

fn gpu_or_panic() {
    if regia_render::RenderDevice::headless().is_err() {
        panic!("no GPU adapter (install Mesa lavapipe or use WARP)");
    }
}

/// A static box collider (a static `RigidBody` on its own entity, as the level's physics and the
/// streaming cook take it).
fn static_box(world: &mut World, center: Vec3, half: Vec3) -> Entity {
    let transform = Transform::from_translation(center);
    world
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box { half_extents: half },
                ..regia_physics::Collider::default()
            },
        ))
        .id()
}

/// The floor-plus-box level: a 12 m static floor slab with its top at y = 0 and a 2 m static box
/// standing on it.
fn floor_plus_box(world: &mut World) {
    static_box(world, Vec3::new(0.0, -0.1, 0.0), Vec3::new(6.0, 0.1, 6.0));
    static_box(world, Vec3::new(2.0, 1.0, 0.0), Vec3::splat(1.0));
}

fn saved_bytes(app: &mut App, path: &std::path::Path) -> Vec<u8> {
    let open = app
        .world()
        .resource::<regia_editor::level_io::OpenLevel>()
        .clone();
    regia_editor::level_io::save_level_file(app.world_mut(), &open, path).expect("save");
    std::fs::read(path).expect("read the saved level")
}

#[test]
fn nav_preview_bakes_static_colliders_and_never_reaches_the_level_file() {
    let mut app = edit_app();
    floor_plus_box(app.world_mut());
    app.update();
    let dir = regia_test_utils::unique_temp_dir("editor_nav_preview_save");
    let path = dir.join("nav.level.ron");
    let before = saved_bytes(&mut app, &path);
    let undo_before = app.world().resource::<UndoStack>().undo_depth();

    let built = call(&mut app, "editor.nav_preview", json!({ "action": "build" }));
    let polys = built["polys"].as_u64().expect("polys");
    assert!(polys > 0, "the floor-plus-box level bakes: {built}");
    assert_eq!(built["active"], true, "{built}");
    assert!(
        built["triangles"].as_u64().expect("triangles") >= 24,
        "two boxes: {built}"
    );
    let mesh = app
        .world()
        .get_resource::<regia_nav::NavMesh>()
        .expect("the preview mesh is the NavMesh resource Show ▸ Navigation reads");
    assert_eq!(mesh.polys.len() as u64, polys);
    // Some polygon lies on top of the box (y = 2) and some on the floor (y = 0).
    let heights: Vec<f32> = mesh
        .polys
        .iter()
        .flat_map(|p| p.vertices.iter().map(|v| v.y))
        .collect();
    assert!(
        heights.iter().any(|y| y.abs() < 0.3),
        "floor polygons: {heights:?}"
    );
    assert!(
        heights.iter().any(|y| (y - 2.0).abs() < 0.3),
        "box-top polygons: {heights:?}"
    );
    assert_eq!(
        app.world().resource::<UndoStack>().undo_depth(),
        undo_before,
        "the preview is not an edit"
    );

    let after = saved_bytes(&mut app, &path);
    assert!(
        before == after,
        "the saved level bytes are unchanged by the preview ({} vs {} bytes)",
        before.len(),
        after.len()
    );

    // Reading the state does not bake.
    let state = call(&mut app, "editor.nav_preview", json!({}));
    assert_eq!(state["active"], true, "{state}");
    assert_eq!(state["polys"], json!(polys), "{state}");

    let cleared = call(&mut app, "editor.nav_preview", json!({ "action": "clear" }));
    assert_eq!(cleared["active"], false, "{cleared}");
    assert_eq!(cleared["polys"], json!(0), "{cleared}");
    assert!(app.world().get_resource::<regia_nav::NavMesh>().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nav_preview_refusals() {
    let mut app = edit_app();
    // No static collider: nothing to bake.
    let err = try_call(&mut app, "editor.nav_preview", json!({ "action": "build" })).expect_err("empty");
    assert_eq!(err, "nav_preview_no_geometry");
    // An unknown action.
    let err = try_call(&mut app, "editor.nav_preview", json!({ "action": "bake" })).expect_err("unknown");
    assert_eq!(err, "nav_preview_unknown_action");
    // A NavMesh the game inserted is never replaced or removed by the preview.
    floor_plus_box(app.world_mut());
    let game = regia_nav::NavMeshBuilder::new(regia_nav::NavMeshBuildParams::default())
        .build(&[[
            Vec3::new(-1.0, 0.0, -1.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(1.0, 0.0, -1.0),
        ]])
        .expect("game mesh");
    let game_polys = game.polys.len();
    app.world_mut().insert_resource(game);
    let err = try_call(&mut app, "editor.nav_preview", json!({ "action": "build" })).expect_err("game mesh");
    assert_eq!(err, "nav_preview_game_mesh");
    let err = try_call(&mut app, "editor.nav_preview", json!({ "action": "clear" })).expect_err("game mesh");
    assert_eq!(err, "nav_preview_game_mesh");
    assert_eq!(
        app.world().resource::<regia_nav::NavMesh>().polys.len(),
        game_polys,
        "the game's mesh is untouched"
    );
}

#[test]
fn nav_preview_then_navigation_flag_changes_pixels() {
    gpu_or_panic();
    let mut app = edit_app();
    floor_plus_box(app.world_mut());
    app.world_mut().resource_mut::<Selection>().clear();
    app.update();
    let built = call(&mut app, "editor.nav_preview", json!({ "action": "build" }));
    assert!(built["polys"].as_u64().unwrap_or(0) > 0, "{built}");

    call(
        &mut app,
        "editor.set_viewport",
        json!({ "grid": false, "navigation": false }),
    );
    let control = capture(&mut app);
    let flags = call(
        &mut app,
        "editor.set_viewport",
        json!({ "grid": false, "navigation": true }),
    );
    assert_eq!(flags["navigation"], true);
    let shown = capture(&mut app);
    let changed = shown
        .pixels()
        .zip(control.pixels())
        .filter(|(a, b)| a.0 != b.0)
        .count();
    eprintln!("navigation preview: {changed} pixels changed");
    assert!(
        changed > 60,
        "the preview's polygons draw: {changed} pixels changed"
    );
}

/// The shape color of a priority-0 post volume (`regia_render::post_volume_gizmos::PRIORITY_COLORS[0]`).
fn priority0_rgb() -> [u8; 3] {
    let c = regia_render::post_volume_gizmos::priority_color(0);
    let to = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [to(c.r), to(c.g), to(c.b)]
}

#[test]
fn post_volume_flag_changes_pixels_only_over_the_volume() {
    gpu_or_panic();
    let mut app = edit_app();
    let center = Vec3::new(0.0, 1.0, 0.0);
    let half = [1.5_f32, 1.0, 1.5];
    let blend = 0.5_f32;
    let transform = Transform::from_translation(center);
    app.world_mut().spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        regia_render::PostVolume {
            shape: regia_render::PostVolumeShape::Box { half_extents: half },
            blend_distance: blend,
            ..regia_render::PostVolume::default()
        },
    ));
    app.world_mut().resource_mut::<Selection>().clear();
    app.update();

    let flags = call(&mut app, "editor.set_viewport", json!({ "grid": false }));
    assert_eq!(flags["post_volumes"], false, "off by default: {flags}");
    let control = capture(&mut app);
    let flags = call(
        &mut app,
        "editor.set_viewport",
        json!({ "grid": false, "post_volumes": true }),
    );
    assert_eq!(flags["post_volumes"], true, "{flags}");
    let shown = capture(&mut app);

    // The screen rectangle of the band's box (shape grown by the blend distance), plus a margin.
    let camera = app
        .world()
        .resource::<regia_editor::viewport::EditorCamera>()
        .clone();
    let view = ViewCamera::of(ViewKind::Perspective, &camera);
    let size = Vec2::new(W as f32, H as f32);
    let reach = Vec3::from_array(half) + Vec3::splat(blend);
    let mut projected = Vec::new();
    for sx in [-1.0, 1.0] {
        for sy in [-1.0, 1.0] {
            for sz in [-1.0, 1.0] {
                let corner = center + reach * Vec3::new(sx, sy, sz);
                projected.push(view.project(corner, size).expect("the volume is in view"));
            }
        }
    }
    let min = projected.iter().fold(Vec2::splat(f32::MAX), |m, p| m.min(*p)) - Vec2::splat(4.0);
    let max = projected.iter().fold(Vec2::splat(f32::MIN), |m, p| m.max(*p)) + Vec2::splat(4.0);
    let over = |x: u32, y: u32| {
        let p = Vec2::new(x as f32, y as f32);
        p.x >= min.x && p.y >= min.y && p.x <= max.x && p.y <= max.y
    };
    let rgb = priority0_rgb();
    let (mut changed_over, mut changed_elsewhere, mut colored, mut colored_outside) = (0, 0, 0, 0);
    for ((x, y, on), off) in shown.enumerate_pixels().zip(control.pixels()) {
        if on.0 != off.0 {
            if over(x, y) {
                changed_over += 1;
            } else {
                changed_elsewhere += 1;
            }
        }
        if on.0[..3] == rgb {
            colored += 1;
            if !over(x, y) {
                colored_outside += 1;
            }
        }
    }
    eprintln!(
        "post volumes: {changed_over} changed over, {changed_elsewhere} elsewhere, {colored} in the shape color, \
         {colored_outside} of them outside"
    );
    assert!(
        changed_over > 60,
        "the outline draws over the volume: {changed_over}"
    );
    assert!(colored > 30, "in the priority color: {colored}");
    assert_eq!(colored_outside, 0, "no outline pixel lies away from the volume");
    // Two renders of one scene may differ by a few pixels of frame-to-frame noise (the capture
    // tests' rule): the change away from the volume stays a small fraction of the change over it.
    assert!(
        changed_elsewhere * 10 < changed_over,
        "the change is over the volume: {changed_elsewhere} elsewhere vs {changed_over}"
    );
    assert_eq!(
        control.pixels().filter(|p| p.0[..3] == rgb).count(),
        0,
        "the control has none"
    );
}

#[test]
fn show_flag_toggles_round_trip_through_the_agent() {
    let mut app = edit_app();
    let read = call(&mut app, "editor.set_viewport", json!({}));
    for name in ["colliders", "light_radii", "post_volumes", "navigation"] {
        assert_eq!(read[name], false, "{name} is off by default: {read}");
        for value in [true, false] {
            let out = call(&mut app, "editor.set_viewport", json!({ name: value }));
            assert_eq!(out[name], value, "{name} = {value}: {out}");
            let again = call(&mut app, "editor.set_viewport", json!({}));
            assert_eq!(again[name], value, "the layout holds {name} = {value}: {again}");
            assert!(app.world().contains_resource::<ViewportLayout>());
        }
    }
    // Each Show menu entry is a registered action whose agent path is editor.set_viewport.
    for id in [
        "viewport.show_colliders",
        "viewport.show_light_radii",
        "viewport.show_post_volumes",
        "viewport.show_navigation",
    ] {
        let action = regia_editor::agent_parity::find(id).unwrap_or_else(|| panic!("{id} is registered"));
        assert_eq!(
            action.path,
            regia_editor::agent_parity::ActionPath::Method("editor.set_viewport"),
            "{id}"
        );
    }
    for id in ["viewport.nav_preview_build", "viewport.nav_preview_clear"] {
        let action = regia_editor::agent_parity::find(id).unwrap_or_else(|| panic!("{id} is registered"));
        assert_eq!(
            action.path,
            regia_editor::agent_parity::ActionPath::Method("editor.nav_preview"),
            "{id}"
        );
    }
}

/// ADR-7664: `editor.nav_preview { action: "build", async: true }` answers at once with the bake's
/// job (`building` names it); the editor app's `Last` tick (`editor_jobs::tick_system`, which the
/// editor binary registers) puts the mesh in place once the worker returns, and the job ends
/// `done`. A second build while it runs is `nav_preview_busy`.
#[test]
fn nav_preview_async_build_ends_through_the_frame_tick() {
    let mut app = edit_app();
    app.add_systems(Last, regia_editor::editor_jobs::tick_system);
    floor_plus_box(app.world_mut());
    let out = call(
        &mut app,
        "editor.nav_preview",
        json!({ "action": "build", "async": true }),
    );
    let job = out["job"].as_str().expect("job id").to_string();
    assert!(job.starts_with("nav_preview:"), "{out}");
    assert_eq!(out["building"], json!(job), "{out}");
    assert_eq!(out["active"], false, "the bake has not ended: {out}");
    let err = try_call(&mut app, "editor.nav_preview", json!({ "action": "build" })).expect_err("busy");
    assert_eq!(err, "nav_preview_busy");
    app.world_mut()
        .resource_mut::<regia_editor::nav_preview::NavPreviewJob>()
        .job
        .wait();
    app.update();
    let state = app
        .world()
        .resource::<regia_editor::editor_jobs::EditorJobs>()
        .get(&job)
        .map(regia_agent::jobs::JobHandle::state)
        .expect("registered");
    assert_eq!(state, regia_agent::jobs::JobState::Done);
    let out = call(&mut app, "editor.nav_preview", json!({}));
    assert_eq!(out["active"], true, "{out}");
    assert_eq!(out["building"], Value::Null, "{out}");
    assert!(out["polys"].as_u64().is_some_and(|p| p > 0), "{out}");
    assert_eq!(out["navigation"], true, "{out}");
}

#[test]
fn w12_paths_flag_round_trips() {
    let mut app = edit_app();
    let out = call(&mut app, "editor.set_viewport", json!({"paths": true}));
    assert_eq!(out["paths"], true);
    let out = call(&mut app, "editor.set_viewport", json!({"paths": false}));
    assert_eq!(out["paths"], false);
}

#[test]
fn w12_debug_colors_persist_and_undo_through_dispatch() {
    let mut app = edit_app();
    let root = regia_test_utils::unique_temp_dir("w12-debug-colors");
    std::fs::write(
        root.join("Regia.toml"),
        "[project]\nname = 'test'\n[custom]\nkeep = 17\n",
    )
    .expect("manifest");
    app.world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
    let before = std::fs::read(root.join("Regia.toml")).expect("read");
    let out = call(
        &mut app,
        "editor.debug_colors",
        json!({"op": "set", "colors": {"path": [0.25, 0.5, 0.75]}}),
    );
    assert_eq!(out["colors"]["path"], json!([0.25, 0.5, 0.75]));
    let mut fresh = edit_app();
    fresh
        .world_mut()
        .insert_resource(regia_editor::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
    let out = call(&mut fresh, "editor.debug_colors", json!({"op": "get"}));
    assert_eq!(out["colors"]["path"], json!([0.25, 0.5, 0.75]));
    call(&mut app, "editor.undo", json!({}));
    assert_eq!(std::fs::read(root.join("Regia.toml")).expect("read"), before);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn w12_debug_and_vfx_operations_cross_the_real_dispatcher() {
    use regia_agent::protocol::Request;
    use regia_agent::server::Inbound;
    let root = regia_test_utils::unique_temp_dir("w12-dispatcher");
    std::fs::write(root.join("Regia.toml"), "[project]\nname='dispatch'\n").expect("manifest");
    std::fs::write(
        root.join("effect.vfx.ron"),
        include_str!("../../regia_vfx/tests/fixtures/muzzle_flash.vfx.ron"),
    )
    .expect("effect");
    let mut app = edit_app();
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.clone()),
    });
    app.add_plugins(regia_agent::AgentPlugin);
    app.insert_resource(regia_editor::agent_ext::EditorProject {
        root: Some(root.clone()),
    });
    app.finish_plugins();
    let (send, inbox) = crossbeam_channel::unbounded::<Inbound>();
    app.insert_resource(regia_agent::AgentInbox(inbox));
    let mut id = 0;
    let mut dispatch = |method: &str, params: Value| {
        id += 1;
        let (reply, response) = crossbeam_channel::bounded(1);
        send.send(Inbound {
            request: Request {
                jsonrpc: "2.0".into(),
                id: Some(json!(id)),
                method: method.into(),
                params,
            },
            reply,
        })
        .expect("inbox");
        app.update();
        let response = response.try_recv().expect("one frame response");
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.expect("result")
    };
    let palette = dispatch(
        "editor.debug_colors",
        json!({"op":"set", "colors":{"path":[0.25,0.5,0.75]}}),
    );
    assert_eq!(palette["colors"]["path"], json!([0.25, 0.5, 0.75]));
    assert_eq!(
        dispatch("editor.set_viewport", json!({"paths":true}))["paths"],
        true
    );
    dispatch("editor.vfx_open", json!({"path":"effect.vfx.ron"}));
    let state = dispatch("editor.vfx_state", json!({"bounds_time":0.1, "seed":1}));
    assert!(state["timeline"].is_array());
    assert_eq!(state["bounds"]["mode"], "computed");
    dispatch(
        "editor.vfx_edit",
        json!({"op":"preview_emitter","emitter":0,"enabled":false,"solo":true}),
    );
    assert_eq!(
        dispatch("editor.vfx_edit", json!({"op":"scrub","time":0.5}))["preview_time"],
        0.5
    );
    drop(app);
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn w12_possessed_capture_obeys_paths_and_colliders_without_goldens() {
    use regia_editor::overlays::paint_capture_overlays_for;
    use regia_editor::render::{PaneFrame, PaneInfo};
    let mut app = edit_app();
    static_box(app.world_mut(), Vec3::ZERO, Vec3::splat(0.5));
    app.world_mut().spawn((
        GlobalTransform::IDENTITY,
        regia_nav::NavAgentState {
            waypoints: vec![Vec3::splat(-50.0), Vec3::X * 2.0, Vec3::new(2.0, 0.0, 2.0)],
            next_waypoint: 1,
            ..Default::default()
        },
    ));
    let view = ViewCamera::of(
        ViewKind::Perspective,
        &regia_editor::viewport::EditorCamera::default(),
    );
    let make = || PaneFrame {
        image: egui::ColorImage::filled([320, 240], egui::Color32::TRANSPARENT),
        view,
        ids: None,
        info: PaneInfo {
            possessed: true,
            ..Default::default()
        },
    };
    let mut layout = ViewportLayout::default();
    let mut control = make();
    paint_capture_overlays_for(&mut control, app.world(), ViewKind::Perspective, &layout, false);
    let mut on = make();
    layout.show.paths = true;
    paint_capture_overlays_for(&mut on, app.world(), ViewKind::Perspective, &layout, false);
    assert!(
        on.image
            .pixels
            .iter()
            .zip(&control.image.pixels)
            .any(|(a, b)| a != b),
        "paths draw while possessed"
    );
    layout.show.paths = false;
    layout.show.colliders = true;
    let mut collider = make();
    paint_capture_overlays_for(&mut collider, app.world(), ViewKind::Perspective, &layout, false);
    assert!(
        collider
            .image
            .pixels
            .iter()
            .zip(&control.image.pixels)
            .any(|(a, b)| a != b),
        "colliders draw while possessed"
    );
    layout.game_view = true;
    let mut hidden = make();
    paint_capture_overlays_for(&mut hidden, app.world(), ViewKind::Perspective, &layout, false);
    assert_eq!(
        hidden.image.pixels, control.image.pixels,
        "Game view still hides debug"
    );
}
