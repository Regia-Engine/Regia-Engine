// SPDX-License-Identifier: MPL-2.0
//! Render-out settle frames (ADR-1449), on lavapipe with TAA on: the last `settle_frames` warm-up
//! frames render at the delivered size after the job's camera cut, so frame 0 carries their
//! temporal history. Frame 0 of such a job is byte for byte the frame a job without settle frames
//! delivers that many frames after its cut, and it differs from the single-sample frame 0 a job
//! without them delivers (a one-frame still was one jittered TAA sample).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
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
/// Settle frames of job A, and the frame of job B that matches its frame 0.
const SETTLE: u64 = 4;

fn vertex(pos: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, normal, [0.0, 0.0])
}

/// A triangle with long slanted edges: TAA's jittered history antialiases them, a single sample
/// does not.
fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-1.3, -0.8, 0.0], [0.0, 0.0, 1.0]),
            vertex([1.1, -0.95, 0.0], [0.0, 0.0, 1.0]),
            vertex([0.15, 0.9, 0.0], [0.0, 0.0, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn taa_settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::Taa;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
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

/// Polls `render.out_progress` until the job is no longer running or cancelling.
fn wait(client: &mut Client, job: u64) -> Value {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(
            client,
            "render.out_progress",
            json!({ "job": job, "hashes": true }),
        );
        if progress["status"] != json!("running") && progress["status"] != json!("cancelling") {
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

/// Starts a job of `frames` frames with `extra` parameters and waits for it to end.
fn render(client: &mut Client, out: &Path, name: &str, frames: u64, extra: Value) -> Value {
    let mut params = json!({
        "out": out.join(name).display().to_string(), "name": name, "frames": frames,
        "width": WIDTH, "height": HEIGHT, "fps": 30, "hud": false,
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let started = call(client, "render.out_start", params);
    let settle = extra.get("settle_frames").cloned().unwrap_or(json!(0));
    assert_eq!(
        started["request"]["settle_frames"], settle,
        "{name}: the request echo carries settle_frames"
    );
    let done = wait(client, started["job"].as_u64().expect("job id"));
    assert_eq!(done["status"], json!("done"), "{name}: {done}");
    done
}

fn hashes(progress: &Value) -> Vec<String> {
    progress["hashes"]["hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|h| h.as_str().expect("hex").to_string())
        .collect()
}

/// A fresh headless app showing a still lit triangle with TAA on; runs `script` against its agent
/// with the output root and returns the project dir (removed by the caller).
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-settle-{tag}"));
    let out = project.join("out");

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    enable_fixed_step(&mut app, 60.0);
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(taa_settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
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
        MeshRenderer::new(triangle(), material),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 60_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let thread = std::thread::Builder::new()
        .name(format!("render-out-settle-{tag}"))
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
            script(&mut client, &out);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    project
}

#[test]
fn settle_frames_give_frame_zero_the_history_of_that_many_delivered_frames() {
    let project = run("history", |client, out| {
        // A: 2 small warm-up frames, the cut, SETTLE full-size frames, then frame 0.
        let a = render(
            client,
            out,
            "settled",
            1,
            json!({ "warmup_frames": 2 + SETTLE, "settle_frames": SETTLE }),
        );
        // B: 2 small warm-up frames, then frame 0 cuts; frame SETTLE is the (SETTLE + 1)-th
        // full-size render after the cut, as A's frame 0 is.
        let b = render(client, out, "sequence", SETTLE + 1, json!({ "warmup_frames": 2 }));
        // C: what a one-frame still was: frame 0 is the first render after the cut.
        let c = render(
            client,
            out,
            "unsettled",
            1,
            json!({ "warmup_frames": 2 + SETTLE, "settle_frames": 0 }),
        );
        let (a, b, c) = (hashes(&a), hashes(&b), hashes(&c));
        assert_eq!((a.len(), b.len(), c.len()), (1, SETTLE as usize + 1, 1));
        assert_eq!(
            a[0], b[SETTLE as usize],
            "a settled frame 0 must be the frame delivered {SETTLE} frames after a cut"
        );
        assert_eq!(
            c[0], b[0],
            "an unsettled frame 0 is the first frame after the cut"
        );
        assert_ne!(
            a[0], c[0],
            "TAA history must change frame 0 (the settle frames were not kept)"
        );
        // One camera cut per job: the opening one, at the first settle frame for A.
        for name in ["settled", "sequence", "unsettled"] {
            let manifest: Value = serde_json::from_slice(
                &std::fs::read(out.join(name).join(format!("{name}_render_out.json"))).expect("manifest"),
            )
            .expect("json");
            assert_eq!(manifest["camera_cuts"], json!(1), "{name}: {manifest}");
        }
    });
    let _ = std::fs::remove_dir_all(&project);
}
