// SPDX-License-Identifier: MPL-2.0
//! Sequence ▸ Render movie… (ADR-7726). The refusal and job-text tests need no GPU; the two job
//! tests render on any Vulkan adapter (lavapipe in CI) with the runtime renderer, as
//! `regia render --job` does on a farm. No golden image; nothing asserts on time (the loops are
//! hang guards in frames).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use regia::prelude::*;
use regia_render::{
    AntiAliasing, Camera, ClearMode, CpuMaterial, CpuMesh, LabVertex, MeshRenderer, PostSettings,
    RenderPlugin, RenderProfilePreset, RenderSettings,
};
use regia_scene::{GlobalTransform, Transform};

const SEQ: &str = "assets/cine/intro.seq.ron";
/// A camera move over 4 s at 10 fps: frame 0 at z 2.6, sliding right.
const SEQUENCE: &str = r#"(version: 3, name: "intro", fps: 10.0, tracks: [
    (name: "camera", target: "CameraTrack", property: "transform.position",
     keys: Vec3([(time: 0.0, value: (0.0, 0.0, 2.6)), (time: 4.0, value: (1.0, 0.0, 2.6))])),
])"#;
/// The job an agent writes by hand for the same render as [`small`] (frames 0..4).
const AGENT_JOB: &str = "(version: 4, name: \"intro\", sequence: Some(\"assets/cine/intro.seq.ron\"), \
     bindings: {\"CameraTrack\": Name(\"CameraTrack\")}, out: \"renders/agent\", \
     output: (width: Some(48), height: Some(32), fps: Some(10), format: Some(\"png\"), hud: Some(false)), \
     takes: [(name: \"movie\", range: Frames(start: 0, end: 4))])";
/// App frames a job may take before the test gives up (a hang guard, not a budget).
const HANG_GUARD: usize = 20_000;

fn project(tag: &str) -> PathBuf {
    let root = regia_test_utils::unique_temp_dir(&format!("render-movie-{tag}"));
    let path = root.join(SEQ);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("project dir");
    regia_test_utils::write_atomic(&path, SEQUENCE.as_bytes()).expect("sequence");
    root
}

fn small(end: Option<u64>) -> RenderMovieSettings {
    RenderMovieSettings {
        sequence: SEQ.into(),
        out: "renders/editor".into(),
        format: "png".into(),
        start: 0,
        end,
        width: 48,
        height: 32,
        fps: 10,
    }
}

/// A plain edit world of a project (no GPU).
fn edit_world(root: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
    world.spawn(regia_scene::components::Name::new("CameraTrack"));
    world
}

