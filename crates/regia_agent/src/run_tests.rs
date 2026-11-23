// SPDX-License-Identifier: MPL-2.0
//! `agent.run_tests` (`docs/specs/23-agent-native.md` § 3.6, ADR-2346): runs one named test target
//! of the project with `cargo test` on a worker thread and streams the results through the job it
//! answers (`agent.job_status`, [`crate::jobs`]).
//!
//! The run compiles and executes project code with the app's privileges, so the method needs the
//! `build` permission (specs/23 § 5: build and play amount to code execution), which no default
//! profile and no tool scope grants. Names that reach the command line are checked against a
//! narrow alphabet ([`check_name`]): no spaces, no leading `-`, so a request cannot add flags.
//! `offline` (default true) keeps cargo off the network, the protocol's no-network rule.
//!
//! The output parser ([`parse_line`]) reads the stable human format of the standard test harness:
//! `running N tests`, `test <name> ... ok|FAILED|ignored`, the `---- <name> stdout ----` failure
//! sections and the `test result:` summary, plus cargo's `Running`/`Doc-tests` banners and
//! `error:` lines. It is fuzzed (`agent.run_tests_output`).

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::golden_tools::scrub_golden_env;
use crate::jobs::{JobHandle, JobRegistry, reason_error};
use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most bytes kept of one failing test's output.
pub const MAX_FAILURE_BYTES: usize = 16 * 1024;

/// Most failures whose output is kept (the rest are named only).
pub const MAX_FAILURES_KEPT: usize = 64;

/// Most compiler error lines kept.
pub const MAX_COMPILE_ERRORS: usize = 64;

/// Longest accepted package, target or filter name.
pub const MAX_NAME_BYTES: usize = 128;

/// The longest output line kept, in bytes; the rest of a longer line is dropped.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// How long the worker waits for the output pipes to close after a cancel kill, in 100 ms
/// polls. A process that survives the kill (and holds the pipes open) does not keep the job
/// running past this.
const CANCEL_GRACE_POLLS: u32 = 50;

/// One meaningful line of `cargo test` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestLine {
    /// `     Running <what> (<path>)` or `   Doc-tests <crate>`: a test binary starts.
    Binary(String),
    /// `running N tests`.
    Running(u64),
    /// `test <name> ... <outcome>`.
    Outcome {
        /// Test path.
        name: String,
        /// `ok`, `failed`, `ignored` or `bench`.
        outcome: &'static str,
    },
    /// `---- <name> stdout ----`: the captured output of a failing test follows.
    FailureHeader(String),
    /// `test result: ...`: one binary's totals.
    Summary {
        /// Whether the binary passed.
        ok: bool,
        /// Passed tests.
        passed: u64,
        /// Failed tests.
        failed: u64,
        /// Ignored tests.
        ignored: u64,
        /// Tests the filter left out.
        filtered_out: u64,
    },
    /// `failures:` or `successes:`: the end of the failure sections.
    SectionEnd,
    /// `error...` from cargo or the compiler.
    Error(String),
}

/// Reads one line of `cargo test` output. `None` for every other line (test output, warnings,
/// progress), which the runner treats as part of a failure section when one is open.
#[must_use]
pub fn parse_line(line: &str) -> Option<TestLine> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let lead = trimmed.trim_start();
    if let Some(rest) = lead.strip_prefix("Running ")
        && trimmed.len() > lead.len()
    {
        return Some(TestLine::Binary(rest.trim().to_string()));
    }
    if let Some(rest) = lead.strip_prefix("Doc-tests ")
        && trimmed.len() > lead.len()
    {
        return Some(TestLine::Binary(format!("doc-tests {}", rest.trim())));
    }
    if let Some(rest) = trimmed.strip_prefix("running ") {
        let count = rest
            .strip_suffix(" tests")
            .or_else(|| rest.strip_suffix(" test"))?;
        return count.parse().ok().map(TestLine::Running);
    }
    if let Some(rest) = trimmed.strip_prefix("test result: ") {
        return Some(parse_summary(rest));
    }
    if let Some(rest) = trimmed.strip_prefix("test ")
        && let Some((name, outcome)) = rest.rsplit_once(" ... ")
    {
        let outcome = match outcome.trim() {
            "ok" => "ok",
            "FAILED" => "failed",
            "bench" => "bench",
            other if other == "ignored" || other.starts_with("ignored,") => "ignored",
            _ => return None,
        };
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        return Some(TestLine::Outcome {
            name: name.to_string(),
            outcome,
        });
    }
    if let Some(rest) = trimmed.strip_prefix("---- ")
        && let Some(name) = rest
            .strip_suffix(" stdout ----")
            .or_else(|| rest.strip_suffix(" stderr ----"))
    {
        return Some(TestLine::FailureHeader(name.trim().to_string()));
    }
    if trimmed == "failures:" || trimmed == "successes:" {
        return Some(TestLine::SectionEnd);
    }
    if lead.starts_with("error:") || lead.starts_with("error[") {
        return Some(TestLine::Error(lead.to_string()));
    }
    None
}

