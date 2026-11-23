// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask test-costs`: what each test binary costs the test build and the test run (ADR-2986).
//!
//! Every `tests/*.rs` file is its own binary that links the whole dependency graph. This command measures,
//! per test binary: its size on disk (the executable plus its split debug-info objects), its link time, the
//! build time of its unit, its test count, and its run time; and per test its run time. It writes
//! `test-costs.csv`, `slowest-tests.csv`, `test-costs.md` (sorted by cost) and the test name lists under
//! `--out` (default `target/test-costs/report`).
//!
//! How each number is taken:
//!
//! - **Link time.** The build runs with a linker shim (`<target-dir>/test-costs/link-shim.sh`) that runs the
//!   real linker (`cc`, which uses `rust-lld` on rustup toolchains) and appends the start, end, exit code and
//!   output path of every link to a log. The linker path is part of cargo's fingerprint, so the build uses
//!   its own target directory (default `target/test-costs/target`): the shim never invalidates the
//!   developer's cache. Unix only; on Windows the link column is empty. With `REGIA_TEST_COSTS_ARGS_DIR` set the
//!   shim also keeps each link's arguments (`<dir>/<binary>.args`), so a link can be replayed with other linker
//!   options on the same inputs.
//! - **Unit time** is cargo's own `--timings` duration of the unit (compile plus link).
//! - **Disk** is the executable plus the `<name>-<hash>.*.dwo` objects `-Csplit-debuginfo=unpacked` leaves
//!   next to it (`.cargo/config.toml`).
//! - **Tests** are the `: test` lines of `<binary> --list`; ignored tests are counted apart.
//! - **Run time** is the wall time of `<binary> --test-threads N` from the package directory, like `cargo
//!   test`; per-test times come from libtest's `--report-time` (stable binaries accept it with
//!   `RUSTC_BOOTSTRAP=1` and `-Z unstable-options`; a binary that refuses runs again without it).
//!
//! Nothing here asserts a time. The host is usually shared: the report records the load average at the
//! start and end of each phase, and numbers compare only within one report.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;

/// Arguments of `cargo xtask test-costs`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Packages to measure (comma separated). Default: the whole workspace.
    #[arg(long, value_delimiter = ',')]
    pub packages: Vec<String>,
    /// Report directory (default `target/test-costs/report`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Cargo target directory for the measured build (default `target/test-costs/target`). The linker
    /// shim changes cargo's fingerprint, so reusing the everyday target directory rebuilds it.
    #[arg(long)]
    pub target_dir: Option<PathBuf>,
    /// Do not build: read an earlier run's `build.json` (and `link.log`, `cargo-timing.html`) from `--out`.
    #[arg(long)]
    pub no_build: bool,
    /// Measure the build and the test lists only; run no test.
    #[arg(long)]
    pub no_run: bool,
    /// Test binaries run at once (each with `--test-threads`).
    #[arg(long, default_value_t = 1)]
    pub jobs: usize,
    /// `--test-threads` of every test binary (the gate's runners use 4 on the Mac and 12 on the build server).
    #[arg(long, default_value_t = 4)]
    pub test_threads: usize,
    /// Per-binary time limit in seconds; a binary past it is killed and reported as timed out.
    #[arg(long, default_value_t = 1500)]
    pub timeout: u64,
    /// Only binaries whose target name contains one of these (comma separated).
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,
    /// Rows in each top list of the Markdown report.
    #[arg(long, default_value_t = 30)]
    pub top: usize,
}

/// One test executable from cargo's `compiler-artifact` messages.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    /// Package name.
    pub package: String,
    /// Target name (`suite`, `formats`, or the package's lib name for unit tests).
    pub target: String,
    /// Target kind (`test`, `lib`, `bin`, ...), `+`-joined.
    pub kind: String,
    /// Path of the executable.
    pub executable: PathBuf,
    /// The package's `Cargo.toml`.
    pub manifest: PathBuf,
}

/// One unit of cargo's `--timings` report.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    /// Package name.
    pub name: String,
    /// Cargo's description of the target (` (test "suite")`, ` lib (test)`, ...).
    pub target: String,
    /// Cargo's compile mode (`test`, `build`, `run-custom-build`, ...).
    pub mode: String,
    /// Seconds from the start of the build.
    pub start: f64,
    /// Seconds the unit took.
    pub duration: f64,
}

/// One link recorded by the shim.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// The `-o` path.
    pub output: String,
    /// Seconds the linker ran.
    pub seconds: f64,
    /// Start, nanoseconds since the epoch.
    pub start_ns: u128,
    /// Linker exit code.
    pub code: i32,
}

/// The outcome of one test, from `--report-time` output.
#[derive(Debug, Clone, PartialEq)]
pub struct TestTime {
    /// Full test name.
    pub name: String,
    /// `ok`, `FAILED`, `ignored`.
    pub status: String,
    /// Seconds, when reported.
    pub seconds: Option<f64>,
}

/// Counts from libtest's `test result:` lines (summed when a binary prints several).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Counts {
    /// Passed tests.
    pub passed: u64,
    /// Failed tests.
    pub failed: u64,
    /// Ignored tests.
    pub ignored: u64,
}

