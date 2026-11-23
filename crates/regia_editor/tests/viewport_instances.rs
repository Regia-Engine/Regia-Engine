// SPDX-License-Identifier: MPL-2.0
//! Several independent viewports per world (Tincal engine request F2): two
//! [`ViewportInstance`]s drawn by [`viewport_panel_with`] in one egui frame render their own
//! cameras and quality presets without touching the main viewport's singletons; linked panes
//! move together and unlinking separates them; each instance captures headless; the agent
//! methods list, edit, link and capture instances.

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_editor::render::{PaneRequest, capture_instance, capture_pane};
use regia_editor::viewport::{EditorCamera, ViewportPanelState, viewport_panel_with};
use regia_editor::viewport_instance::{CameraLinkGroup, ViewportInstance, ViewportInstances};
use regia_editor::views::{ViewCamera, ViewKind, ViewportLayout, ViewportQuality};
use regia_render::RenderProfilePreset;
use regia_scene::prelude::Transform;
use serde_json::{Value, json};

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

/// Two proxy cubes either side of the origin (lit grey; the preview sun stands in).
fn world() -> World {
    let mut world = World::new();
    for x in [-1.5, 1.5] {
        world.spawn(Transform {
            translation: Vec3::new(x, 0.5, 0.0),
            ..Default::default()
        });
    }
    world
}

fn mean_abs_delta(a: &egui::ColorImage, b: &egui::ColorImage) -> f64 {
    assert_eq!(a.pixels.len(), b.pixels.len());
    let mut sum = 0u64;
    for (l, r) in a.pixels.iter().zip(&b.pixels) {
        sum += u64::from(l.r().abs_diff(r.r()))
            + u64::from(l.g().abs_diff(r.g()))
            + u64::from(l.b().abs_diff(r.b()));
    }
    sum as f64 / (a.pixels.len().max(1) as f64 * 3.0)
}

fn camera(yaw: f32) -> EditorCamera {
    EditorCamera {
        target: Vec3::new(0.0, 0.5, 0.0),
        distance: 7.0,
        yaw,
        pitch: 0.35,
        ..EditorCamera::default()
    }
}

/// A side-by-side host: instance `a` on the left half, `b` on the right, one egui frame.
struct SideBySide {
    ctx: egui::Context,
    states: [ViewportPanelState; 2],
}

impl SideBySide {
    const SIZE: egui::Vec2 = egui::vec2(640.0, 260.0);

    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            states: [ViewportPanelState::default(), ViewportPanelState::default()],
        }
    }

    fn frame(
        &mut self,
        world: &mut World,
        device: Option<&regia_render::RenderDevice>,
        views: [&mut ViewportInstance; 2],
        events: Vec<egui::Event>,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Self::SIZE)),
            events,
            ..Default::default()
        };
        let [state_a, state_b] = &mut self.states;
        let [view_a, view_b] = views;
        let mut output = self.ctx.run_ui(input, |ui| {
            let full = egui::Rect::from_min_size(egui::Pos2::ZERO, Self::SIZE);
            let half = egui::vec2(full.width() * 0.5, full.height());
            let left = egui::Rect::from_min_size(full.min, half);
            let right = egui::Rect::from_min_size(full.min + egui::vec2(half.x, 0.0), half);
            ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
                viewport_panel_with(ui, world, device, state_a, &mut *view_a);
            });
            ui.scope_builder(egui::UiBuilder::new().max_rect(right), |ui| {
                viewport_panel_with(ui, world, device, state_b, &mut *view_b);
            });
        });
        output.textures_delta.clear();
    }
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn two_instances_render_their_own_cameras_and_presets_in_one_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    let mut a = ViewportInstance {
        camera: camera(0.4),
        ..ViewportInstance::new(1)
    };
    let mut b = ViewportInstance {
        camera: camera(2.4),
        ..ViewportInstance::new(2)
    };
    b.layout.quality = ViewportQuality::Profile(RenderProfilePreset::DesktopLow);
    b.layout.game_view = true;

    let mut host = SideBySide::new();
    host.frame(&mut world, Some(&device), [&mut a, &mut b], Vec::new());

    let registry = world.resource::<ViewportInstances>();
    let (ra, rb) = (
        registry.get(1).expect("a drawn"),
        registry.get(2).expect("b drawn"),
    );
    assert!(registry.is_live(1) && registry.is_live(2));
    assert_eq!(ra.panes.len(), 1);
    assert_eq!(rb.panes.len(), 1);
    let (pa, pb) = (ra.panes[0], rb.panes[0]);
    assert!(
        pa.size[0] > 0 && pb.size[0] > 0,
        "both panes rendered: {pa:?} {pb:?}"
    );
    let want_a = ViewCamera::of(ViewKind::Perspective, &a.camera).eye;
    let want_b = ViewCamera::of(ViewKind::Perspective, &b.camera).eye;
    assert!(
        (pa.view.eye - want_a).length() < 1e-4,
        "{:?} vs {want_a:?}",
        pa.view.eye
    );
    assert!(
        (pb.view.eye - want_b).length() < 1e-4,
        "{:?} vs {want_b:?}",
        pb.view.eye
    );
    assert!((pa.view.eye - pb.view.eye).length() > 1.0, "different cameras");
    // Each pane drew at its own preset.
    let qa = ra.stats.last.as_ref().map(|info| info.quality);
    let qb = rb.stats.last.as_ref().map(|info| info.quality);
    assert_eq!(qa, Some("editor"));
    assert_eq!(qb, Some("desktop_low"));
    // The main viewport's singletons are untouched.
    assert!(world.get_resource::<EditorCamera>().is_none());
    assert!(world.get_resource::<ViewportLayout>().is_none());
}

