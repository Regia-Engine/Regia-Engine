// SPDX-License-Identifier: MPL-2.0
//! A render job's own sequence plays its sound tracks into the job's mixdown (ADR-1880), on
//! lavapipe with the headless mixer: an audio section of a sub-sequence the job read from its
//! project path sounds at its place on the job's timeline, silent before and after it, and two
//! runs write the same WAV bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::audio::{AudioPlugin, AudioSettings, SoundAsset, SoundAssets, SoundClip};
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
const PER_FRAME: usize = 1_600;
/// Frames of the job (0.5 s).
const FRAMES: usize = 15;

/// The job's sequence: the sub-sequence at 0.1 .. 0.3 s, and a keyed track so the table binds
/// something.
const MAIN: &str = r#"(version: 2, tracks: [
    (name: "fx", keys: Sub([(sequence: "cine/sound.seq.ron", start: 0.1, end: 0.3)])),
    (name: "hold", target: "mesh", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0))])),
])"#;

/// A sound right at the start of the timeline, in the sequence itself.
const AT_ZERO: &str = r#"(version: 2, tracks: [
    (name: "beep", keys: Audio([(sound: "beep", start: 0.0, end: 0.2)])),
    (name: "hold", target: "mesh", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0))])),
])"#;

/// The sub-sequence: the whole of its 0.2 s is one sound.
const SOUND: &str = r#"(version: 2, tracks: [
    (name: "beep", keys: Audio([(sound: "beep", start: 0.0, end: 0.2)])),
])"#;

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

const JOB: &str = "(version: 2, name: \"mix\", sequence: Some(\"cine/main.seq.ron\"), \
     bindings: {\"mesh\": Name(\"Mesh\")}, out: \"out\", \
     output: (width: Some(32), height: Some(24), fps: Some(30), hud: Some(false), \
     audio: Some((format: Some(\"pcm16\")))), \
     takes: [(name: \"all\", range: Frames(start: 0, end: 15))])";

fn run(tag: &str, main: &str) -> Vec<u8> {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-seq-audio-{tag}"));
    std::fs::create_dir_all(project.join("cine")).expect("cine dir");
    std::fs::write(project.join("cine/main.seq.ron"), main).expect("main");
    std::fs::write(project.join("cine/sound.seq.ron"), SOUND).expect("sub");
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
    });
    let clip = SoundClip::new(vec![0.5; 48_000], 48_000).expect("a valid clip");
    app.world_mut()
        .get_resource_or_insert_with(SoundAssets::default)
        .add("beep", SoundAsset::from_clip(&clip));
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

    // Something to draw, and the table's target.
    let mesh = Transform::default();
    app.world_mut().spawn((
        mesh,
        GlobalTransform(mesh.compute_affine()),
        regia_scene::components::Name::new("Mesh"),
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
        .name(format!("render-out-seq-audio-{tag}"))
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
            let started = call(&mut client, "render.out_job", json!({ "job": JOB }));
            assert_eq!(started["plan"]["sequence_players"], json!(1), "{started}");
            assert_eq!(started["plan"]["unbound_targets"], json!([]), "{started}");
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
            let library = call(&mut client, "sequence.player", json!({ "action": "library" }));
            assert_eq!(
                library["sequences"],
                json!([]),
                "the job's sub-sequence left the library"
            );
            let wav =
                std::fs::read(script_project.join("out").join("mix_audio.wav")).expect("the timeline WAV");
            *result.lock().expect("slot") = Some(wav);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let wav = slot.lock().expect("slot").take().expect("the script ran");
    let _ = std::fs::remove_dir_all(&project);
    wav
}

/// The loudest left-channel sample of the frames `from..to`.
fn peak(wav: &[u8], from: usize, to: usize) -> i16 {
    wav[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .skip(from * PER_FRAME)
        .take((to - from) * PER_FRAME)
        .map(|s| i16::from_le_bytes([s[0], s[1]]).saturating_abs())
        .max()
        .unwrap_or(0)
}

#[test]
fn a_sub_sequence_sound_lands_on_the_job_timeline_in_its_mixdown() {
    let wav = run("a", MAIN);
    assert_eq!(wav.len(), 44 + FRAMES * PER_FRAME * 4, "stereo PCM16");
    assert_eq!(peak(&wav, 0, 2), 0, "silent before the section");
    assert!(peak(&wav, 5, 8) > 1_000, "the section sounds");
    assert_eq!(peak(&wav, 12, FRAMES), 0, "silent after it");
    assert_eq!(wav, run("b", MAIN), "two runs write the same bytes");
}

#[test]
fn a_sound_at_zero_keeps_its_first_frames_through_the_pre_roll() {
    let wav = run("zero", AT_ZERO);
    assert!(peak(&wav, 0, 1) > 1_000, "it sounds on frame 0");
    assert!(
        peak(&wav, 4, 5) > 1_000,
        "and to its end: the pre-roll did not eat it"
    );
    assert_eq!(peak(&wav, 8, FRAMES), 0, "silent after its 0.2 s");
}
