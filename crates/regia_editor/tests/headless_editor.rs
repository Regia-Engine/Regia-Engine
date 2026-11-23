// SPDX-License-Identifier: MPL-2.0
//! The real `regia-editor --headless` process (ADR-2412): it starts on a fixture project with no
//! window and no GPU device, prints its ready line once the agent server listens, answers agent
//! calls (open a level, a query, an edit, a save, the generators), and ends with status 0 on
//! `agent.shutdown` (and SIGTERM on Unix) with the dirty level saved.
//!
//! Every wait is on a line from the editor's stdout or on the process, bounded by `HANG_BOUND`
//! (a bound on a hang, not on speed): no sleeps, no polling of files.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use regia_agent::client::{Client, ClientError};
use regia_test_utils::process::{Running, wait_within};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

#[path = "headless_editor/designer_acceptance.rs"]
mod designer_acceptance;
#[path = "headless_editor/render_out_acceptance.rs"]
mod render_out_acceptance;
#[path = "headless_editor/tools_acceptance.rs"]
mod tools_acceptance;
#[path = "headless_editor/viewport_ops_acceptance.rs"]
mod viewport_ops_acceptance;

/// The longest wait for an editor that stopped making progress (a loaded gate is slow).
const HANG_BOUND: Duration = Duration::from_secs(600);
/// The engine's GPU request variables (`regia_render::gpu_select`), without their prefix.
const GPU_REQUEST_ENV: [&str; 5] = ["GPU_UUID", "GPU_PCI", "GPU_INDEX", "GPU_NAME", "GPU_STRICT"];

const GROUND: &str = r#"
format_version = 1

[generator]
kind = "heightfield"
seed = 11
description = "fixture ground"
outputs = ["assets/gen/ground.r16"]

[generator.heightfield]
width = 32
height = 32
"#;

/// A fixture project and the editor's home, removed at the end of the test.
struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = unique_temp_dir(tag);
        std::fs::create_dir_all(dir.join("assets/maps")).expect("assets");
        std::fs::create_dir_all(dir.join("generators")).expect("generators");
        std::fs::create_dir_all(dir.join(".regia")).expect(".regia");
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"Headless\"\nid = \"com.example.headless\"\n",
        )
        .expect("manifest");
        // `gen.run` / `gen.verify` run project code: the fixture grants `build`.
        std::fs::write(
            dir.join(".regia/agent.toml"),
            "permissions = [\"read\", \"edit_scene\", \"assets\", \"play\", \"build\"]\n",
        )
        .expect("agent.toml");
        std::fs::write(dir.join("generators/ground.toml"), GROUND).expect("generator");
        Self(dir.canonicalize().expect("canonical"))
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.0.join("editor.err")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running headless editor and its stdout lines.
struct Editor {
    running: Running,
    lines: Receiver<String>,
}

impl Editor {
    fn start(fixture: &Fixture) -> Self {
        Self::start_with(fixture, &["--no-gpu"], &[])
    }

    fn start_with(fixture: &Fixture, flags: &[&str], env: &[(&str, &str)]) -> Self {
        Self::spawn(fixture, flags, env, Stdio::null())
    }

    /// Starts it with a stdin pipe the test holds.
    fn start_piped(fixture: &Fixture, flags: &[&str]) -> Self {
        Self::spawn(fixture, flags, &[], Stdio::piped())
    }

