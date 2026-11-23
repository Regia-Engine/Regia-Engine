// SPDX-License-Identifier: MPL-2.0
//! The clean exit's renderer step (ADR-188 step 5, DO27 v0.1.8: "runtime renderer dropped in"
//! took 0.36–1.73 s on a loaded box and Bore Twelve 4K finished the whole exit in 2.95 s of
//! DO27's 3 s watchdog). The runtime renderer is dropped off the event-loop thread while the
//! world clears, and the sequence waits for it at most until the device step's timeout:
//!
//! 1. a runtime renderer holding a map's worth of meshes and textured materials is dropped on
//!    the `regia-exit-renderer` thread, its frees and the device's destroy finish on the exit's
//!    drop threads (within the device step's 1.25 s on an idle host; that figure is logged, and
//!    the test waits for the threads as `finish_exit` does, ADR-8598), and no wgpu object is
//!    left;
//! 2. a renderer whose drop takes 5 s (a stuck driver free) does not hold the sequence: the
//!    exit returns within its bounds and the device is destroyed once the drop ends.
//!
//! One test in its own binary: `last_gpu_exit_report` is process-wide.
//!
//! `cargo test -p regia_render --test clean_exit_renderer_drop`

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_platform::{CleanExitSettings, ExitStep, LifecycleEvent, run_clean_exit};
use regia_render::{
    CpuMaterial, CpuMesh, LiveGpuObjects, PipelineWarmQueue, RenderDevice, RenderPlugin, RuntimeRenderer,
};

/// A shared pass draw that draws nothing and, when dropped, notes the thread it was dropped on
/// and optionally sleeps (a slow driver free).
struct DropProbe {
    thread: Arc<Mutex<Option<String>>>,
    sleep: Duration,
}

impl regia_render::SharedPassDraw for DropProbe {
    fn prepare(
        &mut self,
        _info: &regia_render::SharedFrameInfo,
        _upload: &mut regia_render::SharedUpload<'_>,
    ) {
    }
    fn draw<'a>(&'a self, _pass: &mut wgpu::RenderPass<'a>, _kind: regia_render::SharedPass) {}
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        std::thread::sleep(self.sleep);
        let name = std::thread::current().name().unwrap_or("<unnamed>").to_owned();
        *self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(name);
    }
}

/// `count` distinct meshes, each with its own textured material (`size`² albedo): what a map
/// leaves in the runtime renderer's caches.
fn map_frame(count: usize, size: u32) -> regia_render::ExtractedFrame {
    let mut frame = regia_render::pipeline_warm::canonical_frame();
    let template = frame.meshes[0].clone();
    frame.meshes.clear();
    for i in 0..count {
        let mut mesh = CpuMesh {
            vertices: template.mesh.vertices.clone(),
            indices: template.mesh.indices.clone(),
        };
        for v in &mut mesh.vertices {
            v.position[0] += (i % 16) as f32 * 0.1;
            v.position[2] += (i / 16) as f32 * 0.1;
        }
        let mut material = CpuMaterial::default();
        let shade = (i % 251) as u8;
        material.albedo = Some(regia_render::LabTexture {
            width: size,
            height: size,
            rgba: [shade, 128, 255 - shade, 255].repeat((size * size) as usize),
        });
        let mut extracted = template.clone();
        extracted.mesh = Arc::new(mesh);
        extracted.material = Arc::new(material);
        frame.meshes.push(extracted);
    }
    frame
}

/// A headless app with the render plugin and `device`; the warm queue runs no engine jobs.
fn app_with(device: &RenderDevice, dir: &std::path::Path, settings: CleanExitSettings) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: dir.to_path_buf(),
                platform: "linux".into(),
                content_dir: None,
            },
        })
        .add_plugins(RenderPlugin { headless: true })
        .init_resource::<regia_platform::AppLifecycle>()
        .add_message::<LifecycleEvent>()
        .insert_resource(settings)
        .insert_resource(device.clone());
    app.finish_plugins();
    app.world().resource::<PipelineWarmQueue>().set_engine_jobs(false);
    app.update();
    app
}

