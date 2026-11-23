// SPDX-License-Identifier: MIT-0
//! Shared by the render tests: runs a headless showcase app, drives it through the agent protocol
//! (camera path, then a render-out job) and reports the frame hashes.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_agent::client::{Client, ClientError};
use regia_agent::endpoint;
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};
use showcase::app::{Options, ShowcaseStats, TerrainStatsHandle, build_headless_app};
use showcase::features::FeatureOptions;
use showcase::scene::load_scene_with;

pub const SCRIPT_TIMEOUT: Duration = Duration::from_secs(1800);
const CALL_TIMEOUT_MS: u64 = 600_000;

/// What to render.
#[derive(Clone, Debug)]
pub struct JobSpec {
    pub width: u32,
    pub height: u32,
    pub frames: u64,
    pub warmup_frames: u64,
    pub plain: bool,
    pub scatter_lod: usize,
    /// Fixed camera exposure, EV100.
    pub exposure_ev: Option<f32>,
    /// Run the job this many times in the same app (the last run's hashes are reported).
    pub repeat: usize,
    /// Keep the PNGs of this run in a directory (for a look): the first and the last frame.
    pub keep: Option<PathBuf>,
    /// With [`Self::keep`], keep every frame (to judge motion).
    pub keep_every_frame: bool,
    /// The scene directory. `None`: [`scene_dir`].
    pub scene: Option<PathBuf>,
    /// Camera eye and look-at point, world metres. `None`: the default test pose.
    pub camera: Option<([f32; 3], [f32; 3])>,
    /// Keep the Hi-Z occlusion test ([`showcase::app::Options::hiz`]).
    pub hiz: bool,
    /// Keep the GPU-driven draw path ([`showcase::app::Options::gpu_driven`]).
    pub gpu_driven: bool,
    /// The capped entity scatter instead of the streamed field ([`showcase::app::Options::entity_scatter`]).
    pub entity_scatter: bool,
    /// Where the camera eye ends (a straight dolly from `camera`'s eye over the job's frames, looking at the
    /// same point). `None`: a held camera.
    pub camera_end: Option<[f32; 3]>,
    /// The render profile preset ([`showcase::app::Options::profile`]).
    pub profile: Option<regia_render::RenderProfilePreset>,
    /// A quality-ladder tier by name ([`showcase::app::Options::tier`], ADR-8844): wins over [`Self::profile`].
    pub tier: Option<regia_render::QualityTier>,
    /// The contact map ([`showcase::app::Options::contact_ao`]).
    pub contact_ao: bool,
    /// Agent calls (method, params) made once after the warm and before the camera is placed, in order.
    pub calls: Vec<(String, Value)>,
    /// Load the scene with grass on even without `terrain/grass.ron` (`--grass`).
    pub grass: bool,
    /// Agent calls (method, params) made once after the job, in order; the answers are in [`Outcome::reports`].
    pub after: Vec<(String, Value)>,
    /// The job's frame rate. Every frame steps the game clock by `1 / fps` seconds (`render.out_start`'s `fps`).
    pub fps: u32,
    /// More `render.out_start` params (an object merged over the defaults: `aovs`, `audio`, ...).
    pub extra: Value,
    /// Ask for the offscreen warm at the job's size (`render.pipeline_warm { offscreen }`, ADR-1965)
    /// after [`Self::calls`] and wait until it is done, as a filming session does before its first job.
    pub offscreen_warm: bool,
    /// Pause the game clock and set the camera path again before every run of a repeated job (a filming
    /// session re-arming its shot), instead of once before the first: the path starts with each job.
    pub rearm_path: bool,
    /// Place the camera with `camera.set_path` (from [`Self::camera`]) after [`Self::calls`]. Off when a call
    /// already placed it (the tunnels scene's `tunnels.view`, ADR-8825).
    pub place_camera: bool,
    /// Agent calls (method, params) made right after `render.out_start`, while the job runs (`play.hitch_report`
    /// for the job's per-pass GPU times); the answers are in [`Outcome::during`].
    pub during: Vec<(String, Value)>,
}

