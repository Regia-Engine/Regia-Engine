// SPDX-License-Identifier: MIT-0
//! Ambient life through the whole showcase app on the headless adapter (ADR-3024): turned on through
//! the agent, it draws over the scene, flies the automatic birds, aims the occlusion view along the
//! scene's morning sun (past the 45° the rain's view stops at, not reported as the rain's), and two
//! fresh apps deliver the same frames, as `book/agents/render-out.md` promises.
//!
//! Set `REGIA_AMBIENT_SHEET_DIR` to keep the frames for a look.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{JobSpec, run, run_with};
use regia_render::{RenderDevice, RuntimeRenderer};
use regia_vfx::{AmbientLifeReport, AmbientLifeSettings};
use serde_json::json;

fn spec(on: bool, keep: &str) -> JobSpec {
    let calls = if on {
        vec![(
            "render.set_ambient_life".to_string(),
            // The patience the harness gives its other calls: under load a frame can pass the 30 s default.
            json!({ "enabled": true, "season": "summer", "density_scale": 2.0, "timeout_ms": 600_000 }),
        )]
    } else {
        Vec::new()
    };
    JobSpec {
        width: 640,
        height: 360,
        frames: 3,
        warmup_frames: 2,
        calls,
        after: vec![
            ("render.ambient_life".to_string(), json!({})),
            ("render.precipitation".to_string(), json!({})),
        ],
        keep: std::env::var_os("REGIA_AMBIENT_SHEET_DIR").map(|dir| std::path::Path::new(&dir).join(keep)),
        ..JobSpec::default()
    }
}

#[test]
fn the_showcase_comes_alive_and_repeats() {
    if let Err(err) = RenderDevice::headless() {
        eprintln!("skip showcase ambient life test: {err}");
        return;
    }
    let off = run(1, &spec(false, "off"));
    assert_eq!(off.reports[0]["active"], json!(false), "{}", off.reports[0]);
    assert_eq!(off.reports[0]["sun_view"], json!(false));
    assert!(off.reports[0]["sun_view_tilt_deg"].is_null());

    let on = run(2, &spec(true, "on"));
    let report = &on.reports[0];
    println!("{report}");
    assert_eq!(report["active"], json!(true), "{report}");
    assert!(report["drawn"]["motes"].as_f64().unwrap_or(0.0) > 0.0, "{report}");
    assert!(report["birds_drawn"].as_u64().unwrap_or(0) > 0, "{report}");
    assert_eq!(report["flocks"].as_array().map(Vec::len), Some(2), "{report}");
    assert_eq!(report["sun_view"], json!(true), "{report}");
    let tilt = report["sun_view_tilt_deg"]
        .as_f64()
        .expect("the sun view was drawn");
    assert!(tilt > 45.0 && tilt <= 80.0, "the scene's morning sun: {tilt}");
    // The kinds' activity and the shafts run on one sun: the scene's sky clock.
    let elevation = report["conditions"]["sun_elevation_sin"]
        .as_f64()
        .expect("conditions")
        .asin()
        .to_degrees();
    assert!(
        (elevation - (90.0 - tilt)).abs() < 2.0,
        "the activity's sun {elevation} against the view's tilt {tilt}"
    );
    assert!(
        on.reports[1]["occlusion"].is_null(),
        "not the rain's view: {}",
        on.reports[1]
    );
    assert_ne!(on.hashes, off.hashes, "the layer drew");

    let again = run(3, &spec(true, "again"));
    assert_eq!(on.hashes, again.hashes, "two fresh apps deliver the same frames");
}

