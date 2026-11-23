// SPDX-License-Identifier: MPL-2.0
//! Render-out audio (ADR-1520, ADR-1554), on lavapipe with the headless mixer: the WAV mixdown and
//! stems are exactly as long as the frames, a sound the game starts lands on a frame's first sample,
//! two fresh runs give the same bytes, the manifest (`regia.render_out/2`) lists the WAVs in
//! `files.audio`, and under a time map the default render clock leaves the sound as it is, while
//! the simulation clock keeps the output length (`timeline: "output"`, varispeed) or the simulation
//! length (`timeline: "sim"`). Every frame path writes the samples the job's length implies, and a
//! stalled file sync never stops `render.out_progress` answering (ADR-1881).

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
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
const FRAMES: u64 = 18;
/// 48 kHz at 30 fps.
const PER_FRAME: u64 = 1_600;

fn vertex(pos: [f32; 3], normal: [f32; 3]) -> LabVertex {
    LabVertex::from_pos_normal_uv(pos, normal, [0.0, 0.0])
}

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            vertex([-1.2, -0.9, 0.0], [-0.8, 0.0, 0.6]),
            vertex([1.2, -0.9, 0.0], [0.8, 0.0, 0.6]),
            vertex([0.0, 0.9, 0.0], [0.0, 0.0, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
    settings
}

/// A flat 0.1 s burst: its first sample is already loud, so its onset is exact.
fn burst() -> SoundAsset {
    SoundAsset::from_clip(&SoundClip::new(vec![0.5; 4_800], 48_000).expect("a valid clip"))
}

/// Plays the burst on `Sfx` once, the first frame the game clock reaches 0.2 s. The clock is
/// paused until a render-out takes it over, so the sound is part of the job.
fn ping(time: Res<Time>, mut audio: ResMut<Audio>, mut fired: Local<bool>) {
    if *fired || time.elapsed_secs() < 0.2 {
        return;
    }
    *fired = true;
    let params = PlaybackParams {
        bus: Some("Sfx".into()),
        ..PlaybackParams::default()
    };
    if let Err(error) = audio.play_asset(&burst(), params) {
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

/// Starts a job and polls `render.out_progress` until it ends.
fn render(client: &mut Client, params: Value) -> Value {
    let started = call(client, "render.out_start", params);
    assert!(
        started["audio"].is_object(),
        "the answer names the audio: {started}"
    );
    let job = started["job"].as_u64().expect("job id");
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(client, "render.out_progress", json!({ "job": job }));
        if progress["status"] != json!("running") && progress["status"] != json!("cancelling") {
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

/// A fresh headless app (renderer, headless mixer, agent) with a still triangle and the ping; runs
/// `script` against its agent with the output root and returns the project dir.
fn run(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) -> PathBuf {
    run_with(tag, |_| {}, script)
}

/// [`run`] with the app adjusted by `host` first.
fn run_with(
    tag: &str,
    host: impl FnOnce(&mut App),
    script: impl FnOnce(&mut Client, &Path) + Send + 'static,
) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-audio-{tag}"));
    let out = project.join("out");

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
    app.insert_resource(settings());
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
    host(&mut app);
    let agent_dir = endpoint::agent_dir(&project);
    let thread = std::thread::Builder::new()
        .name(format!("render-out-audio-{tag}"))
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

fn job(out: &Path, name: &str, extra: Value) -> Value {
    let mut params = json!({
        "out": out.join(name).display().to_string(), "name": name, "frames": FRAMES,
        "width": 32, "height": 24, "fps": 30, "hud": false, "warmup_frames": 2,
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    params
}

/// The 16-bit samples of a PCM16 WAV (after its 44-byte header).
fn pcm16(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[36..40], b"data");
    bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

/// Renders the stem job and returns (manifest audio block, mix samples, Sfx samples, Music samples).
fn stem_job(tag: &str) -> (Value, Vec<i16>, Vec<i16>, Vec<i16>) {
    let result = Arc::new(std::sync::Mutex::new(None));
    let slot = result.clone();
    let project = run(tag, move |client, out| {
        let done = render(
            client,
            job(
                out,
                "stems",
                json!({ "audio": { "format": "pcm16", "stems": ["Sfx", "Music"] } }),
            ),
        );
        assert_eq!(done["status"], json!("done"), "{done}");
        let dir = out.join("stems");
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("stems_render_out.json")).expect("manifest"))
                .expect("json");
        assert_eq!(manifest["audio"], done["audio"], "progress and manifest agree");
        assert_eq!(manifest["schema"], "regia.render_out/2");
        assert_eq!(
            manifest["audio"]["clock"], "render",
            "the render clock by default"
        );
        let listed = manifest["files"]["audio"].as_array().expect("files.audio");
        let named = [
            &manifest["audio"]["mix"]["file"],
            &manifest["audio"]["stems"][0]["file"],
            &manifest["audio"]["stems"][1]["file"],
        ];
        assert_eq!(listed.len(), 3, "the mix and two stems: {listed:?}");
        for (file, end) in
            named
                .iter()
                .zip(["stems_audio.wav", "stems_audio_Sfx.wav", "stems_audio_Music.wav"])
        {
            assert!(listed.contains(*file), "{file} in files.audio");
            assert!(file.as_str().is_some_and(|f| f.ends_with(end)), "{file}");
        }
        assert_eq!(manifest["files"]["aov"], json!([]));
        let read = |file: &str| pcm16(&dir.join(file));
        *slot.lock().expect("slot") = Some((
            manifest["audio"].clone(),
            read("stems_audio.wav"),
            read("stems_audio_Sfx.wav"),
            read("stems_audio_Music.wav"),
        ));
    });
    let _ = std::fs::remove_dir_all(&project);
    let taken = result.lock().expect("slot").take();
    taken.expect("the script ran")
}

#[test]
fn the_mixdown_is_frame_exact_and_deterministic() {
    let (block, mix, sfx, music) = stem_job("a");
    let samples = FRAMES * PER_FRAME;
    assert_eq!(block["sample_rate"], 48_000);
    assert_eq!(block["samples"], samples);
    assert_eq!(block["mix"]["samples"], samples);
    assert_eq!(block["stems"][0]["bus"], "Sfx");
    for wav in [&mix, &sfx, &music] {
        assert_eq!(wav.len() as u64, 2 * samples, "as long as the frames");
    }
    // The burst starts on a frame's first sample, inside the job.
    let onset = mix.iter().position(|s| *s != 0).expect("the burst is in the mix") as u64 / 2;
    assert!(onset > 0 && onset < samples, "onset {onset}");
    assert_eq!(onset % PER_FRAME, 0, "onset {onset} is on a frame boundary");
    assert_eq!(
        sfx.iter().position(|s| *s != 0).map(|i| i as u64 / 2),
        Some(onset),
        "the Sfx stem hears it at the same sample"
    );
    assert!(music.iter().all(|s| *s == 0), "the Music stem is silent");

    // A second fresh app gives the same bytes.
    let (again, mix2, sfx2, _) = stem_job("b");
    assert_eq!(mix, mix2);
    assert_eq!(sfx, sfx2);
    assert_eq!(block["mix"]["hash"], again["mix"]["hash"]);
    assert_eq!(block["stems"][0]["hash"], again["stems"][0]["hash"]);
}

#[test]
fn a_time_map_keeps_the_output_length_or_the_sim_length() {
    let project = run("time-map", |client, out| {
        let slow = json!([{ "frame": 0, "scale": 0.5 }]);
        let render_clock = render(
            client,
            job(
                out,
                "render",
                json!({ "time_map": slow, "audio": { "format": "float32" } }),
            ),
        );
        assert_eq!(render_clock["status"], json!("done"), "{render_clock}");
        assert_eq!(render_clock["audio"]["timeline"], "render");
        assert_eq!(render_clock["audio"]["clock"], "render");
        assert_eq!(
            render_clock["audio"]["samples"],
            FRAMES * PER_FRAME,
            "the render clock keeps the video length"
        );
        let output = render(
            client,
            job(
                out,
                "vari",
                json!({ "time_map": slow, "audio": { "format": "float32", "timeline": "output" } }),
            ),
        );
        assert_eq!(output["status"], json!("done"), "{output}");
        assert_eq!(output["audio"]["timeline"], "output");
        assert_eq!(output["audio"]["clock"], "sim");
        assert_eq!(
            output["audio"]["samples"],
            FRAMES * PER_FRAME,
            "varispeed keeps the video length"
        );
        let sim = render(
            client,
            job(
                out,
                "sim",
                json!({ "time_map": slow, "audio": { "timeline": "sim" } }),
            ),
        );
        assert_eq!(sim["status"], json!("done"), "{sim}");
        assert_eq!(
            sim["audio"]["samples"],
            FRAMES * PER_FRAME / 2,
            "half the simulation time"
        );
        let len = std::fs::metadata(out.join("sim").join("sim_audio.wav"))
            .expect("wav")
            .len();
        assert_eq!(len, 44 + FRAMES * PER_FRAME / 2 * 6, "pcm24 stereo");
    });
    let _ = std::fs::remove_dir_all(&project);
}

/// One job in a fresh app (so the ping fires in it) with `extra` params; returns the manifest's
/// audio block and the mix's samples.
fn mix_job(tag: &str, extra: Value) -> (Value, Vec<i16>) {
    let result = Arc::new(std::sync::Mutex::new(None));
    let slot = result.clone();
    let project = run(tag, move |client, out| {
        let mut params = json!({ "audio": { "format": "pcm16" } });
        params
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        let done = render(client, job(out, "mix", params));
        assert_eq!(done["status"], json!("done"), "{done}");
        *slot.lock().expect("slot") = Some((
            done["audio"].clone(),
            pcm16(&out.join("mix").join("mix_audio.wav")),
        ));
    });
    let _ = std::fs::remove_dir_all(&project);
    let taken = result.lock().expect("slot").take();
    taken.expect("the script ran")
}

#[test]
fn the_render_clock_wav_does_not_move_with_the_time_map() {
    // The ping fires on the render clock, and the mixer steps by it: slow motion leaves the sound
    // effects at their speed and pitch, sample for sample (ADR-1554).
    let (plain, plain_mix) = mix_job("plain", json!({}));
    let (slow, slow_mix) = mix_job("slow", json!({ "time_map": [{ "frame": 0, "scale": 0.25 }] }));
    let onset = plain_mix
        .iter()
        .position(|s| *s != 0)
        .expect("the burst is in the mix") as u64
        / 2;
    assert_eq!(onset % PER_FRAME, 0, "onset {onset} is on a frame boundary");
    assert_eq!(plain_mix, slow_mix);
    assert_eq!(plain["mix"]["hash"], slow["mix"]["hash"]);
    assert_eq!(slow["samples"], FRAMES * PER_FRAME);
}

fn ffmpeg_on_path() -> bool {
    let Some(path) = regia_core::brand::env_var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("ffmpeg").is_file() || dir.join("ffmpeg.exe").is_file())
}

/// The audio of a finished job has exactly the job's samples (Red-Rouge v0.2.6: `format: "exr"` wrote
/// 0 samples and a 44-byte WAV, and reported `done`).
fn assert_full_length(out: &Path, name: &str, done: &Value) {
    assert_eq!(done["status"], json!("done"), "{name}: {done}");
    let samples = FRAMES * PER_FRAME;
    assert_eq!(done["audio"]["frames"], FRAMES, "{name}");
    assert_eq!(done["audio"]["samples"], samples, "{name}");
    assert_eq!(done["audio"]["mix"]["samples"], samples, "{name}");
    assert_ne!(
        done["audio"]["mix"]["hash"], "cbf29ce484222325",
        "{name}: the FNV-1a start value means no bytes were hashed"
    );
    assert_eq!(done["audio"]["stems"][0]["samples"], samples, "{name}");
    let wav = out.join(name).join(format!("{name}_audio.wav"));
    assert_eq!(pcm16(&wav).len() as u64, 2 * samples, "{name}");
}

#[test]
fn every_format_and_frame_path_writes_the_samples_the_job_implies() {
    let project = run("formats", |client, out| {
        let mut cases = vec![
            ("png", json!({ "format": "png" })),
            ("raw", json!({ "format": "raw" })),
            ("exr", json!({ "format": "exr" })),
            // The accumulating paths: subframes on 8-bit frames, 16-bit frames, and both on EXR.
            ("shutter", json!({ "format": "png", "subframes": 3 })),
            ("deep", json!({ "format": "png", "bit_depth": 16 })),
            ("rawdeep", json!({ "format": "raw", "bit_depth": 16 })),
            ("exrshutter", json!({ "format": "exr", "subframes": 3 })),
        ];
        if ffmpeg_on_path() {
            for format in ["ffv1", "prores", "dnxhr"] {
                cases.push((format, json!({ "format": format, "width": 256, "height": 144 })));
            }
        }
        for (name, extra) in cases {
            let mut params = json!({
                "audio": { "format": "pcm16", "timeline": "output", "stems": ["Sfx"] }
            });
            params
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            let done = render(client, job(out, name, params));
            assert_full_length(out, name, &done);
        }
    });
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_scene_linear_job_on_a_time_map_follows_the_clock_it_asked_for() {
    let project = run("exr-map", |client, out| {
        let slow = json!([{ "frame": 0, "scale": 0.5 }]);
        for (name, timeline, samples) in [
            ("render", "render", FRAMES * PER_FRAME),
            ("output", "output", FRAMES * PER_FRAME),
            ("sim", "sim", FRAMES * PER_FRAME / 2),
        ] {
            let done = render(
                client,
                job(
                    out,
                    name,
                    json!({ "format": "exr", "time_map": slow, "audio": { "timeline": timeline } }),
                ),
            );
            assert_eq!(done["status"], json!("done"), "{done}");
            assert_eq!(done["audio"]["samples"], samples, "{name}");
        }
    });
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_poll_answers_within_its_deadline_while_the_wavs_sync() {
    // Red-Rouge v0.2.6: nine WAVs synced on the app thread of a busy disk, no answer for 52 s, a
    // poll failed with F0006. Here the sync holds until the script has polled through it: if the
    // app thread waited for the sync, the first poll past the last frame would run out its short
    // deadline and fail the test.
    const POLL_DEADLINE_MS: u64 = 20_000;
    let (entered, in_sync) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(Some((entered, released)));
    let sync = regia_agent::RenderOutFileSync(Arc::new(move |file| {
        let hold = held.lock().expect("held").take();
        if let Some((entered, released)) = hold {
            entered.send(()).expect("the script waits");
            released
                .recv_timeout(Duration::from_secs(600))
                .map_err(std::io::Error::other)?;
        }
        file.sync_all()
    }));
    let project = run_with(
        "slow-sync",
        move |app| {
            app.insert_resource(sync);
        },
        move |client, out| {
            let started = call(
                client,
                "render.out_start",
                job(
                    out,
                    "held",
                    json!({ "audio": { "format": "pcm16", "stems": ["Sfx", "Music"] } }),
                ),
            );
            let id = started["job"].as_u64().expect("job id");
            in_sync
                .recv_timeout(SCRIPT_TIMEOUT)
                .expect("the job reached its sync");
            let wav = out.join("held").join("held_audio.wav");
            for _ in 0..8 {
                let poll = client
                    .call(
                        "render.out_progress",
                        json!({ "job": id, "timeout_ms": POLL_DEADLINE_MS }),
                    )
                    .expect("the app answers during the sync");
                assert_eq!(poll["status"], json!("running"), "{poll}");
                assert_eq!(poll["phase"], json!("closing"), "{poll}");
                assert_eq!(poll["frame"], FRAMES, "{poll}");
                assert!(!wav.exists(), "the WAV is renamed only after its sync");
            }
            // Red-Rouge v0.2.8: a cancel in the closing phase answered NOT_FOUND. It is idempotent
            // (ADR-1965): the closing job answers `closing` with the status its frames ended with,
            // by id and without one, and stays closing (the cancel changes nothing).
            for params in [json!({ "job": id }), json!({})] {
                let cancel = call(client, "render.out_cancel", params);
                assert_eq!(cancel["job"], json!(id), "{cancel}");
                assert_eq!(cancel["status"], json!("closing"), "{cancel}");
                assert_eq!(cancel["ending"], json!("done"), "{cancel}");
            }
            let poll = call(client, "render.out_progress", json!({ "job": id }));
            assert_eq!(poll["phase"], json!("closing"), "{poll}");
            release.send(()).expect("the sync waits");
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let done = loop {
                let poll = call(client, "render.out_progress", json!({ "job": id }));
                if poll["status"] != json!("running") {
                    break poll;
                }
                assert!(Instant::now() < deadline, "the job did not end");
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_full_length(out, "held", &done);
            assert!(out.join("held").join("held_render_out.json").is_file());
            let after = call(client, "render.out_cancel", json!({ "job": id }));
            assert_eq!(after["status"], json!("done"), "{after}");
            assert_eq!(after["finished"], json!(true), "{after}");
        },
    );
    let _ = std::fs::remove_dir_all(&project);
}
