// SPDX-License-Identifier: MPL-2.0
//! `regia mcp --headless` (ADR-2821): startup failures name a code and leave nothing running, a live
//! app is attached to and left alone, and the started editor never outlives the adapter, however the
//! adapter ends. Fake editors (shell scripts) drive the failure paths; the real `regia-editor` runs
//! when it was built next to `regia` (`cargo test --workspace`, or `-p regia_cli -p regia_editor`).
//!
//! Waits are on files and processes with a bound on a hang, never a timer that decides a result.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Child;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use regia_test_utils::process::Running;
use regia_test_utils::process::wait_within;
use regia_test_utils::unique_temp_dir;
use rmcp::ClientHandler;
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, ContentBlock, Implementation};
use serde_json::json;
use tokio::process::Command as AsyncCommand;

/// The longest wait for a process that stopped making progress (a loaded gate is slow).
const HANG_BOUND: Duration = Duration::from_secs(600);

/// An empty project, removed at the end of the test.
struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = unique_temp_dir(tag);
        std::fs::write(
            dir.join("Regia.toml"),
            "[project]\nname = \"Mcp\"\nid = \"com.example.mcp\"\n",
        )
        .expect("manifest");
        Self(dir.canonicalize().expect("canonical"))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fake editor: a shell script with `body`, run with the adapter's arguments.
#[cfg(unix)]
fn fake_editor(fixture: &Fixture, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.file("fake-editor.sh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho $$ > '{}'.pid\n{body}\n", path.display()),
    )
    .expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

fn adapter(fixture: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_regia"));
    command
        .args(["mcp", "--headless", "--project"])
        .arg(&fixture.0)
        .env_remove("REGIA_EDITOR")
        .env_remove("FERRITE_EDITOR") // rename-keep
        .env_remove("REGIA_AGENT_DIR")
        .env_remove("FERRITE_AGENT_DIR"); // rename-keep
    command
}

/// Runs the adapter to its end with the client's stdin closed; its stderr and status.
fn run_to_end(command: &mut Command) -> (ExitStatus, String) {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start regia mcp");
    let stderr = child.stderr.take().expect("stderr");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut text);
        text
    });
    let status = wait_within(&mut child, HANG_BOUND).expect("wait");
    (status, reader.join().expect("stderr reader"))
}

/// Waits for a file a fake editor writes.
#[cfg(unix)]
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + HANG_BOUND;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a process still runs (a zombie has ended).
#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| !stat.contains(") Z "))
}