/// A look for the eye, not a check: the same poses with ambient life off and on, film look, at
/// `REGIA_AMBIENT_LOOK_SIZE` (default 2560x1440), on the scene `SHOWCASE_TEST_SCENE` names. Writes
/// `<pose>-off` and `<pose>-on` under `REGIA_AMBIENT_SHEET_DIR`, and prints how many pixels moved.
///
/// `REGIA_AMBIENT_LOOK_POSES` is `name:ex,ey,ez,tx,ty,tz` joined by `;`. `REGIA_AMBIENT_LOOK_CALL` is the
/// `render.set_ambient_life` params as JSON (default: on, summer). The scene's `sky.ron` sets the time of day.
/// `REGIA_AMBIENT_LOOK_EXTRA` is more calls after it, as JSON `[[method, params], ...]` (a placed flock, say).
/// `REGIA_AMBIENT_LOOK_FRAMES` frames at `REGIA_AMBIENT_LOOK_FPS` (defaults 1 and 60) are all kept, to judge motion.
/// `REGIA_AMBIENT_LOOK_ONLY_ON` skips the frames with ambient life off, for a clip to watch.
/// `REGIA_AMBIENT_LOOK_WARMUP` is the warm-up frames (default 12): a held camera's image settles for about 36.
#[test]
#[ignore = "a look at film resolution; slow on the software adapter and read by eye"]
fn look_at_film_resolution() {
    let Some(dir) = std::env::var_os("REGIA_AMBIENT_SHEET_DIR") else {
        eprintln!("skip: REGIA_AMBIENT_SHEET_DIR is not set");
        return;
    };
    let (width, height) = std::env::var("REGIA_AMBIENT_LOOK_SIZE")
        .ok()
        .and_then(|s| s.split_once('x').map(|(w, h)| (w.parse().ok(), h.parse().ok())))
        .and_then(|(w, h)| w.zip(h))
        .unwrap_or((2560, 1440));
    let poses =
        std::env::var("REGIA_AMBIENT_LOOK_POSES").unwrap_or_else(|_| "default:60,46,-170,0,36,-100".into());
    let ambient: serde_json::Value = std::env::var("REGIA_AMBIENT_LOOK_CALL")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "enabled": true, "season": "summer" }));
    let extra: Vec<(String, serde_json::Value)> = std::env::var("REGIA_AMBIENT_LOOK_EXTRA")
        .ok()
        .map(|s| serde_json::from_str(&s).expect("[[method, params], ...]"))
        .unwrap_or_default();
    let number = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    let frames = number("REGIA_AMBIENT_LOOK_FRAMES", 1);
    let fps = number("REGIA_AMBIENT_LOOK_FPS", 60) as u32;
    let warmup_frames = number("REGIA_AMBIENT_LOOK_WARMUP", 12);
    for (run_id, pose) in poses.split(';').enumerate() {
        let (name, numbers) = pose.split_once(':').expect("name:ex,ey,ez,tx,ty,tz");
        let v: Vec<f32> = numbers.split(',').map(|n| n.parse().expect("a number")).collect();
        let camera = ([v[0], v[1], v[2]], [v[3], v[4], v[5]]);
        let job = |on: bool| {
            let mut calls = Vec::new();
            if on {
                let mut params = ambient.clone();
                params["timeout_ms"] = json!(600_000);
                calls.push(("render.set_ambient_life".to_string(), params));
                calls.extend(extra.iter().cloned().map(|(method, mut params)| {
                    params["timeout_ms"] = json!(600_000);
                    (method, params)
                }));
            }
            JobSpec {
                width,
                height,
                frames,
                fps,
                warmup_frames,
                plain: false,
                scatter_lod: 0,
                gpu_driven: true,
                camera: Some(camera),
                calls,
                after: vec![("render.ambient_life".to_string(), json!({}))],
                keep: Some(
                    std::path::Path::new(&dir).join(format!("{name}-{}", if on { "on" } else { "off" })),
                ),
                keep_every_frame: true,
                ..JobSpec::default()
            }
        };
        let on = run(101 + 2 * run_id as u32, &job(true));
        if std::env::var_os("REGIA_AMBIENT_LOOK_ONLY_ON").is_some() {
            println!("{name}: on {} ms; {}", on.job_ms, on.reports[0]);
            continue;
        }
        let off = run(100 + 2 * run_id as u32, &job(false));
        let moved = off
            .first_frame
            .2
            .as_chunks::<4>()
            .0
            .iter()
            .zip(on.first_frame.2.as_chunks::<4>().0)
            .filter(|(a, b)| a.iter().zip(b.iter()).take(3).any(|(x, y)| x.abs_diff(*y) > 2))
            .count();
        println!(
            "{name}: {moved} of {} pixels moved; off {} ms, on {} ms; {}",
            width * height,
            off.job_ms,
            on.job_ms,
            on.reports[0]
        );
    }
}