fn reason(error: &RpcError) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|d| d.get("reason").or_else(|| d.get("code")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[test]
fn refusals_name_their_documented_codes_and_start_nothing() {
    let root = project("codes");
    let mut world = edit_world(&root);
    let cases: Vec<(RenderMovieSettings, &str)> = vec![
        (
            RenderMovieSettings {
                out: "../outside".into(),
                ..small(Some(4))
            },
            "path_outside_project",
        ),
        (
            RenderMovieSettings {
                out: std::env::temp_dir()
                    .join("render-movie-abs")
                    .display()
                    .to_string(),
                ..small(Some(4))
            },
            "path_outside_project",
        ),
        (
            RenderMovieSettings {
                out: String::new(),
                ..small(Some(4))
            },
            "path_outside_project",
        ),
        (
            RenderMovieSettings {
                sequence: "assets/cine/intro.shots.ron".into(),
                ..small(Some(4))
            },
            "render_movie_bad_sequence",
        ),
        (
            RenderMovieSettings {
                sequence: "../intro.seq.ron".into(),
                ..small(Some(4))
            },
            "render_movie_bad_sequence",
        ),
        (
            RenderMovieSettings {
                sequence: "assets/cine/missing.seq.ron".into(),
                ..small(Some(4))
            },
            "render_movie_bad_sequence",
        ),
        (
            RenderMovieSettings {
                format: "gif".into(),
                ..small(Some(4))
            },
            "render_movie_bad_format",
        ),
        (
            RenderMovieSettings {
                start: 4,
                ..small(Some(4))
            },
            "render_movie_bad_range",
        ),
        (
            RenderMovieSettings {
                width: 0,
                ..small(Some(4))
            },
            "render_movie_bad_size",
        ),
    ];
    for (settings, want) in cases {
        let error = start(&mut world, &settings).expect_err(want);
        assert_eq!(reason(&error).as_deref(), Some(want), "{settings:?}: {error:?}");
        // The agent route answers the same reason.
        let params = json!({
            "sequence": settings.sequence, "out": settings.out, "format": settings.format,
            "start": settings.start, "end": settings.end, "width": settings.width,
            "height": settings.height, "fps": settings.fps,
        });
        let error = render_movie(&mut world, &params).expect_err(want);
        assert_eq!(reason(&error).as_deref(), Some(want), "agent {params}");
    }
    assert!(world.get_resource::<RenderMovieJob>().is_none(), "no run started");
    assert!(!root.join("renders").exists(), "nothing was written");
    // No project: F0003 with its reason.
    let mut bare = World::new();
    let error = start(&mut bare, &small(Some(4))).expect_err("no project");
    assert_eq!(reason(&error).as_deref(), Some("render_movie_no_project"));
    let _ = std::fs::remove_dir_all(&root);
}

/// The dialog's job is the job an agent writes for `render.out_job`: same sequence, binding table,
/// output settings and take, so the job runs the same frames.
#[test]
fn the_dialog_builds_the_job_an_agent_writes() {
    let root = project("text");
    let mut world = edit_world(&root);
    let job = job_for(&mut world, &small(Some(4))).expect("job");
    let mut agent = RenderJob::from_ron(AGENT_JOB).expect("agent job");
    agent.out = "renders/editor".into();
    assert_eq!(job, agent, "{}", job.to_ron().unwrap_or_default());
    let dry = render_movie(
        &mut world,
        &json!({ "sequence": SEQ, "out": "renders/editor", "width": 48, "height": 32, "fps": 10, "end": 4, "dry_run": true }),
    )
    .expect("dry run");
    assert_eq!(
        RenderJob::from_ron(dry["job_text"].as_str().expect("text")).expect("parses"),
        agent
    );
    // To the end: the whole sequence; a later start: frames up to the sequence's end (4 s at 10 fps).
    let whole = job_for(&mut world, &small(None)).expect("whole");
    assert_eq!(whole.takes[0].range, TakeRange::Whole);
    let tail = job_for(
        &mut world,
        &RenderMovieSettings {
            start: 30,
            ..small(None)
        },
    )
    .expect("tail");
    assert_eq!(tail.takes[0].range, TakeRange::Frames { start: 30, end: 40 });
    let _ = std::fs::remove_dir_all(&root);
}

/// A headless runtime host of `root` with the runtime renderer, the agent and a lit triangle in
/// front of a camera named `CameraTrack`.
fn gpu_app(root: &Path) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        RenderPlugin { headless: true },
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(root.to_path_buf()),
    });
    app.insert_resource(crate::agent_ext::EditorProject {
        root: Some(root.to_path_buf()),
    });
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
    app.finish_plugins();
    app.update();
    app
}

fn camera_global(mut cameras: Query<(&Transform, &mut GlobalTransform), With<Camera>>) {
    for (transform, mut global) in &mut cameras {
        global.0 = transform.compute_affine();
    }
}

/// Whether this host has a Vulkan adapter (the job tests skip without one, as the render-out
/// tests do).
fn has_renderer(app: &mut App) -> bool {
    let ok = app
        .world()
        .get_resource::<regia_render::RuntimeRenderer>()
        .is_some();
    if !ok {
        eprintln!("SKIP: no runtime renderer");
    }
    ok
}

/// Runs the app until `render.out_progress { job }` is no longer running; answers the last
/// progress with every hash.
fn run_agent_job(app: &mut App, job: u64) -> Value {
    for _ in 0..HANG_GUARD {
        app.update();
        let progress = render_out_host::progress(app.world_mut(), &json!({ "job": job, "hashes": true }))
            .expect("progress");
        if progress["status"] != "running" && progress["status"] != "cancelling" {
            return progress;
        }
    }
    panic!("job {job} did not end in {HANG_GUARD} frames");
}

/// Runs the app (with the editor's tick) until the Render movie run ends.
fn run_editor_job(app: &mut App) -> RenderMovieJob {
    for _ in 0..HANG_GUARD {
        app.update();
        crate::editor_jobs::tick(app.world_mut());
        let run = app.world().resource::<RenderMovieJob>().clone();
        if !run.running() {
            return run;
        }
    }
    panic!("the Render movie run did not end in {HANG_GUARD} frames");
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn frames_in(dir: &Path) -> (Vec<String>, usize) {
    let mut files = Vec::new();
    files_under(dir, &mut files);
    let parts = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "part"))
        .count();
    let mut pngs: Vec<String> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    pngs.sort();
    (pngs, parts)
}