impl Default for JobSpec {
    fn default() -> Self {
        Self {
            width: 640,
            height: 360,
            frames: 1,
            warmup_frames: 0,
            plain: true,
            scatter_lod: 2,
            exposure_ev: None,
            repeat: 1,
            keep: None,
            keep_every_frame: false,
            scene: None,
            camera: None,
            hiz: false,
            gpu_driven: false,
            entity_scatter: false,
            camera_end: None,
            profile: None,
            tier: None,
            contact_ao: false,
            calls: Vec::new(),
            grass: false,
            after: Vec::new(),
            fps: 60,
            extra: Value::Null,
            offscreen_warm: false,
            rearm_path: false,
            place_camera: true,
            during: Vec::new(),
        }
    }
}

/// What a run produced.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub hashes: Vec<String>,
    pub sequence: String,
    pub distinct_pixel_values: usize,
    /// Wall time from `render.out_start` to `done` of the last run, milliseconds.
    pub job_ms: u128,
    /// The same for every run of a repeated job, in order.
    pub all_job_ms: Vec<u128>,
    /// The renderer's GPU pass timings of the last frame (`Debug` text; zeros without timestamp queries).
    pub gpu_timings: String,
    /// The first frame of the last run: width, height and RGBA8 pixels.
    pub first_frame: (u32, u32, Vec<u8>),
    /// Scene load time (`load_scene`), milliseconds.
    pub load_ms: u128,
    /// `render.memory_budget` after the last run.
    pub memory: Value,
    /// The sample's counters after the run.
    pub stats: Option<ShowcaseStats>,
    /// Peak resident memory of the test process (`VmHWM`), KiB; 0 where `/proc` is absent.
    pub peak_rss_kib: u64,
    /// The answers to [`JobSpec::after`], in order.
    pub reports: Vec<Value>,
    /// The `shot_render_out.json` manifest of every run of a repeated job, in order.
    pub manifests: Vec<Value>,
    /// The answers to [`JobSpec::calls`], in order.
    pub before: Vec<Value>,
    /// Per run of a repeated job: the longest wait for a `render.out_progress` answer while the job ran,
    /// and the longest gap between two answers, milliseconds (measured, never asserted).
    pub poll_gaps_ms: Vec<(u128, u128)>,
    /// With [`JobSpec::offscreen_warm`]: the last `render.pipeline_warm` answer (state `done`) and the
    /// longest wait for an answer while the warm ran, milliseconds (measured, never asserted).
    pub offscreen_warm: Option<(Value, u128)>,
    /// The answers to [`JobSpec::during`] of the last run, in order.
    pub during: Vec<Value>,
}

/// Peak resident memory of this process, KiB (Linux `/proc/self/status` `VmHWM`; 0 elsewhere).
pub fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

pub fn scene_dir() -> PathBuf {
    std::env::var_os("SHOWCASE_TEST_SCENE").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("example_scene"),
        PathBuf::from,
    )
}

