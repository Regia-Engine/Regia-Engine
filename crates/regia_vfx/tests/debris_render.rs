// SPDX-License-Identifier: MPL-2.0
//! Mesh debris through the real frame renderer on lavapipe: the picture at a given clock frame is
//! a pure function of that frame (same hash on two runs, and after seeking away and back), the
//! debris is visible, and nothing is drawn before the burst begins. No golden image is compared;
//! the numbers come from the run itself.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{
    AntiAliasing, Camera, CpuMaterial, CpuMesh, DirectionalLight, ExtractedFrame, FrameRenderer, LabVertex,
    MeshRenderer, RenderDevice, RenderSettings, SsaoMode, extract_frame,
};
use regia_scene::prelude::{Transform, propagate_transforms};
use regia_vfx::debris_mesh::{DebrisClass, DebrisMesh, DebrisSpec};
use regia_vfx::{DebrisBursts, DebrisClock, DebrisMeshLibrary, DebrisPlugin};

const W: u32 = 256;
const H: u32 = 144;
const START: u64 = 30;

fn device() -> RenderDevice {
    RenderDevice::headless().unwrap_or_else(|err| panic!("no headless GPU (lavapipe?): {err}"))
}

/// A unit cube, 24 vertices.
fn cube() -> Arc<CpuMesh> {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
    ];
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for (n, u, v) in faces {
        let base = vertices.len() as u32;
        for (su, sv, uv) in [
            (-1.0f32, -1.0f32, [0.0, 0.0]),
            (1.0, -1.0, [1.0, 0.0]),
            (1.0, 1.0, [1.0, 1.0]),
            (-1.0, 1.0, [0.0, 1.0]),
        ] {
            let p = [
                0.5 * (n[0] + su * u[0] + sv * v[0]),
                0.5 * (n[1] + su * u[1] + sv * v[1]),
                0.5 * (n[2] + su * u[2] + sv * v[2]),
            ];
            vertices.push(LabVertex::from_pos_normal_uv(p, n, uv));
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    Arc::new(CpuMesh { vertices, indices })
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(regia_render::RenderProfilePreset::DesktopHigh);
    settings.profile.gi_probes = false;
    settings.profile.screen_space_gi = false;
    settings.profile.ssr = false;
    settings.profile.volumetric_fog = false;
    settings.profile.volumetric_clouds = false;
    settings.profile.physical_atmosphere = false;
    settings.profile.gpu_driven = false;
    settings.profile.reflection_probes = false;
    settings.profile.bloom = false;
    settings.profile.auto_exposure = false;
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.depth_prepass = false;
    settings.profile.ssao = SsaoMode::Off;
    settings.profile.contact_shadows = false;
    settings.profile.motion_blur = false;
    settings.profile.depth_of_field = false;
    settings.post.bloom = false;
    settings.post.auto_exposure = false;
    settings.post.taa = false;
    settings.post.fxaa = false;
    settings.post.exposure_ev = 14.0;
    settings
}

fn flat(_x: f32, _z: f32) -> f32 {
    0.0
}

/// The test burst: slower and larger than the showcase default so a 256 x 144 frame sees it.
fn spec(budget: u32) -> DebrisSpec {
    let mut spec = DebrisSpec::earth_blast(Vec3::new(0.0, 0.0, 0.0), 0x00de_b215, START);
    for class in &mut spec.classes {
        class.speed = [class.speed[0] * 0.2, class.speed[1] * 0.2];
        class.size = [class.size[0] * 2.0, class.size[1] * 2.0];
    }
    spec.apply_budget(budget);
    spec
}

struct Scene {
    app: App,
    renderer: FrameRenderer,
}

fn scene() -> Scene {
    scene_with(300)
}

fn scene_with(budget: u32) -> Scene {
    let mut app = App::new();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(settings());
    app.add_systems(Render, extract_frame);
    app.add_plugins(DebrisPlugin);
    app.world_mut().resource_mut::<DebrisClock>().auto = false;
    let mut library = DebrisMeshLibrary::default();
    for (class, albedo) in [
        (DebrisClass::Earth, [0.35, 0.25, 0.15, 1.0]),
        (DebrisClass::Stone, [0.5, 0.5, 0.5, 1.0]),
        (DebrisClass::Turf, [0.2, 0.35, 0.12, 1.0]),
        (DebrisClass::Hardware, [0.8, 0.8, 0.85, 1.0]),
    ] {
        library.add(
            class,
            DebrisMesh {
                mesh: cube(),
                material: Arc::new(CpuMaterial {
                    base_color: albedo,
                    roughness: 0.9,
                    ..CpuMaterial::default()
                }),
                hot_material: None,
            },
        );
    }
    *app.world_mut().resource_mut::<DebrisMeshLibrary>() = library;
    app.world_mut()
        .resource_mut::<DebrisBursts>()
        .spawn(spec(budget), &flat)
        .expect("burst");
    // Ground: a big flat cube slab under the origin.
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(0.0, -0.5, 0.0),
            rotation: regia_core::math::Quat::IDENTITY,
            scale: Vec3::new(400.0, 1.0, 400.0),
        },
        MeshRenderer::new(
            cube(),
            Arc::new(CpuMaterial {
                base_color: [0.3, 0.3, 0.28, 1.0],
                roughness: 1.0,
                ..CpuMaterial::default()
            }),
        ),
    ));
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 12.0, 60.0)),
        Camera {
            fov_y: 0.9,
            exposure_ev: Some(14.0),
            near: 0.1,
            far: 500.0,
            ..Camera::default()
        },
    ));
    app.world_mut().spawn((
        Transform::IDENTITY,
        DirectionalLight {
            direction: [-0.3, -0.7, -0.5],
            color: [1.0, 0.98, 0.95],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    Scene {
        app,
        renderer: FrameRenderer::new(),
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

impl Scene {
    /// Seeks the clock to `frame` and renders. Returns the pixels.
    fn render(&mut self, device: &RenderDevice, frame: u64) -> Vec<u8> {
        self.render_at(device, frame, W, H)
    }

    fn render_at(&mut self, device: &RenderDevice, frame: u64, w: u32, h: u32) -> Vec<u8> {
        self.app.world_mut().resource_mut::<DebrisClock>().frame = frame;
        self.app.update();
        propagate_transforms(self.app.world_mut());
        self.app.update();
        let extracted = self.app.world().resource::<ExtractedFrame>().clone();
        let scene = extracted.frame_scene(0, w, h);
        self.renderer
            .render_offscreen(device, &scene)
            .unwrap_or_else(|err| panic!("frame {frame}: {err}"))
            .pixels
    }
}

fn differing_pixels(a: &[u8], b: &[u8]) -> usize {
    a.as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .filter(|(p, q)| p != q)
        .count()
}

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_hull_swap_is_frame_pure() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use regia_vfx::{DamageSwap, ImpactSitePlugin};
    let device = device();
    let mut s = scene();
    s.app.add_plugins(ImpactSitePlugin);
    let swap_at = START + 5;
    let intact_pose = Transform {
        translation: Vec3::new(0.0, 8.0, 0.0),
        rotation: regia_core::math::Quat::IDENTITY,
        scale: Vec3::splat(6.0),
    };
    // The wreck lies half sunk and leaning, wider than it is tall.
    let wreck_pose = Transform {
        translation: Vec3::new(0.0, 0.5, 0.0),
        rotation: regia_core::math::Quat::from_rotation_z(0.2),
        scale: Vec3::new(8.0, 3.0, 5.0),
    };
    s.app.world_mut().spawn((
        intact_pose,
        MeshRenderer::new(
            cube(),
            Arc::new(CpuMaterial {
                base_color: [0.6, 0.6, 0.65, 1.0],
                roughness: 0.4,
                ..CpuMaterial::default()
            }),
        ),
        DamageSwap::new(
            swap_at,
            cube(),
            Arc::new(CpuMaterial {
                base_color: [0.25, 0.22, 0.2, 1.0],
                roughness: 0.9,
                ..CpuMaterial::default()
            }),
            wreck_pose,
        ),
    ));
    let before = s.render(&device, swap_at - 1);
    let after = s.render(&device, swap_at);
    let later = s.render(&device, swap_at + 200);
    assert!(
        differing_pixels(&before, &after) > 200,
        "the battered hull must look different from the intact one"
    );
    assert_eq!(
        fnv(&after),
        fnv(&s.render(&device, swap_at)),
        "the swap frame renders the same twice"
    );
    let back = s.render(&device, swap_at - 1);
    assert_eq!(fnv(&before), fnv(&back), "a seek back restores the intact hull");
    assert_ne!(
        fnv(&after),
        fnv(&later),
        "the debris keeps moving around the wreck"
    );
}

const FRAMES: [u64; 4] = [START - 10, START + 40, START + 120, START + 600];

#[test]
fn the_picture_at_a_frame_is_the_same_on_two_runs() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let hashes =
        |scene: &mut Scene| -> Vec<u64> { FRAMES.iter().map(|f| fnv(&scene.render(&device, *f))).collect() };
    let first = hashes(&mut scene());
    let second = hashes(&mut scene());
    eprintln!("debris frame hashes: {first:016x?}");
    assert_eq!(first, second, "two runs must render identical frames");
}

#[test]
fn seeking_away_and_back_gives_the_same_picture() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut s = scene();
    let a = s.render(&device, START + 40);
    let _ = s.render(&device, START + 300);
    let _ = s.render(&device, START - 5);
    let b = s.render(&device, START + 40);
    assert_eq!(fnv(&a), fnv(&b), "a seek is not a simulation");
}

#[test]
fn debris_is_visible_and_moves() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut s = scene();
    let before = s.render(&device, START - 10);
    let up = s.render(&device, START + 40);
    let down = s.render(&device, START + 600);
    let moved = differing_pixels(&before, &up);
    let settled = differing_pixels(&before, &down);
    eprintln!(
        "pixels changed: in flight {moved}, settled {settled} of {}",
        W * H
    );
    assert!(
        moved > 100,
        "debris in flight must change the picture, saw {moved} pixels"
    );
    assert!(
        settled > 20,
        "settled debris must still be on the ground, saw {settled} pixels"
    );
    assert!(
        differing_pixels(&up, &down) > 100,
        "the flight and the rest are different pictures"
    );
}

