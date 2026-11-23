// SPDX-License-Identifier: MPL-2.0
//! Render-out of an editor-authored sequence (v0.5 ledger B07/C03). The headless editor (started
//! with `--no-gpu`) edits a camera curve and an audio section gain over the agent socket and saves
//! the `.seq.ron`; it must refuse a render-out with `editor_headless` rather than open a GPU device
//! behind its flag (ADR-5180). As with `regia render --job` on a farm, a runtime host renders the
//! saved file: a headless app with the runtime renderer (any Vulkan adapter, lavapipe in CI), the
//! headless mixer and the
//! agent runs `render.out_job` on it, with a binding table that puts the camera track on the
//! camera. The test checks the plan, the takes against the full render, that the edited curve moves
//! the picture (frame 0 matches the unedited sequence, the last frame does not), that the -12 to
//! -6 dB gain edit doubles the section's level in the mixdown with silence around it, and that the
//! same job writes the same frames and WAV bytes twice. No golden image; nothing asserts on time.

use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use regia::audio::{AudioPlugin, AudioSettings, SoundAsset, SoundAssets};
use regia::prelude::*;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, PostSettings,
    RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use regia_test_utils::write_atomic;

const SEQ: &str = "assets/cine/intro.seq.ron";
const WAV: &str = "assets/audio/chime.wav";
const SEQUENCE: &str = r#"(version: 3, name: "intro", fps: 30.0, tracks: [
    (name: "camera", target: "camera", property: "transform.position",
     keys: Vec3([(time: 0.0, value: (0.0, 0.0, 2.6)), (time: 2.0, value: (0.5, 0.0, 2.6))])),
    (name: "audio", keys: Audio([(sound: "chime", start: 0.5, end: 1.5, volume_db: -12.0)])),
])"#;
/// 10 fps at the mixer's 48 kHz.
const FPS: usize = 10;
const PER_FRAME: usize = 4_800;
/// The job: the whole sequence and its last second as a second take.
const JOB: &str = "(version: 2, name: \"intro\", sequence: Some(\"assets/cine/intro.seq.ron\"), \
     bindings: {\"camera\": Name(\"CameraTrack\")}, out: \"renders\", \
     output: (width: Some(48), height: Some(32), fps: Some(10), hud: Some(false), \
     audio: Some((format: Some(\"pcm16\")))), \
     takes: [(name: \"all\", range: Whole), (name: \"tail\", range: Seconds(start: 1.0, end: 2.0))])";
/// A hang guard only.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);

/// What one runtime render-out wrote.
struct Rendered {
    plan: Value,
    manifest: Value,
    first_png: Vec<u8>,
    wav: Vec<u8>,
}

impl Rendered {
    fn take(&self, name: &str) -> &Value {
        self.manifest["job"]["takes"]
            .as_array()
            .unwrap_or_else(|| panic!("manifest takes: {}", self.manifest))
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("take {name}: {}", self.manifest))
    }

    fn hashes(&self, take: &str) -> Vec<Value> {
        self.take(take)["frame_hashes"]
            .as_array()
            .unwrap_or_else(|| panic!("{take} hashes: {}", self.take(take)))
            .clone()
    }
}

