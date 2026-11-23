// SPDX-License-Identifier: MPL-2.0
//! Effect pipeline warm-up (DO27 PERF_JANK 5): after `vfx_warm_job` ran on a
//! `PipelineWarmQueue` (and the renderer warmed its decal receiver), the first emitter of every
//! kind (haze, additive, sorted alpha, soft smoke, mesh, stretched), the first projected decal
//! and the first trail compile nothing on the frame path, on HDR frames and on display-pass
//! frames alike, and so does the first mesh-asset particle (`.vfx.ron` v8 `render.mesh`, ADR-7261).
//! The warm stops at once when its queue shuts down. Its own test binary: the
//! pipeline counter is per process, so the tests take turns.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regia_core::math::Vec3;
use regia_render::pipeline_cache::pipelines_created;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameMesh, FrameQuality, FrameRenderer, FrameScene, LabMaterial,
    LabVertex, PipelineWarmQueue, PostSettings, RenderDevice, WarmTier,
};
use regia_vfx::format::{
    BlendMode, ColorKey, EmitterDef, InitModule, RangeDef, RenderMode, RenderModule, ShapeDef, SpawnMode,
    SpawnModule, SpriteId, SystemModule, UpdateModule, VFX_FORMAT_VERSION, VelocityDef, VfxSystemDef,
};
use regia_vfx::gpu::{SoftParticleRes, TrailVertex};
use regia_vfx::sim::LodInput;
use regia_vfx::{
    CHANNEL_ALBEDO, Decal, DecalSurface, VfxGpu, VfxProfile, VfxScene, align_to_normal, translucent_layer,
    vfx_warm_job,
};

/// One test at a time (the pipeline counter is process-wide).
static SERIAL: Mutex<()> = Mutex::new(());

/// What `render_offscreen` draws into.
const TARGET: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const EYE: [f32; 3] = [0.0, 3.0, 8.0];
const LOOK: [f32; 3] = [0.0, 0.6, 0.0];

fn device() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Half-resolution soft layer, so its pipelines are covered too.
fn profile() -> VfxProfile {
    VfxProfile {
        particle_capacity: 512,
        decal_capacity: 16,
        sort: true,
        light_budget: 0,
        screen_floor_px: 0.0,
        smoke_budget: 256,
        soft_particle_res: SoftParticleRes::Half,
        event_capacity: 16,
        child_capacity: 16,
    }
}

fn emitter(name: &str, render: RenderModule) -> VfxSystemDef {
    VfxSystemDef {
        version: VFX_FORMAT_VERSION,
        name: name.into(),
        system: SystemModule {
            bounds: None,
            budget: 16,
            lod: None,
        },
        emitters: vec![EmitterDef {
            name: name.into(),
            delay: 0.0,
            duration: None,
            spawn: SpawnModule {
                mode: SpawnMode::Burst(4),
                lifetime: RangeDef::from(30.0),
                count_scale: 1.0,
            },
            init: InitModule {
                shape: ShapeDef::Point,
                velocity: VelocityDef::default(),
                size: RangeDef::from(0.8),
                rotation: RangeDef::from(0.0),
                spin: RangeDef::from(0.0),
                color: vec![ColorKey {
                    t: 0.0,
                    color: "#ff9a3c".into(),
                }],
            },
            update: UpdateModule::default(),
            render,
        }],
    }
}

/// One effect per emitter kind the renderer draws differently.
fn library() -> regia_vfx::scene::VfxLibrary {
    let kinds = [
        (
            "additive",
            RenderModule {
                blend: BlendMode::Additive,
                sprite: SpriteId::Flare,
                emissive: 4.0,
                ..RenderModule::default()
            },
        ),
        (
            "alpha",
            RenderModule {
                blend: BlendMode::Alpha,
                sprite: SpriteId::HardDisc,
                emissive: 2.0,
                softness: 0.0,
                ..RenderModule::default()
            },
        ),
        (
            "smoke",
            RenderModule {
                blend: BlendMode::Alpha,
                sprite: SpriteId::Puff,
                softness: 1.0,
                emissive: 0.5,
                ..RenderModule::default()
            },
        ),
        (
            "haze",
            RenderModule {
                blend: BlendMode::Additive,
                heat_haze: 10.0,
                ..RenderModule::default()
            },
        ),
        (
            "mesh",
            RenderModule {
                mode: RenderMode::Mesh,
                sprite: SpriteId::Shard,
                blend: BlendMode::Alpha,
                ..RenderModule::default()
            },
        ),
        (
            "stretched",
            RenderModule {
                mode: RenderMode::Stretched,
                sprite: SpriteId::Spark,
                blend: BlendMode::Additive,
                emissive: 6.0,
                ..RenderModule::default()
            },
        ),
    ];
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    for (name, render) in kinds {
        lib.add(emitter(name, render)).expect("effect def");
    }
    lib
}