#[test]
fn linked_panes_move_together_and_unlinking_separates_them() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut world = world();
    let group = CameraLinkGroup::new(5);
    let mut a = ViewportInstance {
        camera: camera(0.4),
        ..ViewportInstance::new(1)
    };
    let mut b = ViewportInstance {
        camera: camera(2.4),
        ..ViewportInstance::new(2)
    };
    a.link_to(&group);
    b.link_to(&group);
    assert_eq!(a.camera, b.camera, "joining adopts the group's camera");

    // No GPU needed: the panel's camera input and linking run without a device.
    let mut host = SideBySide::new();
    host.frame(&mut world, None, [&mut a, &mut b], Vec::new());

    // The host moves A (as a SpaceMouse or a script would): B follows in the same frame.
    a.camera.yaw += 0.8;
    host.frame(&mut world, None, [&mut a, &mut b], Vec::new());
    assert_eq!(a.camera, b.camera, "B follows a host-driven move of A");

    // The user orbits B with a right-button drag: A follows on the next frame.
    let at = egui::pos2(480.0, 140.0);
    let yaw_before = b.camera.yaw;
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    host.frame(
        &mut world,
        None,
        [&mut a, &mut b],
        vec![egui::Event::PointerMoved(at)],
    );
    host.frame(&mut world, None, [&mut a, &mut b], vec![button(true)]);
    for step in 1..=4 {
        let to = at + egui::vec2(15.0 * step as f32, 0.0);
        host.frame(
            &mut world,
            None,
            [&mut a, &mut b],
            vec![egui::Event::PointerMoved(to)],
        );
    }
    host.frame(&mut world, None, [&mut a, &mut b], vec![button(false)]);
    host.frame(&mut world, None, [&mut a, &mut b], Vec::new());
    assert!(
        (b.camera.yaw - yaw_before).abs() > 0.05,
        "the drag orbits B: {} -> {}",
        yaw_before,
        b.camera.yaw
    );
    assert_eq!(a.camera, b.camera, "A follows the drag in B");

    // Unlinked, B stays put while A moves.
    b.unlink();
    let b_before = b.camera.clone();
    a.camera.yaw -= 1.0;
    host.frame(&mut world, None, [&mut a, &mut b], Vec::new());
    assert_eq!(b.camera, b_before, "an unlinked pane keeps its camera");
    assert!((a.camera.yaw - b.camera.yaw).abs() > 0.5);
}

