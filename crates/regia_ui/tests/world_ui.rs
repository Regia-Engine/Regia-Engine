// SPDX-License-Identifier: MPL-2.0
//! Camera-target and world-space UI (Y13/Y14, ADR-2406): extraction groups roots per target, a
//! deterministic ray clicks the expected widget of a world panel, a camera-target root draws into
//! its camera's texture, a world panel composites depth-tested into the frame, and frames without
//! camera or world UI are byte-identical to frames without the passes.
//!
//! The GPU tests run on any headless adapter (lavapipe is the reference) and need no golden
//! image: every check compares two frames of this test with each other or with a known colour.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use regia_app::prelude::*;
use regia_core::glam::{Affine3A, Vec3};
use regia_render::{
    Camera, DirectionalLightUniform, FramePass, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    PostSettings, RenderDevice, RenderTarget, TextureViewSettings,
};
use regia_scene::GlobalTransform;
use regia_ui::prelude::*;
use regia_ui::render::SharedUiRenderer;
use regia_ui::world_target::{CameraTargetPass, UiTargetKey, WorldPanel, WorldPanelPass};
use regia_ui::{BackgroundKind, UiQuad, UiTarget, UiTargetNode};
use serde_json::json;

static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn ui_app() -> App {
    let mut app = App::new();
    app.add_plugins(UiPlugin::code_only());
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app
}

fn color(rgba: [f32; 4]) -> Background {
    Background {
        kind: BackgroundKind::Color(UiColor(rgba)),
    }
}

fn full() -> NodeStyle {
    NodeStyle::new(Style {
        size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
        ..Default::default()
    })
}

/// A root with one full-size child of `rgba`; returns `(root, child)`.
fn root_with_fill(world: &mut World, root: UiRoot, rgba: [f32; 4]) -> (Entity, Entity) {
    let root = world.spawn((root, full())).id();
    let child = world.spawn((UiNode, full(), color(rgba))).id();
    world.entity_mut(root).add_child(child);
    (root, child)
}

fn has_color(quads: &[UiQuad], rgba: [f32; 4]) -> bool {
    quads.iter().any(|q| q.color.0 == rgba)
}

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

#[test]
fn extraction_groups_roots_per_target() {
    let mut app = ui_app();
    let world = app.world_mut();
    root_with_fill(world, UiRoot::default(), WHITE);
    let (panel, _) = root_with_fill(world, UiRoot::world_space([200.0, 100.0], 100.0), RED);
    world
        .entity_mut(panel)
        .insert(GlobalTransform(Affine3A::IDENTITY));
    let screen = world
        .spawn((
            Camera {
                target: RenderTarget::Texture { asset_id: 7 },
                ..Camera::default()
            },
            TextureViewSettings {
                size: [64, 32],
                ..TextureViewSettings::default()
            },
        ))
        .id();
    let (monitor, monitor_fill) = root_with_fill(world, UiRoot::camera(screen), GREEN);
    let split = world
        .spawn(Camera {
            viewport: [0.5, 0.0, 0.5, 1.0],
            ..Camera::default()
        })
        .id();
    let (half, _) = root_with_fill(world, UiRoot::camera(split), BLUE);
    for _ in 0..3 {
        app.update();
    }
    let world = app.world();
    let ui = world.resource::<SharedUiRenderer>().lock();
    assert_eq!(
        ui.target_keys(),
        vec![UiTargetKey::CameraTexture(7), UiTargetKey::World(panel.to_bits())]
    );
    let window = ui.quads();
    assert!(has_color(window, WHITE), "the window root draws to the window");
    assert!(!has_color(window, RED), "the world panel left the window list");
    assert!(
        !has_color(window, GREEN),
        "the texture camera's root left the window list"
    );
    let world_list = ui
        .target_quads(UiTargetKey::World(panel.to_bits()))
        .expect("panel list");
    assert!(has_color(world_list, RED) && !has_color(world_list, WHITE));
    let camera_list = ui
        .target_quads(UiTargetKey::CameraTexture(7))
        .expect("camera list");
    assert!(has_color(camera_list, GREEN) && !has_color(camera_list, RED));
    // The texture camera's root is laid out at the texture's size.
    let fill = world.get::<ComputedNode>(monitor_fill).expect("laid out");
    assert_eq!([fill.rect[2], fill.rect[3]], [64.0, 32.0]);
    // A window camera's root stays in the window list, offset to its viewport.
    let window_size = world.resource::<regia_ui::UiScales>().window_size;
    let blue = window
        .iter()
        .find(|q| q.color.0 == BLUE)
        .expect("window camera's root in the window list");
    assert!(
        (blue.pos[0] - (window_size[0] * 0.5).round()).abs() < 0.5,
        "{:?}",
        blue.pos
    );
    assert!(
        (blue.size[0] - (window_size[0] * 0.5).round()).abs() < 0.5,
        "{:?}",
        blue.size
    );
    // Pointer routing: texture and world nodes are off the window; the split root is offset.
    assert_eq!(
        world.get::<UiTargetNode>(monitor).map(|t| t.window_origin),
        Some(None)
    );
    assert_eq!(
        world.get::<UiTargetNode>(half).and_then(|t| t.window_origin),
        Some([(window_size[0] * 0.5).round(), 0.0])
    );
}