/// Parses the part of a summary after `test result: `, e.g.
/// `ok. 3 passed; 0 failed; 1 ignored; 0 measured; 2 filtered out; finished in 0.01s`.
fn parse_summary(rest: &str) -> TestLine {
    let ok = rest.starts_with("ok");
    let count = |word: &str| {
        rest.split(';')
            .find_map(|part| {
                let part = part
                    .trim()
                    .trim_start_matches("ok.")
                    .trim_start_matches("FAILED.")
                    .trim();
                part.strip_suffix(word).and_then(|n| n.trim().parse::<u64>().ok())
            })
            .unwrap_or(0)
    };
    TestLine::Summary {
        ok,
        passed: count("passed"),
        failed: count("failed"),
        ignored: count("ignored"),
        filtered_out: count("filtered out"),
    }
}

/// The running totals and failure texts of one run, fed line by line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunTally {
    /// Test binaries seen.
    pub binaries: Vec<String>,
    /// Passed tests (outcome lines).
    pub passed: u64,
    /// Failed tests.
    pub failed: u64,
    /// Ignored tests.
    pub ignored: u64,
    /// Filtered-out tests (from the summaries).
    pub filtered_out: u64,
    /// Failing test names, in order.
    pub failed_names: Vec<String>,
    /// Captured output per failing test (up to [`MAX_FAILURES_KEPT`]).
    pub failure_output: Vec<(String, String)>,
    /// Compiler and cargo error lines (up to [`MAX_COMPILE_ERRORS`]).
    pub errors: Vec<String>,
    /// The failure section being read.
    open: Option<usize>,
}

impl RunTally {
    /// Feeds one output line; answers the event to stream for it, if any.
    pub fn feed(&mut self, line: &str) -> Option<Value> {
        let parsed = parse_line(line);
        match parsed {
            None => {
                if let Some(index) = self.open
                    && let Some((_, text)) = self.failure_output.get_mut(index)
                    && text.len() < MAX_FAILURE_BYTES
                {
                    let room = MAX_FAILURE_BYTES - text.len();
                    let mut cut = line.len().min(room.saturating_sub(1));
                    while !line.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.push_str(&line[..cut]);
                    text.push('\n');
                }
                None
            }
            Some(parsed) => {
                // `Binary` and `Error` lines come from cargo's stderr, read by another thread
                // than the test's stdout, so they may arrive in the middle of a failure
                // section: only stdout's own structure lines close it.
                if !matches!(
                    parsed,
                    TestLine::FailureHeader(_) | TestLine::Binary(_) | TestLine::Error(_)
                ) {
                    self.open = None;
                }
                match parsed {
                    TestLine::Binary(name) => {
                        self.binaries.push(name.clone());
                        Some(json!({ "kind": "binary", "name": name }))
                    }
                    TestLine::Running(count) => Some(json!({ "kind": "running", "tests": count })),
                    TestLine::Outcome { name, outcome } => {
                        match outcome {
                            "ok" | "bench" => self.passed += 1,
                            "failed" => {
                                self.failed += 1;
                                self.failed_names.push(name.clone());
                            }
                            _ => self.ignored += 1,
                        }
                        Some(json!({ "kind": "test", "name": name, "outcome": outcome }))
                    }
                    TestLine::FailureHeader(name) => {
                        self.open = None;
                        if self.failure_output.len() < MAX_FAILURES_KEPT {
                            self.failure_output.push((name, String::new()));
                            self.open = Some(self.failure_output.len() - 1);
                        }
                        None
                    }
                    TestLine::Summary {
                        ok,
                        passed,
                        failed,
                        ignored,
                        filtered_out,
                    } => {
                        self.filtered_out += filtered_out;
                        Some(json!({
                            "kind": "summary", "ok": ok, "passed": passed, "failed": failed,
                            "ignored": ignored, "filtered_out": filtered_out,
                        }))
                    }
                    TestLine::SectionEnd => None,
                    TestLine::Error(text) => {
                        if self.errors.len() < MAX_COMPILE_ERRORS {
                            self.errors.push(text.clone());
                        }
                        Some(json!({ "kind": "error", "message": text }))
                    }
                }
            }
        }
    }

