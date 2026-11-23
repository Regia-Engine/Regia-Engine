// SPDX-License-Identifier: MPL-2.0
//! A render job's own sequence plays its video sections frame-exact (ADR-2862), on lavapipe with the
//! headless mixer: every output frame published the source picture its time asks for (read from the
//! decoded picture, not from the image), a picture bound to a mesh reaches the rendered frames, its
//! sound reaches the mixdown, and two runs write the same frame hashes and the same WAV.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regia::audio::{AudioPlugin, AudioSettings};
use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_framework::sequencer::{SequencePlayer, SequenceVideos};
use regia_media::mux::{PatternClip, ToneAudio, pattern_index};
use regia_media::plugin::MediaFiles;
use regia_media::{MediaPlugin, VideoFramePlugin, VideoPlayerState};
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, PostSettings,
    RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
/// 2 s at 30 fps.
const FRAMES: u32 = 60;
const FPS: u32 = 30;
/// The clip: 3 s at 60 fps with a 1 kHz tone.
const SOURCE_FPS: u32 = 60;

/// The section starts at 0.5 s, 0.25 s into the source, on the mesh's emissive slot.
const SEQ: &str = r#"(version: 4, tracks: [
    (name: "wall", keys: Video([(video: "movies/clip.webm", start: 0.5, end: 1.5, offset: 0.25,
        target: Screen(binding: "mesh", slot: Emissive))])),
    (name: "hold", target: "mesh", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 2.0, value: (0.0, 0.0, 0.0))])),
])"#;

/// The same timeline with nothing on the wall.
const NO_VIDEO: &str = r#"(version: 4, tracks: [
    (name: "hold", target: "mesh", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 2.0, value: (0.0, 0.0, 0.0))])),
])"#;

const JOB: &str = "(version: 2, name: \"wall\", sequence: Some(\"cine/main.seq.ron\"), \
     bindings: {\"mesh\": Name(\"Mesh\")}, out: \"out\", \
     output: (width: Some(32), height: Some(24), fps: Some(30), hud: Some(false), \
     audio: Some((format: Some(\"pcm16\")))), \
     takes: [(name: \"all\", range: Frames(start: 0, end: 60))])";

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

/// What the frames showed, by the sequence clock in nanoseconds: the index the picture carries, and
/// the sections' counters.
#[derive(Resource, Clone, Default)]
struct Shown(Arc<Mutex<BTreeMap<u64, (u32, u64)>>>);

/// `Last`: reads the picture of the frame that is about to be rendered.
fn record_shown(
    players: Query<(Entity, &SequencePlayer)>,
    videos: Res<SequenceVideos>,
    mut sessions: Query<&mut VideoPlayerState>,
    shown: Res<Shown>,
) {
    for (entity, player) in &players {
        for section in videos.sections(entity) {
            let Some(media) = section.media else { continue };
            let Ok(mut state) = sessions.get_mut(media) else {
                continue;
            };
            let core = state.core_mut();
            let Some(picture) = core.presented() else { continue };
            assert!(section.exact && section.certain, "{section:?}");
            let index = pattern_index(&picture.planes[0]);
            shown
                .0
                .lock()
                .expect("shown")
                .insert(player.time_nanos(), (index, section.frames_dropped));
        }
    }
}

struct Run {
    shown: BTreeMap<u64, (u32, u64)>,
    hash: String,
    frame_hashes: Vec<Value>,
    wav: Vec<u8>,
    sections: Value,
}

