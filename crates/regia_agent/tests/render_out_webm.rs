// SPDX-License-Identifier: MPL-2.0
//! Render-out format `webm` (ADR-3369) end to end on lavapipe with the headless mixer: a job encodes
//! its frames to AV1 and its mixdown to Opus in one `.webm` with no external tool; the file passes the
//! cook validator and demuxes to exactly the delivered frames at their times, with the Opus track; a
//! cancelled job leaves a playable file of every delivered frame and no spool; hostile settings are
//! refused with their `data.reason` codes. CPU encode, lavapipe render; no goldens.
#![cfg(feature = "media-encode")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::audio::{Audio, AudioPlugin, AudioSettings, PlaybackParams, SoundAsset, SoundClip};
use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_media::container::{Demuxer, MkvDemuxer, TrackKind};
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
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-webm-{tag}"));
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
        .name(format!("render-out-webm-{tag}"))
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

fn wait(client: &mut Client, job: u64) -> Value {
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

/// Video packet times and keyframe flags, and the audio track's packet count, of a `.webm`.
fn demux(path: &Path) -> (Vec<i64>, Vec<bool>, usize) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    regia_media::validate(std::io::Cursor::new(&bytes)).expect("the cook validator accepts it");
    let mut d = MkvDemuxer::open(std::io::Cursor::new(bytes)).expect("demux");
    let info = d.probe().expect("probe");
    let video = info.video_track().expect("video").number;
    let audio = info
        .tracks
        .iter()
        .find(|t| t.kind == TrackKind::Audio)
        .map(|t| t.number);
    let (mut pts, mut keys, mut audio_packets) = (Vec::new(), Vec::new(), 0);
    while let Some(p) = d.next_packet().expect("packet") {
        if p.track == video {
            pts.push(p.pts_ns);
            keys.push(p.keyframe);
        } else if Some(p.track) == audio {
            audio_packets += 1;
        }
    }
    (pts, keys, audio_packets)
}

fn spool_left(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part"))
        .collect()
}

#[test]
fn a_webm_job_writes_av1_and_opus_that_demux_to_the_delivered_frames() {
    let project = run("done", |client, out| {
        let started = call(
            client,
            "render.out_start",
            job(
                out,
                "movie",
                json!({ "format": "webm", "audio": true, "encode": { "threads": 1, "speed": 10, "keyframe_interval": 6 } }),
            ),
        );
        assert_eq!(started["request"]["format"], json!("webm"), "{started}");
        assert_eq!(
            started["request"]["encode"]["keyframe_interval"],
            json!(6),
            "{started}"
        );
        assert_eq!(started["request"]["encode"]["threads"], json!(1), "{started}");
        let done = wait(client, started["job"].as_u64().expect("job"));
        assert_eq!(done["status"], json!("done"), "{done}");
        let dir = out.join("movie");
        let (pts, keys, audio_packets) = demux(&dir.join("movie.webm"));
        assert_eq!(pts.len() as u64, FRAMES);
        for (k, t) in pts.iter().enumerate() {
            // 30 fps: frame k at floor(k * 33.33 ms) in 1 ms units.
            assert_eq!(
                *t,
                (k as i64 * 1_000_000_000 / 30) / 1_000_000 * 1_000_000,
                "frame {k}"
            );
        }
        assert!(
            keys[0] && keys.iter().filter(|k| **k).count() >= 3,
            "keyframe interval 6: {keys:?}"
        );
        // 18 frames at 30 fps are 0.6 s: 30 Opus packets of 20 ms, plus one for the look-ahead.
        assert!((30..=31).contains(&audio_packets), "{audio_packets} Opus packets");
        assert!(spool_left(&dir).is_empty(), "{:?}", spool_left(&dir));
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("movie_render_out.json")).expect("manifest"))
                .expect("json");
        assert_eq!(
            manifest["files"]["frames"].as_array().map(Vec::len),
            Some(1),
            "{manifest}"
        );
        assert!(
            manifest["files"]["frames"][0]
                .as_str()
                .is_some_and(|f| f.ends_with("movie.webm")),
            "{manifest}"
        );
        assert!(
            dir.join("movie_audio.wav").is_file(),
            "the mixdown stays beside the movie"
        );
    });
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn a_cancelled_webm_job_leaves_a_playable_file_of_every_delivered_frame() {
    let project = run("cancel", |client, out| {
        let started = call(
            client,
            "render.out_start",
            job(
                out,
                "cut",
                json!({ "format": "webm", "frames": 400, "encode": { "threads": 1, "speed": 10 } }),
            ),
        );
        let job = started["job"].as_u64().expect("job");
        loop {
            let progress = call(client, "render.out_progress", json!({ "job": job }));
            if progress["frame"].as_u64().is_some_and(|f| f >= 3) || progress["status"] != json!("running") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The job may already be over on a fast machine; the guarantee holds either way.
        let _ = client.call(
            "render.out_cancel",
            json!({ "job": job, "timeout_ms": CALL_TIMEOUT_MS }),
        );
        let done = wait(client, job);
        let delivered = done["frame"].as_u64().expect("frame");
        assert!(delivered >= 3, "{done}");
        let dir = out.join("cut");
        let (pts, _, audio_packets) = demux(&dir.join("cut.webm"));
        assert_eq!(
            pts.len() as u64,
            delivered,
            "every delivered frame is in the movie: {done}"
        );
        assert_eq!(audio_packets, 0, "no audio requested");
        assert!(spool_left(&dir).is_empty(), "{:?}", spool_left(&dir));
    });
    let _ = std::fs::remove_dir_all(project);
}

#[test]
fn hostile_webm_settings_are_refused_with_their_codes() {
    let project = run("hostile", |client, out| {
        let reason = |client: &mut Client, extra: Value| -> Value {
            let mut params = job(out, "bad", extra);
            params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
            let err = client
                .call("render.out_start", params)
                .err()
                .unwrap_or_else(|| panic!("refused"));
            let text = format!("{err:?}");
            json!(text)
        };
        let odd = reason(client, json!({ "format": "webm", "width": 33 }));
        assert!(
            odd.as_str().is_some_and(|t| t.contains("encode_odd_size")),
            "{odd}"
        );
        let small = reason(client, json!({ "format": "webm", "width": 8, "height": 8 }));
        assert!(
            small.as_str().is_some_and(|t| t.contains("encode_bad_size")),
            "{small}"
        );
        let gop = reason(
            client,
            json!({ "format": "webm", "encode": { "keyframe_interval": 100000 } }),
        );
        assert!(
            gop.as_str()
                .is_some_and(|t| t.contains("encode_bad_keyframe_interval")),
            "{gop}"
        );
        let key = reason(client, json!({ "format": "webm", "encode": { "qualty": 3 } }));
        assert!(
            key.as_str().is_some_and(|t| t.contains("encode_bad_input")),
            "{key}"
        );
        let rate = reason(
            client,
            json!({ "format": "webm", "audio": { "sample_rate": 44100 } }),
        );
        assert!(rate.as_str().is_some_and(|t| t.contains("Opus")), "{rate}");
        let png = reason(client, json!({ "format": "png", "encode": { "quality": 40 } }));
        assert!(png.as_str().is_some_and(|t| t.contains("webm")), "{png}");
    });
    let _ = std::fs::remove_dir_all(project);
}
