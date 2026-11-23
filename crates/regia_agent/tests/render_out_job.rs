// SPDX-License-Identifier: MPL-2.0
//! Render jobs (ADR-1616) on a real headless renderer (lavapipe on the gate): a take rendered on
//! its own hashes equal to the same frames inside a full render of the timeline, the job's cut
//! frames are the shot boundaries, each take lands in its own folder numbered from 0, and a dry
//! run answers the plan without starting anything.

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
    MeshRenderer, PostSettings, RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Value, json};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(180);
const CALL_TIMEOUT_MS: u64 = 60_000;
/// Yaw added each fixed step, so the simulation state differs on every frame.
const SPIN_STEP: f32 = 0.35;

/// Two shots of 0.25 s at 20 fps: the boundary is exactly frame 5, the timeline 10 frames.
const SHOTS: &str = r#"(version: 2, fps: 20.0, shots: [
    (name: "a", duration: 0.25, camera: (path: (interp: Linear,
        keys: [(time: 0.0, position: (0.0, 0.0, 2.6)), (time: 0.25, position: (0.5, 0.0, 2.6))],
        look_at: Point((0.0, 0.0, 0.0))))),
    (name: "b", duration: 0.25, camera: (path: (interp: Linear,
        keys: [(time: 0.0, position: (0.0, 0.6, 2.2)), (time: 0.25, position: (-0.5, 0.6, 2.2))],
        look_at: Point((0.0, 0.0, 0.0))))),
])"#;

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

/// This app has no transform propagation (`MinimalPlugins`): the shot player writes the camera's
/// `Transform`, and this copies it to the `GlobalTransform` the renderer reads.
fn camera_global(mut cameras: Query<(&Transform, &mut GlobalTransform), With<Camera>>) {
    for (transform, mut global) in &mut cameras {
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

/// How a run plays the slide sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slide {
    /// No slider and no sequence.
    None,
    /// The host spawns an external-clock player bound to the slider (ADR-1614).
    HostPlayer,
    /// The job's `.render_job.ron` v2 binding table plays it (ADR-1880): no player before the job.
    JobTable,
    /// The table plays a parent sequence whose sub-sequence (read from the project) holds the
    /// slide under another target name, and leaves one target of the parent out.
    JobTree,
}

/// A job over the shot list: 64x48 at 20 fps, one take per `(name, start, end)`.
fn job_text(takes: &[(&str, u64, u64)]) -> String {
    job_text_with(takes, "")
}

/// [`job_text`] with extra top-level fields (`sequence`, `bindings`).
fn job_text_with(takes: &[(&str, u64, u64)], extra: &str) -> String {
    let takes: Vec<String> = takes
        .iter()
        .map(|(name, start, end)| format!("(name: \"{name}\", range: Frames(start: {start}, end: {end}))"))
        .collect();
    format!(
        "(version: 2, name: \"job\", shots: Some(\"shots/two.shots.ron\"), out: \"out\", {extra}\
         output: (width: Some(64), height: Some(48), fps: Some(20), hud: Some(false), writers: Some(1)), \
         takes: [{}])",
        takes.join(", ")
    )
}

/// What one run leaves: the manifest and the PNG bytes of every take's files, by take name, and
/// the external sequence player's clock after the job (runs with a sequence player only).
struct Outcome {
    plan: Value,
    manifest: Value,
    files: Vec<(String, Vec<Vec<u8>>)>,
    sequence_time: Option<f64>,
}

/// A `.seq.ron` sliding the `slider` entity across the view over the 0.5 s timeline.
const SLIDE: &str = r#"(version: 1, tracks: [(name: "slide", target: "slider", property: "transform.position",
    keys: Vec3([(time: 0.0, value: (-0.8, -0.2, 0.3)), (time: 0.5, value: (0.8, -0.2, 0.3))]))])"#;

/// The parent of [`SUB_SLIDE`]: the renamed slide, and a track the job's table leaves out.
const TREE: &str = r#"(version: 2, tracks: [
    (name: "fx", keys: Sub([(sequence: "cine/sub.seq.ron", start: 0.0, end: 0.5, bindings: {"thing": "slider"})])),
    (name: "ghost", target: "nobody", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0))])),
])"#;