    fn spawn(fixture: &Fixture, flags: &[&str], env: &[(&str, &str)], stdin: Stdio) -> Self {
        let err = std::fs::File::create(fixture.0.join("editor.err")).expect("stderr log");
        // Cross-built test executables can run on a native host with the editor beside them.
        let binary = std::env::var_os("REGIA_EDITOR_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_regia-editor").into());
        let mut command = Command::new(binary);
        command.arg("--headless").args(flags);
        for (key, value) in env {
            command.env(key, value);
        }
        if env.iter().any(|(key, _)| *key == "VK_LOADER_DRIVERS_SELECT") {
            // The test picks the driver itself: a card request inherited from the runner (the
            // GPU pass pins a card, strictly) names an adapter this loader filter hides.
            for name in GPU_REQUEST_ENV {
                for prefix in ["REGIA_", "FERRITE_"] {
                    command.env_remove(format!("{prefix}{name}"));
                }
            }
        }
        let mut child: Child = command
            .arg("--project")
            .arg(&fixture.0)
            .env("HOME", &fixture.0)
            .env("XDG_CONFIG_HOME", fixture.0.join("config"))
            .env("XDG_DATA_HOME", fixture.0.join("data"))
            .env("APPDATA", fixture.0.join("appdata"))
            .env_remove("REGIA_AGENT_DIR")
            .env_remove("FERRITE_AGENT_DIR")
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("start the headless editor");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            running: Running(child),
            lines,
        }
    }

    /// The next stdout line that is a JSON event `event`.
    fn event(&self, event: &str, fixture: &Fixture) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(HANG_BOUND)
                .unwrap_or_else(|e| panic!("no `{event}` line ({e}); stderr:\n{}", fixture.log()));
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && value["event"] == event
            {
                return value;
            }
        }
    }

    fn wait(&mut self) -> ExitStatus {
        wait_within(&mut self.running.0, HANG_BOUND).expect("wait for the editor")
    }
}

fn connect(fixture: &Fixture) -> Client {
    let mut client = Client::connect_project(&fixture.0).expect("connect to the ready editor");
    client.set_read_timeout(Some(HANG_BOUND)).expect("read timeout");
    client
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    client
        .call(method, params)
        .unwrap_or_else(|e| panic!("{method}: {e}"))
}