/// Hand-run cost measurement (not part of the gate): `REGIA_GPU_UUID=... cargo test -p regia_vfx
/// --test debris_render --release -- --ignored --nocapture cost`. Renders the full 1236-piece burst
/// in flight and the same scene before the burst begins (the baseline), at 1440p and 4K, and
/// prints the wall time per frame (render plus readback) and the CPU time of the debris update.
#[test]
#[ignore = "hand-run cost measurement"]
fn cost_at_1440p_and_4k() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    eprintln!("adapter: {}", device.adapter.get_info().name);
    for (w, h) in [(2560u32, 1440u32), (3840, 2160)] {
        let mut s = scene_with(5000);
        let fragments = s.app.world().resource::<DebrisBursts>().info(START)[0].fragments;
        for (label, frame) in [("baseline (no debris)", START - 10), ("in flight", START + 60)] {
            for _ in 0..3 {
                let _ = s.render_at(&device, frame, w, h);
            }
            let runs = 10u32;
            let mut render_ms = 0.0f64;
            let mut update_ms = 0.0f64;
            for _ in 0..runs {
                s.app.world_mut().resource_mut::<DebrisClock>().frame = frame;
                let t0 = std::time::Instant::now();
                s.app.update();
                update_ms += t0.elapsed().as_secs_f64() * 1e3;
                propagate_transforms(s.app.world_mut());
                s.app.update();
                let extracted = s.app.world().resource::<ExtractedFrame>().clone();
                let scene = extracted.frame_scene(0, w, h);
                let t1 = std::time::Instant::now();
                s.renderer
                    .render_offscreen(&device, &scene)
                    .unwrap_or_else(|err| panic!("{err}"));
                render_ms += t1.elapsed().as_secs_f64() * 1e3;
            }
            eprintln!(
                "{w}x{h} {label}: {fragments} fragments, render+readback {:.2} ms/frame, \
                 app update (extract + debris drive) {:.3} ms/frame",
                render_ms / f64::from(runs),
                update_ms / f64::from(runs)
            );
        }
    }
}

