// SPDX-License-Identifier: MPL-2.0
//! Render-out time map, its two clocks and replays on the render-out clock (ADR-1519, ADR-1524),
//! on lavapipe:
//!
//! - a 25 fps job at half speed walks the simulation clock exactly as a 50 fps job does, so its
//!   frames hash the same as the 50 fps frames (a quad slides with simulation time, in
//!   `FixedUpdate`), while `Update` keeps seeing the 25 fps render step;
//! - with a ramp, every delivered frame's simulation time (`Time::sim_elapsed` as the app saw it)
//!   is the manifest's `frame_times[].sim_time_ns`, which is the documented formula, the render
//!   time (`Time::elapsed`) is `i · step`, and the shutter changes neither;
//! - a replay armed with `replay.play { start: "render_out" }` is at `from_tick` on frame 0 and at
//!   `from_tick + tick_rate · speed · sim_time` on every later frame: its speed composes with the
//!   map.
//!
//! Nothing asserts on wall time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_framework::replay_sim::{ReplayHeader, ReplaySim, SyntheticReplay};
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// World units per game second of the sliding quad.
const SPEED: f32 = 6.0;

#[derive(Component)]
struct Slider;

/// `FixedUpdate`: the quad is gameplay, so it moves with the simulation clock.
fn slide(time: Res<Time>, mut query: Query<(&mut Transform, &mut GlobalTransform), With<Slider>>) {
    let x = -1.2 + SPEED * time.elapsed_secs();
    for (mut transform, mut global) in &mut query {
        transform.translation.x = x;
        global.0 = transform.compute_affine();
    }
}

/// One app frame whose render clock moved, as `Update` saw it.
#[derive(Clone, Copy, Debug)]
struct Row {
    /// `Time::delta`, ns (the render clock).
    delta: u64,
    /// `Time::elapsed`, ns.
    elapsed: u64,
    /// `Time::sim_delta`, ns (the simulation clock).
    sim_delta: u64,
    /// `Time::sim_elapsed`, ns.
    sim_elapsed: u64,
    /// The replay's tick and fraction when a replay is loaded.
    replay: Option<(u32, f64)>,
}

/// The rows of every app frame whose render clock moved.
#[derive(Resource, Clone, Default)]
struct ClockLog(Arc<Mutex<Vec<Row>>>);

fn record(time: Res<Time>, log: Res<ClockLog>, replay: Option<Res<ReplaySim>>) {
    if time.delta().is_zero() {
        return;
    }
    let replay = replay
        .filter(|sim| sim.file().is_some())
        .map(|sim| (sim.tick(), sim.tick_fraction()));
    log.0.lock().expect("log").push(Row {
        delta: time.delta().as_nanos() as u64,
        elapsed: time.elapsed().as_nanos() as u64,
        sim_delta: time.sim_delta().as_nanos() as u64,
        sim_elapsed: time.sim_elapsed().as_nanos() as u64,
        replay,
    });
}

fn vertex(pos: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, normal, [0.0, 0.0])
}

fn quad() -> Arc<CpuMesh> {
    let n = [0.0, 0.0, 1.0];
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-0.3, -0.4, 0.0], n),
            vertex([0.3, -0.4, 0.0], n),
            vertex([0.3, 0.4, 0.0], n),
            vertex([-0.3, 0.4, 0.0], n),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::Fxaa;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
    settings
}