fn error_code(result: Result<Value, ClientError>) -> String {
    match result {
        Err(ClientError::Rpc(err)) => err
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

fn level_text(fixture: &Fixture, rel: &str) -> String {
    std::fs::read_to_string(fixture.0.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Creates `assets/maps/room.level.ron` holding a `Crate` through the protocol and reopens it.
fn make_and_open_room(client: &mut Client) {
    call(
        client,
        "editor.spawn",
        json!({ "name": "Crate", "translation": [1.0, 0.0, 2.0] }),
    );
    let saved = call(
        client,
        "editor.save_level",
        json!({ "path": "assets/maps/room.level.ron" }),
    );
    assert!(saved.to_string().contains("room.level.ron"), "{saved}");
    let opened = call(
        client,
        "editor.open_level",
        json!({ "path": "assets/maps/room.level.ron" }),
    );
    assert_eq!(opened["path"], "assets/maps/room.level.ron");
    assert!(opened["entities"].as_u64().unwrap_or(0) >= 1, "{opened}");
}

#[test]
fn the_headless_editor_answers_agent_calls_and_saves_on_shutdown() {
    let fixture = Fixture::new("headless-editor-shutdown");
    let mut editor = Editor::start(&fixture);
    let ready = editor.event("ready", &fixture);
    assert_eq!(ready["adapter"], Value::Null, "--no-gpu: {ready}");
    assert!(
        ready["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("ws://127.0.0.1:")),
        "{ready}"
    );
    let mut client = connect(&fixture);

    let status = call(&mut client, "editor.headless_status", json!({}));
    assert_eq!(status["headless"], true, "{status}");
    assert_eq!(status["adapter"], Value::Null);
    let hello = call(&mut client, "agent.hello", json!({}));
    assert!(
        hello.to_string().contains("generators"),
        "capabilities name generators: {hello}"
    );

    // A level: create it, save it, open it, query it.
    make_and_open_room(&mut client);
    let found = call(&mut client, "scene.query_entities", json!({ "name": "Crate" }));
    assert!(found.to_string().contains("Crate"), "{found}");
    let status = call(&mut client, "editor.headless_status", json!({}));
    assert_eq!(status["level"], "assets/maps/room.level.ron");
    assert_eq!(status["dirty"], false);

    // A view without an adapter answers editor_headless.
    assert_eq!(
        error_code(client.call("editor.capture_viewport", json!({}))),
        "editor_headless"
    );

    // Generators through the editor.
    let listed = call(&mut client, "gen.list", json!({}));
    assert_eq!(listed["generators"][0]["name"], "ground", "{listed}");
    let ran = call(&mut client, "gen.run", json!({ "name": "ground" }));
    assert_eq!(ran["outputs"][0]["path"], "assets/gen/ground.r16", "{ran}");
    let verified = call(&mut client, "gen.verify", json!({}));
    assert_eq!(verified["ok"], true, "{verified}");

    // An unsaved edit, then shutdown: the level is saved before the process ends.
    call(&mut client, "editor.spawn", json!({ "name": "Barrel" }));
    assert_eq!(
        call(&mut client, "editor.headless_status", json!({}))["dirty"],
        true
    );
    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    assert_eq!(out["saved"], json!(["assets/maps/room.level.ron"]), "{out}");
    let exit = editor.event("exit", &fixture);
    assert_eq!(exit["cause"], "agent.shutdown", "{exit}");
    assert_eq!(exit["status"], 0, "{exit}");
    let status = editor.wait();
    assert_eq!(status.code(), Some(0), "{status:?}; stderr:\n{}", fixture.log());
    assert!(!fixture.log().contains("panicked"), "{}", fixture.log());
    let text = level_text(&fixture, "assets/maps/room.level.ron");
    assert!(text.contains("Barrel") && text.contains("Crate"), "{text}");
    assert!(
        !fixture.0.join(".regia/agent/endpoint.json").exists(),
        "the endpoint is removed on exit"
    );
}

#[cfg(unix)]
#[test]
fn sigterm_saves_the_dirty_level_and_exits_cleanly() {
    let fixture = Fixture::new("headless-editor-sigterm");
    let mut editor = Editor::start(&fixture);
    let ready = editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    call(&mut client, "editor.spawn", json!({ "name": "Lamp" }));
    assert_eq!(
        call(&mut client, "editor.headless_status", json!({}))["dirty"],
        true
    );

    let pid = ready["pid"].as_u64().expect("pid").to_string();
    let kill = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("run kill");
    assert!(kill.success(), "kill -TERM {pid}");
    let exit = editor.event("exit", &fixture);
    assert_eq!(exit["cause"], "signal", "{exit}");
    assert_eq!(exit["saved"], json!(["assets/maps/room.level.ron"]), "{exit}");
    let status = editor.wait();
    assert_eq!(status.code(), Some(0), "{status:?}; stderr:\n{}", fixture.log());
    let text = level_text(&fixture, "assets/maps/room.level.ron");
    assert!(text.contains("Lamp"), "{text}");
}

#[test]
fn quit_request_autosaves_like_the_windowed_editor() {
    let fixture = Fixture::new("headless-editor-quit");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    call(&mut client, "editor.spawn", json!({ "name": "Draft" }));
    // The answer may be lost with the connection: the exit line is what counts.
    let _ = client.call("quit.request", json!({}));
    let exit = editor.event("exit", &fixture);
    assert_eq!(exit["cause"], "quit.request", "{exit}");
    assert!(
        exit["autosaved"].is_string(),
        "the dirty level is autosaved: {exit}"
    );
    assert_eq!(editor.wait().code(), Some(0), "stderr:\n{}", fixture.log());
    let text = level_text(&fixture, "assets/maps/room.level.ron");
    assert!(
        !text.contains("Draft"),
        "quit.request does not save over the level: {text}"
    );
}

/// The software Vulkan driver's manifest, when this machine has one.
fn lavapipe_icd() -> Option<String> {
    let dir = std::fs::read_dir("/usr/share/vulkan/icd.d").ok()?;
    dir.filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("lvp_icd"))
        })
        .map(|path| path.display().to_string())
}

#[test]
fn views_render_offscreen_on_the_software_adapter() {
    let Some(icd) = lavapipe_icd() else {
        eprintln!("skip: no lavapipe driver manifest in /usr/share/vulkan/icd.d");
        return;
    };
    let fixture = Fixture::new("headless-editor-views");
    let mut editor = Editor::start_with(
        &fixture,
        &[],
        &[
            ("VK_ICD_FILENAMES", icd.as_str()),
            ("VK_LOADER_DRIVERS_SELECT", "*lvp*"),
        ],
    );
    let ready = editor.event("ready", &fixture);
    assert_eq!(
        ready["adapter"]["device_type"],
        "cpu",
        "lavapipe: {ready}\nstderr:\n{}",
        fixture.log()
    );
    let mut client = connect(&fixture);
    call(&mut client, "editor.spawn", json!({ "name": "Crate" }));
    let pane = call(
        &mut client,
        "editor.capture_viewport",
        json!({ "width": 96, "height": 64, "timeout_ms": 600_000 }),
    );
    assert!(
        pane["png_base64"].is_string(),
        "an offscreen pane: {}",
        pane["width"]
    );
    assert_eq!(pane["width"], 96);
    let shot = call(&mut client, "seeing.screenshot", json!({ "timeout_ms": 600_000 }));
    assert_eq!(shot["width"], 1280, "the window answer is the 1280x720 viewport");
    assert_eq!(shot["height"], 720);
    let out = call(&mut client, "agent.shutdown", json!({ "save": false }));
    assert_eq!(out["saved"], json!([]));
    assert_eq!(editor.wait().code(), Some(0), "stderr:\n{}", fixture.log());
}

// -------------------------------------------------------------------------------------------------
// Generator jobs, the end of stdin and golden paths (ADR-2821).

/// A long generator: it says it started, then blocks on a FIFO the test controls. No sleeps: the
/// generator ends exactly when the test writes the FIFO.
#[cfg(unix)]
fn add_gated_generator(fixture: &Fixture) -> PathBuf {
    let gate = fixture.0.join("gate");
    let made = Command::new("mkfifo").arg(&gate).status().expect("run mkfifo");
    assert!(made.success(), "mkfifo");
    std::fs::write(
        fixture.0.join("generators/slow.toml"),
        r#"
format_version = 1

[generator]
kind = "command"
seed = 1
description = "blocks on a FIFO"
outputs = ["assets/gen/slow.bin"]
command = ["sh", "-c", "echo $$ > \"$REGIA_PROJECT_DIR/slow.pid\"; echo \"bless=${REGIA_BLESS:-unset}\" > \"$REGIA_PROJECT_DIR/slow.env\"; echo started >&2; mkdir -p \"$REGIA_GEN_OUT/assets/gen\"; cat \"$REGIA_PROJECT_DIR/gate\" > \"$REGIA_GEN_OUT/assets/gen/slow.bin\""]
"#,
    )
    .expect("generator");
    gate
}

/// Polls a job until `done` says so, bounded by `HANG_BOUND`.
#[cfg(unix)]
fn poll_job(client: &mut Client, job: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + HANG_BOUND;
    loop {
        let status = call(client, "agent.job_status", json!({ "job": job }));
        if done(&status) {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job {job} never got there: {status}"
        );
        std::thread::yield_now();
    }
}

#[cfg(unix)]
fn has_started_line(status: &Value) -> bool {
    status["events"].as_array().is_some_and(|events| {
        events
            .iter()
            .any(|e| e["kind"] == "output" && e["line"] == "started")
    })
}

/// The stall of ADR-2412's known limit, gone: the call answers a job at once, the edit world keeps
/// stepping and answering, the job streams, and it ends with the report the short path answers.
#[cfg(unix)]
#[test]
fn a_long_generator_answers_a_job_and_the_editor_keeps_stepping() {
    let fixture = Fixture::new("headless-editor-gen-job");
    let gate = add_gated_generator(&fixture);
    // REGIA_BLESS in the editor's environment reaches no generator (ADR-2821).
    let mut editor = Editor::start_with(&fixture, &["--no-gpu"], &[("REGIA_BLESS", "1")]);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);

    // The short path still answers its report directly.
    let quick = call(&mut client, "gen.run", json!({ "name": "ground" }));
    assert_eq!(quick["outputs"][0]["path"], "assets/gen/ground.r16", "{quick}");

    let started = call(&mut client, "gen.run", json!({ "name": "slow", "wait_ms": 0 }));
    let job = started["job"].as_str().expect("a job id").to_string();
    assert_eq!(started["state"], "running", "{started}");
    assert!(job.starts_with("gen:"), "{job}");
    poll_job(&mut client, &job, has_started_line);
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("slow.env")).expect("env file"),
        "bless=unset\n"
    );

    // The editor is not held: frames advance while the generator blocks.
    let first = call(&mut client, "editor.headless_status", json!({}))["frames"]
        .as_u64()
        .expect("frames");
    let deadline = std::time::Instant::now() + HANG_BOUND;
    loop {
        let now = call(&mut client, "editor.headless_status", json!({}))["frames"]
            .as_u64()
            .expect("frames");
        if now > first {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the frame count stopped at {first}"
        );
    }
    // One generator job at a time; other calls still answer.
    assert_eq!(
        error_code(client.call("gen.run", json!({ "name": "ground", "wait_ms": 0 }))),
        "gen_busy"
    );
    assert_eq!(
        call(&mut client, "gen.list", json!({}))["generators"][0]["name"],
        "ground"
    );
    // `gen.verify` shares the slot.
    assert_eq!(
        error_code(client.call("gen.verify", json!({ "wait_ms": 0 }))),
        "gen_busy"
    );
    assert!(
        client
            .call("gen.run", json!({ "name": "slow", "wait_ms": 25_001 }))
            .is_err(),
        "wait_ms has a limit"
    );

    // Opening the FIFO for writing lets the generator end; the job holds the short path's report.
    std::fs::write(&gate, b"payload").expect("write the gate");
    let finished = poll_job(&mut client, &job, |s| s["finished"] == true);
    assert_eq!(finished["state"], "done", "{finished}");
    assert_eq!(finished["result"]["generator"], "slow");
    assert_eq!(finished["result"]["outputs"][0]["path"], "assets/gen/slow.bin");
    assert_eq!(
        std::fs::read(fixture.0.join("assets/gen/slow.bin")).expect("output"),
        b"payload"
    );
    call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(editor.wait().code(), Some(0), "{}", fixture.log());
}

