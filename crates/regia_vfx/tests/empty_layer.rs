// SPDX-License-Identifier: MPL-2.0
//! An empty VFX layer is free (ADR-2585): once its warm frames are done, a layer with no
//! effect, decal, trail or weather leaves the frame as if it were not installed (the same
//! pixels, no target with a lazy layer), and the first effect after idle frames creates nothing
//! (`specs/27` § 2.4).
//!
//! The DO27 chase measured the layer with nothing in it at +0.25 to +0.30 ms of GPU post,
//! +181 heap allocations per frame and +128 MiB (`docs/perf-log.md`, `vfx-layer` rows): the
//! frame copied the HDR image for it, ran its decal and HDR passes and simulated its pool every
//! frame. The allocation count is `empty_layer_alloc.rs` (its own binary: a counting allocator).
//! Fixed frame counts, no wall clock.

use std::sync::{Arc, Mutex};

use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FramePathCounters, FrameQuality, FrameRenderer,
    FrameScene, LabMaterial, LabVertex, PostSettings, RenderDevice, frame_path_counters_all_threads,
    take_frame_path_events, trace_frame_path,
};
use regia_vfx::format::{
    BlendMode, ColorKey, EmitterDef, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef, SpawnMode,
    SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VfxSystemDef,
};
use regia_vfx::gpu::{LAYER_WARM_FRAMES, TrailVertex};
use regia_vfx::sim::LodInput;
use regia_vfx::{
    Decal, DecalSurface, VfxCamera, VfxGpu, VfxProfile, VfxScene, align_to_normal, translucent_layer,
};

const W: u32 = 96;
const H: u32 = 64;
const EYE: [f32; 3] = [0.0, 3.0, 8.0];
const LOOK: [f32; 3] = [0.0, 0.6, 0.0];
const FOV: f32 = 0.9;
/// Frames past the layer's warm frames before anything is compared: the first idle frame hands
/// the post stack the image the layer no longer draws into, which it may not have bound yet.
const SETTLE_FRAMES: u32 = LAYER_WARM_FRAMES + 8;

/// One GPU test at a time: the create counts are process-wide (a frame's encode workers create
/// on their own threads), so a test beside them would add its own.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

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

/// One orange puff at `(0, 1.6, 0)` living `lifetime` seconds.
fn puff(lifetime: f32) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: "puff".into(),
        system: SystemModule {
            bounds: None,
            budget: 8,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: "puff".into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Burst(1),
                lifetime: RangeDef::from(lifetime),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Point,
                velocity: VelocityDef::default(),
                size: RangeDef::from(1.1),
                rotation: RangeDef::from(0.0),
                spin: RangeDef::from(0.0),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#ff9a3c".into(),
                }],
            },
            update: UpdateModule::default(),
            render: RenderModule {
                mode: RenderMode::Billboard,
                sprite: SpriteId::SoftDisc,
                blend: BlendMode::Alpha,
                emissive: 1.5,
                ..RenderModule::default()
            },
        }],
    }
}

fn ground() -> Vec<LabVertex> {
    let half = 30.0;
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

/// A single-sample HDR frame with the post stack on: the layer's HDR path (ADR-049).
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

/// Renders frames of the ground; `scene` (with its context) is stepped and uploaded first.
struct Drive {
    device: RenderDevice,
    renderer: FrameRenderer,
    vertices: Vec<LabVertex>,
    material: LabMaterial,
    post: PostSettings,
}

impl Drive {
    fn new(device: RenderDevice) -> Self {
        Self {
            device,
            renderer: FrameRenderer::new(),
            vertices: ground(),
            material: LabMaterial {
                base_color: [0.5, 0.5, 0.5, 1.0],
                roughness: 0.9,
                ..LabMaterial::EMPTY
            },
            post: post(),
        }
    }

    fn frame(&mut self, effects: Option<(&mut VfxScene, &Arc<Mutex<VfxGpu>>)>) -> Vec<u8> {
        if let Some((scene, gpu)) = effects {
            let mut locked = gpu.lock().expect("vfx gpu");
            scene.update(
                1.0 / 60.0,
                &LodInput {
                    camera_pos: Vec3::from_array(EYE),
                },
                256,
            );
            scene.upload(&mut locked);
        }
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
            exposure_ev: 15.0,
            ambient_lux: 20_000.0,
            double_sided: false,
            post: Some(&self.post),
            fog: FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 100_000.0),
            local_lights: &[],
            quality: FrameQuality::LAB,
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &self.vertices,
                indices: &GROUND_INDICES,
                material: &self.material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: regia_render::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        self.renderer
            .render_offscreen(&self.device, &frame)
            .expect("frame")
            .pixels
    }
}