fn call(client: &mut Client, method: &str, mut params: Value) -> Value {
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn read_token(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_string()
}

/// Starts a job of `frames` frames named `name` with `extra` parameters, waits for it to end and
/// returns its progress (with hashes) and its manifest.
fn render(client: &mut Client, out: &Path, name: &str, frames: u64, extra: Value) -> (Value, Value) {
    let mut params = json!({
        "out": out.join(name).display().to_string(), "name": name, "frames": frames,
        "width": WIDTH, "height": HEIGHT, "hud": false,
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let started = call(client, "render.out_start", params);
    let job = started["job"].as_u64().expect("job id");
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(
            client,
            "render.out_progress",
            json!({ "job": job, "hashes": true }),
        );
        if progress["status"] != json!("running") && progress["status"] != json!("cancelling") {
            assert_eq!(progress["status"], json!("done"), "{name}: {progress}");
            let manifest: Value = serde_json::from_slice(
                &std::fs::read(out.join(name).join(format!("{name}_render_out.json"))).expect("manifest"),
            )
            .expect("json");
            return (progress, manifest);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

fn hashes(progress: &Value) -> Vec<String> {
    progress["hashes"]["hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|h| h.as_str().expect("hex").to_string())
        .collect()
}

/// One column of the manifest's `frame_times` table.
fn column(manifest: &Value, key: &str) -> Vec<u64> {
    manifest["frame_times"]
        .as_array()
        .expect("frame_times")
        .iter()
        .map(|row| row[key].as_u64().expect("u64"))
        .collect()
}

/// A replay of `ticks` ticks at 60 Hz with no fulls: enough for the clock.
fn write_replay(project: &Path, ticks: u32) -> PathBuf {
    let header = ReplayHeader {
        version: 3,
        tick_rate: 60,
        ..ReplayHeader::default()
    };
    let mut writer = SyntheticReplay::new(&header);
    for tick in 0..=ticks {
        writer = writer.inputs(tick, &[]);
    }
    std::fs::create_dir_all(project).expect("project dir");
    let path = project.join("clock.do27replay");
    let staging = path.with_extension("tmp");
    std::fs::write(&staging, writer.finish()).expect("write replay");
    std::fs::rename(&staging, &path).expect("rename replay");
    path
}

/// A fresh headless app with a sliding quad and a fixed 50 Hz loop; runs `script` against its
/// agent and returns the project dir (removed by the caller) and the clock log.
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path, &Path) + Send + 'static) -> (PathBuf, Vec<Row>) {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-time-map-{tag}"));
    let out = project.join("out");
    let log = ClockLog::default();

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    // 20 ms fixed steps: one per frame of both the 50 fps and the half-speed 25 fps job.
    enable_fixed_step(&mut app, 50.0);
    // Paused until a job takes the clock, so nothing depends on the client's speed.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.insert_resource(log.clone());
    app.add_systems(FixedUpdate, slide);
    app.add_systems(Update, record);

    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            fov_y: 0.9,
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    let material = Arc::new(CpuMaterial {
        base_color: [0.85, 0.42, 0.12, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..CpuMaterial::default()
    });
    let mesh_tf = Transform::default();
    app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        MeshRenderer::new(quad(), material),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
        Slider,
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_project = project.clone();
    let thread = std::thread::Builder::new()
        .name(format!("render-out-time-map-{tag}"))
        .spawn(move || {
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let mut client = None;
            while Instant::now() < deadline {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&agent_dir))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            script(&mut client, &out, &script_project);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let rows = log.0.lock().expect("log").clone();
    (project, rows)
}

/// The documented frame step: `round(step · s(j + ½))` with the scale in millionths, for a
/// linear ramp from `from` (at frame `a`) to `to` (at frame `b`).
fn linear_ramp_step(step: u64, j: u64, (a, from): (u64, f64), (b, to): (u64, f64)) -> u64 {
    let x = j as f64 + 0.5;
    let scale = if x < a as f64 {
        from
    } else if x > b as f64 {
        to
    } else {
        from + (to - from) * (x - a as f64) / (b - a) as f64
    };
    let ppm = (scale * 1e6).round() as u64;
    (step * ppm + 500_000) / 1_000_000
}

#[test]
fn half_speed_at_25_fps_renders_the_frames_of_50_fps() {
    let frames = 8;
    let (slow_project, slow_log) = run("slow", move |client, out, _| {
        let (progress, manifest) = render(
            client,
            out,
            "slow",
            frames,
            json!({ "fps": 25, "time_map": [{ "frame": 0, "scale": 0.5 }] }),
        );
        let slow = hashes(&progress);
        assert_eq!(
            manifest["request"]["time_map"],
            json!([{ "frame": 0, "scale": 0.5, "ease": "smooth" }])
        );
        assert_eq!(manifest["schema"], "regia.render_out/2");
        assert_eq!(manifest["files"]["audio"], json!([]));
        assert_eq!(manifest["files"]["aov"], json!([]));
        assert_eq!(
            manifest["files"]["frames"].as_array().map(Vec::len),
            Some(frames as usize)
        );
        let sim = column(&manifest, "sim_time_ns");
        assert_eq!(sim, (0..frames).map(|i| i * 20_000_000).collect::<Vec<_>>());
        let render = column(&manifest, "game_time_ns");
        assert_eq!(render, (0..frames).map(|i| i * 40_000_000).collect::<Vec<_>>());
        assert_eq!(
            progress["sim_time_s"],
            json!(0.16),
            "8 frames of 20 ms simulation time"
        );
        assert_eq!(
            progress["game_time_s"],
            json!(0.32),
            "the render clock: 8 frames at 25 fps"
        );
        std::fs::write(out.join("slow.hashes"), slow.join("\n")).expect("write hashes");
    });
    let (fast_project, fast_log) = run("fast", move |client, out, _| {
        let (progress, manifest) = render(client, out, "fast", frames, json!({ "fps": 50 }));
        assert_eq!(manifest["request"]["time_map"], json!([]));
        assert_eq!(progress["game_time_s"], json!(0.16));
        assert_eq!(progress["sim_time_s"], json!(0.16), "no map: one clock");
        std::fs::write(out.join("fast.hashes"), hashes(&progress).join("\n")).expect("write hashes");
    });
    let slow = std::fs::read_to_string(slow_project.join("out/slow.hashes")).expect("slow hashes");
    let fast = std::fs::read_to_string(fast_project.join("out/fast.hashes")).expect("fast hashes");
    assert_eq!(slow, fast, "the same simulation times give the same frames");
    let slow_hashes: Vec<&str> = slow.lines().collect();
    assert!(
        slow_hashes.windows(2).all(|w| w[0] != w[1]),
        "the quad moves every frame: {slow_hashes:?}"
    );
    // Both apps saw the same simulation deltas, 20 ms per frame; `Update` (VFX, HUD, shots) saw
    // the 25 fps render step in the slow job.
    let sim = |log: &[Row]| log.iter().map(|r| r.sim_delta).collect::<Vec<_>>();
    assert_eq!(sim(&slow_log[..]), sim(&fast_log[..]));
    assert!(
        sim(&slow_log[..]).iter().all(|d| *d == 20_000_000),
        "{:?}",
        sim(&slow_log[..])
    );
    assert!(slow_log.iter().all(|r| r.delta == 40_000_000), "{slow_log:?}");
    assert!(fast_log.iter().all(|r| r.delta == 20_000_000), "{fast_log:?}");
    let _ = std::fs::remove_dir_all(&slow_project);
    let _ = std::fs::remove_dir_all(&fast_project);
}

#[test]
fn a_ramp_moves_the_simulation_clock_by_the_documented_steps_and_not_the_render_clock() {
    let frames = 12;
    let ramp = json!([{ "frame": 2, "scale": 1 }, { "frame": 8, "scale": 0.25, "ease": "linear" }]);
    let manifests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let seen = manifests.clone();
    let (project, log) = run("ramp", move |client, out, _| {
        let (_, plain) = render(
            client,
            out,
            "ramp",
            frames,
            json!({ "fps": 30, "time_map": ramp.clone() }),
        );
        let (_, blurred) = render(
            client,
            out,
            "ramp_blur",
            frames,
            json!({ "fps": 30, "time_map": ramp, "subframes": 3 }),
        );
        seen.lock().expect("manifests").extend([plain, blurred]);
    });
    let manifests = manifests.lock().expect("manifests").clone();
    let step = 33_333_333;
    let mut expected = Vec::new();
    let mut game = 0;
    for j in 0..frames {
        expected.push(game);
        game += linear_ramp_step(step, j, (2, 1.0), (8, 0.25));
    }
    let render: Vec<u64> = (0..frames).map(|i| i * step).collect();
    for manifest in &manifests {
        assert_eq!(
            column(manifest, "sim_time_ns"),
            expected,
            "{}",
            manifest["request"]
        );
        assert_eq!(
            column(manifest, "game_time_ns"),
            render,
            "{}",
            manifest["request"]
        );
        let end = manifest["sim_time_s"].as_f64().expect("sim time");
        assert!((end - game as f64 / 1e9).abs() < 1e-9, "{end}");
        let end = manifest["game_time_s"].as_f64().expect("game time");
        assert!((end - (frames * step) as f64 / 1e9).abs() < 1e-9, "{end}");
    }
    // The first job's rows are its frames: the simulation time each one rendered at is the
    // manifest's, and the render time is the output's.
    let first = &log[..frames as usize];
    let (sim0, render0) = (first[0].sim_elapsed, first[0].elapsed);
    assert_eq!(
        first.iter().map(|r| r.sim_elapsed - sim0).collect::<Vec<_>>(),
        expected,
        "Time::sim_elapsed of each rendered frame"
    );
    assert_eq!(
        first.iter().map(|r| r.elapsed - render0).collect::<Vec<_>>(),
        render,
        "Time::elapsed of each rendered frame"
    );
    assert_eq!(
        (log[0].delta, log[0].sim_delta),
        (step, step),
        "the step into frame 0 is the scale before the first key"
    );
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn an_armed_replay_starts_on_frame_zero_and_follows_the_map() {
    let frames = 10;
    let from = 300;
    let (project, log) = run("replay", move |client, out, project| {
        let path = write_replay(project, 2_000);
        call(
            client,
            "replay.load",
            json!({ "path": path.display().to_string() }),
        );
        let armed = call(
            client,
            "replay.play",
            json!({ "from_tick": from, "speed": 2, "start": "render_out" }),
        );
        assert_eq!(armed["armed_for"], "next_job");
        assert_eq!(armed["driver"], "agent");
        let (progress, manifest) = render(
            client,
            out,
            "replay",
            frames,
            json!({ "fps": 30, "warmup_frames": 3, "time_map": [{ "frame": 0, "scale": 0.5 }] }),
        );
        assert_eq!(progress["replay_from_tick"], from);
        assert_eq!(manifest["replay_from_tick"], from);
        call(client, "replay.pause", json!({}));
    });
    // The warm-up frames held the replay at its tick; from frame 0 on it plays two ticks per
    // frame: 30 fps at half speed is 1/60 s of simulation time per frame, 60 ticks/s, at 2x.
    let rows: Vec<(u32, f64)> = log.iter().filter_map(|r| r.replay).collect();
    let frame_rows = &rows[rows.len() - frames as usize..];
    for warm in &rows[..rows.len() - frames as usize] {
        assert_eq!(warm.0, from, "the replay waits during warm-up: {warm:?}");
    }
    for (i, (tick, fraction)) in frame_rows.iter().enumerate() {
        assert_eq!(*tick, from + 2 * i as u32, "frame {i}");
        assert!(*fraction < 1e-6, "frame {i}: fraction {fraction}");
    }
    let _ = std::fs::remove_dir_all(&project);
}
