// SPDX-License-Identifier: MPL-2.0
//! `sim_bridge` smoke (M3, specs/23 § 8): spawns the sample, drives it through the agent protocol
//! and verifies it terminates cleanly. Same client pattern as [`smoke.rs`](super::smoke) — no new
//! protocol methods.
//!
//! The caller must have built the sample first: the smoke only *spawns*
//! `target/debug/sim_bridge`, it never builds it.
//!
//! ```sh
//! cargo build -p sim_bridge
//! cargo run -p regia_agent --example sim_smoke
//! ```
//!
//! What it does: starts `target/debug/sim_bridge --exit-after-frames 600` with
//! `samples/sim_bridge` as cwd (the agent server advertises
//! `samples/sim_bridge/.regia/agent/endpoint.json`), waits for that file, then calls
//! `agent.hello` (the current protocol version, our child's pid), `scene.query_entities` (at least one entity),
//! `play.step` (10 frames), `play.inject_input` (KeyW press + release) and `logs.read` (engine
//! lifecycle history). Afterwards it waits for the sample's own `--exit-after-frames` exit and
//! requires exit code 0; a sample that does not exit in time is killed, which fails the check —
//! a killed process reports the signal, not a code, so "exits 0" can only be observed on the
//! sample's own exit.
//!
//! Sizing of that wait: a window that is not visible on screen (minimised, occluded — e.g. it
//! opened behind the operator's other windows — or on a hidden workspace) runs throttled frames
//! of 100 ms (`regia_platform::runner::THROTTLED_FRAME`, specs/11 § 2), so 600 frames take
//! ≈ 60 s. [`EXIT_WAIT`] covers that worst case plus startup and margin; a visible window exits
//! much sooner.
//!
//! A previous run killed mid-flight leaves a stale `endpoint.json` behind; the smoke removes it
//! before spawning, so the wait below only triggers on this run's server.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use serde_json::{Value, json};

/// How long the smoke waits for `endpoint.json` to appear.
const ENDPOINT_WAIT: Duration = Duration::from_secs(20);
/// How long the smoke waits for the sample's own `--exit-after-frames` exit before killing it.
/// Sized for the throttled case: 600 frames at the 100 ms hidden-window rate is ≈ 60 s
/// (see the module docs); a visible window exits in a few seconds.
const EXIT_WAIT: Duration = Duration::from_secs(75);
/// Poll interval while waiting for the endpoint or the exit.
const POLL: Duration = Duration::from_millis(25);
/// Retries for `play.inject_input` while the window is still being created.
const INJECT_RETRIES: usize = 5;

struct Checks {
    failed: usize,
}

impl Checks {
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        if ok {
            println!("PASS  {name}: {detail}");
        } else {
            println!("FAIL  {name}: {detail}");
            self.failed += 1;
        }
    }
}

/// Last `lines` lines of a file, for failure diagnostics.
fn tail(path: &Path, lines: usize) -> String {
    let Ok(all) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let skip = all.lines().count().saturating_sub(lines);
    all.lines().skip(skip).collect::<Vec<_>>().join("\n")
}

fn run() -> Result<usize, String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot locate the workspace root")?
        .to_path_buf();
    let project = root.join("samples").join("sim_bridge");
    let exe = root.join("target").join("debug").join("sim_bridge");
    if !exe.is_file() {
        return Err(format!(
            "{} not found — the caller must have built it first: cargo build -p sim_bridge",
            exe.display()
        ));
    }
    if !project.join("Regia.toml").is_file() {
        return Err(format!("{} is not a Regia Engine project", project.display()));
    }

    let log_path = std::env::temp_dir().join(format!("regia-sim-smoke-{}.log", std::process::id()));
    let failed = run_checks(&project, &exe, &log_path)?;
    if failed > 0 {
        // Keep the log around for a post-mortem; the tail is printed inline.
        println!("      app log tail: {}", tail(&log_path, 10));
        println!("      app log kept: {}", log_path.display());
        return Ok(failed);
    }
    let _ = std::fs::remove_file(&log_path);
    Ok(failed)
}

/// Spawns the sample and runs the endpoint wait, protocol checks and exit check. Any error or
/// failed check leaves the child dead and reaped.
fn run_checks(project: &Path, exe: &Path, log_path: &Path) -> Result<usize, String> {
    // A smoke killed mid-flight leaves `endpoint.json` behind; drop it so the wait only
    // triggers on this run's server.
    let endpoint_file = project.join(".regia").join("agent").join("endpoint.json");
    let _ = std::fs::remove_file(&endpoint_file);

    let log = std::fs::File::create(log_path).map_err(|e| format!("cannot create log: {e}"))?;
    let mut child = Command::new(exe)
        .arg("--exit-after-frames")
        .arg("600")
        .current_dir(project)
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .spawn()
        .map_err(|e| format!("cannot spawn {}: {e}", exe.display()))?;

    let outcome = drive(&mut child, project, log_path);
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome
}

