// SPDX-License-Identifier: MPL-2.0
//! The Profiler panel's sources (`docs/specs/83-prefab-profiler-volumes.md` § 4.3, ADR-9740).
//!
//! - In process: a span from a system that runs only in the play world shows under the play
//!   source and not under the edit source, with the process frame profiler as the global
//!   subscriber (so a play system on a task-pool thread is recorded too).
//! - Over the protocol: a dedicated-server PIE session started the way the Net tab starts it
//!   (the `regia-pie-fixture` stand-in game, with its agent protocol on) is profiled through the
//!   process source; the panel's top spans equal the child's own `profiler.frame` answer, and
//!   stopping the session shows `disconnected` without the editor waiting.
//! - The Timeline tab (ADR-7325) reads both: the play-only span is on the play timeline and not
//!   the edit one, and the PIE server's own spans arrive over the protocol through the same worker.
//!
//! Waits are bounded counts of blocking waits on the children's output or the worker's polls; no
//! assert reads the wall clock.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::profiling::FrameProfiler;
use regia_editor::agent_ext::{self, EditorProject};
use regia_editor::level_io;
use regia_editor::pie_net;
use regia_editor::pie_process::{Binaries, PieProcesses, ProcessRole};
use regia_editor::profiler_panel::ProfilerPanel;
use regia_editor::profiler_source::{self, ProfilerSource, ProfilerView};
use serde_json::{Value, json};

const FIXTURE: &str = env!("CARGO_BIN_EXE_regia-pie-fixture");
const MAX_WAITS: usize = 1200;
const WAIT: Duration = Duration::from_millis(50);

/// The process frame profiler, installed once as the global subscriber of this test binary.
fn profiler() -> FrameProfiler {
    static PROFILER: OnceLock<FrameProfiler> = OnceLock::new();
    PROFILER
        .get_or_init(|| {
            use tracing_subscriber::layer::SubscriberExt;
            let profiler = FrameProfiler::new();
            profiler.set_flame_enabled(true);
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(profiler.clone()),
            );
            profiler
        })
        .clone()
}

fn project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"Prof\"\nid = \"prof.regia.dev\"\nversion = \"1.0.0\"\n",
    )
    .expect("Regia.toml");
    dir
}

fn edit_app(project: &Path) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(regia_scene::ScenePlugin);
    app.init_resource::<regia_editor::pie::PieRequests>()
        .init_resource::<regia_editor::pie::PieStatus>();
    let open = level_io::new_level(app.world_mut());
    app.world_mut().insert_resource(open);
    app.world_mut().insert_resource(EditorProject {
        root: Some(project.to_path_buf()),
    });
    agent_ext::install(app.world_mut());
    app.update();
    app
}

fn names(capture: &regia::telemetry::profile_capture::ProfileCapture) -> Vec<String> {
    regia_editor::profiler_model::capture_names(capture)
        .into_iter()
        .collect()
}

fn play_only_system() {
    let _span = tracing::info_span!("profiler_test_play_only").entered();
}

/// Runs `span` as a root on a new thread named `thread` and waits for it (ADR-7427).
fn root_on_thread(thread: &str, async_work: bool) {
    let joined = std::thread::Builder::new()
        .name(thread.to_owned())
        .spawn(move || {
            let _span = if async_work {
                tracing::info_span!("profiler_test_async_import").entered()
            } else {
                tracing::info_span!("profiler_test_compute_work").entered()
            };
        })
        .map(|h| h.join().is_ok());
    assert!(matches!(joined, Ok(true)), "{thread}");
}

/// A play system that hands work to an async-pool thread (an editor import or network poll
/// stand-in) and to a compute-pool thread (where a multi-threaded schedule runs systems), each
/// opened and closed while the editor holds the `pie_frame` context.
fn play_hands_off_work_system() {
    root_on_thread("regia-async (7)", true);
    root_on_thread("regia-compute (7)", false);
}

