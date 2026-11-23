// SPDX-License-Identifier: MPL-2.0
//! The clean exit (ADR-188) on a headless device: the steps run in order, the GPU drains within
//! its timeout, and the device and every wgpu object of its instance are gone before
//! `run_clean_exit` returns, with a warm worker holding a device until shutdown and GPU work
//! in flight at the quit.
//!
//! `cargo test -p regia_render --test clean_exit_sequence`

use std::time::Instant;

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_platform::{AppLifecycle, ExitStep, LifecycleEvent, run_clean_exit};
use regia_render::{FrameRenderer, LiveGpuObjects, PipelineWarmQueue, RenderDevice, RenderPlugin, WarmTier};

/// GPU objects owned by the world (a game's renderer, buffers and textures).
#[derive(Resource)]
struct GameGpu {
    _renderer: FrameRenderer,
    _buffers: Vec<wgpu::Buffer>,
    _texture: wgpu::Texture,
}

#[test]
fn the_clean_exit_runs_in_order_and_destroys_the_device_before_returning() {
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            eprintln!("skip clean_exit_sequence: no GPU device ({err})");
            return;
        }
    };
    let instance = device.instance.clone();
    let backend = device.adapter.get_info().backend;
    let dir = regia_test_utils::unique_temp_dir("regia-clean-exit-seq");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(AssetPlugin {
            settings: AssetServerSettings {
                project_root: dir.clone(),
                platform: "linux".into(),
                content_dir: None,
            },
        })
        .add_plugins(RenderPlugin { headless: true })
        .init_resource::<AppLifecycle>()
        .add_message::<LifecycleEvent>()
        .insert_resource(device.clone());
    app.finish_plugins();
    app.update();

    // A renderer with its frame's pipelines and copy buffers. This verifies teardown order,
    // not copy throughput: a 2 GiB train consumed the unchanged one-second idle budget on
    // a loaded software adapter (ADR-4760).
    let mut renderer = FrameRenderer::new();
    renderer
        .warm_extracted(
            &device,
            &regia_render::pipeline_warm::canonical_frame(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            true,
        )
        .unwrap_or_else(|e| panic!("warm: {e}"));
    let make = |size: u64, usage: wgpu::BufferUsages| {
        device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("clean exit test"),
            size,
            usage,
            mapped_at_creation: false,
        })
    };
    const COPY_BYTES: u64 = 4096;
    let src = make(
        COPY_BYTES,
        wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
    );
    let dst = make(
        COPY_BYTES,
        wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
    );
    let texture = device.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("clean exit test"),
        size: wgpu::Extent3d {
            width: 1024,
            height: 1024,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });

    // The worker holds a device clone until shutdown drops the queued release sender.
    // No sleep or scheduler speed determines whether the worker is running at the quit.
    // One worker: with a second one the queued job ran at once and let the first job go, so no
    // worker was inside a job at the quit, and the queue caught the job's panic as a failed job
    // that no assertion saw.
    let queue = app.world().resource::<PipelineWarmQueue>().clone();
    queue.set_engine_jobs(false);
    queue.set_threads(1);
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    let (release, resume) = std::sync::mpsc::channel::<()>();
    queue.push(WarmTier::Boot, "long compile", move |_device: &RenderDevice| {
        let _ = started_tx.send(());
        let _ = resume.recv();
        Ok(())
    });
    let queued_job_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran = queued_job_ran.clone();
    queue.push(WarmTier::Map, "release on shutdown", move |_| {
        ran.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(release);
        Ok(())
    });
    queue.start(&device);
    started.recv().expect("the warm job started");

    // GPU work submitted at the quit, not waited for.
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for _ in 0..16 {
        encoder.copy_buffer_to_buffer(&src, 0, &dst, 0, COPY_BYTES);
        encoder.copy_buffer_to_buffer(&dst, 0, &src, 0, COPY_BYTES);
    }
    device.queue.submit([encoder.finish()]);
    app.insert_resource(GameGpu {
        _renderer: renderer,
        _buffers: vec![src, dst],
        _texture: texture,
    });
    drop(queue);
    drop(device);
    let before = LiveGpuObjects::of(&instance).expect("a wgpu-core backend");
    assert_eq!(before.devices, 1, "{before:?}");

    let t = Instant::now();
    let report = run_clean_exit(
        &mut app,
        &[LifecycleEvent::Suspended, LifecycleEvent::WillTerminate],
    );
    let took = t.elapsed();

    let steps: Vec<ExitStep> = report.steps.iter().map(|s| s.step).collect();
    assert_eq!(steps, ExitStep::ALL);
    assert!(
        !queued_job_ran.load(std::sync::atomic::Ordering::SeqCst),
        "shutdown must drop the queued job without running it"
    );
    let gpu = regia_render::last_gpu_exit_report().expect("the renderer's hooks ran");
    eprintln!("clean exit in {took:?}: {report:?}\nrenderer: {gpu:?}");
    assert!(gpu.warm_stopped, "{gpu:?}");
    assert_eq!(gpu.gpu_idle, Some(true), "the copies drained within the timeout");
    assert!(gpu.had_device && gpu.device_dropped, "{gpu:?}");
    let after = LiveGpuObjects::of(&instance).expect("a wgpu-core backend");
    assert_eq!(after.devices, 0, "the device is gone before return: {after:?}");
    assert_eq!(
        after.total(),
        0,
        "no wgpu object outlives the clean exit: {after:?}"
    );
    assert_eq!(gpu.live_after, Some(after));
    drop(app);
    // The clean exit also let go of the process's hold on the Vulkan drivers (ADR-5794): once the
    // last instance is gone the loader is unloaded, and its drivers with it, so the process ends
    // with no driver left for its exit handlers, as it did before that hold existed.
    drop(instance);
    if cfg!(target_os = "linux")
        && backend == wgpu::Backend::Vulkan
        && let Some(loaders) = mapped_vulkan_loaders()
    {
        assert!(
            loaders.is_empty(),
            "the Vulkan loader is still loaded after the clean exit and the last instance: {loaders:?}"
        );
        // Letting go is not final: a device opened afterwards holds the drivers again, also
        // once it is gone.
        drop(RenderDevice::headless().expect("a device after the clean exit"));
        assert!(
            mapped_vulkan_loaders().is_some_and(|loaders| !loaders.is_empty()),
            "a device opened after the clean exit did not keep the Vulkan loader loaded"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Vulkan loader libraries mapped into this process now (the `libvulkan.so*` paths of
/// `/proc/self/maps`), or `None` where that file cannot be read.
fn mapped_vulkan_loaders() -> Option<std::collections::BTreeSet<String>> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    Some(
        maps.lines()
            .filter_map(|line| line.split_whitespace().nth(5))
            .filter(|path| {
                path.rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .starts_with("libvulkan.so")
            })
            .map(str::to_owned)
            .collect(),
    )
}
