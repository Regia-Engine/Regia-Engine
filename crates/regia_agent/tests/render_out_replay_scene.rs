// SPDX-License-Identifier: MPL-2.0
//! The tiny replay scene Red-Rouge starts from (ADR-1559), on lavapipe: a two-second replay in the
//! engine's replay format (60 ticks per second, no game data) in a sample project folder
//! (`tests/fixtures/replay_scene/`). The test copies the folder to a temp project, then does
//! exactly the steps of `book/agents/replay-scene.md`: `replay.load`, `replay.play` with
//! `start: "render_out"`, `render.out_start` with a relative `out`, and checks that the replay
//! is at `from_tick` on frame 0 and one tick further on every later frame, and that the frames
//! land under the project directory. Two jobs that arm the same tick in one app render the same
//! frames: the replay is where a job's world starts from (ADR-1881).
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
/// The last tick of the fixture replay: 120 ticks at 60 Hz is two seconds.
const END_TICK: u32 = 120;

/// The replay's tick and fraction on every app frame whose render clock moved.
#[derive(Resource, Clone, Default)]
struct TickLog(Arc<Mutex<Vec<(u32, f64)>>>);

fn record(time: Res<Time>, log: Res<TickLog>, replay: Option<Res<ReplaySim>>) {
    if time.delta().is_zero() {
        return;
    }
    if let Some(sim) = replay.filter(|sim| sim.file().is_some()) {
        log.0.lock().expect("log").push((sim.tick(), sim.tick_fraction()));
    }
}

fn quad() -> Arc<CpuMesh> {
    let vertex = |pos: [f32; 3]| LabVertex::from_pos_normal_uv(pos, [0.0, 0.0, 1.0], [0.0, 0.0]);
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-0.3, -0.4, 0.0]),
            vertex([0.3, -0.4, 0.0]),
            vertex([0.3, 0.4, 0.0]),
            vertex([-0.3, 0.4, 0.0]),
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

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay_scene")
}

/// The bytes `SyntheticReplay` writes for the fixture: version 3, 60 Hz, inputs for ticks 0..=120.
fn expected_replay() -> Vec<u8> {
    let header = ReplayHeader {
        version: 3,
        tick_rate: 60,
        ..ReplayHeader::default()
    };
    let mut writer = SyntheticReplay::new(&header);
    for tick in 0..=END_TICK {
        writer = writer.inputs(tick, &[]);
    }
    writer.finish()
}

/// A headless app with a camera, a lit quad and the tick log, its clock paused until a job takes it.
fn scene_app(project: &Path, log: &TickLog) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.to_path_buf()),
    });
    enable_fixed_step(&mut app, 50.0);
    // Paused until the job takes the clock, so nothing depends on the client's speed.
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
    ));
    app
}

#[test]
fn the_committed_replay_is_the_documented_two_second_scene() {
    let committed = std::fs::read(fixture_dir().join("replays/tiny.do27replay")).expect("fixture replay");
    assert_eq!(
        committed,
        expected_replay(),
        "tests/fixtures/replay_scene/replays/tiny.do27replay must equal expected_replay(); regenerate it from SyntheticReplay"
    );
}