/// [`SLIDE`] with its own target name.
const SUB_SLIDE: &str = r#"(version: 2, tracks: [(name: "slide", target: "thing", property: "transform.position",
    keys: Vec3([(time: 0.0, value: (-0.8, -0.2, 0.3)), (time: 0.5, value: (0.8, -0.2, 0.3))]))])"#;

#[derive(Component)]
struct Slider;

/// Copies the slider's `Transform` (written by the sequencer) to the `GlobalTransform` the
/// renderer reads (no transform propagation in this app).
fn slider_global(mut sliders: Query<(&Transform, &mut GlobalTransform), With<Slider>>) {
    for (transform, mut global) in &mut sliders {
        global.0 = transform.compute_affine();
    }
}

fn scenario(client: &mut Client, project: &Path, takes: &[(&str, u64, u64)], slide: Slide) -> Outcome {
    let table = "bindings: {\"slider\": Name(\"Slider\")}, ";
    let text = match slide {
        Slide::JobTable => job_text_with(takes, &format!("sequence: Some(\"cine/slide.seq.ron\"), {table}")),
        Slide::JobTree => job_text_with(takes, &format!("sequence: Some(\"cine/tree.seq.ron\"), {table}")),
        Slide::None | Slide::HostPlayer => job_text(takes),
    };
    // A dry run resolves the plan and starts nothing.
    let dry = call(client, "render.out_job", json!({ "job": text, "dry_run": true }));
    assert_eq!(dry["dry_run"], json!(true), "{dry}");
    assert!(
        client.call("render.out_progress", json!({})).is_err(),
        "a dry run starts no job"
    );
    let started = call(client, "render.out_job", json!({ "job": text }));
    let job = started["job"].as_u64().expect("job id");
    assert_eq!(started["plan"], dry["plan"], "the dry run's plan is the job's");
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
    let sequence_time = players["players"][0]["time"].as_f64();
    if matches!(slide, Slide::JobTable | Slide::JobTree) {
        assert_eq!(
            players["count"],
            json!(0),
            "the job removed its own player: {players}"
        );
        assert_eq!(
            dry["plan"]["bindings"][0]["target"],
            json!("slider"),
            "{}",
            dry["plan"]
        );
    }
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
        sequence_time,
    }
}

fn run(tag: &str, takes: &'static [(&'static str, u64, u64)], with_sequence: bool) -> Outcome {
    run_slide(
        tag,
        takes,
        if with_sequence {
            Slide::HostPlayer
        } else {
            Slide::None
        },
    )
}