    /// The job's result once the process ended with `exit_code`.
    #[must_use]
    pub fn result(&self, exit_code: Option<i32>, command: &[String]) -> Value {
        let failures: Vec<Value> = self
            .failed_names
            .iter()
            .map(|name| {
                let output = self
                    .failure_output
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, text)| text.as_str());
                json!({ "name": name, "output": output })
            })
            .collect();
        json!({
            "ok": exit_code == Some(0) && self.failed == 0,
            "exit_code": exit_code,
            "passed": self.passed,
            "failed": self.failed,
            "ignored": self.ignored,
            "filtered_out": self.filtered_out,
            "binaries": self.binaries,
            "failures": failures,
            "errors": self.errors,
            "command": command,
        })
    }
}

/// Checks a name that reaches the command line: 1..=[`MAX_NAME_BYTES`] bytes of ASCII letters,
/// digits, `_`, `-`, `:` and `.`, not starting with `-`.
///
/// # Errors
/// `run_tests_bad_name` naming `key`.
pub fn check_name(key: &str, name: &str) -> Result<(), RpcError> {
    check_name_as(key, name, "run_tests_bad_name")
}

/// [`check_name`] answering a caller's reason code (`build_game_bad_name` for
/// [`crate::build_game`], so a refusal names the method that answered it).
///
/// # Errors
/// `reason` naming `key`.
pub fn check_name_as(key: &str, name: &str, reason: &str) -> Result<(), RpcError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            reason,
            format!(
                "`{key}` must be 1..={MAX_NAME_BYTES} ASCII letters, digits, `_`, `-`, `:` or `.`, not starting with `-` (got {name:?})"
            ),
        ))
    }
}

/// The `cargo test` arguments for a request (without the program).
///
/// # Errors
/// `run_tests_bad_name` for a name outside the allowed alphabet.
pub fn command_args(params: &Value) -> Result<Vec<String>, RpcError> {
    let text = |key: &str| params.get(key).and_then(Value::as_str);
    // Spec 23 § 3.6 names the call `run_tests(filter)`: without a target every target runs.
    let target = text("target").unwrap_or("all");
    let mut args = vec!["test".to_string(), "--color".into(), "never".into()];
    if let Some(package) = text("package") {
        check_name("package", package)?;
        args.extend(["--package".into(), package.to_string()]);
    }
    match target {
        "all" => {}
        "lib" => args.push("--lib".into()),
        "bins" => args.push("--bins".into()),
        "doc" => args.push("--doc".into()),
        name => {
            check_name("target", name)?;
            args.extend(["--test".into(), name.to_string()]);
        }
    }
    if params.get("offline").and_then(Value::as_bool).unwrap_or(true) {
        args.push("--offline".into());
    }
    args.push("--".into());
    if let Some(filter) = text("filter") {
        check_name("filter", filter)?;
        args.push(filter.to_string());
    }
    if params.get("exact").and_then(Value::as_bool).unwrap_or(false) {
        args.push("--exact".into());
    }
    if params
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        args.push("--include-ignored".into());
    }
    Ok(args)
}

/// The cargo program to run: the host's override ([`RunTestsProgram`]), else `$CARGO` (set when
/// the app itself runs under cargo), else `cargo`.
fn program(world: &World) -> PathBuf {
    world
        .get_resource::<RunTestsProgram>()
        .map(|p| p.0.clone())
        .or_else(|| std::env::var_os("CARGO").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

/// The cargo program to run instead of `cargo` (a host with its own toolchain path; tests).
#[derive(Resource, Debug, Clone)]
pub struct RunTestsProgram(pub PathBuf);

/// Extra environment for the run (tests point `CARGO_TARGET_DIR` at a temp dir).
#[derive(Resource, Debug, Clone, Default)]
pub struct RunTestsEnv(pub Vec<(String, String)>);

/// A line-oriented tally of one child-process run ([`agent.run_tests`], [`crate::build_game`]):
/// feeds output lines, answers the events to stream for each, and builds the job's result value.
pub(crate) trait Tally: Send {
    /// Feeds one output line; answers the events to stream for it (usually zero or one).
    fn feed(&mut self, line: &str) -> Vec<Value>;

    /// A progress message for a streamed event, when one applies.
    fn progress(&self, event: &Value) -> Option<String>;

    /// The job's result value for the child's `exit_code` (`None` when it could not be read).
    fn result(&mut self, exit_code: Option<i32>, killed: bool, command: &[String]) -> Value;
}

/// `agent.run_tests`: starts the run and answers its job id (or, with `dry_run`, the command).
pub(crate) fn run_tests(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let args = command_args(params)?;
    let root = crate::req_path::project_root(world);
    if !root.join("Cargo.toml").is_file() {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "run_tests_no_manifest",
            "the project root has no Cargo.toml, so it has no test targets",
        ));
    }
    let program = program(world);
    let mut command_line = vec![program.display().to_string()];
    command_line.extend(args.iter().cloned());
    if params.get("dry_run").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(json!({ "dry_run": true, "command": command_line, "cwd": root.display().to_string() }));
    }
    let env = world.get_resource::<RunTestsEnv>().cloned().unwrap_or_default();
    let mut registry = world.get_resource_or_insert_with::<JobRegistry>(Default::default);
    if let Some(running) = registry.running("tests").or_else(|| registry.running("golden")) {
        return Err(reason_error(
            INVALID_PARAMS,
            "F0002",
            "run_tests_busy",
            format!(
                "{running} is still running; poll it with agent.job_status or stop it with agent.job_cancel"
            ),
        ));
    }
    let label = params
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("all")
        .to_string();
    let (job, handle) = registry.start("tests", label, true);
    let run = Run {
        program: program.clone(),
        args,
        root,
        env: env.0,
        command_line: command_line.clone(),
        start_message: "compiling and running",
        thread_name: "regia-run-tests",
        tally: Box::new(RunTally::default()),
        finish: Box::new(|result| result),
    };
    if let Err(error) = spawn_run(run, handle.clone()) {
        handle.fail(
            "run_tests_spawn_failed",
            format!("cannot start {}: {error}", program.display()),
        );
    }
    Ok(json!({ "job": job, "command": command_line }))
}

