// SPDX-License-Identifier: MPL-2.0
//! Translucent shadow casters (colored shadow map, `frame/translucent_shadow.rs`).
//!
//! An alpha-blended quad between the sun (or a spot light) and a floor lets `1 − α` of the
//! light through; a transmissive pane tints it by its base colour. Measured on the linear HDR
//! capture as the ratio of the floor's radiance with and without the caster. Frames without
//! a translucent caster do not change at all.

use regia_core::glam::Mat4;
use regia_render::{
    ALPHA_BLEND, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabLocalLight,
    LabMaterial, LabSpotLight, LabVertex, PostSettings, RenderDevice, ShadowCast,
};

const W: u32 = 96;
const H: u32 = 64;

fn device() -> Option<RenderDevice> {
    match RenderDevice::headless() {
        Ok(device) => Some(device),
        Err(err) => {
            eprintln!("skip translucent shadows: {err}");
            None
        }
    }
}

/// A horizontal square of half-size `h` at height `y`, facing up.
fn square(h: f32, y: f32) -> (Vec<LabVertex>, Vec<u32>) {
    let corners = [[-h, y, -h], [h, y, -h], [h, y, h], [-h, y, h]];
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    let vertices = corners
        .into_iter()
        .zip(uvs)
        .map(|(position, uv)| LabVertex::from_pos_normal_uv(position, [0.0, 1.0, 0.0], uv))
        .collect();
    (vertices, vec![0, 2, 1, 0, 3, 2])
}

fn floor_material() -> LabMaterial {
    LabMaterial {
        base_color: [0.8, 0.8, 0.8, 1.0],
        roughness: 1.0,
        ..LabMaterial::default()
    }
}

fn blended(alpha: f32) -> LabMaterial {
    LabMaterial {
        base_color: [1.0, 1.0, 1.0, alpha],
        alpha_mode: ALPHA_BLEND,
        roughness: 0.5,
        ..LabMaterial::default()
    }
}

fn mesh<'a>(
    key: u64,
    geometry: &'a (Vec<LabVertex>, Vec<u32>),
    material: &'a LabMaterial,
    shadow: ShadowCast,
) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: key,
        material_key: key + 100,
        object_key: key + 200,
        vertices: &geometry.0,
        indices: &geometry.1,
        material,
        model: Mat4::IDENTITY.to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow,
        uv1: &[],
        lod_dither: 0.0,
    }
}

/// Floor at y = 0 seen from below the pane's height (the pane's back face is culled and the
/// view ray to the floor centre passes under it), sun almost straight down.
fn scene<'a>(
    meshes: Vec<FrameMesh<'a>>,
    post: &'a PostSettings,
    quality: FrameQuality,
    sun: f32,
    lights: &'a [LabLocalLight],
) -> FrameScene<'a> {
    FrameScene {
        width: W,
        height: H,
        eye: [0.0, 0.9, 3.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.8,
        near: 0.1,
        far: 40.0,
        depth_prepass: false,
        directional_shadows: sun > 0.0,
        ibl: false,
        exposure_ev: 12.0,
        ambient_lux: 0.0,
        double_sided: false,
        post: Some(post),
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.02, -1.0, 0.01], [1.0; 3], sun),
        local_lights: lights,
        quality,
        meshes,
    }
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        exposure_ev: 12.0,
        ..PostSettings::default()
    }
}

/// Mean linear radiance of the 5×5 block at the image centre (the floor under the pane).
fn centre_rgb(device: &RenderDevice, scene: &FrameScene<'_>) -> [f32; 3] {
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer.render_offscreen(device, scene).expect("frame");
    let hdr = renderer
        .take_hdr_capture(device)
        .expect("readback")
        .expect("the post stack ran");
    let mut sum = [0.0f32; 3];
    for y in H / 2 - 2..=H / 2 + 2 {
        for x in W / 2 - 2..=W / 2 + 2 {
            let c = hdr.rgb[(y * W + x) as usize];
            for i in 0..3 {
                sum[i] += c[i] / 25.0;
            }
        }
    }
    sum
}

fn ratio(with: [f32; 3], without: [f32; 3]) -> [f32; 3] {
    [with[0] / without[0], with[1] / without[1], with[2] / without[2]]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_blended_pane_lets_one_minus_alpha_of_the_sun_through() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let post = post();
    let floor = square(6.0, 0.0);
    let pane = square(1.5, 1.0);
    let floor_mat = floor_material();
    let open = centre_rgb(
        &device,
        &scene(
            vec![mesh(1, &floor, &floor_mat, ShadowCast::All)],
            &post,
            FrameQuality::LAB,
            50_000.0,
            &[],
        ),
    );
    assert!(open[1] > 0.0, "the floor is sunlit: {open:?}");
    for alpha in [0.25f32, 0.5, 0.9] {
        let glass = blended(alpha);
        let shaded = centre_rgb(
            &device,
            &scene(
                vec![
                    mesh(1, &floor, &floor_mat, ShadowCast::All),
                    mesh(2, &pane, &glass, ShadowCast::All),
                ],
                &post,
                FrameQuality::LAB,
                50_000.0,
                &[],
            ),
        );
        let r = ratio(shaded, open);
        for c in r {
            assert!(
                (c - (1.0 - alpha)).abs() < 0.02,
                "alpha {alpha}: the floor keeps {r:?} of the sun, expected {}",
                1.0 - alpha
            );
        }
    }
}

