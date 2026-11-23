// SPDX-License-Identifier: MPL-2.0
//! `regia test` and `regia test session` (`docs/specs/41-functional-tests-and-session-harness.md` § 2.7, § 2.8).
//!
//! `regia test` starts the project's game on one level (a one-process session) with
//! `REGIA_SESSION_RENDER=0`. That variable is advisory: a desktop build still opens its window
//! (spec 41 § 2.8). The command runs the functional tests placed in the level through the agent's
//! `functest.*` methods, writes the JSON report and optional JUnit XML, and exits 0 (all passed),
//! 1 (a failure), 2 (aborted or undecided), 3 (no tests ran) or 4 (harness or launch failure).
//! `regia test session FILE` runs a multi-process session. `regia test --platform android` starts
//! nothing: it drives the tests of the game already running on a device, through the agent
//! connection `regia install-android --attach` set up (spec 41 § 8 test 20, ADR-2137).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use regia_cli::session::{
    self, Instance, LaunchOptions, Poll, Session, SpawnSpec, connect_ready, spawn_instance, wait_ready,
    ws_connector,
};
use regia_framework::functional::report::{EXIT_HARNESS, TestReport};
use serde_json::{Value, json};

/// Arguments of `regia test`.
#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct TestArgs {
    /// `regia test session FILE`.
    #[command(subcommand)]
    pub session: Option<TestSubcommand>,
    /// Project directory.
    #[arg(default_value = ".")]
    pub project: PathBuf,
    /// Level to load (`.level.ron` under the project).
    #[arg(long)]
    pub level: Option<PathBuf>,
    /// Name glob; repeatable (any-of).
    #[arg(long)]
    pub filter: Vec<String>,
    /// Tag; repeatable (all-of).
    #[arg(long)]
    pub tag: Vec<String>,
    /// Seed for the tests' random generator.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// `key=value` parameter; repeatable.
    #[arg(long = "param", value_parser = parse_param)]
    pub params: Vec<(String, String)>,
    /// Write the JSON report here.
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Write a JUnit XML report here.
    #[arg(long)]
    pub junit: Option<PathBuf>,
    /// Write missing or changed expected values instead of failing.
    #[arg(long)]
    pub update_expected: bool,
    /// Repeat the set until the game exits (soak use).
    #[arg(long = "loop")]
    pub looped: bool,
    /// A built game executable to run instead of building the project's launcher.
    #[arg(long)]
    pub exe: Option<PathBuf>,
    /// Seconds a single-shot run waits for the tests to finish (status polling) before failing
    /// with a harness error (exit 4); at least 1, default 60. The ready wait keeps its own 60 s
    /// budget. A soak (`--loop`) runs until the game exits and ignores this.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout_s: u64,
    /// Folder for process logs (default: a `test-artifacts` folder under the project's state directory).
    #[arg(long)]
    pub artifacts: Option<PathBuf>,
    /// Where the game runs: `desktop` (default; this machine, and the game still opens its window)
    /// or `android` (the game already running on the attached device; run
    /// `regia install-android --attach` first).
    #[arg(long, value_enum, default_value_t = TestPlatform::Desktop)]
    pub platform: TestPlatform,
    /// The device agent with `--platform android`: `attached` (default, the endpoint
    /// `regia install-android --attach` wrote) or `127.0.0.1:<port>` of a forwarded agent.
    #[arg(long, default_value = session::REMOTE_ATTACHED)]
    pub address: String,
}

/// Where `regia test` runs the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TestPlatform {
    /// Started on this machine. `REGIA_SESSION_RENDER=0` is set; the desktop game still opens its window.
    Desktop,
    /// Already running on an attached Android device.
    Android,
}

/// `regia test` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum TestSubcommand {
    /// Launch the processes of a `.session.ron`, drive them and decide from verdicts and logs.
    Session(SessionArgs),
}

