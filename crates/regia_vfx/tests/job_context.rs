// SPDX-License-Identifier: MPL-2.0
//! ADR-2814: a render-out job that starts before the effects context exists installs it first. Left
//! to its worker the context arrived during the job: the job's earlier effects were lost, a held job
//! had no pool to put back, and the new context's warm frames moved the particle clock, so the first
//! job in a host differed from every later one. Its own test binary: the build on the main thread
//! would count against `warm_runtime`'s per-process "outside the warm queue" counters.

use std::sync::mpsc;
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_render::{PipelineWarmQueue, RenderDevice, RuntimeRenderer};
use regia_vfx::gpu::{LAYER_WARM_FRAMES, VfxCamera};
use regia_vfx::live_state::{EffectsJobStart, LiveEffectsCapture};
use regia_vfx::runtime::VfxGpuHandle;
use regia_vfx::{VfxPlugin, VfxRuntimePlugin, VfxScene};

/// Serializes device creation and teardown across this binary's test threads (ADR-1438). Each test
/// holds it for its whole body.
static SERIAL_GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn headless() -> RenderDevice {
    match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

fn effects_app(device: &RenderDevice) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.insert_resource(device.clone());
    app.init_resource::<RuntimeRenderer>();
    app.add_plugins((VfxPlugin, VfxRuntimePlugin));
    app
}

fn warm_frames(app: &App) -> u32 {
    let handle = app
        .world()
        .get_resource::<VfxGpuHandle>()
        .expect("the context is installed");
    handle.lock().expect("context").layer_warm_frames()
}

/// The only worker is busy, so the context's build waits in the queue however fast the machine is.
#[test]
fn a_job_start_installs_a_context_still_waiting_for_its_worker() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = headless();
    let queue = PipelineWarmQueue::new(1);
    queue.start(&device);
    let (release, wait) = mpsc::channel::<()>();
    queue.push(
        regia_render::WarmTier::Boot,
        "hold the worker",
        move |_: &RenderDevice| {
            let _ = wait.recv();
            Ok(())
        },
    );
    let mut app = effects_app(&device);
    app.insert_resource(queue.clone());
    app.update();
    assert!(
        !app.world().contains_resource::<VfxGpuHandle>(),
        "the build waits for the worker"
    );

    let start = EffectsJobStart::restart(app.world_mut());
    assert!(
        app.world().contains_resource::<VfxGpuHandle>(),
        "a job does not start without the context"
    );
    assert!(start.pool_restarted(), "the first job restarts like any other");
    assert_eq!(
        warm_frames(&app),
        0,
        "the job's clock moves only for what it draws"
    );

    // The job's frames, with the worker's own build coming back meanwhile: nothing changes.
    drop(release);
    for _ in 0..8 {
        app.update();
    }
    assert_eq!(warm_frames(&app), 0);
    start.finish(app.world_mut());
    assert_eq!(
        warm_frames(&app),
        LAYER_WARM_FRAMES,
        "the app's warm frames came back"
    );
    queue.shutdown();
}

/// A held job captures first: the pool it puts back exists by then, also when the job comes before
/// the app's first `Update`. The job's frames move the clock; the restore takes them back.
#[test]
fn a_held_job_has_a_pool_to_put_back_before_the_first_update() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = headless();
    let mut app = effects_app(&device);
    // `Startup` has not run: a host's own scene is what stands here, as `restore` needs one.
    app.insert_resource(VfxScene::new(regia_vfx::scene::VfxLibrary::new()));
    assert!(!app.world().contains_resource::<VfxGpuHandle>());
    let capture = LiveEffectsCapture::capture(app.world_mut());
    let handle = app
        .world()
        .get_resource::<VfxGpuHandle>()
        .expect("the capture has the context's pool")
        .clone();
    {
        let mut gpu = handle.lock().expect("context");
        let camera = VfxCamera::looking_at(Vec3::new(0.0, 2.0, -10.0), Vec3::ZERO);
        gpu.set_frame(&camera, 0.5, 64, 64);
        assert_eq!(gpu.sim_clock(), 0.5);
    }
    capture.restore(app.world_mut());
    assert_eq!(
        handle.lock().expect("context").sim_clock(),
        0.0,
        "the pool went back to where the job found it"
    );
}

/// A host without the runtime plugin stays without a context: a job does not add effects to it.
#[test]
fn a_job_start_installs_nothing_without_the_runtime_plugin() {
    let _gpu = SERIAL_GPU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let device = headless();
    let mut app = App::new();
    app.insert_resource(device.clone());
    app.init_resource::<RuntimeRenderer>();
    app.add_plugins(VfxPlugin);
    let start = EffectsJobStart::restart(app.world_mut());
    assert!(!app.world().contains_resource::<VfxGpuHandle>());
    assert!(!start.pool_restarted());
    start.finish(app.world_mut());
}
