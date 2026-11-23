// SPDX-License-Identifier: MPL-2.0
//! Particles under the temporal upscalers (ADR-3101).
//!
//! A bright particle moves fast over a striped wall while the camera pans, through TSR, TAAU and
//! the FSR 3.1 port at render scale 0.67 (and TSR at 0.5, ADR-4859), through the real `FrameRenderer` and the VFX
//! `TranslucentLayer`. The same frames are rendered without the particle. The translucent layers
//! draw after the temporal resolve, into a copy of its output (the resolve is next frame's
//! history), so the particle never enters any upscaler's history: N frames after it passes, the
//! pixels it covered match the particle-free run, and so does every pixel away from it. A
//! reactive mask would have no trail to remove there; ADR-3101 records why the VFX mask is not
//! fed to the upscalers. The vendor slot is checked the same way with the scripted binding
//! (`regia_render::upscaler::vendor::scripted`), which also records that the frame hands it no
//! reactive mask while OIT is off.

use std::sync::{Arc, Mutex};

use regia_core::math::Vec3;
use regia_render::upscaler::VendorUpscaler;
use regia_render::upscaler::vendor::dlss::GpuBinding;
use regia_render::upscaler::vendor::scripted::{
    Recorder, ScriptConfig, ScriptLog, ScriptedVendor, install_binding_factory,
};
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PostSettings, RenderDevice, TemporalUpscaler,
};
use regia_vfx::format::{
    BlendMode, ColorKey, EmitterDef, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef, SpawnMode,
    SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VelocityMode,
    VfxSystemDef,
};
use regia_vfx::sim::LodInput;
use regia_vfx::{VfxGpu, VfxProfile, VfxScene, translucent_layer};

const W: u32 = 192;
const H: u32 = 108;
const FOV: f32 = 0.9;
/// Camera distance to the wall (m).
const DIST: f32 = 6.0;
/// Camera pan per frame (m), about 0.56 output px.
const PAN_M: f32 = 0.03;
/// Frames rendered.
const FRAMES: u32 = 32;
/// Frames before scoring starts (the histories settle).
const SETTLE: u32 = 12;
/// How many frames behind the particle the trail is read.
const TRAIL_LAG: usize = 8;
/// Output pixels around the particle's current centre that belong to the particle itself.
const FOOTPRINT_PX: f32 = 12.0;
/// Render scale of the brief (permille).
const SCALE_PERMILLE: u16 = 670;
/// TSR's half-scale mode (permille, ADR-4859).
const HALF_SCALE_PERMILLE: u16 = 500;

static SERIAL_GPU: Mutex<()> = Mutex::new(());

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 64,
        decal_capacity: 4,
        sort: true,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 64,
        soft_particle_res: regia_vfx::gpu::SoftParticleRes::Full,
        event_capacity: 4,
        child_capacity: 4,
    }
}

/// One bright spark flying along +X at 12 m/s (about 3.7 output px per frame).
fn spark() -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "spark".into(),
        system: SystemModule {
            bounds: None,
            budget: 4,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "spark".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Burst(1),
                lifetime: RangeDef::from(30.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Point,
                velocity: VelocityDef {
                    mode: VelocityMode::Directional {
                        // Emitter-local: x is `forward × up`, world -X for the +Z forward this
                        // test spawns with, so local -x flies along world +X across the view.
                        // (Local +x left the frame after five frames and nothing was scored.)
                        dir: (-1.0, 0.0, 0.0),
                        spread: 0.0,
                        speed: RangeDef::from(12.0),
                    },
                    inherit: 0.0,
                },
                size: RangeDef::from(0.4),
                rotation: RangeDef::from(0.0),
                spin: RangeDef::from(0.0),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#fff2c0".into(),
                }],
            },
            update: UpdateModule::default(),
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::SoftDisc,
                blend: BlendMode::Additive,
                emissive: 6.0,
                ..RenderModule::default()
            },
        }],
    }
}

fn vertex(x: f32, y: f32) -> LabVertex {
    LabVertex {
        position: [x, y, 0.0],
        normal: [0.0, 0.0, 1.0],
        uv: [0.0, 0.0],
        tangent: [1.0, 0.0, 0.0, 1.0],
        color: [1.0; 4],
        joints: [0; 4],
        weights: [0.0; 4],
    }
}