const KINDS: [&str; 6] = ["additive", "alpha", "smoke", "haze", "mesh", "stretched"];

fn paint() -> (Decal, DecalSurface) {
    let decal = Decal {
        position: Vec3::new(-1.6, 0.0, 0.6),
        rotation: align_to_normal(Vec3::Y, 0.0),
        half_size: Vec3::new(0.9, 0.5, 0.9),
        color: [0.95, 0.78, 0.08, 1.0],
        age: 0.0,
        lifetime: None,
    };
    let mut surface = DecalSurface::from_decal(&decal);
    surface.atlas_layer = -1;
    surface.channels = CHANNEL_ALBEDO;
    (decal, surface)
}

fn trail() -> Vec<TrailVertex> {
    let v = |x: f32, y: f32, side: f32| TrailVertex {
        position: [x, y, 0.0],
        _pad: 0.0,
        uv: [0.0, side],
        _pad2: [0.0; 2],
        color: [1.0, 0.55, 0.15, 1.0],
    };
    let (a_l, a_r) = (v(0.6, 0.85, -1.0), v(0.6, 1.15, 1.0));
    let (b_l, b_r) = (v(3.0, 0.85, -1.0), v(3.0, 1.15, 1.0));
    vec![a_l, b_l, a_r, a_r, b_l, b_r]
}

fn ground() -> Vec<LabVertex> {
    let half = 30.0;
    [[-half, -half], [half, -half], [half, half], [-half, half]]
        .into_iter()
        .map(|[x, z]| LabVertex::from_pos_normal_uv([x, 0.0, z], [0.0, 1.0, 0.0], [0.0, 0.0]))
        .collect()
}

const GROUND_INDICES: [u32; 6] = [0, 2, 1, 0, 3, 2];

/// The two frame shapes the effects draw in: the HDR pass (post on: `Rgba16Float`, soft layer)
/// and the display pass (no post: the target's format).
#[derive(Clone, Copy, Debug)]
enum Shape {
    Hdr,
    Display,
}

fn render(device: &RenderDevice, renderer: &mut FrameRenderer, shape: Shape) {
    let post = PostSettings {
        enabled: true,
        bloom: false,
        auto_exposure: false,
        exposure_ev: 15.0,
        fxaa: false,
        taa: false,
        ..PostSettings::default()
    };
    let vertices = ground();
    let material = LabMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 0.9,
        ..LabMaterial::EMPTY
    };
    let frame = FrameScene {
        width: 160,
        height: 120,
        eye: EYE,
        target: LOOK,
        fov_y: 0.9,
        near: 0.1,
        far: 60.0,
        depth_prepass: true,
        directional_shadows: false,
        ibl: false,
        exposure_ev: 15.0,
        ambient_lux: 20_000.0,
        double_sided: false,
        post: match shape {
            Shape::Hdr => Some(&post),
            Shape::Display => None,
        },
        fog: FogSettings::default(),
        directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0; 3], 100_000.0),
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
    renderer.render_offscreen(device, &frame).expect("frame");
}

/// A renderer warmed on both frame shapes before any effect layer exists (a host's map warm).
fn warm_renderer(device: &RenderDevice) -> FrameRenderer {
    let mut renderer = FrameRenderer::new();
    for _ in 0..2 {
        render(device, &mut renderer, Shape::Hdr);
        render(device, &mut renderer, Shape::Display);
    }
    renderer
}

