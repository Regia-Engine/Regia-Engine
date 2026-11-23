// SPDX-License-Identifier: MPL-2.0
//! Wireframe debug view (Tincal engine request F3): the pane overlays the meshes' edges on the
//! lit frame through the gizmo pass, for rigid (instanced on a GPU-driven device) and skinned
//! (per-object) meshes alike, and `Lit` stays untouched.

use std::sync::Arc;

use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_editor::render::{PaneRequest, render_pane};
use regia_editor::viewport::EditorCamera;
use regia_editor::views::ViewKind;
use regia_render::{CpuMesh, DebugViewMode, JointMatrix, LabMaterial, MeshRenderer, SkinJoints};
use regia_scene::prelude::Transform;

fn device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    }
}

fn world_with_cubes() -> (regia_app::App, Entity, Entity) {
    let mut app = regia_app::App::new();
    app.add_plugins(regia_scene::ScenePlugin);
    regia_editor::mesh_source::register_level_types(app.world_mut());
    app.world_mut().insert_resource(EditorCamera::default());
    let world = app.world_mut();
    let (vertices, indices) = regia_render::box_mesh([0.5, 0.5, 0.5]);
    let mesh = Arc::new(CpuMesh { vertices, indices });
    let material = Arc::new(LabMaterial {
        base_color: [0.6, 0.6, 0.6, 1.0],
        ..LabMaterial::default()
    });
    let rigid = world
        .spawn((
            Transform::from_translation(Vec3::new(-1.2, 0.5, 0.0)),
            MeshRenderer::new(Arc::clone(&mesh), Arc::clone(&material)),
        ))
        .id();
    // The same cube, skinned: every vertex fully on joint 0, which carries the world pose.
    let mut skinned_vertices = mesh.vertices.clone();
    for v in &mut skinned_vertices {
        v.joints = [0; 4];
        v.weights = [1.0, 0.0, 0.0, 0.0];
    }
    let skinned_mesh = Arc::new(CpuMesh {
        vertices: skinned_vertices,
        indices: mesh.indices.clone(),
    });
    let skinned = world
        .spawn((
            Transform::IDENTITY,
            MeshRenderer::new(skinned_mesh, material),
            SkinJoints {
                palette: vec![JointMatrix::translation(1.2, 0.5, 0.0)],
            },
        ))
        .id();
    (app, rigid, skinned)
}

fn request(debug_view: DebugViewMode) -> PaneRequest {
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
        width: 480,
        height: 320,
        ids: false,
        warm_frames: 0,
        debug_view,
        reference: None,
    }
}

/// Pixels the wire colour (linear 0, 1, 0.25) lands on: strongly green.
fn wire_pixels(image: &egui::ColorImage, x_range: std::ops::Range<usize>) -> usize {
    let width = image.width();
    image
        .pixels
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            x_range.contains(&(i % width)) && p.g() > 200 && p.r() < 90 && u32::from(p.b()) < 170
        })
        .count()
}

#[test]
fn wireframe_overlays_rigid_and_skinned_meshes_and_lit_is_untouched() {
    let device = device();
    let (mut app, _rigid, _skinned) = world_with_cubes();
    let world = app.world_mut();

    let lit = render_pane(&device, world, &request(DebugViewMode::Lit)).expect("lit");
    let wire = render_pane(&device, world, &request(DebugViewMode::Wireframe)).expect("wireframe");
    let lit_again = render_pane(&device, world, &request(DebugViewMode::Lit)).expect("lit again");

    let dir = regia_core::brand::env_var("REGIA_VIEWPORT_SHOTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("regia-viewport-shots"));
    let _ = std::fs::create_dir_all(&dir);
    for (name, frame) in [("debug_view_wireframe.png", &wire), ("debug_view_lit.png", &lit)] {
        let bytes: Vec<u8> = frame
            .image
            .pixels
            .iter()
            .flat_map(|p| [p.r(), p.g(), p.b(), p.a()])
            .collect();
        if let Some(buffer) =
            image::RgbaImage::from_raw(frame.image.width() as u32, frame.image.height() as u32, bytes)
        {
            let _ = buffer.save(dir.join(name));
        }
    }

    let width = lit.image.width();
    let all = 0..width;
    assert_eq!(
        wire_pixels(&lit.image, all.clone()),
        0,
        "no wire colour in the lit frame"
    );
    assert_eq!(
        lit.image.pixels, lit_again.image.pixels,
        "wireframe leaves no state behind"
    );

    // The rigid cube is left of centre, the skinned one right of it.
    let left = wire_pixels(&wire.image, 0..width / 2);
    let right = wire_pixels(&wire.image, width / 2..width);
    assert!(left > 60, "the rigid cube's edges draw ({left} px)");
    assert!(right > 60, "the CPU-skinned cube's edges draw ({right} px)");

    // The overlay sits on the lit picture: away from the wires the frame is the lit one.
    let same = lit
        .image
        .pixels
        .iter()
        .zip(&wire.image.pixels)
        .filter(|(a, b)| a == b)
        .count();
    assert!(
        same * 10 > lit.image.pixels.len() * 9,
        "the overlay only touches the edges ({same} equal)"
    );
    eprintln!(
        "wireframe: rigid {left} px, skinned {right} px; draws instanced {} per_object {}",
        wire.info.draws.instanced, wire.info.draws.per_object
    );
}
