// SPDX-License-Identifier: MPL-2.0
//! The showcase intro (`samples/showcase/example_scene/cinematics`, `.seq.ron` v2, ADR-2135) through
//! render-out on a real headless renderer (lavapipe on the gate): the shipped `intro.render_job.ron`,
//! shrunk to 64x36, reads the intro's two-level sub-sequence tree from the project, plays it on its own
//! player bound to a beacon and a sun, reports the keeper it leaves unbound, and two runs write the
//! same bytes while the beacon visibly moves.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_agent::render_job::{JobBinding, RenderJob};
use regia_app::enable_fixed_step;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, DirectionalLight, Environment, LabVertex,
    MeshRenderer, PointLight, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);
const CALL_TIMEOUT_MS: u64 = 60_000;

/// The intro's files, project-relative, as the showcase ships them.
const FILES: [&str; 4] = [
    "cinematics/intro.seq.ron",
    "cinematics/fx/sparkle.seq.ron",
    "cinematics/fx/twinkle.seq.ron",
    "cinematics/intro.render_job.ron",
];

fn showcase_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/showcase/example_scene")
}

/// The shipped job, smaller and quicker, with the sun bound too.
fn job_text() -> String {
    let text = std::fs::read_to_string(showcase_dir().join(FILES[3])).expect("the showcase job");
    let mut job = RenderJob::from_ron(&text).expect("the showcase job parses");
    job.output.width = Some(64);
    job.output.height = Some(36);
    job.output.warmup_frames = Some(2);
    job.output.settle_frames = Some(1);
    job.bindings.insert("sun".into(), JobBinding::Name("sun".into()));
    job.to_ron().expect("ron")
}

#[derive(Component)]
struct Beacon;

