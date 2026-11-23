// SPDX-License-Identifier: MPL-2.0
//! Audit G render5 #10: a texture view's GPU state is released while nothing is renderable.
//!
//! `render_texture_views` releases a view that is no longer extracted after
//! `TEXTURE_VIEW_RELEASE_FRAMES` (120) frames. It returned before its clock moved on every frame
//! with no mesh, so a level unloaded behind a mesh-less menu kept the mirror's texture, history
//! and shadow maps for the whole menu. These tests pin the release on such frames, and that a
//! view still extracted on a mesh-less frame is kept.

use regia_app::prelude::*;
use regia_render::{
    Camera, DirectionalLightUniform, LabMaterial, LabMesh, LabScene, MeshRenderer, PostSettings,
    RenderDevice, RenderTarget, RuntimeRenderer, TextureViewSettings, extract_frame, render_texture_views,
    spawn_lab_scene,
};
use regia_scene::{GlobalTransform, Transform};

/// Asset id of the mirror view.
const MIRROR: u64 = 0x31;
/// Renders past `TEXTURE_VIEW_RELEASE_FRAMES` (120).
const PAST_GRACE: usize = 130;

/// One serial GPU user per test binary.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip texture_view_release: {err}");
            None
        }
    }
}

fn scene() -> LabScene {
    let (cube, cube_indices) = regia_render::box_mesh([0.6, 0.6, 0.6]);
    LabScene {
        width: 32,
        height: 32,
        eye: [0.0, 2.0, 6.0],
        target: [0.0, 0.5, 0.0],
        fov_y: 0.9,
        near: 0.1,
        far: 100.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 14.5,
        ambient_lux: 12_000.0,
        double_sided: false,
        post: Some(PostSettings {
            exposure_ev: 14.5,
            auto_exposure: false,
            bloom: false,
            taa: false,
            ..PostSettings::default()
        }),
        directional: DirectionalLightUniform::new([0.35, -0.8, -0.45], [1.0, 0.97, 0.92], 100_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.6, 0.6, 0.6, 1.0],
            roughness: 0.7,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: regia_render::translate_mesh(&cube, [0.0, 0.6, 0.0]),
            indices: cube_indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    }
}

/// An app with the lab scene, a window camera and a mirror camera rendering into [`MIRROR`];
/// returns the mirror camera.
fn app(device: RenderDevice) -> (App, Entity) {
    let mut app = App::new();
    app.init_resource::<regia_render::ExtractedFrame>();
    app.insert_resource(device);
    app.init_resource::<RuntimeRenderer>();
    spawn_lab_scene(app.world_mut(), &scene());
    let transform = Transform::from_translation(regia_core::math::Vec3::new(0.0, 2.0, -6.0));
    let mirror = app
        .world_mut()
        .spawn((
            transform,
            GlobalTransform(transform.compute_affine()),
            Camera {
                target: RenderTarget::Texture { asset_id: MIRROR },
                ..Camera::default()
            },
            TextureViewSettings {
                size: [32, 32],
                ..TextureViewSettings::default()
            },
        ))
        .id();
    app.add_systems(Render, (extract_frame, render_texture_views).chain());
    (app, mirror)
}

/// Despawns every mesh: the frame keeps its window camera but has nothing to draw.
fn unload_meshes(app: &mut App) {
    let meshes: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<MeshRenderer>>()
        .iter(app.world())
        .collect();
    assert!(!meshes.is_empty());
    for entity in meshes {
        app.world_mut().despawn(entity);
    }
}

fn view_ids(app: &App) -> Vec<u64> {
    app.world().resource::<RuntimeRenderer>().0.texture_view_ids()
}

#[test]
fn an_unloaded_mirror_is_released_on_mesh_less_frames() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let (mut app, mirror) = app(device);
    for _ in 0..2 {
        app.update();
    }
    assert_eq!(view_ids(&app), vec![MIRROR], "the mirror never rendered");
    // The level unloads: its meshes and its mirror camera go together; a menu with no mesh
    // follows.
    unload_meshes(&mut app);
    app.world_mut().despawn(mirror);
    for _ in 0..PAST_GRACE {
        app.update();
    }
    assert!(
        view_ids(&app).is_empty(),
        "the mirror's GPU state outlived the grace period on mesh-less frames: {:?}",
        view_ids(&app)
    );
}

#[test]
fn a_mirror_still_extracted_on_mesh_less_frames_is_kept() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let (mut app, _mirror) = app(device);
    for _ in 0..2 {
        app.update();
    }
    assert_eq!(view_ids(&app), vec![MIRROR]);
    // The meshes go, the mirror camera stays: the view is still extracted, so it is not released.
    unload_meshes(&mut app);
    for _ in 0..PAST_GRACE {
        app.update();
    }
    assert_eq!(
        view_ids(&app),
        vec![MIRROR],
        "a mirror still in the scene was released"
    );
}