fn chime() -> Vec<u8> {
    // A steady 440 Hz tone, 2 s at 48 kHz, peak 0.5.
    let samples: Vec<f32> = (0..96_000)
        .map(|i| 0.5 * (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect();
    regia_media::encode::wav::write_pcm16(48_000, 1, &samples)
}

/// The headless editor edits the camera's second key (x 0.5 -> 1.0) and the section gain
/// (-12 -> -6 dB), saves, and its saved file is returned.
fn author_in_editor() -> String {
    let f = Fixture::new("render-out-acceptance");
    for (path, bytes) in [(SEQ, SEQUENCE.as_bytes().to_vec()), (WAV, chime())] {
        let path = f.0.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("fixture dir");
        write_atomic(&path, &bytes).expect("fixture");
    }
    let mut editor = Editor::start(&f);
    editor.event("ready", &f);
    let mut client = connect(&f);
    // An editor started with --no-gpu opens no device for a render-out (ADR-5180): it refuses
    // with `editor_headless`, like its view methods, and starts no job.
    // The job's binding resolves here too, so the refusal is the GPU's, not the table's.
    call(&mut client, "editor.spawn", json!({"name": "CameraTrack"}));
    for (method, params) in [
        ("render.out_start", json!({"out": "renders", "frames": 2})),
        ("render.out_job", json!({"job": JOB})),
    ] {
        assert_eq!(
            error_code(client.call(method, params)),
            "editor_headless",
            "{method}"
        );
    }
    assert!(
        client.call("render.out_progress", json!({})).is_err(),
        "no job was started"
    );
    assert!(!f.0.join("renders").exists(), "nothing was written");
    let opened = call(&mut client, "editor.seq_curve_open", json!({"path": SEQ}));
    let player = opened["player"].clone();
    call(
        &mut client,
        "editor.seq_curve_edit",
        json!({"player": player, "track": "camera", "component": 0,
        "op": "move", "index": 1, "time": 2.0, "value": 1.0}),
    );
    let keys = call(
        &mut client,
        "editor.seq_curve_state",
        json!({"player": player, "track": "camera", "component": 0}),
    )["keys"]
        .clone();
    assert!(keys.to_string().contains("1.0"), "{keys}");
    call(
        &mut client,
        "editor.seq_section_edit",
        json!({"player": player, "track": "audio", "op": "gain", "index": 0, "value": -6.0}),
    );
    let saved = call(&mut client, "editor.seq_curve_save", json!({"player": player}));
    // The current `.seq.ron` version (v5 since ADR-2948; this said 3).
    assert_eq!(
        saved["version"],
        regia_framework::sequencer::SEQUENCE_FORMAT_VERSION,
        "{saved}"
    );
    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    editor.event("exit", &f);
    assert_eq!(editor.wait().code(), Some(0), "{}", f.log());
    level_text(&f, SEQ)
}

fn read_token(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_string()
}

fn timed(client: &mut Client, method: &str, mut params: Value) -> Value {
    params["timeout_ms"] = json!(60_000);
    client
        .call(method, params)
        .unwrap_or_else(|e| panic!("{method}: {e}"))
}

/// Renders `JOB` with `sequence` in a fresh runtime host and project.
fn render(tag: &str, sequence: &str) -> Rendered {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-acceptance-{tag}"));
    for (path, bytes) in [(SEQ, sequence.as_bytes().to_vec()), (WAV, chime())] {
        let path = project.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("project dir");
        write_atomic(&path, &bytes).expect("project file");
    }
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
    // The sound the section names, loaded from the project's own file.
    let sound = SoundAsset::from_file_static(project.join(WAV)).expect("the project's chime");
    app.world_mut()
        .get_resource_or_insert_with(SoundAssets::default)
        .add("chime", sound);
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
    app.add_systems(
        PostUpdate,
        camera_global.after(regia_framework::sequencer::SequencerSet::Apply),
    );
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 2.6));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        regia_scene::components::Name::new("CameraTrack"),
        Camera {
            exposure_ev: Some(14.0),
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

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let slot: Arc<Mutex<Option<Rendered>>> = Arc::new(Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let root = project.clone();
    let script = std::thread::Builder::new()
        .name(format!("render-out-acceptance-{tag}"))
        .spawn(move || {
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let mut client = None;
            // Bounded backoff (1 ms doubling to 50 ms), so the poll does not compete with the
            // `app.update()` thread on a loaded host.
            let mut backoff = Duration::from_millis(1);
            while Instant::now() < deadline {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&agent_dir))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            let mut client = client.unwrap_or_else(|| panic!("no endpoint at {}", agent_dir.display()));
            let dry = timed(
                &mut client,
                "render.out_job",
                json!({"job": JOB, "dry_run": true}),
            );
            assert_eq!(dry["dry_run"], true, "{dry}");
            let started = timed(&mut client, "render.out_job", json!({"job": JOB}));
            assert_eq!(started["plan"], dry["plan"], "the dry run is the plan that runs");
            let job = started["job"].as_u64().expect("job id");
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            loop {
                let progress = timed(&mut client, "render.out_progress", json!({"job": job}));
                if progress["status"] != "running" {
                    assert_eq!(progress["status"], "done", "{progress}");
                    break;
                }
                assert!(Instant::now() < deadline, "the job did not finish: {progress}");
                std::thread::yield_now();
            }
            let manifest: Value = serde_json::from_slice(
                &std::fs::read(root.join("renders/intro_render_out.json")).expect("the job manifest"),
            )
            .expect("manifest JSON");
            let rendered = Rendered {
                plan: started["plan"].clone(),
                manifest,
                first_png: std::fs::read(root.join("renders/all/all_000000.png")).expect("frame 0 of all"),
                wav: std::fs::read(root.join("renders/intro_audio.wav")).expect("the timeline WAV"),
            };
            *result.lock().expect("slot") = Some(rendered);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn the script");
    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("the script finished");
    drop(app);
    let rendered = slot.lock().expect("slot").take().expect("the script ran");
    let _ = std::fs::remove_dir_all(&project);
    rendered
}

/// This host has no transform propagation (`MinimalPlugins`): the job's player writes the camera's
/// `Transform`, and this copies it to the `GlobalTransform` the renderer reads.
fn camera_global(mut cameras: Query<(&Transform, &mut GlobalTransform), With<Camera>>) {
    for (transform, mut global) in &mut cameras {
        global.0 = transform.compute_affine();
    }
}

/// Root mean square of the left channel over frames `from..to`, in full-scale units.
fn rms(wav: &[u8], from: usize, to: usize) -> f64 {
    let samples: Vec<f64> = wav[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .skip(from * PER_FRAME)
        .take((to - from) * PER_FRAME)
        .map(|s| f64::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0)
        .collect();
    (samples.iter().map(|s| s * s).sum::<f64>() / samples.len().max(1) as f64).sqrt()
}

/// The editor half on every host: a `--no-gpu` editor needs no GPU, so its render-out refusal
/// (ADR-5180: `editor_headless`, no job, nothing written) and the saved edits are checked even where
/// the runtime half below is skipped for want of a Vulkan driver.
#[test]
fn tools_no_gpu_editor_refuses_render_out_and_saves_its_edits() {
    let edited = author_in_editor();
    assert_ne!(edited, SEQUENCE, "the editor saved its edits");
}

#[test]
fn tools_render_out_of_an_editor_authored_sequence() {
    let Some(_icd) = lavapipe_icd() else {
        eprintln!("skip: no lavapipe driver manifest in /usr/share/vulkan/icd.d");
        return;
    };
    let edited = author_in_editor();
    assert_ne!(edited, SEQUENCE, "the editor saved its edits");
    let original = render("original", SEQUENCE);
    let first = render("edited-a", &edited);
    let second = render("edited-b", &edited);

    // The plan: the sequence's length at 10 fps, one job-owned player, the camera bound.
    let frames = 2 * FPS;
    assert_eq!(first.plan["frames"], frames as u64, "{}", first.plan);
    assert_eq!(first.plan["sequence_players"], 1, "{}", first.plan);
    assert_eq!(first.plan["unbound_targets"], json!([]), "{}", first.plan);
    let warnings = first.plan["warnings"].to_string();
    assert!(!warnings.contains("sequence_targets_unbound"), "{warnings}");
    assert!(!warnings.contains("sequence_tracks_not_applied"), "{warnings}");

    // Takes: every frame of the whole take, the tail is the same frames of the full render.
    for r in [&original, &first, &second] {
        assert_eq!(r.take("all")["status"], "done");
        assert_eq!(r.take("all")["written"], frames as u64);
        assert_eq!(r.take("tail")["written"], FPS as u64);
        assert_eq!(
            r.hashes("tail"),
            r.hashes("all")[FPS..].to_vec(),
            "a take is the full render's frames"
        );
    }
    assert_eq!(&first.first_png[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(
        &first.first_png[16..24],
        &[0, 0, 0, 48, 0, 0, 0, 32],
        "48x32 frames"
    );

    // The edited curve moves the picture: the same start, a different camera later on.
    let (before, after) = (original.hashes("all"), first.hashes("all"));
    assert_eq!(before[0], after[0], "frame 0 is the shared first key");
    assert_ne!(
        before[frames - 1],
        after[frames - 1],
        "the moved key shows in the last frame"
    );
    assert_ne!(after[0], after[frames - 1], "the camera moved during the take");

    // The gain edit: silence outside the 0.5 .. 1.5 s section, 6 dB louder inside it.
    let wav = &first.wav;
    assert_eq!(
        wav.len(),
        44 + frames * PER_FRAME * 4,
        "stereo PCM16 of the timeline"
    );
    assert_eq!(rms(wav, 0, 4), 0.0, "silent before the section");
    assert_eq!(rms(wav, 16, frames), 0.0, "silent after it");
    let (loud, quiet) = (rms(wav, 7, 13), rms(&original.wav, 7, 13));
    assert!(quiet > 0.01, "the unedited section sounds: {quiet}");
    let ratio = loud / quiet;
    let expected = 10f64.powf(6.0 / 20.0);
    assert!(
        (ratio - expected).abs() / expected < 0.02,
        "-6 dB against -12 dB: ratio {ratio}, expected {expected}"
    );

    // The same job on the same saved file repeats exactly.
    assert_eq!(first.hashes("all"), second.hashes("all"), "frames repeat");
    assert_eq!(first.wav, second.wav, "the mixdown repeats");
    println!(
        "render_out_acceptance: frames={frames} last_hash={} section_rms={loud:.4} ratio={ratio:.4}",
        after[frames - 1]
    );
}
