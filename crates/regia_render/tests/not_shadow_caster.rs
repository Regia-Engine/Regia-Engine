// SPDX-License-Identifier: MPL-2.0
//! Per-entity shadow opt-out on the runtime path: a [`regia_render::NotShadowCaster`] entity
//! is mapped to [`regia_render::ShadowCast::Off`] by `extract_frame`, so a huge opaque mesh (a
//! host sky dome) casts no sun shadow over the map. Without the marker a mesh keeps
//! [`regia_render::ShadowCast::All`], as before the marker existed.

use regia_app::prelude::*;
use regia_render::{
    DirectionalLightUniform, LabMaterial, LabScene, LabVertex, MeshRenderer, NotShadowCaster, RenderDevice,
    extract_frame, spawn_lab_scene,
};

/// A lit floor with a sphere above it; the sphere casts a sun shadow unless it is marked.
fn lit_floor_scene() -> LabScene {
    let h = 2.5;
    let corners = [[-h, 0.0, -h], [h, 0.0, -h], [h, 0.0, h], [-h, 0.0, h]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let vertices = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv,
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect();
    let (sphere, sphere_idx) = regia_render::uv_sphere(0.45, 16, 12);
    let sphere_verts = sphere
        .into_iter()
        .map(|mut v| {
            v.position[1] += 0.45;
            v
        })
        .collect();
    LabScene {
        width: 160,
        height: 120,
        eye: [1.6, 2.2, 3.2],
        target: [0.0, 0.3, 0.0],
        fov_y: 45.0_f32.to_radians(),
        near: 0.1,
        far: 30.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: false,
        exposure_ev: 14.8,
        ambient_lux: 10_000.0,
        double_sided: false,
        post: None,
        directional: DirectionalLightUniform::new([0.35, -1.0, 0.25], [1.0, 0.98, 0.94], 100_000.0),
        local_lights: vec![],
        materials: vec![LabMaterial {
            base_color: [0.75, 0.75, 0.78, 1.0],
            roughness: 0.6,
            ..LabMaterial::default()
        }],
        meshes: vec![
            regia_render::LabMesh {
                vertices,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..regia_render::LabMesh::unskinned()
            },
            regia_render::LabMesh {
                vertices: sphere_verts,
                indices: sphere_idx,
                material: 0,
                ..regia_render::LabMesh::unskinned()
            },
        ],
    }
}

/// How the sphere behaves on the runtime path.
#[derive(Clone, Copy, PartialEq)]
enum Sphere {
    /// Default: casts a sun shadow everywhere.
    Casts,
    /// `NotShadowCaster` marker on the entity: casts nothing.
    Marker,
}

/// Spawns the scene through the runtime path, applies `sphere` to the sphere entity, extracts and
/// renders it.
fn render_runtime(device: &RenderDevice, sphere: Sphere) -> Vec<u8> {
    let scene = lit_floor_scene();
    let mut app = App::new();
    app.init_resource::<regia_render::ExtractedFrame>();
    spawn_lab_scene(app.world_mut(), &scene);

    // The sphere is the mesh with more than eight vertices (the floor has four).
    let sphere_entity = {
        let mut query = app.world_mut().query::<(Entity, &MeshRenderer)>();
        let mut found = None;
        for (entity, renderer) in query.iter(app.world()) {
            if renderer.mesh.vertices.len() > 8 {
                found = Some(entity);
            }
        }
        found.expect("the sphere mesh entity")
    };
    match sphere {
        Sphere::Casts => {}
        Sphere::Marker => {
            app.world_mut().entity_mut(sphere_entity).insert(NotShadowCaster);
        }
    }

    app.add_systems(Render, extract_frame);
    app.update();

    let extracted = app.world().resource::<regia_render::ExtractedFrame>().clone();
    let frame = extracted.frame_scene(0, 160, 120);
    regia_render::FrameRenderer::new()
        .render_offscreen(device, &frame)
        .expect("render")
        .pixels
}

fn mean_luma(pixels: &[u8]) -> f32 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for px in pixels.as_chunks::<4>().0 {
        sum += f32::from(px[0]) + f32::from(px[1]) + f32::from(px[2]);
        n += 1.0;
    }
    sum / n
}

#[test]
fn not_shadow_caster_casts_no_sun_shadow_and_default_is_unchanged() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip not_shadow_caster: {err}");
            return;
        }
    };

    // The default runtime path (no marker) casts a shadow on the floor; the marked sphere
    // does not, so the frame is brighter (the marker is the only difference).
    let casts = render_runtime(&device, Sphere::Casts);
    let marker = render_runtime(&device, Sphere::Marker);

    let casts_l = mean_luma(&casts);
    let marker_l = mean_luma(&marker);
    assert!(
        marker_l > casts_l + 1.0,
        "a NotShadowCaster sphere does not darken the floor (casts {casts_l}, marker {marker_l})"
    );
}