#[cfg(target_os = "linux")]
fn pid_in(path: &Path) -> u32 {
    wait_for_file(path);
    let deadline = Instant::now() + HANG_BOUND;
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_missing_editor_is_a_named_failure() {
    let fixture = Fixture::new("mcp-headless-missing");
    let missing = fixture.file("no-such-editor");
    let (status, stderr) = run_to_end(adapter(&fixture).arg("--editor-bin").arg(&missing));
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("mcp_editor_start_failed"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn an_editor_that_exits_early_is_a_named_failure_with_its_status() {
    let fixture = Fixture::new("mcp-headless-exits");
    let editor = fake_editor(&fixture, "echo not ready\nexit 3");
    let (status, stderr) = run_to_end(adapter(&fixture).arg("--editor-bin").arg(editor));
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("mcp_editor_exited"), "{stderr}");
    assert!(stderr.contains('3'), "the status is named: {stderr}");
}

/// The timeout is the point of this test: the fake never prints a ready line, whatever the load.
#[cfg(target_os = "linux")]
#[test]
fn no_ready_line_times_out_and_kills_the_editor() {
    let fixture = Fixture::new("mcp-headless-timeout");
    // Ignores the end of its stdin: only a kill ends it.
    let editor = fake_editor(&fixture, "trap '' TERM\necho starting\nwhile :; do sleep 1; done");
    let (status, stderr) = run_to_end(
        adapter(&fixture)
            .arg("--editor-bin")
            .arg(&editor)
            .args(["--editor-timeout", "1"]),
    );
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("mcp_editor_ready_timeout"), "{stderr}");
    let pid = pid_in(&editor.with_extension("sh.pid"));
    assert!(!alive(pid), "the editor {pid} outlived its failed start");
}

/// A fake that waits for a ready-gated client: prints ready, then ends when its stdin does and
/// leaves a mark.
#[cfg(unix)]
fn fake_ready_editor(fixture: &Fixture) -> PathBuf {
    let mark = fixture.file("editor-saw-eof");
    fake_editor(
        fixture,
        &format!(
            "echo '{{\"event\":\"ready\",\"url\":\"ws://fake\"}}'\ncat > /dev/null\necho done > '{}'",
            mark.display()
        ),
    )
}

/// The client never arrives: stdin is at its end when the adapter starts to serve. The adapter ends,
/// and the editor with it.
#[cfg(unix)]
#[test]
fn the_editor_ends_when_the_client_disappears() {
    let fixture = Fixture::new("mcp-headless-client-gone");
    let editor = fake_ready_editor(&fixture);
    let (status, stderr) = run_to_end(adapter(&fixture).arg("--editor-bin").arg(&editor));
    assert!(fixture.file("editor-saw-eof").exists(), "{stderr}");
    assert!(stderr.contains("the headless editor exited"), "{stderr}");
    assert!(
        status.code().is_some(),
        "the adapter ended on its own: {status:?}; {stderr}"
    );
}

/// A connected client leaves: the transport ends, the adapter ends, and the editor ends with it.
#[cfg(unix)]
#[tokio::test]
async fn the_editor_ends_when_a_connected_client_leaves() {
    let fixture = Fixture::new("mcp-headless-client-leaves");
    let editor = fake_ready_editor(&fixture);
    let mut command = AsyncCommand::new(env!("CARGO_BIN_EXE_regia"));
    command
        .args(["mcp", "--headless", "--project"])
        .arg(&fixture.0)
        .arg("--editor-bin")
        .arg(&editor)
        .env_remove("REGIA_AGENT_DIR")
        .env_remove("FERRITE_AGENT_DIR") // rename-keep
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    let transport = rmcp::transport::child_process::TokioChildProcess::new(command).expect("transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");
    assert!(
        !fixture.file("editor-saw-eof").exists(),
        "the editor stays while the client does"
    );
    running.cancel().await.expect("the client leaves");
    wait_for_file(&fixture.file("editor-saw-eof"));
}

/// The adapter is killed outright, on either transport: no destructor runs, the kernel closes the
/// pipe, and the editor ends.
#[cfg(unix)]
#[test]
fn the_editor_ends_when_the_adapter_is_killed() {
    use std::io::{BufRead as _, BufReader};
    for (tag, http) in [("stdio", false), ("http", true)] {
        let fixture = Fixture::new(&format!("mcp-headless-killed-{tag}"));
        let editor = fake_ready_editor(&fixture);
        let mut command = adapter(&fixture);
        command.arg("--editor-bin").arg(&editor);
        if http {
            command.arg("--http");
        }
        // A client that stays: a pipe nobody writes.
        let mut running: Child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start regia mcp");
        let _client = running.stdin.take();
        // Waiting on this line: a kill before the fake's ready line would end it by SIGPIPE.
        let stderr = running.stderr.take().expect("stderr");
        let mut lines = BufReader::new(stderr).lines();
        let started = lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.contains("started a headless editor"));
        assert!(started, "{tag}: the adapter never saw the ready line");
        assert!(
            !fixture.file("editor-saw-eof").exists(),
            "{tag}: the editor ended early"
        );
        running.kill().expect("SIGKILL the adapter");
        let _ = running.wait();
        wait_for_file(&fixture.file("editor-saw-eof"));
    }
}

