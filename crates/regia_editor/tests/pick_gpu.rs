// SPDX-License-Identifier: MPL-2.0
//! Picks over a view, an id buffer and a size (`pick_in_view`, `pick_hit_in_view`), over a GPU
//! pane (`pick_in_gpu_frame`, `pick_hit_in_gpu_frame`) and over a windowed id read
//! (`render_pane_id_window`): every one agrees with the old `PaneFrame` picks on identical
//! input, pixel for pixel.

use std::sync::Arc;

use regia_core::math::{Quat, Vec2, Vec3};
use regia_editor::picking::{FaceCull, IdRect};
use regia_editor::render::{PaneRequest, render_pane, render_pane_gpu, render_pane_id_window};
use regia_editor::viewport::{
    EditorCamera, pick_hit_in_frame, pick_hit_in_gpu_frame, pick_hit_in_view, pick_in_frame,
    pick_in_gpu_frame, pick_in_view,
};
use regia_editor::views::ViewKind;
use regia_render::{CpuMesh, DebugViewMode, LabMaterial, MeshRenderer};
use regia_scene::prelude::Transform;

const W: u32 = 160;
const H: u32 = 120;

/// Serialises device creation across this binary's test threads (ADR-1438).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn request() -> PaneRequest {
    PaneRequest {
        pane: 0,
        kind: ViewKind::Perspective,
        camera: EditorCamera {
            target: Vec3::new(0.0, 0.5, 0.0),
            distance: 6.0,
            yaw: 0.4,
            pitch: 0.35,
            ..EditorCamera::default()
        },
        width: W,
        height: H,
        ids: true,
        warm_frames: 0,
        debug_view: DebugViewMode::Lit,
        reference: None,
    }
}

/// A world with two boxes, one rotated, so the pane has ids, background and an edge.
fn scene(device: &regia_render::RenderDevice) -> regia_app::App {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(request().camera);
    app.world_mut().insert_resource(device.clone());
    let world = app.world_mut();
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let material = Arc::new(LabMaterial::default());
    world.spawn((
        Transform::from_translation(Vec3::new(-0.8, 0.5, 0.0)),
        MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
    ));
    world.spawn((
        Transform {
            translation: Vec3::new(0.9, 0.5, 0.2),
            rotation: Quat::from_rotation_y(0.6),
            ..Transform::IDENTITY
        },
        MeshRenderer::new(mesh, material),
    ));
    app
}

/// A coarse grid of pixels covering both boxes and the background.
fn pixels() -> impl Iterator<Item = Vec2> {
    (0..H)
        .step_by(7)
        .flat_map(|y| (0..W).step_by(7).map(move |x| Vec2::new(x as f32, y as f32)))
}

#[test]
fn view_gpu_and_window_picks_match_the_pane_frame_picks() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = regia_render::RenderDevice::headless().expect("lavapipe adapter");
    let mut app = scene(&device);
    let world = app.world_mut();
    let request = request();
    let cpu = render_pane(&device, world, &request).expect("cpu pane");
    let gpu = render_pane_gpu(&device, world, &request).expect("gpu pane");
    assert_eq!(gpu.size(), [W, H]);
    let (cpu_ids, gpu_ids) = (
        cpu.ids.as_ref().expect("cpu ids"),
        gpu.ids.as_ref().expect("gpu ids"),
    );
    assert_eq!(
        (cpu_ids.ids.as_slice(), &cpu_ids.entities),
        (gpu_ids.ids.as_slice(), &gpu_ids.entities)
    );

    let mut mesh_hits = 0;
    let mut background = 0;
    for pixel in pixels() {
        let old = pick_in_frame(world, &cpu, pixel);
        assert_eq!(
            pick_in_gpu_frame(world, &gpu, pixel),
            old,
            "gpu pick at {pixel:?}"
        );
        assert_eq!(
            pick_in_view(world, &cpu.view, [W, H], cpu.ids.as_ref(), pixel),
            old,
            "view pick at {pixel:?}"
        );
        let old_hit = pick_hit_in_frame(world, &cpu, pixel, FaceCull::None);
        assert_eq!(
            pick_hit_in_gpu_frame(world, &gpu, pixel, FaceCull::None),
            old_hit,
            "gpu hit at {pixel:?}"
        );
        assert_eq!(
            pick_hit_in_view(world, &cpu.view, [W, H], cpu.ids.as_ref(), pixel, FaceCull::None),
            old_hit,
            "view hit at {pixel:?}"
        );
        if old_hit.is_some() {
            mesh_hits += 1;
        } else {
            background += 1;
        }
    }
    assert!(
        mesh_hits > 0 && background > 0,
        "grid covers meshes ({mesh_hits}) and background ({background})"
    );

    // No ids: the view pick casts every mesh, like the old pick without a buffer.
    let center = Vec2::new(W as f32 * 0.5, H as f32 * 0.5);
    assert_eq!(
        pick_hit_in_view(
            world,
            &cpu.view,
            [W, H],
            None::<&regia_editor::picking::IdBuffer>,
            center,
            FaceCull::None
        )
        .map(|h| h.entity),
        pick_hit_in_frame(world, &cpu, center, FaceCull::None).map(|h| h.entity),
    );
}

