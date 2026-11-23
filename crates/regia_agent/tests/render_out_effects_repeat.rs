// SPDX-License-Identifier: MPL-2.0
//! ADR-2687 (audit renderout-state findings 1 to 3): two identical render-out jobs in one app give
//! the same color frames when the game spawns an effect without a seed during each job, after an
//! earlier job already spawned and finished one. The effects that job left are gone, so the world is
//! the same at both starts; only the effects layer's history differed: the seed counter of unseeded
//! effects, the particle clock (heat-haze phase) and the spawn ring (the slots new particles take,
//! the blend sort's last tie-break). Headless (lavapipe in the gate).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use regia_vfx::{SpawnEffect, VfxPlugin, VfxRuntimePlugin, VfxSettings};
use serde_json::{Value, json};

/// A hang guard only (the app stops stepping past it).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;
/// The game spawns its effect when the game clock crosses this (seconds).
const SPAWN_AT_S: f32 = 1.0;
const EFFECT: &str = "repeat_puff";
/// Frames of each job at [`JOB_FPS`]: 6 s.
const JOB_FRAMES: u32 = 48;
const JOB_FPS: u32 = 8;

/// A one-shot burst of alpha-blended puffs with random positions and speeds: every particle depends
/// on the seed, and overlapping puffs blend in sorted order.
const PUFF: &str = r##"(
    version: 4,
    name: "repeat_puff",
    system: (budget: 256),
    emitters: [
        (
            name: "puffs",
            delay: 0.0,
            spawn: (mode: Burst(48), lifetime: (min: 0.25, max: 0.35)),
            init: (
                shape: Sphere(radius: (min: 0.0, max: 0.5), surface: false),
                velocity: (mode: Directional(
                    dir: (0.0, 1.0, 0.0),
                    spread: 60.0,
                    speed: (min: 0.5, max: 2.0),
                )),
                size: (min: 0.15, max: 0.35),
                color: [(t: 0.0, color: "#f0e0c0")],
            ),
            update: (
                gravity: (0.0, 0.0, 0.0),
                drag: 0.2,
                alpha_over_life: [(t: 0.0, v: 0.8), (t: 1.0, v: 0.6)],
            ),
            render: (
                mode: Billboard,
                sprite: SoftDisc,
                blend: Alpha,
                emissive: 1.0,
                screen_floor_px: 1.0,
            ),
        ),
    ],
)
"##;

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

