// SPDX-License-Identifier: MPL-2.0
//! A render job whose sequence spawns an actor from a prefab (ADR-2948), on lavapipe: the job
//! pre-rolls the prefab on its first frame, the actor exists for its range only, and a second run
//! gives the same frame count, frame hashes and event list (markers and spawns). No golden image.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::enable_fixed_step;
use regia_framework::sequencer::{MarkerReached, SpawnableChanged};
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, PostSettings,
    RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

/// A hang guard only: nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
const CALL_TIMEOUT_MS: u64 = 60_000;

/// The job's sequence: `actor` spawned from 0.1 to 0.3 s and moved, markers around it.
const MAIN: &str = r#"(version: 5, tracks: [
    (name: "slide", target: "actor", property: "transform.position",
        keys: Vec3([(time: 0.1, value: (-0.5, 0.0, 0.0)), (time: 0.3, value: (0.5, 0.0, 0.0))])),
    (name: "hold", target: "mesh", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0))])),
], markers: [(name: "in", time: 0.1), (name: "out", time: 0.3), (name: "end", time: 0.45)],
spawnables: [(binding: "actor", prefab: "actors/box.prefab.ron", start: 0.1, end: Some(0.3))])"#;

fn prefab() -> String {
    format!(
        r#"SceneFile(format_version: {}, entities: [
    (guid: "44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0", name: "Box", components: {{
        "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (0.3, 0.3, 0.3)),
    }}),
])
"#,
        regia_scene::format::SCENE_FORMAT_VERSION
    )
}

const JOB: &str = "(version: 2, name: \"spawn\", sequence: Some(\"cine/main.seq.ron\"), \
     bindings: {\"mesh\": Name(\"Mesh\")}, out: \"out\", \
     output: (width: Some(32), height: Some(24), fps: Some(30), hud: Some(false)), \
     takes: [(name: \"all\", range: Frames(start: 0, end: 15))])";

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

/// What the app saw: markers (name, skipped) and spawns (binding, spawned), in order.
#[derive(Resource, Default)]
struct Events(Vec<String>);

fn record(
    mut markers: MessageReader<MarkerReached>,
    mut spawns: MessageReader<SpawnableChanged>,
    mut events: ResMut<Events>,
) {
    for m in markers.read() {
        events.0.push(format!("marker {} {}", m.name, m.skipped));
    }
    for s in spawns.read() {
        events.0.push(format!("spawn {} {}", s.binding, s.spawned));
    }
}

/// One run: (frames written, frame hashes, events).
fn run(tag: &str) -> (u64, Vec<String>, Vec<String>) {
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-spawnable-{tag}"));
    std::fs::create_dir_all(project.join("cine")).expect("cine dir");
    std::fs::create_dir_all(project.join("actors")).expect("actors dir");
    std::fs::write(project.join("cine/main.seq.ron"), MAIN).expect("main");
    std::fs::write(project.join("actors/box.prefab.ron"), prefab()).expect("prefab");
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .init_resource::<Events>()
    .add_systems(Last, record);
    app.world_mut()
        .resource_mut::<regia_framework::sequencer::SequenceSpawnables>()
        .project_root = Some(project.clone());
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
        .name(format!("render-out-spawnable-{tag}"))
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
            assert_eq!(
                started["plan"]["unbound_targets"],
                json!([]),
                "a spawnable binds itself: {started}"
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
            let manifest_path = script_project.join(finished["manifest"].as_str().expect("manifest path"));
            let manifest: Value = serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest"))
                .expect("manifest json");
            *result.lock().expect("slot") = Some(manifest);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawn script");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !ok.load(Ordering::Relaxed) && !thread.is_finished() && Instant::now() < deadline {
        app.update();
    }
    thread.join().expect("script finished");
    // A few more frames so the last messages are read.
    for _ in 0..3 {
        app.update();
    }
    let manifest = slot.lock().expect("slot").take().expect("the script ran");
    let events = std::mem::take(&mut app.world_mut().resource_mut::<Events>().0);
    let _ = std::fs::remove_dir_all(&project);
    let written = manifest["job"]["takes"][0]["written"].as_u64().unwrap_or(0);
    let hashes = manifest["frame_hashes"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    (written, hashes, events)
}

#[test]
fn a_spawnable_render_job_repeats_with_the_same_frames_and_events() {
    let (frames_a, hashes_a, events_a) = run("a");
    assert_eq!(frames_a, 15, "every frame of the take");
    assert_eq!(hashes_a.len(), 15);
    let spawns: Vec<&String> = events_a.iter().filter(|e| e.starts_with("spawn actor")).collect();
    assert_eq!(spawns, ["spawn actor true", "spawn actor false"], "{events_a:?}");
    for name in ["marker in false", "marker out false", "marker end false"] {
        assert!(events_a.iter().any(|e| e == name), "{name} in {events_a:?}");
    }
    let (frames_b, hashes_b, events_b) = run("b");
    assert_eq!(frames_a, frames_b);
    assert_eq!(hashes_a, hashes_b, "the same pictures");
    assert_eq!(events_a, events_b, "the same events in the same order");
}