#[test]
fn a_windowed_id_read_equals_the_full_read_and_picks_the_same() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = regia_render::RenderDevice::headless().expect("lavapipe adapter");
    let mut app = scene(&device);
    let world = app.world_mut();
    let request = request();
    let cpu = render_pane(&device, world, &request).expect("cpu pane");
    let full = cpu.ids.as_ref().expect("ids");
    let radius = 3;
    // Eight pixels on a mesh and four on the background (the grid's first rows are all
    // background, so taking the first twelve would never window a mesh).
    let on_mesh = |p: &Vec2| full.at(p.x as u32, p.y as u32).is_some();
    let chosen: Vec<Vec2> = pixels()
        .filter(on_mesh)
        .take(8)
        .chain(pixels().filter(|p| !on_mesh(p)).take(4))
        .collect();
    assert_eq!(chosen.len(), 12, "the grid has mesh and background pixels");
    let mut mesh_picks = 0;
    for pixel in chosen {
        let center = (pixel.x as u32, pixel.y as u32);
        let rect = IdRect::around(center, radius, W, H).expect("inside");
        let (view, window) = render_pane_id_window(&device, world, &request, rect).expect("window");
        assert_eq!(view, cpu.view);
        assert_eq!(window, full.window(rect), "window at {center:?}");
        // Inside the window the lookups agree; outside it there is nothing.
        for y in 0..H {
            for x in 0..W {
                let inside =
                    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height;
                let want = if inside { full.at(x, y) } else { None };
                assert_eq!(window.at(x, y), want, "at ({x}, {y}) of window {rect:?}");
            }
        }
        assert_eq!(
            pick_in_view(world, &view, [W, H], Some(&window), pixel),
            pick_in_frame(world, &cpu, pixel),
            "windowed pick at {pixel:?}"
        );
        let hit = pick_hit_in_view(world, &view, [W, H], Some(&window), pixel, FaceCull::None);
        assert_eq!(
            hit,
            pick_hit_in_frame(world, &cpu, pixel, FaceCull::None),
            "windowed hit at {pixel:?}"
        );
        mesh_picks += usize::from(hit.is_some());
    }
    assert!(mesh_picks > 0, "a windowed pick hit a mesh");
    // A rectangle outside the pane is refused, not clamped to nothing silently.
    let outside = IdRect {
        x: W,
        y: 0,
        width: 4,
        height: 4,
    };
    assert!(render_pane_id_window(&device, world, &request, outside).is_none());
}

#[test]
fn a_pane_without_ids_skips_the_id_pass() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = regia_render::RenderDevice::headless().expect("lavapipe adapter");
    let mut app = scene(&device);
    let world = app.world_mut();
    let mut request = request();
    request.ids = false;
    let gpu = render_pane_gpu(&device, world, &request).expect("gpu pane");
    assert!(gpu.ids.is_none(), "`ids: false` renders no id buffer");
}
