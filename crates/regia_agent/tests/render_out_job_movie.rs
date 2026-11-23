// SPDX-License-Identifier: MPL-2.0
//! Render jobs with a movie format (ADR-1616): each take pipes its own frames to its own ffmpeg
//! process, opened on the take's first frame and finalized after its last, so two takes give two
//! movies. Without ffmpeg on `PATH` the job is refused before anything renders (`F0019`), as one
//! `render.out_start` movie is.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, Environment, LabVertex, MeshRenderer,
    PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(180);
const CALL_TIMEOUT_MS: u64 = 60_000;

const JOB: &str = r#"(version: 1, name: "movie", out: "out",
    output: (width: Some(64), height: Some(48), fps: Some(20), hud: Some(false), format: Some("ffv1")),
    takes: [(name: "a", range: Frames(start: 0, end: 3)), (name: "b", range: Frames(start: 3, end: 6))])"#;

fn ffmpeg_on_path() -> bool {
    let Some(path) = regia_core::brand::env_var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("ffmpeg").is_file() || dir.join("ffmpeg.exe").is_file())
}

fn call(client: &mut Client, method: &str, mut params: Value) -> Result<Value, String> {
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    client.call(method, params).map_err(|err| format!("{err:?}"))
}

fn read_token(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_string()
}

fn scenario(client: &mut Client, project: &Path) {
    let dry = call(client, "render.out_job", json!({ "job": JOB, "dry_run": true })).expect("dry run");
    assert_eq!(dry["plan"]["frames"], json!(6), "{dry}");
    let started = call(client, "render.out_job", json!({ "job": JOB }));
    if !ffmpeg_on_path() {
        let error = started.expect_err("no ffmpeg");
        assert!(error.contains("F0019"), "{error}");
        assert!(!project.join("out").exists(), "nothing was created");
        return;
    }
    let job = started.expect("started")["job"].as_u64().expect("job id");
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let finished = loop {
        let progress = call(client, "render.out_progress", json!({ "job": job })).expect("progress");
        if progress["status"] != json!("running") {
            break progress;
        }
        assert!(Instant::now() < deadline, "the job did not finish");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(finished["status"], json!("done"), "{finished}");
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(project.join(finished["manifest"].as_str().expect("manifest")))
            .expect("manifest file"),
    )
    .expect("manifest json");
    let takes = manifest["job"]["takes"].as_array().expect("takes");
    assert_eq!(takes.len(), 2);
    for (take, name) in takes.iter().zip(["a", "b"]) {
        assert_eq!(take["status"], json!("done"), "{take}");
        assert_eq!(take["written"], json!(3), "{take}");
        let files = take["files"].as_array().expect("files");
        assert_eq!(files.len(), 1, "one movie per take: {take}");
        let movie = files[0].as_str().expect("movie path");
        assert!(movie.ends_with(&format!("out/{name}/{name}.mkv")), "{movie}");
        let size = std::fs::metadata(project.join(movie)).expect("movie file").len();
        assert!(size > 0, "{movie} is empty");
        assert_eq!(take["frame_hashes"].as_array().map(Vec::len), Some(3));
    }
}

#[test]
fn each_movie_take_gets_its_own_movie_and_no_ffmpeg_is_refused_up_front() {
    let project = regia_test_utils::unique_temp_dir("render-out-job-movie");
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    app.insert_resource(settings);
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
    let mesh = Transform::default();
    app.world_mut().spawn((
        mesh,
        GlobalTransform(mesh.compute_affine()),
        MeshRenderer::new(
            Arc::new(CpuMesh {
                vertices: vec![
                    LabVertex::from_pos_normal_uv([-0.9, -0.7, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
                    LabVertex::from_pos_normal_uv([0.9, -0.7, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
                    LabVertex::from_pos_normal_uv([0.0, 0.9, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
                ],
                indices: vec![0, 1, 2],
            }),
            Arc::new(CpuMaterial {
                base_color: [0.8, 0.4, 0.1, 1.0],
                ..CpuMaterial::default()
            }),
        ),
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_project = project.clone();
    let script = std::thread::Builder::new()
        .name("render-out-job-movie".into())
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
            scenario(&mut client, &script_project);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(ok.load(Ordering::Relaxed), "the scenario ran to the end");
    let _ = std::fs::remove_dir_all(&project);
}
