// SPDX-License-Identifier: MPL-2.0
//! The surface response on the visibility-buffer path (spec 99 § 6.5, ADR-5823): the resolve shades
//! lab materials through `shade_surface`, the lab forward body, so it takes the same weather rule. A
//! dry frame is the frame without the feature byte for byte, wetness darkens what is porous by the
//! forward path's amount, and snow settles on the floor by the forward path's amount and not on the
//! wall. The visibility path needs a depth prepass and the linear HDR frame, so both paths render
//! through the same deterministic post. No goldens: each check compares renders of the same scene
//! made here (lavapipe).

use std::sync::Arc;

use regia_asset_pipeline::meshlet::{MeshletAttributes, MeshletSettings, build_meshlet_mesh_with_attributes};
use regia_render::visbuffer::VisBufferStats;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    MeshletBuilder, PostSettings, RenderDevice, SurfaceWeatherFrame,
};

const W: u32 = 200;
const H: u32 = 150;
const EYE: [f32; 3] = [0.0, 7.0, 9.0];
const TARGET: [f32; 3] = [0.0, 0.0, 0.0];
const FOV_Y: f32 = 0.9;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn quad(corners: [[f32; 3]; 4], n: [f32; 3]) -> (Vec<LabVertex>, Vec<u32>) {
    (
        corners
            .into_iter()
            .zip([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .map(|(p, uv)| LabVertex::from_pos_normal_uv(p, n, uv))
            .collect(),
        vec![0, 2, 1, 0, 3, 2],
    )
}

fn mesh(corners: [[f32; 3]; 4], n: [f32; 3], material: usize) -> LabMesh {
    let (vertices, indices) = quad(corners, n);
    LabMesh {
        vertices,
        indices,
        material,
        ..LabMesh::unskinned()
    }
}

/// Concrete floor, a metal plate on it, a wall behind it, and a roof over one patch.
fn scene() -> LabScene {
    let up = [0.0, 1.0, 0.0];
    let down = [0.0, -1.0, 0.0];
    let concrete = LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    let metal = LabMaterial {
        base_color: [0.7, 0.7, 0.7, 1.0],
        metallic: 1.0,
        roughness: 0.5,
        ..LabMaterial::default()
    };
    let wall = LabMaterial {
        base_color: [0.3, 0.3, 0.35, 1.0],
        roughness: 0.9,
        ..LabMaterial::default()
    };
    LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.1,
        far: 60.0,
        // The visibility path joins the depth prepass.
        depth_prepass: true,
        directional_shadows: false,
        ibl: true,
        // Low on the tone curve, so a change of albedo reads as about that change.
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        // The visibility path shades into the linear HDR frame; a fixed exposure, no auto.
        post: Some(PostSettings {
            exposure_ev: 14.0,
            ..PostSettings::golden_deterministic()
        }),
        directional: DirectionalLightUniform::new([0.3, -1.0, -0.4], [1.0; 3], 20_000.0),
        local_lights: Vec::new(),
        materials: vec![concrete, metal, wall],
        meshes: vec![
            mesh(
                [
                    [-9.0, 0.0, -9.0],
                    [9.0, 0.0, -9.0],
                    [9.0, 0.0, 9.0],
                    [-9.0, 0.0, 9.0],
                ],
                up,
                0,
            ),
            mesh(
                [
                    [2.0, 0.02, -2.0],
                    [5.0, 0.02, -2.0],
                    [5.0, 0.02, 2.0],
                    [2.0, 0.02, 2.0],
                ],
                up,
                1,
            ),
            mesh(
                [
                    [-9.0, 0.0, -6.0],
                    [9.0, 0.0, -6.0],
                    [9.0, 4.0, -6.0],
                    [-9.0, 4.0, -6.0],
                ],
                [0.0, 0.0, 1.0],
                2,
            ),
            mesh(
                [
                    [-5.0, 1.5, -2.0],
                    [-2.0, 1.5, -2.0],
                    [-2.0, 1.5, 2.0],
                    [-5.0, 1.5, 2.0],
                ],
                down,
                0,
            ),
        ],
    }
}

/// The pixel a world point lands on.
fn project(p: [f32; 3]) -> (usize, usize) {
    use regia_core::glam::Vec3;
    let view =
        regia_render::view_math::look_at_view(Vec3::from_array(EYE), Vec3::from_array(TARGET), Vec3::Y);
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.1, 60.0);
    let clip: regia_core::glam::Vec4 = proj * view * Vec3::from_array(p).extend(1.0);
    let ndc = clip / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as usize,
        ((0.5 - ndc.y * 0.5) * H as f32) as usize,
    )
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = f32::from(c) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Mean linear luminance of a 9×9 window around a world point.
fn luminance(pixels: &[u8], p: [f32; 3]) -> f32 {
    let (x, y) = project(p);
    let mut sum = 0.0;
    for j in y.saturating_sub(4)..(y + 5).min(H as usize) {
        for i in x.saturating_sub(4)..(x + 5).min(W as usize) {
            let at = (j * W as usize + i) * 4;
            sum += 0.2126 * srgb_to_linear(pixels[at])
                + 0.7152 * srgb_to_linear(pixels[at + 1])
                + 0.0722 * srgb_to_linear(pixels[at + 2]);
        }
    }
    sum / 81.0
}