fn call(client: &mut Client, method: &str, mut params: Value) -> Value {
    params["timeout_ms"] = json!(CALL_TIMEOUT_MS);
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

/// A call with exactly `params` (methods that validate their parameters strictly refuse the `timeout_ms` that
/// [`call`] adds). Without it the connection waits its default 30 s, which a cold headless host's first frames
/// can outlast under load, so `F0006` asks again: dispatch skips a request its connection gave up on.
fn call_exact(client: &mut Client, method: &str, params: Value) -> Value {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    loop {
        match client.call(method, params.clone()) {
            Ok(value) => return value,
            Err(ClientError::Rpc(err))
                if err.data.as_ref().and_then(|data| data.get("code")) == Some(&json!("F0006"))
                    && Instant::now() < deadline => {}
            Err(err) => panic!("{method} failed: {err:?}"),
        }
    }
}

fn read_token(agent_dir: &Path) -> String {
    std::fs::read_to_string(agent_dir.join("token"))
        .expect("token file")
        .trim()
        .to_string()
}

/// Waits until the renderer's boot warm and deferred pipeline builds are done, as a filming session
/// does before it starts a job (`render.pipeline_warm` -> `boot_background.idle`).
fn wait_for_warm(client: &mut Client) {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    loop {
        let warm = call(client, "render.pipeline_warm", json!({}));
        if warm["boot_background"]["idle"].as_bool().unwrap_or(false) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the pipeline warm did not finish: {warm}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn script(client: &mut Client, out: &Path, spec: &JobSpec) -> Outcome {
    wait_for_warm(client);
    let before: Vec<Value> = spec
        .calls
        .iter()
        .map(|(method, params)| call_exact(client, method, params.clone()))
        .collect();
    let mut poll_gaps_ms = Vec::new();
    let offscreen_warm = spec.offscreen_warm.then(|| {
        let ask = json!({ "offscreen": { "width": spec.width, "height": spec.height } });
        let deadline = Instant::now() + SCRIPT_TIMEOUT;
        let mut longest = 0_u128;
        let mut params = ask;
        loop {
            let asked = Instant::now();
            let warm = call(client, "render.pipeline_warm", params);
            longest = longest.max(asked.elapsed().as_millis());
            if warm["offscreen_warm"]["state"] == json!("done") {
                break (warm, longest);
            }
            assert!(
                Instant::now() < deadline,
                "the offscreen warm did not finish: {warm}"
            );
            params = json!({});
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    // The camera is placed with the agent protocol, as a filming session does.
    // A dolly (`camera_end`) plays on the game clock over the job's frames; a held camera stays at time 0.
    let seconds = if spec.camera_end.is_some() {
        spec.frames.max(1) as f64 / 60.0
    } else {
        1.0
    };
    let path = match spec.camera {
        Some((eye, target)) => json!({"interp": "Linear",
                 "keys": [{"time": 0, "position": eye}, {"time": seconds, "position": spec.camera_end.unwrap_or(eye)}],
                 "look_at": {"Point": target}}),
        None => json!({"interp": "Linear",
                 "keys": [{"time": 0, "position": [60, 46, -170]}, {"time": 1, "position": [62, 46, -168]}],
                 "look_at": {"Point": [0, 36, -100]}}),
    };
    let place = |client: &mut Client| {
        if !spec.place_camera {
            return;
        }
        if spec.rearm_path {
            // The shot player steps by the game clock's delta: paused, the path waits at its start
            // until the job takes the clock (and the job pauses it again when it hands it back).
            // The long answer wait: a cold headless host's next frame can hold the app thread.
            call(client, "play.pause", json!({}));
        }
        let placed = call(
            client,
            "camera.set_path",
            json!({ "path": path, "duration": seconds, "play": spec.camera_end.is_some() }),
        );
        assert!(placed["applied"].as_bool().unwrap_or(false), "{placed}");
    };
    place(client);
    let mut all_job_ms = Vec::new();
    let mut manifests = Vec::new();
    let mut last = None;
    let mut during = Vec::new();
    for repeat in 0..spec.repeat.max(1) {
        if spec.rearm_path && repeat > 0 {
            place(client);
        }
        let out = out.join(format!("r{repeat}"));
        let started_at = Instant::now();
        let mut params = json!({
            "out": out.display().to_string(), "name": "shot", "frames": spec.frames,
            "width": spec.width, "height": spec.height, "fps": spec.fps, "warmup_frames": spec.warmup_frames,
        });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), spec.extra.as_object()) {
            params.extend(extra.clone());
        }
        let started = call(client, "render.out_start", params);
        let job = started["job"].as_u64().expect("job id");
        during = spec
            .during
            .iter()
            .map(|(method, params)| call_exact(client, method, params.clone()))
            .collect();
        let deadline = Instant::now() + SCRIPT_TIMEOUT;
        let (mut longest_call, mut longest_gap) = (0_u128, 0_u128);
        let mut last_answer = Instant::now();
        let finished = loop {
            let asked = Instant::now();
            let progress = call(client, "render.out_progress", json!({ "job": job }));
            longest_call = longest_call.max(asked.elapsed().as_millis());
            longest_gap = longest_gap.max(last_answer.elapsed().as_millis());
            last_answer = Instant::now();
            if progress["status"] != json!("running") {
                break progress;
            }
            assert!(Instant::now() < deadline, "the render-out job did not finish");
            std::thread::sleep(Duration::from_millis(20));
        };
        all_job_ms.push(started_at.elapsed().as_millis());
        poll_gaps_ms.push((longest_call, longest_gap));
        assert_eq!(finished["status"], json!("done"), "{finished}");
        assert_eq!(finished["frame"], json!(spec.frames));
        // Hashes come in windows of up to 4096.
        let mut hashes: Vec<String> = Vec::new();
        while (hashes.len() as u64) < spec.frames {
            let window = call(
                client,
                "render.out_progress",
                json!({ "job": job, "hashes": true, "hashes_from": hashes.len() }),
            );
            let got: Vec<String> = window["hashes"]["hashes"]
                .as_array()
                .expect("hashes")
                .iter()
                .map(|h| h.as_str().expect("hex").to_string())
                .collect();
            assert!(!got.is_empty(), "no more hashes at {}", hashes.len());
            hashes.extend(got);
        }
        assert_eq!(hashes.len() as u64, spec.frames);
        let sequence = finished["sequence_hash"].as_str().expect("sequence").to_string();
        let manifest = out.join("shot_render_out.json");
        let manifest = std::fs::read(&manifest).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
        manifests.push(serde_json::from_slice(&manifest).expect("manifest json"));
        last = Some((hashes, sequence, out));
    }
    let reports: Vec<Value> = spec
        .after
        .iter()
        .map(|(method, params)| call_exact(client, method, params.clone()))
        .collect();
    let memory = client
        .call("render.memory_budget", json!({ "timeout_ms": CALL_TIMEOUT_MS }))
        .unwrap_or(Value::Null);
    let (hashes, sequence, out) = last.expect("at least one run");
    let job_ms = all_job_ms.last().copied().unwrap_or(0);

    // The first frame is a PNG with more than a few pixel values: a scene was drawn.
    let file = out.join("shot_000000.png");
    let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let (frame_w, frame_h) = (reader.info().width, reader.info().height);
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("png data");
    assert_eq!(buf.len(), (frame_w * frame_h * 4) as usize, "RGBA8 frames");
    let distinct: std::collections::HashSet<[u8; 3]> = buf
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| [p[0], p[1], p[2]])
        .collect();
    Outcome {
        hashes,
        sequence,
        distinct_pixel_values: distinct.len(),
        job_ms,
        all_job_ms,
        gpu_timings: String::new(),
        first_frame: (frame_w, frame_h, buf),
        load_ms: 0,
        memory,
        stats: None,
        peak_rss_kib: 0,
        reports,
        manifests,
        before,
        poll_gaps_ms,
        offscreen_warm,
        during,
    }
}

/// Runs one fresh headless app through one job.
pub fn run(run_id: u32, spec: &JobSpec) -> Outcome {
    run_with(run_id, spec, |_| {})
}

/// [`run`], with `each_update` called after every app update (to read or change the app between a job's frames).
pub fn run_with(run_id: u32, spec: &JobSpec, mut each_update: impl FnMut(&mut regia_app::App)) -> Outcome {
    let project = unique_temp_dir(&format!("showcase-render-{run_id}"));
    let out = project.join("out");
    let load_started = Instant::now();
    let scene = Arc::new(
        load_scene_with(
            &spec.scene.clone().unwrap_or_else(scene_dir),
            &FeatureOptions { grass: spec.grass },
        )
        .expect("scene"),
    );
    let load_ms = load_started.elapsed().as_millis();
    let has_scatter = scene.scatter.is_some();
    let mut app = build_headless_app(
        scene,
        Options {
            size: (spec.width, spec.height),
            plain: spec.plain,
            scatter_lod: spec.scatter_lod,
            exposure_ev: spec.exposure_ev,
            hiz: spec.hiz,
            gpu_driven: spec.gpu_driven,
            entity_scatter: spec.entity_scatter,
            profile: spec.profile,
            tier: spec.tier,
            contact_ao: spec.contact_ao,
            foliage_wind: false,
            ambience: false,
            ambient_life: false,
        },
        Some(project.clone()),
    )
    .expect("headless app");

    {
        let device = app.world().resource::<regia_render::RenderDevice>().clone();
        app.world_mut()
            .resource_mut::<regia_render::RuntimeRenderer>()
            .0
            .set_gpu_timing(&device, true);
    }
    let ok = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ok);
    let slot = Arc::new(std::sync::Mutex::new(None));
    let result = Arc::clone(&slot);
    let agent_dir = endpoint::agent_dir(&project);
    let job_spec = spec.clone();
    let worker = std::thread::Builder::new()
        .name(format!("showcase-render-{run_id}"))
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
            *result.lock().expect("slot") = Some(script(&mut client, &out, &job_spec));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !worker.is_finished() && Instant::now() < deadline {
        app.update();
        each_update(&mut app);
    }
    worker.join().expect("script finished");
    let stats = app.world().resource::<ShowcaseStats>().clone();
    assert!(
        stats.terrain_installed,
        "the terrain draw was installed in the renderer: {:?}",
        stats.terrain_error
    );
    assert!(
        !has_scatter || stats.scatter_instances > 0,
        "scatter instances exist"
    );
    for problem in app
        .world()
        .resource::<regia_render::RuntimeRenderer>()
        .0
        .frame_problems()
    {
        eprintln!("frame problem {}: {}", problem.code, problem.message);
    }
    let selected = app.world().resource::<TerrainStatsHandle>().0.selected_nodes();
    assert!(selected > 0, "the terrain draw selected nodes");
    let mut outcome = slot.lock().expect("slot").take().expect("scenario ran");
    outcome.load_ms = load_ms;
    outcome.stats = Some(stats.clone());
    outcome.peak_rss_kib = peak_rss_kib();
    outcome.gpu_timings = format!(
        "{:?}",
        app.world()
            .resource::<regia_render::RuntimeRenderer>()
            .0
            .gpu_timings()
    );
    if let Some(keep) = &spec.keep {
        let _ = std::fs::create_dir_all(keep);
        let last_run = format!("r{}", spec.repeat.max(1) - 1);
        let last = spec.frames.saturating_sub(1);
        let kept: Vec<u64> = if spec.keep_every_frame {
            (0..=last).collect()
        } else {
            vec![0, last]
        };
        for index in kept {
            let name = format!("shot_{index:06}.png");
            let _ = std::fs::copy(project.join("out").join(&last_run).join(&name), keep.join(&name));
        }
    }
    let _ = std::fs::remove_dir_all(&project);
    outcome
}

/// Which optional features an [`overlay_scene`] switches on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Overlay {
    /// `terrain/grass.ron` with the default file.
    pub grass: bool,
    /// A `mist:` block in `sky.ron` (the source's own, when it has one).
    pub mist: bool,
    /// `terrain/stream.json` (the source's; nothing when it has none).
    pub water: bool,
    /// `entry.ron` (the source's own, else a procedural track).
    pub entry: bool,
}

/// `sky` (a `sky.ron` text) without its `mist:` entry, however many lines the entry spans: it
/// ends where its parentheses close, and its trailing comma and line end go with it. A sky without
/// a `mist:` entry comes back unchanged.
pub fn strip_mist(sky: &str) -> String {
    let Some(at) = sky.find("mist:") else {
        return sky.to_string();
    };
    let mut depth = 0u32;
    let mut stop = sky.len();
    for (i, c) in sky[at..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    stop = at + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let rest = &sky[stop..];
    let rest = rest.strip_prefix(',').unwrap_or(rest);
    let rest = match rest.find('\n') {
        Some(n) if rest[..n].trim().is_empty() => &rest[n + 1..],
        _ => rest,
    };
    let mut out = String::with_capacity(sky.len());
    out.push_str(&sky[..at]);
    out.push_str(rest);
    out
}

/// A directory of symlinks onto the scene `src` (which stays read-only) with the optional feature files of
/// `on` added and the feature files `on` leaves out removed.
#[cfg(unix)]
pub fn overlay_scene(src: &Path, tag: &str, on: Overlay) -> PathBuf {
    use std::os::unix::fs::symlink;
    let dir = unique_temp_dir(tag);
    for entry in std::fs::read_dir(src).expect("scene dir") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        if matches!(name.to_str(), Some("terrain" | "sky.ron" | "entry.ron")) {
            continue;
        }
        symlink(entry.path(), dir.join(&name)).expect("link");
    }
    let terrain = dir.join("terrain");
    std::fs::create_dir_all(&terrain).expect("terrain dir");
    for entry in std::fs::read_dir(src.join("terrain")).expect("terrain") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        if matches!(name.to_str(), Some("grass.ron" | "stream.json")) {
            continue;
        }
        symlink(entry.path(), terrain.join(&name)).expect("link");
    }
    if on.grass {
        std::fs::write(terrain.join("grass.ron"), "Grass(version: 1)\n").expect("grass.ron");
    }
    if on.water && src.join("terrain/stream.json").is_file() {
        symlink(src.join("terrain/stream.json"), terrain.join("stream.json")).expect("link");
    }
    let mut sky = std::fs::read_to_string(src.join("sky.ron")).expect("sky.ron");
    if on.mist && !sky.contains("mist:") {
        let end = sky.rfind(')').expect("a closing paren");
        sky.insert_str(end, "    mist: Some((density: 0.012)),\n");
    } else if !on.mist {
        // The source may have a mist block: this configuration has no mist.
        sky = strip_mist(&sky);
    }
    std::fs::write(dir.join("sky.ron"), sky).expect("sky.ron");
    if on.entry {
        if src.join("entry.ron").is_file() {
            symlink(src.join("entry.ron"), dir.join("entry.ron")).expect("link");
        } else {
            std::fs::write(
                dir.join("entry.ron"),
                "Entry(version: 1, points: [(time: 0.0, position: (3000.0, 60000.0, 3000.0)), (time: 3.0, position: (300.0, 300.0, -300.0))])\n",
            )
            .expect("entry.ron");
        }
    }
    dir
}