/// Everything measured for one test binary.
#[derive(Debug, Clone, Default)]
pub struct Row {
    /// Package name.
    pub package: String,
    /// Target name.
    pub target: String,
    /// Target kind.
    pub kind: String,
    /// Tests listed (ignored included).
    pub tests: usize,
    /// Ignored tests listed.
    pub ignored: usize,
    /// Executable bytes.
    pub bytes: u64,
    /// Split debug-info bytes next to the executable.
    pub dwo_bytes: u64,
    /// Link seconds, when the shim saw the link.
    pub link_s: Option<f64>,
    /// Cargo's unit seconds (compile + link), when `--timings` has the unit.
    pub unit_s: Option<f64>,
    /// Run seconds, when run.
    pub run_s: Option<f64>,
    /// Exit code of the run (`None`: not run or killed).
    pub exit: Option<i32>,
    /// Killed at `--timeout`.
    pub timed_out: bool,
    /// Counts from the run's `test result:` lines.
    pub counts: Counts,
}

impl Row {
    /// Disk bytes: executable plus split debug info.
    pub fn disk(&self) -> u64 {
        self.bytes + self.dwo_bytes
    }

    /// The sort key of the report: link plus run seconds.
    pub fn cost(&self) -> f64 {
        self.link_s.unwrap_or(0.0) + self.run_s.unwrap_or(0.0)
    }
}

/// The package name in a cargo package id, both the `name version (source)` form and the
/// `path+file:///dir#name@version` / `path+file:///dir/name#version` forms.
pub fn package_name(id: &str) -> String {
    if let Some((left, right)) = id.split_once('#') {
        if let Some((name, _)) = right.split_once('@') {
            return name.to_string();
        }
        return left.rsplit('/').next().unwrap_or(left).to_string();
    }
    id.split_whitespace().next().unwrap_or(id).to_string()
}

/// The test executables in cargo's `--message-format=json` output (other lines are ignored).
pub fn parse_build_json(text: &str) -> Vec<Artifact> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-artifact" || msg["profile"]["test"] != true {
            continue;
        }
        let Some(exe) = msg["executable"].as_str() else {
            continue;
        };
        let kind = msg["target"]["kind"]
            .as_array()
            .map(|k| k.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join("+"))
            .unwrap_or_default();
        out.push(Artifact {
            package: package_name(msg["package_id"].as_str().unwrap_or("?")),
            target: msg["target"]["name"].as_str().unwrap_or("?").to_string(),
            kind,
            executable: PathBuf::from(exe),
            manifest: PathBuf::from(msg["manifest_path"].as_str().unwrap_or("")),
        });
    }
    out
}

/// The shim's log: `start_ns end_ns code output` per line, tab separated.
pub fn parse_link_log(text: &str) -> Vec<Link> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, '\t');
            let start: u128 = f.next()?.trim().parse().ok()?;
            let end: u128 = f.next()?.trim().parse().ok()?;
            let code: i32 = f.next()?.trim().parse().ok()?;
            let output = f.next()?.trim().to_string();
            let seconds = end.saturating_sub(start) as f64 / 1e9;
            Some(Link {
                output,
                seconds,
                start_ns: start,
                code,
            })
        })
        .collect()
}

/// The units of cargo's `cargo-timing.html` (`const UNIT_DATA = [...];`).
pub fn parse_unit_data(html: &str) -> Result<Vec<Unit>> {
    let start = html
        .find("UNIT_DATA = ")
        .context("no UNIT_DATA in the timings page")?
        + "UNIT_DATA = ".len();
    let rest = &html[start..];
    let end = rest.find("];").context("UNIT_DATA is not terminated")? + 1;
    let units: Vec<serde_json::Value> = serde_json::from_str(&rest[..end]).context("parsing UNIT_DATA")?;
    Ok(units
        .iter()
        .map(|u| Unit {
            name: u["name"].as_str().unwrap_or("").to_string(),
            target: u["target"].as_str().unwrap_or("").to_string(),
            mode: u["mode"].as_str().unwrap_or("").to_string(),
            start: u["start"].as_f64().unwrap_or(0.0),
            duration: u["duration"].as_f64().unwrap_or(0.0),
        })
        .collect())
}

/// Whether a `--timings` unit builds a test binary. Cargo 1.9x writes `mode: "todo"` and puts `(test)` at
/// the end of the target description; older cargo wrote `mode: "test"`.
pub fn is_test_unit(u: &Unit) -> bool {
    u.mode == "test" || u.target.trim_end().ends_with("(test)")
}

/// The target name of a test unit's description, `None` for a library's unit tests. Cargo 1.9x writes
/// ` suite "test" (test)`, ` regia_ui "lib" (test)`, ` regia "bin" (test)`; older cargo wrote ` (test "suite")`,
/// ` lib (test)` and ` bin "regia" (test)`.
pub fn unit_target(target: &str) -> Option<String> {
    const KINDS: [&str; 7] = [
        "lib",
        "bin",
        "test",
        "bench",
        "example",
        "proc-macro",
        "custom-build",
    ];
    let t = target.trim();
    if t.starts_with("(test") {
        return t.split('"').nth(1).map(str::to_string);
    }
    let (first, rest) = t.split_once(' ').unwrap_or((t, ""));
    if first == "lib" {
        return None;
    }
    let quoted = rest.split('"').nth(1);
    match quoted {
        Some("lib") => None,
        Some(kind) if KINDS.contains(&kind) => Some(first.to_string()),
        Some(name) => Some(name.to_string()),
        None => None,
    }
}