/// No transform propagation in this app: copies the beacon's `Transform` (the sequencer writes it)
/// to the `GlobalTransform` the renderer reads.
fn beacon_global(mut beacons: Query<(&Transform, &mut GlobalTransform), With<Beacon>>) {
    for (transform, mut global) in &mut beacons {
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

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect()
}

/// One run: the plan, the manifest and every take's PNG bytes.
struct Outcome {
    plan: Value,
    manifest: Value,
    files: Vec<(String, Vec<Vec<u8>>)>,
}

fn scenario(client: &mut Client, project: &Path) -> Outcome {
    let text = job_text();
    let started = call(client, "render.out_job", json!({ "job": text }));
    let job = started["job"].as_u64().expect("job id");
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let finished = loop {
        let progress = call(client, "render.out_progress", json!({ "job": job }));
        if progress["status"] != json!("running") {
            break progress;
        }
        assert!(Instant::now() < deadline, "the job did not finish");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(finished["status"], json!("done"), "{finished}");
    let players = call(client, "sequence.player", json!({}));
    assert_eq!(
        players["count"],
        json!(0),
        "the job removed its own player: {players}"
    );
    let manifest_path = project.join(finished["manifest"].as_str().expect("manifest path"));
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest")).expect("manifest json");
    let files = manifest["job"]["takes"]
        .as_array()
        .expect("takes")
        .iter()
        .map(|take| {
            let name = take["name"].as_str().expect("name").to_string();
            let bytes = take["files"]
                .as_array()
                .expect("files")
                .iter()
                .map(|file| std::fs::read(project.join(file.as_str().expect("file"))).expect("frame file"))
                .collect();
            (name, bytes)
        })
        .collect();
    Outcome {
        plan: started["plan"].clone(),
        manifest,
        files,
    }
}

fn run(tag: &str) -> Outcome {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-showcase-seq-{tag}"));
    for file in FILES {
        let to = project.join(file);
        std::fs::create_dir_all(to.parent().expect("parent")).expect("dir");
        let bytes = std::fs::read(showcase_dir().join(file)).expect("showcase file");
        std::fs::write(&to, bytes).expect("project file");
    }

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    enable_fixed_step(&mut app, 30.0);
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(deterministic_settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.add_systems(
        PostUpdate,
        beacon_global.after(regia_framework::sequencer::SequencerSet::Apply),
    );
    // Twelve meters in front of the beacon's column, which rises from y = -6 to 5 at z = -100.
    let camera = Transform::from_translation(regia_core::math::Vec3::new(0.0, 0.0, -88.0));
    app.world_mut().spawn((
        camera,
        GlobalTransform(camera.compute_affine()),
        Camera {
            exposure_ev: Some(14.0),
            fov_y: 1.1,
            clear: ClearMode::Color([0.05, 0.07, 0.12, 1.0]),
            ..Camera::default()
        },
    ));
    let start = Transform::from_translation(regia_core::math::Vec3::new(0.0, -6.0, -100.0));
    app.world_mut().spawn((
        start,
        GlobalTransform(start.compute_affine()),
        Beacon,
        regia_scene::components::Name::new("beacon"),
        PointLight::default(),
        MeshRenderer::new(
            triangle(),
            Arc::new(CpuMaterial {
                base_color: [0.9, 0.75, 0.3, 1.0],
                roughness: 0.5,
                metallic: 0.0,
                ..CpuMaterial::default()
            }),
        ),
    ));
    app.world_mut().spawn((
        regia_scene::components::Name::new("sun"),
        DirectionalLight {
            direction: [0.15, -0.45, -1.0],
            color: [1.0, 0.96, 0.9],
            illuminance: 20_000.0,
            cast_shadows: false,
            static_shadow: false,
        },
    ));

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let slot = Arc::new(std::sync::Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_project: PathBuf = project.clone();
    let script = std::thread::Builder::new()
        .name(format!("render-out-showcase-seq-{tag}"))
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
            *result.lock().expect("slot") = Some(scenario(&mut client, &script_project));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    let outcome = slot.lock().expect("slot").take().expect("scenario ran");
    let _ = std::fs::remove_dir_all(&project);
    outcome
}

#[test]
fn the_showcase_intro_renders_through_its_job_the_same_twice() {
    let a = run("a");
    let b = run("b");
    // The plan: the job's own player, bound by name, and the keeper reported as unbound.
    assert_eq!(a.plan["sequence_players"], json!(1), "{}", a.plan);
    assert_eq!(a.plan["unbound_targets"], json!(["keeper"]), "{}", a.plan);
    assert!(
        strings(&a.plan["warnings"]).contains(&"sequence_targets_unbound".to_string()),
        "{}",
        a.plan
    );
    let bound: Vec<&str> = a.plan["bindings"]
        .as_array()
        .expect("bindings")
        .iter()
        .filter_map(|b| b["target"].as_str())
        .collect();
    assert_eq!(bound, ["beacon", "sun"], "{}", a.plan);
    // Two takes on one 6 s timeline at 30 fps: the rise (0..3 s) and the sparkles (3..6 s).
    let names: Vec<&str> = a.files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["rise", "sparkles"]);
    assert_eq!(a.files[0].1.len(), 90);
    assert_eq!(a.files[1].1.len(), 90);
    // Deterministic: the same bytes on a second run.
    assert_eq!(
        strings(&a.manifest["frame_hashes"]),
        strings(&b.manifest["frame_hashes"]),
        "two runs agree frame for frame"
    );
    for ((name, fa), (_, fb)) in a.files.iter().zip(&b.files) {
        for (k, (x, y)) in fa.iter().zip(fb).enumerate() {
            assert_eq!(x, y, "{name}_{k:06}.png");
        }
    }
    // The beacon moves: the rise's first and last frames differ, and so do the sparkles'.
    let hashes = strings(&a.manifest["frame_hashes"]);
    assert_eq!(hashes.len(), 180);
    assert_ne!(hashes[0], hashes[89], "the beacon rose");
    assert_ne!(
        hashes[90], hashes[179],
        "the pulses and sparkles change the picture"
    );
}