/// Arguments of `regia test session`.
#[derive(Debug, Clone, Args)]
pub struct SessionArgs {
    /// The `.session.ron` file.
    pub file: PathBuf,
    /// Write `session-progress.json`, restart dead processes and skip completed tests.
    #[arg(long)]
    pub recover: bool,
    /// Keep logs and artifacts after a clean run.
    #[arg(long)]
    pub keep_artifacts: bool,
    /// `key=value` parameter; repeatable.
    #[arg(long = "param", value_parser = parse_param)]
    pub params: Vec<(String, String)>,
    /// A built game executable to run instead of the session's build.
    #[arg(long)]
    pub exe: Option<PathBuf>,
    /// Folder for session artifacts (default: `artifacts` next to the session file).
    #[arg(long)]
    pub artifacts: Option<PathBuf>,
}

fn parse_param(text: &str) -> Result<(String, String), String> {
    text.split_once('=')
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| format!("`{text}` is not key=value"))
}

/// Runs the command and returns the process exit code.
pub fn run(args: &TestArgs) -> ExitCode {
    let result = match &args.session {
        Some(TestSubcommand::Session(s)) => run_session(s),
        None => run_level(args),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(EXIT_HARNESS as u8)
        }
    }
}

fn run_session(args: &SessionArgs) -> Result<u8> {
    let session = Session::load(&args.file)?;
    let base = args
        .file
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    let name = args
        .file
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or("session", |s| s.strip_suffix(".session").unwrap_or(s))
        .to_owned();
    let mut opts = LaunchOptions::new(
        args.artifacts.clone().unwrap_or_else(|| base.join("artifacts")),
        &name,
    );
    opts.base_dir = base;
    opts.params = args.params.iter().cloned().collect::<BTreeMap<_, _>>();
    opts.recover = args.recover;
    opts.keep_artifacts = args.keep_artifacts;
    opts.exe_override = args.exe.clone();
    let outcome = session::launch(&session, &opts)?;
    println!("session {name}: {:?}", outcome.status);
    for reason in &outcome.reasons {
        println!("  {reason}");
    }
    println!("artifacts: {}", outcome.artifacts.display());
    Ok(outcome.exit_code() as u8)
}

/// What `--platform android` refuses, or `None`: the device game is already running, so nothing
/// can be built, given a level or soaked until it exits.
fn android_conflict(args: &TestArgs) -> Option<&'static str> {
    if args.level.is_some() {
        Some(
            "--level does not apply: the device runs the level it was started with (package it as the start level)",
        )
    } else if args.exe.is_some() {
        Some("--exe does not apply: the game is already running on the device")
    } else if args.looped {
        Some("--loop needs a process the harness owns; on a device run the tests once")
    } else if !session::remote_address_ok(&args.address) {
        Some("--address must be `attached` or 127.0.0.1:<port>")
    } else {
        None
    }
}

/// `regia test --platform android`: the tests of the game running on the device.
fn run_device(args: &TestArgs) -> Result<u8> {
    if let Some(conflict) = android_conflict(args) {
        bail!("--platform android: {conflict}");
    }
    let project = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    let (endpoint, token) = session::remote_endpoint(&args.address, &project)?;
    let instance = Instance {
        role: "device".into(),
        index: 0,
    };
    let poll = Poll::default();
    let connector = ws_connector();
    let mut link = connect_ready(&connector, &instance, &endpoint, &token, poll).map_err(|e| anyhow!(e))?;
    let report = drive_link(link.as_mut(), args, poll, &mut || false, None)?;
    write_reports(&report, args)?;
    print_summary(&report);
    Ok(report.exit_code() as u8)
}