fn step_ms(report: &regia_platform::CleanExitReport, step: ExitStep) -> Duration {
    report
        .steps
        .iter()
        .find(|s| s.step == step)
        .map_or(Duration::ZERO, |s| s.elapsed)
}

/// How long the test waits for the exit's own drop threads (`regia-exit-renderer`,
/// `regia-exit-device`) after the sequence returned. The device step's budget (1 s + 250 ms) is a
/// figure the exit logs, not this test's bar: on a loaded NVIDIA host `vkDestroyDevice` alone
/// took 1.2 s (pregate card 7). The product never returns from `main` while they run either:
/// `regia_platform::finish_exit` waits for every registered worker.
const EXIT_THREAD_WAIT: Duration = Duration::from_secs(10);

/// The exit's drop threads still registered in the process's GPU worker registry.
fn exit_drop_threads() -> Vec<String> {
    regia_platform::gpu_workers()
        .running()
        .into_iter()
        .map(|w| w.name)
        .filter(|name| name.starts_with("regia-exit-"))
        .collect()
}

/// Waits, at most `budget`, until the exit's drop threads have ended (their registration covers
/// the drop of everything they captured, the device's destroy included). Returns how long it
/// waited and the ones still running. The libtest harness returns from `main` right after the
/// test, without the product's `finish_exit` wait: a drop thread still inside the driver then
/// meets the process's exit handlers (SIGSEGV -11 in the NVIDIA teardown after `test result:
/// ok`, pregate 0f89ab970 card 7). Blocks on the registry's condition variable, which every
/// worker's end signals.
fn wait_for_exit_drop_threads(budget: Duration) -> (Duration, Vec<String>) {
    let t = Instant::now();
    loop {
        let left = exit_drop_threads();
        let remaining = budget.saturating_sub(t.elapsed());
        if left.is_empty() || remaining.is_zero() {
            return (t.elapsed(), left);
        }
        // Other registered workers may stay outstanding: wake at least every 50 ms to look again.
        let _ = regia_platform::gpu_workers().wait(remaining.min(Duration::from_millis(50)));
    }
}