fn run_slide(tag: &str, takes: &'static [(&'static str, u64, u64)], slide: Slide) -> Outcome {
    let with_sequence = slide != Slide::None;
    let project = regia_test_utils::unique_temp_dir(&format!("render-out-job-{tag}"));
    std::fs::create_dir_all(project.join("shots")).expect("shots dir");
    std::fs::write(project.join("shots").join("two.shots.ron"), SHOTS).expect("shots file");
    std::fs::create_dir_all(project.join("cine")).expect("cine dir");
    std::fs::write(project.join("cine").join("slide.seq.ron"), SLIDE).expect("sequence file");
    std::fs::write(project.join("cine").join("tree.seq.ron"), TREE).expect("sequence file");
    std::fs::write(project.join("cine").join("sub.seq.ron"), SUB_SLIDE).expect("sequence file");

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    });
    enable_fixed_step(&mut app, 20.0);
    // Paused until the job takes the clock, so the spin does not depend on the client's speed.
    let mut time = Time::default();
    time.paused = true;
    app.insert_resource(time);
    app.insert_resource(Spin(0.0));
    app.insert_resource(deterministic_settings());
    app.insert_resource(Environment {
        ibl: false,
        ambient_lux: 18_000.0,
        double_sided: true,
    });
    app.add_systems(FixedUpdate, spin_mesh);
    app.add_systems(
        PostUpdate,
        camera_global.after(regia_framework::shots::ShotDriveSet),
    );
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
    if with_sequence {
        let start = Transform::from_translation(regia_core::math::Vec3::new(-0.8, -0.2, 0.3));
        let slider = app
            .world_mut()
            .spawn((
                start,
                GlobalTransform(start.compute_affine()),
                Slider,
                regia_scene::components::Name::new("Slider"),
                MeshRenderer::new(
                    triangle(),
                    Arc::new(CpuMaterial {
                        base_color: [0.2, 0.6, 0.9, 1.0],
                        roughness: 0.6,
                        metallic: 0.0,
                        ..CpuMaterial::default()
                    }),
                ),
            ))
            .id();
        if slide == Slide::HostPlayer {
            let mut player = regia_framework::sequencer::SequencePlayer::new(
                regia_framework::sequencer::Sequence::from_ron(SLIDE).expect("slide sequence"),
            );
            player.clock = regia_framework::sequencer::ClockSource::External;
            app.world_mut().spawn((
                player,
                regia_framework::sequencer::SequenceBindings::default()
                    .with("slider", regia_framework::sequencer::Binding::Entity(slider)),
            ));
        }
        app.add_systems(
            PostUpdate,
            slider_global.after(regia_framework::sequencer::SequencerSet::Apply),
        );
    }
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

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let slot = Arc::new(std::sync::Mutex::new(None));
    let result = slot.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script_project: PathBuf = project.clone();
    let script = std::thread::Builder::new()
        .name(format!("render-out-job-{tag}"))
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
            *result.lock().expect("slot") = Some(scenario(&mut client, &script_project, takes, slide));
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

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect()
}

#[test]
fn a_take_alone_equals_the_same_frames_of_a_full_render_and_cuts_are_the_shot_boundaries() {
    let full = run("full", &[("all", 0, 10)], false);
    let alone = run("alone", &[("late", 6, 10)], false);

    // The cut list is the shot boundary at frame 5 in both plans.
    assert_eq!(full.plan["cuts"], json!([5]), "{}", full.plan);
    assert_eq!(alone.plan["cuts"], json!([5]), "{}", alone.plan);
    assert_eq!(full.plan["preroll_frames"], json!(0));
    assert_eq!(
        alone.plan["preroll_frames"],
        json!(6),
        "frames 0..6 are rendered unwritten"
    );
    let all = &full.manifest["job"]["takes"][0];
    assert_eq!(all["cut_frames"], json!([5]));
    assert_eq!(all["cut_list"][0]["shot"], json!("a"));
    assert_eq!(all["cut_list"][1]["shot"], json!("b"));
    assert_eq!(all["cut_list"][1]["local_start"], json!(5));
    let late = &alone.manifest["job"]["takes"][0];
    assert_eq!(late["cut_frames"], json!([]), "the boundary is before the take");
    assert_eq!(late["first_shot"], json!("b"));
    assert_eq!(late["status"], json!("done"));
    assert_eq!(late["written"], json!(4));

    // The timeline hashes agree frame for frame, and the take's are the full render's 6..10.
    let full_hashes = strings(&full.manifest["frame_hashes"]);
    let alone_hashes = strings(&alone.manifest["frame_hashes"]);
    assert_eq!(full_hashes.len(), 10);
    assert_eq!(alone_hashes, full_hashes, "the pre-roll renders the same frames");
    assert_eq!(strings(&late["frame_hashes"]), full_hashes[6..10].to_vec());
    assert_eq!(
        strings(&all["frame_hashes"]),
        full_hashes,
        "a take over the whole timeline is the timeline"
    );
    // The frames move: the spin and the camera path change every frame.
    for pair in full_hashes.windows(2) {
        assert_ne!(pair[0], pair[1], "consecutive frames differ");
    }

    // The files: each take in its own folder, numbered from 0, byte-identical to the full render.
    let (name, late_files) = &alone.files[0];
    assert_eq!(name, "late");
    assert_eq!(late_files.len(), 4);
    let full_files = &full.files[0].1;
    assert_eq!(full_files.len(), 10);
    for (k, bytes) in late_files.iter().enumerate() {
        assert_eq!(bytes, &full_files[6 + k], "late_{k:06}.png is frame {}", 6 + k);
    }
    let shown = strings(&late["files"]);
    assert!(shown[0].ends_with("out/late/late_000000.png"), "{shown:?}");
    assert!(shown[3].ends_with("out/late/late_000003.png"), "{shown:?}");
    assert_eq!(
        full.manifest["camera_cuts"],
        json!(2),
        "frame 0 and the boundary, once each"
    );
}