/// A cancel kills the command's process tree and installs nothing; so does the editor exiting.
#[cfg(target_os = "linux")]
#[test]
fn a_generator_is_stopped_by_a_cancel_and_by_the_editor_exiting() {
    let alive = |pid: &str| {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z "))
    };
    let wait_gone = |pid: &str| {
        let deadline = std::time::Instant::now() + HANG_BOUND;
        while alive(pid) {
            assert!(std::time::Instant::now() < deadline, "generator {pid} survived");
            std::thread::yield_now();
        }
    };
    let fixture = Fixture::new("headless-editor-gen-cancel");
    add_gated_generator(&fixture);
    let pid_file = fixture.0.join("slow.pid");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    let started = call(&mut client, "gen.run", json!({ "name": "slow", "wait_ms": 0 }));
    let job = started["job"].as_str().expect("job").to_string();
    poll_job(&mut client, &job, has_started_line);
    let pid = std::fs::read_to_string(&pid_file)
        .expect("pid")
        .trim()
        .to_string();
    assert!(alive(&pid));
    let cancel = call(&mut client, "agent.job_cancel", json!({ "job": job }));
    assert_eq!(cancel["requested"], true, "{cancel}");
    let finished = poll_job(&mut client, &job, |s| s["finished"] == true);
    assert_eq!(finished["state"], "cancelled", "{finished}");
    wait_gone(&pid);
    assert!(
        !fixture.0.join("assets/gen/slow.bin").exists(),
        "a cancel installs nothing"
    );

    // Again, and the editor exits with the job running.
    std::fs::remove_file(&pid_file).expect("old pid");
    let started = call(&mut client, "gen.run", json!({ "name": "slow", "wait_ms": 0 }));
    let job = started["job"].as_str().expect("job").to_string();
    poll_job(&mut client, &job, has_started_line);
    let pid = std::fs::read_to_string(&pid_file)
        .expect("pid")
        .trim()
        .to_string();
    call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(editor.wait().code(), Some(0), "{}", fixture.log());
    assert!(!alive(&pid), "the generator {pid} outlived the editor");
}

