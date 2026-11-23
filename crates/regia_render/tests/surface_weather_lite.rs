// SPDX-License-Identifier: MPL-2.0
//! The surface response on the Mobile tiers' lite forward path (spec 99 § 6.5, ADR-2032): the lite
//! shader composes its weather variant only on a wet, snowy or extinguishing frame, so a dry frame
//! draws the lite bytes it always drew; a wet one darkens what is porous, snow settles on what faces
//! up, the High tier keeps the roofed patch dry through the occlusion tile, and the extinction
//! fades the far wall. Lavapipe never takes the lite path by itself (`adapter_takes_lite`), so
//! every renderer here forces it (`FrameRenderer::set_forward_lite`). No goldens: each check
//! compares two renders of the same scene.

use regia_render::forward_lite::ForwardLite;
use regia_render::{
    DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene, LabVertex,
    RenderDevice, SurfaceWeatherFrame,
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

fn render_lab(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
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

fn lite(tier: ForwardLite) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    renderer.set_forward_lite(Some(tier));
    renderer
}

fn render(renderer: &mut FrameRenderer, device: &RenderDevice, lab: &LabScene) -> Vec<u8> {
    render_lab(renderer, device, lab)
}

const TIERS: [ForwardLite; 2] = [ForwardLite::Low, ForwardLite::High];

/// Dry weather draws the lite bytes of the scene without the feature, and the lite path is the
/// one drawing (its pixels are not the lab shader's).
#[test]
fn a_dry_lite_frame_is_the_lite_frame_without_the_feature() {
    let device = device();
    let lab = scene();
    let full = render(&mut FrameRenderer::new(), &device, &lab);
    for tier in TIERS {
        let plain = render(&mut lite(tier), &device, &lab);
        assert_ne!(plain, full, "{tier:?}: the lite shader draws");
        let mut renderer = lite(tier);
        renderer.set_surface_weather(SurfaceWeatherFrame::DRY);
        renderer.set_precipitation_occlusion_budget(Some((256, 32.0)), 16_384);
        assert_eq!(plain, render(&mut renderer, &device, &lab), "{tier:?}");
        assert_eq!(renderer.precipitation_occlusion(), None, "no view on a dry frame");
    }
}

/// `wetness = 1` darkens the concrete by 35-55 % on both tiers, as on the lab path. The metal keeps
/// its albedo (porosity 0) and takes only the film: it renders as the dry metal at the film's
/// roughness, `mix(0.5, 0.08, 0.9) = 0.122` (spec 99 § 6.3).
#[test]
fn wetness_darkens_what_is_porous_on_the_lite_path() {
    let device = device();
    let lab = scene();
    let mut glossy = scene();
    glossy.materials[1].roughness = 0.5 + (0.08 - 0.5) * 0.9;
    for tier in TIERS {
        let dry = render(&mut lite(tier), &device, &lab);
        let mut renderer = lite(tier);
        renderer.set_surface_weather(wet(1.0));
        let soaked = render(&mut renderer, &device, &lab);
        let (before, after) = (luminance(&dry, CONCRETE), luminance(&soaked, CONCRETE));
        let concrete = 1.0 - after / before;
        assert!(
            (0.35..=0.55).contains(&concrete),
            "{tier:?}: concrete fell {concrete}: {before} to {after}"
        );
        let film = luminance(&render(&mut lite(tier), &device, &glossy), METAL);
        let metal = luminance(&soaked, METAL);
        assert!(
            (metal - film).abs() < 0.01 * film,
            "{tier:?}: wet metal {metal} vs dry metal at the film's roughness {film}"
        );
        // Back to dry: the variant-free pipelines again, the same bytes.
        renderer.set_surface_weather(SurfaceWeatherFrame::DRY);
        assert_eq!(dry, render(&mut renderer, &device, &lab), "{tier:?}");
    }
}

/// `snow_cover = 1` brightens the floor and not the wall.
#[test]
fn snow_settles_on_the_floor_and_not_the_wall_on_the_lite_path() {
    let device = device();
    let lab = scene();
    for tier in TIERS {
        let dry = render(&mut lite(tier), &device, &lab);
        let mut renderer = lite(tier);
        renderer.set_surface_weather(snow(1.0));
        let covered = render(&mut renderer, &device, &lab);
        let (floor_before, floor_after) = (luminance(&dry, CONCRETE), luminance(&covered, CONCRETE));
        assert!(
            floor_after > 1.3 * floor_before,
            "{tier:?}: {floor_before} to {floor_after}"
        );
        let (before, after) = (luminance(&dry, WALL), luminance(&covered, WALL));
        assert!(
            (after - before).abs() < 0.03 * before,
            "{tier:?}: {before} to {after}"
        );
    }
}

/// On the High tier the occlusion tile keeps the patch under the roof dry; the Low tier has no
/// occlusion view (spec 99 § 5.7) and wets it with the open floor.
#[test]
fn the_high_tier_keeps_the_roofed_patch_dry() {
    let device = device();
    let lab = scene();
    for tier in TIERS {
        let dry = render(&mut lite(tier), &device, &lab);
        let mut renderer = lite(tier);
        renderer.set_surface_weather(wet(1.0));
        renderer.set_precipitation_occlusion_budget(Some((512, 32.0)), 16_384);
        drop(render(&mut renderer, &device, &lab));
        let frame = render(&mut renderer, &device, &lab);
        let (before, after) = (luminance(&dry, ROOFED), luminance(&frame, ROOFED));
        let roofed = 1.0 - after / before;
        let open = 1.0 - luminance(&frame, CONCRETE) / luminance(&dry, CONCRETE);
        assert!(open > 0.3, "{tier:?}: the open floor is wet: {open}");
        match tier {
            ForwardLite::High => {
                assert!(
                    roofed.abs() < 0.02,
                    "{tier:?}: under the roof {before} to {after}"
                );
            }
            _ => assert!(
                roofed > 0.3,
                "{tier:?}: no view, the roofed patch is wet: {roofed}"
            ),
        }
    }
}

/// The extinction term thickens the air on the lite path, and clearing it restores the bytes.
#[test]
fn the_extinction_fades_the_far_wall_on_the_lite_path() {
    let device = device();
    let lab = scene();
    for tier in TIERS {
        let clear = render(&mut lite(tier), &device, &lab);
        let mut renderer = lite(tier);
        renderer.set_precipitation_extinction(0.2);
        let thick = render(&mut renderer, &device, &lab);
        assert_ne!(clear, thick, "{tier:?}");
        renderer.set_precipitation_extinction(0.0);
        assert_eq!(
            clear,
            render(&mut renderer, &device, &lab),
            "{tier:?}: back to the plain frame"
        );
    }
}