#[test]
fn each_instance_captures_its_own_view_and_the_main_viewport_is_unchanged() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut world = world();
    let size = [320, 240];
    let left = ViewportInstance {
        camera: camera(0.4),
        ..ViewportInstance::new(11)
    };
    let mut right = ViewportInstance {
        camera: camera(2.4),
        ..ViewportInstance::new(12)
    };
    let shot_left = capture_instance(&device, &mut world, &left, ViewKind::Perspective, size).expect("left");
    let shot_right =
        capture_instance(&device, &mut world, &right, ViewKind::Perspective, size).expect("right");
    assert!(
        mean_abs_delta(&shot_left.image, &shot_right.image) > 2.0,
        "different cameras give different pictures"
    );
    let grid = regia_editor::viewport::measure_viewport(&shot_right.image).grid_pixels;
    assert!(grid >= 40, "the grid is painted: {grid}");

    // The instance's own show flags apply to its capture only.
    right.layout.game_view = true;
    let hidden =
        capture_instance(&device, &mut world, &right, ViewKind::Perspective, size).expect("game view");
    assert_eq!(
        regia_editor::viewport::measure_viewport(&hidden.image).grid_pixels,
        0
    );

    // An instance mirroring the main viewport's singletons renders what the main viewport does.
    world.insert_resource(camera(0.4));
    let main = capture_pane(
        &device,
        &mut world,
        &PaneRequest {
            pane: 0,
            kind: ViewKind::Perspective,
            camera: camera(0.4),
            width: size[0],
            height: size[1],
            ids: true,
            warm_frames: 0,
            debug_view: regia_render::DebugViewMode::Lit,
            reference: None,
        },
    )
    .expect("main");
    let mirror = ViewportInstance::from_world(&world, 13);
    let mirrored =
        capture_instance(&device, &mut world, &mirror, ViewKind::Perspective, size).expect("mirror");
    let delta = mean_abs_delta(&main.image, &mirrored.image);
    assert!(delta < 0.5, "the instance path matches the main path: {delta}");
}

fn call(world: &mut World, method: &str, params: Value) -> Value {
    let (_, handler) = regia_editor::agent_ext::all_methods()
        .into_iter()
        .find(|(schema, _)| schema.name == method)
        .unwrap_or_else(|| panic!("{method} is registered"));
    match handler(world, &params) {
        Ok(value) => value,
        Err(err) => panic!("{method}: {err:?}"),
    }
}

#[test]
fn agent_methods_list_edit_link_capture_and_remove_instances() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut world = world();
    let a = ViewportInstance {
        camera: camera(0.4),
        ..ViewportInstance::new(1)
    };
    ViewportInstances::publish(&mut world, &a);
    call(
        &mut world,
        "editor.viewport_instance_set",
        json!({ "id": 2, "create": true, "yaw": 2.4, "distance": 7.0, "target": [0.0, 0.5, 0.0], "quality": "desktop_low" }),
    );
    let listed = call(&mut world, "editor.viewport_instances", json!({}));
    let ids: Vec<u64> = listed["instances"]
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|i| i["id"].as_u64())
        .collect();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(listed["instances"][1]["quality"], "desktop_low");

    // Link both: 2 adopts 1's camera; moving 2 moves 1's snapshot too.
    call(&mut world, "editor.viewport_link", json!({ "id": 1, "group": 9 }));
    let linked = call(&mut world, "editor.viewport_link", json!({ "id": 2, "group": 9 }));
    assert_eq!(linked["link"], 9);
    assert_eq!(linked["camera"]["yaw"].as_f64(), Some(f64::from(0.4f32)));
    call(
        &mut world,
        "editor.viewport_instance_set",
        json!({ "id": 2, "yaw": 1.0 }),
    );

    // The panel side picks the agent's edits up on its next draw.
    let mut host_a = a.clone();
    assert!(ViewportInstances::take_agent_edits(&mut world, &mut host_a));
    assert_eq!(host_a.link.as_ref().map(CameraLinkGroup::id), Some(9));
    host_a.sync_link();
    assert!(
        (host_a.camera.yaw - 1.0).abs() < 1e-6,
        "1 follows 2: {}",
        host_a.camera.yaw
    );

    // Unlink 2, move it alone.
    let unlinked = call(
        &mut world,
        "editor.viewport_link",
        json!({ "id": 2, "group": null }),
    );
    assert!(unlinked["link"].is_null());

    // Captures need a GPU adapter (lavapipe in CI).
    let shot = call(
        &mut world,
        "editor.capture_viewport_instance",
        json!({ "id": 2, "width": 160, "height": 120 }),
    );
    assert_eq!(shot["mime"], "image/png");
    assert_eq!(shot["quality"], "desktop_low");
    let want = ViewCamera::of(ViewKind::Perspective, &camera(1.0)).eye;
    let eye: Vec<f64> = shot["eye"]
        .as_array()
        .expect("eye")
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    assert!(
        (eye[0] as f32 - want.x).abs() < 1e-3 && (eye[2] as f32 - want.z).abs() < 1e-3,
        "{eye:?} vs {want:?}"
    );

    let removed = call(&mut world, "editor.viewport_instance_remove", json!({ "id": 2 }));
    assert_eq!(removed["removed"], true);
    let listed = call(&mut world, "editor.viewport_instances", json!({}));
    assert_eq!(listed["instances"].as_array().map(Vec::len), Some(1));
}