#[test]
fn an_external_clock_sequence_plays_on_the_job_timeline_and_a_take_alone_still_matches() {
    let full = run("seq-full", &[("all", 0, 10)], true);
    let alone = run("seq-alone", &[("late", 6, 10)], true);
    for outcome in [&full, &alone] {
        assert_eq!(outcome.plan["sequence_players"], json!(1), "{}", outcome.plan);
        assert!(
            !strings(&outcome.plan["warnings"]).contains(&"sequence_tracks_not_applied".to_string()),
            "{}",
            outcome.plan
        );
        // The job wrote the player's clock up to the last frame's time (9 / 20 s).
        assert_eq!(outcome.sequence_time, Some(0.45));
    }
    let full_hashes = strings(&full.manifest["frame_hashes"]);
    assert_eq!(strings(&alone.manifest["frame_hashes"]), full_hashes);
    assert_eq!(
        strings(&alone.manifest["job"]["takes"][0]["frame_hashes"]),
        full_hashes[6..10].to_vec()
    );
    // The sliding mesh is in the picture: the frames differ from the run without it.
    let plain = run("seq-plain", &[("all", 0, 10)], false);
    let plain_hashes = strings(&plain.manifest["frame_hashes"]);
    for (frame, (with, without)) in full_hashes.iter().zip(&plain_hashes).enumerate() {
        assert_ne!(with, without, "frame {frame} shows the slider");
    }
}

#[test]
fn a_job_reads_a_sub_sequence_tree_and_reports_the_targets_its_table_leaves_out() {
    let host = run_slide("tree-host", &[("all", 0, 10)], Slide::HostPlayer);
    let tree = run_slide("tree-job", &[("all", 0, 10)], Slide::JobTree);
    assert_eq!(tree.plan["unbound_targets"], json!(["nobody"]), "{}", tree.plan);
    assert!(
        strings(&tree.plan["warnings"]).contains(&"sequence_targets_unbound".to_string()),
        "{}",
        tree.plan
    );
    assert_eq!(
        strings(&tree.manifest["frame_hashes"]),
        strings(&host.manifest["frame_hashes"]),
        "the renamed sub-sequence moves the slider like the plain sequence"
    );
}

#[test]
fn a_job_binding_table_plays_its_own_sequence_exactly_like_a_host_player() {
    let host = run_slide("table-host", &[("all", 0, 10)], Slide::HostPlayer);
    let table = run_slide("table-job", &[("all", 0, 10)], Slide::JobTable);
    assert_eq!(table.plan["sequence_players"], json!(1), "{}", table.plan);
    assert!(
        !strings(&table.plan["warnings"]).contains(&"sequence_tracks_not_applied".to_string()),
        "{}",
        table.plan
    );
    assert_eq!(table.sequence_time, None, "no player is left after the job");
    assert_eq!(table.plan["unbound_targets"], json!([]), "{}", table.plan);
    assert_eq!(
        strings(&table.manifest["frame_hashes"]),
        strings(&host.manifest["frame_hashes"]),
        "the job's own player renders the same frames as the host's"
    );
    let take = |o: &Outcome| o.files[0].1.clone();
    assert_eq!(take(&table), take(&host), "byte-identical PNGs");
    let plain = run_slide("table-plain", &[("all", 0, 10)], Slide::None);
    assert_ne!(
        strings(&table.manifest["frame_hashes"]),
        strings(&plain.manifest["frame_hashes"]),
        "the slider moved"
    );
}