/// An editor that ignores the end of its stdin is killed when the adapter ends normally.
#[cfg(target_os = "linux")]
#[test]
fn an_editor_ignoring_the_end_of_stdin_is_killed_at_the_end() {
    let fixture = Fixture::new("mcp-headless-stubborn");
    let editor = fake_editor(
        &fixture,
        "trap '' TERM\necho '{\"event\":\"ready\",\"url\":\"ws://fake\"}'\nwhile :; do sleep 1; done",
    );
    let (_, stderr) = run_to_end(
        adapter(&fixture)
            .arg("--editor-bin")
            .arg(&editor)
            .args(["--editor-timeout", "20"]),
    );
    assert!(stderr.contains("did not exit in time"), "{stderr}");
    let pid = pid_in(&editor.with_extension("sh.pid"));
    assert!(!alive(pid), "the editor {pid} outlived the adapter");
}

/// A running app is attached to: nothing is started, and that app is left alone.
#[test]
fn a_live_endpoint_is_attached_to_and_no_editor_starts() {
    use regia_agent::{endpoint, protocol, server};
    let fixture = Fixture::new("mcp-headless-live");
    let token = "test-token-0123456789abcdef0123456789abcdef".to_string();
    let live = server::start(token.clone()).expect("server");
    let agent_dir = endpoint::agent_dir(&fixture.0);
    let advert = protocol::Endpoint {
        url: format!("ws://{}", live.addr),
        pid: std::process::id(),
        protocol: protocol::PROTOCOL_VERSION.into(),
        token_file: agent_dir.join("token").display().to_string(),
        app: "live".into(),
    };
    endpoint::write(&agent_dir, &advert, &token).expect("endpoint");
    let missing = fixture.file("no-such-editor");
    let (_, stderr) = run_to_end(adapter(&fixture).arg("--editor-bin").arg(&missing));
    assert!(stderr.contains("attaching"), "{stderr}");
    assert!(!stderr.contains("mcp_editor_start_failed"), "{stderr}");
}

#[test]
fn attach_never_starts_an_editor() {
    let fixture = Fixture::new("mcp-attach");
    let (_, stderr) = run_to_end(
        Command::new(env!("CARGO_BIN_EXE_regia"))
            .args(["mcp", "--attach", "--project"])
            .arg(&fixture.0)
            .env("REGIA_EDITOR", fixture.file("no-such-editor")),
    );
    assert!(!stderr.contains("mcp_editor"), "{stderr}");
}

// ---------------------------------------------------------------------------------------------
// The real editor.

/// `regia-editor` beside this `regia`, when the test run built it.
fn real_editor() -> Option<PathBuf> {
    let beside = Path::new(env!("CARGO_BIN_EXE_regia"))
        .with_file_name(format!("regia-editor{}", std::env::consts::EXE_SUFFIX));
    if beside.is_file() {
        return Some(beside);
    }
    // The gate builds every binary, so this line does not appear there.
    eprintln!("SKIPPED: {} is not built", beside.display());
    None
}

#[derive(Clone)]
struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("regia-cli-test", "0.1.0"),
        )
    }
}

fn editor_home(command: &mut AsyncCommand, fixture: &Fixture) {
    command
        .env("HOME", &fixture.0)
        .env("XDG_CONFIG_HOME", fixture.file("config"))
        .env("XDG_DATA_HOME", fixture.file("data"))
        .env("APPDATA", fixture.file("appdata"));
}