/// `--exit-on-stdin-eof`: whoever started the editor closes (or loses) the pipe, and the editor saves
/// and exits as on SIGTERM. `regia mcp --headless` relies on this.
#[test]
fn the_end_of_stdin_saves_and_exits() {
    let fixture = Fixture::new("headless-editor-stdin");
    let mut editor = Editor::start_piped(&fixture, &["--no-gpu", "--exit-on-stdin-eof"]);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);
    call(&mut client, "editor.spawn", json!({ "name": "Lamp" }));
    drop(editor.running.0.stdin.take().expect("the pipe"));
    let exit = editor.event("exit", &fixture);
    assert_eq!(exit["cause"], "parent_exit", "{exit}");
    assert_eq!(exit["saved"], json!(["assets/maps/room.level.ron"]), "{exit}");
    let status = editor.wait();
    assert_eq!(status.code(), Some(0), "{status:?}; stderr:\n{}", fixture.log());
    assert!(level_text(&fixture, "assets/maps/room.level.ron").contains("Lamp"));
    assert!(!fixture.0.join(".regia/agent/endpoint.json").exists());
}

/// Without the flag, stdin means nothing: the same closed pipe leaves the editor running.
#[test]
fn stdin_does_not_matter_without_the_flag() {
    let fixture = Fixture::new("headless-editor-stdin-off");
    let mut editor = Editor::start_piped(&fixture, &["--no-gpu"]);
    editor.event("ready", &fixture);
    drop(editor.running.0.stdin.take().expect("the pipe"));
    let mut client = connect(&fixture);
    let first = call(&mut client, "editor.headless_status", json!({}))["frames"]
        .as_u64()
        .expect("frames");
    let deadline = std::time::Instant::now() + HANG_BOUND;
    loop {
        let now = call(&mut client, "editor.headless_status", json!({}))["frames"]
            .as_u64()
            .expect("frames");
        if now > first + 100 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "frames stopped");
    }
    call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(editor.event("exit", &fixture)["cause"], "agent.shutdown");
    assert_eq!(editor.wait().code(), Some(0));
}