fn write_reports(report: &TestReport, args: &TestArgs) -> Result<()> {
    if let Some(path) = &args.report {
        std::fs::write(path, report.to_json()?).with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(path) = &args.junit {
        std::fs::write(path, report.to_junit()).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// A per-run artifacts folder name: the pid plus a process-wide counter, so consecutive runs in
/// one process (and a crashed earlier run under a reused pid) never share a `test-*` folder and
/// a fresh run never inherits stale logs (sprint-deep-zai-cli-tools finding 5).
fn run_artifacts_name() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("test-{}-{n}", std::process::id())
}

fn run_level(args: &TestArgs) -> Result<u8> {
    if args.platform == TestPlatform::Android {
        return run_device(args);
    }
    let project = args
        .project
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.project.display()))?;
    if !regia_core::brand::is_project_root(&project) {
        bail!(
            "{} is not a Regia Engine project (no Regia.toml)",
            project.display()
        );
    }
    let level = args
        .level
        .as_deref()
        .ok_or_else(|| anyhow!("--level is required (the level whose tests to run)"))
        .and_then(|l| crate::commands::run::resolve_level(&project, l))?;
    let exe = match &args.exe {
        Some(exe) => exe.clone(),
        None => {
            let package = crate::commands::run::launcher_package(&project)?;
            build_launcher(&project, &package)?
        }
    };
    let artifacts = args
        .artifacts
        .clone()
        .unwrap_or_else(|| regia_core::brand::project_state_dir(&project).join("test-artifacts"));
    let dir = artifacts.join(run_artifacts_name()).join("game-0");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let instance = Instance {
        role: "game".into(),
        index: 0,
    };
    let env = vec![
        ("REGIA_START_LEVEL".to_owned(), level.display().to_string()),
        ("REGIA_PROJECT_DIR".to_owned(), project.display().to_string()),
    ];
    let mut process = spawn_instance(&SpawnSpec {
        exe: &exe,
        args: &[],
        env: &env,
        instance: &instance,
        dir: &dir,
        driver: None,
        render: false,
    })?;
    let poll = Poll::default();
    // A level outside the project has no project-relative form: the game runs the one it started
    // on (`REGIA_START_LEVEL`) and `functest.run` names none.
    let relative = project_relative_level(&project, &level);
    let driven = drive(&mut process, &instance, args, poll, relative.as_deref());
    let exit = process.stop(poll);
    let report = match driven {
        Ok(report) => report,
        Err(e) => {
            eprintln!("{}", session::log_tail(&dir, 30));
            return Err(e);
        }
    };
    write_reports(&report, args)?;
    print_summary(&report);
    if exit.is_some_and(|c| c != 0) && !process_stopped_ok(exit) {
        eprintln!("note: the game exited with {exit:?}; logs in {}", dir.display());
    }
    Ok(report.exit_code() as u8)
}

fn process_stopped_ok(exit: Option<i32>) -> bool {
    exit == Some(0) || exit.is_none()
}

fn build_launcher(project: &Path, package: &str) -> Result<PathBuf> {
    let output = std::process::Command::new("cargo")
        .args(["build", "--manifest-path"])
        .arg(project.join("Cargo.toml"))
        .args(["-p", package, "--message-format=json-render-diagnostics"])
        .current_dir(project)
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running cargo build")?;
    if !output.status.success() {
        bail!("cargo build -p {package} failed ({})", output.status);
    }
    session::executable_from_cargo_json(&String::from_utf8_lossy(&output.stdout), package)
        .ok_or_else(|| anyhow!("cargo built no executable for `{package}`"))
}

fn drive(
    process: &mut session::Process,
    instance: &Instance,
    args: &TestArgs,
    poll: Poll,
    level: Option<&str>,
) -> Result<TestReport> {
    let (endpoint, token) = wait_ready(process, poll)?;
    let connector = ws_connector();
    let mut link = connect_ready(&connector, instance, &endpoint, &token, poll).map_err(|e| anyhow!(e))?;
    drive_link(
        link.as_mut(),
        args,
        poll,
        &mut || process.exited().is_some(),
        level,
    )
}

