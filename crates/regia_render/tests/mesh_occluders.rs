// SPDX-License-Identifier: MPL-2.0
//! Large-mesh occlusion proxies (`FrameRenderer::set_mesh_occluders`, lane mesh-occlusion):
//! a frame without proxies is byte-identical to one that never had them (and to one whose only
//! proxy is out of reach), and a hull's belly, which the IBL lit as if the ground under it were
//! sunlit, darkens once the hull's proxy puts its own shadow under it.

use regia_core::glam::{Mat4, Vec3, Vec4};
use regia_render::{
    AtmospherePreset, DirectionalLightUniform, FrameQuality, FrameRenderer, LabMaterial, LabMesh, LabScene,
    MeshOccluder, OutdoorParams, PostSettings, RenderDevice, RenderProfile, RenderProfilePreset, SsaoMode,
    box_mesh, exposure_from_ev100, inverse_tonemap_agx, translate_mesh,
};

const W: u32 = 320;
const H: u32 = 180;
const EV: f32 = 13.0;
const EYE: [f32; 3] = [3.5, 0.15, 3.0];
const TARGET: [f32; 3] = [0.0, 0.6, 0.0];
const FOV_Y: f32 = 0.9;
/// The hull: a 4 × 1.4 × 2 m box whose belly is 0.4 m above the ground plane y = 0.
const HALF: [f32; 3] = [2.0, 0.7, 1.0];
const BELLY: f32 = 0.4;

fn hull_proxy() -> MeshOccluder {
    MeshOccluder::from_local_bounds(
        Mat4::IDENTITY.to_cols_array_2d(),
        [-HALF[0], BELLY, -HALF[2]],
        [HALF[0], BELLY + 2.0 * HALF[1], HALF[2]],
        [0.3; 3],
    )
    .with_ground(0.0)
}