/// `frames` frames of a fresh renderer, with `layer` (an empty context) or no layer: the last
/// frame's pixels, every frame-path create of the run, and the labels of the creates made on
/// this thread (the encode workers' own creates are counted but not named).
fn run(
    device: &RenderDevice,
    layer: Option<Arc<Mutex<VfxGpu>>>,
    frames: u32,
) -> (Vec<u8>, FramePathCounters, Vec<String>) {
    let mut drive = Drive::new(device.clone());
    trace_frame_path(true);
    let before = frame_path_counters_all_threads();
    if let Some(gpu) = layer {
        drive
            .renderer
            .set_translucent_layers(vec![translucent_layer(gpu)]);
    }
    let mut pixels = Vec::new();
    for _ in 0..frames {
        pixels = drive.frame(None);
    }
    let creates = frame_path_counters_all_threads().since(before);
    let mut named: Vec<String> = take_frame_path_events()
        .into_iter()
        .map(|event| format!("{:?} {}", event.kind, event.label))
        .collect();
    named.sort();
    trace_frame_path(false);
    (pixels, creates, named)
}

fn empty_context(device: &RenderDevice, warm_frames: u32) -> Arc<Mutex<VfxGpu>> {
    let mut gpu = VfxGpu::new(device, profile()).expect("vfx gpu");
    gpu.set_layer_warm_frames(warm_frames);
    Arc::new(Mutex::new(gpu))
}

#[test]
fn an_empty_layer_leaves_the_frame_as_it_was() {
    let _serial = serial();
    let device = device();
    let (bare, _, _) = run(&device, None, SETTLE_FRAMES);
    let (empty, _, _) = run(
        &device,
        Some(empty_context(&device, LAYER_WARM_FRAMES)),
        SETTLE_FRAMES,
    );
    assert_eq!(bare.len(), empty.len());
    let differing = bare.iter().zip(&empty).filter(|(a, b)| a != b).count();
    assert_eq!(
        differing, 0,
        "an empty layer changed {differing} bytes of the frame"
    );
}

/// Built lazily, an empty layer adds no texture, bind group or pipeline to the frame: the base
/// copied the HDR image for it (`translucent hdr scratch`), sized a decal receiver and its scene
/// copy, and simulated its pool every frame.
#[test]
fn a_lazy_empty_layer_creates_no_target() {
    let _serial = serial();
    let device = device();
    // Once without a layer first: pipelines cached for the process (shared by every renderer on
    // the device) are compiled by whichever run comes first, so the two compared runs both start
    // after them. Which test of this binary ran before must not decide the comparison.
    let _ = run(&device, None, SETTLE_FRAMES);
    let (bare, bare_creates, bare_named) = run(&device, None, SETTLE_FRAMES);
    let gpu = empty_context(&device, 0);
    let (empty, empty_creates, empty_named) = run(&device, Some(gpu), SETTLE_FRAMES);
    let differing = bare.iter().zip(&empty).filter(|(a, b)| a != b).count();
    assert_eq!(
        (bare.len(), differing),
        (empty.len(), 0),
        "a lazy empty layer changed the frame"
    );
    assert_eq!(
        (
            empty_creates.textures,
            empty_creates.texture_bytes,
            empty_creates.bind_groups,
            empty_creates.pipelines,
            empty_creates.buffers,
        ),
        (
            bare_creates.textures,
            bare_creates.texture_bytes,
            bare_creates.bind_groups,
            bare_creates.pipelines,
            bare_creates.buffers,
        ),
        "an empty lazy layer created GPU objects the frame without it does not: \
         {empty_creates:?} vs {bare_creates:?}; named with the layer {empty_named:?}, \
         without {bare_named:?}"
    );
}

/// The default layer builds its targets on its warm frames, goes idle, and its first effect
/// draws with what it built: no bind group, pipeline, texture or buffer after the warm-up, the
/// idle frames and the spawn included.
#[test]
fn the_first_effect_after_idle_frames_creates_nothing() {
    let _serial = serial();
    let device = device();
    let gpu = empty_context(&device, LAYER_WARM_FRAMES);
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(puff(30.0)).expect("puff def");
    let mut scene = VfxScene::new(lib);
    let mut drive = Drive::new(device.clone());
    drive
        .renderer
        .set_translucent_layers(vec![translucent_layer(Arc::clone(&gpu))]);
    let mut idle = Vec::new();
    for _ in 0..SETTLE_FRAMES {
        idle = drive.frame(Some((&mut scene, &gpu)));
    }
    assert!(
        gpu.lock().expect("vfx gpu").is_idle(),
        "nothing spawned: the layer is idle"
    );
    trace_frame_path(true);
    let before = frame_path_counters_all_threads();
    for _ in 0..4 {
        drive.frame(Some((&mut scene, &gpu)));
    }
    scene
        .spawn("puff", Vec3::new(0.0, 1.6, 0.0), Vec3::Z, 3)
        .expect("spawn puff");
    let mut lit = Vec::new();
    for _ in 0..4 {
        lit = drive.frame(Some((&mut scene, &gpu)));
    }
    let creates = frame_path_counters_all_threads().since(before);
    let named: Vec<String> = take_frame_path_events()
        .into_iter()
        .map(|event| format!("{:?} {}", event.kind, event.label))
        .collect();
    trace_frame_path(false);
    assert!(
        !gpu.lock().expect("vfx gpu").is_idle(),
        "the puff keeps the layer live"
    );
    let drawn = idle.iter().zip(&lit).filter(|(a, b)| a != b).count();
    assert!(drawn > 0, "the puff must draw: the layer went live again");
    assert_eq!(
        (
            creates.bind_groups,
            creates.pipelines,
            creates.textures,
            creates.buffers
        ),
        (0, 0, 0, 0),
        "the idle frames and the first effect created: {named:?}"
    );
}