/// Project-relative `level` with `/` separators, the form `functest.run` accepts (ADR-1816);
/// `None` for a level outside the project (its path would otherwise be read as a project path).
/// `.` and `..` are folded lexically: `functest.run` validates the level with
/// `regia_core::project_path::check_relative`, which rejects a dots-only segment, so emitting
/// one would fail the run with `bad_params`.
fn project_relative_level(project: &Path, level: &Path) -> Option<String> {
    let rel = level.strip_prefix(project).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop()?; // A `..` past the project root is not project-relative.
            }
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Runs the selected tests over an agent link and returns the report. `exited` tells a soak
/// (`--loop`) when the game is gone. `level` is the project-relative level (`None` on a device,
/// which already loaded its own).
fn drive_link(
    link: &mut dyn session::AgentLink,
    args: &TestArgs,
    poll: Poll,
    exited: &mut dyn FnMut() -> bool,
    level: Option<&str>,
) -> Result<TestReport> {
    let params: serde_json::Map<String, Value> =
        args.params.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let mut call = json!({
        "filter": args.filter, "tags": args.tag, "seed": args.seed, "params": params,
        "update_expected": args.update_expected, "looped": args.looped,
    });
    if let Some(level) = level {
        call["level"] = json!(level);
    }
    let started = link.call("functest.run", call);
    if let Err(e) = started {
        // The client renders an RPC error as `message (code): data`, with `data` a JSON object
        // whose `reason` field carries the structured refusal (F0002, the functional-tests page
        // of the agent guide). Match the delimited reason, never a bare substring: another
        // refusal whose message merely mentions `no_tests` (a level path echoed by
        // `golden_path_denied`) is a real failure and must surface as one, not as "0 tests ran".
        if e.contains("\"reason\":\"no_tests\"") {
            return Ok(TestReport::new("", args.seed));
        }
        return Err(anyhow!("functest.run: {e}"));
    }
    if args.looped {
        // Soak: run until the game exits or the user interrupts.
        while !exited() {
            poll.pause_public();
        }
    } else {
        // The single-shot status wait has its own budget, `--timeout-s` (default 60 s), not the
        // ready wait's: a healthy suite legitimately outlives the ready wait on a loaded host or
        // a first launch (pipeline compiles), and stopping it there misread a slow pass as a
        // broken harness (sprint-deep-zai-cli-tools finding 2).
        let status_poll = poll.with_budget(args.timeout_s);
        let mut finished = false;
        for _ in 0..status_poll.max_attempts {
            let status = link
                .call("functest.status", json!({}))
                .map_err(|e| anyhow!("functest.status: {e}"))?;
            if status["finished"] == true {
                finished = true;
                break;
            }
            poll.pause_public();
        }
        if !finished {
            bail!(
                "the tests did not finish within {} s of status polling (raise --timeout-s)",
                args.timeout_s
            );
        }
    }
    let results = link
        .call("functest.results", json!({}))
        .map_err(|e| anyhow!("functest.results: {e}"))?;
    TestReport::from_json(&results.to_string()).map_err(|e| anyhow!("report: {e}"))
}

fn print_summary(report: &TestReport) {
    for line in summary_lines(report) {
        println!("{line}");
    }
}