/// Hand-run evidence: `REGIA_DEBRIS_SHEETS=<dir> cargo test -p regia_vfx --test debris_render --
/// --ignored --nocapture sheets` writes one PNG per listed clock frame (the full 1236-piece burst).
#[test]
#[ignore = "hand-run evidence"]
fn write_frame_sheets() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(dir) = regia_core::brand::env_var_os("REGIA_DEBRIS_SHEETS") else {
        eprintln!("REGIA_DEBRIS_SHEETS is not set; nothing written");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    let device = device();
    let mut s = scene_with(5000);
    let (w, h) = (960u32, 540u32);
    for offset in [-10i64, 15, 40, 80, 130, 220, 600] {
        let frame = START.saturating_add_signed(offset);
        let pixels = s.render_at(&device, frame, w, h);
        let path = dir.join(format!("debris_f{frame:04}.png"));
        let tmp = dir.join(format!("debris_f{frame:04}.png.tmp"));
        let file = std::fs::File::create(&tmp).unwrap_or_else(|err| panic!("{}: {err}", tmp.display()));
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap_or_else(|err| panic!("png: {err}"));
        writer
            .write_image_data(&pixels)
            .unwrap_or_else(|err| panic!("png: {err}"));
        drop(writer);
        std::fs::rename(&tmp, &path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        eprintln!("wrote {}", path.display());
    }
}