/// The cargo program and environment `agent.run_tests` and `agent.golden_test` run with.
pub(crate) fn host_command(world: &World) -> (PathBuf, Vec<(String, String)>) {
    (
        program(world),
        world.get_resource::<RunTestsEnv>().cloned().unwrap_or_default().0,
    )
}

/// One child-process run for [`spawn_run`] (a test run, a [`crate::build_game`] build).
pub(crate) struct Run {
    /// The cargo program.
    pub program: PathBuf,
    /// Its arguments ([`command_args`]).
    pub args: Vec<String>,
    /// The project root, its working directory.
    pub root: PathBuf,
    /// Extra environment.
    pub env: Vec<(String, String)>,
    /// The command line the result reports.
    pub command_line: Vec<String>,
    /// The job's first progress message.
    pub start_message: &'static str,
    /// The worker thread's name (its output readers add `-io`).
    pub thread_name: &'static str,
    /// The line tally that turns output into events and the result.
    pub tally: Box<dyn Tally>,
    /// Turns the run's result into the job's, on the worker.
    pub finish: Box<dyn FnOnce(Value) -> Value + Send>,
}

/// Removes the caller's libtest harness variables (`RUST_TEST_NOCAPTURE`, `RUST_TEST_THREADS`,
/// ...) from a nested run's environment. The agent's own process may run under a test harness —
/// a GPU sweep exports `RUST_TEST_NOCAPTURE` to count adapter-loss lines — and a nested
/// `cargo test` that inherits one runs with the caller's flags and its output changes: under
/// `RUST_TEST_NOCAPTURE` libtest prints a failing test's output as it happens instead of in its
/// `---- <name> stdout ----` section, so every failure's `output` came back `null` (ADR-7611).
/// The run's explicit environment ([`RunTestsEnv`]) is applied after this and may still set them.
/// `host` names the variables of the agent's own process (`std::env::vars_os` in a real run).
fn scrub_harness_env(command: &mut Command, host: impl IntoIterator<Item = std::ffi::OsString>) {
    for name in host {
        if name.as_encoded_bytes().starts_with(b"RUST_TEST_") {
            command.env_remove(&name);
        }
    }
}

/// The child process of one run: `program args` in `root` with the run's environment, piped
/// output, no golden bless or skip variables, and none of the host's own `RUST_TEST_*` harness
/// variables ([`scrub_harness_env`]; the run's explicit `env` wins over that scrub).
fn child_command(
    program: &std::path::Path,
    args: &[String],
    root: &std::path::Path,
    env: &[(String, String)],
) -> Command {
    let host = std::env::vars_os().map(|(name, _)| name);
    child_command_from_host(program, args, root, env, host)
}

/// [`child_command`] for a host environment named by `host` (the unit tests pass their own).
fn child_command_from_host(
    program: &std::path::Path,
    args: &[String],
    root: &std::path::Path,
    env: &[(String, String)],
    host: impl IntoIterator<Item = std::ffi::OsString>,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .env("CARGO_TERM_COLOR", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_harness_env(&mut command, host);
    for (key, value) in env {
        command.env(key, value);
    }
    scrub_golden_env(&mut command);
    // Its own process group, so a cancel stops cargo's rustc and test processes too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
}

/// Starts the worker thread that runs `run` and feeds `handle`. The child never sees the golden
/// bless or skip variables (ADR-2821): a compare under the agent protocol only ever compares.
/// Nor does it see the caller's `RUST_TEST_*` harness variables ([`scrub_harness_env`]): the
/// agent's own process may run under a test harness, and a nested `cargo test` that inherits
/// them changes behaviour and fails.
pub(crate) fn spawn_run(run: Run, handle: JobHandle) -> std::io::Result<()> {
    let Run {
        program,
        args,
        root,
        env,
        command_line,
        start_message,
        thread_name,
        tally,
        finish,
    } = run;
    let mut command = child_command(&program, &args, &root, &env);
    let mut child = command.spawn()?;
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    for stream in [
        stdout.map(|s| Box::new(s) as Box<dyn Read + Send>),
        stderr.map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = lines_tx.clone();
        std::thread::Builder::new()
            .name(format!("{thread_name}-io"))
            .spawn(move || {
                let mut reader = BufReader::new(stream);
                while let Some(line) = read_line_lossy(&mut reader) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })?;
    }
    drop(lines_tx);
    handle.progress(None, start_message);
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || supervise(child, &lines_rx, &handle, &command_line, tally, finish))?;
    Ok(())
}