/// The test names in `--list` output (both `name: test` and `name: bench` lines).
pub fn parse_list(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_suffix(": test").or_else(|| l.strip_suffix(": bench")))
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// Per-test outcomes in libtest's pretty output; `--report-time` appends `<1.234s>`.
pub fn parse_test_lines(text: &str) -> Vec<TestTime> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, outcome)) = rest.rsplit_once(" ... ") else {
            continue;
        };
        let mut words = outcome.split_whitespace();
        let Some(status) = words.next() else {
            continue;
        };
        // `#[ignore = "reason"]` prints `ignored, reason`.
        let status = status.trim_end_matches(',');
        if !matches!(status, "ok" | "FAILED" | "ignored") {
            continue;
        }
        let seconds = outcome
            .rsplit_once('<')
            .and_then(|(_, t)| t.strip_suffix("s>"))
            .and_then(|t| t.parse().ok());
        out.push(TestTime {
            name: name.to_string(),
            status: status.to_string(),
            seconds,
        });
    }
    out
}

/// Sums every `test result:` line of a run.
pub fn parse_counts(text: &str) -> Counts {
    let mut c = Counts::default();
    for line in text.lines().filter(|l| l.starts_with("test result:")) {
        let words: Vec<&str> = line.split_whitespace().collect();
        for pair in words.windows(2) {
            let n: u64 = pair[0].parse().unwrap_or(0);
            match pair[1].trim_end_matches([';', ',']) {
                "passed" => c.passed += n,
                "failed" => c.failed += n,
                "ignored" => c.ignored += n,
                _ => {}
            }
        }
    }
    c
}

fn load() -> String {
    fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "n/a".into())
}

/// Writes the linker shim (Unix) and returns its path.
fn write_shim(target_dir: &Path) -> Result<Option<PathBuf>> {
    if cfg!(windows) {
        return Ok(None);
    }
    let dir = target_dir.join("test-costs");
    fs::create_dir_all(&dir)?;
    let path = dir.join("link-shim.sh");
    let text = include_str!("test_costs_link_shim.sh");
    if fs::read_to_string(&path).ok().as_deref() != Some(text) {
        let tmp = dir.join("link-shim.sh.tmp");
        fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        }
        fs::rename(&tmp, &path)?;
    }
    Ok(Some(path))
}

/// The linker variable for the host target, e.g. `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER`.
fn host_linker_var() -> Option<String> {
    let out = Command::new("rustc").arg("-vV").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let host = text
        .lines()
        .find_map(|l| l.strip_prefix("host: "))?
        .trim()
        .to_string();
    Some(format!(
        "CARGO_TARGET_{}_LINKER",
        host.to_uppercase().replace(['-', '.'], "_")
    ))
}

/// Size of the executable and of the `.dwo` objects that belong to it.
fn disk_of(exe: &Path) -> (u64, u64) {
    let bytes = fs::metadata(exe).map(|m| m.len()).unwrap_or(0);
    let Some(stem) = exe.file_name().and_then(|n| n.to_str()) else {
        return (bytes, 0);
    };
    let prefix = format!("{stem}.");
    let dwo = exe
        .parent()
        .and_then(|d| fs::read_dir(d).ok())
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.starts_with(&prefix) && n.ends_with(".dwo")
                })
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    (bytes, dwo)
}

fn test_command(a: &Artifact, target_dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(&a.executable);
    let pkg_dir = a.manifest.parent().unwrap_or(Path::new("."));
    let deps = target_dir.join("debug/deps");
    let mut ld = std::env::join_paths([deps, target_dir.join("debug")]).unwrap_or_default();
    if let Some(old) = std::env::var_os("LD_LIBRARY_PATH") {
        let mut joined = ld.clone();
        joined.push(":");
        joined.push(old);
        ld = joined;
    }
    cmd.current_dir(pkg_dir)
        .args(args)
        .env("CARGO_MANIFEST_DIR", pkg_dir)
        .env("CARGO_PKG_NAME", &a.package)
        .env("CARGO_CRATE_NAME", a.target.replace('-', "_"))
        .env("LD_LIBRARY_PATH", ld)
        .stdin(Stdio::null());
    cmd
}

