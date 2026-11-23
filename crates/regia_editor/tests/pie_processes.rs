// SPDX-License-Identifier: MPL-2.0
//! Play-in-editor with separate processes (`docs/specs/77-session-lifecycle.md` § 10.4, ADR-2682).
//!
//! A fixture project and the `regia-pie-fixture` binary (a stand-in game on the engine's server host
//! and UDP session) stand for a game `regia server --clients N` built. Every port is ephemeral.
//! Waits are bounded counts of `wait_and_pump`, which blocks on the children's output; no assert
//! reads the wall clock.

use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_app::prelude::*;
use regia_editor::agent_ext::{self, EditorProject};
use regia_editor::level_io;
use regia_editor::pie_net::{self, PieNet};
use regia_editor::pie_process::{Binaries, PieProcesses, ProcessRole, ProcessState};
use regia_editor::pie_process_view::{GameViews, ViewSnapshot};
use serde_json::{Value, json};

const FIXTURE: &str = env!("CARGO_BIN_EXE_regia-pie-fixture");

/// Most waits per phase; each blocks at most [`WAIT`] on the children's output.
const MAX_WAITS: usize = 1200;
const WAIT: Duration = Duration::from_millis(50);

fn project(tag: &str) -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets/levels")).expect("assets");
    std::fs::write(
        dir.join("Regia.toml"),
        "[project]\nname = \"Pie\"\nid = \"pie.regia.dev\"\nversion = \"1.0.0\"\n",
    )
    .expect("Regia.toml");
    dir
}

/// The edit world the editor builds, with a project and a new level open.
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
    let mut procs = PieProcesses::default();
    procs.binaries = Some(Binaries {
        server: PathBuf::from(FIXTURE),
        client: PathBuf::from(FIXTURE),
    });
    app.world_mut().insert_resource(procs);
    app.update();
    app
}

fn call(world: &mut World, name: &str, params: Value) -> Value {
    let (_, handler) = pie_net::methods()
        .into_iter()
        .find(|(schema, _)| schema.name == name)
        .unwrap_or_else(|| panic!("{name} registered"));
    handler(world, &params).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

fn processes(world: &mut World) -> Value {
    call(world, "editor.pie_processes", json!({ "tail": 40 }))
}

fn wait(world: &mut World) {
    world.resource_mut::<PieProcesses>().wait_and_pump(WAIT);
}

/// The process of `label` (`[server]`, `[client 1]`), when it was started.
fn find<'a>(state: &'a Value, label: &str) -> Option<&'a Value> {
    state["processes"]
        .as_array()
        .and_then(|ps| ps.iter().find(|p| p["label"] == label))
}

/// The process of `label`; it must have been started.
fn process<'a>(state: &'a Value, label: &str) -> &'a Value {
    find(state, label).unwrap_or_else(|| panic!("{label} in {state:#}"))
}

fn tail_has(p: &Value, needle: &str) -> bool {
    p["log_tail"].as_array().is_some_and(|lines| {
        lines
            .iter()
            .any(|l| l.as_str().is_some_and(|l| l.contains(needle)))
    })
}

