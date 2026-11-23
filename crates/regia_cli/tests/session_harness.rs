// SPDX-License-Identifier: MPL-2.0
//! Session harness tests (spec 41 § 8, tests 14-16). The fake child is this test executable itself,
//! re-run on `fake_child_entry`; its behavior comes from `REGIA_FAKE_MODE` (a role `env` entry).
//! No wall-clock asserts and no sleeps as synchronization: waits are counted attempts that yield.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use regia_agent::protocol::Endpoint;
use regia_cli::session::{
    AgentLink, Connector, Instance, LaunchOptions, Poll, Role, RoleKind, Session, SessionError,
    SessionStatus, launch,
};
use serde_json::{Value, json};

/// Entry point of the fake child. A normal test run has no `REGIA_FAKE_MODE`, so it returns at once.
#[test]
fn fake_child_entry() {
    let Ok(mode) = std::env::var("REGIA_FAKE_MODE") else {
        return;
    };
    let agent_dir = PathBuf::from(std::env::var("REGIA_AGENT_DIR").expect("harness sets REGIA_AGENT_DIR"));
    let instance_dir = agent_dir.parent().expect("instance dir").to_path_buf();
    let publish = || {
        let endpoint = Endpoint {
            url: "ws://127.0.0.1:1".into(),
            pid: std::process::id(),
            protocol: "0.1".into(),
            token_file: String::new(),
            app: "fake".into(),
        };
        regia_agent::endpoint::write(&agent_dir, &endpoint, "token").expect("publishes");
    };
    let wait_for_stdin_close = || {
        let mut sink = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut sink);
    };
    match mode.as_str() {
        "ok" => {
            publish();
            wait_for_stdin_close();
        }
        "capture_limit" => {
            let hint = std::env::var("REGIA_SESSION_LIMIT_FRAMES").unwrap_or_default();
            std::fs::write(instance_dir.join("frame-hint.txt"), hint).expect("frame hint");
            publish();
            wait_for_stdin_close();
        }
        "early_exit" => {
            eprintln!("boom before ready");
            std::process::exit(7);
        }
        "nonzero_exit" => {
            publish();
            wait_for_stdin_close();
            std::process::exit(5);
        }
        "panic_marker" => {
            publish();
            eprintln!("thread 'main' panicked at fake.rs:1:1");
            wait_for_stdin_close();
        }
        "error_line" => {
            publish();
            println!("ERROR disk on fire");
            println!("ERROR known flake");
            wait_for_stdin_close();
        }
        "crash" => {
            publish();
            std::fs::create_dir_all(instance_dir.join("crash")).expect("crash dir");
            std::fs::write(instance_dir.join("crash").join("bundle.dmp"), b"x").expect("bundle");
            wait_for_stdin_close();
        }
        "write_limit" => {
            publish();
            let limit =
                std::env::var(regia_cli::session::LIMIT_FRAMES_ENV).unwrap_or_else(|_| "unset".into());
            std::fs::write(instance_dir.join("limit.txt"), limit).expect("limit file");
            wait_for_stdin_close();
        }
        "die_once" => {
            publish();
            let marker = instance_dir.join("died_once");
            if !marker.exists() {
                std::fs::write(&marker, b"1").expect("marker");
                std::process::exit(3);
            }
            wait_for_stdin_close();
        }
        other => panic!("unknown fake mode {other}"),
    }
}

fn fake_exe() -> PathBuf {
    std::env::current_exe().expect("test exe")
}

fn role(name: &str, kind: RoleKind, count: u32, mode: &str) -> Role {
    Role {
        name: name.into(),
        kind,
        count,
        args: ["--exact", "fake_child_entry", "--nocapture", "--test-threads=1"]
            .map(String::from)
            .to_vec(),
        driver: None,
        render: false,
        env: vec![("REGIA_FAKE_MODE".into(), mode.into())],
        remote: None,
    }
}

fn session(roles: Vec<Role>) -> Session {
    let text = "(format_version: 1, build: Packaged(path: \"unused\"), roles: [(name: \"x\", kind: Game)], allow_errors: [\"known flake\"])";
    let mut s = Session::parse(text).expect("base session");
    s.roles = roles;
    s
}

fn options(tag: &str) -> LaunchOptions {
    let mut o = LaunchOptions::new(regia_test_utils::unique_temp_dir(tag), "s");
    o.exe_override = Some(fake_exe());
    o.drive_tests = false;
    o.poll = Poll {
        interval: Duration::ZERO,
        max_attempts: 2_000_000,
    };
    o
}

fn cleanup(o: &LaunchOptions) {
    let _ = std::fs::remove_dir_all(&o.artifacts_dir);
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("reads")).expect("json")
}