/// Polls `render.out_progress` until the job leaves `running`.
fn wait_done(client: &mut Client, job: u64) -> Value {
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while Instant::now() < deadline {
        let progress = call(client, "render.out_progress", json!({ "job": job }));
        if progress["status"] != json!("running") {
            return progress;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("render-out job {job} did not finish");
}

/// A second effect the game spawns right after [`EFFECT`], if the test sets one.
#[derive(Resource, Default)]
struct SecondEffect(Option<&'static str>);

/// The game clock before this frame, so the spawn fires once each time the clock crosses
/// [`SPAWN_AT_S`]: once per job, whose clock runs from zero.
#[derive(Resource, Default)]
struct LastElapsed(f32);

/// Effects the game spawned so far.
#[derive(Resource, Default)]
struct Spawned(u32);

fn spawn_on_the_game_clock(
    time: Res<Time>,
    mut last: ResMut<LastElapsed>,
    mut spawned: ResMut<Spawned>,
    second: Res<SecondEffect>,
    mut requests: MessageWriter<SpawnEffect>,
) {
    let now = time.elapsed().as_secs_f32();
    if last.0 < SPAWN_AT_S && now >= SPAWN_AT_S {
        requests.write(SpawnEffect {
            effect: EFFECT.to_string(),
            position: regia_core::math::Vec3::new(0.0, -0.2, 0.6),
            forward: regia_core::math::Vec3::Z,
            seed: None,
        });
        spawned.0 += 1;
        if let Some(effect) = second.0 {
            requests.write(SpawnEffect {
                effect: effect.to_string(),
                position: regia_core::math::Vec3::new(0.05, -0.2, 0.6),
                forward: regia_core::math::Vec3::Z,
                seed: None,
            });
        }
    }
    last.0 = now;
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

fn effects_app(project: &Path, library: PathBuf) -> App {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.post = PostSettings::golden_deterministic();
    settings.profile.anti_aliasing = AntiAliasing::None;
    settings.profile.auto_exposure = false;
    settings.profile.bloom = false;
    let mut app = App::new();
    app.insert_resource(VfxSettings {
        library_dir: Some(library),
        ..VfxSettings::default()
    });
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
        VfxPlugin,
        VfxRuntimePlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.to_path_buf()),
    });
    // Paused until a job takes the clock, so nothing depends on the client's speed.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(settings);
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.init_resource::<LastElapsed>();
    app.init_resource::<Spawned>();
    app.init_resource::<SecondEffect>();
    app.add_systems(Update, spawn_on_the_game_clock);

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
            triangle(),
            Arc::new(CpuMaterial {
                base_color: [0.35, 0.42, 0.52, 1.0],
                roughness: 0.6,
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
        .name("render-out-effects-script".to_string())
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

/// One job; its color and AOV hashes and the manifest's clock block.
fn job(client: &mut Client, out: &Path, frames: u32) -> (Value, Value) {
    let started = call(
        client,
        "render.out_start",
        json!({
            "out": out.display().to_string(), "name": "shot", "frames": frames, "width": 64,
            "height": 48, "fps": JOB_FPS, "warmup_frames": 2, "aovs": ["depth"], "aov_format": "raw",
        }),
    );
    let id = started["job"].as_u64().expect("job id");
    let done = wait_done(client, id);
    assert_eq!(done["status"], json!("done"), "{done}");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(out.join("shot_render_out.json")).expect("manifest"))
            .expect("json");
    let frames = json!({
        "frames": manifest["frame_hashes"],
        "depth": manifest["aov"]["frame_hashes"]["depth"],
    });
    (frames, manifest["clock"].clone())
}

#[test]
fn two_jobs_repeat_their_color_with_an_unseeded_effect_after_an_earlier_one() {
    let project = regia_test_utils::unique_temp_dir("render-out-effects-repeat");
    let library = project.join("effects");
    std::fs::create_dir_all(&library).expect("library dir");
    std::fs::write(library.join("repeat_puff.vfx.ron"), PUFF).expect("effect");
    let out = project.join("out");
    let mut app = effects_app(&project, library);
    let (first, second, first_clock, second_clock) = drive(&mut app, &project, move |client| {
        // Every job outlives its effect, so each job starts with nothing alive; the spawn at 1 s
        // comes after the warm-up, so delivered frames show it. The first job is history: it moved
        // the seed counter, the particle clock and the ring on.
        let _ = job(client, &out.join("history"), JOB_FRAMES);
        let (first, first_clock) = job(client, &out.join("a"), JOB_FRAMES);
        let (second, second_clock) = job(client, &out.join("b"), JOB_FRAMES);
        (first, second, first_clock, second_clock)
    });
    assert_eq!(app.world().resource::<Spawned>().0, 3, "one effect per job");
    let hashes = first["frames"].as_array().expect("hashes");
    assert!(
        hashes.iter().any(|hash| Some(hash) != hashes.last()),
        "the puffs show in some frames and are gone by the last"
    );
    assert_eq!(first["depth"], second["depth"], "the geometry repeats");
    assert_eq!(
        first["frames"], second["frames"],
        "the same request in one app draws the same effect"
    );
    assert_eq!(first_clock["effect_seeds"], "job_stream");
    assert_eq!(first_clock["particle_clock"], "restarted", "{first_clock}");
    assert_eq!(second_clock["particle_clock"], "restarted", "{second_clock}");
    let _ = std::fs::remove_dir_all(&project);
}

/// ADR-2814: jobs far shorter than the library's longest lifetime (the plugin's library holds 3 s),
/// with two effects whose first one retires first. Each job starts with nothing alive. Judged by the
/// library's longest lifetime the pool continued on another clock and ring, and the retired instance
/// slots came back in the other order, so the same spawns took other slots.
#[test]
fn short_jobs_repeat_their_color_with_two_effects_that_retire_in_turn() {
    const LONGER: &str = "repeat_puff_longer";
    let project = regia_test_utils::unique_temp_dir("render-out-effects-repeat-short");
    let library = project.join("effects");
    std::fs::create_dir_all(&library).expect("library dir");
    std::fs::write(library.join("repeat_puff.vfx.ron"), PUFF).expect("effect");
    let longer = PUFF.replace(EFFECT, LONGER).replace(
        "lifetime: (min: 0.25, max: 0.35)",
        "lifetime: (min: 0.5, max: 0.6)",
    );
    assert!(longer.contains("min: 0.5, max: 0.6"), "the fixture changed");
    std::fs::write(library.join("repeat_puff_longer.vfx.ron"), longer).expect("effect");
    let out = project.join("out");
    let mut app = effects_app(&project, library);
    app.insert_resource(SecondEffect(Some(LONGER)));
    // 2 s: the spawn at 1 s has gone with its 0.6 s and the 0.25 s margin before each job ends.
    const SHORT_JOB_FRAMES: u32 = 16;
    let (first, second, first_clock, second_clock) = drive(&mut app, &project, move |client| {
        let _ = job(client, &out.join("history"), SHORT_JOB_FRAMES);
        let (first, first_clock) = job(client, &out.join("a"), SHORT_JOB_FRAMES);
        let (second, second_clock) = job(client, &out.join("b"), SHORT_JOB_FRAMES);
        (first, second, first_clock, second_clock)
    });
    assert_eq!(app.world().resource::<Spawned>().0, 3, "one pair per job");
    assert_eq!(first["depth"], second["depth"], "the geometry repeats");
    assert_eq!(first_clock["particle_clock"], "restarted", "{first_clock}");
    assert_eq!(second_clock["particle_clock"], "restarted", "{second_clock}");
    assert_eq!(
        first["frames"], second["frames"],
        "the same request in one app draws the same effects"
    );
    let _ = std::fs::remove_dir_all(&project);
}