#[test]
fn a_window_only_ui_has_no_targets_and_no_tags() {
    let mut app = ui_app();
    let (root, child) = root_with_fill(app.world_mut(), UiRoot::default(), WHITE);
    for _ in 0..3 {
        app.update();
    }
    let world = app.world();
    assert!(
        world
            .resource::<SharedUiRenderer>()
            .lock()
            .target_keys()
            .is_empty()
    );
    assert!(world.get::<UiTargetNode>(root).is_none());
    assert!(world.get::<UiTargetNode>(child).is_none());
    assert!(has_color(
        world.resource::<SharedUiRenderer>().lock().quads(),
        WHITE
    ));
}

#[test]
fn moving_a_root_between_targets_moves_its_quads() {
    let mut app = ui_app();
    let (root, _) = root_with_fill(app.world_mut(), UiRoot::default(), RED);
    app.world_mut()
        .entity_mut(root)
        .insert(GlobalTransform(Affine3A::IDENTITY));
    app.update();
    let to_world = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.set_target",
        &json!({ "root": root.to_bits(), "target": { "world": { "size": [128, 64], "ppm": 64, "billboard": "axis_y" } } }),
    )
    .expect("set_target");
    assert_eq!(to_world["target"], "world");
    assert_eq!(to_world["billboard"], "axis_y");
    for _ in 0..2 {
        app.update();
    }
    {
        let ui = app.world().resource::<SharedUiRenderer>().lock();
        assert!(!has_color(ui.quads(), RED));
        assert!(has_color(
            ui.target_quads(UiTargetKey::World(root.to_bits())).expect("list"),
            RED
        ));
    }
    regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.set_target",
        &json!({ "root": root.to_bits(), "target": "window" }),
    )
    .expect("back to the window");
    for _ in 0..2 {
        app.update();
    }
    let ui = app.world().resource::<SharedUiRenderer>().lock();
    assert!(has_color(ui.quads(), RED));
    assert!(ui.target_keys().is_empty());
    drop(ui);
    let bad = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.set_target",
        &json!({ "root": root.to_bits(), "target": { "world": { "size": [0, 64], "ppm": 64 } } }),
    )
    .expect_err("zero size");
    assert_eq!(bad.reason, "ui_bad_target");
}

/// A 2 m × 1 m panel (400 × 200 px at 200 px/m) 2 m in front of the origin, facing +Z, with an
/// `ok` button on its left half and a `cancel` button on its right half.
fn panel_with_buttons(app: &mut App) -> (Entity, Entity, Entity) {
    let world = app.world_mut();
    let root = world
        .spawn((
            UiRoot::world_space([400.0, 200.0], 200.0),
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(100.0)],
                direction: regia_ui::style::FlexDirection::Row,
                ..Default::default()
            }),
            GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 1.0, -2.0))),
        ))
        .id();
    let button = |world: &mut World, name: &str| {
        world
            .spawn((
                UiNode,
                NodeStyle::new(Style {
                    size: [Dimension::Percent(50.0), Dimension::Percent(100.0)],
                    ..Default::default()
                }),
                Interactable::default(),
                regia_ui::NodeName(name.to_owned()),
                color(WHITE),
            ))
            .id()
    };
    let ok = button(world, "ok");
    let cancel = button(world, "cancel");
    world.entity_mut(root).add_children(&[ok, cancel]);
    for _ in 0..3 {
        app.update();
    }
    (root, ok, cancel)
}