/// MCP client, adapter and a real headless editor: a tool call reaches the editor, and the client
/// leaving ends the adapter and the editor, which saves its dirty level and removes its endpoint.
#[tokio::test]
async fn a_client_drives_a_real_editor_and_leaving_ends_it() {
    let Some(editor) = real_editor() else { return };
    let fixture = Fixture::new("mcp-headless-real");
    std::fs::create_dir_all(fixture.file("assets")).expect("assets");
    let log = std::fs::File::create(fixture.file("adapter.err")).expect("log");
    let mut command = AsyncCommand::new(env!("CARGO_BIN_EXE_regia"));
    command
        .args(["mcp", "--headless", "--no-gpu", "--project"])
        .arg(&fixture.0)
        .arg("--editor-bin")
        .arg(&editor)
        .env_remove("REGIA_AGENT_DIR")
        .env_remove("FERRITE_AGENT_DIR") // rename-keep
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .kill_on_drop(false);
    editor_home(&mut command, &fixture);
    let transport = rmcp::transport::child_process::TokioChildProcess::new(command).expect("transport");
    let running = rmcp::serve_client(TestClient, transport)
        .await
        .expect("client connects");
    let adverts = regia_agent::endpoint::agent_dir(&fixture.0).join("endpoint.json");
    assert!(
        adverts.is_file(),
        "the adapter serves only once the editor is ready"
    );

    // A built-in tool reaches the editor through the adapter.
    let hello = running
        .call_tool(CallToolRequestParams::new("hello"))
        .await
        .expect("hello");
    let text = match hello.content.first() {
        Some(ContentBlock::Text(block)) => block.text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        text.contains("editor_headless"),
        "the capabilities are the editor's: {text}"
    );
    // An edit through the protocol, for the exit to save.
    let mut direct = regia_agent::client::Client::connect_project(&fixture.0).expect("the editor listens");
    direct.set_read_timeout(Some(HANG_BOUND)).expect("read timeout");
    direct
        .call("editor.spawn", json!({ "name": "Crate" }))
        .expect("an edit to save");
    let status = direct
        .call("editor.headless_status", json!({}))
        .expect("headless status");
    assert_eq!(status["dirty"], true, "{status}");
    drop(direct);

    running.cancel().await.expect("the client leaves");
    // The adapter ends, then the editor: its last act removes the endpoint.
    let deadline = Instant::now() + HANG_BOUND;
    while adverts.exists() {
        assert!(
            Instant::now() < deadline,
            "the editor outlived its client: {}",
            std::fs::read_to_string(fixture.file("adapter.err")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let saved = std::fs::read_dir(fixture.file("assets"))
        .map(|files| files.flatten().count())
        .unwrap_or(0);
    assert!(saved >= 1, "the dirty level was saved on the way out");
}

/// The adapter is killed with a real editor behind it: the editor still saves and exits.
#[cfg(unix)]
#[test]
fn a_real_editor_ends_when_its_adapter_is_killed() {
    use std::io::{BufRead as _, BufReader};
    let Some(editor) = real_editor() else { return };
    let fixture = Fixture::new("mcp-headless-real-kill");
    std::fs::create_dir_all(fixture.file("assets")).expect("assets");
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_regia"))
            .args(["mcp", "--headless", "--no-gpu", "--http", "--project"])
            .arg(&fixture.0)
            .arg("--editor-bin")
            .arg(&editor)
            .env("HOME", &fixture.0)
            .env("XDG_CONFIG_HOME", fixture.file("config"))
            .env("XDG_DATA_HOME", fixture.file("data"))
            .env_remove("REGIA_AGENT_DIR")
            .env_remove("FERRITE_AGENT_DIR") // rename-keep
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start regia mcp"),
    );
    let stderr = child.0.stderr.take().expect("stderr");
    let (lines_tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    // Over HTTP the adapter says when it serves, after the editor's ready line.
    let mut seen = String::new();
    while !seen.contains("Streamable HTTP on") {
        let line = lines.recv_timeout(HANG_BOUND).expect("the adapter serves");
        seen.push_str(&line);
        seen.push('\n');
    }
    let mut client = regia_agent::client::Client::connect_project(&fixture.0).expect("the editor listens");
    client.set_read_timeout(Some(HANG_BOUND)).expect("read timeout");
    client
        .call("editor.spawn", json!({ "name": "Crate" }))
        .expect("an edit to save");
    child.0.kill().expect("SIGKILL");
    let _ = child.0.wait();
    let advert = regia_agent::endpoint::agent_dir(&fixture.0).join("endpoint.json");
    let deadline = Instant::now() + HANG_BOUND;
    while advert.exists() {
        assert!(
            Instant::now() < deadline,
            "the editor outlived the adapter:\n{seen}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let saved = std::fs::read_dir(fixture.file("assets"))
        .map(|files| files.flatten().count())
        .unwrap_or(0);
    assert!(saved >= 1, "the dirty level was saved on the way out");
}