/// A pane farther from the sun than the receiver (under the floor) does not tint it: only
/// what lies behind the nearest translucent depth takes the transmittance.
#[test]
fn a_pane_behind_the_receiver_does_not_tint_it() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let post = post();
    let floor = square(6.0, 0.0);
    let below = square(1.5, -0.5);
    let floor_mat = floor_material();
    let glass = blended(0.9);
    let open = centre_rgb(
        &device,
        &scene(
            vec![mesh(1, &floor, &floor_mat, ShadowCast::All)],
            &post,
            FrameQuality::LAB,
            50_000.0,
            &[],
        ),
    );
    let with = centre_rgb(
        &device,
        &scene(
            vec![
                mesh(1, &floor, &floor_mat, ShadowCast::All),
                mesh(2, &below, &glass, ShadowCast::All),
            ],
            &post,
            FrameQuality::LAB,
            50_000.0,
            &[],
        ),
    );
    let r = ratio(with, open);
    for c in r {
        assert!((c - 1.0).abs() < 0.005, "a pane under the floor tinted it: {r:?}");
    }
}

#[test]
fn a_transmissive_pane_tints_the_sun_by_its_base_colour() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let post = post();
    let floor = square(6.0, 0.0);
    let pane = square(1.5, 1.0);
    let floor_mat = floor_material();
    let glass = LabMaterial {
        base_color: [1.0, 0.5, 0.25, 1.0],
        transmission: 1.0,
        ior: 1.5,
        roughness: 0.05,
        ..LabMaterial::default()
    };
    let open = centre_rgb(
        &device,
        &scene(
            vec![mesh(1, &floor, &floor_mat, ShadowCast::All)],
            &post,
            FrameQuality::LAB,
            50_000.0,
            &[],
        ),
    );
    let shaded = centre_rgb(
        &device,
        &scene(
            vec![
                mesh(1, &floor, &floor_mat, ShadowCast::All),
                mesh(2, &pane, &glass, ShadowCast::All),
            ],
            &post,
            FrameQuality::LAB,
            50_000.0,
            &[],
        ),
    );
    // transmission · (1 − F0) · base, F0 = 0.04 at IOR 1.5.
    let expected = [0.96, 0.48, 0.24];
    let r = ratio(shaded, open);
    for (c, e) in r.iter().zip(expected) {
        assert!((c - e).abs() < 0.02, "tinted sun {r:?}, expected {expected:?}");
    }
}

#[test]
fn a_blended_pane_shadows_a_spot_light_by_one_minus_alpha() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let post = post();
    let floor = square(6.0, 0.0);
    let pane = square(1.5, 1.0);
    let floor_mat = floor_material();
    let lights = [LabLocalLight::Spot(LabSpotLight {
        position: [0.0, 3.0, 0.0],
        direction: [0.0, -1.0, 0.001],
        color: [1.0, 1.0, 1.0],
        intensity: 2_000.0,
        range: 8.0,
        inner_angle: 0.5,
        outer_angle: 0.7,
        cast_shadows: true,
        source_radius: None,
    })];
    let open = centre_rgb(
        &device,
        &scene(
            vec![mesh(1, &floor, &floor_mat, ShadowCast::All)],
            &post,
            FrameQuality::LAB,
            0.0,
            &lights,
        ),
    );
    assert!(open[1] > 0.0, "the spot lights the floor: {open:?}");
    let alpha = 0.6;
    let glass = blended(alpha);
    let shaded = centre_rgb(
        &device,
        &scene(
            vec![
                mesh(1, &floor, &floor_mat, ShadowCast::All),
                mesh(2, &pane, &glass, ShadowCast::All),
            ],
            &post,
            FrameQuality::LAB,
            0.0,
            &lights,
        ),
    );
    let r = ratio(shaded, open);
    for c in r {
        assert!(
            (c - (1.0 - alpha)).abs() < 0.03,
            "the spot keeps {r:?} through the pane, expected {}",
            1.0 - alpha
        );
    }
}

/// Without a translucent caster the frame is the same bytes whether the translucency maps are
/// enabled or not (opaque shadows, sun and spot, untouched); a blended draw that casts no
/// shadow does not switch them on.
#[test]
fn frames_without_translucent_casters_are_bit_identical() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = device() else { return };
    let post = post();
    let floor = square(6.0, 0.0);
    let blocker = square(0.6, 1.2);
    let pane = square(1.5, 1.0);
    let floor_mat = floor_material();
    let opaque = floor_material();
    let glass = blended(0.5);
    let lights = [LabLocalLight::Spot(LabSpotLight {
        position: [0.5, 3.0, 0.2],
        direction: [-0.1, -1.0, 0.0],
        color: [1.0, 0.9, 0.8],
        intensity: 800.0,
        range: 8.0,
        inner_angle: 0.4,
        outer_angle: 0.7,
        cast_shadows: true,
        source_radius: None,
    })];
    let mut off = FrameQuality::LAB;
    off.translucent_shadow_cascades = 0;
    off.translucent_local_shadows = false;
    let meshes = || {
        vec![
            mesh(1, &floor, &floor_mat, ShadowCast::All),
            mesh(3, &blocker, &opaque, ShadowCast::All),
            mesh(2, &pane, &glass, ShadowCast::Off),
        ]
    };
    let render = |quality: FrameQuality| {
        let mut renderer = FrameRenderer::new();
        let frame = renderer
            .render_offscreen(&device, &scene(meshes(), &post, quality, 50_000.0, &lights))
            .expect("frame");
        let report = renderer.shadow_cache_report().clone();
        (frame.pixels, report)
    };
    let (on_pixels, on_report) = render(FrameQuality::LAB);
    let (off_pixels, _) = render(off);
    assert_eq!(on_report.translucent_casters, 0);
    assert_eq!(on_report.translucent_passes, 0);
    assert!(
        on_pixels == off_pixels,
        "a frame without translucent casters changed"
    );
}