#[test]
fn each_instance_gets_its_own_agent_directory_and_roles_start_in_order() {
    let o = options("session_dirs");
    let s = session(vec![
        role("server", RoleKind::Server, 1, "ok"),
        role("client", RoleKind::Client, 4, "ok"),
    ]);
    let outcome = launch(&s, &o).expect("launches");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    let keys: Vec<_> = outcome.processes.iter().map(|p| p.instance.clone()).collect();
    assert_eq!(keys, ["server-0", "client-0", "client-1", "client-2", "client-3"]);
    let root = o.artifacts_dir.join("s");
    let mut pids = BTreeSet::new();
    for key in &keys {
        // Each instance published its endpoint into its own folder (the harness removed nothing).
        let (endpoint, token) = regia_agent::endpoint::read(&root.join(key).join("agent")).expect("endpoint");
        assert_eq!(token, "token");
        pids.insert(endpoint.pid);
    }
    assert_eq!(
        pids.len(),
        5,
        "five different processes, five different agent directories"
    );
    assert!(root.join("session-result.json").exists());
    assert!(!root.join("session-failure.json").exists());
    assert_eq!(outcome.exit_code(), 0);
    cleanup(&o);
}

#[test]
fn four_concurrent_sessions_never_collide() {
    let handles: Vec<_> = (0..4)
        .map(|n| {
            std::thread::spawn(move || {
                let o = options(&format!("session_conc{n}"));
                let s = session(vec![role("g", RoleKind::Game, 2, "ok")]);
                let outcome = launch(&s, &o).expect("launches");
                let ok = outcome.status == SessionStatus::Passed;
                cleanup(&o);
                ok
            })
        })
        .collect();
    for h in handles {
        assert!(h.join().expect("thread"), "a concurrent session failed");
    }
}

#[test]
fn a_child_exiting_before_ready_fails_with_exit_code_and_log_tail() {
    let o = options("session_early");
    let s = session(vec![role("server", RoleKind::Server, 1, "early_exit")]);
    match launch(&s, &o) {
        Err(SessionError::ExitedBeforeReady {
            instance,
            code,
            log_tail,
        }) => {
            assert_eq!(instance, "server-0");
            assert_eq!(code, Some(7));
            assert!(log_tail.contains("boom before ready"), "{log_tail}");
        }
        other => panic!("expected ExitedBeforeReady, got {other:?}"),
    }
    assert!(o.artifacts_dir.join("s/session-failure.json").exists());
    cleanup(&o);
}

#[test]
fn aggregation_each_failure_kind_fails_the_session_and_writes_the_failure_file() {
    for (mode, needle) in [
        ("nonzero_exit", "exited with Some(5)"),
        ("panic_marker", "marker:"),
        ("error_line", "ERROR disk on fire"),
        ("crash", "crash bundle"),
    ] {
        let o = options(&format!("session_agg_{mode}"));
        let s = session(vec![role("g", RoleKind::Game, 1, mode)]);
        let outcome = launch(&s, &o).expect("runs to a verdict");
        assert_eq!(outcome.status, SessionStatus::Failed, "{mode}");
        assert!(
            outcome.reasons.iter().any(|r| r.contains(needle)),
            "{mode}: {:?}",
            outcome.reasons
        );
        assert!(
            outcome.reasons.iter().all(|r| !r.contains("known flake")),
            "the allowed ERROR line must not fail the session"
        );
        let root = o.artifacts_dir.join("s");
        assert!(root.join("session-failure.json").exists(), "{mode}");
        assert!(!root.join("session-result.json").exists(), "{mode}");
        assert_eq!(read_json(&root.join("session-failure.json"))["status"], "failed");
        assert_eq!(outcome.exit_code(), 1);
        cleanup(&o);
    }
}

// -- recovery: scripted agent ---------------------------------------------------------------------

#[derive(Default)]
struct Script {
    connects: u32,
    run_filters: Vec<Value>,
}

struct FakeAgent {
    incarnation: u32,
    polls: u32,
    state: Rc<RefCell<Script>>,
}

impl AgentLink for FakeAgent {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let pass = |n: &str| json!({ "name": n, "verdict": "Passed", "message": "", "skipped": false });
        match method {
            "functest.list" => Ok(json!({ "tests": [
                { "name": "a", "enabled": true }, { "name": "b", "enabled": true }, { "name": "c", "enabled": true } ] })),
            "functest.run" => {
                self.state.borrow_mut().run_filters.push(params["filter"].clone());
                Ok(json!({ "started": 1 }))
            }
            "functest.status" if self.incarnation == 1 && self.polls >= 2 => Err("connection lost".into()),
            "functest.status" => {
                self.polls += 1;
                Ok(json!({ "state": "running", "finished": self.incarnation > 1 }))
            }
            "functest.results" if self.incarnation == 1 => Ok(json!({ "tests": [pass("a")] })),
            "functest.results" => Ok(json!({ "tests": [pass("b"), pass("c")] })),
            other => Err(format!("unexpected {other}")),
        }
    }
}