fn pids(state: &Value) -> Vec<u32> {
    state["processes"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p["pid"].as_u64().and_then(|n| u32::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default()
}

/// No process with this pid runs any more (Linux: gone from `/proc`, or a zombie nobody reaped).
#[cfg(target_os = "linux")]
fn pid_dead(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

/// Waits until `done` holds for the `editor.pie_processes` state; panics with it otherwise.
fn wait_for(world: &mut World, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..MAX_WAITS {
        let state = processes(world);
        if done(&state) {
            return state;
        }
        wait(world);
    }
    panic!("{what} did not happen: {:#}", processes(world));
}

/// Spec 77 § 10.4, spec 76 § 10: with separate processes and a dedicated mode, the editor starts a
/// server process and two client processes; both clients join (the server's `/health` and the PIE
/// state report 2 peers), every process got the level and the conditioner, each line reached the
/// tails with its prefix, and stop leaves no child alive and no play files behind.
#[test]
fn a_dedicated_server_and_two_client_processes_join_and_stop_leaves_nothing_running() {
    let dir = project("pie-proc-dedicated");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    call(
        world,
        "editor.pie_net_config",
        json!({ "mode": "dedicated", "clients": 2, "preset": "bad", "seed": 7, "separate_processes": true }),
    );
    let started = call(world, "editor.pie_net_run", json!({ "action": "start" }));
    assert_eq!(started["separate_processes"], true, "{started}");

    let state = wait_for(world, "both clients in game", |s| s["peers"] == 2);
    assert_eq!(state["state"], "running", "{state:#}");
    assert_eq!(state["mode"], "dedicated");
    assert_eq!(state["alive"], 3, "server and two clients run: {state:#}");
    let port = state["port"].as_u64().expect("port");
    assert_ne!(port, 0, "an ephemeral port was passed on");
    assert_eq!(
        state["addr"].as_str(),
        Some(format!("127.0.0.1:{port}").as_str()),
        "the clients dial the port the server printed"
    );
    let level = state["level"].as_str().expect("level").to_owned();
    assert!(level.starts_with(".pie/play-"), "{level}");
    assert!(
        dir.join("assets").join(&level).is_file(),
        "the play copy exists while playing"
    );

    let server = process(&state, "[server]");
    assert!(
        tail_has(server, &format!("listening 127.0.0.1:{port}")),
        "{server:#}"
    );
    assert!(tail_has(server, "conditioner bad seed 7"), "{server:#}");
    for k in 0..2 {
        let client = process(&state, &format!("[client {k}]"));
        assert_eq!(client["state"], "running", "{client:#}");
        assert!(
            tail_has(client, &format!("client{k} state in_game")),
            "{client:#}"
        );
        assert!(tail_has(client, "pie mode dedicated"), "{client:#}");
        assert!(tail_has(client, "conditioner bad seed 7"), "{client:#}");
        assert!(
            tail_has(client, &level),
            "the level reached client {k}: {client:#}"
        );
        assert!(tail_has(client, "title -"), "offscreen: no window title");
    }
    let net = call(world, "editor.pie_net_state", json!({}));
    assert_eq!(net["peers"], 2, "{net:#}");
    assert_eq!(net["processes_state"], "running");

    let running = pids(&state);
    assert_eq!(running.len(), 3);
    call(world, "editor.pie_net_run", json!({ "action": "stop" }));
    let stopped = processes(world);
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["alive"], 0, "{stopped:#}");
    for p in stopped["processes"].as_array().expect("processes") {
        assert_eq!(p["state"], "exited", "{p:#}");
        assert_eq!(p["stopped_by_editor"], true, "{p:#}");
        assert_eq!(p["crashed"], false, "{p:#}");
    }
    #[cfg(target_os = "linux")]
    for pid in running {
        assert!(pid_dead(pid), "pid {pid} still runs after stop");
    }
    assert!(
        !dir.join("assets").join(&level).exists(),
        "the play copy is removed"
    );
    assert!(
        std::fs::read_dir(dir.join("build/pie")).map_or(true, |mut d| d.next().is_none()),
        "the server directory is removed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A child that exits on its own is reported with its exit code (70, the crash code) and its last
/// lines; the other processes keep running until stop.
#[test]
fn a_client_that_exits_with_code_70_is_reported_with_its_code_and_last_lines() {
    let dir = project("pie-proc-crash");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    world.resource_mut::<PieProcesses>().extra_env =
        vec![("REGIA_PIE_FIXTURE_CRASH".to_owned(), "client1".to_owned())];
    call(
        world,
        "editor.pie_net_config",
        json!({ "mode": "dedicated", "clients": 2, "separate_processes": true }),
    );
    call(world, "editor.pie_net_run", json!({ "action": "start" }));
    // The exit and the last line travel separately (the exit status and the pipe); wait for both.
    let state = wait_for(world, "client 1 exited and its last line arrived", |s| {
        find(s, "[client 1]")
            .is_some_and(|p| p["state"] == "exited" && tail_has(p, "client1 crashing on purpose"))
    });
    let crashed = process(&state, "[client 1]");
    assert_eq!(crashed["exit_code"], 70, "{crashed:#}");
    assert_eq!(crashed["crashed"], true);
    assert_eq!(crashed["stopped_by_editor"], false);
    assert!(tail_has(crashed, "client1 crashing on purpose"), "{crashed:#}");
    assert_eq!(state["state"], "running", "the server still runs");
    assert_eq!(process(&state, "[server]")["state"], "running");
    // Client 0 keeps playing.
    let state = wait_for(world, "client 0 in game", |s| {
        find(s, "[client 0]").is_some_and(|p| tail_has(p, "client0 state in_game"))
    });
    assert_eq!(process(&state, "[client 0]")["state"], "running");
    #[cfg(target_os = "linux")]
    let running = pids(&state);
    call(world, "editor.pie_net_run", json!({ "action": "stop" }));
    let stopped = processes(world);
    assert_eq!(stopped["alive"], 0);
    assert_eq!(
        process(&stopped, "[client 1]")["exit_code"],
        70,
        "the crash stays reported"
    );
    #[cfg(target_os = "linux")]
    for pid in running {
        assert!(pid_dead(pid), "pid {pid} still runs after stop");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Listen mode through play: starting play hosts the editor's PIE session on an ephemeral UDP port
/// and two windowed client processes ("PIE client k") join it; ending play stops them. Dropping the
/// resource (the editor exiting) kills a session that was never stopped.
#[test]
fn listen_mode_follows_play_and_the_editor_exit_kills_the_children() {
    let dir = project("pie-proc-listen");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    call(
        world,
        "editor.pie_net_config",
        json!({ "mode": "listen", "clients": 2, "separate_processes": true, "windowed": true }),
    );
    pie_net::sync_with_play(world, true);
    assert!(
        world.resource::<PieNet>().running(),
        "the editor hosts the listen session"
    );
    let mut joined = None;
    for _ in 0..MAX_WAITS {
        pie_net::sync_with_play(world, true);
        pie_net::step_frame(world, false);
        let state = processes(world);
        if state["peers"] == 2 {
            joined = Some(state);
            break;
        }
        wait(world);
    }
    let state = joined.unwrap_or_else(|| panic!("2 peers: {:#}", processes(world)));
    assert_eq!(state["mode"], "listen");
    assert_eq!(
        state["alive"], 2,
        "two client processes, no server process: {state:#}"
    );
    for k in 0..2 {
        let client = process(&state, &format!("[client {k}]"));
        assert!(tail_has(client, &format!("title PIE client {k}")), "{client:#}");
    }
    pie_net::sync_with_play(world, false);
    let stopped = processes(world);
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["alive"], 0);
    assert!(
        !world.resource::<PieNet>().running(),
        "the listen session ended with play"
    );

    // Started again, then the resource is dropped without a stop.
    pie_net::sync_with_play(world, true);
    let state = wait_for(world, "two clients started", |s| s["alive"] == 2);
    #[cfg(target_os = "linux")]
    let running = pids(&state);
    #[cfg(not(target_os = "linux"))]
    let _ = &state;
    assert_eq!(world.resource::<PieProcesses>().state(), ProcessState::Running);
    drop(world.remove_resource::<PieProcesses>());
    #[cfg(target_os = "linux")]
    for pid in running {
        assert!(pid_dead(pid), "pid {pid} outlived the editor's session");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Linux: the parent-death signal ends the children even when the editor never runs its own
/// cleanup (a crash, a kill): the session is started on a thread that then ends, and the resource
/// is leaked so nothing kills them.
#[cfg(target_os = "linux")]
#[test]
fn children_die_with_the_thread_that_started_them_without_any_cleanup() {
    let dir = project("pie-proc-orphan");
    let project_dir = dir.clone();
    let pids = std::thread::spawn(move || {
        let mut app = edit_app(&project_dir);
        let world = app.world_mut();
        call(
            world,
            "editor.pie_net_config",
            json!({ "mode": "dedicated", "clients": 1, "separate_processes": true }),
        );
        call(world, "editor.pie_net_run", json!({ "action": "start" }));
        let state = wait_for(world, "server and client started", |s| s["alive"] == 2);
        let pids = pids(&state);
        // No Drop: nothing in this process kills them.
        if let Some(procs) = world.remove_resource::<PieProcesses>() {
            std::mem::forget(procs);
        }
        std::mem::forget(app);
        pids
    })
    .join()
    .expect("thread");
    assert_eq!(pids.len(), 2);
    let mut dead = false;
    for _ in 0..MAX_WAITS {
        if pids.iter().all(|&pid| pid_dead(pid)) {
            dead = true;
            break;
        }
        // The kernel delivers SIGKILL on the thread's exit; this only bounds the check.
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(dead, "children of an ended thread still run: {pids:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The longest bounded wait on a Game tab's worker (it returns as soon as the condition holds).
const VIEW_WAIT: Duration = Duration::from_secs(60);

/// `play.get_diagnostics` of client `k`, over its own agent endpoint.
fn client_diagnostics(world: &World, k: u32) -> Value {
    let dir = world
        .resource::<PieProcesses>()
        .agent_dir(ProcessRole::Client(k))
        .expect("agent dir");
    let (endpoint, token) = regia_agent::endpoint::read(&dir).expect("endpoint");
    let mut client = regia_agent::client::Client::connect(&endpoint.url, &token).expect("connect");
    client
        .call("play.get_diagnostics", json!({}))
        .expect("diagnostics")
}

/// Waits until client `k`'s Game tab snapshot satisfies `done`; panics with it otherwise.
fn view_wait(world: &World, k: u32, what: &str, done: impl Fn(&ViewSnapshot) -> bool) -> ViewSnapshot {
    let views = world.resource::<GameViews>();
    let view = views.get(ProcessRole::Client(k)).expect("tab open");
    let worker = view.worker().expect("worker");
    let snapshot = worker.wait_until(VIEW_WAIT, &done);
    assert!(done(&snapshot), "client {k}: {what}: {snapshot:#?}");
    snapshot
}

/// The red channel of the left half of client `k`'s newest picture (the fixture lights it by the
/// keys its game read).
fn left_red(snapshot: &ViewSnapshot) -> u8 {
    let frame = snapshot.frame.as_ref().expect("a picture");
    let (x, y) = (10_usize, frame.height as usize / 2);
    frame.rgba[(y * frame.width as usize + x) * 4]
}

/// ADR-7831 (parity row P-286): with two client processes, each Game tab shows its own process's
/// pictures with a frame number; a key sent from client 0's tab reaches client 0 only (its
/// `play.get_diagnostics` counter and its game's own key count, which lights its picture); pause
/// then step advance exactly one frame of game time; speed and mute answer; closing a tab leaves
/// its process running, and only `stop_process` stops it.
#[test]
fn two_client_processes_show_in_game_tabs_and_input_reaches_only_its_process() {
    let dir = project("pie-proc-view");
    let mut app = edit_app(&dir);
    let world = app.world_mut();
    world.resource_mut::<PieProcesses>().extra_env =
        vec![("REGIA_PIE_FIXTURE_VIEW".to_owned(), "1".to_owned())];
    call(
        world,
        "editor.pie_net_config",
        json!({ "mode": "dedicated", "clients": 2, "separate_processes": true }),
    );
    call(world, "editor.pie_net_run", json!({ "action": "start" }));
    wait_for(world, "both clients in game", |s| s["peers"] == 2);

    for k in 0..2 {
        let open = call(
            world,
            "editor.pie_process_view",
            json!({ "process": format!("client {k}"), "open": true }),
        );
        assert_eq!(open["open"], true, "{open:#}");
        assert_eq!(open["process_running"], true, "{open:#}");
    }
    assert_eq!(
        world.resource::<GameViews>().roles(),
        vec![ProcessRole::Client(0), ProcessRole::Client(1)],
        "two tabs"
    );
    for k in 0..2 {
        let snapshot = view_wait(world, k, "two pictures", |s| s.frames_received >= 2);
        let frame = snapshot.frame.as_ref().expect("picture");
        assert_eq!((frame.width, frame.height), (160, 90), "the fixture's feed");
        assert!(frame.frame > 0, "a frame number");
        assert!(frame.latency_ms >= 0.0);
        assert_eq!(left_red(&snapshot), 0, "no key yet");
    }

    // A key pressed in client 0's tab reaches client 0 only.
    call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 0", "action": "input", "events": [
            { "type": "key", "key": "KeyW", "state": "pressed" },
            { "type": "key", "key": "KeyW", "state": "released" },
        ] }),
    );
    let after_key = view_wait(world, 0, "the key answered", |s| s.commands_done >= 1);
    assert_eq!(after_key.last_error, None, "{after_key:#?}");
    assert_eq!(client_diagnostics(world, 0)["injected_keys"], 2);
    assert_eq!(
        client_diagnostics(world, 1)["injected_keys"],
        0,
        "client 1 got nothing"
    );
    let seen = after_key.frames_received;
    let lit = view_wait(world, 0, "a picture after the key", |s| {
        s.frames_received > seen + 1 && left_red(s) > 0
    });
    assert_eq!(left_red(&lit), 80, "client 0's game read both key events");
    let other = view_wait(world, 1, "another picture", |s| s.frames_received >= 4);
    assert_eq!(left_red(&other), 0, "client 1's game read no key");

    // Pause, then step: exactly one frame of game time.
    call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 0", "action": "pause" }),
    );
    let paused = view_wait(world, 0, "pause answered", |s| s.commands_done >= 2);
    assert_eq!(paused.last_error, None, "{paused:#?}");
    let frozen = client_diagnostics(world, 0)["game_frames"].clone();
    let seen = paused.frames_received;
    view_wait(world, 0, "pictures while paused", |s| {
        s.frames_received >= seen + 3
    });
    assert_eq!(
        client_diagnostics(world, 0)["game_frames"],
        frozen,
        "no game time while paused"
    );
    call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 0", "action": "step" }),
    );
    let stepped = view_wait(world, 0, "step answered", |s| s.commands_done >= 3);
    assert_eq!(stepped.last_error, None, "{stepped:#?}");
    let frozen = frozen.as_u64().expect("game_frames");
    assert_eq!(
        client_diagnostics(world, 0)["game_frames"],
        frozen + 1,
        "one step, one frame"
    );
    let seen = stepped.frames_received;
    view_wait(world, 0, "pictures after the step", |s| {
        s.frames_received >= seen + 3
    });
    assert_eq!(
        client_diagnostics(world, 0)["game_frames"],
        frozen + 1,
        "paused again after the step"
    );
    let before = client_diagnostics(world, 1)["game_frames"]
        .as_u64()
        .expect("frames");
    let seen = view_wait(world, 1, "a picture", |_| true).frames_received;
    view_wait(world, 1, "more pictures", |s| s.frames_received >= seen + 3);
    assert!(
        client_diagnostics(world, 1)["game_frames"]
            .as_u64()
            .expect("frames")
            > before,
        "client 1 was not paused"
    );

    // Speed and mute reach the process.
    let state = call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 0", "action": "speed", "speed": 0.5 }),
    );
    assert_eq!(state["speed"], 0.5);
    let sped = view_wait(world, 0, "speed answered", |s| s.commands_done >= 4);
    assert_eq!(sped.last_error, None, "{sped:#?}");
    assert_eq!(sped.last_reply.as_ref().expect("reply")["answer"]["after"], 0.5);
    call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 0", "action": "mute" }),
    );
    let muted = view_wait(world, 0, "mute answered", |s| s.commands_done >= 5);
    assert_eq!(muted.last_error, None, "{muted:#?}");
    assert_eq!(muted.last_reply.as_ref().expect("reply")["answer"]["muted"], true);

    // Closing client 1's tab leaves its process running.
    let closed = call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 1", "open": false }),
    );
    assert_eq!(closed["open"], false);
    assert_eq!(closed["process_running"], true, "{closed:#}");
    let state = processes(world);
    assert_eq!(process(&state, "[client 1]")["state"], "running");
    // Closing client 0's tab with stop_process stops it (and only it).
    let stopped = call(
        world,
        "editor.pie_process_view",
        json!({ "process": "client 0", "open": false, "stop_process": true }),
    );
    assert_eq!(stopped["process_running"], false, "{stopped:#}");
    let state = processes(world);
    let client0 = process(&state, "[client 0]");
    assert_eq!(client0["state"], "exited");
    assert_eq!(client0["stopped_by_editor"], true);
    assert_eq!(process(&state, "[client 1]")["state"], "running");
    assert_eq!(process(&state, "[server]")["state"], "running");
    assert!(world.resource::<GameViews>().views().is_empty());

    call(world, "editor.pie_net_run", json!({ "action": "stop" }));
    assert_eq!(processes(world)["alive"], 0);
    let _ = std::fs::remove_dir_all(&dir);
}
