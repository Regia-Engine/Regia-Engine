// SPDX-License-Identifier: MPL-2.0
//! The entry effect through the real frame path (lavapipe or a GPU): bright while it glows,
//! unlit in dark flight, and byte-identical across two renders.
//!
//! Camera: Shot 4's position (3.3 km SW of the site), 4K-style framing at a small size.
//! `REGIA_ENTRY_DUMP=<dir>` writes PNGs for the evidence sheets.

use std::sync::{Arc, Mutex};

use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice,
};
use regia_vfx::entry::{
    EntryProfile, EntryTier, EntryTrack, EntryView, TrackPoint, build_vertices, evaluate,
};
use regia_vfx::gpu::TrailVertex;
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxGpu, VfxProfile, VfxScene, translucent_layer};

const W: u32 = 320;
const H: u32 = 180;
const EYE: [f32; 3] = [-1_752.0, 353.2, -2_203.0];
const FOV: f32 = 0.5;

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 256,
        decal_capacity: 16,
        sort: true,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 256,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 16,
        child_capacity: 16,
    }
}

fn track() -> EntryTrack {
    let at = |time: f32, alt: f32, dist: f32| TrackPoint {
        time,
        position: Vec3::new(700.0 + 0.643 * dist, alt, 5.0 + 0.766 * dist),
    };
    EntryTrack::new(vec![
        at(0.0, 80_000.0, 140_000.0),
        at(6.0, 56_000.0, 78_000.0),
        at(9.0, 42_000.0, 52_000.0),
        at(12.0, 31_000.0, 36_000.0),
        at(15.0, 23_000.0, 26_000.0),
        at(18.0, 17_500.0, 19_000.0),
        at(100.0, 3_000.0, 1_500.0),
        at(120.0, 105.7, 0.0),
    ])
    .expect("track")
}

fn ground() -> Vec<LabVertex> {
    let half = 60_000.0;
    [[-half, -half], [half, -half], [half, half], [-half, half]]
        .into_iter()
        .map(|[x, z]| LabVertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            tangent: [1.0, 0.0, 0.0, 1.0],
            color: [1.0; 4],
            joints: [0; 4],
            weights: [0.0; 4],
        })
        .collect()
}

const GROUND_INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

fn ribbons(time_s: f32, tier: EntryTier) -> Vec<TrailVertex> {
    let profile = EntryProfile::default();
    let frame = evaluate(&track(), &profile, tier, time_s);
    let focal_px = H as f32 / (2.0 * (0.5 * FOV).tan());
    let view = EntryView {
        camera: Vec3::from_array(EYE),
        focal_px,
        min_px: 2.0,
        shutter_s: 1.0 / 60.0,
        far_m: f32::INFINITY,
    };
    let mut out = Vec::new();
    build_vertices(&frame, &view, &profile, &mut out);
    out
}

/// Renders the ground and `ribbon`, looking at `target`.
fn render(device: &RenderDevice, target: Vec3, ribbon: &[TrailVertex]) -> Vec<u8> {
    let gpu = Arc::new(Mutex::new(VfxGpu::new(device, profile()).expect("vfx gpu")));
    let mut renderer = FrameRenderer::new();
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let mut scene = VfxScene::new(regia_vfx::scene::VfxLibrary::new());
    let vertices = ground();
    let material = LabMaterial {
        base_color: [0.3, 0.3, 0.3, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 15.0,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    };
    let mut pixels = Vec::new();
    for _ in 0..2 {
        {
            let mut locked = gpu.lock().expect("vfx gpu");
            scene.update(
                1.0 / 60.0,
                &LodInput {
                    camera_pos: Vec3::from_array(EYE),
                },
                256,
            );
            scene.upload(&mut locked);
            locked.write_decals_with(&[], &[]);
            locked.write_trails(ribbon);
        }
        let frame = FrameScene {
            width: W,
            height: H,
            eye: EYE,
            target: target.to_array(),
            fov_y: FOV,
            near: 1.0,
            far: 400_000.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 15.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: Some(&post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 100_000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &GROUND_INDICES,
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

fn fnv(pixels: &[u8]) -> u64 {
    pixels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn dump(label: &str, pixels: &[u8]) {
    let Ok(dir) = regia_core::brand::env_var("REGIA_ENTRY_DUMP") else {
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{label}.png"));
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(&path).expect("png create");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(pixels).expect("png data");
}

/// Brightest red-channel byte in the frame.
fn peak(pixels: &[u8]) -> u8 {
    pixels
        .chunks(4)
        .map(|p| p[0].max(p[1]).max(p[2]))
        .max()
        .unwrap_or(0)
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn bolide_glows_and_dark_flight_does_not() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let t = track();
    // The glowing head at 10.4 s, framed on the body.
    let aloft_target = t.position_at(10.4);
    let empty = render(&device, aloft_target, &[]);
    let aloft = render(&device, aloft_target, &ribbons(10.4, EntryTier::Full));
    dump("aloft_full", &aloft);
    assert!(
        peak(&aloft) > peak(&empty).saturating_add(60),
        "the head must brighten the frame: {} vs {}",
        peak(&aloft),
        peak(&empty)
    );
    // Dark flight at 118 s over the site: nothing brighter than the bare frame.
    let site = Vec3::new(700.0, 105.7, 5.0);
    let base = render(&device, site, &[]);
    let dark = render(&device, site, &ribbons(118.0, EntryTier::Full));
    dump("dark_full", &dark);
    assert!(
        peak(&dark) <= peak(&base).saturating_add(4),
        "dark flight must not glow: {} vs {}",
        peak(&dark),
        peak(&base)
    );
    assert_ne!(fnv(&dark), fnv(&base), "the dark body still draws");
}

#[test]
fn entry_frames_are_hash_stable() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let target = track().position_at(10.4);
    for tier in [EntryTier::Full, EntryTier::Cheap] {
        let a = render(&device, target, &ribbons(10.4, tier));
        let b = render(&device, target, &ribbons(10.4, tier));
        assert_eq!(fnv(&a), fnv(&b), "{tier:?}");
        dump(&format!("stable_{tier:?}"), &a);
    }
}