fn clicks(app: &mut App) -> Vec<(Entity, UiEventKind)> {
    app.world_mut()
        .resource_mut::<Messages<UiEvent>>()
        .iter_current_update_messages()
        .filter(|e| matches!(e.kind, UiEventKind::Click))
        .map(|e| (e.target, e.kind.clone()))
        .collect()
}

#[test]
fn a_deterministic_ray_clicks_the_expected_widget_on_a_world_panel() {
    let mut app = ui_app();
    let (root, ok, cancel) = panel_with_buttons(&mut app);
    // From 0.5 m right of the centre, straight at the panel: u = 0.75, v = 0.5.
    let hit = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "origin": [0.5, 1.0, 0.0], "direction": [0.0, 0.0, -1.0] }),
    )
    .expect("world click");
    assert_eq!(hit["root"], root.to_bits());
    assert_eq!(hit["node"], cancel.to_bits());
    assert_eq!(hit["clicked"], true);
    assert!((hit["distance"].as_f64().unwrap() - 2.0).abs() < 1e-4);
    assert!((hit["uv"][0].as_f64().unwrap() - 0.75).abs() < 1e-4);
    assert!((hit["px"][0].as_f64().unwrap() - 300.0).abs() < 1e-2);
    assert_eq!(clicks(&mut app), vec![(cancel, UiEventKind::Click)]);

    // The same ray twice gives the same answer; the left half is `ok`.
    let again = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "ray": { "origin": [-0.5, 1.0, 0.0], "direction": [0.0, 0.0, -3.0] } }),
    )
    .expect("world click");
    assert_eq!(again["node"], ok.to_bits());

    // A scene surface 1 m along the ray hides the panel 2 m along it.
    let blocked = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "origin": [0.5, 1.0, 0.0], "direction": [0.0, 0.0, -1.0], "occluder_distance": 1.0 }),
    )
    .expect("occluded click answers");
    assert_eq!(blocked["occluded"], true);
    assert_eq!(blocked["clicked"], false);

    // Direct form: the panel root and a uv.
    let direct = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "entity": root.to_bits(), "uv": [0.1, 0.9] }),
    )
    .expect("direct click");
    assert_eq!(direct["node"], ok.to_bits());

    let miss = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "origin": [5.0, 1.0, 0.0], "direction": [0.0, 0.0, -1.0] }),
    )
    .expect_err("miss");
    assert_eq!(miss.reason, "ui_world_miss");
    let not_panel = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "entity": ok.to_bits() }),
    )
    .expect_err("a button is not a panel root");
    assert_eq!(not_panel.reason, "ui_not_world_panel");
    let bad = regia_ui::agent_bridge::call(app.world_mut(), "ui.world_click", &json!({ "uv": [2, 0] }))
        .expect_err("uv out of range");
    assert_eq!(bad.reason, "ui_bad_param");

    // The window pointer does not reach world-panel nodes: their layout rects are panel pixels.
    app.world_mut().resource_mut::<regia_ui::UiPointer>().cursor = Some([300.0, 100.0]);
    app.update();
    assert_eq!(app.world().resource::<regia_ui::UiPointer>().hovered, None);
    let off = regia_ui::agent_bridge::call(app.world_mut(), "ui.click", &json!({ "name": "cancel" }))
        .expect_err("ui.click refuses world nodes");
    assert_eq!(off.reason, "ui_node_off_window");

    let targets = regia_ui::agent_bridge::call(app.world_mut(), "ui.targets", &json!({})).expect("targets");
    let roots = targets["roots"].as_array().expect("roots");
    let panel = roots
        .iter()
        .find(|r| r["root"] == root.to_bits())
        .expect("panel listed");
    assert_eq!(panel["target"], "world");
    assert_eq!(panel["size_m"], json!([2.0, 1.0]));
}

