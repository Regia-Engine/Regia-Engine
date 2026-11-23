// SPDX-License-Identifier: MPL-2.0
//! The ECS effects context is built on a warm worker (ADR-195 "Warm coverage"; DO27
//! INTEGRATE_005 § 3 logged `regia_vfx/src/gpu/{mod.rs, radix_sort.rs}` pipelines "created
//! outside the warm queue"). With a started `PipelineWarmQueue`, `VfxRuntimePlugin` builds the
//! `VfxGpu` (simulation, sort and radix-sort compute pipelines) and its render sets for the HDR
//! formats on a worker; the main thread, which draws smoke from the first frame the layer is
//! installed, creates none of them. Its own test binary: the counters are per process.

use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{
    DirectionalLightUniform, FogSettings, FrameQuality, FrameScene, PipelineWarmQueue, PostSettings,
    RenderDevice, RenderProfile, RenderProfilePreset, RuntimeRenderer,
};
use regia_vfx::runtime::VfxGpuHandle;
use regia_vfx::{VfxPlugin, VfxRuntimePlugin, VfxScene};

/// Serialises device creation and teardown across this binary's test threads: concurrent
/// Vulkan device churn can live-lock the NVIDIA driver (615.71; ADR-1438). Each test holds it
/// for its whole body (declared first, so the device drops while it is held).
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_runtime_effects_context_compiles_on_a_warm_worker() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let queue = PipelineWarmQueue::new(2);
    queue.start(&device);
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.insert_resource(device.clone());
    app.insert_resource(queue.clone());
    app.init_resource::<RuntimeRenderer>();
    let mut lib = regia_vfx::scene::VfxLibrary::new();
    lib.add(regia_vfx::showcase::smoke()).expect("smoke");
    app.insert_resource(VfxScene::new(lib));
    app.add_plugins((VfxPlugin, VfxRuntimePlugin));
    let smoke_at = Vec3::new(0.0, 0.0, 0.0);
    app.world_mut()
        .resource_mut::<VfxScene>()
        .spawn("smoke", smoke_at, Vec3::Z, 7)
        .expect("spawn smoke");
    let post = PostSettings::default();
    let quality = FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut installed_at = None;
    for update in 0.. {
        app.update();
        if installed_at.is_none() && app.world().contains_resource::<VfxGpuHandle>() {
            installed_at = Some(update);
        }
        let frame = FrameScene {
            width: 96,
            height: 54,
            eye: [0.0, 3.0, 20.0],
            target: [0.0, 3.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 2000.0,
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
            quality,
            meshes: Vec::new(),
        };
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .render_offscreen(&device, &frame)
            .expect("frame");
        if installed_at.is_some_and(|at| update >= at + 20) {
            break;
        }
        assert!(Instant::now() < deadline, "the effects layer was installed");
        std::thread::sleep(Duration::from_millis(2));
    }
    let progress = queue.progress();
    let vfx_outside: Vec<_> = progress
        .outside_sites
        .iter()
        .filter(|(site, _)| site.contains("regia_vfx"))
        .collect();
    let map = progress.tiers[regia_render::WarmTier::Map as usize].pipelines;
    println!(
        "effects context: installed after {installed_at:?} updates; map-tier pipelines {map}; \
         regia_vfx sites outside the queue: {vfx_outside:?}"
    );
    assert!(map > 0, "the context compiled on the warm worker");
    // On the map tier: it never holds back the first present or the menu's jobs.
    assert_eq!(
        progress.tiers[regia_render::WarmTier::Boot as usize].jobs_total,
        0
    );
    assert!(
        vfx_outside.is_empty(),
        "effect pipelines compiled outside the warm queue: {vfx_outside:?}"
    );
    queue.shutdown();
}

/// A context build that fails (here: an out-of-range pool) is queued once, not on every
/// `Update` (it kept a worker compiling and the tier never ready).
#[test]
fn a_failed_effects_context_is_not_requeued_every_frame() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    };
    let queue = PipelineWarmQueue::new(1);
    queue.start(&device);
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.insert_resource(device.clone());
    app.insert_resource(queue.clone());
    app.init_resource::<RuntimeRenderer>();
    app.add_plugins((VfxPlugin, VfxRuntimePlugin));
    app.world_mut()
        .resource_mut::<regia_vfx::VfxSettings>()
        .profile
        .particle_capacity = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    let map = || queue.progress().tiers[regia_render::WarmTier::Map as usize].clone();
    while map().jobs_failed == 0 {
        app.update();
        assert!(Instant::now() < deadline, "the build failed on the worker");
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..30 {
        app.update();
    }
    let tier = map();
    println!("failed effects context: {tier:?}");
    assert!(!app.world().contains_resource::<VfxGpuHandle>());
    assert_eq!(tier.jobs_total, 1, "the failed build was queued again");
    assert!(queue.progress().is_tier_ready(regia_render::WarmTier::Map));
    queue.shutdown();
}