/// Reads one output line: invalid UTF-8 is replaced (a test printing raw bytes does not end
/// the stream), and a line longer than [`MAX_LINE_BYTES`] keeps only its start. `None` at the
/// end of the stream or on a read error.
fn read_line_lossy(reader: &mut impl BufRead) -> Option<String> {
    let mut line = Vec::new();
    let mut dropped = false;
    loop {
        let (consumed, done) = {
            let chunk = match reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            };
            if chunk.is_empty() {
                if line.is_empty() && !dropped {
                    return None;
                }
                break;
            }
            let (part, done) = match chunk.iter().position(|&b| b == b'\n') {
                Some(end) => (&chunk[..end], true),
                None => (chunk, false),
            };
            let room = MAX_LINE_BYTES.saturating_sub(line.len());
            if part.len() > room {
                dropped = true;
            }
            line.extend_from_slice(&part[..part.len().min(room)]);
            (part.len() + usize::from(done), done)
        };
        reader.consume(consumed);
        if done {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Some(String::from_utf8_lossy(&line).into_owned())
}

/// Stops the run: the whole process group on Unix (cargo, rustc and the test binaries), the
/// process tree on Windows, else cargo alone. Best effort: a failure leaves the grace timeout.
fn kill_run(child: &mut Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// The worker loop: feeds output lines to the tally and the job, kills the run on cancel, and
/// finishes the job when the output ends (or the cancel grace runs out) and the child exits.
fn supervise(
    mut child: Child,
    lines: &mpsc::Receiver<String>,
    handle: &JobHandle,
    command_line: &[String],
    mut tally: Box<dyn Tally>,
    finish: Box<dyn FnOnce(Value) -> Value + Send>,
) {
    let mut killed = false;
    let mut grace = CANCEL_GRACE_POLLS;
    loop {
        if handle.cancel_requested() && !killed {
            kill_run(&mut child);
            killed = true;
        }
        if killed {
            if grace == 0 {
                break;
            }
            grace -= 1;
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                for event in tally.feed(&line) {
                    if let Some(message) = tally.progress(&event) {
                        handle.progress(None, message);
                    }
                    handle.event(event);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let exit_code = match child.wait() {
        Ok(status) => status.code(),
        Err(error) => {
            handle.fail(
                "run_tests_spawn_failed",
                format!("waiting for cargo failed: {error}"),
            );
            return;
        }
    };
    handle.finish(finish(tally.result(exit_code, killed, command_line)));
}

impl Tally for RunTally {
    fn feed(&mut self, line: &str) -> Vec<Value> {
        RunTally::feed(self, line).into_iter().collect()
    }

    fn progress(&self, event: &Value) -> Option<String> {
        if event["kind"] == "binary" {
            Some(format!("running {}", event["name"].as_str().unwrap_or_default()))
        } else {
            None
        }
    }

    fn result(&mut self, exit_code: Option<i32>, killed: bool, command: &[String]) -> Value {
        let mut result = RunTally::result(self, exit_code, command);
        // No test ran and cargo reported errors: the build failed (the errors name why).
        let build_failed = !killed
            && self.passed + self.failed + self.ignored == 0
            && exit_code != Some(0)
            && !self.errors.is_empty();
        if let Some(object) = result.as_object_mut() {
            object.insert("build_failed".into(), json!(build_failed));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dry_run reply's `cwd` is the real project root, not "." (the command has no
    /// `--project` flag; running it in the caller's directory would test the wrong project).
    #[test]
    fn dry_run_answers_the_real_project_root_as_cwd() {
        let root = regia_test_utils::unique_temp_dir("run_tests_dry_run_cwd");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"dry\"\n").expect("Cargo.toml");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let answer = run_tests(&mut world, &json!({ "dry_run": true })).expect("dry run");
        assert_eq!(answer["dry_run"], json!(true));
        assert_eq!(answer["cwd"], json!(root.display().to_string()), "{answer}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// ADR-7611: a host running under `RUST_TEST_NOCAPTURE=1` (a test runner that wants live
    /// output) must not pass it to the child run, or libtest drops the failures section the tally
    /// reads each failure's `output` from. The run's explicit environment still wins. The host's
    /// variables are named here, so the test does not depend on how it was itself started (the
    /// spawned end-to-end check is `the_nested_run_does_not_inherit_the_harness_env`).
    #[test]
    fn a_child_run_does_not_inherit_the_hosts_libtest_capture_setting() {
        use std::ffi::{OsStr, OsString};
        use std::path::Path;
        let value = |command: &Command, name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| value.map(OsStr::to_os_string))
        };
        let host = || ["RUST_TEST_NOCAPTURE", "RUST_TEST_THREADS", "PATH"].map(OsString::from);
        let args = vec!["test".to_string()];
        let inherited = child_command_from_host(Path::new("cargo"), &args, Path::new("."), &[], host());
        assert_eq!(
            value(&inherited, "RUST_TEST_NOCAPTURE"),
            Some(None),
            "removed from the child's environment"
        );
        assert_eq!(
            value(&inherited, "RUST_TEST_THREADS"),
            Some(None),
            "every RUST_TEST_ harness variable is removed"
        );
        assert_eq!(value(&inherited, "PATH"), None, "other variables are inherited");
        let explicit = child_command_from_host(
            Path::new("cargo"),
            &args,
            Path::new("."),
            &[("RUST_TEST_NOCAPTURE".to_string(), "1".to_string())],
            host(),
        );
        assert_eq!(
            value(&explicit, "RUST_TEST_NOCAPTURE"),
            Some(Some("1".into())),
            "the run's own setting is kept"
        );
    }

    const SAMPLE: &str = "\
   Compiling demo v0.1.0 (/tmp/demo)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.50s
     Running unittests src/lib.rs (target/debug/deps/demo-0123abcd)

running 3 tests
test tests::adds ... ok
test tests::skipped ... ignored, needs a GPU
test tests::breaks ... FAILED

failures:

---- tests::breaks stdout ----

thread 'tests::breaks' panicked at src/lib.rs:9:9:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    tests::breaks

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 2 filtered out; finished in 0.00s

error: test failed, to rerun pass `--lib`
";

    #[test]
    fn the_sample_run_tallies_outcomes_failures_and_errors() {
        let mut tally = RunTally::default();
        let events: Vec<Value> = SAMPLE.lines().filter_map(|l| tally.feed(l)).collect();
        assert_eq!(tally.passed, 1);
        assert_eq!(tally.failed, 1);
        assert_eq!(tally.ignored, 1);
        assert_eq!(tally.filtered_out, 2);
        assert_eq!(
            tally.binaries,
            vec!["unittests src/lib.rs (target/debug/deps/demo-0123abcd)"]
        );
        assert_eq!(tally.failed_names, vec!["tests::breaks"]);
        let output = &tally.failure_output[0].1;
        assert!(output.contains("assertion `left == right` failed"), "{output}");
        assert!(
            !output.contains("tests::breaks\n    "),
            "the failures list is not part of the output"
        );
        assert_eq!(tally.errors.len(), 1);
        let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(
            kinds,
            ["binary", "running", "test", "test", "test", "summary", "error"]
        );
        let result = tally.result(Some(101), &["cargo".into()]);
        assert_eq!(result["ok"], false);
        assert_eq!(result["failures"][0]["name"], "tests::breaks");
    }

    /// stdout and stderr reach the tally through two reader threads, so cargo's stderr lines
    /// (`error: test failed`, the next binary's `Running`) can overtake the end of a failing
    /// test's captured stdout. They must not cut that output short.
    #[test]
    fn stderr_lines_overtaking_stdout_keep_the_failure_output() {
        let mut tally = RunTally::default();
        for line in [
            "---- tests::breaks stdout ----",
            "",
            "thread 'tests::breaks' (17) panicked at src/lib.rs:6:19:",
            "error: test failed, to rerun pass `--lib`",
            "     Running unittests src/main.rs (target/debug/deps/demo-4567)",
            "assertion `left == right` failed: deliberate",
            "  left: 1",
            "failures:",
            "    tests::breaks",
        ] {
            tally.feed(line);
        }
        let output = &tally.failure_output[0].1;
        assert!(output.contains("panicked at src/lib.rs:6:19:"), "{output}");
        assert!(output.contains("failed: deliberate\n  left: 1"), "{output}");
        assert!(!output.contains("error: test failed"), "{output}");
        assert!(!output.contains("Running"), "{output}");
        assert!(!output.contains("    tests::breaks"), "{output}");
        assert_eq!(tally.errors, ["error: test failed, to rerun pass `--lib`"]);
        assert_eq!(tally.binaries.len(), 1);
    }

    #[test]
    fn lines_parse_to_their_kinds() {
        assert_eq!(parse_line("running 1 test"), Some(TestLine::Running(1)));
        assert_eq!(parse_line("running 12 tests"), Some(TestLine::Running(12)));
        assert_eq!(
            parse_line("   Doc-tests demo"),
            Some(TestLine::Binary("doc-tests demo".into()))
        );
        assert_eq!(
            parse_line("test a::b ... ok"),
            Some(TestLine::Outcome {
                name: "a::b".into(),
                outcome: "ok"
            })
        );
        assert_eq!(
            parse_line("test src/lib.rs - f (line 3) ... ok").map(|_| ()),
            Some(())
        );
        assert_eq!(parse_line("test a ... weird"), None);
        assert_eq!(parse_line("test  ... ok"), None);
        assert_eq!(
            parse_line(
                "test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s"
            ),
            Some(TestLine::Summary {
                ok: true,
                passed: 4,
                failed: 0,
                ignored: 0,
                filtered_out: 0
            })
        );
        assert_eq!(
            parse_line("Running is not a banner"),
            None,
            "banners are indented"
        );
        assert_eq!(parse_line("hello"), None);
    }

    #[test]
    fn names_that_could_add_flags_are_refused() {
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        for bad in ["", "-x", "--offline", "a b", "a;b", "a/b", "ü", long.as_str()] {
            assert!(check_name("target", bad).is_err(), "{bad:?}");
        }
        for good in ["lib_tests", "my-crate", "tests::adds", "v1.2"] {
            assert!(check_name("target", good).is_ok(), "{good}");
        }
        let args =
            command_args(&json!({ "target": "smoke", "package": "game", "filter": "door", "exact": true }))
                .expect("args");
        assert_eq!(
            args,
            [
                "test",
                "--color",
                "never",
                "--package",
                "game",
                "--test",
                "smoke",
                "--offline",
                "--",
                "door",
                "--exact"
            ]
        );
        let all = command_args(&json!({ "target": "all", "offline": false })).expect("all");
        assert_eq!(all, ["test", "--color", "never", "--"]);
        // Spec 23 § 3.6: `run_tests(filter)` alone runs every target.
        assert_eq!(
            command_args(&json!({ "filter": "door" })).expect("filter only"),
            ["test", "--color", "never", "--offline", "--", "door"]
        );
        assert!(command_args(&json!({ "target": "lib", "filter": "--nocapture" })).is_err());
    }

    #[test]
    fn output_lines_survive_invalid_utf8_and_are_capped() {
        let mut long = vec![b'x'; MAX_LINE_BYTES + 100];
        long.push(b'\n');
        let mut input = b"test a ... ok\r\ncaf\xe9 \xff\n".to_vec();
        input.extend_from_slice(&long);
        input.extend_from_slice(b"test b ... ok");
        let mut reader = std::io::Cursor::new(input);
        assert_eq!(read_line_lossy(&mut reader).as_deref(), Some("test a ... ok"));
        assert_eq!(
            read_line_lossy(&mut reader).as_deref(),
            Some("caf\u{fffd} \u{fffd}")
        );
        assert_eq!(
            read_line_lossy(&mut reader).map(|l| l.len()),
            Some(MAX_LINE_BYTES)
        );
        assert_eq!(read_line_lossy(&mut reader).as_deref(), Some("test b ... ok"));
        assert_eq!(read_line_lossy(&mut reader), None);
        // The tally keeps counting after the bad bytes.
        let mut tally = RunTally::default();
        for line in ["test a ... ok", "caf\u{fffd}", "test b ... ok"] {
            tally.feed(line);
        }
        assert_eq!(tally.passed, 2);
    }

    #[test]
    fn failure_output_is_capped_on_a_char_boundary() {
        let mut tally = RunTally::default();
        tally.feed("test big ... FAILED");
        tally.feed("---- big stdout ----");
        let line = "é".repeat(MAX_FAILURE_BYTES);
        tally.feed(&line);
        tally.feed(&line);
        assert!(tally.failure_output[0].1.len() <= MAX_FAILURE_BYTES);
    }

    /// The parser and the tally take arbitrary text (fuzz target `agent.run_tests_output`).
    #[test]
    fn mutated_output_never_panics_and_counts_stay_consistent() {
        let mut rng = regia_core::fuzz::FuzzRng::new(0x2319_0001);
        for _ in 0..2000 {
            let text = regia_core::fuzz::mutate_text(SAMPLE, &mut rng);
            let mut tally = RunTally::default();
            for line in text.lines() {
                tally.feed(line);
            }
            assert_eq!(tally.failed as usize, tally.failed_names.len());
            assert!(tally.failure_output.len() <= MAX_FAILURES_KEPT);
            assert!(
                tally
                    .failure_output
                    .iter()
                    .all(|(_, t)| t.len() <= MAX_FAILURE_BYTES + 1)
            );
        }
    }

    /// Records the name of the thread the tally is fed on (the run's supervisor).
    #[cfg(unix)]
    struct ThreadNameTally(mpsc::Sender<String>);

    #[cfg(unix)]
    impl Tally for ThreadNameTally {
        fn feed(&mut self, _line: &str) -> Vec<Value> {
            let name = std::thread::current().name().unwrap_or_default().to_string();
            let _ = self.0.send(name);
            Vec::new()
        }

        fn progress(&self, _event: &Value) -> Option<String> {
            None
        }

        fn result(&mut self, _exit_code: Option<i32>, _killed: bool, _command: &[String]) -> Value {
            json!({})
        }
    }

    /// The worker threads carry the run's own name (a build is `regia-build-game`, not
    /// `regia-run-tests`; review rk9 finding 10).
    #[cfg(unix)]
    #[test]
    fn the_worker_thread_is_named_after_the_run() {
        let root = regia_test_utils::unique_temp_dir("run-thread-name");
        let (tx, rx) = mpsc::channel();
        let (_, handle) = JobRegistry::default().start("build", "name probe", true);
        let run = Run {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "echo line".into()],
            root: root.clone(),
            env: Vec::new(),
            command_line: vec!["sh".into()],
            start_message: "probing",
            thread_name: crate::build_game::THREAD_NAME,
            tally: Box::new(ThreadNameTally(tx)),
            finish: Box::new(|result| result),
        };
        spawn_run(run, handle).expect("spawn sh");
        // A bounded receive (not a sleep): the tally sends on its first line.
        let name = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the tally was fed");
        assert_eq!(name, "regia-build-game");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every line the child printed, in order (the env probe's tally).
    #[cfg(unix)]
    struct LinesTally(mpsc::Sender<String>);

    #[cfg(unix)]
    impl Tally for LinesTally {
        fn feed(&mut self, line: &str) -> Vec<Value> {
            let _ = self.0.send(line.to_string());
            Vec::new()
        }

        fn progress(&self, _event: &Value) -> Option<String> {
            None
        }

        fn result(&mut self, _exit_code: Option<i32>, _killed: bool, _command: &[String]) -> Value {
            json!({})
        }
    }

    /// Child mode of [`the_nested_run_does_not_inherit_the_harness_env`]: the harness variables
    /// are set on this process by the parent, so a nested run through [`spawn_run`] must not
    /// carry them down. A no-op in a normal run. Unix only: the probe is an `sh` script, like the
    /// other `sh`-spawning tests in this file (sprint-rev-w9-zai-k finding 1 on framework-smalls).
    #[cfg(unix)]
    #[test]
    #[ignore = "run by the_nested_run_does_not_inherit_the_harness_env"]
    fn child_nested_run_env_probe() {
        let root = regia_test_utils::unique_temp_dir("run-env-probe");
        let (tx, rx) = mpsc::channel();
        let (_, handle) = JobRegistry::default().start("test", "env probe", true);
        let run = Run {
            program: PathBuf::from("sh"),
            args: vec![
                "-c".into(),
                "if [ -n \"$RUST_TEST_NOCAPTURE\" ] || [ -n \"$RUST_TEST_THREADS\" ]; then echo LEAKED; \
                 else echo CLEAN; fi"
                    .into(),
            ],
            root: root.clone(),
            env: Vec::new(),
            command_line: vec!["sh".into()],
            start_message: "probing",
            thread_name: crate::build_game::THREAD_NAME,
            tally: Box::new(LinesTally(tx)),
            finish: Box::new(|result| result),
        };
        spawn_run(run, handle).expect("spawn sh");
        // Bounded: the channel closes when the run ends and the tally is dropped.
        let mut lines = Vec::new();
        while let Ok(line) = rx.recv_timeout(Duration::from_secs(60)) {
            lines.push(line);
        }
        assert_eq!(lines, ["CLEAN"], "the nested run saw a harness variable");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The agent's own process may run under a test harness (a GPU sweep exports
    /// `RUST_TEST_NOCAPTURE` to count adapter-loss lines), and a nested `cargo test` that
    /// inherits it changes behaviour and fails. The nested run carries none of it. Unix only, with
    /// its `sh` child.
    #[cfg(unix)]
    #[test]
    fn the_nested_run_does_not_inherit_the_harness_env() {
        let exe = std::env::current_exe().expect("test binary");
        let out = std::process::Command::new(&exe)
            .args([
                "--ignored",
                "--exact",
                // libtest names a unit test by its module path inside the crate.
                "run_tests::tests::child_nested_run_env_probe",
                "--test-threads=1",
            ])
            .env("RUST_TEST_NOCAPTURE", "1")
            .env("RUST_TEST_THREADS", "1")
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "the probe failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
    }
}