#[test]
fn a_billboard_panel_is_hit_from_the_side() {
    let mut app = ui_app();
    let (root, _, cancel) = panel_with_buttons(&mut app);
    app.world_mut().entity_mut(root).insert(WorldPanel {
        billboard: regia_ui::Billboard::AxisY,
        ..WorldPanel::default()
    });
    // From +X, looking -X: without the billboard the ray is parallel to the panel.
    let hit = regia_ui::agent_bridge::call(
        app.world_mut(),
        "ui.world_click",
        &json!({ "origin": [4.0, 1.0, -2.5], "direction": [-1.0, 0.0, 0.0] }),
    )
    .expect("billboard faces the ray origin");
    assert_eq!(hit["root"], root.to_bits());
    assert_eq!(hit["node"], cancel.to_bits(), "{hit}");
}

// ---------------------------------------------------------------------------------------------
// GPU: composite and camera targets.
// ---------------------------------------------------------------------------------------------

fn device() -> RenderDevice {
    match RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (install Mesa lavapipe): {err}"),
    }
}

const W: u32 = 64;
const H: u32 = 48;

/// Two lit quads at z = 0 (x around -0.6) and z = -0.5 (x around +0.6), camera at z = 3.
fn lab() -> LabScene {
    let quad = |x: f32, z: f32| LabMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([x - 0.5, -0.5, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, -0.5, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
            LabVertex::from_pos_normal_uv([x + 0.5, 0.5, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([x - 0.5, 0.5, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..LabMesh::unskinned()
    };
    LabScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 20.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 5_000.0,
        double_sided: false,
        post: Some(PostSettings::default()),
        directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.7, 0.6, 0.5, 1.0],
            ..LabMaterial::default()
        }],
        meshes: vec![quad(-0.6, 0.0), quad(0.6, -0.5)],
    }
}

/// The lab camera's pixel for world point `p`.
fn pixel_of(p: Vec3) -> (u32, u32) {
    // The lab camera sits at z = 3 looking down -Z with world +Y up.
    let depth = 3.0 - p.z;
    let half = (0.9f32 * 0.5).tan();
    let ndc_x = p.x / (depth * half * (W as f32 / H as f32));
    let ndc_y = p.y / (depth * half);
    let x = ((ndc_x * 0.5 + 0.5) * W as f32).floor() as u32;
    let y = ((0.5 - ndc_y * 0.5) * H as f32).floor() as u32;
    (x.min(W - 1), y.min(H - 1))
}

fn frame(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    renderer
        .render_offscreen(device, &lab.as_frame_scene())
        .expect("offscreen frame")
        .pixels
}

fn at(pixels: &[u8], (x, y): (u32, u32)) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// A UI app holding one red world panel at `transform` (`size_m` metres at 100 px/m).
fn panel_app(transform: Affine3A, size_m: [f32; 2], panel: WorldPanel) -> App {
    let mut app = ui_app();
    let (root, _) = root_with_fill(
        app.world_mut(),
        UiRoot::world_space([size_m[0] * 100.0, size_m[1] * 100.0], 100.0),
        RED,
    );
    app.world_mut()
        .entity_mut(root)
        .insert((GlobalTransform(transform), panel));
    for _ in 0..3 {
        app.update();
    }
    app
}

fn is_red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 40 && p[2] < 40
}

#[test]
fn frames_without_world_or_camera_ui_are_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    // A window-only UI: the plugin registers no pass, and the passes, even when registered by
    // hand, record nothing.
    let mut app = ui_app();
    root_with_fill(app.world_mut(), UiRoot::default(), WHITE);
    for _ in 0..3 {
        app.update();
    }
    let handle = app.world().resource::<SharedUiRenderer>().0.clone();
    assert_eq!(
        handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_targets(),
        (false, false)
    );
    let mut plain = FrameRenderer::new();
    let mut with_passes = FrameRenderer::new();
    with_passes
        .register_pass(Box::new(WorldPanelPass::new(handle.clone(), device.clone())))
        .expect("world pass");
    with_passes
        .register_pass(Box::new(CameraTargetPass::new(handle.clone(), device.clone())))
        .expect("camera pass");
    for index in 0..3 {
        let a = frame(&mut plain, &device, &lab);
        let b = frame(&mut with_passes, &device, &lab);
        assert!(
            a == b,
            "frame {index}: the UI passes changed a frame with no camera or world UI"
        );
    }
}