/// ADR-2821 on the real editor's whole surface: every method its schema lists, the editor's own among
/// them, refuses a string parameter naming a golden path, with every permission granted. Then
/// `gen.run` refuses a generator whose declared output is a golden.
#[test]
fn every_method_of_the_real_editor_refuses_a_golden_path() {
    const RELATIVE: &str = "crates/demo/goldens/sponza.png";
    let fixture = Fixture::new("headless-editor-goldens");
    std::fs::write(
        fixture.0.join(".regia/agent.toml"),
        "permissions = [\"read\", \"edit_scene\", \"assets\", \"play\", \"build\", \"tools\", \"review\"]\nrate_limit_per_second = 10000\nheavy_rate_limit_per_second = 10000\n",
    )
    .expect("agent.toml");
    std::fs::create_dir_all(fixture.0.join("crates/demo/goldens")).expect("goldens");
    let golden = fixture.0.join(RELATIVE);
    std::fs::write(&golden, b"the owner's image").expect("golden");
    std::fs::write(
        fixture.0.join("generators/aimed.toml"),
        "format_version = 1\n\n[generator]\nkind = \"heightfield\"\nseed = 1\noutputs = [\"crates/demo/goldens/aimed.r16\"]\n\n[generator.heightfield]\nwidth = 8\nheight = 8\n",
    )
    .expect("generator");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    let schema = call(&mut client, "agent.schema", json!({}));
    let methods = schema["methods"].as_array().expect("methods");
    let exempt = [
        "agent.hello",
        "agent.guide",
        "agent.schema",
        "agent.changes",
        "errors.explain",
        "logs.read",
        "agent.job_status",
        "agent.job_cancel",
        "agent.golden_test",
        "agent.bless_golden",
        "agent.run_tests",
        "seeing.compare_images",
        "review.list",
        "review.diff",
        "review.accept",
        "review.reject",
    ];
    let free_text = [
        "reason",
        "note",
        "title",
        "text",
        "description",
        "message",
        "ron",
        "source",
        "script",
        "content",
        "summary",
        "comment",
    ];
    let (mut refused, mut editor_methods, mut reached) = (0, 0, Vec::new());
    for method in methods {
        let name = method["name"].as_str().expect("name");
        if exempt.contains(&name) {
            continue;
        }
        let params = method["params"]["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (key, _) in params.iter().filter(|(_, p)| p["type"] == "string") {
            if free_text.contains(&key.as_str()) {
                continue;
            }
            let mut body = serde_json::Map::new();
            body.insert(key.clone(), json!(RELATIVE));
            match error_code_or_reason(client.call(name, Value::Object(body))) {
                reason if reason == "golden_path_denied" => {
                    refused += 1;
                    editor_methods += usize::from(name.starts_with("editor.") || name.starts_with("gen."));
                }
                // A contract limit on the value refuses earlier; that is no write either.
                reason if reason == "F0002" => {}
                other => reached.push(format!("{name}.{key}: {other}")),
            }
        }
    }
    assert!(reached.is_empty(), "reached a handler:\n{}", reached.join("\n"));
    assert!(refused > 200, "the guard saw {refused} calls");
    assert!(editor_methods > 20, "the editor's own methods: {editor_methods}");
    assert_eq!(
        error_code_or_reason(client.call("gen.run", json!({ "name": "aimed" }))),
        "golden_path_denied"
    );
    // A job passed inline, a path inside text; a line break inside a path.
    for (method, params) in [
        (
            "render.out_job",
            json!({ "job": "(out:\"crates/demo/goldens\",takes:[(name:\"a\")])", "dry_run": true }),
        ),
        (
            "editor.keymap_export",
            json!({ "path": "crates/demo/goldens/a\n.png" }),
        ),
    ] {
        assert_eq!(
            error_code_or_reason(client.call(method, params)),
            "golden_path_denied",
            "{method}"
        );
    }
    assert_eq!(
        std::fs::read(&golden).expect("golden"),
        b"the owner's image",
        "the golden did not change"
    );
    assert!(!fixture.0.join("crates/demo/goldens/aimed.r16").exists());
    call(&mut client, "agent.shutdown", json!({ "save": false }));
    assert_eq!(editor.wait().code(), Some(0), "{}", fixture.log());
}

