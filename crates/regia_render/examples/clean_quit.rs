// SPDX-License-Identifier: MPL-2.0
//! Child process of `tests/clean_quit_abort.rs`: an app on a headless device quits in the middle
//! of a frame, with a real cold frame warm running on the pipeline warm queue and 64 MiB copies
//! in flight on the GPU, then runs the clean exit (`regia_platform::run_clean_exit`, what the
//! platform runner does from winit's `exiting`) and returns from `main`. The test builds it with
//! `panic = "abort"` (the shipping profile's panic strategy).
//!
//! `clean_quit <marker-file>`. Writes `panic=abort` or `panic=unwind`, `quit at <unix ms>`, one
//! `step <name> <ms>` line per exit step, `gpu <report>`, `live <n>` (wgpu objects left),
//! `device gone <bool>` and `main returns at <unix ms>`; `skip: <reason>` without a GPU adapter.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regia_app::prelude::*;
use regia_asset::{AssetPlugin, AssetServerSettings};
use regia_platform::{AppLifecycle, LifecycleEvent, LifecycleTracker, run_clean_exit};
use regia_render::{LiveGpuObjects, PipelineWarmQueue, RenderDevice, RenderPlugin, WarmTier};

fn note(marker: &Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(marker) {
        let _ = writeln!(file, "{line}");
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

#[derive(Resource)]
struct Marker(PathBuf);

/// GPU buffers owned by the world, copied back and forth every frame.
#[derive(Resource)]
struct Copies(wgpu::Buffer, wgpu::Buffer);

/// `Update`: a frame's GPU work, and on frame 5 the quit (mid-frame: the rest of the frame's
/// schedules still run).
fn frame(
    device: Res<RenderDevice>,
    copies: Res<Copies>,
    marker: Res<Marker>,
    mut frames: Local<u32>,
    mut exit: MessageWriter<AppExit>,
) {
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    for _ in 0..8 {
        encoder.copy_buffer_to_buffer(&copies.0, 0, &copies.1, 0, copies.0.size());
        encoder.copy_buffer_to_buffer(&copies.1, 0, &copies.0, 0, copies.0.size());
    }
    device.queue.submit([encoder.finish()]);
    *frames += 1;
    if *frames == 5 {
        note(&marker.0, &format!("quit at {}", unix_ms()));
        exit.write(AppExit::Success);
    }
}

fn main() {
    let Some(marker) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: clean_quit <marker-file>");
        std::process::exit(2);
    };
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).try_init();
    note(
        &marker,
        if cfg!(panic = "abort") {
            "panic=abort"
        } else {
            "panic=unwind"
        },
    );
    let device = match RenderDevice::headless() {
        Ok(device) => device,
        Err(err) => {
            note(&marker, &format!("skip: {err}"));
            return;
        }
    };
    let instance = device.instance.clone();
    let dir = std::env::temp_dir().join(format!("regia-clean-quit-{}", std::process::id()));
    let buffer = |label| {
        device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: 64 << 20,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
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
        .insert_resource(Copies(buffer("copy a"), buffer("copy b")))
        .insert_resource(Marker(marker.clone()))
        .insert_resource(device.clone())
        .add_systems(Update, frame);
    app.finish_plugins();

    // A cold frame warm on the warm queue, far longer than the run: fresh renderers warming
    // the canonical frame one after another.
    let queue = app.world().resource::<PipelineWarmQueue>().clone();
    queue.set_engine_jobs(false);
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    queue.push(WarmTier::Boot, "frame warms", move |device: &RenderDevice| {
        let _ = started_tx.send(());
        for _ in 0..200 {
            let mut renderer = regia_render::FrameRenderer::new();
            renderer
                .warm_extracted(
                    device,
                    &regia_render::pipeline_warm::canonical_frame(),
                    wgpu::TextureFormat::Bgra8UnormSrgb,
                    true,
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    });
    queue.start(&device);
    drop(queue);
    drop(device);
    if started.recv_timeout(Duration::from_secs(60)).is_err() {
        note(&marker, "the warm job never started");
        std::process::exit(3);
    }
    // Let the warm get into its compiles.
    std::thread::sleep(Duration::from_millis(200));

    // The frame loop, as the runner drives it, until the app asks to exit.
    let mut tracker = LifecycleTracker::default();
    tracker.resumed();
    let t = Instant::now();
    while app.should_exit().is_none() && t.elapsed() < Duration::from_secs(60) {
        app.update();
    }
    let report = run_clean_exit(&mut app, &tracker.exiting());
    for step in &report.steps {
        note(
            &marker,
            &format!("step {:?} {}", step.step, step.elapsed.as_millis()),
        );
    }
    let gpu = regia_render::last_gpu_exit_report();
    note(&marker, &format!("gpu {gpu:?}"));
    let live = LiveGpuObjects::of(&instance);
    note(&marker, &format!("live {}", live.map_or(0, |l| l.total())));
    note(
        &marker,
        &format!("device gone {}", live.is_some_and(|l| l.devices == 0)),
    );
    drop(app);
    drop(instance);
    let _ = std::fs::remove_dir_all(&dir);
    note(&marker, &format!("main returns at {}", unix_ms()));
}