#[test]
fn a_world_panel_composites_in_front_and_is_hidden_behind_geometry() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    let mut plain = FrameRenderer::new();
    let base = frame(&mut plain, &device, &lab);

    // In front of everything at z = 1.5: the centre is the panel's red; a far corner is not.
    let front = panel_app(
        Affine3A::from_translation(Vec3::new(0.0, 0.0, 1.5)),
        [1.0, 0.5],
        WorldPanel::default(),
    );
    let handle = front.world().resource::<SharedUiRenderer>().0.clone();
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(WorldPanelPass::new(handle.clone(), device.clone())))
        .expect("world pass");
    let shot = frame(&mut renderer, &device, &lab);
    let centre = pixel_of(Vec3::new(0.0, 0.0, 1.5));
    assert!(is_red(at(&shot, centre)), "{:?}", at(&shot, centre));
    assert_eq!(
        at(&shot, (1, 1)),
        at(&base, (1, 1)),
        "outside the panel the frame is unchanged"
    );
    let report = renderer.graph_report();
    let pass = report
        .extensions
        .iter()
        .find(|p| p.name == regia_ui::world_target::WORLD_PASS_NAME)
        .expect("listed");
    assert_eq!((pass.runs, pass.failures), (1, 0), "{pass:?}");

    // Behind the near quad at z = -1: hidden where the quad is, visible above it.
    let behind = panel_app(
        Affine3A::from_translation(Vec3::new(0.0, 0.0, -1.0)),
        [4.0, 3.0],
        WorldPanel::default(),
    );
    let handle = behind.world().resource::<SharedUiRenderer>().0.clone();
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(WorldPanelPass::new(handle, device.clone())))
        .expect("world pass");
    let shot = frame(&mut renderer, &device, &lab);
    let on_quad = pixel_of(Vec3::new(-0.6, 0.0, 0.0));
    assert_eq!(
        at(&shot, on_quad),
        at(&base, on_quad),
        "the near quad hides the panel"
    );
    let above = pixel_of(Vec3::new(-0.6, 1.0, -1.0));
    assert!(is_red(at(&shot, above)), "{:?}", at(&shot, above));

    // The same panel with the depth test off draws over the quad; faded out it draws nothing.
    let over = panel_app(
        Affine3A::from_translation(Vec3::new(0.0, 0.0, -1.0)),
        [4.0, 3.0],
        WorldPanel {
            depth_test: false,
            ..WorldPanel::default()
        },
    );
    let handle = over.world().resource::<SharedUiRenderer>().0.clone();
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(WorldPanelPass::new(handle, device.clone())))
        .expect("world pass");
    let shot = frame(&mut renderer, &device, &lab);
    assert!(is_red(at(&shot, on_quad)), "{:?}", at(&shot, on_quad));
    let faded = panel_app(
        Affine3A::from_translation(Vec3::new(0.0, 0.0, -1.0)),
        [4.0, 3.0],
        WorldPanel {
            fade: Some([1.0, 2.0]),
            ..WorldPanel::default()
        },
    );
    let handle = faded.world().resource::<SharedUiRenderer>().0.clone();
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(WorldPanelPass::new(handle, device.clone())))
        .expect("world pass");
    let shot = frame(&mut renderer, &device, &lab);
    assert_eq!(
        at(&shot, above),
        at(&base, above),
        "4 m away is past the fade end"
    );
}

