// SPDX-License-Identifier: MIT-0
//! Real-GPU acceptance (ignored by default; the lead runs it on the build server):
//!
//! 1. two runs of the same 120-frame render-out give identical hashes;
//! 2. one 3840x2160 frame renders, and its time is recorded.
//!
//! Environment: `SHOWCASE_ACCEPT_LOOK=plain|film|film-fixed` (default film; `film-fixed` is the film look with a
//! fixed exposure of EV100 14.5), `SHOWCASE_ACCEPT_SIZE=WxH` for
//! part 1 (default 1920x1080), `SHOWCASE_KEEP_FRAMES=<dir>` keeps first and last frames,
//! `SHOWCASE_ACCEPT_DOLLY=ex,ey,ez` moves the camera eye from `SHOWCASE_ACCEPT_CAMERA`'s to there over the job,
//! `SHOWCASE_ACCEPT_CPU_DRAW=1` draws on the CPU path instead of the GPU-driven default and
//! `SHOWCASE_ACCEPT_ENTITY_SCATTER=1` uses the capped entity scatter instead of the streamed field (ADR-1415).
//! Pick the card with `REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1 VK_LOADER_DRIVERS_SELECT=*nvidia*`
//! under the card's `flock /tmp/regia-gpu-<n>.lock flock /tmp/ferrite-gpu-<n>.lock`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{JobSpec, run};

fn look() -> String {
    std::env::var("SHOWCASE_ACCEPT_LOOK").unwrap_or_else(|_| "film".into())
}

fn look_is_plain() -> bool {
    look() == "plain"
}

/// `film-fixed`: the film look (temporal anti-aliasing, bloom) with a fixed exposure.
fn look_exposure() -> Option<f32> {
    (look() == "film-fixed").then_some(14.5)
}

/// `SHOWCASE_ACCEPT_CAMERA=ex,ey,ez,tx,ty,tz`: the camera pose (default: the harness's).
fn camera() -> Option<([f32; 3], [f32; 3])> {
    let text = std::env::var("SHOWCASE_ACCEPT_CAMERA").ok()?;
    let v: Vec<f32> = text.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    (v.len() == 6).then(|| ([v[0], v[1], v[2]], [v[3], v[4], v[5]]))
}

/// `SHOWCASE_ACCEPT_DOLLY=ex,ey,ez`: where the camera eye ends (a dolly over the job); default a held camera.
fn camera_end() -> Option<[f32; 3]> {
    let text = std::env::var("SHOWCASE_ACCEPT_DOLLY").ok()?;
    let v: Vec<f32> = text.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    (v.len() == 3).then(|| [v[0], v[1], v[2]])
}

/// `SHOWCASE_ACCEPT_CPU_DRAW=1`: the CPU draw path instead of the GPU-driven default.
fn gpu_driven() -> bool {
    std::env::var_os("SHOWCASE_ACCEPT_CPU_DRAW").is_none()
}

/// `SHOWCASE_ACCEPT_ENTITY_SCATTER=1`: the capped entity scatter (the path before ADR-1415) for a before/after.
fn entity_scatter() -> bool {
    std::env::var_os("SHOWCASE_ACCEPT_ENTITY_SCATTER").is_some()
}