/// Waits for the endpoint, runs the protocol checks, then waits for the sample's own exit.
fn drive(child: &mut Child, project: &Path, log_path: &Path) -> Result<usize, String> {
    let endpoint_file = project.join(".regia").join("agent").join("endpoint.json");

    // The agent server advertises itself in `finish`, before the first frame.
    let deadline = Instant::now() + ENDPOINT_WAIT;
    while !endpoint_file.is_file() {
        if let Some(status) = try_wait(child)? {
            return Err(format!(
                "sim_bridge exited before advertising an endpoint ({status}); {}",
                tail(log_path, 10)
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} never appeared within {:?}",
                endpoint_file.display(),
                ENDPOINT_WAIT
            ));
        }
        std::thread::sleep(POLL);
    }

    let mut client = Client::connect_project(project).map_err(|e| e.to_string())?;
    let mut c = Checks { failed: 0 };
    let mut call = |m: &str, p: Value| client.call(m, p).map_err(|e| format!("{m}: {e}"));

    let hello = call("agent.hello", json!({}))?;
    c.check(
        "agent.hello",
        hello["protocol"] == regia_agent::protocol::PROTOCOL_VERSION
            && hello["pid"].as_u64() == Some(child.id() as u64),
        format!(
            "protocol {} app {} pid {} (child pid {}) frame {}",
            hello["protocol"],
            hello["app"],
            hello["pid"],
            child.id(),
            hello["frame"]
        ),
    );

    let found = call("scene.query_entities", json!({}))?;
    let count = found["entities"].as_array().map_or(0, Vec::len);
    c.check(
        "scene.query_entities",
        count >= 1,
        format!("{count} entities; first = {}", found["entities"][0]),
    );

    let f0 = hello["frame"].as_u64().unwrap_or(0);
    let stepped = call("play.step", json!({ "frames": 10 }))?;
    c.check(
        "play.step",
        stepped["frame"].as_u64().unwrap_or(0) >= f0 + 10,
        format!("frame {f0} -> {}, paused {}", stepped["frame"], stepped["paused"]),
    );
    // `play.step` leaves the sample paused; hand it back its own clock.
    call("play.resume", json!({}))?;

    // The window must exist for `play.inject_input`; retry while the app is still opening it.
    // On the retried error nothing was injected (the window is resolved before any event is
    // written), so a retry cannot double-inject.
    let events = json!({ "events": [
        { "type": "key", "key": "KeyW", "text": "w", "state": "pressed" },
        { "type": "key", "key": "KeyW", "text": "w", "state": "released" },
    ] });
    let mut retries = INJECT_RETRIES;
    let injected = loop {
        match call("play.inject_input", events.clone()) {
            Ok(v) => break Ok(v),
            Err(e) if retries > 0 && e.contains("no primary window") => {
                retries -= 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => break Err(e),
        }
    };
    let injected = injected.map_err(|e| format!("play.inject_input: {e}"))?;
    c.check(
        "play.inject_input",
        injected["injected"] == 2,
        format!("{} events at frame {}", injected["injected"], injected["frame"]),
    );

    // `regia lifecycle: Resumed` is logged by regia_platform in every app, so it doubles as
    // the logs.read history check for the sample.
    let logs = call("logs.read", json!({ "contains": "regia lifecycle: Resumed" }))?;
    let records = logs["records"].as_array().map_or(0, Vec::len);
    c.check("logs.read", records >= 1, format!("{records} lifecycle records"));

    let camera = call("camera.get", json!({}))?;
    c.check(
        "camera.get",
        camera["mode"] == "chase",
        format!("mode {}", camera["mode"]),
    );

    let counters = call("sim.counters", json!({}))?;
    c.check(
        "sim.counters",
        counters["frames"].as_u64().unwrap_or(0) >= 1,
        format!("frames {}", counters["frames"]),
    );

    let scripts = call("scripts.list", json!({}))?;
    c.check(
        "scripts.list",
        scripts["count"].as_u64().unwrap_or(0) >= 1,
        format!("{} registered script(s)", scripts["count"]),
    );

    // Leave the sample running and wait for its own `--exit-after-frames 600` exit so the smoke
    // can verify a clean exit 0. Kill only if it does not exit in time (see the module docs).
    let deadline = Instant::now() + EXIT_WAIT;
    let status: Option<ExitStatus> = loop {
        match try_wait(child)? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            child.wait().map_err(|e| e.to_string())?
        }
    };
    c.check(
        "sample exit",
        status.code() == Some(0),
        match status.code() {
            Some(0) => "exited 0 (the --exit-after-frames exit)".to_string(),
            Some(code) => format!("exited {code}"),
            None => format!(
                "did not exit within {:?}; killed — status {status} reports the signal, not a code",
                EXIT_WAIT
            ),
        },
    );

    Ok(c.failed)
}

/// `Some(status)` once the child has exited, `None` while it is running.
fn try_wait(child: &mut Child) -> Result<Option<ExitStatus>, String> {
    child.try_wait().map_err(|e| format!("try_wait: {e}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(0) => {
            println!("sim_bridge smoke: PASS");
            ExitCode::SUCCESS
        }
        Ok(n) => {
            println!("sim_bridge smoke: FAIL ({n} check(s))");
            ExitCode::FAILURE
        }
        Err(e) => {
            println!("sim_bridge smoke: ERROR {e}");
            ExitCode::FAILURE
        }
    }
}