#[test]
fn the_runtime_renderer_drops_off_the_event_loop_thread_and_a_slow_drop_is_bounded() {
    let dir = std::env::temp_dir().join(format!("regia-clean-exit-drop-{}", std::process::id()));

    // 1. A loaded runtime renderer.
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => panic!("no GPU adapter (install Mesa lavapipe or use WARP): {err}"),
    };
    let instance = device.instance.clone();
    let mut app = app_with(&device, &dir, CleanExitSettings::default());
    let frame = map_frame(192, 128);
    let (view, _) = frame.window_views()[0];
    let thread = Arc::new(Mutex::new(None));
    {
        let mut runtime = app.world_mut().resource_mut::<RuntimeRenderer>();
        let scene = frame.frame_scene(view, 320, 180);
        runtime.0.render_offscreen(&device, &scene).expect("map frame");
        runtime.0.set_shared_draw(Some(Box::new(DropProbe {
            thread: thread.clone(),
            sleep: Duration::ZERO,
        })));
        let (meshes, materials, ..) = runtime.0.cache_sizes();
        assert!(
            meshes >= 192 && materials >= 192,
            "the map is resident: {meshes} meshes, {materials} materials"
        );
    }
    drop(device);
    let before = LiveGpuObjects::of(&instance).expect("a wgpu-core backend");
    let t = Instant::now();
    let report = run_clean_exit(
        &mut app,
        &[LifecycleEvent::Suspended, LifecycleEvent::WillTerminate],
    );
    let took = t.elapsed();
    let gpu = regia_render::last_gpu_exit_report().expect("the renderer's hooks ran");
    let at_return = LiveGpuObjects::of(&instance).expect("a wgpu-core backend");
    let (waited, left) = wait_for_exit_drop_threads(EXIT_THREAD_WAIT);
    let after = LiveGpuObjects::of(&instance).expect("a wgpu-core backend");
    let device_step = report.steps.iter().find(|s| s.step == ExitStep::DropDevice);
    eprintln!(
        "loaded renderer: exit in {took:?}; objects before {before:?}; steps {report:?}; {gpu:?}; \
         device step {device_step:?} (its budget is logged, not a bar); objects at return \
         {at_return:?}; waited {waited:?} for the exit's drop threads, still running {left:?}"
    );
    assert_eq!(
        thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some("regia-exit-renderer"),
        "the runtime renderer is dropped off the event-loop thread"
    );
    assert!(gpu.had_device, "{gpu:?}");
    // The exit's drop threads end on their own: the renderer's frees, then the device's destroy.
    // Within its 1.25 s the step reports both done; past it they finish detached, and this wait
    // is what the product's `finish_exit` does before `main` returns.
    assert!(
        left.is_empty(),
        "the exit's drop threads still run {EXIT_THREAD_WAIT:?} after the exit: {left:?} ({gpu:?})"
    );
    // `device_dropped` tells that the step's own device handle went within its time; a renderer
    // still being freed holds another (`at_return` above shows it), so it is logged, not asserted.
    assert_eq!(
        after.total(),
        0,
        "no wgpu object outlives the clean exit's drop threads: {after:?} ({gpu:?})"
    );
    // Generous (lavapipe on a loaded farm); on a 5090 the whole exit takes tens of ms.
    assert!(
        step_ms(&report, ExitStep::DropGpuResources) < Duration::from_secs(5),
        "{report:?}"
    );
    assert!(took < Duration::from_secs(20), "clean exit took {took:?}");
    drop(app);

    // 2. A renderer whose drop is stuck for 5 s: the sequence waits for it no longer than the
    // device step's timeout.
    let device = RenderDevice::headless().expect("the adapter is still there");
    let instance = device.instance.clone();
    let settings = CleanExitSettings {
        device_drop_timeout: Duration::from_millis(500),
        ..CleanExitSettings::default()
    };
    let mut app = app_with(&device, &dir, settings);
    let slow = Arc::new(Mutex::new(None));
    {
        let mut runtime = app.world_mut().resource_mut::<RuntimeRenderer>();
        let small = map_frame(4, 16);
        let scene = small.frame_scene(view, 64, 36);
        runtime.0.render_offscreen(&device, &scene).expect("frame");
        runtime.0.set_shared_draw(Some(Box::new(DropProbe {
            thread: slow.clone(),
            sleep: Duration::from_secs(5),
        })));
    }
    drop(device);
    let t = Instant::now();
    let report = run_clean_exit(
        &mut app,
        &[LifecycleEvent::Suspended, LifecycleEvent::WillTerminate],
    );
    let took = t.elapsed();
    let gpu = regia_render::last_gpu_exit_report().expect("the renderer's hooks ran");
    eprintln!("stuck renderer drop: exit in {took:?}; steps {report:?}; {gpu:?}");
    assert!(
        step_ms(&report, ExitStep::DropGpuResources) < Duration::from_secs(3),
        "the world clear does not wait for the renderer: {report:?}"
    );
    // Inline, the drop alone took 5 s; `renderer_dropped == false` below proves it was cut short,
    // and the per-step bound above is the contract. The total (GPU idle ≤ 1 s, the device step
    // 0.5 s + its 0.1 s minimum wait, a headless frame and thread joins) gets the same 20 s as the
    // unstuck case on a loaded farm (audit-v035-flaky M8: 4.5 s had no scheduling slack).
    assert!(took < Duration::from_secs(20), "clean exit took {took:?}");
    assert!(
        !gpu.renderer_dropped,
        "the report tells the renderer's drop was cut short: {gpu:?}"
    );
    assert!(
        !gpu.device_dropped || gpu.live_after.is_some_and(|l| l.total() > 0),
        "{gpu:?}"
    );
    // The drop ends on its own; the device goes with the renderer's last resource.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let live = LiveGpuObjects::of(&instance).expect("a wgpu-core backend");
        if live.devices == 0 && live.total() == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "the device never went: {live:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        slow.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some("regia-exit-renderer")
    );
    let (waited, left) = wait_for_exit_drop_threads(EXIT_THREAD_WAIT);
    eprintln!("stuck renderer drop: waited {waited:?} for the exit's drop threads after the device went");
    assert!(
        left.is_empty(),
        "the exit's drop threads still run {EXIT_THREAD_WAIT:?} after the device went: {left:?}"
    );
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}