#[test]
fn a_camera_target_root_renders_into_its_texture() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let lab = lab();
    let mut app = ui_app();
    let screen = app
        .world_mut()
        .spawn((
            Camera {
                target: RenderTarget::Texture { asset_id: 7 },
                ..Camera::default()
            },
            TextureViewSettings {
                size: [W, H],
                ..TextureViewSettings::default()
            },
        ))
        .id();
    // A green bar over the top quarter of the monitor.
    let root = app.world_mut().spawn((UiRoot::camera(screen), full())).id();
    let bar = app
        .world_mut()
        .spawn((
            UiNode,
            NodeStyle::new(Style {
                size: [Dimension::Percent(100.0), Dimension::Percent(25.0)],
                ..Default::default()
            }),
            color(GREEN),
        ))
        .id();
    app.world_mut().entity_mut(root).add_child(bar);
    for _ in 0..3 {
        app.update();
    }
    let handle = app.world().resource::<SharedUiRenderer>().0.clone();
    assert_eq!(
        handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_targets(),
        (false, true)
    );
    let settings = TextureViewSettings {
        size: [W, H],
        ..TextureViewSettings::default()
    };
    let read = |renderer: &FrameRenderer| {
        let view = renderer.texture_target_view(7).expect("texture view rendered");
        let target = regia_ui::OffscreenTarget::new(&device, W, H);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("copy") });
        encoder.copy_texture_to_texture(
            view.texture().as_image_copy(),
            target.texture().as_image_copy(),
            wgpu::Extent3d {
                width: W,
                height: H,
                depth_or_array_layers: 1,
            },
        );
        device.queue.submit([encoder.finish()]);
        target.read(&device).expect("readback")
    };
    let mut plain = FrameRenderer::new();
    plain
        .render_texture_view(&device, 7, lab.as_frame_scene(), &settings)
        .expect("plain view");
    let base = read(&plain);
    let mut renderer = FrameRenderer::new();
    renderer
        .register_pass(Box::new(CameraTargetPass::new(handle, device.clone())))
        .expect("camera pass");
    renderer
        .render_texture_view(&device, 7, lab.as_frame_scene(), &settings)
        .expect("view with UI");
    let shot = read(&renderer);
    let top = at(&shot, (W / 2, 2));
    assert!(top[1] > 200 && top[0] < 40 && top[2] < 40, "green bar: {top:?}");
    let bottom = (W / 2, H - 2);
    assert_eq!(
        at(&shot, bottom),
        at(&base, bottom),
        "below the bar the view is untouched"
    );
    // The main view (no texture view) draws none of it.
    let main = frame(&mut renderer, &device, &lab);
    let main_plain = frame(&mut FrameRenderer::new(), &device, &lab);
    assert_eq!(at(&main, (W / 2, 2)), at(&main_plain, (W / 2, 2)));
}

#[test]
fn the_plugin_registers_the_passes_only_while_targets_exist() {
    // Needs the runtime renderer resource and a device: lavapipe.
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut app = ui_app();
    app.insert_resource(device);
    app.init_resource::<regia_render::RuntimeRenderer>();
    let (root, _) = root_with_fill(app.world_mut(), UiRoot::default(), WHITE);
    app.world_mut()
        .entity_mut(root)
        .insert(GlobalTransform(Affine3A::IDENTITY));
    for _ in 0..2 {
        app.update();
    }
    // Only the UI's two passes count: the renderer registers passes of its own.
    let passes = |app: &App| {
        app.world()
            .resource::<regia_render::RuntimeRenderer>()
            .0
            .registered_passes()
            .into_iter()
            .filter(|n| n.starts_with("ui_"))
            .collect::<Vec<_>>()
    };
    assert!(passes(&app).is_empty(), "window-only UI registers nothing");
    app.world_mut().get_mut::<UiRoot>(root).expect("root").target = UiTarget::WorldSpace {
        size: [64.0, 64.0],
        ppm: 64.0,
    };
    for _ in 0..2 {
        app.update();
    }
    assert_eq!(
        passes(&app),
        vec![regia_ui::world_target::WORLD_PASS_NAME.to_owned()]
    );
    app.world_mut().get_mut::<UiRoot>(root).expect("root").target = UiTarget::Window(None);
    for _ in 0..2 {
        app.update();
    }
    assert!(passes(&app).is_empty(), "the pass goes with the last panel");
    // The FramePass trait object stays usable (name and slot are part of the graph listing).
    let pass = WorldPanelPass::new(
        app.world().resource::<SharedUiRenderer>().0.clone(),
        app.world().resource::<RenderDevice>().clone(),
    );
    assert_eq!(pass.slot(), regia_render::GraphSlot::AfterPost);
}
