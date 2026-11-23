// SPDX-License-Identifier: MPL-2.0
//! Offline fixed-dt render-out (RR-F4): two fresh headless runs of the same request give the same
//! frame hashes (lavapipe), the clock moves the scene one fixed step per frame, supersampling,
//! cuts and warm-up run, the files and the manifest are written, a job can be cancelled, and a
//! movie format either works (ffmpeg on PATH) or fails with F0019. Two jobs in one app start alike
//! whatever the app did between them: the clocks, the fixed loop and the temporal render state are
//! the job's own (ADR-1881).

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
    MeshLodChain, MeshLodLevel, MeshLodScales, MeshRenderer, PhysicalCamera, PostSettings, RenderPlugin,
    RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only (the app stops stepping past it): a loaded software rasterizer builds an
/// inline material graph and two jobs in minutes.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);
const CALL_TIMEOUT_MS: u64 = 60_000;
/// Yaw added each fixed step.
const SPIN_STEP: f32 = 0.35;
const FOV_Y: f32 = 0.9;

#[derive(Resource)]
struct Spin(f32);

#[derive(Component)]
struct Spinner;

fn spin_mesh(
    mut spin: ResMut<Spin>,
    mut query: Query<(&mut Transform, &mut GlobalTransform), With<Spinner>>,
) {
    spin.0 += SPIN_STEP;
    let rotation = regia_core::math::Quat::from_rotation_y(spin.0);
    for (mut transform, mut global) in &mut query {
        transform.rotation = rotation;
        global.0 = transform.compute_affine();
    }
}