/// Runs `cmd` with its output in `log`; kills its process group after `limit`. Returns the exit code
/// (`None` when killed) and the wall time.
fn run_limited(mut cmd: Command, log: &Path, limit: Duration) -> Result<(Option<i32>, Duration, bool)> {
    let file = fs::File::create(log)?;
    cmd.stdout(file.try_clone()?).stderr(file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().with_context(|| format!("starting {cmd:?}"))?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), start.elapsed(), false));
        }
        if start.elapsed() > limit {
            if cfg!(unix) {
                let _ = Command::new("kill")
                    .args(["-KILL", &format!("-{}", child.id())])
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok((None, start.elapsed(), true));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Builds the `regia` CLI into the measured target directory before the runs, outside the measured build.
///
/// Tests that shell out to the CLI (`samples/lookdev/src/workspace_bin.rs`, e.g. `regia_terrain`
/// `cook_pak_frame`) build it on demand when `<target>/debug/regia` is missing. A gate's workspace build
/// already has it; a `--packages` build does not, so without this the CLI's build would land inside that
/// test's run time and per-binary timeout. The same linker variable as the measured build keeps cargo's
/// fingerprints, so the dependencies are reused; its links are not logged.
fn prebuild_cli(root: &Path, target_dir: &Path) -> Option<String> {
    if !root.join("crates/regia_cli/Cargo.toml").is_file() {
        return None;
    }
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.current_dir(root)
        .args(["build", "-p", "regia_cli", "--bin", "regia"])
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("REGIA_TEST_COSTS_LINK_LOG");
    if let (Ok(Some(shim)), Some(var)) = (write_shim(target_dir), host_linker_var()) {
        cmd.env(var, shim);
    }
    println!("test-costs: building the regia CLI for tests that run it");
    let start = Instant::now();
    let note = match cmd.status() {
        Ok(status) if status.success() => format!(
            "regia CLI built before the runs (not part of the measured build): {:.0} s",
            start.elapsed().as_secs_f64()
        ),
        Ok(status) => {
            format!("regia CLI build before the runs failed ({status}); tests that run it build it on demand")
        }
        Err(e) => format!("regia CLI build before the runs did not start: {e}"),
    };
    Some(note)
}

/// One finished binary run.
struct RunOutcome {
    index: usize,
    exit: Option<i32>,
    seconds: f64,
    timed_out: bool,
    output: String,
}

/// A binary that rejected `-Z unstable-options --report-time` (it ran no test and said why).
pub fn refused_report_time(output: &str) -> bool {
    !output.contains("test result:")
        && (output.contains("unstable-options")
            || output.contains("nightly")
            || output.contains("Unrecognized option"))
}

fn file_key(a: &Artifact) -> String {
    format!("{}__{}__{}", a.package, a.target, a.kind.replace('+', "-"))
}

/// Runs the measurement.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| root.join("target/test-costs/report"));
    let out = std::path::absolute(&out).unwrap_or(out);
    let target_dir = args
        .target_dir
        .clone()
        .unwrap_or_else(|| root.join("target/test-costs/target"));
    let target_dir = std::path::absolute(&target_dir).unwrap_or(target_dir);
    fs::create_dir_all(out.join("lists"))?;
    fs::create_dir_all(out.join("logs"))?;
    let build_json = out.join("build.json");
    let link_log = out.join("link.log");
    let timings_copy = out.join("cargo-timing.html");
    let mut notes = Vec::new();
    let scope = if args.packages.is_empty() {
        vec!["--workspace".to_string()]
    } else {
        args.packages
            .iter()
            .flat_map(|p| ["-p".to_string(), p.clone()])
            .collect()
    };

    let mut build_wall = None;
    if !args.no_build {
        let shim = write_shim(&target_dir)?;
        let _ = fs::remove_file(&link_log);
        let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        cmd.current_dir(root)
            .args([
                "test",
                "--no-run",
                "--timings",
                "--message-format=json-render-diagnostics",
            ])
            .args(&scope)
            .env("CARGO_TARGET_DIR", &target_dir)
            .stdout(fs::File::create(&build_json)?);
        if let (Some(shim), Some(var)) = (shim, host_linker_var()) {
            cmd.env(var, shim).env("REGIA_TEST_COSTS_LINK_LOG", &link_log);
        } else {
            notes.push("link times not measured on this host (no linker shim)".to_string());
        }
        let load_before = load();
        println!(
            "test-costs: building ({}) in {}",
            scope.join(" "),
            target_dir.display()
        );
        let start = Instant::now();
        let status = cmd.status().context("running cargo test --no-run")?;
        let wall = start.elapsed().as_secs_f64();
        build_wall = Some(wall);
        notes.push(format!(
            "build: {wall:.0} s wall, load average {load_before} at the start and {} at the end",
            load()
        ));
        if !status.success() {
            bail!("cargo test --no-run failed (see the messages above)");
        }
        let html = target_dir.join("cargo-timings/cargo-timing.html");
        if html.exists() {
            fs::copy(&html, &timings_copy)?;
        }
    }

    let artifacts: Vec<Artifact> = parse_build_json(
        &fs::read_to_string(&build_json).with_context(|| format!("reading {}", build_json.display()))?,
    )
    .into_iter()
    .filter(|a| args.only.is_empty() || args.only.iter().any(|o| a.target.contains(o.as_str())))
    .collect();
    if artifacts.is_empty() {
        bail!("no test executables in {}", build_json.display());
    }
    let links = fs::read_to_string(&link_log)
        .map(|t| parse_link_log(&t))
        .unwrap_or_default();
    let units = fs::read_to_string(&timings_copy)
        .ok()
        .and_then(|h| parse_unit_data(&h).ok())
        .unwrap_or_default();

    let link_by_output: HashMap<String, f64> = links.iter().map(|l| (l.output.clone(), l.seconds)).collect();
    let mut unit_by_target: HashMap<(String, Option<String>), f64> = HashMap::new();
    for u in units.iter().filter(|u| is_test_unit(u)) {
        *unit_by_target
            .entry((u.name.clone(), unit_target(&u.target)))
            .or_default() += u.duration;
    }

    let mut rows: Vec<Row> = artifacts
        .iter()
        .map(|a| {
            let (bytes, dwo_bytes) = disk_of(&a.executable);
            let unit_key = (
                a.package.clone(),
                matches!(a.kind.as_str(), "test" | "bin" | "bench" | "example").then(|| a.target.clone()),
            );
            Row {
                package: a.package.clone(),
                target: a.target.clone(),
                kind: a.kind.clone(),
                bytes,
                dwo_bytes,
                link_s: link_by_output.get(&a.executable.display().to_string()).copied(),
                unit_s: unit_by_target.get(&unit_key).copied(),
                ..Row::default()
            }
        })
        .collect();

    // Lists, then runs.
    let mut all_tests: Vec<(String, TestTime)> = Vec::new();
    for (a, row) in artifacts.iter().zip(rows.iter_mut()) {
        let key = file_key(a);
        let listed = test_command(a, &target_dir, &["--list"]).output();
        let ignored = test_command(a, &target_dir, &["--list", "--ignored"]).output();
        let names = listed
            .map(|o| parse_list(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        row.tests = names.len();
        row.ignored = ignored
            .map(|o| parse_list(&String::from_utf8_lossy(&o.stdout)).len())
            .unwrap_or(0);
        fs::write(
            out.join("lists").join(format!("{key}.txt")),
            names.join("\n") + "\n",
        )?;
    }
    if !args.no_run
        && let Some(note) = prebuild_cli(root, &target_dir)
    {
        notes.push(note);
    }
    let load_run_before = load();
    let run_start = Instant::now();
    if !args.no_run {
        let jobs = args.jobs.max(1);
        let next = Mutex::new(0usize);
        let results: Mutex<Vec<RunOutcome>> = Mutex::new(Vec::new());
        let threads = args.test_threads.to_string();
        std::thread::scope(|s| {
            for _ in 0..jobs {
                s.spawn(|| {
                    loop {
                        let i = {
                            let Ok(mut n) = next.lock() else { return };
                            let i = *n;
                            *n += 1;
                            i
                        };
                        let Some(a) = artifacts.get(i) else { return };
                        if rows[i].tests == rows[i].ignored {
                            continue;
                        }
                        let log = out.join("logs").join(format!("{}.log", file_key(a)));
                        let timed = [
                            "--test-threads",
                            &threads,
                            "-Z",
                            "unstable-options",
                            "--report-time",
                        ];
                        let mut cmd = test_command(a, &target_dir, &timed);
                        cmd.env("RUSTC_BOOTSTRAP", "1");
                        let limit = Duration::from_secs(args.timeout);
                        let mut res = run_limited(cmd, &log, limit);
                        let refused = fs::read_to_string(&log)
                            .map(|t| refused_report_time(&t))
                            .unwrap_or(false);
                        if refused {
                            let plain = ["--test-threads", threads.as_str()];
                            res = run_limited(test_command(a, &target_dir, &plain), &log, limit);
                        }
                        let text = fs::read_to_string(&log).unwrap_or_default();
                        let entry = match res {
                            Ok((exit, wall, timed_out)) => RunOutcome {
                                index: i,
                                exit,
                                seconds: wall.as_secs_f64(),
                                timed_out,
                                output: text,
                            },
                            Err(e) => RunOutcome {
                                index: i,
                                exit: None,
                                seconds: 0.0,
                                timed_out: false,
                                output: format!("{e:#}"),
                            },
                        };
                        println!(
                            "test-costs: {} {} {:.1} s{}",
                            a.package,
                            a.target,
                            entry.seconds,
                            if entry.timed_out { " TIMEOUT" } else { "" }
                        );
                        if let Ok(mut r) = results.lock() {
                            r.push(entry);
                        }
                    }
                });
            }
        });
        for done in results.into_inner().unwrap_or_default() {
            let row = &mut rows[done.index];
            row.run_s = Some(done.seconds);
            row.exit = done.exit;
            row.timed_out = done.timed_out;
            row.counts = parse_counts(&done.output);
            for t in parse_test_lines(&done.output) {
                all_tests.push((format!("{}:{}", row.package, row.target), t));
            }
        }
        notes.push(format!(
            "runs: {:.0} s wall, {} binaries at once, --test-threads {}, timeout {} s, load average {load_run_before} \
             at the start and {} at the end",
            run_start.elapsed().as_secs_f64(),
            args.jobs.max(1),
            args.test_threads,
            args.timeout,
            load()
        ));
    }

    let unit_total: f64 = units.iter().map(|u| u.duration).sum();
    let link_total: f64 = links.iter().map(|l| l.seconds).sum();
    let summary = Summary {
        command: if args.no_build {
            format!(
                "--no-build: an earlier test build, read from {}",
                build_json.display()
            )
        } else {
            format!("cargo test --no-run --timings {}", scope.join(" "))
        },
        build_wall,
        unit_total,
        test_unit_total: units.iter().filter(|u| is_test_unit(u)).map(|u| u.duration).sum(),
        link_total,
        links: links.len(),
        notes,
        host: format!(
            "{} {}, {} hardware threads",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
        ),
    };
    fs::write(out.join("test-costs.csv"), render_csv(&rows))?;
    fs::write(out.join("slowest-tests.csv"), render_tests_csv(&all_tests))?;
    let md = render_markdown(&summary, &rows, &all_tests, args.top);
    fs::write(out.join("test-costs.md"), &md)?;
    println!(
        "test-costs: {} binaries; report {}",
        rows.len(),
        out.join("test-costs.md").display()
    );
    let failed = rows
        .iter()
        .filter(|r| r.timed_out || r.exit.is_some_and(|c| c != 0))
        .count();
    if failed > 0 {
        println!("test-costs: {failed} binaries failed or timed out (listed in the report)");
    }
    Ok(ExitCode::SUCCESS)
}

/// Build-wide numbers for the report header.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    /// The measured cargo command.
    pub command: String,
    /// Wall seconds of the build, when this run built.
    pub build_wall: Option<f64>,
    /// Sum of every unit's duration (cargo `--timings`).
    pub unit_total: f64,
    /// Sum of the test units' durations.
    pub test_unit_total: f64,
    /// Sum of every link's duration (all links: tests, build scripts, proc macros, bins).
    pub link_total: f64,
    /// Number of links the shim saw.
    pub links: usize,
    /// Load and phase notes.
    pub notes: Vec<String>,
    /// Host description.
    pub host: String,
}

fn opt(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.2}")).unwrap_or_default()
}

/// `test-costs.csv`, sorted by cost.
pub fn render_csv(rows: &[Row]) -> String {
    let mut sorted: Vec<&Row> = rows.iter().collect();
    sorted.sort_by(|a, b| b.cost().total_cmp(&a.cost()).then(b.disk().cmp(&a.disk())));
    let mut s = String::from(
        "package,target,kind,tests,ignored,bytes,dwo_bytes,link_s,unit_s,run_s,exit,timed_out,passed,failed\n",
    );
    for r in sorted {
        let _ = writeln!(
            s,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.package,
            r.target,
            r.kind,
            r.tests,
            r.ignored,
            r.bytes,
            r.dwo_bytes,
            opt(r.link_s),
            opt(r.unit_s),
            opt(r.run_s),
            r.exit.map(|c| c.to_string()).unwrap_or_default(),
            r.timed_out,
            r.counts.passed,
            r.counts.failed
        );
    }
    s
}

/// `slowest-tests.csv`, slowest first.
pub fn render_tests_csv(tests: &[(String, TestTime)]) -> String {
    let mut sorted: Vec<&(String, TestTime)> = tests.iter().filter(|(_, t)| t.seconds.is_some()).collect();
    sorted.sort_by(|a, b| b.1.seconds.unwrap_or(0.0).total_cmp(&a.1.seconds.unwrap_or(0.0)));
    let mut s = String::from("binary,test,status,seconds\n");
    for (bin, t) in sorted {
        let _ = writeln!(s, "{bin},{},{},{}", t.name, t.status, opt(t.seconds));
    }
    s
}

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0))
}