fn scripted(state: Rc<RefCell<Script>>) -> Connector {
    Box::new(move |_: &Instance, _: &Endpoint, _: &str| {
        let mut st = state.borrow_mut();
        st.connects += 1;
        // The first connection is the process's first incarnation; any later one follows a restart.
        let incarnation = if st.connects == 1 { 1 } else { 2 };
        drop(st);
        Ok(Box::new(FakeAgent {
            incarnation,
            polls: 0,
            state: state.clone(),
        }) as Box<dyn AgentLink>)
    })
}

#[test]
fn a_process_that_dies_once_is_restarted_and_completed_tests_are_skipped() {
    let state = Rc::new(RefCell::new(Script::default()));
    let mut o = options("session_recover");
    o.drive_tests = true;
    o.recover = true;
    o.connector = Some(scripted(state.clone()));
    let s = session(vec![role("g", RoleKind::Game, 1, "die_once")]);
    let outcome = launch(&s, &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    assert_eq!(outcome.processes[0].restarts, 1);
    let names: Vec<_> = outcome.processes[0].tests.iter().map(|t| t.0.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    let filters = state.borrow().run_filters.clone();
    assert_eq!(
        filters,
        vec![json!(["a", "b", "c"]), json!(["b", "c"])],
        "test a was not run again"
    );
    let progress = read_json(&o.artifacts_dir.join("s/session-progress.json"));
    assert_eq!(progress["completed"]["g-0"], json!(["a", "b", "c"]));
    assert_eq!(progress["restarts"]["g-0"], 1);
    cleanup(&o);
}

#[test]
fn exceeding_max_restarts_aborts_with_too_many_restarts() {
    let state = Rc::new(RefCell::new(Script::default()));
    let mut o = options("session_restarts");
    o.drive_tests = true;
    o.recover = true;
    o.connector = Some(scripted(state));
    let mut s = session(vec![role("g", RoleKind::Game, 1, "die_once")]);
    s.max_restarts = 0;
    let outcome = launch(&s, &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Aborted);
    assert!(outcome.reasons.contains(&"too_many_restarts".to_owned()));
    assert_eq!(outcome.exit_code(), 2);
    cleanup(&o);
}

// -- device instance (`Remote`): a scripted agent, nothing is started ------------------------------

#[derive(Default)]
struct Device {
    run_params: Vec<Value>,
    connects: Vec<(String, String)>,
    calls: Vec<String>,
}

struct DeviceAgent {
    device: Rc<RefCell<Device>>,
    verdict: &'static str,
    /// `functest.status` fails from this call on (the first call is the connect check).
    lose_at: Option<u32>,
    statuses: u32,
}

impl AgentLink for DeviceAgent {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.device.borrow_mut().calls.push(method.to_owned());
        if method == "functest.run" {
            self.device.borrow_mut().run_params.push(params);
        }
        match method {
            "functest.list" => Ok(json!({ "tests": [{ "name": "t", "enabled": true }] })),
            "functest.run" => Ok(json!({ "started": 1 })),
            "functest.status" => {
                self.statuses += 1;
                if self.lose_at.is_some_and(|at| self.statuses >= at) {
                    return Err("connection lost".into());
                }
                Ok(json!({ "state": "finished", "finished": self.statuses > 1 }))
            }
            "functest.results" => Ok(json!({ "tests": [
                { "name": "t", "verdict": self.verdict, "message": "", "skipped": false } ] })),
            "seeing.screenshot" => {
                use base64::Engine as _;
                Ok(
                    json!({"png_base64": base64::engine::general_purpose::STANDARD.encode(screenshot_png()), "width": 1, "height": 1}),
                )
            }
            "play.pause" => Ok(json!({})),
            other => Err(format!("unexpected {other}")),
        }
    }
}

fn device_connector(device: Rc<RefCell<Device>>, verdict: &'static str, lose_at: Option<u32>) -> Connector {
    Box::new(move |_: &Instance, endpoint: &Endpoint, token: &str| {
        device
            .borrow_mut()
            .connects
            .push((endpoint.url.clone(), token.to_owned()));
        Ok(Box::new(DeviceAgent {
            device: device.clone(),
            verdict,
            lose_at,
            statuses: 0,
        }) as Box<dyn AgentLink>)
    })
}

/// A project with the agent files `regia install-android --attach` writes.
fn attached_project(tag: &str) -> PathBuf {
    let project = regia_test_utils::unique_temp_dir(tag);
    let dir = regia_agent::endpoint::agent_dir(&project);
    let endpoint = Endpoint {
        url: "ws://127.0.0.1:41234".into(),
        pid: 1,
        protocol: "0.2".into(),
        token_file: String::new(),
        app: "device".into(),
    };
    regia_agent::endpoint::write(&dir, &endpoint, "device-token").expect("writes the agent files");
    project
}

fn remote_session(address: &str) -> Session {
    Session::parse(&format!(
        "(format_version: 1, build: Remote(address: \"{address}\"), roles: [(name: \"phone\", kind: Game)], \
         script: [(role: \"phone\", call: \"play.pause\")])"
    ))
    .expect("remote session")
}

fn remote_options(tag: &str, project: &Path) -> LaunchOptions {
    let mut o = options(tag);
    o.exe_override = None;
    o.drive_tests = true;
    o.base_dir = project.to_path_buf();
    o.poll.max_attempts = 3;
    o
}

#[test]
fn a_remote_instance_is_driven_and_nothing_is_started() {
    let project = attached_project("session_remote_ok");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_ok_out", &project);
    o.connector = Some(device_connector(device.clone(), "Passed", None));
    let outcome = launch(&remote_session("attached"), &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    assert_eq!(outcome.exit_code(), 0);
    let p = &outcome.processes[0];
    assert_eq!(p.instance, "phone-0");
    assert!(p.remote);
    assert_eq!(p.exit_code, None, "the harness owns no process");
    assert_eq!(p.tests.len(), 1);
    let d = device.borrow();
    assert_eq!(
        d.connects,
        [("ws://127.0.0.1:41234".to_owned(), "device-token".to_owned())],
        "the attached endpoint and its token"
    );
    assert!(
        d.calls.iter().any(|c| c == "play.pause"),
        "script calls reach the device: {:?}",
        d.calls
    );
    let result = read_json(&o.artifacts_dir.join("s/session-result.json"));
    assert_eq!(result["processes"][0]["remote"], true);
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_remote_address_replaces_the_url_and_keeps_the_token() {
    let project = attached_project("session_remote_addr");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_addr_out", &project);
    o.connector = Some(device_connector(device.clone(), "Passed", None));
    let outcome = launch(&remote_session("127.0.0.1:7777"), &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    assert_eq!(
        device.borrow().connects,
        [("ws://127.0.0.1:7777".to_owned(), "device-token".to_owned())]
    );
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_session_file_in_a_subfolder_finds_the_project_agent_files() {
    let project = attached_project("session_remote_sub");
    std::fs::write(project.join("Regia.toml"), "").expect("writes the manifest");
    let tests = project.join("tests");
    std::fs::create_dir_all(&tests).expect("creates the subfolder");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_sub_out", &tests);
    o.connector = Some(device_connector(device.clone(), "Passed", None));
    let outcome = launch(&remote_session("attached"), &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_failed_test_on_a_device_fails_the_session() {
    let project = attached_project("session_remote_fail");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_fail_out", &project);
    o.connector = Some(device_connector(device, "Failed", None));
    let outcome = launch(&remote_session("attached"), &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Failed);
    assert_eq!(outcome.exit_code(), 1);
    assert!(o.artifacts_dir.join("s/session-failure.json").is_file());
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_lost_device_connection_fails_the_session_and_is_not_restarted() {
    let project = attached_project("session_remote_lost");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_lost_out", &project);
    o.recover = true;
    o.connector = Some(device_connector(device.clone(), "Passed", Some(2)));
    let outcome = launch(&remote_session("attached"), &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Failed);
    assert_eq!(outcome.processes[0].restarts, 0);
    assert!(
        outcome.reasons.iter().any(|r| r.contains("connection lost")),
        "{:?}",
        outcome.reasons
    );
    assert_eq!(device.borrow().connects.len(), 1, "one connection, no retry");
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_remote_session_without_attached_agent_files_says_how_to_attach() {
    let project = regia_test_utils::unique_temp_dir("session_remote_none");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_none_out", &project);
    o.connector = Some(device_connector(device.clone(), "Passed", None));
    let err = match launch(&remote_session("attached"), &o) {
        Ok(outcome) => panic!("no agent files, but {:?}", outcome.status),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("install-android --attach"), "{err}");
    assert!(device.borrow().connects.is_empty());
    assert!(o.artifacts_dir.join("s/session-failure.json").is_file());
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_hand_built_remote_session_with_two_instances_is_refused() {
    let project = attached_project("session_remote_two");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut o = remote_options("session_remote_two_out", &project);
    o.connector = Some(device_connector(device.clone(), "Passed", None));
    let mut s = remote_session("attached");
    s.roles[0].count = 2;
    assert!(matches!(launch(&s, &o), Err(SessionError::Invalid(_))));
    assert!(device.borrow().connects.is_empty());
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

// -- a device role inside a local session (`remote` on a role, format v2, ADR-2137) ----------------

/// Records which instance connected where; every link is a `DeviceAgent`.
fn mixed_connector(
    device: Rc<RefCell<Device>>,
    seen: Rc<RefCell<Vec<(String, String)>>>,
    lose_at: Option<u32>,
) -> Connector {
    Box::new(move |instance: &Instance, endpoint: &Endpoint, token: &str| {
        seen.borrow_mut().push((instance.key(), endpoint.url.clone()));
        device
            .borrow_mut()
            .connects
            .push((endpoint.url.clone(), token.to_owned()));
        // Only the device link loses its connection.
        let lose = if token == "device-token" { lose_at } else { None };
        Ok(Box::new(DeviceAgent {
            device: device.clone(),
            verdict: "Passed",
            lose_at: lose,
            statuses: 0,
        }) as Box<dyn AgentLink>)
    })
}

fn device_role(name: &str, address: &str) -> Role {
    Role {
        remote: Some(address.into()),
        env: Vec::new(),
        args: Vec::new(),
        ..role(name, RoleKind::Client, 1, "ok")
    }
}

fn mixed_session(roles: Vec<Role>) -> Session {
    let text = "(format_version: 2, build: Packaged(path: \"unused\"), roles: [(name: \"x\", kind: Game)], \
                script: [(role: \"phone\", call: \"play.pause\")])";
    let mut s = Session::parse(text).expect("base session");
    s.roles = roles;
    s
}

fn mixed_options(tag: &str, project: &Path) -> LaunchOptions {
    let mut o = options(tag);
    o.drive_tests = true;
    o.base_dir = project.to_path_buf();
    o
}

#[test]
fn a_desktop_server_and_a_device_client_run_as_one_session() {
    let project = attached_project("session_mixed_ok");
    let device = Rc::new(RefCell::new(Device::default()));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut o = mixed_options("session_mixed_ok_out", &project);
    o.connector = Some(mixed_connector(device.clone(), seen.clone(), None));
    let s = mixed_session(vec![
        role("server", RoleKind::Server, 1, "ok"),
        device_role("phone", "attached"),
    ]);
    let outcome = launch(&s, &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    let keys: Vec<&str> = outcome.processes.iter().map(|p| p.instance.as_str()).collect();
    assert_eq!(keys, ["server-0", "phone-0"], "listed order");
    let (server, phone) = (&outcome.processes[0], &outcome.processes[1]);
    assert!(!server.remote);
    assert_eq!(
        server.exit_code,
        Some(0),
        "the harness started and stopped the server"
    );
    assert!(phone.remote);
    assert_eq!(phone.exit_code, None, "the harness owns no device process");
    assert_eq!((server.tests.len(), phone.tests.len()), (1, 1));
    assert_eq!(
        *seen.borrow(),
        [
            ("server-0".to_owned(), "ws://127.0.0.1:1".to_owned()),
            ("phone-0".to_owned(), "ws://127.0.0.1:41234".to_owned()),
        ],
        "the server first, then the attached device"
    );
    let device = device.borrow();
    assert_eq!(device.connects[1].1, "device-token");
    assert!(
        device.calls.iter().any(|c| c == "play.pause"),
        "script calls reach the device role: {:?}",
        device.calls
    );
    let dirs = o.artifacts_dir.join("s");
    assert!(dirs.join("server-0/agent").is_dir());
    assert!(dirs.join("phone-0").is_dir());
    assert!(
        !dirs.join("phone-0/agent").exists(),
        "no agent directory for a device"
    );
    let result = read_json(&dirs.join("session-result.json"));
    assert_eq!(result["processes"][1]["remote"], true);
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_lost_device_role_fails_the_session_is_not_restarted_and_the_server_is_stopped() {
    let project = attached_project("session_mixed_lost");
    let device = Rc::new(RefCell::new(Device::default()));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut o = mixed_options("session_mixed_lost_out", &project);
    o.recover = true;
    o.connector = Some(mixed_connector(device, seen.clone(), Some(2)));
    let s = mixed_session(vec![
        role("server", RoleKind::Server, 1, "ok"),
        device_role("phone", "attached"),
    ]);
    let outcome = launch(&s, &o).expect("runs");
    assert_eq!(outcome.status, SessionStatus::Failed);
    assert!(
        outcome
            .reasons
            .iter()
            .any(|r| r.contains("phone-0") && r.contains("connection lost")),
        "{:?}",
        outcome.reasons
    );
    assert_eq!(outcome.processes[1].restarts, 0);
    assert_eq!(seen.borrow().len(), 2, "one connection each, no retry");
    assert_eq!(outcome.processes[0].exit_code, Some(0));
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_device_role_without_attached_agent_files_stops_the_started_roles() {
    let project = regia_test_utils::unique_temp_dir("session_mixed_none");
    let device = Rc::new(RefCell::new(Device::default()));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut o = mixed_options("session_mixed_none_out", &project);
    o.connector = Some(mixed_connector(device, seen.clone(), None));
    let s = mixed_session(vec![
        role("server", RoleKind::Server, 1, "ok"),
        device_role("phone", "attached"),
    ]);
    let err = match launch(&s, &o) {
        Ok(outcome) => panic!("no agent files, but {:?}", outcome.status),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("install-android --attach"), "{err}");
    assert_eq!(seen.borrow().len(), 1, "only the server connected");
    assert!(o.artifacts_dir.join("s/session-failure.json").is_file());
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

#[test]
fn a_hand_built_session_with_a_bad_device_role_is_refused_before_anything_starts() {
    let project = attached_project("session_mixed_bad");
    let device = Rc::new(RefCell::new(Device::default()));
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut o = mixed_options("session_mixed_bad_out", &project);
    o.connector = Some(mixed_connector(device, seen.clone(), None));
    let mut two = device_role("phone", "attached");
    two.count = 2;
    let s = mixed_session(vec![role("server", RoleKind::Server, 1, "ok"), two]);
    assert!(matches!(launch(&s, &o), Err(SessionError::Invalid(_))));
    let mut v1 = mixed_session(vec![device_role("phone", "attached")]);
    v1.format_version = 1;
    assert!(matches!(launch(&v1, &o), Err(SessionError::Invalid(_))));
    assert!(seen.borrow().is_empty(), "nothing connected");
    assert!(!o.artifacts_dir.join("s/server-0").exists(), "nothing started");
    cleanup(&o);
    let _ = std::fs::remove_dir_all(&project);
}

// -- keep.screenshots_on_failure (spec 41 § 8, ADR-2755) ----------------------------------------

/// A one-pixel PNG header worth of bytes, base64: what a fake `seeing.screenshot` answers.
const FAKE_PNG_B64: &str = "iVBORw0KGgo=";

struct ShotAgent {
    verdict: &'static str,
    screenshot: Result<(), &'static str>,
    calls: Rc<RefCell<Vec<String>>>,
}

impl AgentLink for ShotAgent {
    fn call(&mut self, method: &str, _params: Value) -> Result<Value, String> {
        self.calls.borrow_mut().push(method.to_owned());
        match method {
            "functest.list" => Ok(json!({ "tests": [{ "name": "t", "enabled": true }] })),
            "functest.run" => Ok(json!({ "started": 1 })),
            "functest.status" => Ok(json!({ "state": "finished", "finished": true })),
            "functest.results" => Ok(json!({ "tests": [
                { "name": "t", "verdict": self.verdict, "message": "", "skipped": false } ] })),
            "seeing.screenshot" => match self.screenshot {
                Ok(()) => Ok(json!({ "png_base64": FAKE_PNG_B64, "width": 1, "height": 1 })),
                Err(e) => Err(e.to_owned()),
            },
            other => Err(format!("unexpected {other}")),
        }
    }
}

fn shot_connector(
    verdict: &'static str,
    screenshot: Result<(), &'static str>,
    calls: Rc<RefCell<Vec<String>>>,
) -> Connector {
    Box::new(move |_: &Instance, _: &Endpoint, _: &str| {
        Ok(Box::new(ShotAgent {
            verdict,
            screenshot,
            calls: calls.clone(),
        }) as Box<dyn AgentLink>)
    })
}

/// Runs a one-role session; returns the outcome, the options (for the artifacts) and the agent calls.
fn run_shot_session(
    tag: &str,
    keep: bool,
    render: bool,
    verdict: &'static str,
    screenshot: Result<(), &'static str>,
) -> (regia_cli::session::SessionOutcome, LaunchOptions, Vec<String>) {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut o = options(tag);
    o.drive_tests = true;
    o.connector = Some(shot_connector(verdict, screenshot, calls.clone()));
    let mut r = role("g", RoleKind::Game, 1, "ok");
    r.render = render;
    let mut s = session(vec![r]);
    s.keep.screenshots_on_failure = keep;
    let outcome = launch(&s, &o).expect("runs");
    let seen = calls.borrow().clone();
    (outcome, o, seen)
}

#[test]
fn a_failed_test_saves_a_screenshot_and_names_it_in_the_result() {
    let (outcome, o, calls) = run_shot_session("shot_saved", true, true, "Failed", Ok(()));
    assert_eq!(outcome.status, SessionStatus::Failed);
    let shots = &outcome.processes[0].screenshots;
    assert_eq!(shots.len(), 1, "{shots:?}");
    assert_eq!(shots[0].step, "test:t");
    assert_eq!(shots[0].file.as_deref(), Some("g-0/failure-1.png"));
    assert_eq!(shots[0].reason, None);
    let png =
        std::fs::read(o.artifacts_dir.join("s/g-0/failure-1.png")).expect("the PNG is next to the logs");
    assert_eq!(png, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    assert!(calls.iter().any(|c| c == "seeing.screenshot"));
    let failure = read_json(&o.artifacts_dir.join("s/session-failure.json"));
    assert_eq!(
        failure["processes"][0]["screenshots"][0]["file"],
        "g-0/failure-1.png"
    );
    cleanup(&o);
}

#[test]
fn keep_false_saves_no_screenshot_and_does_not_ask_for_one() {
    let (outcome, o, calls) = run_shot_session("shot_off", false, true, "Failed", Ok(()));
    assert_eq!(outcome.status, SessionStatus::Failed);
    assert!(outcome.processes[0].screenshots.is_empty());
    assert!(!o.artifacts_dir.join("s/g-0/failure-1.png").exists());
    assert!(!calls.iter().any(|c| c == "seeing.screenshot"));
    cleanup(&o);
}

#[test]
fn a_passing_session_takes_no_screenshot() {
    let (outcome, o, calls) = run_shot_session("shot_pass", true, true, "Passed", Ok(()));
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    assert!(outcome.processes[0].screenshots.is_empty());
    assert!(!calls.iter().any(|c| c == "seeing.screenshot"));
    cleanup(&o);
}

#[test]
fn a_role_that_does_not_render_records_why_there_is_no_screenshot() {
    let (outcome, o, calls) = run_shot_session("shot_headless", true, false, "Failed", Ok(()));
    let shots = &outcome.processes[0].screenshots;
    assert_eq!(shots.len(), 1);
    assert_eq!(shots[0].file, None);
    assert_eq!(shots[0].reason.as_deref(), Some("the role does not render"));
    assert!(!calls.iter().any(|c| c == "seeing.screenshot"));
    cleanup(&o);
}

#[test]
fn no_display_records_the_reason_and_never_fails_the_run_by_itself() {
    let (failed, o1, _) = run_shot_session("shot_nodisplay", true, true, "Failed", Err("no window"));
    let shots = &failed.processes[0].screenshots;
    assert_eq!(shots[0].file, None);
    assert!(
        shots[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("no window")),
        "{shots:?}"
    );
    // The reasons are the test's only: the screenshot error is not one of them.
    assert_eq!(failed.reasons.len(), 1, "{:?}", failed.reasons);
    let (plain, o2, _) = run_shot_session("shot_nodisplay_ref", false, true, "Failed", Ok(()));
    assert_eq!(failed.reasons, plain.reasons);
    cleanup(&o1);
    cleanup(&o2);
}

#[test]
fn the_arena_smoke_session_file_parses_and_runs_the_smoke_tests() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/arena/smoke.session.ron");
    let text = std::fs::read_to_string(&path).expect("samples/arena/smoke.session.ron exists");
    let session = Session::parse(&text).expect("the arena smoke session parses");
    assert_eq!(session.roles.len(), 1);
    assert_eq!(session.roles[0].name, "arena");
    assert!(
        session.roles[0].args.iter().any(|a| a == "--smoke"),
        "the role starts the smoke test"
    );
    assert_eq!(session.tests.tags, ["smoke"]);
    assert!(session.keep.screenshots_on_failure);
}

#[test]
fn limit_frames_reaches_every_role_through_the_environment() {
    let o = options("session_limit");
    let mut s = session(vec![role("g", RoleKind::Game, 2, "write_limit")]);
    s.limit_frames = 600;
    let outcome = launch(&s, &o).expect("launches");
    assert_eq!(outcome.status, SessionStatus::Passed, "{:?}", outcome.reasons);
    for instance in ["g-0", "g-1"] {
        let text = std::fs::read_to_string(o.artifacts_dir.join("s").join(instance).join("limit.txt"))
            .expect("the child wrote its limit");
        assert_eq!(text, "600");
    }
    let mut none = session(vec![role("g", RoleKind::Game, 1, "write_limit")]);
    none.limit_frames = 0;
    let o2 = options("session_limit_none");
    launch(&none, &o2).expect("launches");
    let text = std::fs::read_to_string(o2.artifacts_dir.join("s/g-0/limit.txt")).expect("file");
    assert_eq!(text, "unset", "no limit, no variable");
    cleanup(&o);
    cleanup(&o2);
}

#[test]
fn audit_aborted_and_undecided_device_verdicts_keep_exit_code_two() {
    for (verdict, status, code) in [
        ("Passed", SessionStatus::Passed, 0),
        ("Failed", SessionStatus::Failed, 1),
        ("Aborted", SessionStatus::Aborted, 2),
        ("Undecided", SessionStatus::Aborted, 2),
    ] {
        let project = attached_project("session-audit-verdict");
        let device = Rc::new(RefCell::new(Device::default()));
        let mut options = remote_options("session-audit-verdict-out", &project);
        options.connector = Some(device_connector(device, verdict, None));
        let outcome = launch(&remote_session("attached"), &options).expect("scripted verdict");
        assert_eq!(outcome.status, status, "{verdict}");
        assert_eq!(outcome.exit_code(), code, "{verdict}");
        cleanup(&options);
        std::fs::remove_dir_all(project).expect("cleanup");
    }
}

#[test]
fn audit_session_frame_hint_reaches_a_local_role() {
    let options = options("session-frame-hint");
    let mut session = session(vec![role("game", RoleKind::Game, 1, "capture_limit")]);
    session.limit_frames = 10;
    launch(&session, &options).expect("launch hint role");
    assert_eq!(
        std::fs::read_to_string(options.artifacts_dir.join("s/game-0/frame-hint.txt")).expect("hint"),
        "10"
    );
    cleanup(&options);
}

#[test]
fn audit_session_frame_hint_reaches_remote_functional_test_params() {
    let project = attached_project("session-remote-frame-hint");
    let device = Rc::new(RefCell::new(Device::default()));
    let mut options = remote_options("session-remote-frame-hint-out", &project);
    options.connector = Some(device_connector(device.clone(), "Passed", None));
    let mut session = remote_session("attached");
    session.limit_frames = 10;
    launch(&session, &options).expect("run remote");
    assert_eq!(
        device.borrow().run_params[0]["params"]["regia_session_limit_frames"],
        "10"
    );
    cleanup(&options);
    std::fs::remove_dir_all(project).expect("cleanup");
}

fn screenshot_png() -> Vec<u8> {
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255])))
        .write_to(&mut encoded, image::ImageFormat::Png)
        .expect("PNG fixture");
    encoded.into_inner()
}

#[test]
fn audit_failure_screenshots_follow_keep_render_and_verdict() {
    for (verdict, keep, render, wanted) in [
        ("Failed", true, true, true),
        ("Failed", false, true, false),
        ("Failed", true, false, false),
        ("Passed", true, true, false),
        ("Aborted", true, true, true),
        ("Undecided", true, true, true),
    ] {
        let project = attached_project("session-capture-policy");
        let device = Rc::new(RefCell::new(Device::default()));
        let mut options = remote_options("session-capture-policy-out", &project);
        options.connector = Some(device_connector(device.clone(), verdict, None));
        let mut session = remote_session("attached");
        session.roles[0].render = render;
        session.keep.screenshots_on_failure = keep;
        launch(&session, &options).expect("capture policy run");
        let path = options.artifacts_dir.join("s/phone-0/failure.png");
        assert_eq!(path.is_file(), wanted, "{verdict} keep={keep} render={render}");
        assert_eq!(
            device
                .borrow()
                .calls
                .iter()
                .any(|call| call == "seeing.screenshot"),
            wanted
        );
        if wanted {
            assert_eq!(std::fs::read(path).expect("PNG"), screenshot_png());
        }
        cleanup(&options);
        std::fs::remove_dir_all(project).expect("cleanup");
    }
}

#[test]
fn audit_local_failure_is_captured_before_its_process_is_stopped() {
    let device = Rc::new(RefCell::new(Device::default()));
    let mut options = options("session-local-failure-capture");
    options.drive_tests = true;
    options.connector = Some(device_connector(device.clone(), "Failed", None));
    let mut session = session(vec![role("game", RoleKind::Game, 1, "ok")]);
    session.roles[0].render = true;
    session.keep.screenshots_on_failure = true;
    let outcome = launch(&session, &options).expect("local failure");
    assert_eq!(outcome.exit_code(), 1);
    assert_eq!(
        std::fs::read(options.artifacts_dir.join("s/game-0/failure.png")).expect("capture"),
        screenshot_png()
    );
    assert!(
        device
            .borrow()
            .calls
            .iter()
            .any(|call| call == "seeing.screenshot")
    );
    cleanup(&options);
}