fn run(tag: &str, seq: &str) -> Run {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-video-{tag}"));
    std::fs::create_dir_all(project.join("cine")).expect("cine dir");
    std::fs::create_dir_all(project.join("movies")).expect("movies dir");
    std::fs::write(project.join("cine/main.seq.ron"), seq).expect("sequence");
    let mut clip = PatternClip::new(16, 8, 3 * SOURCE_FPS, SOURCE_FPS);
    clip.audio = Some(ToneAudio {
        rate: 48_000,
        channels: 1,
        freq_hz: 1_000.0,
        amplitude: 0.5,
    });
    std::fs::write(project.join("movies/clip.webm"), clip.to_webm()).expect("clip");
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..AudioSettings::default()
    });
    app.insert_resource(MediaFiles::with_root(&project));
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        AudioPlugin,
        regia_agent::AgentPlugin,
        MediaPlugin,
        VideoFramePlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    let shown = Shown::default();
    app.insert_resource(shown.clone());
    app.add_systems(Last, record_shown);
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
                // The video's picture is emissive in nits (ADR-9606): at 1 nit (the binding's
                // default) it is about 5e-5 of white under the camera's EV 14 and never shows in
                // the 8-bit frame, so the frames saw it only through the indirect light's own
                // update cadence. 10,000 nits puts the picture itself in the frame. The colour
                // stays black: without a picture the wall emits nothing, as before.
                emissive_intensity: 10_000.0,
                ..CpuMaterial::default()
            }),
        ),
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let slot = Arc::new(Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_project: PathBuf = project.clone();
    let thread = std::thread::Builder::new()
        .name(format!("render-out-video-{tag}"))
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
            let job = started["job"].as_u64().expect("job id");
            let deadline = Instant::now() + SCRIPT_TIMEOUT;
            let finished = loop {
                let progress = call(
                    &mut client,
                    "render.out_progress",
                    json!({ "job": job, "hashes": true }),
                );
                if progress["status"] != json!("running") {
                    break progress;
                }
                assert!(Instant::now() < deadline, "the job did not finish");
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(finished["status"], json!("done"), "{finished}");
            let wav =
                std::fs::read(script_project.join("out").join("wall_audio.wav")).expect("the timeline WAV");
            *result.lock().expect("slot") = Some((finished, wav));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let (finished, wav) = slot.lock().expect("slot").take().expect("the script ran");
    let sections = json!({
        "opened": app.world().resource::<SequenceVideos>().opened(),
        "closed": app.world().resource::<SequenceVideos>().closed(),
    });
    let _ = std::fs::remove_dir_all(&project);
    let shown = shown.0.lock().expect("shown").clone();
    Run {
        shown,
        hash: finished["sequence_hash"].as_str().unwrap_or_default().to_string(),
        frame_hashes: finished["hashes"]["hashes"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        wav,
        sections,
    }
}

/// The loudest left-channel sample of output frames `from..to`.
fn peak(wav: &[u8], from: usize, to: usize) -> i16 {
    let per_frame = 48_000 / FPS as usize;
    wav[44..]
        .as_chunks::<4>()
        .0
        .iter()
        .skip(from * per_frame)
        .take((to - from) * per_frame)
        .map(|s| i16::from_le_bytes([s[0], s[1]]).saturating_abs())
        .max()
        .unwrap_or(0)
}

/// The brief's acceptance: a 2 s render-out shows the expected source frame on every output frame,
/// with no drops, and two runs agree on every picture and every sample.
#[test]
fn render_out_shows_the_expected_source_frame_on_every_output_frame() {
    let first = run("a", SEQ);
    // Frame k of the job is at sequence time k / 30 (the render clock); only 0.5 s to 1.5 s has a
    // section. Each is read from the picture, by the clock of the frame it was published on.
    let mut checked = 0;
    for k in 0..FRAMES {
        let t = u64::from(k) * 1_000_000_000 / u64::from(FPS);
        let in_section = (500_000_000..1_500_000_000).contains(&t);
        // The job's clock rounds its nanoseconds; integer division here truncates them.
        let entry = first
            .shown
            .range(t.saturating_sub(2)..=t + 2)
            .next()
            .map(|(_, e)| e);
        if !in_section {
            assert!(entry.is_none(), "frame {k} has no section: {entry:?}");
            continue;
        }
        // 0.25 s into a 60 fps source at the section's start, and it advances two source frames per
        // output frame.
        let want = (k - 15) * 2 + 15;
        let (index, drops) =
            *entry.unwrap_or_else(|| panic!("frame {k} showed no picture: {:?}", first.shown));
        assert_eq!(index, want, "output frame {k}");
        assert_eq!(drops, 0, "output frame {k}");
        checked += 1;
    }
    assert_eq!(checked, 30, "the whole section");
    assert_eq!(first.sections["opened"], json!(1), "{}", first.sections);
    assert_eq!(
        first.sections["closed"], first.sections["opened"],
        "job player gone"
    );
    assert_eq!(first.frame_hashes.len(), FRAMES as usize);

    // The picture reaches the rendered frames: the frames in the section differ from a run with
    // nothing on the wall, and the frames before it do not.
    let bare = run("bare", NO_VIDEO);
    assert_eq!(
        first.frame_hashes[..15],
        bare.frame_hashes[..15],
        "before the section"
    );
    assert_ne!(first.frame_hashes[16], bare.frame_hashes[16], "in the section");
    assert_ne!(
        first.frame_hashes[16], first.frame_hashes[17],
        "a new picture each frame"
    );

    // The source's own sound is in the mixdown, inside the section only.
    assert!(peak(&first.wav, 0, 14) == 0, "silent before the section");
    assert!(
        peak(&first.wav, 17, 44) > 8_000,
        "the tone: {}",
        peak(&first.wav, 17, 44)
    );
    assert!(
        peak(&first.wav, 48, FRAMES as usize) == 0,
        "silent after the section"
    );

    // Two runs: the same pictures, frames and sound.
    let second = run("b", SEQ);
    assert_eq!(first.shown, second.shown);
    assert_eq!(first.hash, second.hash, "the frame hashes");
    assert_eq!(first.wav, second.wav, "the WAV");
}