#[test]
fn the_sample_project_plays_a_replay_into_a_render_out() {
    let project = regia_test_utils::unique_temp_dir("render-out-replay-scene");
    let replays = project.join("replays");
    std::fs::create_dir_all(&replays).expect("project dir");
    let replay_path = replays.join("tiny.do27replay");
    let staging = replays.join("tiny.tmp");
    std::fs::copy(fixture_dir().join("replays/tiny.do27replay"), &staging).expect("copy fixture");
    std::fs::rename(&staging, &replay_path).expect("rename fixture");

    let log = TickLog::default();
    let mut app = scene_app(&project, &log);

    let frames: u64 = 10;
    let from: u32 = 30;
    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_replay = replay_path.clone();
    let thread = std::thread::Builder::new()
        .name("render-out-replay-scene".into())
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
            // Step 1: replay.load with an absolute path under an allowed root (the project, ADR-1965).
            let loaded = call(
                &mut client,
                "replay.load",
                json!({ "path": script_replay.display().to_string() }),
            );
            assert_eq!(loaded["end_tick"], json!(END_TICK));
            assert_eq!(loaded["header"]["tick_rate"], json!(60));
            // Step 2: arm the replay for the next render-out's frame 0.
            let armed = call(
                &mut client,
                "replay.play",
                json!({ "from_tick": from, "start": "render_out" }),
            );
            assert_eq!(armed["armed_for"], "next_job");
            assert_eq!(armed["position"]["playing"], json!(false));
            // Step 3: start the job; `out` is relative, so it lands in the project directory.
            let started = call(
                &mut client,
                "render.out_start",
                json!({
                    "out": "renders/take1", "name": "take1", "frames": frames, "fps": 60,
                    "width": WIDTH, "height": HEIGHT, "hud": false, "warmup_frames": 2,
                }),
            );
            let job = started["job"].as_u64().expect("job id");
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            loop {
                assert!(Instant::now() < deadline, "render-out job {job} did not finish");
                let progress = call(&mut client, "render.out_progress", json!({ "job": job }));
                if progress["status"] != json!("running") && progress["status"] != json!("cancelling") {
                    assert_eq!(progress["status"], json!("done"), "{progress}");
                    assert_eq!(progress["replay_from_tick"], json!(from));
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            call(&mut client, "replay.pause", json!({}));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");

    // The frames and the manifest are under the project directory, not the process's cwd.
    let out = project.join("renders/take1");
    assert!(
        out.join("take1_render_out.json").is_file(),
        "manifest in {}",
        out.display()
    );
    assert!(out.join("take1_000000.png").is_file());
    assert!(out.join(format!("take1_{:06}.png", frames - 1)).is_file());
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("take1_render_out.json")).expect("manifest"))
            .expect("manifest json");
    assert_eq!(manifest["replay_from_tick"], json!(from));

    // Warm-up frames hold the replay at `from`; then one tick per frame (60 ticks/s at 60 fps).
    let rows = log.0.lock().expect("log").clone();
    let frame_rows = &rows[rows.len() - frames as usize..];
    for warm in &rows[..rows.len() - frames as usize] {
        assert_eq!(warm.0, from, "the replay waits during warm-up: {warm:?}");
    }
    for (i, (tick, fraction)) in frame_rows.iter().enumerate() {
        assert_eq!(*tick, from + i as u32, "frame {i}");
        assert!(*fraction < 1e-6, "frame {i}: fraction {fraction}");
    }
    let _ = std::fs::remove_dir_all(&project);
}

/// Turns the quad with the replay, the way a game's world follows it.
fn pose_from_replay(
    replay: Option<Res<ReplaySim>>,
    mut query: Query<(&mut Transform, &mut GlobalTransform)>,
) {
    let Some(sim) = replay.filter(|sim| sim.file().is_some()) else {
        return;
    };
    let turn = (f64::from(sim.tick()) + sim.tick_fraction()) as f32 * 0.03;
    for (mut transform, mut global) in &mut query {
        if transform.translation.z == 0.0 {
            transform.rotation = regia_core::math::Quat::from_rotation_y(turn);
            global.0 = transform.compute_affine();
        }
    }
}

#[test]
fn two_jobs_armed_at_one_tick_render_the_same_frames_in_one_app() {
    // Red-Rouge renders one job per shot in one host, putting the replay at the shot's start first.
    let project = regia_test_utils::unique_temp_dir("render-out-replay-repeat");
    let replays = project.join("replays");
    std::fs::create_dir_all(&replays).expect("project dir");
    let replay_path = replays.join("tiny.do27replay");
    std::fs::copy(fixture_dir().join("replays/tiny.do27replay"), &replay_path).expect("copy fixture");

    let log = TickLog::default();
    let mut app = scene_app(&project, &log);
    app.add_systems(Update, pose_from_replay.before(record));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let renders = project.join("renders");
    let thread = std::thread::Builder::new()
        .name("render-out-replay-repeat".into())
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
            call(
                &mut client,
                "replay.load",
                json!({ "path": replay_path.display().to_string() }),
            );
            let mut take = |name: &str| -> Value {
                call(
                    &mut client,
                    "replay.play",
                    json!({ "from_tick": 30, "start": "render_out" }),
                );
                let started = call(
                    &mut client,
                    "render.out_start",
                    json!({
                        "out": format!("renders/{name}"), "name": name, "frames": 8, "fps": 24,
                        "width": WIDTH, "height": HEIGHT, "hud": false, "warmup_frames": 2,
                    }),
                );
                let job = started["job"].as_u64().expect("job id");
                let deadline = Instant::now() + SCRIPT_TIMEOUT;
                loop {
                    assert!(Instant::now() < deadline, "render-out job {job} did not finish");
                    let progress = call(&mut client, "render.out_progress", json!({ "job": job }));
                    if progress["status"] != json!("running") {
                        assert_eq!(progress["status"], json!("done"), "{progress}");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                call(&mut client, "replay.pause", json!({}));
                let manifest = renders.join(name).join(format!("{name}_render_out.json"));
                serde_json::from_slice(&std::fs::read(manifest).expect("manifest")).expect("manifest json")
            };
            let first = take("a");
            let second = take("b");
            let hashes = first["frame_hashes"].as_array().expect("hashes");
            assert_ne!(hashes.first(), hashes.last(), "the replay moved the quad");
            assert_ne!(first["clock"], second["clock"], "the app clock moved on");
            assert_eq!(first["frame_hashes"], second["frame_hashes"]);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let _ = std::fs::remove_dir_all(&project);
}