fn camera() -> VfxCamera {
    VfxCamera {
        eye: Vec3::from_array(EYE),
        target: Vec3::from_array(LOOK),
        fov_y: FOV,
        near: 0.1,
        far: 60.0,
    }
}

/// What keeps a context live, step by step on its own clock (no host frame).
#[test]
fn a_context_is_idle_only_with_nothing_that_can_draw() {
    let _serial = serial();
    let device = device();
    let mut gpu = VfxGpu::new(&device, profile()).expect("vfx gpu");
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(puff(0.5)).expect("puff def");
    let mut scene = VfxScene::new(lib);
    // One host frame on the context's own clock: the scene stepped and uploaded, then the frame.
    let step = |gpu: &mut VfxGpu, scene: &mut VfxScene, frames: u32| {
        for _ in 0..frames {
            scene.update(
                1.0 / 60.0,
                &LodInput {
                    camera_pos: Vec3::from_array(EYE),
                },
                256,
            );
            scene.upload(gpu);
            gpu.set_frame(&camera(), 1.0 / 60.0, W, H);
        }
    };
    // A new context counts its warm frames as live.
    for _ in 0..LAYER_WARM_FRAMES {
        assert!(!gpu.is_idle(), "a warm frame of a new context is live");
        step(&mut gpu, &mut scene, 1);
    }
    assert!(gpu.is_idle(), "the warm frames are done and nothing was spawned");

    // A live instance keeps the context live even before it spawns (its LOD may hold it back:
    // the layer must keep following the camera).
    gpu.set_scene_live(true);
    assert!(!gpu.is_idle(), "a live scene instance is live");
    gpu.set_scene_live(false);
    assert!(gpu.is_idle());

    // A spawn: live while the instance lives and a particle can still be alive (0.5 s + the
    // 0.25 s margin).
    scene
        .spawn("puff", Vec3::new(0.0, 1.6, 0.0), Vec3::Z, 3)
        .expect("spawn puff");
    scene.update(
        1.0 / 60.0,
        &LodInput {
            camera_pos: Vec3::from_array(EYE),
        },
        256,
    );
    scene.upload(&mut gpu);
    assert!(gpu.queued_spawn_jobs() > 0, "the burst staged its spawn jobs");
    assert!(!gpu.is_idle(), "a staged spawn is live");
    gpu.set_frame(&camera(), 1.0 / 60.0, W, H);
    step(&mut gpu, &mut scene, 29);
    assert!(
        !gpu.is_idle(),
        "0.5 s after the spawn the particle may still live"
    );
    step(&mut gpu, &mut scene, 60);
    assert_eq!(scene.live_instances(), 0, "the 0.5 s puff retired");
    assert!(gpu.is_idle(), "1.5 s after a 0.5 s spawn every particle has died");

    // A decal is live until the list is emptied; so is a trail.
    let decal = Decal {
        position: Vec3::ZERO,
        rotation: align_to_normal(Vec3::Y, 0.0),
        half_size: Vec3::splat(0.5),
        color: [1.0; 4],
        age: 0.0,
        lifetime: None,
    };
    gpu.write_decals_with(&[decal], &[DecalSurface::from_decal(&decal)]);
    assert!(!gpu.is_idle(), "a staged decal is live");
    gpu.write_decals_with(&[], &[]);
    assert!(gpu.is_idle(), "an emptied decal list is idle");
    let vertex = TrailVertex {
        position: [0.0, 1.0, 0.0],
        _pad: 0.0,
        uv: [0.0, 0.0],
        _pad2: [0.0; 2],
        color: [1.0; 4],
    };
    gpu.write_trails(&[vertex; 3]);
    assert!(!gpu.is_idle(), "a trail is live");
    gpu.write_trails(&[]);
    assert!(gpu.is_idle(), "no trail, no decal, nothing alive: idle");
}
