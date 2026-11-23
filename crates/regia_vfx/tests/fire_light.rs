// SPDX-License-Identifier: MPL-2.0
//! Fire light on the real renderer path: the deterministic flicker moves the brightness of a
//! lit sphere, the same frame renders the same pixels, and a burnt-out fire adds nothing.
//!
//! Needs a headless GPU (lavapipe is enough). No golden images: the checks are relations
//! between renders of one run.

use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice,
};
use regia_vfx::lights::VfxFrameLights;
use regia_vfx::{FireLight, FireLightInstance, FireLights, particle_point_lights};

const W: u32 = 160;
const H: u32 = 120;
const EYE: [f32; 3] = [0.0, 1.5, 6.0];
const LOOK: [f32; 3] = [0.0, 1.0, 0.0];
const FOV: f32 = 0.9;
const SPHERE: [f32; 3] = [0.0, 1.0, 0.0];
const LIGHT: [f32; 3] = [2.5, 1.8, 2.5];
const EV: f32 = 10.0;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A unit UV sphere around [`SPHERE`].
fn sphere() -> (Vec<LabVertex>, Vec<u32>) {
    let (stacks, slices) = (20u32, 32u32);
    let mut vertices = Vec::new();
    for i in 0..=stacks {
        let theta = std::f32::consts::PI * i as f32 / stacks as f32;
        for j in 0..=slices {
            let phi = std::f32::consts::TAU * j as f32 / slices as f32;
            let n = [theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()];
            vertices.push(LabVertex {
                position: [SPHERE[0] + n[0], SPHERE[1] + n[1], SPHERE[2] + n[2]],
                normal: n,
                uv: [0.0, 0.0],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            });
        }
    }
    let mut indices = Vec::new();
    for i in 0..stacks {
        for j in 0..slices {
            let a = i * (slices + 1) + j;
            let b = a + slices + 1;
            indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    (vertices, indices)
}

/// Renders the sphere under `lights` (night: no sun, a little ambient).
fn render(device: &RenderDevice, lights: &VfxFrameLights) -> Vec<u8> {
    let point = particle_point_lights(lights);
    let (vertices, indices) = sphere();
    let material = LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: EV,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    };
    let mut renderer = FrameRenderer::new();
    let mut pixels = Vec::new();
    // Two frames: the first warms any history, the second is the one compared.
    for _ in 0..2 {
        let frame = FrameScene {
            width: W,
            height: H,
            eye: EYE,
            target: LOOK,
            fov_y: FOV,
            near: 0.1,
            far: 60.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: EV,
            ambient_lux: 5.0,
            double_sided: true,
            post: Some(&post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 0.0),
            local_lights: &point,
            quality: FrameQuality::LAB,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &indices,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        pixels = renderer.render_offscreen(device, &frame).expect("frame").pixels;
    }
    pixels
}

/// Mean display luma (0..1) of the whole frame. The fire is the only light, so the sum over
/// every pixel is a robust brightness measure that does not depend on which side of the
/// sphere the camera sees lit.
fn lit_side_luma(pixels: &[u8]) -> f32 {
    let (sum, n) = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .fold((0.0f32, 0.0f32), |(s, n), p| {
            (
                s + 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]),
                n + 1.0,
            )
        });
    sum / n / 255.0
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn fires() -> FireLights {
    let mut fires = FireLights::default();
    fires.spawn(FireLightInstance {
        position: Vec3::from_array(LIGHT),
        radius: 20.0,
        light: FireLight {
            base_intensity: 1.0,
            color_temperature_k: 1800.0,
            flicker_depth: 0.6,
            flicker_hz: 8.0,
            seed: 77,
            lifetime_frames: 300,
        },
        start_frame: 0,
    });
    fires
}

/// The published particle lights of the fire clock at `frame`.
fn lights_at(frame: u64) -> VfxFrameLights {
    let mut fires = fires();
    fires.set_frame(frame);
    let mut lights = VfxFrameLights::default();
    lights.gather(fires.candidates(), 4);
    lights
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438, ADR-1440). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn flicker_moves_the_sphere_brightness_and_the_same_frame_renders_identically() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let light = fires().fires()[0].light;
    let (bright_frame, dim_frame) = {
        let samples: Vec<(u64, f32)> = (0..240).map(|f| (f, light.sample(f).intensity)).collect();
        let bright = samples
            .iter()
            .copied()
            .fold((0, f32::MIN), |a, b| if b.1 > a.1 { b } else { a });
        let dim = samples
            .iter()
            .copied()
            .fold((0, f32::MAX), |a, b| if b.1 < a.1 { b } else { a });
        assert!(
            bright.1 > 1.5 * dim.1,
            "the flicker swings enough: {dim:?} .. {bright:?}"
        );
        (bright.0, dim.0)
    };
    let bright = render(&device, &lights_at(bright_frame));
    let dim = render(&device, &lights_at(dim_frame));
    let off = render(&device, &VfxFrameLights::default());
    let (lb, ld, lo) = (lit_side_luma(&bright), lit_side_luma(&dim), lit_side_luma(&off));
    assert!(
        lb > ld * 1.05,
        "the swell is brighter than the lull: {ld:.4} .. {lb:.4}"
    );
    assert!(
        ld > lo * 1.5 + 0.0005,
        "even the lull lights the sphere: {lo:.4} .. {ld:.4}"
    );

    // The same frame, rendered again: identical bytes (hence identical hash).
    let again = render(&device, &lights_at(bright_frame));
    assert_eq!(bright, again);
    assert_eq!(fnv1a(&bright), fnv1a(&again));
    assert_ne!(fnv1a(&bright), fnv1a(&dim), "different frames render differently");

    if let Ok(dir) = regia_core::brand::env_var("REGIA_FIRE_LIGHT_DUMP") {
        let _ = std::fs::create_dir_all(&dir);
        for (name, pixels) in [("bright", &bright), ("dim", &dim), ("off", &off)] {
            let file = std::fs::File::create(std::path::Path::new(&dir).join(format!("fire_{name}.png")))
                .expect("png create");
            let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(pixels).expect("png data");
        }
    }
}

#[test]
fn a_burnt_out_fire_adds_no_light() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let spent = lights_at(300);
    assert!(spent.lights.is_empty(), "no light from the first dark frame on");
    let off = render(&device, &VfxFrameLights::default());
    assert_eq!(render(&device, &spent), off);
    let lit = render(&device, &lights_at(10));
    assert!(lit_side_luma(&lit) > lit_side_luma(&off) * 1.5 + 0.0005);
}
