// SPDX-License-Identifier: MPL-2.0
//! Two renderer fixes found by the reference path tracer (`specs/27` § 2):
//!
//! - `FrameMesh` with keys 0 is ephemeral: several zero-key meshes in one frame each draw
//!   (they used to share one cache slot, so all but one vanished or moved).
//! - The linear HDR target is pre-exposed (`pre_exposure_scale`): radiance far above the
//!   half-float maximum (65504) survives into the post stack and the HDR capture.

use regia_core::glam::{Mat4, Vec3};
use regia_render::{
    DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial, PostSettings,
    RenderDevice, ShadowCast,
};

const W: u32 = 96;
const H: u32 = 48;

fn device() -> RenderDevice {
    RenderDevice::headless().expect("headless device (Mesa lavapipe on CI)")
}

fn mesh<'a>(
    verts: &'a [regia_render::LabVertex],
    idx: &'a [u32],
    material: &'a LabMaterial,
    x: f32,
) -> FrameMesh<'a> {
    FrameMesh {
        is_static: false,
        mesh_key: 0,
        material_key: 0,
        object_key: 0,
        vertices: verts,
        indices: idx,
        material,
        model: Mat4::from_translation(Vec3::new(x, 0.0, 0.0)).to_cols_array_2d(),
        joints: &[],
        receiver_layer: 0,
        shadow: ShadowCast::Off,
        uv1: &[],
        lod_dither: 0.0,
    }
}

fn scene<'a>(meshes: Vec<FrameMesh<'a>>, post: Option<&'a PostSettings>, exposure_ev: f32) -> FrameScene<'a> {
    FrameScene {
        width: W,
        height: H,
        eye: [0.0, 0.0, 8.0],
        target: [0.0, 0.0, 0.0],
        fov_y: 0.7,
        near: 0.1,
        far: 50.0,
        depth_prepass: false,
        directional_shadows: false,
        ibl: false,
        exposure_ev,
        ambient_lux: 0.0,
        double_sided: false,
        post,
        fog: regia_render::FogSettings::default(),
        directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 0.0),
        local_lights: &[],
        quality: FrameQuality::LAB,
        meshes,
    }
}

fn unlit(rgb: [f32; 3]) -> LabMaterial {
    LabMaterial {
        base_color: [rgb[0], rgb[1], rgb[2], 1.0],
        unlit: true,
        ..LabMaterial::EMPTY
    }
}

/// Pixel at the centre of the left (x = −2 m) or right (x = +2 m) sphere.
fn probe(pixels: &[u8], right: bool) -> [u8; 3] {
    let x = if right { W * 3 / 4 } else { W / 4 } as usize;
    let y = (H / 2) as usize;
    let o = (y * W as usize + x) * 4;
    [pixels[o], pixels[o + 1], pixels[o + 2]]
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn zero_key_meshes_each_draw_and_re_upload_every_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (sphere_v, sphere_i) = regia_render::uv_sphere(1.0, 24, 12);
    let red = unlit([1.0, 0.0, 0.0]);
    let green = unlit([0.0, 1.0, 0.0]);
    let mut renderer = FrameRenderer::new();
    let first = renderer
        .render_offscreen(
            &device,
            &scene(
                vec![
                    mesh(&sphere_v, &sphere_i, &red, -2.0),
                    mesh(&sphere_v, &sphere_i, &green, 2.0),
                ],
                None,
                0.0,
            ),
        )
        .expect("frame");
    let (l, r) = (probe(&first.pixels, false), probe(&first.pixels, true));
    assert!(l[0] > 200 && l[1] < 40, "left sphere is red: {l:?}");
    assert!(r[1] > 200 && r[0] < 40, "right sphere is green: {r:?}");
    // Same keys (0), same buffer sizes, swapped content: the next frame must not reuse
    // the previous frame's upload or material.
    let second = renderer
        .render_offscreen(
            &device,
            &scene(
                vec![
                    mesh(&sphere_v, &sphere_i, &green, -2.0),
                    mesh(&sphere_v, &sphere_i, &red, 2.0),
                ],
                None,
                0.0,
            ),
        )
        .expect("frame");
    let (l, r) = (probe(&second.pixels, false), probe(&second.pixels, true));
    assert!(l[1] > 200 && l[0] < 40, "left sphere is green now: {l:?}");
    assert!(r[0] > 200 && r[1] < 40, "right sphere is red now: {r:?}");
}

#[test]
fn hdr_capture_holds_radiance_above_the_half_float_maximum() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let (sphere_v, sphere_i) = regia_render::uv_sphere(1.0, 24, 12);
    // A sun-like emitter (2·10⁶ cd/m², 30× the Rgba16F maximum) and a dim one (300 cd/m²).
    let emitter = |nits: f32| LabMaterial {
        base_color: [0.0, 0.0, 0.0, 1.0],
        emissive: [1.0, 1.0, 1.0],
        emissive_intensity: nits,
        ..LabMaterial::EMPTY
    };
    let (bright, dim) = (emitter(2.0e6), emitter(300.0));
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        taa: false,
        exposure_ev: 15.0,
        ..PostSettings::default()
    };
    let mut renderer = FrameRenderer::new();
    renderer.request_hdr_capture();
    renderer
        .render_offscreen(
            &device,
            &scene(
                vec![
                    FrameMesh {
                        mesh_key: 1,
                        material_key: 2,
                        object_key: 3,
                        ..mesh(&sphere_v, &sphere_i, &bright, -2.0)
                    },
                    FrameMesh {
                        mesh_key: 1,
                        material_key: 4,
                        object_key: 5,
                        ..mesh(&sphere_v, &sphere_i, &dim, 2.0)
                    },
                ],
                Some(&post),
                15.0,
            ),
        )
        .expect("frame");
    let hdr = renderer
        .take_hdr_capture(&device)
        .expect("readback")
        .expect("the post stack ran");
    let at = |right: bool| {
        let x = if right { W * 3 / 4 } else { W / 4 } as usize;
        hdr.rgb[(H / 2) as usize * W as usize + x]
    };
    let b = at(false)[1];
    let d = at(true)[1];
    // Half floats keep 11 significant bits: 0.1 % plus the emitter's own shading.
    assert!(
        (b / 2.0e6 - 1.0).abs() < 0.01,
        "bright emitter reads {b} cd/m², not 2e6"
    );
    assert!(
        (d / 300.0 - 1.0).abs() < 0.01,
        "dim emitter reads {d} cd/m², not 300"
    );
    assert!(b.is_finite() && b > 65504.0);
}