fn triangle() -> Arc<CpuMesh> {
    Arc::new(CpuMesh {
        vertices: vec![
            LabVertex::from_pos_normal_uv([-0.95, -0.75, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.95, -0.75, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
            LabVertex::from_pos_normal_uv([0.0, 0.95, 0.0], [0.0, 0.0, 1.0], [0.5, 1.0]),
        ],
        indices: vec![0, 1, 2],
    })
}

fn deterministic_settings() -> RenderSettings {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    settings.profile.depth_prepass = false;
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

fn ffmpeg_on_path() -> bool {
    let Some(path) = regia_core::brand::env_var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("ffmpeg").is_file() || dir.join("ffmpeg.exe").is_file())
}

/// Polls `render.out_progress` until `done` says so.
fn wait_for(client: &mut Client, job: u64, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(
            client,
            "render.out_progress",
            json!({ "job": job, "hashes": true }),
        );
        if done(&progress) {
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not reach the wanted state");
}

#[derive(Clone, Debug, PartialEq)]
struct Outcome {
    hashes: Vec<String>,
    sequence: String,
    file_sizes: Vec<u64>,
}

fn scenario(client: &mut Client, out: &Path) -> Outcome {
    // A: PNG sequence, 2x supersampled, a cut at frame 2, one warm-up frame.
    let started = call(
        client,
        "render.out_start",
        json!({
            "out": out.display().to_string(), "name": "shot", "frames": 4, "width": 64, "height": 48,
            "fps": 60, "supersample": 2, "cuts": [2], "warmup_frames": 1,
        }),
    );
    assert_eq!(started["render_size"], json!([128, 96]));
    assert_eq!(started["request"]["step_ns"], json!(16_666_667));
    let job = started["job"].as_u64().expect("job id");

    // One job at a time.
    let second = client.call(
        "render.out_start",
        json!({ "out": out.display().to_string(), "frames": 1 }),
    );
    assert!(second.is_err(), "a second job while one runs is refused (F0018)");

    let finished = wait_for(client, job, |p| p["status"] != json!("running"));
    assert_eq!(finished["status"], json!("done"), "{finished}");
    assert_eq!(finished["frame"], json!(4));
    assert!(
        finished["camera_cuts"].as_u64().is_some_and(|n| n >= 2),
        "frame 0 and the requested cut: {finished}"
    );
    let hashes: Vec<String> = finished["hashes"]["hashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|h| h.as_str().expect("hex").to_string())
        .collect();
    assert_eq!(hashes.len(), 4);
    assert_ne!(hashes[0], hashes[3], "the clock moved the scene between frames");
    let sequence = finished["sequence_hash"].as_str().expect("sequence").to_string();

    let mut file_sizes = Vec::new();
    for index in 0..4 {
        let file: PathBuf = out.join(format!("shot_{index:06}.png"));
        let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "{}", file.display());
        file_sizes.push(bytes.len() as u64);
    }
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("shot_render_out.json")).expect("manifest"))
            .expect("json");
    assert_eq!(manifest["status"], json!("done"));
    assert_eq!(manifest["sequence_hash"], json!(sequence));
    assert_eq!(manifest["frame_hashes"].as_array().map(Vec::len), Some(4));

    // B: cancel a long job.
    let long = call(
        client,
        "render.out_start",
        json!({ "out": out.join("long").display().to_string(), "frames": 100_000, "width": 32, "height": 32 }),
    );
    let long_id = long["job"].as_u64().expect("job id");
    wait_for(client, long_id, |p| p["frame"].as_u64().is_some_and(|f| f >= 1));
    let cancelling = call(client, "render.out_cancel", json!({}));
    assert_eq!(cancelling["status"], json!("cancelling"));
    let cancelled = wait_for(client, long_id, |p| p["status"] == json!("cancelled"));
    assert!(cancelled["frame"].as_u64().is_some_and(|f| f < 100_000));
    // Cancelling again is idempotent (ADR-1965): the finished job answers its final status, by
    // id and without one; only an unknown id is NOT_FOUND.
    for params in [json!({}), json!({ "job": long_id })] {
        let again = call(client, "render.out_cancel", params);
        assert_eq!(again["job"], json!(long_id), "{again}");
        assert_eq!(again["status"], json!("cancelled"), "{again}");
        assert_eq!(again["finished"], json!(true), "{again}");
    }
    assert!(
        client
            .call(
                "render.out_cancel",
                json!({ "job": long_id + 1000, "timeout_ms": CALL_TIMEOUT_MS })
            )
            .is_err(),
        "an unknown job is NOT_FOUND"
    );

    // C: a movie needs ffmpeg; without it the error is F0019, with it the file appears.
    let movie = client.call(
        "render.out_start",
        json!({
            "out": out.join("movie").display().to_string(), "format": "ffv1", "frames": 3,
            "width": 64, "height": 48, "fps": 60, "timeout_ms": CALL_TIMEOUT_MS,
        }),
    );
    if ffmpeg_on_path() {
        let movie = movie.expect("ffv1 job starts");
        let id = movie["job"].as_u64().expect("job id");
        let done = wait_for(client, id, |p| p["status"] != json!("running"));
        assert_eq!(done["status"], json!("done"), "{done}");
        assert!(out.join("movie").join("frame.mkv").is_file());
    } else {
        let error = movie.expect_err("no ffmpeg");
        assert!(format!("{error:?}").contains("F0019"), "{error:?}");
    }
    Outcome {
        hashes,
        sequence,
        file_sizes,
    }
}

/// The binary's one device (ADR-7557), inserted into every app. Without one, the agent opened
/// a device per app at its first capture (`seeing::headless_render_device`, a new device outside
/// regia_agent's own unit tests) and dropped it with the app, so a device was destroyed while
/// other test threads named objects: the Vulkan loader list race of ADR-2113. Every app still has
/// its own renderer, so "fresh run" still means a fresh app and renderer.
/// `two_jobs_draw_a_material_graph_alike_from_its_first_frame` keeps its cold start on this
/// device: its graph is drawn by no other test, so its pipelines are not built before its job 1.
fn shared_device() -> regia_render::RenderDevice {
    match regia_render::RenderDevice::shared_headless() {
        Ok(device) => device,
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// Whether this host has a headless adapter. Without one the GPU tests skip with a printed line, as they did
/// before the shared device; a run that expects a GPU sets `REGIA_REQUIRE_ADAPTER` (ADR-7613) and then
/// `RenderDevice::shared_headless` panics on its own (regia_render's `require_adapter`), so a lost card
/// stays a failure. Any other device error is still a failure.
fn adapter_available() -> bool {
    match regia_render::RenderDevice::shared_headless() {
        Ok(_) => true,
        Err(regia_render::RenderError::NoAdapter(err)) => {
            // Straight to stderr: libtest's capture only holds `eprintln!` of a passing test.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "skipped: no headless GPU adapter on this host ({err}); set REGIA_REQUIRE_ADAPTER=1 to fail instead"
            );
            false
        }
        Err(err) => panic!("no headless GPU (lavapipe?): {err}"),
    }
}

/// A headless app with the test's camera and mesh, its clock paused until a job takes it.
fn scene_app(project: &Path, settings: RenderSettings, exposure_ev: Option<f32>) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.to_path_buf()),
    })
    .insert_resource(shared_device());
    enable_fixed_step(&mut app, 60.0);
    // Paused until the job takes the clock, so the spin does not depend on the client's speed.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(Spin(0.0));
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
            exposure_ev,
            fov_y: FOV_Y,
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    let mesh_tf = Transform::default();
    app.world_mut().spawn((
        mesh_tf,
        GlobalTransform(mesh_tf.compute_affine()),
        Spinner,
        MeshRenderer::new(
            triangle(),
            Arc::new(CpuMaterial {
                base_color: [0.85, 0.42, 0.12, 1.0],
                roughness: 0.55,
                metallic: 0.0,
                ..CpuMaterial::default()
            }),
        ),
        DirectionalLight {
            direction: [0.15, -0.45, -1.0],
            color: [1.0, 0.96, 0.9],
            illuminance: 90_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));
    app
}