fn error_code_or_reason(result: Result<Value, ClientError>) -> String {
    match result {
        Err(ClientError::Rpc(err)) => {
            let data = err.data.as_ref();
            data.and_then(|d| d.get("reason").or_else(|| d.get("code")))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
        Ok(value) => format!("answered {value}"),
        other => panic!("expected an RPC error, got {other:?}"),
    }
}

/// `docs/security/threat-model.md`: `gen.run` runs project code, so a profile without `build` is
/// refused with the permission's code, and the generator's output is not written.
#[test]
fn gen_run_needs_the_build_permission() {
    let fixture = Fixture::new("headless-gen-permission");
    std::fs::write(
        fixture.0.join(".regia/agent.toml"),
        "permissions = [\"read\", \"edit_scene\", \"assets\", \"play\"]\n",
    )
    .expect("agent.toml");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    assert_eq!(
        error_code(client.call("gen.run", json!({ "name": "ground" }))),
        "F0012"
    );
    assert!(!fixture.0.join("assets/gen/ground.r16").exists());
    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    let _ = editor.wait();
}

/// The separate play window (`docs/specs/19-editor.md` § 4, ADR-7272) needs a window system: the
/// headless editor answers `editor.play { window: true }` with the named reason
/// `play_window_unavailable` (its message says there is no window system) instead of panicking,
/// starts nothing, and still plays in its offscreen view when asked without the window.
#[test]
fn play_window_answers_a_named_reason_without_a_window_system() {
    let fixture = Fixture::new("headless-editor-play-window");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);

    let refused = client.call("editor.play", json!({ "window": true }));
    let Err(ClientError::Rpc(err)) = refused else {
        panic!("expected an RPC error, got {refused:?}");
    };
    let reason = err
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert_eq!(reason, "play_window_unavailable", "{err:?}");
    assert!(
        err.message.contains("no window system"),
        "the message names the cause: {}",
        err.message
    );
    let state = call(&mut client, "editor.play_state", json!({}));
    assert_eq!(state["mode"], "stopped", "nothing started: {state}");

    // Without the window, play runs (the headless editor steps it).
    let queued = call(&mut client, "editor.play", json!({}));
    assert_eq!(queued["window"], "viewport", "{queued}");
    let deadline = std::time::Instant::now() + HANG_BOUND;
    let state = loop {
        let state = call(&mut client, "editor.play_state", json!({}));
        if state["mode"] != "stopped" {
            break state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "play never started (a hang): {state}; stderr:\n{}",
            fixture.log()
        );
        std::thread::yield_now();
    };
    assert_eq!(state["mode"], "playing", "{state}");
    assert_eq!(state["play_window"]["open"], false, "{state}");

    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    let status = editor.wait();
    assert_eq!(status.code(), Some(0), "{status:?}; stderr:\n{}", fixture.log());
    assert!(!fixture.log().contains("panicked"), "{}", fixture.log());
}