/// The draw's real cost on a card, for the budget in ADR-3024: one job at 2560x1440 on the scene
/// `SHOWCASE_TEST_SCENE` names, at the first pose of `REGIA_AMBIENT_LOOK_POSES`, with
/// `REGIA_AMBIENT_LOOK_CALL` as the call that turns it on. The kinds take turns between the job's frames, so
/// the card's state and the scene's motion fall on all alike, and the first round (the pipelines warming) is
/// dropped. Prints the median of each pass that a kind can move. Numbers only: nothing is asserted, and a host
/// busy elsewhere leaves the card idling between frames, so they lean high.
///
/// `REGIA_GPU_UUID=<card> SHOWCASE_TEST_SCENE=<scene> cargo test -p showcase --test ambient_life bench_on_a_card -- --ignored --nocapture`
#[test]
#[ignore = "prints numbers, and only means something on a real card"]
fn bench_on_a_card() {
    const ROUNDS: u64 = 8;
    const SKIPPED_AT_START: u64 = 30;
    const SETTLE: u64 = 5;
    const KEPT: u64 = 8;
    // Motes, insects, birds, leaves.
    let modes: [(&str, [bool; 4]); 7] = [
        ("off", [false; 4]),
        ("motes", [true, false, false, false]),
        ("insects", [false, true, false, false]),
        ("birds", [false, false, true, false]),
        ("leaves", [false, false, false, true]),
        ("off", [false; 4]),
        ("all", [true; 4]),
    ];
    let pose =
        std::env::var("REGIA_AMBIENT_LOOK_POSES").unwrap_or_else(|_| "default:60,46,-170,0,36,-100".into());
    let (_, numbers) = pose
        .split(';')
        .next()
        .and_then(|p| p.split_once(':'))
        .expect("name:ex,ey,ez,tx,ty,tz");
    let v: Vec<f32> = numbers.split(',').map(|n| n.parse().expect("a number")).collect();
    let mut ambient: serde_json::Value = std::env::var("REGIA_AMBIENT_LOOK_CALL")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "enabled": true, "season": "summer" }));
    ambient["timeout_ms"] = json!(600_000);
    let spec = JobSpec {
        width: 2560,
        height: 1440,
        frames: SKIPPED_AT_START + ROUNDS * modes.len() as u64 * (SETTLE + KEPT),
        warmup_frames: 12,
        plain: false,
        scatter_lod: 0,
        gpu_driven: true,
        camera: Some(([v[0], v[1], v[2]], [v[3], v[4], v[5]])),
        calls: vec![("render.set_ambient_life".to_string(), ambient)],
        after: vec![("render.ambient_life".to_string(), json!({}))],
        ..JobSpec::default()
    };

    #[derive(Clone, Copy, Default)]
    struct Sample {
        frame: f32,
        scene: f32,
        post: f32,
        shadows: f32,
        drawn: f32,
        /// 1 when the frame drew the occlusion view along the sun (its cost is in `shadows`).
        sun_view: f32,
    }
    let mut samples: Vec<Vec<Sample>> = vec![Vec::new(); modes.len()];
    let (mut last_frame, mut seen, mut current) = (0, 0_u64, usize::MAX);
    let outcome = run_with(1, &spec, |app| {
        // A headless app never turns the timestamps on (the windowed showcase's `--gpu-timing` does).
        let device = app.world().get_resource::<RenderDevice>().cloned();
        if let (Some(device), Some(mut renderer)) =
            (device, app.world_mut().get_resource_mut::<RuntimeRenderer>())
            && device.capabilities.timestamp_query
            && !renderer.0.gpu_timing_enabled()
        {
            renderer.0.set_gpu_timing(&device, true);
        }
        let Some(renderer) = app.world().get_resource::<RuntimeRenderer>().map(|r| &r.0) else {
            return;
        };
        let frame = renderer.frame_index();
        if frame == last_frame {
            return;
        }
        last_frame = frame;
        seen += 1;
        let Some(step) = seen.checked_sub(SKIPPED_AT_START) else {
            return;
        };
        let turn = step / (SETTLE + KEPT);
        let mode = (turn % modes.len() as u64) as usize;
        if step % (SETTLE + KEPT) >= SETTLE && turn >= modes.len() as u64 {
            let timing = renderer.gpu_timings();
            let counts = app.world().resource::<AmbientLifeReport>().counts;
            samples[mode].push(Sample {
                frame: timing.frame_ms,
                scene: timing.scene_ms,
                post: timing.post_ms,
                shadows: timing.shadows_ms,
                drawn: counts.motes
                    + counts.swarm_members
                    + counts.butterflies
                    + counts.fireflies
                    + counts.leaves
                    + counts.birds,
                sun_view: if renderer.sun_occlusion().is_some() {
                    1.0
                } else {
                    0.0
                },
            });
        }
        if mode != current {
            current = mode;
            let [motes, insects, birds, leaves] = modes[mode].1;
            let mut settings = app.world_mut().resource_mut::<AmbientLifeSettings>();
            settings.motes = motes;
            settings.insects = insects;
            settings.birds = birds;
            settings.leaves = leaves;
        }
    });
    let median = |mode: usize, of: fn(&Sample) -> f32| {
        let mut values: Vec<f32> = samples[mode].iter().map(of).collect();
        values.sort_by(f32::total_cmp);
        values.get(values.len() / 2).copied().unwrap_or(f32::NAN)
    };
    let mean = |mode: usize, of: fn(&Sample) -> f32| {
        samples[mode].iter().map(of).sum::<f32>() / samples[mode].len().max(1) as f32
    };
    // Taken after the job, in whichever kind's turn it ended: its counts are not the rows'.
    println!("{}", outcome.reports[0]);
    let off_post = median(0, |s| s.post);
    let off_frame = median(0, |s| s.frame);
    assert!(
        off_frame > 0.0,
        "no GPU timings were read: the adapter has no timestamp queries, or they never turned on"
    );
    for (mode, (name, _)) in modes.iter().enumerate() {
        println!(
            "{name:8} n {:3}  drawn {:7.0}  post {:7.3} ms ({:+.3})  frame {:7.3} ms ({:+.3})  scene {:7.3}  shadows {:7.3}  sun view {:3.0} %",
            samples[mode].len(),
            median(mode, |s| s.drawn),
            median(mode, |s| s.post),
            median(mode, |s| s.post) - off_post,
            median(mode, |s| s.frame),
            median(mode, |s| s.frame) - off_frame,
            median(mode, |s| s.scene),
            median(mode, |s| s.shadows),
            100.0 * mean(mode, |s| s.sun_view),
        );
    }
}