/// Writes a closed box glTF (8 shared vertices, 12 triangles wound outward, one material of `color`, single-sided)
/// at `dir/rel` with its `.bin` beside it. `half` is the half size along each axis, metres, centred on the origin.
pub fn write_box_gltf(dir: &Path, rel: &str, half: [f32; 3], color: [f32; 3]) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let corner = |i: usize| -> [f32; 3] {
        [0, 1, 2].map(|axis| {
            if (i >> axis) & 1 == 1 {
                half[axis]
            } else {
                -half[axis]
            }
        })
    };
    let faces: [[usize; 4]; 6] = [
        [0, 1, 3, 2],
        [4, 5, 7, 6],
        [0, 1, 5, 4],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 3, 7, 5],
    ];
    let mut indices: Vec<u16> = Vec::new();
    for [a, b, c, d] in faces {
        for [x, y, z] in [[a, b, c], [a, c, d]] {
            let (pa, pb, pc) = (corner(x), corner(y), corner(z));
            let e1 = [pb[0] - pa[0], pb[1] - pa[1], pb[2] - pa[2]];
            let e2 = [pc[0] - pa[0], pc[1] - pa[1], pc[2] - pa[2]];
            let normal = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            // The origin is inside the box, so an outward face has its normal along the vertex position.
            let outward = normal[0] * pa[0] + normal[1] * pa[1] + normal[2] * pa[2] > 0.0;
            let tri = if outward { [x, y, z] } else { [x, z, y] };
            indices.extend(tri.map(|i| i as u16));
        }
    }
    let mut bin: Vec<u8> = Vec::new();
    for i in 0..8 {
        for v in corner(i) {
            bin.extend_from_slice(&v.to_le_bytes());
        }
    }
    for i in 0..8 {
        let c = corner(i);
        let length = (c[0] * c[0] + c[1] * c[1] + c[2] * c[2]).sqrt();
        for v in c {
            bin.extend_from_slice(&(v / length).to_le_bytes());
        }
    }
    for i in &indices {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    let stem = path.file_stem().expect("stem").to_string_lossy().into_owned();
    std::fs::write(path.with_file_name(format!("{stem}.bin")), &bin).expect("bin");
    let json = format!(
        r#"{{"asset": {{"version": "2.0"}}, "scene": 0, "scenes": [{{"nodes": [0]}}],
  "nodes": [{{"name": "{stem}", "mesh": 0}}],
  "materials": [{{"pbrMetallicRoughness": {{"baseColorFactor": [{r}, {g}, {b}, 1.0], "metallicFactor": 0.0, "roughnessFactor": 0.8}}}}],
  "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0, "NORMAL": 1}}, "indices": 2, "material": 0}}]}}],
  "buffers": [{{"uri": "{stem}.bin", "byteLength": {len}}}],
  "bufferViews": [{{"buffer": 0, "byteOffset": 0, "byteLength": 96}}, {{"buffer": 0, "byteOffset": 96, "byteLength": 96}},
                  {{"buffer": 0, "byteOffset": 192, "byteLength": 72}}],
  "accessors": [{{"bufferView": 0, "componentType": 5126, "count": 8, "type": "VEC3", "min": [{nx}, {ny}, {nz}], "max": [{px}, {py}, {pz}]}},
                {{"bufferView": 1, "componentType": 5126, "count": 8, "type": "VEC3"}},
                {{"bufferView": 2, "componentType": 5123, "count": 36, "type": "SCALAR"}}]}}"#,
        r = color[0],
        g = color[1],
        b = color[2],
        len = bin.len(),
        nx = -half[0],
        ny = -half[1],
        nz = -half[2],
        px = half[0],
        py = half[1],
        pz = half[2],
    );
    std::fs::write(&path, json).expect("gltf");
}
