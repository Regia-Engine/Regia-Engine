// SPDX-License-Identifier: MPL-2.0
//! Render-out cuts with auto exposure on (ADR-1455), on lavapipe: a job cut meters its own frame
//! before it is delivered, so frame 0 of a one-frame still and the frame of every `cuts` entry are
//! exposed like the frames after them, not at the meter's initial EV (the top of the auto-exposure
//! band, EV100 15, about 4 EV under this scene's metered exposure). The metered cut is one job cut
//! in the manifest and in `render.out_progress`, and the same job gives the same hashes again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_render::{
    AntiAliasing, Camera, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex, MeshRenderer,
    PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// Largest exposure step (stops of display-linear mean) a metered cut may leave between frames
/// of one still view; before ADR-1455 the frames of a cut printed about 4 stops darker.
const MAX_STEP_EV: f64 = 0.25;

fn vertex(pos: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, [0.0, 0.0, 1.0], [0.0, 0.0])
}

/// A quad that fills the view: the whole frame is one sunlit diffuse surface.
fn wall() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-6.0, -6.0, 0.0]),
            vertex([6.0, -6.0, 0.0]),
            vertex([6.0, 6.0, 0.0]),
            vertex([-6.0, 6.0, 0.0]),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    })
}

/// Auto exposure on, everything else deterministic (no TAA, bloom, grain).
fn metered_settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings {
        auto_exposure: true,
        ..PostSettings::golden_deterministic()
    };
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = true;
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

/// Starts a raw-format job of `frames` frames with `extra` parameters and waits for it to end.
fn render(client: &mut Client, out: &Path, name: &str, frames: u64, extra: Value) -> Value {
    let mut params = json!({
        "out": out.join(name).display().to_string(), "name": name, "frames": frames,
        "width": WIDTH, "height": HEIGHT, "fps": 30, "hud": false, "format": "raw",
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let started = call(client, "render.out_start", params);
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

/// Display-linear mean luminance of frame `index` of job `name` (a raw RGBA8 sRGB file).
fn mean_luminance(out: &Path, name: &str, index: u64) -> f64 {
    let path = out.join(name).join(format!("{name}_{index:06}.raw"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(bytes.len(), (WIDTH * HEIGHT * 4) as usize, "{}", path.display());
    let decode = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let sum: f64 = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * decode(p[0]) + 0.7152 * decode(p[1]) + 0.0722 * decode(p[2]))
        .sum();
    sum / f64::from(WIDTH * HEIGHT)
}

/// Stops between two display-linear means.
fn stops(a: f64, b: f64) -> f64 {
    (a.max(1.0e-6) / b.max(1.0e-6)).log2().abs()
}

fn manifest(out: &Path, name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(out.join(name).join(format!("{name}_render_out.json"))).expect("manifest"),
    )
    .expect("json")
}

/// A fresh headless app showing a sunlit wall with auto exposure on; runs `script` against its
/// agent with the output root and returns the project dir (removed by the caller).
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-cut-exposure-{tag}"));
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
    app.insert_resource(metered_settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 200.0,
        double_sided: true,
    });
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        // No camera EV: the view meters its exposure.
        Camera {
            fov_y: 0.9,
            ..Camera::default()
        },
    ));
    let material = Arc::new(CpuMaterial {
        base_color: [0.5, 0.5, 0.5, 1.0],
        roughness: 1.0,
        metallic: 0.0,
        ..CpuMaterial::default()
    });
    let mesh_tf = Transform::default();
    app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        MeshRenderer::new(wall(), material),
        DirectionalLight {
            direction: [0.0, 0.0, -1.0],
            color: [1.0, 1.0, 1.0],
            illuminance: 2_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let thread = std::thread::Builder::new()
        .name(format!("render-out-cut-exposure-{tag}"))
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
fn a_cut_frame_is_exposed_by_its_own_meter() {
    let project = run("meter", |client, out| {
        // A: the opening cut at frame 0, then two frames of the same still view.
        let a = render(client, out, "still", 3, json!({ "warmup_frames": 2 }));
        let means: Vec<f64> = (0..3).map(|i| mean_luminance(out, "still", i)).collect();
        assert!(
            (0.03..0.6).contains(&means[2]),
            "the meter keys the wall near middle grey: {means:?}"
        );
        for (index, mean) in means.iter().enumerate().take(2) {
            assert!(
                stops(*mean, means[2]) < MAX_STEP_EV,
                "frame {index} must be exposed like frame 2 (the meter's own reading): {means:?}"
            );
        }
        // B: a `cuts` frame is metered too.
        let b = render(client, out, "cut", 4, json!({ "warmup_frames": 2, "cuts": [2] }));
        let cut: Vec<f64> = (0..4).map(|i| mean_luminance(out, "cut", i)).collect();
        assert!(
            stops(cut[2], cut[1]) < MAX_STEP_EV && stops(cut[3], cut[1]) < MAX_STEP_EV,
            "the cut frame keeps the view's exposure: {cut:?}"
        );
        // A metered cut is one job cut (two renderer cuts), reported as metered.
        for (name, progress, cuts) in [("still", &a, 1), ("cut", &b, 2)] {
            assert_eq!(progress["camera_cuts"], json!(cuts), "{name}: {progress}");
            assert_eq!(
                progress["exposure_metered_cuts"],
                json!(cuts),
                "{name}: {progress}"
            );
            let manifest = manifest(out, name);
            assert_eq!(manifest["camera_cuts"], json!(cuts), "{name}: {manifest}");
            assert_eq!(
                manifest["exposure_metered_cuts"],
                json!(cuts),
                "{name}: {manifest}"
            );
        }
        // The metering is deterministic: the same job again gives the same frames.
        let again = render(client, out, "still_again", 3, json!({ "warmup_frames": 2 }));
        assert_eq!(
            hashes(&a),
            hashes(&again),
            "the same job must give the same hashes"
        );
    });
    let _ = std::fs::remove_dir_all(&project);
}