/// The wall facing +Z: a dark backing and 0.15 m bright vertical stripes every 0.3 m.
fn wall() -> [(Vec<LabVertex>, Vec<u32>); 2] {
    let quad = |x0: f32, x1: f32, z_bias: f32, v: &mut Vec<LabVertex>, i: &mut Vec<u32>| {
        let base = v.len() as u32;
        for (x, y) in [(x0, -4.0), (x1, -4.0), (x1, 4.0), (x0, 4.0)] {
            let mut p = vertex(x, y);
            p.position[2] = z_bias;
            v.push(p);
        }
        i.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    };
    let (mut back_v, mut back_i) = (Vec::new(), Vec::new());
    quad(-12.0, 12.0, -0.01, &mut back_v, &mut back_i);
    let (mut stripe_v, mut stripe_i) = (Vec::new(), Vec::new());
    let mut x = -12.0;
    while x < 12.0 {
        quad(x, x + 0.15, 0.0, &mut stripe_v, &mut stripe_i);
        x += 0.3;
    }
    [(back_v, back_i), (stripe_v, stripe_i)]
}

fn quality(scale_permille: u16) -> FrameQuality {
    let mut q = FrameQuality::LAB;
    q.taau = true;
    q.depth_velocity = true;
    q.render_scale_permille = scale_permille;
    q
}

fn post() -> PostSettings {
    PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 15.0,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    }
}

