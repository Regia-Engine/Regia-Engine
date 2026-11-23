// SPDX-License-Identifier: MPL-2.0
//! Render jobs with audio (ADR-1616, ADR-1520), on lavapipe with the headless mixer: a job writes
//! one WAV of its whole timeline into its `out` (every frame is mixed, pre-roll included), each
//! take names its exact sample range in it, and a lone late take gives the same WAV bytes as a job
//! over the whole timeline, because the pre-roll is mixed as a full render mixes it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::audio::{Audio, AudioPlugin, AudioSettings, PlaybackParams, SoundAsset, SoundClip};
use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, PostSettings,
    RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
/// 48 kHz at 30 fps.
const PER_FRAME: u64 = 1_600;

/// Plays a flat 0.1 s burst on `Sfx` once, the first frame the game clock reaches 0.1 s: inside
/// the first take's frames and before the lone take starts, so the lone job mixes it in its
/// pre-roll.
fn ping(time: Res<Time>, mut audio: ResMut<Audio>, mut fired: Local<bool>) {
    if *fired || time.elapsed_secs() < 0.1 {
        return;
    }
    *fired = true;
    let clip = SoundClip::new(vec![0.5; 4_800], 48_000).expect("a valid clip");
    let params = PlaybackParams {
        bus: Some("Sfx".into()),
        ..PlaybackParams::default()
    };
    if let Err(error) = audio.play_asset(&SoundAsset::from_clip(&clip), params) {
        panic!("play: {error}");
    }
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

/// The job: 32x24 at 30 fps, a PCM16 mixdown, the given takes.
fn job_text(takes: &str) -> String {
    format!(
        "(version: 1, name: \"mix\", out: \"out\", output: (width: Some(32), height: Some(24), fps: Some(30), \
         hud: Some(false), audio: Some((format: Some(\"pcm16\")))), takes: [{takes}])"
    )
}

/// What a job left: the manifest and the timeline WAV's bytes.
struct Outcome {
    manifest: Value,
    wav: Vec<u8>,
}

fn run(tag: &str, takes: &'static str) -> Outcome {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-job-audio-{tag}"));
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        AudioPlugin,
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_systems(Update, ping.before(regia::audio::drain_one_shots));
    enable_fixed_step(&mut app, 60.0);
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    app.insert_resource(settings);
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));

    // Something to draw: an offscreen capture of an empty frame is refused.
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
    let slot = Arc::new(std::sync::Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_project: PathBuf = project.clone();
    let thread = std::thread::Builder::new()
        .name(format!("render-out-job-audio-{tag}"))
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
            let started = call(&mut client, "render.out_job", json!({ "job": job_text(takes) }));
            assert!(
                started["audio"].is_object(),
                "the answer names the audio: {started}"
            );
            let job = started["job"].as_u64().expect("job id");
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let finished = loop {
                let progress = call(&mut client, "render.out_progress", json!({ "job": job }));
                if progress["status"] != json!("running") {
                    break progress;
                }
                assert!(Instant::now() < deadline, "the job did not finish");
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(finished["status"], json!("done"), "{finished}");
            let manifest: Value = serde_json::from_slice(
                &std::fs::read(script_project.join(finished["manifest"].as_str().expect("manifest")))
                    .expect("manifest file"),
            )
            .expect("manifest json");
            let wav =
                std::fs::read(script_project.join("out").join("mix_audio.wav")).expect("the timeline WAV");
            *result.lock().expect("slot") = Some(Outcome { manifest, wav });
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let outcome = slot.lock().expect("slot").take().expect("the script ran");
    let _ = std::fs::remove_dir_all(&project);
    outcome
}

#[test]
fn a_job_mixes_its_whole_timeline_and_each_take_names_its_samples() {
    let both = run(
        "both",
        "(name: \"early\", range: Frames(start: 0, end: 6)), (name: \"late\", range: Frames(start: 6, end: 12))",
    );
    let lone = run("lone", "(name: \"late\", range: Frames(start: 6, end: 12))");

    let takes = both.manifest["job"]["takes"].as_array().expect("takes");
    assert_eq!(
        takes[0]["audio_samples"],
        json!({ "start": 0, "end": 6 * PER_FRAME })
    );
    assert_eq!(
        takes[1]["audio_samples"],
        json!({ "start": 6 * PER_FRAME, "end": 12 * PER_FRAME })
    );
    assert_eq!(
        lone.manifest["job"]["takes"][0]["audio_samples"],
        json!({ "start": 6 * PER_FRAME, "end": 12 * PER_FRAME }),
        "a lone take names the same range: the WAV is the timeline's"
    );
    for outcome in [&both, &lone] {
        assert_eq!(
            outcome.manifest["audio"]["samples"],
            json!(12 * PER_FRAME),
            "every timeline frame is mixed"
        );
        // 44-byte header, stereo PCM16.
        assert_eq!(outcome.wav.len() as u64, 44 + 12 * PER_FRAME * 4);
    }
    assert!(both.wav[44..].iter().any(|b| *b != 0), "the burst is in the mix");
    assert_eq!(
        both.wav, lone.wav,
        "the pre-roll is mixed as a full render mixes it"
    );
}