const CONCRETE: [f32; 3] = [-0.5, 0.0, 3.0];
const METAL: [f32; 3] = [3.5, 0.02, 0.0];
const WALL: [f32; 3] = [0.0, 2.0, -6.0];

fn builder() -> MeshletBuilder {
    Arc::new(|vertices: &[LabVertex], indices: &[u32]| {
        let positions: Vec<[f32; 3]> = vertices.iter().map(|v| v.position).collect();
        let normals: Vec<[f32; 3]> = vertices.iter().map(|v| v.normal).collect();
        let uvs: Vec<[f32; 2]> = vertices.iter().map(|v| v.uv).collect();
        let attributes = Some(MeshletAttributes {
            normals: &normals,
            uvs: &uvs,
        });
        build_meshlet_mesh_with_attributes(&positions, attributes, indices, &MeshletSettings::default()).ok()
    })
}

/// One frame of the scene, on the visibility path when `visibility`, with `weather` set on the
/// renderer when given.
fn render(
    device: &RenderDevice,
    visibility: bool,
    weather: Option<SurfaceWeatherFrame>,
) -> (Vec<u8>, VisBufferStats) {
    let lab = scene();
    let scene = lab.as_frame_scene_with_quality(FrameQuality {
        visibility_buffer: visibility,
        ..FrameQuality::LAB
    });
    let mut renderer = FrameRenderer::new();
    if visibility {
        renderer.set_meshlet_builder(Some(builder()));
    }
    if let Some(weather) = weather {
        renderer.set_surface_weather(weather);
    }
    let pixels = match renderer.render_offscreen(device, &scene) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
    };
    (pixels, renderer.visbuffer_stats())
}

fn wet(wetness: f32) -> SurfaceWeatherFrame {
    SurfaceWeatherFrame {
        wetness,
        ..SurfaceWeatherFrame::DRY
    }
}

fn snow(snow_cover: f32) -> SurfaceWeatherFrame {
    SurfaceWeatherFrame {
        snow_cover,
        ..SurfaceWeatherFrame::DRY
    }
}

/// The visibility path draws the scene's meshes (else the checks below would test the forward
/// path), and dry weather renders the bytes of the frame without the feature.
#[test]
fn a_dry_visibility_frame_is_the_frame_without_the_feature() {
    let device = device();
    let (plain, stats) = render(&device, true, None);
    assert!(
        stats.instances >= 3,
        "the visibility path drew the meshes: {stats:?}"
    );
    let (dry, _) = render(&device, true, Some(SurfaceWeatherFrame::DRY));
    assert_eq!(plain, dry);
}

/// `wetness = 1` darkens the concrete on the visibility path by the forward path's amount (the same
/// rule, through the same post), and moves the metal by less than 5 %.
#[test]
fn wetness_darkens_the_visibility_frame_as_the_forward_frame() {
    let device = device();
    let (dry, stats) = render(&device, true, None);
    assert!(
        stats.instances >= 3,
        "the visibility path drew the meshes: {stats:?}"
    );
    let (soaked, _) = render(&device, true, Some(wet(1.0)));
    let (before, after) = (luminance(&dry, CONCRETE), luminance(&soaked, CONCRETE));
    let drop = 1.0 - after / before;
    assert!(drop > 0.1, "concrete fell {drop}: {before} to {after}");
    let (before, after) = (luminance(&dry, METAL), luminance(&soaked, METAL));
    let metal = (1.0 - after / before).abs();
    assert!(metal < 0.05, "metal moved {metal}: {before} to {after}");
    let (forward_dry, _) = render(&device, false, None);
    let (forward_soaked, _) = render(&device, false, Some(wet(1.0)));
    let forward = 1.0 - luminance(&forward_soaked, CONCRETE) / luminance(&forward_dry, CONCRETE);
    assert!(
        (drop - forward).abs() < 0.03,
        "visibility path fell {drop}, forward path {forward}"
    );
}

/// `snow_cover = 1` brightens the floor on the visibility path by the forward path's amount, and not
/// the wall.
#[test]
fn snow_settles_on_the_visibility_floor_and_not_the_wall() {
    let device = device();
    let (dry, _) = render(&device, true, None);
    let (covered, stats) = render(&device, true, Some(snow(1.0)));
    assert!(
        stats.instances >= 3,
        "the visibility path drew the meshes: {stats:?}"
    );
    let gain = luminance(&covered, CONCRETE) / luminance(&dry, CONCRETE);
    assert!(gain > 1.1, "the floor brightened by {gain}");
    let (before, after) = (luminance(&dry, WALL), luminance(&covered, WALL));
    assert!((after - before).abs() < 0.03 * before, "{before} to {after}");
    let (forward_dry, _) = render(&device, false, None);
    let (forward_covered, _) = render(&device, false, Some(snow(1.0)));
    let forward = luminance(&forward_covered, CONCRETE) / luminance(&forward_dry, CONCRETE);
    assert!(
        (gain - forward).abs() < 0.05 * forward,
        "visibility path gained {gain}, forward path {forward}"
    );
}