#[test]
fn a_play_only_span_shows_under_the_play_source_and_not_the_edit_source() {
    let profiler = profiler();
    let dir = project("profiler-src-pie");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    world.insert_resource(regia::Profiler(profiler.clone()));
    let mut session = regia_editor::pie::start_play(world, None).expect("play");
    session
        .app
        .add_systems(Update, (play_only_system, play_hands_off_work_system));
    for _ in 0..4 {
        {
            let _ui = tracing::info_span!("profiler_test_edit_ui").entered();
        }
        assert!(session.frame().is_none());
        profiler.end_frame();
    }
    profiler_source::set_source(world, ProfilerSource::Pie).expect("pie source");
    let play = profiler_source::read_source(world, 1, false, 30);
    profiler_source::set_source(world, ProfilerSource::Edit).expect("edit source");
    let edit = profiler_source::read_source(world, 2, false, 30);
    let (play, edit) = (
        names(play.capture.as_ref().expect("play tree")),
        names(edit.capture.as_ref().expect("edit tree")),
    );
    assert!(
        play.contains(&"profiler_test_play_only".to_owned()),
        "play: {play:?}"
    );
    assert!(
        !edit.contains(&"profiler_test_play_only".to_owned()),
        "edit: {edit:?}"
    );
    assert!(
        edit.contains(&"profiler_test_edit_ui".to_owned()),
        "edit: {edit:?}"
    );
    assert!(
        !play.contains(&"profiler_test_edit_ui".to_owned()),
        "play: {play:?}"
    );
    // ADR-7427: a root on an async-pool thread is not play work even while `pie_frame` holds the
    // context; a root on a compute-pool thread still is.
    assert!(
        play.contains(&"profiler_test_compute_work".to_owned()),
        "play: {play:?}"
    );
    assert!(
        !play.contains(&"profiler_test_async_import".to_owned()),
        "play: {play:?}"
    );
    assert!(
        edit.contains(&"profiler_test_async_import".to_owned()),
        "edit: {edit:?}"
    );

    // The Timeline tab reads the same frames per thread (ADR-7325).
    profiler.set_timeline_enabled(true);
    for _ in 0..2 {
        {
            let _ui = tracing::info_span!("profiler_test_edit_ui").entered();
        }
        assert!(session.frame().is_none());
        profiler.end_frame();
    }
    let span_names = |data: &profiler_source::TimelineData| -> Vec<String> {
        data.frame
            .as_ref()
            .map(|f| {
                f.threads
                    .iter()
                    .flat_map(|t| t.spans.iter().map(|s| s.name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    profiler_source::set_source(world, ProfilerSource::Pie).expect("pie source");
    let play = profiler_source::read_timeline(world);
    profiler_source::set_source(world, ProfilerSource::Edit).expect("edit source");
    let edit = profiler_source::read_timeline(world);
    assert_eq!((play.status.as_str(), edit.status.as_str()), ("live", "live"));
    let (play, edit) = (span_names(&play), span_names(&edit));
    assert!(
        play.contains(&"profiler_test_play_only".to_owned()),
        "play: {play:?}"
    );
    assert!(play.contains(&"pie_frame".to_owned()), "play: {play:?}");
    assert!(
        !edit.contains(&"profiler_test_play_only".to_owned()),
        "edit: {edit:?}"
    );
    assert!(
        edit.contains(&"profiler_test_edit_ui".to_owned()),
        "edit: {edit:?}"
    );
    regia_editor::pie::stop_play(session);
    let _ = std::fs::remove_dir_all(&dir);
}

fn call(world: &mut World, name: &str, params: Value) -> Value {
    let (_, handler) = pie_net::methods()
        .into_iter()
        .chain(profiler_source::methods())
        .find(|(schema, _)| schema.name == name)
        .unwrap_or_else(|| panic!("{name} registered"));
    handler(world, &params).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

fn top_names(rows: &Value) -> Vec<String> {
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_pie_server_process_is_profiled_over_the_protocol() {
    let dir = project("profiler-src-process");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    let mut procs = PieProcesses::default();
    procs.binaries = Some(Binaries {
        server: PathBuf::from(FIXTURE),
        client: PathBuf::from(FIXTURE),
    });
    procs
        .extra_env
        .push(("REGIA_PIE_FIXTURE_PROFILE".into(), "1".into()));
    world.insert_resource(procs);
    world.insert_resource(ProfilerPanel {
        open: true,
        status: String::new(),
        ..ProfilerPanel::default()
    });
    call(
        world,
        "editor.pie_net_config",
        json!({ "mode": "dedicated", "clients": 1, "separate_processes": true }),
    );
    call(world, "editor.pie_net_run", json!({ "action": "start" }));
    let agent_dir = world
        .resource::<PieProcesses>()
        .agent_dir(ProcessRole::Server)
        .expect("the server's agent directory");
    let state = call(
        world,
        "editor.profiler_source",
        json!({ "kind": "process", "address": "server" }),
    );
    assert_eq!(state["source"]["kind"], "process", "{state:#}");
    assert_eq!(state["source"]["process"], "server");

    // Wait (bounded) until the worker has polled the child twice.
    let mut polls = 0;
    for _ in 0..MAX_WAITS {
        world.resource_mut::<PieProcesses>().wait_and_pump(WAIT);
        polls = world
            .resource::<ProfilerView>()
            .worker()
            .map_or(0, |w| w.snapshot().polls);
        if polls >= 2 {
            break;
        }
    }
    assert!(
        polls >= 2,
        "the worker polled the server: {:#}",
        call(world, "editor.profiler_state", json!({}))
    );
    let panel = call(world, "editor.profiler_state", json!({}));
    assert_eq!(panel["status"], "connected", "{panel:#}");

    // The child's own answer, through a second connection.
    let (endpoint, token) = regia_agent::endpoint::read(&agent_dir).expect("endpoint");
    let mut client = regia_agent::client::Client::connect(&endpoint.url, &token).expect("connect");
    let own = client
        .call("profiler.frame", json!({ "frames": 60 }))
        .expect("profiler.frame");
    let (panel_top, own_top) = (top_names(&panel["top"]), top_names(&own["top"]));
    assert_eq!(panel_top, own_top, "panel {panel:#} vs child {own:#}");
    assert_eq!(panel_top, ["pie_fixture_tick", "pie_fixture_work"]);
    let entities = panel["entities"]["entities"].as_u64();
    let report = client.call("memory.report", json!({})).expect("memory.report");
    assert!(entities.is_some(), "{panel:#}");
    assert!(report["ecs"]["entities"].as_u64().is_some());

    // The Timeline tab over the same worker (ADR-7325): asking for it turns the child's timeline
    // on, and the child's spans arrive per thread.
    let (_, timeline) = profiler_source::methods()
        .into_iter()
        .find(|(schema, _)| schema.name == "editor.profiler_timeline")
        .expect("editor.profiler_timeline registered");
    let mut answer = Value::Null;
    for _ in 0..MAX_WAITS {
        world.resource_mut::<PieProcesses>().wait_and_pump(WAIT);
        if let Ok(a) = timeline(world, &json!({ "max_bars": 50 }))
            && a["bars"]
                .as_array()
                .is_some_and(|bars| bars.iter().any(|b| b["label"] == "pie_fixture_tick"))
        {
            answer = a;
            break;
        }
    }
    assert!(
        answer["threads"].as_array().is_some_and(|t| !t.is_empty()),
        "the child's timeline arrived: {:#}",
        timeline(world, &json!({}))
            .map_err(|e| format!("{e:?}"))
            .unwrap_or_else(|e| json!(e))
    );
    let own = client
        .call("profiler.frame", json!({ "frames": 1 }))
        .expect("profiler.frame");
    assert_eq!(
        own["timeline_enabled"], true,
        "the worker turned the child's timeline on"
    );

    // Stopping the session drops the connection: the panel says so and never waits.
    call(world, "editor.pie_net_run", json!({ "action": "stop" }));
    let mut status = String::new();
    for _ in 0..MAX_WAITS {
        let snapshot = world
            .resource::<ProfilerView>()
            .worker()
            .map(|w| w.wait_for_polls(u64::MAX, WAIT));
        status = snapshot.map(|s| s.link.as_str().to_owned()).unwrap_or_default();
        if status == "disconnected" {
            break;
        }
    }
    assert_eq!(status, "disconnected");
    profiler_source::set_source(world, ProfilerSource::Edit).expect("back to edit");
    let _ = std::fs::remove_dir_all(&dir);
}