/// The chosen play resolution (ADR-7283) on a host with no window system: a valid preset with
/// `window: true` still answers `play_window_unavailable` (ADR-7272's answer holds), an invalid one
/// is named first (`play_resolution_invalid`), a preset without the window is
/// `play_resolution_needs_window`, and `editor.play_state` reports the stored resolution.
#[test]
fn play_resolution_presets_keep_the_no_window_system_answer() {
    let fixture = Fixture::new("headless-editor-play-sizes");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);

    let rpc_reason = |result: Result<Value, ClientError>| -> (String, String) {
        let Err(ClientError::Rpc(err)) = result else {
            panic!("expected an RPC error, got {result:?}");
        };
        let reason = err
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        (reason, err.message)
    };
    let (reason, message) = rpc_reason(client.call(
        "editor.play",
        json!({ "window": true, "preset": "phone_9_16_portrait" }),
    ));
    assert_eq!(reason, "play_window_unavailable");
    assert!(message.contains("no window system"), "{message}");
    let (reason, message) =
        rpc_reason(client.call("editor.play", json!({ "window": true, "preset": "8k_phone" })));
    assert_eq!(reason, "play_resolution_invalid", "{message}");
    assert!(
        message.contains("phone_9_16_portrait"),
        "the message lists the presets: {message}"
    );
    let (reason, _) = rpc_reason(client.call("editor.play", json!({ "size": [1080, 1920] })));
    assert_eq!(reason, "play_resolution_needs_window");

    let state = call(&mut client, "editor.play_state", json!({}));
    assert_eq!(state["mode"], "stopped", "nothing started: {state}");
    assert_eq!(state["play_window"]["resolution"]["preset"], "window", "{state}");

    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    let status = editor.wait();
    assert_eq!(status.code(), Some(0), "{status:?}; stderr:\n{}", fixture.log());
    assert!(!fixture.log().contains("panicked"), "{}", fixture.log());
}

/// Split-screen players on a host with no window system (ADR-7395): `players` joins that many
/// local players, `editor.play_state` reports their viewports, and the play window's own status
/// reports the split it would show (no panes: no window can open). An out-of-range `players` is
/// a named error.
#[test]
fn split_screen_players_report_their_viewports_without_a_window_system() {
    let fixture = Fixture::new("headless-editor-play-split");
    let mut editor = Editor::start(&fixture);
    editor.event("ready", &fixture);
    let mut client = connect(&fixture);
    make_and_open_room(&mut client);

    let refused = client.call("editor.play", json!({ "players": 9 }));
    let Err(ClientError::Rpc(err)) = refused else {
        panic!("expected an RPC error, got {refused:?}");
    };
    let reason = err
        .data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    assert_eq!(reason, "play_players_out_of_range", "{err:?}");

    let queued = call(&mut client, "editor.play", json!({ "players": 2 }));
    assert_eq!(queued["players"], 2, "{queued}");
    assert_eq!(queued["window"], "viewport", "{queued}");
    let deadline = std::time::Instant::now() + HANG_BOUND;
    let state = loop {
        let state = call(&mut client, "editor.play_state", json!({}));
        if state["mode"] != "stopped" {
            break state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "play never started (a hang): {state}; stderr:\n{}",
            fixture.log()
        );
        std::thread::yield_now();
    };
    assert_eq!(state["mode"], "playing", "{state}");
    assert_eq!(state["players"], 2, "{state}");
    let viewports = state["viewports"].as_array().expect("viewports");
    assert_eq!(viewports.len(), 2, "{state}");
    assert_eq!(viewports[0]["rect"], json!([0.0, 0.0, 0.5, 1.0]), "{state}");
    assert_eq!(viewports[1]["rect"], json!([0.5, 0.0, 0.5, 1.0]), "{state}");
    assert_eq!(state["play_window"]["players"], 2, "{state}");
    assert_eq!(
        state["play_window"]["panes"],
        json!([]),
        "no window is open, so no panes: {state}"
    );

    let out = call(&mut client, "agent.shutdown", json!({}));
    assert_eq!(out["exiting"], true, "{out}");
    let status = editor.wait();
    assert_eq!(status.code(), Some(0), "{status:?}; stderr:\n{}", fixture.log());
    assert!(!fixture.log().contains("panicked"), "{}", fixture.log());
}