fn size() -> (u32, u32) {
    std::env::var("SHOWCASE_ACCEPT_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1920, 1080))
}

#[test]
#[ignore = "needs a real GPU; run by the lead on buildhost"]
fn two_runs_of_120_frames_give_identical_hashes() {
    let (width, height) = size();
    let spec = JobSpec {
        width,
        height,
        frames: 120,
        warmup_frames: 0,
        plain: look_is_plain(),
        scatter_lod: 1,
        exposure_ev: look_exposure(),
        repeat: 1,
        keep: std::env::var_os("SHOWCASE_KEEP_FRAMES").map(Into::into),

        camera: camera(),
        camera_end: camera_end(),
        gpu_driven: gpu_driven(),
        entity_scatter: entity_scatter(),
        ..JobSpec::default()
    };
    let a = run(1, &spec);
    let b = run(
        2,
        &JobSpec {
            keep: None,
            ..spec.clone()
        },
    );
    eprintln!(
        "showcase acceptance: 120 frames at {width}x{height}, plain={}: run 1 {} ms, run 2 {} ms, sequence {} / {}",
        spec.plain, a.job_ms, b.job_ms, a.sequence, b.sequence
    );
    eprintln!(
        "showcase acceptance: GPU pass timings of the last frame: {}",
        a.gpu_timings
    );
    let differing: Vec<usize> = a
        .hashes
        .iter()
        .zip(&b.hashes)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .map(|(i, _)| i)
        .collect();
    assert!(
        differing.is_empty(),
        "frames {differing:?} differ between the two runs"
    );
    assert_eq!(a.sequence, b.sequence);
}

#[test]
#[ignore = "needs a real GPU; run by the lead on buildhost"]
fn one_4k_frame_renders_and_its_time_is_recorded() {
    let base = JobSpec {
        width: 3840,
        height: 2160,
        frames: 1,
        warmup_frames: 0,
        plain: look_is_plain(),
        scatter_lod: 1,
        exposure_ev: look_exposure(),
        repeat: 1,
        keep: std::env::var_os("SHOWCASE_KEEP_FRAMES").map(Into::into),
        gpu_driven: gpu_driven(),
        entity_scatter: entity_scatter(),
        ..JobSpec::default()
    };
    // The same one-frame job three times in one app: the first pays the cold costs (terrain tiles,
    // pipelines, scatter upload); the later ones are the steady frame time, which includes the 4K readback
    // and the PNG encode.
    let timed = run(3, &JobSpec { repeat: 3, ..base });
    assert!(
        timed.distinct_pixel_values > 1000,
        "{} colours",
        timed.distinct_pixel_values
    );
    eprintln!(
        "showcase acceptance: 3840x2160 one-frame jobs, wall time per job in ms (readback and PNG write included): {:?}",
        timed.all_job_ms
    );
    eprintln!(
        "showcase acceptance: GPU pass timings of the last frame: {}",
        timed.gpu_timings
    );
}

/// One 120-frame run per process, hashes written to `SHOWCASE_HASH_OUT`: run it twice (two processes) and
/// compare the files to rule out state that two runs in one process share.
#[test]
#[ignore = "needs a real GPU and SHOWCASE_HASH_OUT; run by the lead on buildhost"]
fn one_run_of_120_frames_writes_its_hashes() {
    let Some(path) = std::env::var_os("SHOWCASE_HASH_OUT") else {
        eprintln!("SHOWCASE_HASH_OUT is not set; nothing to write");
        return;
    };
    let (width, height) = size();
    let spec = JobSpec {
        width,
        height,
        frames: 120,
        warmup_frames: 0,
        plain: look_is_plain(),
        scatter_lod: 1,
        exposure_ev: look_exposure(),
        repeat: 1,
        keep: None,
        gpu_driven: gpu_driven(),
        entity_scatter: entity_scatter(),
        ..JobSpec::default()
    };
    let outcome = run(
        1,
        &JobSpec {
            camera: camera(),
            camera_end: camera_end(),
            ..spec
        },
    );
    std::fs::write(&path, outcome.hashes.join("\n")).expect("write hashes");
    eprintln!(
        "showcase acceptance: wrote {} hashes, sequence {}",
        outcome.hashes.len(),
        outcome.sequence
    );
}

/// Frame cost of a scene (`SHOWCASE_TEST_SCENE`, e.g. the 8192 m showcase map) at 2560x1440 and 3840x2160 from
/// `SHOWCASE_ACCEPT_CAMERA`: scene load time, the GPU pass timings of the last of three one-frame jobs, the
/// renderer's memory report, peak process memory and the scatter counts. Prints only; nothing is asserted but
/// that a frame was drawn.
#[test]
#[ignore = "needs a real GPU; run by the lead on buildhost"]
fn frame_cost_at_1440p_and_4k() {
    for (width, height) in [(2560, 1440), (3840, 2160)] {
        let outcome = run(
            4,
            &JobSpec {
                width,
                height,
                frames: 1,
                warmup_frames: 30,
                plain: look_is_plain(),
                scatter_lod: 1,
                exposure_ev: look_exposure(),
                repeat: 3,
                camera: camera(),
                hiz: std::env::var_os("SHOWCASE_ACCEPT_HIZ").is_some(),
                gpu_driven: gpu_driven(),
                entity_scatter: entity_scatter(),
                keep: std::env::var_os("SHOWCASE_KEEP_FRAMES")
                    .map(|d| std::path::PathBuf::from(d).join(format!("{height}p"))),
                ..JobSpec::default()
            },
        );
        assert!(
            outcome.distinct_pixel_values > 1000,
            "{} colours",
            outcome.distinct_pixel_values
        );
        eprintln!(
            "showcase cost {width}x{height}: load {} ms, jobs {:?} ms, peak RSS {} MiB",
            outcome.load_ms,
            outcome.all_job_ms,
            outcome.peak_rss_kib / 1024
        );
        eprintln!("showcase cost {width}x{height}: stats {:?}", outcome.stats);
        eprintln!("showcase cost {width}x{height}: GPU {}", outcome.gpu_timings);
        eprintln!("showcase cost {width}x{height}: memory {}", outcome.memory);
    }
}

/// Mean cost per frame of a 600-frame render-out job at 2560x1440 (Desktop High film look): three repeats of the same
/// job, so the first (pipeline compiles) is told apart from the steady two. Prints the wall time per frame (render,
/// readback and hashing included) and the GPU pass timings of the last frame. Comparable between cards when run
/// on the same build; nothing is asserted but that a frame was drawn.
#[test]
#[ignore = "needs a real GPU; run by the lead on buildhost"]
fn mean_frame_cost_over_600_frames_at_1440p() {
    let outcome = run(
        5,
        &JobSpec {
            width: 2560,
            height: 1440,
            frames: 600,
            warmup_frames: 60,
            plain: look_is_plain(),
            scatter_lod: 1,
            exposure_ev: look_exposure(),
            repeat: 3,
            camera: camera(),
            camera_end: camera_end(),
            gpu_driven: gpu_driven(),
            entity_scatter: entity_scatter(),
            ..JobSpec::default()
        },
    );
    assert!(
        outcome.distinct_pixel_values > 1000,
        "{} colours",
        outcome.distinct_pixel_values
    );
    let per_frame: Vec<String> = outcome
        .all_job_ms
        .iter()
        .map(|ms| format!("{:.3}", *ms as f64 / 600.0))
        .collect();
    eprintln!(
        "showcase 600 frames 2560x1440: ms per frame by repeat {per_frame:?}, peak RSS {} MiB",
        outcome.peak_rss_kib / 1024
    );
    eprintln!("showcase 600 frames 2560x1440: GPU {}", outcome.gpu_timings);
}