/// Acceptance (1): the dialog's run of a 4-frame range writes the frame count and the first-frame
/// hash (and every other hash) of `render.out_job` with the same settings, reports through
/// `events.read` and the task row, and ends `done`.
#[test]
fn the_dialog_job_renders_what_render_out_job_renders() {
    let root = project("same");
    let (editor, editor_events) = {
        let mut app = gpu_app(&root);
        if !has_renderer(&mut app) {
            return;
        }
        let started = start(app.world_mut(), &small(Some(4))).expect("start");
        assert_eq!(started["frames"], 4, "{started}");
        assert!(
            app.world()
                .resource::<BackgroundTasks>()
                .rows()
                .iter()
                .any(|r| r.id == RENDER_MOVIE_TASK),
            "the task row is published"
        );
        let run = run_editor_job(&mut app);
        let progress = render_out_host::progress(app.world_mut(), &json!({ "job": run.job, "hashes": true }))
            .expect("progress");
        let events: Vec<Value> = app
            .world()
            .resource::<regia_agent::AgentEventLog>()
            .records_snapshot(512)
            .into_iter()
            .filter(|r| r.kind == RENDER_MOVIE_EVENT)
            .map(|r| r.data)
            .collect();
        assert!(
            app.world()
                .resource::<BackgroundTasks>()
                .rows()
                .iter()
                .all(|r| r.id != RENDER_MOVIE_TASK),
            "the task row is gone at the end"
        );
        ((run, progress), events)
    };
    let (run, editor_progress) = editor;
    assert_eq!(run.state, MovieState::Done, "{:?}", run.error);
    assert_eq!(run.frame, 4);
    assert_eq!(
        editor_events.first().map(|e| e["state"].clone()),
        Some(json!("running"))
    );
    assert_eq!(
        editor_events.last().map(|e| e["state"].clone()),
        Some(json!("done"))
    );
    assert!(
        editor_events
            .iter()
            .any(|e| e["frame"].as_u64().is_some_and(|f| f > 0 && f < 4)),
        "progress records between start and end: {editor_events:?}"
    );

    let mut app = gpu_app(&root);
    let started =
        render_out_host::out_job(app.world_mut(), &json!({ "job": AGENT_JOB })).expect("render.out_job");
    let job = started["job"].as_u64().expect("job id");
    let agent = run_agent_job(&mut app, job);
    assert_eq!(agent["status"], "done", "{agent}");
    assert_eq!(agent["frame"], 4);
    assert_eq!(editor_progress["frame"], agent["frame"], "same frame count");
    let hashes = |p: &Value| p["hashes"]["hashes"].clone();
    assert_eq!(hashes(&editor_progress)[0], hashes(&agent)[0], "same first frame");
    assert_eq!(hashes(&editor_progress), hashes(&agent), "same frames");
    assert_eq!(run.first_hash.as_deref(), hashes(&agent)[0].as_str());
    let (editor_pngs, editor_parts) = frames_in(&root.join("renders/editor"));
    let (agent_pngs, _) = frames_in(&root.join("renders/agent"));
    assert_eq!(editor_pngs.len(), 4, "{editor_pngs:?}");
    assert_eq!(editor_pngs, agent_pngs, "same file names");
    assert_eq!(editor_parts, 0);
    let _ = std::fs::remove_dir_all(&root);
}

/// Acceptance (1): Cancel (the task row's flag, as `editor.cancel_task` sets it) stops the run;
/// what stays are whole frames from 0 up, no `.part` file and no later frame.
#[test]
fn cancel_stops_the_run_and_leaves_no_partial_frame() {
    let root = project("cancel");
    let mut app = gpu_app(&root);
    if !has_renderer(&mut app) {
        return;
    }
    start(app.world_mut(), &small(Some(40))).expect("start");
    let mut cancelled = false;
    for _ in 0..HANG_GUARD {
        app.update();
        crate::editor_jobs::tick(app.world_mut());
        let run = app.world().resource::<RenderMovieJob>().clone();
        if !run.running() {
            break;
        }
        if !cancelled && run.frame >= 2 {
            assert!(
                crate::tasks::cancel(app.world(), RENDER_MOVIE_TASK),
                "the row is cancellable"
            );
            cancelled = true;
        }
    }
    let run = app.world().resource::<RenderMovieJob>().clone();
    assert!(
        cancelled,
        "the run reached frame 2 (it ended {:?} at frame {}: {:?})",
        run.state, run.frame, run.error
    );
    assert_eq!(run.state, MovieState::Cancelled, "{:?}", run.error);
    assert!(run.frame < 40, "stopped early at {}", run.frame);
    let (pngs, parts) = frames_in(&root.join("renders/editor"));
    assert_eq!(parts, 0, "no partial frame");
    assert!(
        !pngs.is_empty() && pngs.len() as u64 <= run.frame,
        "{pngs:?} vs {}",
        run.frame
    );
    // The frames kept are the first ones, in order, with no gap.
    let all = job_for(app.world_mut(), &small(Some(40))).expect("job");
    assert_eq!(all.takes.len(), 1);
    let mut numbers: Vec<u64> = pngs
        .iter()
        .filter_map(|name| {
            name.trim_end_matches(".png")
                .rsplit(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok())
        })
        .collect();
    numbers.sort_unstable();
    let first = numbers.first().copied().unwrap_or(0);
    assert!(
        numbers.iter().enumerate().all(|(i, n)| *n == first + i as u64),
        "{numbers:?}"
    );
    // A cancel after the end does nothing.
    assert!(!cancel(app.world_mut()));
    let _ = std::fs::remove_dir_all(&root);
}