/// Renders the pan through `upscaler` (or the vendor slot), with or without the spark; returns
/// every frame's pixels.
fn run(
    device: &RenderDevice,
    upscaler: TemporalUpscaler,
    vendor: Option<VendorUpscaler>,
    spark_on: bool,
    scale_permille: u16,
) -> Vec<Vec<u8>> {
    let gpu = match VfxGpu::new(device, profile()) {
        Ok(gpu) => Arc::new(Mutex::new(gpu)),
        Err(err) => panic!("vfx gpu: {err}"),
    };
    let mut renderer = FrameRenderer::new();
    renderer.set_temporal_upscaler(upscaler);
    renderer.set_vendor_upscaler(vendor);
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    if let Err(err) = lib.add(spark()) {
        panic!("spark def: {err:?}");
    }
    let mut scene = VfxScene::new(lib);
    if spark_on
        && scene
            .spawn("spark", Vec3::new(-3.5, 0.3, 0.5), Vec3::Z, 7)
            .is_none()
    {
        panic!("spark did not spawn");
    }
    let [(back_v, back_i), (stripe_v, stripe_i)] = wall();
    let dark = LabMaterial {
        base_color: [0.08, 0.08, 0.1, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let bright = LabMaterial {
        base_color: [0.7, 0.7, 0.65, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let post = post();
    let mut frames = Vec::new();
    for frame in 0..FRAMES {
        let x = PAN_M * frame as f32;
        let eye = [x, 0.0, DIST];
        {
            let mut locked = gpu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            scene.update(
                1.0 / 60.0,
                &LodInput {
                    camera_pos: Vec3::from_array(eye),
                },
                64,
            );
            scene.upload(&mut locked);
        }
        let scene_frame = FrameScene {
            width: W,
            height: H,
            eye,
            target: [x, 0.0, 0.0],
            fov_y: FOV,
            near: 0.1,
            far: 60.0,
            depth_prepass: true,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 15.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: Some(&post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.2, -0.4, -1.0], [1.0, 1.0, 1.0], 100_000.0),
            local_lights: &[],
            quality: quality(scale_permille),
            meshes: vec![
                FrameMesh {
                    is_static: false,
                    mesh_key: 1,
                    material_key: 1,
                    object_key: 1,
                    vertices: &back_v,
                    indices: &back_i,
                    material: &dark,
                    model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: regia_render::ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                },
                FrameMesh {
                    is_static: false,
                    mesh_key: 2,
                    material_key: 2,
                    object_key: 2,
                    vertices: &stripe_v,
                    indices: &stripe_i,
                    material: &bright,
                    model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: regia_render::ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                },
            ],
        };
        match renderer.render_offscreen(device, &scene_frame) {
            Ok(result) => frames.push(result.pixels),
            Err(err) => panic!("frame {frame}: {err}"),
        }
    }
    if vendor.is_some() {
        let report = renderer.vendor_upscaler_report();
        assert!(report.active, "the vendor slot did not run: {report:?}");
        assert_eq!(report.fallbacks, 0, "{report:?}");
    }
    frames
}

/// Largest channel difference of pixel `i` between two frames.
fn diff(a: &[u8], b: &[u8], i: usize) -> u8 {
    (0..3)
        .map(|c| a[i * 4 + c].abs_diff(b[i * 4 + c]))
        .max()
        .unwrap_or(0)
}

/// What one upscaler's run shows.
#[derive(Debug, Default)]
struct Scores {
    /// Largest channel difference away from the particle's current footprint, any scored frame.
    away_max: u8,
    /// Mean channel difference (8-bit levels) over the particle's footprint `TRAIL_LAG` frames ago,
    /// outside its current one, worst scored frame.
    trail_energy: f32,
    /// Pixels of the smallest footprint seen (the particle drew).
    min_footprint: usize,
}

fn score(with: &[Vec<u8>], without: &[Vec<u8>]) -> Scores {
    let mut centres: Vec<Option<(f32, f32)>> = Vec::new();
    let mut out = Scores {
        min_footprint: usize::MAX,
        ..Scores::default()
    };
    for (frame, (a, b)) in with.iter().zip(without).enumerate() {
        // The footprint: pixels the spark visibly brightens.
        let (mut sx, mut sy, mut n) = (0.0, 0.0, 0usize);
        for y in 0..H {
            for x in 0..W {
                let i = (y * W + x) as usize;
                if diff(a, b, i) > 24 {
                    sx += x as f32;
                    sy += y as f32;
                    n += 1;
                }
            }
        }
        let centre = (n > 0).then(|| (sx / n as f32, sy / n as f32));
        centres.push(centre);
        if (frame as u32) < SETTLE {
            continue;
        }
        let Some((cx, cy)) = centre else {
            out.min_footprint = 0;
            continue;
        };
        out.min_footprint = out.min_footprint.min(n);
        let old = frame
            .checked_sub(TRAIL_LAG)
            .and_then(|f| centres.get(f).copied().flatten());
        let (mut trail, mut trail_n) = (0.0f32, 0.0f32);
        for y in 0..H {
            for x in 0..W {
                let (fx, fy) = (x as f32, y as f32);
                if (fx - cx).hypot(fy - cy) <= FOOTPRINT_PX {
                    continue;
                }
                let i = (y * W + x) as usize;
                let d = diff(a, b, i);
                out.away_max = out.away_max.max(d);
                if let Some((ox, oy)) = old
                    && (fx - ox).hypot(fy - oy) <= FOOTPRINT_PX * 0.5
                {
                    trail += f32::from(d);
                    trail_n += 1.0;
                }
            }
        }
        if trail_n > 0.0 {
            out.trail_energy = out.trail_energy.max(trail / trail_n);
        }
    }
    out
}

fn check(upscaler: TemporalUpscaler, scale_permille: u16) {
    let _guard = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let with = run(&device, upscaler, None, true, scale_permille);
    let without = run(&device, upscaler, None, false, scale_permille);
    let label = format!("{upscaler:?} at {}", f32::from(scale_permille) / 1000.0);
    assert_no_trail(&label, &with, &without);
}

fn assert_no_trail(upscaler: &str, with: &[Vec<u8>], without: &[Vec<u8>]) {
    let s = score(with, without);
    eprintln!("{upscaler}: {s:?}");
    assert!(
        s.min_footprint >= 8 && s.min_footprint != usize::MAX,
        "{upscaler}: the spark must draw on every scored frame ({s:?})"
    );
    // No trail: TRAIL_LAG frames after the spark passed, its old footprint is the particle-free
    // frame within one 8-bit level, and so is every pixel away from it.
    assert!(
        s.trail_energy <= 1.0,
        "{upscaler}: a trail behind the particle ({s:?})"
    );
    assert!(
        s.away_max <= 1,
        "{upscaler}: pixels away from the particle moved ({s:?})"
    );
}

#[test]
fn tsr_keeps_no_particle_trail_at_067() {
    check(TemporalUpscaler::Tsr, SCALE_PERMILLE);
}

/// TSR's half-scale mode keeps no trail either (ADR-4859): the layers draw after its resolve too.
#[test]
fn tsr_keeps_no_particle_trail_at_050() {
    check(TemporalUpscaler::Tsr, HALF_SCALE_PERMILLE);
}

#[test]
fn taau_keeps_no_particle_trail_at_067() {
    check(TemporalUpscaler::Taau, SCALE_PERMILLE);
}

#[test]
fn fsr3_keeps_no_particle_trail_at_067() {
    check(TemporalUpscaler::Fsr3, SCALE_PERMILLE);
}

#[test]
fn the_vendor_slot_keeps_no_particle_trail_and_gets_no_mask_from_it() {
    let _guard = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let vendor = VendorUpscaler::Dlss;
    let log = ScriptLog::new(false);
    let recording = log.clone();
    let _factory = install_binding_factory(move |asked, _device| {
        let binding: Box<dyn GpuBinding> = Box::new(Recorder::new(
            ScriptedVendor::new(ScriptConfig::ready(asked)),
            recording.clone(),
        ));
        Some(binding)
    });
    let with = run(
        &device,
        TemporalUpscaler::Taau,
        Some(vendor),
        true,
        SCALE_PERMILLE,
    );
    let evals = log.evals();
    assert_eq!(
        evals.len(),
        FRAMES as usize,
        "the scripted binding evaluated every frame"
    );
    // OIT is off: the frame hands the vendor no reactive mask, particles or not.
    assert!(evals.iter().all(|eval| eval.reactive.is_none()));
    let without = run(
        &device,
        TemporalUpscaler::Taau,
        Some(vendor),
        false,
        SCALE_PERMILLE,
    );
    assert_no_trail("scripted vendor", &with, &without);
}