/// Runs `app` while `script` drives it through a client, and returns what the script returns.
fn drive<T: Send + 'static>(
    app: &mut App,
    project: &Path,
    script: impl FnOnce(&mut Client) -> T + Send + 'static,
) -> T {
    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let slot = Arc::new(std::sync::Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(project);
    let thread = std::thread::Builder::new()
        .name("render-out-script".to_string())
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
            *result.lock().expect("slot") = Some(script(&mut client));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    let outcome = slot.lock().expect("slot").take();
    outcome.expect("the script ran")
}

fn run(run_id: u32) -> Outcome {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-{run_id}"));
    let out = project.join("out");
    let mut app = scene_app(&project, deterministic_settings(), Some(14.0));
    app.add_systems(FixedUpdate, spin_mesh);
    let outcome = drive(&mut app, &project, move |client| scenario(client, &out));
    let _ = std::fs::remove_dir_all(&project);
    outcome
}

/// Poses the spinner from the clocks alone, the way a scene animates on `Time::elapsed`: `Update` on
/// the game clock around one axis, the fixed loop on the simulation clock around another.
#[derive(Resource, Default, Clone, Copy)]
struct ClockPose {
    game: f32,
    sim: f32,
}

fn pose_from_game_clock(time: Res<Time>, mut pose: ResMut<ClockPose>) {
    pose.game = time.elapsed_secs();
}

fn pose_from_fixed_clock(time: Res<Time>, mut pose: ResMut<ClockPose>) {
    pose.sim = time.elapsed_secs();
}

/// Where the camera starts and how fast it backs away on the game clock, metres (per second).
const DOLLY_START: f32 = 2.6;
const DOLLY_SPEED: f32 = 6.0;

/// What [`apply_clock_pose`] writes.
type Pose = (&'static mut Transform, &'static mut GlobalTransform);

fn apply_clock_pose(
    pose: Res<ClockPose>,
    mut spinners: Query<Pose, (With<Spinner>, Without<Camera>)>,
    mut cameras: Query<Pose, With<Camera>>,
) {
    let rotation = regia_core::math::Quat::from_rotation_y(pose.game * 2.5)
        * regia_core::math::Quat::from_rotation_x(pose.sim * 1.5);
    for (mut transform, mut global) in &mut spinners {
        transform.rotation = rotation;
        global.0 = transform.compute_affine();
    }
    for (mut transform, mut global) in &mut cameras {
        transform.translation.z = DOLLY_START + pose.game * DOLLY_SPEED;
        global.0 = transform.compute_affine();
    }
}

/// A small quad with two LODs (the coarse one drops a corner), switching just below its size at the
/// camera's start: inside the hysteresis band there, so a fresh pick is the fine level and a level
/// held from a frame further away is the coarse one. The dolly carries it through the switch.
fn spawn_lod_quad(app: &mut App, profile: &regia_render::RenderProfile) {
    const RADIUS: f32 = 0.2;
    let at = regia_core::math::Vec3::new(0.9, 0.6, 0.5);
    let vertex = |x: f32, y: f32| LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
    let distance = (at - regia_core::math::Vec3::new(0.0, 0.0, DOLLY_START)).length();
    let size =
        RADIUS / (distance * (FOV_Y * 0.5).tan()) * MeshLodScales::of_profile(profile).color_factor(1.0);
    let transform = Transform::from_translation(at);
    app.world_mut().spawn((
        transform,
        GlobalTransform(transform.compute_affine()),
        MeshRenderer::new(
            Arc::new(CpuMesh {
                vertices: vec![
                    vertex(-0.14, -0.14),
                    vertex(0.14, -0.14),
                    vertex(0.14, 0.14),
                    vertex(-0.14, 0.14),
                ],
                indices: vec![0, 1, 2, 0, 2, 3],
            }),
            Arc::new(CpuMaterial {
                base_color: [0.1, 0.8, 0.3, 1.0],
                ..CpuMaterial::default()
            }),
        ),
        MeshLodChain {
            levels: vec![
                MeshLodLevel {
                    indices: Arc::new(vec![0, 1, 2, 0, 2, 3]),
                    screen_size: size / 1.05,
                },
                MeshLodLevel {
                    indices: Arc::new(vec![0, 1, 2]),
                    screen_size: 0.0,
                },
            ],
            bounds_center: [0.0; 3],
            bounds_radius: RADIUS,
        },
    ));
}

/// One job's repeatable parts: frames, AOVs and where the app's clocks stood.
fn clocked_job(client: &mut Client, out: &Path, extra: Value) -> (Value, Value) {
    let mut params = json!({
        "out": out.display().to_string(), "name": "shot", "frames": 6, "width": 64, "height": 48,
        "fps": 24, "cuts": [3], "warmup_frames": 2, "settle_frames": 1,
        "aovs": ["depth", "id", "motion"], "aov_format": "raw",
    });
    params
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    let started = call(client, "render.out_start", params);
    let job = started["job"].as_u64().expect("job id");
    let done = wait_for(client, job, |p| p["status"] != json!("running"));
    assert_eq!(done["status"], json!("done"), "{done}");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("shot_render_out.json")).expect("manifest"))
            .expect("json");
    let frames = json!({
        "frames": manifest["frame_hashes"],
        "sequence": manifest["sequence_hash"],
        "aov": manifest["aov"]["frame_hashes"],
        "ids": manifest["aov"]["ids"],
    });
    (frames, manifest["clock"].clone())
}