/// Installs `gpu` as the renderer's layer, draws a frame of each shape with nothing alive,
/// then spawns every emitter kind, a decal and a trail and draws a few frames of each shape.
/// Returns the pipelines `(installing, effects)` created.
fn effect_frames(
    device: &RenderDevice,
    renderer: &mut FrameRenderer,
    gpu: &Arc<Mutex<VfxGpu>>,
) -> (u64, u64) {
    let before = pipelines_created();
    renderer.set_translucent_layers(vec![translucent_layer(Arc::clone(gpu))]);
    render(device, renderer, Shape::Hdr);
    render(device, renderer, Shape::Display);
    let installing = pipelines_created() - before;

    let before = pipelines_created();
    let mut scene = VfxScene::new(library());
    for (i, kind) in KINDS.iter().enumerate() {
        scene
            .spawn(
                kind,
                Vec3::new(i as f32 * 0.6 - 1.5, 1.4, 0.0),
                Vec3::Z,
                i as u64 + 1,
            )
            .expect("spawn");
    }
    for _ in 0..3 {
        for shape in [Shape::Hdr, Shape::Display] {
            {
                let mut locked = gpu.lock().expect("vfx gpu");
                scene.update(
                    1.0 / 60.0,
                    &LodInput {
                        camera_pos: Vec3::from_array(EYE),
                    },
                    512,
                );
                scene.upload(&mut locked);
                let (decal, surface) = paint();
                locked.write_decals_with(&[decal], &[surface]);
                locked.write_trails(&trail());
            }
            render(device, renderer, shape);
        }
    }
    (installing, pipelines_created() - before)
}

#[test]
fn every_effect_kind_draws_without_compiling_after_the_warm() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();

    // Control: a context nobody warmed compiles its pipelines on the frame path.
    let mut cold_renderer = warm_renderer(&device);
    cold_renderer.prewarm_decals(&device).expect("decal receiver");
    let cold = Arc::new(Mutex::new(VfxGpu::new(&device, profile()).expect("vfx gpu")));
    let (cold_install, cold_effects) = effect_frames(&device, &mut cold_renderer, &cold);
    assert!(
        cold_install + cold_effects > 0,
        "an unwarmed effect context compiles on the frame path"
    );

    // The warm: the effect context on the queue, the renderer's decal receiver beside it.
    let mut renderer = warm_renderer(&device);
    renderer.prewarm_decals(&device).expect("decal receiver");
    let gpu = Arc::new(Mutex::new(VfxGpu::new(&device, profile()).expect("vfx gpu")));
    let queue = PipelineWarmQueue::new(1);
    queue.push(
        WarmTier::Map,
        "vfx",
        vfx_warm_job(Arc::clone(&gpu), VfxGpu::warm_formats(TARGET)),
    );
    queue.start(&device);
    assert!(
        queue.wait_tier(WarmTier::Map, Duration::from_secs(300)),
        "the warm did not finish"
    );
    let progress = queue.progress();
    queue.shutdown();
    assert_eq!(progress.last_error, None);
    let map = &progress.tiers[WarmTier::Map as usize];
    // Seven per format (window + HDR; the mesh-asset pipeline since ADR-7261) and the soft composite.
    assert_eq!(map.pipelines, 15, "{progress:?}");
    {
        let locked = gpu.lock().expect("vfx gpu");
        let mut formats: Vec<_> = locked.formats().collect();
        formats.sort_by_key(|f| format!("{f:?}"));
        assert_eq!(formats, vec![wgpu::TextureFormat::Rgba16Float, TARGET]);
    }

    let (install, effects) = effect_frames(&device, &mut renderer, &gpu);
    println!(
        "frame-path pipeline creates: unwarmed {cold_install} installing + {cold_effects} effects; \
         warmed {install} + {effects} (warm job built {})",
        map.pipelines
    );
    assert_eq!(
        install + effects,
        0,
        "an effect compiled on the frame path after the warm; outside sites: {:?}",
        PipelineWarmQueue::default().progress().outside_sites
    );
}

#[test]
fn warm_in_place_builds_once() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut gpu = VfxGpu::new(&device, profile()).expect("vfx gpu");
    let formats = VfxGpu::warm_formats(wgpu::TextureFormat::Bgra8UnormSrgb);
    let before = pipelines_created();
    let first = gpu.warm(&formats).expect("warm");
    assert_eq!(first.pipelines, 15);
    assert!(!first.canceled);
    assert_eq!(pipelines_created() - before, 15);
    let again = gpu.warm(&formats).expect("warm again");
    assert_eq!(again.pipelines, 0, "a warmed format costs nothing");
    assert_eq!(pipelines_created() - before, 15);
}