fn render(device: &RenderDevice, renderer: &mut FrameRenderer) -> Vec<u8> {
    let to_sun = Vec3::new(-0.4, 0.7, -0.5).normalize();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: EV,
        bloom: false,
        fxaa: false,
        taa: false,
        contrast: 1.0,
        saturation: 1.0,
        ..PostSettings::default()
    };
    let (vertices, indices) = box_mesh(HALF);
    let lab = LabScene {
        width: W,
        height: H,
        eye: EYE,
        target: TARGET,
        fov_y: FOV_Y,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new((-to_sun).to_array(), [1.0; 3], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.3, 0.3, 0.3, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&vertices, [0.0, BELLY + HALF[1], 0.0]),
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    // The proxies' analytic term (Balanced). Under Depth (Desktop High's default since gi-probes)
    // the GPU irradiance probes replace the sky term the proxies adjust and converge over frames,
    // so these frame comparisons would measure the probes, not the proxies.
    quality.gi_probes = false;
    renderer.set_outdoor(OutdoorParams {
        planet: regia_render::AtmosphereParameters::from_preset(AtmospherePreset::Mars),
        ..OutdoorParams::default()
    });
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Pixel of a world point.
fn project(p: Vec3) -> (u32, u32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(EYE),
        Vec3::from_array(TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.05, 200.0);
    let clip: Vec4 = (proj * view as Mat4) * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

/// Scene luminance (cd/m²) around a pixel.
fn luminance(pixels: &[u8], at: (u32, u32)) -> f32 {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0;
    for dy in 0..5 {
        for dx in 0..5 {
            let (x, y) = (at.0 + dx - 2, at.1 + dy - 2);
            let i = ((y * W + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f32::from(pixels[i + c]) / 255.0;
            }
            n += 1.0;
        }
    }
    let rgb = inverse_tonemap_agx(sum.map(|v| v / n));
    (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]) / exposure_from_ev100(EV)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn frames_without_reachable_proxies_are_byte_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let plain = render(&device, &mut FrameRenderer::new());
    // Set, then cleared.
    let mut cleared = FrameRenderer::new();
    cleared.set_mesh_occluders(vec![hull_proxy()]);
    let _ = render(&device, &mut cleared);
    cleared.set_mesh_occluders(Vec::new());
    assert!(
        render(&device, &mut cleared) == plain,
        "a cleared proxy list changed the frame"
    );
    // One proxy far out of reach (count 1, no term at any pixel).
    let mut far = FrameRenderer::new();
    far.set_mesh_occluders(vec![MeshOccluder {
        center: [400.0, 1.0, -300.0],
        ..hull_proxy()
    }]);
    let far_pixels = render(&device, &mut far);
    assert_eq!(far.mesh_occluders_in_use(), 1);
    assert!(far_pixels == plain, "an out-of-reach proxy changed the frame");
}

#[test]
fn the_hulls_proxy_darkens_its_belly() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let plain = render(&device, &mut FrameRenderer::new());
    let mut with = FrameRenderer::new();
    with.set_mesh_occluders(vec![hull_proxy()]);
    let proxied = render(&device, &mut with);
    assert_eq!(with.mesh_occluders_in_use(), 1);
    let belly = project(Vec3::new(0.8, BELLY - 0.001, 0.6));
    let (b0, b1) = (luminance(&plain, belly), luminance(&proxied, belly));
    eprintln!("belly {b0:.1} → {b1:.1} cd/m²");
    // The belly sees the ground straight below it, almost all of it in the hull's shadow: the
    // sun's share of the IBL ground (most of it on Mars at 44°) goes.
    assert!(b1 < 0.75 * b0, "belly {b0:.1} → {b1:.1} cd/m²");
}

// Far band (ADR-7970): a ground slab seen from above, the proxies' meshes not drawn (only
// their terms on the ground).
const GROUND_EYE: [f32; 3] = [0.0, 30.0, 20.0];
const GROUND_TARGET: [f32; 3] = [0.0, 0.0, 0.0];

/// The `hull_proxy` box standing on the ground at `(x, 0, z)`.
fn hull_on_ground(x: f32, z: f32, far_band: bool) -> MeshOccluder {
    MeshOccluder::from_local_bounds(
        Mat4::from_translation(Vec3::new(x, 0.0, z)).to_cols_array_2d(),
        [-HALF[0], BELLY, -HALF[2]],
        [HALF[0], BELLY + 2.0 * HALF[1], HALF[2]],
        [0.3; 3],
    )
    .with_ground(0.0)
    .with_far_band(far_band)
}

fn render_ground(device: &RenderDevice, renderer: &mut FrameRenderer) -> Vec<u8> {
    let to_sun = Vec3::new(-0.4, 0.7, -0.5).normalize();
    let post = PostSettings {
        auto_exposure: false,
        exposure_ev: EV,
        bloom: false,
        fxaa: false,
        taa: false,
        contrast: 1.0,
        saturation: 1.0,
        ..PostSettings::default()
    };
    let (vertices, indices) = box_mesh([60.0, 0.05, 60.0]);
    let lab = LabScene {
        width: W,
        height: H,
        eye: GROUND_EYE,
        target: GROUND_TARGET,
        fov_y: FOV_Y,
        near: 0.05,
        far: 200.0,
        depth_prepass: true,
        directional_shadows: true,
        ibl: true,
        exposure_ev: EV,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        directional: DirectionalLightUniform::new((-to_sun).to_array(), [1.0; 3], 30_000.0),
        local_lights: Vec::new(),
        materials: vec![LabMaterial {
            base_color: [0.3, 0.3, 0.3, 1.0],
            metallic: 0.0,
            roughness: 1.0,
            ..LabMaterial::default()
        }],
        meshes: vec![LabMesh {
            vertices: translate_mesh(&vertices, [0.0, -0.05, 0.0]),
            indices,
            material: 0,
            ..LabMesh::unskinned()
        }],
    };
    let mut quality =
        FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    quality.ssao = SsaoMode::Off;
    quality.taau = false;
    quality.screen_space_gi = false;
    quality.ssr = false;
    quality.gi_probes = false;
    renderer.set_outdoor(OutdoorParams {
        planet: regia_render::AtmosphereParameters::from_preset(AtmospherePreset::Mars),
        ..OutdoorParams::default()
    });
    let frame = lab.as_frame_scene_with_quality(quality);
    let mut pixels = Vec::new();
    for _ in 0..3 {
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Pixel of a world point seen from `GROUND_EYE`.
fn project_ground(p: Vec3) -> (u32, u32) {
    let view = regia_core::glam::camera::rh::view::look_at_mat4(
        Vec3::from_array(GROUND_EYE),
        Vec3::from_array(GROUND_TARGET),
        Vec3::Y,
    );
    let proj =
        regia_core::glam::camera::rh::proj::directx::perspective(FOV_Y, W as f32 / H as f32, 0.05, 200.0);
    let clip: Vec4 = (proj * view as Mat4) * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * W as f32) as u32,
        ((0.5 - ndc.y * 0.5) * H as f32) as u32,
    )
}

/// FNV-1a of a frame (printed, so a run at another commit can be compared by hand).
fn frame_hash(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The 20 proxies of the at-most-32 case: a 5 × 4 grid in view.
fn twenty_hulls(far_band: bool) -> Vec<MeshOccluder> {
    (0..20)
        .map(|k| {
            hull_on_ground(
                -16.0 + 8.0 * (k % 5) as f32,
                -12.0 + 8.0 * (k / 5) as f32,
                far_band,
            )
        })
        .collect()
}

/// ADR-7970: with 32 proxies or fewer the far band is never used, so opting every proxy in
/// renders the same bytes as leaving it off (the base path).
#[test]
fn at_most_32_proxies_render_the_same_bytes_with_the_far_band() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let mut off = FrameRenderer::new();
    off.set_mesh_occluders(twenty_hulls(false));
    let plain = render_ground(&device, &mut off);
    let mut on = FrameRenderer::new();
    on.set_mesh_occluders(twenty_hulls(true));
    let banded = render_ground(&device, &mut on);
    assert_eq!(on.mesh_occluders_in_use(), 20);
    assert_eq!(
        on.mesh_occluder_bands(),
        regia_render::OccluderBands {
            near: 20,
            far: 0,
            dropped: 0
        }
    );
    eprintln!("twenty-hull ground frame fnv1a {:016x}", frame_hash(&plain));
    assert!(
        banded == plain,
        "opting <= 32 proxies into the far band changed the frame"
    );
}

/// ADR-7970: 40 proxies, 32 near ones out of view behind the eye and 8 in view farther out:
/// the 8 are far-band contact blobs that darken the ground under them on the GPU, counted
/// 32 / 8 / 0; with the band off they are dropped (32 / 0 / 8) and their ground is unshaded.
#[test]
fn far_band_hulls_darken_the_ground_under_them() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = RenderDevice::headless().expect("lavapipe");
    let proxies = |far_band: bool| -> Vec<MeshOccluder> {
        let near = (0..32).map(|k| {
            hull_on_ground(
                -15.0 + 2.0 * (k % 16) as f32,
                28.0 + 4.0 * (k / 16) as f32,
                far_band,
            )
        });
        let far = (0..8).map(|k| {
            hull_on_ground(
                -12.0 + 8.0 * (k % 4) as f32,
                -4.0 - 8.0 * (k / 4) as f32,
                far_band,
            )
        });
        near.chain(far).collect()
    };
    let mut off = FrameRenderer::new();
    off.set_mesh_occluders(proxies(false));
    let plain = render_ground(&device, &mut off);
    assert_eq!(
        off.mesh_occluder_bands(),
        regia_render::OccluderBands {
            near: 32,
            far: 0,
            dropped: 8
        }
    );
    let mut on = FrameRenderer::new();
    on.set_mesh_occluders(proxies(true));
    let banded = render_ground(&device, &mut on);
    assert_eq!(
        on.mesh_occluder_bands(),
        regia_render::OccluderBands {
            near: 32,
            far: 8,
            dropped: 0
        }
    );
    for (x, z) in [(-4.0, -4.0), (4.0, -12.0)] {
        let at = project_ground(Vec3::new(x, 0.0, z));
        let (b0, b1) = (luminance(&plain, at), luminance(&banded, at));
        eprintln!("ground under the far hull at ({x}, {z}): {b0:.1} → {b1:.1} cd/m²");
        assert!(b1 < 0.98 * b0, "({x}, {z}): {b0:.1} → {b1:.1} cd/m²");
    }
}