/// Two jobs in one app, with the app stepped on between them, on a scene posed from the clocks, with
/// TAA, auto exposure, bloom and motion blur on.
fn two_jobs_in_one_app(extra: Value) {
    let project = regia_test_utils::unique_temp_dir("render-out-two-jobs");
    let out = project.join("out");
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.profile.anti_aliasing = AntiAliasing::Taa;
    settings.profile.auto_exposure = true;
    settings.profile.motion_blur = true;
    let profile = settings.profile.clone();
    let mut app = scene_app(&project, settings, None);
    spawn_lod_quad(&mut app, &profile);
    // More than one fixed step a frame, so a frame's remainder stays banked.
    app.world_mut().resource_mut::<FixedTime>().max_substeps = 8;
    let cameras: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<Camera>>()
        .iter(app.world())
        .collect();
    for camera in cameras {
        app.world_mut().entity_mut(camera).insert(PhysicalCamera {
            shutter_angle: 0.5,
            motion_blur_samples: 8,
            ..PhysicalCamera::default()
        });
    }
    app.init_resource::<ClockPose>();
    app.add_systems(FixedUpdate, pose_from_fixed_clock);
    app.add_systems(Update, (pose_from_game_clock, apply_clock_pose).chain());
    drive(&mut app, &project, move |client| {
        let (first, first_clock) = clocked_job(client, &out.join("a"), extra.clone());
        let frames = first["frames"].as_array().expect("hashes");
        assert_ne!(frames.first(), frames.last(), "the clocks moved the scene");
        // Another job and a stretch of play leave the clocks, the fixed loop's banked time, the frame
        // counters and every temporal history somewhere else.
        call(client, "play.step", json!({ "frames": 7 }));
        let _ = clocked_job(
            client,
            &out.join("other"),
            json!({ "frames": 5, "fps": 25, "cuts": [] }),
        );
        call(client, "play.step", json!({ "frames": 3 }));
        let (second, second_clock) = clocked_job(client, &out.join("b"), extra);
        assert!(
            second_clock["app_frame_at_start"].as_u64() > first_clock["app_frame_at_start"].as_u64(),
            "{first_clock} {second_clock}"
        );
        assert!(
            second_clock["app_game_time_at_start_ns"].as_u64()
                > first_clock["app_game_time_at_start_ns"].as_u64(),
            "{first_clock} {second_clock}"
        );
        assert_eq!(first, second, "the same request in one app gives the same frames");
    });
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn two_jobs_in_one_app_give_the_same_frames_and_aovs() {
    if !adapter_available() {
        return;
    }
    two_jobs_in_one_app(json!({}));
}

#[test]
fn two_jobs_with_a_shutter_and_a_time_map_give_the_same_frames() {
    if !adapter_available() {
        return;
    }
    two_jobs_in_one_app(json!({
        "subframes": 3,
        "time_map": [{ "frame": 0, "scale": 0.5 }, { "frame": 4, "scale": 1.5 }],
    }));
}

#[test]
fn two_fresh_runs_give_identical_frame_hashes_and_files() {
    if !adapter_available() {
        return;
    }
    let a = run(1);
    // `loader_list_lock_count` counts every instance and device creation: the second fresh run
    // renders on the binary's one device and creates neither (ADR-7557).
    let creations = regia_render::loader_list_lock_count();
    let b = run(2);
    assert_eq!(
        regia_render::loader_list_lock_count(),
        creations,
        "the second fresh run created an instance or a device"
    );
    assert_eq!(a.hashes, b.hashes, "frame hashes");
    assert_eq!(a.sequence, b.sequence, "sequence hash");
    assert_eq!(a.file_sizes, b.file_sizes, "PNG sizes");
}

/// Red-Rouge v0.2.13 (ADR-2582): the first `render.out_progress` after a job started waited 30 s
/// and got `F0006`, because a cold host's first render holds the app thread while it compiles
/// pipelines. Here the job's HUD hook (it runs inside the job's frame step) holds the app thread
/// until the script lets it go: the poll sent meanwhile is answered from the progress board (frame
/// 0, `warming`, `app_busy`) while the app thread is provably still inside the step, and once the
/// step ends the app answers as before.
#[test]
fn a_poll_during_a_long_step_is_answered_while_the_app_thread_is_held() {
    if !adapter_available() {
        return;
    }
    use std::sync::Mutex;
    use std::sync::mpsc;
    let project = regia_test_utils::unique_temp_dir("render-out-busy-poll");
    let out = project.join("out");
    let mut app = scene_app(&project, deterministic_settings(), Some(14.0));
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(Some((entered_tx, release_rx))));
    let hold = Arc::clone(&gate);
    app.insert_resource(regia_render::RenderOutHud::new(
        Arc::new(move |_: &mut World, _: &regia_render::RenderOutSpec| {
            // The first frame only; bounded, so a broken board fails the test instead of hanging it.
            let first = hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some((entered, release)) = first {
                let _ = entered.send(());
                let _ = release.recv_timeout(SCRIPT_TIMEOUT);
            }
        }),
        Arc::new(|_: &mut World, _: &regia_render::RenderOutSpec| {}),
    ));
    let waited = drive(&mut app, &project, move |client| {
        let started = call(
            client,
            "render.out_start",
            json!({ "out": out.display().to_string(), "name": "shot", "frames": 2, "width": 32,
                    "height": 24, "fps": 24, "warmup_frames": 0 }),
        );
        let job = started["job"].as_u64().expect("job id");
        assert_eq!(started["hud"], json!(true), "{started}");
        entered_rx
            .recv_timeout(SCRIPT_TIMEOUT)
            .expect("the job's first frame reached the HUD hook");
        let asked = Instant::now();
        let busy = call(
            client,
            "render.out_progress",
            json!({ "job": job, "hashes": true }),
        );
        let waited = asked.elapsed();
        assert_eq!(busy["app_busy"], json!(true), "answered from the board: {busy}");
        assert_eq!(busy["job"], json!(job), "{busy}");
        assert_eq!(busy["status"], json!("running"), "{busy}");
        assert_eq!(busy["phase"], json!("rendering"), "{busy}");
        assert_eq!(busy["frame"], json!(0), "{busy}");
        assert_eq!(
            busy["warming"],
            json!(true),
            "no render of the job has finished: {busy}"
        );
        assert_eq!(busy["hashes"]["hashes"], json!([]), "{busy}");
        assert!(busy["busy_ms"].is_u64(), "{busy}");
        release_tx.send(()).expect("release the app thread");
        let done = wait_for(client, job, |p| p["status"] != json!("running"));
        assert_eq!(done["status"], json!("done"), "{done}");
        assert_eq!(
            done["app_busy"],
            Value::Null,
            "the app answers once the step ended: {done}"
        );
        assert_eq!(done["frame"], json!(2));
        waited
    });
    // Not asserted (no wall-clock budgets in tests): the board answers after `BOARD_ANSWER_AFTER`.
    println!(
        "a poll during a held step was answered in {} ms (board after {} ms)",
        waited.as_millis(),
        regia_agent::BOARD_ANSWER_AFTER.as_millis()
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// A material graph that shades red; its standard-surface fallback (the material's base color) is green.
const RED_GRAPH: &str = r"
fn material_surface(in: regia::pbr::surface::SurfaceInput) -> regia::pbr::surface::Surface {
    return regia::pbr::surface::Surface(
        vec3<f32>(0.9, 0.05, 0.05),
        0.6,
        0.0,
        1.0,
        in.world_normal,
        1.0,
        vec3<f32>(0.0),
        0.0,
    );
}
";

/// Red-Rouge v0.2.13 on an RTX 5090 (ADR-2582): work the runtime renderer moves off the frame landed
/// partway through the first of two identical jobs and stayed for the second. A material graph shows
/// it on any device: a headless host draws nothing outside a job, so the graph is first drawn on job
/// 1's frame 0, where the runtime renderer queued its pipelines on a worker and drew the green
/// fallback, while job 2 found them built. With complete frames both jobs draw the graph.
#[test]
fn two_jobs_draw_a_material_graph_alike_from_its_first_frame() {
    if !adapter_available() {
        return;
    }
    let project = regia_test_utils::unique_temp_dir("render-out-graph-repeat");
    let out = project.join("out");
    let mut app = scene_app(&project, deterministic_settings(), Some(14.0));
    let quad = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, 0.5));
    let vertex = |x: f32, y: f32| LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]);
    app.world_mut().spawn((
        quad,
        GlobalTransform(quad.compute_affine()),
        MeshRenderer::new(
            Arc::new(CpuMesh {
                vertices: vec![
                    vertex(-0.4, -0.4),
                    vertex(0.4, -0.4),
                    vertex(0.4, 0.4),
                    vertex(-0.4, 0.4),
                ],
                indices: vec![0, 1, 2, 0, 2, 3],
            }),
            Arc::new(CpuMaterial {
                base_color: [0.05, 0.9, 0.05, 1.0],
                surface_graph: Some(Arc::new(regia_render::SurfaceGraph::new(RED_GRAPH))),
                ..CpuMaterial::default()
            }),
        ),
    ));
    let (first, second) = drive(&mut app, &project, move |client| {
        let job = |client: &mut Client, dir: &str| {
            let started = call(
                client,
                "render.out_start",
                json!({ "out": out.join(dir).display().to_string(), "name": "shot", "frames": 3,
                        "width": 48, "height": 32, "fps": 24, "warmup_frames": 0, "hud": false }),
            );
            let id = started["job"].as_u64().expect("job id");
            let done = wait_for(client, id, |p| p["status"] != json!("running"));
            assert_eq!(done["status"], json!("done"), "{done}");
            done["hashes"]["hashes"].clone()
        };
        (job(client, "a"), job(client, "b"))
    });
    assert_eq!(first.as_array().map(Vec::len), Some(3), "{first}");
    assert_eq!(first, second, "the graph's first frames repeat in the second job");
    // The graph, not its fallback, is what job 1's frame 0 shows: its pixel at the center is red.
    let png = std::fs::read(project.join("out").join("a").join("shot_000000.png")).expect("frame 0");
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .expect("png");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("pixels");
    let center = ((16 * 48 + 24) * 4) as usize;
    let [r, g, ..] = [buf[center], buf[center + 1], buf[center + 2]];
    assert!(
        r > g,
        "frame 0 of the first job draws the graph (red), not the fallback (green): {r} {g}"
    );
    let _ = std::fs::remove_dir_all(&project);
}