/// `test-costs.md`: header, per-package totals, and the top `top` binaries by disk, link, run and cost,
/// then the slowest `top` single tests.
pub fn render_markdown(summary: &Summary, rows: &[Row], tests: &[(String, TestTime)], top: usize) -> String {
    let mut s = String::from("# Test costs (`cargo xtask test-costs`)\n\n");
    let _ = writeln!(s, "Command: `{}`. Host: {}.\n", summary.command, summary.host);
    for n in &summary.notes {
        let _ = writeln!(s, "- {n}");
    }
    let disk: u64 = rows.iter().map(Row::disk).sum();
    // `+ 0.0`: an empty float sum is -0.0, which would print as "-0".
    let test_links: f64 = rows.iter().filter_map(|r| r.link_s).sum::<f64>() + 0.0;
    let runs: f64 = rows.iter().filter_map(|r| r.run_s).sum::<f64>() + 0.0;
    let tests_n: usize = rows.iter().map(|r| r.tests).sum();
    let _ = writeln!(
        s,
        "\n| Measure | Value |\n|---|---|\n| Test binaries | {} |\n| Tests listed | {tests_n} |\n\
         | Disk of test binaries (with split debug info) | {} MiB |\n| Link seconds of test binaries (sum) | {test_links:.0} |\n\
         | Link seconds of every link in the build (sum, {} links) | {:.0} |\n| Unit seconds of the build (sum of cargo's units) | {:.0} |\n\
         | Unit seconds of the test units (compile + link) | {:.0} |\n| Share of unit seconds spent linking | {} |\n\
         | Run seconds of the test binaries (sum) | {runs:.0} |",
        rows.len(),
        mib(disk),
        summary.links,
        summary.link_total,
        summary.unit_total,
        summary.test_unit_total,
        if summary.unit_total > 0.0 {
            format!("{:.1} %", 100.0 * summary.link_total / summary.unit_total)
        } else {
            "n/a".into()
        }
    );
    if let Some(w) = summary.build_wall {
        let _ = writeln!(s, "| Build wall seconds | {w:.0} |");
    }

    let mut packages: BTreeMap<&str, (usize, u64, f64, f64, usize)> = BTreeMap::new();
    for r in rows {
        let e = packages.entry(&r.package).or_default();
        e.0 += 1;
        e.1 += r.disk();
        e.2 += r.link_s.unwrap_or(0.0);
        e.3 += r.run_s.unwrap_or(0.0);
        e.4 += r.tests;
    }
    s.push_str("\n## Per package\n\n| Package | Binaries | Tests | Disk MiB | Link s | Run s |\n|---|---|---|---|---|---|\n");
    let mut pk: Vec<_> = packages.into_iter().collect();
    pk.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(b.0)));
    for (p, (n, d, l, r, t)) in pk {
        let _ = writeln!(s, "| {p} | {n} | {t} | {} | {l:.1} | {r:.1} |", mib(d));
    }

    let header = "| Package | Target | Kind | Tests | Disk MiB | Link s | Unit s | Run s | Exit |\n|---|---|---|---|---|---|---|---|---|\n";
    let line = |r: &Row| {
        format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.package,
            r.target,
            r.kind,
            r.tests,
            mib(r.disk()),
            opt(r.link_s),
            opt(r.unit_s),
            opt(r.run_s),
            if r.timed_out {
                "timeout".to_string()
            } else {
                r.exit.map(|c| c.to_string()).unwrap_or_default()
            }
        )
    };
    type Key = fn(&Row) -> f64;
    let lists: [(&str, Key); 4] = [
        ("cost (link + run seconds)", |r| r.cost()),
        ("disk", |r| r.disk() as f64),
        ("link time", |r| r.link_s.unwrap_or(0.0)),
        ("run time", |r| r.run_s.unwrap_or(0.0)),
    ];
    for (title, key) in lists {
        let mut sorted: Vec<&Row> = rows.iter().collect();
        sorted.sort_by(|a, b| key(b).total_cmp(&key(a)));
        let _ = write!(s, "\n## Top {top} by {title}\n\n{header}");
        for r in sorted.into_iter().take(top) {
            s.push_str(&line(r));
        }
    }
    let failing: Vec<&Row> = rows
        .iter()
        .filter(|r| r.timed_out || r.exit.is_some_and(|c| c != 0))
        .collect();
    if !failing.is_empty() {
        let _ = write!(s, "\n## Failed or timed out\n\n{header}");
        for r in failing {
            s.push_str(&line(r));
        }
    }
    let mut timed: Vec<&(String, TestTime)> = tests.iter().filter(|(_, t)| t.seconds.is_some()).collect();
    timed.sort_by(|a, b| b.1.seconds.unwrap_or(0.0).total_cmp(&a.1.seconds.unwrap_or(0.0)));
    let _ = write!(
        s,
        "\n## Slowest {top} single tests\n\n| Binary | Test | Status | Seconds |\n|---|---|---|---|\n"
    );
    for (bin, t) in timed.into_iter().take(top) {
        let _ = writeln!(s, "| {bin} | `{}` | {} | {} |", t.name, t.status, opt(t.seconds));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_come_out_of_every_package_id_form() {
        assert_eq!(package_name("path+file:///w/crates/regia_ui#0.3.6"), "regia_ui");
        assert_eq!(
            package_name("path+file:///w/samples/lab#render_lab@0.1.0"),
            "render_lab"
        );
        assert_eq!(
            package_name("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.228"),
            "serde"
        );
        assert_eq!(
            package_name("regia_ui 0.3.6 (path+file:///w/crates/regia_ui)"),
            "regia_ui"
        );
    }

    #[test]
    fn only_test_profile_executables_are_read_from_the_build_messages() {
        let text = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/regia_ui#0.3.6","manifest_path":"/w/crates/regia_ui/Cargo.toml","target":{"kind":["test"],"name":"suite","src_path":"/w/crates/regia_ui/tests/suite/main.rs"},"profile":{"test":true},"executable":"/t/debug/deps/suite-0123","fresh":false}
{"reason":"compiler-artifact","package_id":"path+file:///w/crates/regia_ui#0.3.6","manifest_path":"/w/crates/regia_ui/Cargo.toml","target":{"kind":["lib"],"name":"regia_ui"},"profile":{"test":false},"executable":null,"fresh":true}
{"reason":"compiler-artifact","package_id":"path+file:///w/crates/regia_ui#0.3.6","manifest_path":"/w/crates/regia_ui/Cargo.toml","target":{"kind":["lib"],"name":"regia_ui"},"profile":{"test":true},"executable":"/t/debug/deps/regia_ui-77","fresh":true}
not json
{"reason":"build-finished","success":true}"#;
        let a = parse_build_json(text);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].package, "regia_ui");
        assert_eq!(a[0].target, "suite");
        assert_eq!(a[0].kind, "test");
        assert_eq!(a[0].executable, PathBuf::from("/t/debug/deps/suite-0123"));
        assert_eq!(a[0].manifest, PathBuf::from("/w/crates/regia_ui/Cargo.toml"));
        assert_eq!(a[1].kind, "lib");
    }

    #[test]
    fn the_link_log_gives_seconds_per_output_and_skips_torn_lines() {
        let text = "1000000000\t3500000000\t0\t/t/debug/deps/suite-0123\n1\t2\n5\t4\t1\t/t/x\n";
        let links = parse_link_log(text);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].output, "/t/debug/deps/suite-0123");
        assert!((links[0].seconds - 2.5).abs() < 1e-9);
        assert_eq!(links[0].code, 0);
        // A clock step backwards never yields a negative time.
        assert_eq!(links[1].seconds, 0.0);
        assert_eq!(links[1].code, 1);
    }

    #[test]
    fn unit_data_is_read_from_the_timings_page() {
        let html = r#"<script>
const UNIT_DATA = [{"i":0,"name":"regia_ui","version":"0.3.6","mode":"test","target":" (test \"suite\")","start":1.5,"duration":40.25,"rmeta_time":null,"unblocked_units":[],"unblocked_rmeta_units":[]},
{"i":1,"name":"regia_ui","version":"0.3.6","mode":"test","target":" lib (test)","start":2.0,"duration":10.0},
{"i":2,"name":"serde","version":"1.0.0","mode":"build","target":"","start":0.0,"duration":3.0}];
const CONCURRENCY_DATA = [];
</script>"#;
        let units = parse_unit_data(html).expect("parses");
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].mode, "test");
        assert_eq!(units[0].duration, 40.25);
        assert_eq!(unit_target(&units[0].target).as_deref(), Some("suite"));
        assert_eq!(unit_target(&units[1].target), None);
        assert_eq!(unit_target(" bin \"regia\" (test)").as_deref(), Some("regia"));
        // Cargo 1.99's form: the target name first, then its kind.
        assert_eq!(unit_target(" suite \"test\" (test)").as_deref(), Some("suite"));
        assert_eq!(unit_target(" regia_ui \"lib\" (test)"), None);
        assert_eq!(unit_target(" regia \"bin\" (test)").as_deref(), Some("regia"));
        let todo = Unit {
            name: "regia_ui".into(),
            target: " suite \"test\" (test)".into(),
            mode: "todo".into(),
            start: 0.0,
            duration: 1.0,
        };
        assert!(is_test_unit(&todo));
        assert!(is_test_unit(&units[0]));
        assert!(!is_test_unit(&units[2]));
        assert!(parse_unit_data("<html></html>").is_err());
    }

    #[test]
    fn lists_results_and_report_times_are_parsed() {
        let list = "theme_switch::a_switch_redraws: test\nb: test\nbench_x: bench\n\n3 tests, 1 benchmark\n";
        assert_eq!(
            parse_list(list),
            ["b", "bench_x", "theme_switch::a_switch_redraws"]
        );
        let run = "running 3 tests\n\
                   test a ... ok <0.012s>\n\
                   test m::b ... FAILED <1.500s>\n\
                   test c ... ignored, slow\n\
                   test d ... ok\n\
                   test result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.52s\n\
                   test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let t = parse_test_lines(run);
        assert_eq!(t.len(), 4);
        assert_eq!(t[0].seconds, Some(0.012));
        assert_eq!(t[1].name, "m::b");
        assert_eq!(t[1].status, "FAILED");
        assert_eq!(t[2].status, "ignored");
        assert_eq!(t[2].seconds, None);
        assert_eq!(t[3].seconds, None);
        assert_eq!(
            parse_counts(run),
            Counts {
                passed: 6,
                failed: 1,
                ignored: 1
            }
        );
    }

    #[test]
    fn the_report_sorts_by_cost_and_totals_disk_and_links() {
        let rows = vec![
            Row {
                package: "regia_ui".into(),
                target: "suite".into(),
                kind: "test".into(),
                tests: 300,
                bytes: 2 * 1024 * 1024,
                dwo_bytes: 1024 * 1024,
                link_s: Some(2.0),
                run_s: Some(10.0),
                exit: Some(0),
                ..Row::default()
            },
            Row {
                package: "regia_physics".into(),
                target: "stability".into(),
                kind: "test".into(),
                tests: 32,
                bytes: 1024 * 1024,
                link_s: Some(1.0),
                run_s: Some(30.0),
                exit: Some(101),
                ..Row::default()
            },
        ];
        let csv = render_csv(&rows);
        let mut lines = csv.lines();
        assert!(lines.next().is_some_and(|h| h.starts_with("package,target")));
        assert!(
            lines
                .next()
                .is_some_and(|l| l.starts_with("regia_physics,stability"))
        );
        let summary = Summary {
            command: "cargo test --no-run --timings -p regia_ui".into(),
            unit_total: 100.0,
            link_total: 25.0,
            links: 4,
            ..Summary::default()
        };
        let tests = vec![
            (
                "regia_ui:suite".to_string(),
                TestTime {
                    name: "tq_draw::batches".into(),
                    status: "ok".into(),
                    seconds: Some(0.5),
                },
            ),
            (
                "regia_physics:stability".to_string(),
                TestTime {
                    name: "tall_stack".into(),
                    status: "ok".into(),
                    seconds: Some(9.0),
                },
            ),
        ];
        let md = render_markdown(&summary, &rows, &tests, 30);
        assert!(md.contains("| Test binaries | 2 |"), "{md}");
        assert!(
            md.contains("| Disk of test binaries (with split debug info) | 4.0 MiB |"),
            "{md}"
        );
        assert!(
            md.contains("| Share of unit seconds spent linking | 25.0 % |"),
            "{md}"
        );
        assert!(md.contains("## Failed or timed out"), "{md}");
        // A report with nothing run prints 0, not -0.
        let idle = render_markdown(&summary, &[Row::default()], &[], 30);
        assert!(
            idle.contains("| Run seconds of the test binaries (sum) | 0 |"),
            "{idle}"
        );
        let slow = md.split("## Slowest").nth(1).expect("slowest section");
        assert!(slow.find("tall_stack") < slow.find("tq_draw::batches"), "{slow}");
        assert!(
            render_tests_csv(&tests)
                .lines()
                .nth(1)
                .is_some_and(|l| l.contains("tall_stack"))
        );
    }

    #[test]
    fn a_binary_that_rejects_report_time_is_run_again_without_it() {
        assert!(refused_report_time(
            "error: the option `Z` is only accepted on the nightly compiler\n"
        ));
        assert!(!refused_report_time(
            "running 1 test\ntest a ... ok <0.1s>\ntest result: ok. 1 passed; 0 failed\n"
        ));
        // A crash before any result is a failure to report, not a refusal.
        assert!(!refused_report_time("thread 'main' panicked at src/x.rs\n"));
    }

    #[test]
    fn the_shim_records_its_output_path() {
        let shim = include_str!("test_costs_link_shim.sh");
        assert!(shim.starts_with("#!/bin/sh"));
        assert!(shim.contains("REGIA_TEST_COSTS_LINK_LOG"));
        assert!(shim.contains("REGIA_TEST_COSTS_REAL_LINKER:-cc"));
        // Kept link arguments are named after the output, so a replay finds them by binary name.
        assert!(shim.contains("${out##*/}.args"));
    }
}
