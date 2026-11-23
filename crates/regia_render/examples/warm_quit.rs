// SPDX-License-Identifier: MPL-2.0
//! Child process of `tests/warm_quit_abort.rs`: quits with the pipeline warm queue in a given
//! state, the way a game does (shutdown on `WillTerminate`, then return from `main`). The test
//! builds it with `panic = "abort"` (the shipping profile's panic strategy) and requires a clean
//! exit: the warm's shutdown and exit hook must never unwind there.
//!
//! `warm_quit <mode> <marker-file>`, mode:
//! - `plain`: start the queue with no job, shut it down, return.
//! - `during-warm`: a job holds its device until shutdown drops a queued release sender;
//!   quit after the job starts, then join it before exit.
//! - `no-shutdown`: the same job, but the queue is never shut down (a custom runner without
//!   `WillTerminate`); the exit hook shuts it down and joins the worker.
//! - `frame-warm`: a real cold frame warm (fresh renderers warming the canonical frame one after
//!   another, far longer than the test); quit once it has created `REGIA_WARM_QUIT_AFTER`
//!   pipelines (default 12, mid-frame). The engine's warm stops between its compiles without
//!   unwinding, so `shutdown` returns soon after the compile in flight.
//! - `frame-warm-no-shutdown`: the same warm, never shut down; the exit hook stops it.
//!
//! Writes `panic=abort` or `panic=unwind`, then `finished` when a job ran to its end, `job
//! stopped: <error>` when a frame warm returned early, `quit at <unix ms>` and `shutdown <ms>`,
//! and `main returns at <unix ms>` to the marker file; `skip: <reason>` without a GPU adapter.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use regia_render::{PipelineWarmQueue, RenderDevice, WarmTier};

fn note(marker: &Path, line: &str) {
    // Main and the warm worker can report concurrently; keep each marker line intact.
    static NOTES: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _note = NOTES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(marker) {
        let _ = writeln!(file, "{line}");
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Fresh renderers warming the canonical frame one after another: each compiles the whole
/// frame's pipeline set again (no pipeline cache is installed), far longer than the test.
fn frame_warms(device: &RenderDevice, marker: &Path) -> Result<(), String> {
    let frame = regia_render::pipeline_warm::canonical_frame();
    let formats = [
        wgpu::TextureFormat::Rgba8UnormSrgb,
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Bgra8Unorm,
    ];
    for format in formats.into_iter().cycle().take(200) {
        let mut renderer = regia_render::FrameRenderer::new();
        if let Err(err) = renderer.warm_extracted(device, &frame, format, true) {
            note(marker, &format!("job stopped: {err}"));
            return Err(err.to_string());
        }
    }
    note(marker, "finished");
    Ok(())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(mode), Some(marker)) = (args.next(), args.next().map(PathBuf::from)) else {
        eprintln!(
            "usage: warm_quit <plain|during-warm|no-shutdown|frame-warm|frame-warm-no-shutdown> <marker-file>"
        );
        std::process::exit(2);
    };
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
    let queue = PipelineWarmQueue::new(1);
    let frame_warm = mode.starts_with("frame-warm");
    if mode != "plain" {
        let (started_tx, started) = std::sync::mpsc::channel::<()>();
        let job_marker = marker.clone();
        if frame_warm {
            let counted_queue = queue.clone();
            queue.push(WarmTier::Boot, "frame warms", move |device| {
                let _ = started_tx.send(());
                let result = frame_warms(device, &job_marker);
                note(
                    &job_marker,
                    &format!("late pipelines {}", counted_queue.cancel_counts().1),
                );
                result
            });
        } else {
            let (release, resume) = std::sync::mpsc::channel::<()>();
            queue.push(WarmTier::Boot, "long cold warm", move |device| {
                let _ = started_tx.send(());
                // The pending job is dropped only when shutdown (or the exit hook)
                // signals cancellation. The worker holds its device until then.
                let _ = resume.recv();
                // Deliberately visit a pipeline creation site after cancellation. In an
                // abort build this must return normally, never try to unwind the job.
                let _ = regia_render::pipeline_cache::cache_for(device);
                note(&job_marker, "finished");
                Ok(())
            });
            queue.push(WarmTier::Map, "release on shutdown", move |_| {
                drop(release);
                panic!("queued job ran after shutdown");
            });
        }
        queue.start(&device);
        if started.recv().is_err() {
            note(&marker, "the warm job never started");
            std::process::exit(3);
        }
        if frame_warm {
            let after: u64 = regia_core::brand::env_var("REGIA_WARM_QUIT_AFTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(12);
            while queue.progress().tiers[WarmTier::Boot as usize].pipelines < after {
                assert!(
                    !queue.is_tier_ready(WarmTier::Boot),
                    "the warm ended before the requested pipeline count"
                );
                std::thread::yield_now();
            }
        }
    } else {
        queue.start(&device);
    }
    note(&marker, &format!("quit at {}", unix_ms()));
    if !mode.ends_with("no-shutdown") {
        let t = Instant::now();
        queue.shutdown();
        note(&marker, &format!("shutdown {}", t.elapsed().as_millis()));
    }
    note(&marker, &format!("main returns at {}", unix_ms()));
}