#[test]
fn the_warm_stops_when_its_queue_shuts_down() {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let gpu = Arc::new(Mutex::new(VfxGpu::new(&device, profile()).expect("vfx gpu")));
    let queue = PipelineWarmQueue::new(1);
    let (started_tx, started_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (result_tx, result_rx) = mpsc::channel();
    let job = vfx_warm_job(Arc::clone(&gpu), VfxGpu::warm_formats(TARGET));
    // The warm runs inside a job that first waits until the queue is shutting down: the
    // shutdown lands while it runs, exactly as when the player quits during a map warm.
    queue.push(WarmTier::Map, "vfx (held)", move |device: &RenderDevice| {
        let _ = started_tx.send(());
        let _ = go_rx.recv_timeout(Duration::from_secs(60));
        let result = job(device);
        let _ = result_tx.send(result.clone());
        result
    });
    queue.start(&device);
    started_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the job started");
    let before = pipelines_created();
    let stopping = queue.clone();
    let shutdown = std::thread::spawn(move || stopping.shutdown());
    while !queue.is_shutting_down() {
        std::thread::sleep(Duration::from_millis(1));
    }
    go_tx.send(()).expect("job waiting");
    let result = result_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the job finished");
    shutdown.join().expect("shutdown");
    assert_eq!(result, Err("canceled at shutdown".to_string()));
    assert_eq!(
        pipelines_created() - before,
        0,
        "nothing compiled after the shutdown"
    );
    assert_eq!(gpu.lock().expect("vfx gpu").formats().count(), 0);
}

/// One still mesh particle, 1 m across: the built-in cube (`mesh: None`) or a registered mesh.
fn mesh_effect(name: &str, mesh: Option<&str>) -> VfxSystemDef {
    emitter(
        name,
        RenderModule {
            mode: RenderMode::Mesh,
            blend: BlendMode::Alpha,
            mesh: mesh.map(str::to_string),
            ..RenderModule::default()
        },
    )
}

#[test]
fn a_mesh_asset_particle_draws_without_compiling_after_the_warm() {
    use regia_vfx::offscreen::{HARNESS_FORMAT, VfxHarness};
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = device();
    let mut library = regia_vfx::scene::VfxLibrary::new();
    library.add_mesh(
        "shard",
        regia_vfx::mesh_shape::VfxMesh::from_triangles(
            &[
                [0.5, 0.0, 0.0],
                [-0.5, 0.0, 0.0],
                [0.0, 0.5, 0.0],
                [0.0, 0.0, 0.5],
            ],
            &[0, 2, 1, 0, 1, 3, 1, 2, 3, 0, 3, 2],
        )
        .expect("shard mesh"),
    );
    library.add(mesh_effect("cube", None)).expect("cube effect");
    library
        .add(mesh_effect("asset", Some("shard")))
        .expect("asset effect");
    let mut scene = VfxScene::new(library);
    let mut harness = VfxHarness::new(&device, profile()).expect("harness");
    let report = harness
        .gpu()
        .warm(&VfxGpu::warm_formats(HARNESS_FORMAT))
        .expect("warm");
    let camera = regia_vfx::VfxCamera::looking_at(Vec3::new(0.0, 2.0, -6.0), Vec3::new(0.0, 2.0, 0.0));
    let frame = |scene: &mut VfxScene, harness: &mut VfxHarness| {
        scene.update(
            1.0 / 60.0,
            &LodInput {
                camera_pos: camera.eye,
            },
            512,
        );
        scene.upload(harness.gpu());
        harness
            .render(&device, &camera, 160, 120, 1.0 / 60.0)
            .expect("frame")
    };
    // Control: a cube mesh particle, so everything but the mesh-asset path is in use.
    scene
        .spawn("cube", Vec3::new(-1.0, 2.0, 0.0), Vec3::Z, 1)
        .expect("spawn cube");
    for _ in 0..2 {
        frame(&mut scene, &mut harness);
    }
    let before = pipelines_created();
    scene
        .spawn("asset", Vec3::new(1.0, 2.0, 0.0), Vec3::Z, 2)
        .expect("spawn asset");
    let mut last = None;
    for _ in 0..3 {
        last = Some(frame(&mut scene, &mut harness));
    }
    let compiled = pipelines_created() - before;
    let counters = last.expect("frames ran").counters;
    assert_eq!(
        counters[2], 8,
        "4 cube and 4 mesh-asset particles in the mesh list"
    );
    assert_eq!(harness.gpu().render_mesh_count(), 1, "the shard was uploaded");
    println!("warm report {report:?}; pipelines compiled by the first mesh-asset frames: {compiled}");
    assert_eq!(
        compiled, 0,
        "the first mesh-asset particle compiled {compiled} pipeline(s) on the draw path after the warm"
    );
}