/// The lines `regia test` prints: one per test (verdict, name, message), one per step that did not
/// pass (so a failure names its step, spec 41 § 8 test 13), then the counts.
fn summary_lines(report: &TestReport) -> Vec<String> {
    let mut lines = Vec::new();
    for t in &report.tests {
        let verdict = if t.skipped {
            "Skipped".to_owned()
        } else {
            format!("{:?}", t.verdict)
        };
        lines.push(format!(
            "{verdict:<10} {}{}",
            t.name,
            if t.message.is_empty() {
                String::new()
            } else {
                format!(": {}", t.message)
            }
        ));
        for step in t
            .steps
            .iter()
            .filter(|s| s.verdict != regia_framework::functional::Verdict::Passed)
        {
            lines.push(format!("             step `{}` {:?}", step.name, step.verdict));
        }
    }
    let s = report.summary();
    lines.push(format!(
        "{} passed, {} failed, {} aborted, {} undecided, {} skipped",
        s.passed, s.failed, s.aborted, s.undecided, s.skipped
    ));
    lines
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        test: TestArgs,
    }

    fn args(argv: &[&str]) -> TestArgs {
        let mut full = vec!["regia-test"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full).expect("parses").test
    }

    /// A device agent whose tests finish on the second status call.
    struct DeviceLink {
        statuses: u32,
        calls: Vec<String>,
        results: Value,
        run_params: Value,
    }

    impl session::AgentLink for DeviceLink {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            if method == "functest.run" {
                self.run_params = params;
            }
            self.calls.push(method.to_owned());
            match method {
                "functest.run" => Ok(json!({ "started": 1 })),
                "functest.status" => {
                    self.statuses += 1;
                    Ok(json!({ "finished": self.statuses > 1 }))
                }
                "functest.results" => Ok(self.results.clone()),
                other => Err(format!("unexpected {other}")),
            }
        }
    }

    #[test]
    fn platform_android_parses_with_an_address_and_defaults_to_desktop() {
        let a = args(&[
            "--platform",
            "android",
            "--address",
            "127.0.0.1:5555",
            "--filter",
            "smoke*",
        ]);
        assert_eq!(a.platform, TestPlatform::Android);
        assert_eq!(a.address, "127.0.0.1:5555");
        assert_eq!(android_conflict(&a), None);
        let d = args(&[]);
        assert_eq!(d.platform, TestPlatform::Desktop);
        assert_eq!(d.address, session::REMOTE_ATTACHED);
        assert!(Cli::try_parse_from(["regia-test", "--platform", "ios"]).is_err());
    }

    #[test]
    fn platform_android_refuses_what_needs_a_local_process() {
        for (argv, why) in [
            (
                &["--platform", "android", "--level", "levels/a.level.ron"][..],
                "--level",
            ),
            (&["--platform", "android", "--exe", "game"][..], "--exe"),
            (&["--platform", "android", "--loop"][..], "--loop"),
            (
                &["--platform", "android", "--address", "192.168.1.5:5555"][..],
                "--address",
            ),
        ] {
            let conflict = android_conflict(&args(argv)).expect("refused");
            assert!(conflict.starts_with(why), "{argv:?}: {conflict}");
        }
    }

    #[test]
    fn platform_android_without_attached_agent_files_says_how_to_attach() {
        let project = regia_test_utils::unique_temp_dir("regia_test_android_none");
        let mut a = args(&["--platform", "android"]);
        a.project = project.clone();
        let err = run_level(&a).expect_err("no agent files").to_string();
        assert!(err.contains("install-android --attach"), "{err}");
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn a_device_link_runs_the_tests_and_returns_the_report() {
        let report = TestReport::new("device", 7);
        let mut link = DeviceLink {
            statuses: 0,
            calls: Vec::new(),
            results: serde_json::from_str(&report.to_json().expect("json")).expect("value"),
            run_params: Value::Null,
        };
        let a = args(&["--platform", "android", "--seed", "7"]);
        let poll = Poll {
            interval: std::time::Duration::ZERO,
            max_attempts: 10,
        };
        let out = drive_link(&mut link, &a, poll, &mut || false, None).expect("report");
        assert_eq!(out, report);
        assert_eq!(out.exit_code(), 3, "an empty report means no tests ran");
        assert_eq!(
            link.calls,
            [
                "functest.run",
                "functest.status",
                "functest.status",
                "functest.results"
            ]
        );
        assert!(link.run_params.get("level").is_none());
    }

    #[test]
    fn project_relative_level_uses_forward_slashes() {
        let project = Path::new("/work/game");
        let level = Path::new("/work/game/levels/a.level.ron");
        assert_eq!(
            project_relative_level(project, level).as_deref(),
            Some("levels/a.level.ron")
        );
    }

    /// A level outside the project is not turned into a made-up project path (`/tmp/x.level.ron`
    /// was sent as `tmp/x.level.ron`, and the runner read and wrote `<project>/tmp/...`).
    #[test]
    fn audit_a_level_outside_the_project_sends_no_level() {
        let project = Path::new("/work/game");
        assert_eq!(
            project_relative_level(project, Path::new("/tmp/x.level.ron")),
            None
        );
        assert_eq!(project_relative_level(project, project), None);
    }

    /// `.` and `..` must be folded, not emitted: `functest.run` validates the level with
    /// `check_relative`, which rejects a dots-only segment (`ProjectPathError::ParentDir`), so a
    /// literal `..` fails the run with `bad_params` even though it names a real file.
    #[test]
    fn audfix2_project_relative_level_folds_dot_components() {
        let project = Path::new("/work/game");
        let level = Path::new("/work/game/levels/../levels/a.level.ron");
        let folded = project_relative_level(project, level);
        assert_eq!(folded.as_deref(), Some("levels/a.level.ron"));
        assert!(
            regia_core::project_path::check_relative(folded.as_deref().expect("a folded level")).is_ok(),
            "functest.run rejects the emitted level: {folded:?}"
        );
        // `.` segments fold away the same way.
        assert_eq!(
            project_relative_level(project, Path::new("/work/game/./levels/a.level.ron")).as_deref(),
            Some("levels/a.level.ron")
        );
        // A `..` that escapes the project is not project-relative at all.
        assert_eq!(
            project_relative_level(project, Path::new("/work/game/../outside/a.level.ron")).as_deref(),
            None
        );
    }

    #[test]
    fn drive_link_sends_the_project_relative_level() {
        let report = TestReport::new("level", 1);
        let mut link = DeviceLink {
            statuses: 0,
            calls: Vec::new(),
            results: serde_json::from_str(&report.to_json().expect("json")).expect("value"),
            run_params: Value::Null,
        };
        let a = args(&["--level", "levels/a.level.ron", "--seed", "1"]);
        let poll = Poll {
            interval: std::time::Duration::ZERO,
            max_attempts: 4,
        };
        drive_link(&mut link, &a, poll, &mut || false, Some("levels/a.level.ron")).expect("report");
        assert_eq!(link.run_params["level"], "levels/a.level.ron");
    }

    /// A fast poll for the drive-link tests: yield instead of sleeping, a small attempt bound.
    fn quick_poll() -> Poll {
        Poll {
            interval: std::time::Duration::ZERO,
            max_attempts: 10,
        }
    }

    /// The single-shot run's artifacts folder is unique per call, not per pid
    /// (sprint-deep-zai-cli-tools finding 5): a crashed earlier run plus pid reuse made
    /// `create_dir_all` inherit stale logs under the pid-only name.
    #[test]
    fn run_artifacts_names_are_unique_per_call() {
        let a = run_artifacts_name();
        let b = run_artifacts_name();
        assert_ne!(a, b, "two runs in one process get distinct folders");
        for name in [&a, &b] {
            assert!(name.starts_with("test-"), "{name}");
            assert!(
                std::path::Path::new(name)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
                "a single relative component: {name}"
            );
        }
    }

    /// `--timeout-s` extends the single-shot status budget beyond the shared 60 s poll
    /// (sprint-deep-zai-cli-tools finding 2): a suite that legitimately runs longer (loaded
    /// shared host, first-launch pipeline compiles, many ready-gated tests) is not stopped and
    /// classified as a harness failure. The default is unchanged; a soak (`--loop`) ignores it.
    #[test]
    fn timeout_s_extends_the_single_shot_status_budget() {
        assert_eq!(args(&[]).timeout_s, 60, "the default budget is unchanged");
        assert_eq!(args(&["--timeout-s", "300"]).timeout_s, 300);
        assert_eq!(
            args(&["--platform", "android", "--timeout-s", "120"]).timeout_s,
            120
        );
        assert!(
            Cli::try_parse_from(["regia-test", "--timeout-s", "0"]).is_err(),
            "zero seconds is not a budget"
        );

        // A run that never finishes fails after the budget, with the flag named in the error.
        let err = drive_link(
            &mut NeverFinishes,
            &args(&["--timeout-s", "1"]),
            quick_poll(),
            &mut || false,
            None,
        )
        .expect_err("the tests never finish");
        let text = format!("{err:#}");
        assert!(text.contains("1 s"), "{text}");
        assert!(text.contains("--timeout-s"), "{text}");
    }

    /// A link whose `functest.run` fails with the given rendered error.
    struct FailingRun(String);

    impl session::AgentLink for FailingRun {
        fn call(&mut self, method: &str, _params: Value) -> Result<Value, String> {
            Err(if method == "functest.run" {
                self.0.clone()
            } else {
                format!("unexpected {method}")
            })
        }
    }

    /// A link whose tests never finish: every `functest.status` says `finished: false`.
    struct NeverFinishes;

    impl session::AgentLink for NeverFinishes {
        fn call(&mut self, method: &str, _params: Value) -> Result<Value, String> {
            match method {
                "functest.run" => Ok(json!({ "started": 1 })),
                "functest.status" => Ok(json!({ "finished": false })),
                other => Err(format!("unexpected {other}")),
            }
        }
    }

    /// `no_tests` is recognized by its structured reason, not by a substring of the whole
    /// rendered error (sprint-deep-zai-cli-tools finding 3). The genuine refusal still yields
    /// the empty report (exit 3), but another refusal whose message happens to contain
    /// `no_tests` — here `golden_path_denied` echoing a level path, rendered by the client
    /// exactly as `WsLink` renders it — is a real failure and must surface as one.
    #[test]
    fn only_a_no_tests_reason_yields_the_empty_report() {
        let mut error = regia_agent::protocol::RpcError::new(
            regia_agent::protocol::INVALID_PARAMS,
            "F0002",
            "no enabled functional test matches the selection",
            "see book/agents/functional-tests.md for the reasons and the recipe",
        );
        if let Some(data) = error.data.as_mut().and_then(Value::as_object_mut) {
            data.insert("reason".into(), json!("no_tests"));
        }
        let mut genuine = FailingRun(regia_agent::client::ClientError::Rpc(error).to_string());
        let report = drive_link(&mut genuine, &args(&[]), quick_poll(), &mut || false, None)
            .expect("the genuine no_tests refusal is the empty report");
        assert_eq!(report.exit_code(), 3, "no tests ran");
        assert!(report.tests.is_empty());

        let denied = regia_agent::golden_tools::golden_path_denied("goldens/no_tests.level.ron");
        let rendered = regia_agent::client::ClientError::Rpc(denied).to_string();
        assert!(
            rendered.contains("no_tests") && !rendered.contains("\"reason\":\"no_tests\""),
            "the fixture must hit the bare-substring hazard: {rendered}"
        );
        let mut echo = FailingRun(rendered);
        let err = drive_link(&mut echo, &args(&[]), quick_poll(), &mut || false, None)
            .expect_err("a refusal that only mentions no_tests is a real failure");
        let text = format!("{err:#}");
        assert!(text.contains("golden_path_denied"), "{text}");
        assert!(text.contains("no_tests.level.ron"), "{text}");
    }

    mod in_process {
        use regia_app::prelude::*;
        use regia_framework::functional::{
            FunctionalAppExt, FunctionalTest, FunctionalTestPlugin, RunOptions, Runner, RunnerStatus,
            Selection, TestBody, TestBodyRef, TestContext, Verdict,
        };
        use regia_scene::components::Name;

        use super::*;

        #[derive(Default)]
        struct Opens;
        impl TestBody for Opens {
            fn begin(&mut self, cx: &mut TestContext) {
                cx.conclude(Verdict::Passed, "");
            }
        }

        #[derive(Default)]
        struct StaysShut;
        impl TestBody for StaysShut {
            fn begin(&mut self, cx: &mut TestContext) {
                cx.begin_step("door opens");
                cx.equal(&"Closed", &"Open", "door state");
                cx.end_step();
                cx.conclude(Verdict::Passed, "");
            }
        }

        /// The game side of `functest.run` / `.status` / `.results` over a real `Runner`, one frame per
        /// status call, so `drive_link` sees what a game process would answer.
        struct GameLink {
            app: App,
        }

        impl session::AgentLink for GameLink {
            fn call(&mut self, method: &str, params: Value) -> std::result::Result<Value, String> {
                let strings = |key: &str| -> Vec<String> {
                    params[key]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                        .unwrap_or_default()
                };
                match method {
                    "functest.run" => {
                        let selection = Selection {
                            name_globs: strings("filter"),
                            tags: strings("tags"),
                        };
                        let opts = RunOptions {
                            seed: params["seed"].as_u64().unwrap_or(0),
                            ..RunOptions::default()
                        };
                        self.app
                            .world_mut()
                            .resource_mut::<Runner>()
                            .start(selection, opts);
                        Ok(json!({ "started": 1 }))
                    }
                    "functest.status" => {
                        self.app.update();
                        let done = self.app.world().resource::<Runner>().status() == RunnerStatus::Done;
                        Ok(json!({ "finished": done }))
                    }
                    "functest.results" => {
                        serde_json::to_value(self.app.world().resource::<Runner>().report())
                            .map_err(|e| e.to_string())
                    }
                    other => Err(format!("unexpected {other}")),
                }
            }
        }

        fn game() -> GameLink {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, FunctionalTestPlugin));
            app.register_functional_test::<Opens>()
                .register_functional_test::<StaysShut>();
            for (name, body) in [
                ("a_door_opens", std::any::type_name::<Opens>()),
                ("b_door_opens_again", std::any::type_name::<Opens>()),
                ("c_door_jammed", std::any::type_name::<StaysShut>()),
            ] {
                app.world_mut().spawn((
                    Name::new(name),
                    FunctionalTest {
                        body: TestBodyRef::Rust(body.to_owned()),
                        ..FunctionalTest::default()
                    },
                ));
            }
            GameLink { app }
        }

        /// Spec 41 § 8 test 13 (in process; the spawned-game run is the lead's): two passing tests and one
        /// deliberately failing test exit 1 and the printed summary names the failing step; selecting only
        /// the passing tests exits 0.
        #[test]
        fn two_passing_and_one_failing_test_exit_one_naming_the_step() {
            let poll = Poll {
                interval: std::time::Duration::ZERO,
                max_attempts: 200,
            };
            let a = args(&["--level", "levels/doors.level.ron"]);
            let mut link = game();
            let report = drive_link(&mut link, &a, poll, &mut || false, Some("levels/doors.level.ron"))
                .expect("report");
            assert_eq!(report.exit_code(), 1);
            let lines = summary_lines(&report);
            assert!(
                lines
                    .iter()
                    .any(|l| l.starts_with("Passed") && l.contains("a_door_opens")),
                "{lines:?}"
            );
            let failed = lines
                .iter()
                .find(|l| l.starts_with("Failed") && l.contains("c_door_jammed"))
                .unwrap_or_else(|| panic!("{lines:?}"));
            assert!(failed.contains("[door opens] door state"), "{failed}");
            assert!(
                lines.iter().any(|l| l.trim() == "step `door opens` Failed"),
                "{lines:?}"
            );
            assert_eq!(
                lines.last().map(String::as_str),
                Some("2 passed, 1 failed, 0 aborted, 0 undecided, 0 skipped")
            );

            let passing = args(&["--level", "levels/doors.level.ron", "--filter", "*opens*"]);
            let mut link = game();
            let report = drive_link(&mut link, &passing, poll, &mut || false, None).expect("report");
            assert_eq!(report.exit_code(), 0, "{:?}", summary_lines(&report));
            assert_eq!(report.summary().passed, 2);
        }
    }
}
