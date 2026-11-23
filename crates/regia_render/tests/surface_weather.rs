// SPDX-License-Identifier: MPL-2.0
//! The surface response on lab materials (spec 99 § 11): a dry frame is the frame without the
//! feature, a wet one darkens what is porous, snow settles on what faces up, and the roofed patch
//! keeps its statistics. No goldens: each check compares two renders of the same scene.

use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice, SurfaceWeatherFrame, WeatherResponse,
};
use regia_test_utils::serial_gpu::{SerialGpu, serial_device};

const W: u32 = 200;
const H: u32 = 150;
const EYE: [f32; 3] = [0.0, 7.0, 9.0];
const TARGET: [f32; 3] = [0.0, 0.0, 0.0];
const FOV_Y: f32 = 0.9;

/// The binary's one shared device, one test at a time: per-test devices on parallel threads hung
/// the NVIDIA driver (RTX 5090 GPU pre-gate, ADR-7483).
fn device() -> SerialGpu {
    match serial_device() {
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
        depth_prepass: false,
        directional_shadows: false,
        ibl: true,
        // Low on the tone curve, so a change of albedo reads as about that change.
        exposure_ev: 14.0,
        ambient_lux: 8_000.0,
        double_sided: true,
        post: None,
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

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    match renderer.render_offscreen(device, &lab.as_frame_scene_with_quality(FrameQuality::LAB)) {
        Ok(frame) => frame.pixels,
        Err(err) => panic!("frame: {err}"),
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
const ROOFED: [f32; 3] = [-3.5, 0.0, 1.4];

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

/// Dry weather renders the bytes of the scene without the feature.
#[test]
fn a_dry_frame_is_the_frame_without_the_feature() {
    let device = device();
    let lab = scene();
    let plain = render(&mut FrameRenderer::new(), &device, &lab);
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(SurfaceWeatherFrame::DRY);
    renderer.set_precipitation_occlusion_budget(Some((256, 32.0)), 16_384);
    assert_eq!(plain, render(&mut renderer, &device, &lab));
    assert_eq!(renderer.precipitation_occlusion(), None, "no view on a dry frame");
}

/// `wetness = 1` darkens the concrete by 35-55 % and the metal by less than 5 %.
#[test]
fn wetness_darkens_what_is_porous() {
    let device = device();
    let lab = scene();
    let dry = render(&mut FrameRenderer::new(), &device, &lab);
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(wet(1.0));
    let soaked = render(&mut renderer, &device, &lab);
    let (before, after) = (luminance(&dry, CONCRETE), luminance(&soaked, CONCRETE));
    let drop = 1.0 - after / before;
    assert!(
        (0.35..=0.55).contains(&drop),
        "concrete fell {drop}: {before} to {after}"
    );
    let (before, after) = (luminance(&dry, METAL), luminance(&soaked, METAL));
    let drop = (1.0 - after / before).abs();
    assert!(drop < 0.05, "metal moved {drop}: {before} to {after}");
}

/// `snow_cover = 1` brightens the plane and not the wall.
#[test]
fn snow_settles_on_the_floor_and_not_the_wall() {
    let device = device();
    let lab = scene();
    let dry = render(&mut FrameRenderer::new(), &device, &lab);
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(snow(1.0));
    let covered = render(&mut renderer, &device, &lab);
    assert!(
        luminance(&covered, CONCRETE) > 1.3 * luminance(&dry, CONCRETE),
        "{} vs {}",
        luminance(&covered, CONCRETE),
        luminance(&dry, CONCRETE)
    );
    let (before, after) = (luminance(&dry, WALL), luminance(&covered, WALL));
    assert!((after - before).abs() < 0.03 * before, "{before} to {after}");
}

/// With the occlusion view, the patch under the roof keeps its dry statistics.
#[test]
fn the_roofed_patch_stays_dry() {
    let device = device();
    let lab = scene();
    let dry = render(&mut FrameRenderer::new(), &device, &lab);
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(wet(1.0));
    renderer.set_precipitation_occlusion_budget(Some((512, 32.0)), 16_384);
    let frame = render(&mut renderer, &device, &lab);
    let frame = {
        // The tile is drawn in the frame that plans it; a second one reads it held.
        drop(frame);
        render(&mut renderer, &device, &lab)
    };
    let info = renderer.precipitation_occlusion().expect("the view is on");
    assert_eq!(info.redraws, 1, "{info:?}");
    let (before, after) = (luminance(&dry, ROOFED), luminance(&frame, ROOFED));
    assert!(
        (after - before).abs() < 0.02 * before,
        "under the roof {before} to {after}"
    );
    let drop = 1.0 - luminance(&frame, CONCRETE) / luminance(&dry, CONCRETE);
    assert!(drop > 0.3, "the open floor is wet: {drop}");
}

/// Puddles plus ripples move with the clock.
#[test]
fn ripples_change_the_frame_at_another_clock() {
    let device = device();
    let mut lab = scene();
    for material in &mut lab.materials {
        material.weather = WeatherResponse {
            puddles: true,
            ..WeatherResponse::DEFAULT
        };
    }
    let mut renderer = FrameRenderer::new();
    let puddled = |clock_s| SurfaceWeatherFrame {
        wetness: 1.0,
        puddles: 1.0,
        snow_cover: 0.0,
        ripple: 1.0,
        clock_s,
    };
    renderer.set_surface_weather(puddled(1.0));
    let a = render(&mut renderer, &device, &lab);
    renderer.set_surface_weather(puddled(1.3));
    let b = render(&mut renderer, &device, &lab);
    assert_ne!(a, b);
    renderer.set_surface_weather(puddled(1.0));
    assert_eq!(
        a,
        render(&mut renderer, &device, &lab),
        "fixed inputs, fixed bytes"
    );
}

/// The extinction term thickens the air without any fog on the host.
#[test]
fn the_extinction_fades_the_far_wall() {
    let device = device();
    let lab = scene();
    let clear = render(&mut FrameRenderer::new(), &device, &lab);
    let mut renderer = FrameRenderer::new();
    renderer.set_precipitation_extinction(0.2);
    let thick = render(&mut renderer, &device, &lab);
    assert_ne!(clear, thick);
    assert_eq!(renderer.precipitation_extinction(), 0.2);
    renderer.set_precipitation_extinction(f32::NAN);
    assert_eq!(renderer.precipitation_extinction(), 0.0);
    assert_eq!(
        clear,
        render(&mut renderer, &device, &lab),
        "back to the plain frame"
    );
}

/// A material can opt out.
#[test]
fn a_material_can_refuse_the_weather() {
    let device = device();
    let mut lab = scene();
    lab.materials[0].weather = WeatherResponse::NONE;
    let dry = render(&mut FrameRenderer::new(), &device, &lab);
    let mut renderer = FrameRenderer::new();
    renderer.set_surface_weather(wet(1.0));
    let soaked = render(&mut renderer, &device, &lab);
    let (before, after) = (luminance(&dry, CONCRETE), luminance(&soaked, CONCRETE));
    assert!((after - before).abs() < 0.01 * before, "{before} to {after}");
}

/// Every host of `set_outdoor` shades the weather: it travels with the outdoor parameters.
#[test]
fn the_outdoor_parameters_carry_the_weather() {
    let mut renderer = FrameRenderer::new();
    renderer.set_outdoor(regia_render::OutdoorParams {
        surface_weather: wet(0.7),
        precip_extinction_per_m: 0.01,
        ..regia_render::OutdoorParams::default()
    });
    assert_eq!(renderer.surface_weather(), wet(0.7));
    assert_eq!(renderer.precipitation_extinction(), 0.01);
}
