// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask fuzz`: coverage-guided fuzzing of the fuzz-target registry under libFuzzer (ADR-2723,
//! `docs/specs/97-fuzz-hardening.md` § 8, `book/agents/fuzzing.md`).
//!
//! The crate `fuzz/` (nightly, outside the workspace) builds one ASan binary that picks its target from
//! `REGIA_FUZZ_TARGET`. This command runs that binary: it seeds `fuzz/corpus/<target>/` from the target's fixture, passes
//! `fuzz/findings/<target>/` as a read-only corpus, sorts what libFuzzer leaves behind (a crash that reproduces is
//! minimized into `fuzz/findings/<target>/<hash>`; one that does not is reported as flaky), and sums the executions.
//! `replay` needs no nightly: it runs every committed finding through the same one-input check on stable.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Subcommand};
use regia_agent::fuzz_targets::{self, FuzzInput, FuzzTarget};
use serde_json::{Value, json};

mod extra_targets;
mod one_input;

/// The directory of the cargo-fuzz crate, relative to the workspace root.
const FUZZ_DIR: &str = "fuzz";
/// The cargo-fuzz binary (one, dispatched by name).
const FUZZ_BIN: &str = "registry";
/// Findings owned by another lane: committed, not replayed by default.
const PENDING: &str = "pending";
/// How much of a worker log is kept and read: the final stats and the crash are at its end.
const LOG_TAIL_BYTES: u64 = 256 * 1024;
/// A replay call that does not return in this time is reported as a hang (a guard, never a pass condition).
const REPLAY_GUARD: Duration = Duration::from_secs(300);

/// `cargo xtask fuzz`.
#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: FuzzCommand,
}

#[derive(Subcommand)]
enum FuzzCommand {
    /// List every target the guided fuzzer runs: the registry, then the extra targets.
    List {
        /// One JSON object instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Write each target's fixture into `fuzz/corpus/<target>/` (gitignored).
    Seed {
        /// Only this target.
        #[arg(long)]
        target: Option<String>,
    },
    /// Build the fuzz binary (nightly, ASan): `run` and `run-all` do this first unless told `--no-build`.
    Build(BuildOptions),
    /// Fuzz one target; a crash that reproduces is minimized into `fuzz/findings/<target>/<hash>`.
    Run {
        /// The target's name (`cargo xtask fuzz list`).
        #[arg(long)]
        target: String,
        /// Fuzzing time.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Fuzzer processes sharing the corpus.
        #[arg(long, default_value_t = 1)]
        workers: usize,
        #[command(flatten)]
        run: RunOptions,
        #[command(flatten)]
        build: BuildOptions,
    },
    /// Fuzz every target (or those with a prefix), `--parallel` at a time, and write the summary.
    RunAll {
        /// Fuzzing time per target.
        #[arg(long, default_value_t = 60)]
        seconds_per_target: u64,
        /// Targets fuzzed at once, one process each.
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// Only the targets whose name starts with this.
        #[arg(long)]
        prefix: Option<String>,
        /// Skip the targets the summary file already holds (a stopped run continues).
        #[arg(long)]
        resume: bool,
        /// The JSON summary, rewritten after every target (default `fuzz/logs/summary.json`).
        #[arg(long)]
        summary: Option<PathBuf>,
        #[command(flatten)]
        run: RunOptions,
        #[command(flatten)]
        build: BuildOptions,
    },
    /// Replay every committed finding through the registry on stable Rust.
    Replay {
        /// Also replay `fuzz/findings/pending/` (reports them; failures there do not fail the command).
        #[arg(long)]
        pending: bool,
        /// Only this target.
        #[arg(long)]
        target: Option<String>,
    },
}

/// How to build the fuzz binary.
#[derive(ClapArgs, Clone)]
struct BuildOptions {
    /// The rustup toolchain with `-Zsanitizer`.
    #[arg(long, default_value = "nightly")]
    toolchain: String,
    /// Target directory of the fuzz build (default `$REGIA_FUZZ_TARGET_DIR`, then `fuzz/target`).
    #[arg(long)]
    target_dir: Option<PathBuf>,
    /// Use the fuzz binary already built.
    #[arg(long)]
    no_build: bool,
}

/// libFuzzer limits.
#[derive(ClapArgs, Clone, Copy)]
struct RunOptions {
    /// One input over this time is a finding (`-timeout`). Generous: the first call of a material-graph target is a cold
    /// start of tens of seconds under the sanitizer on a loaded host.
    #[arg(long, default_value_t = 120)]
    timeout_seconds: u64,
    /// Peak memory of the process (`-rss_limit_mb`).
    #[arg(long, default_value_t = 4096)]
    rss_limit_mb: u64,
    /// Largest single allocation (`-malloc_limit_mb`): an input of at most 1 MiB has no reason to ask for more.
    #[arg(long, default_value_t = 2048)]
    malloc_limit_mb: u64,
    /// Stop a target after this many findings.
    #[arg(long, default_value_t = 5)]
    max_findings: usize,
    /// Time to spend minimizing one input. libFuzzer's `-max_total_time` limits each minimizer step, not the loop, so a slow
    /// input would otherwise hold a whole `run-all`; the smallest input so far is kept when this ends.
    #[arg(long, default_value_t = 180)]
    minimize_seconds: u64,
}

/// Runs the command.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    match &args.command {
        FuzzCommand::List { json } => {
            list(*json);
            Ok(ExitCode::SUCCESS)
        }
        FuzzCommand::Seed { target } => {
            let chosen = chosen_targets(target.as_deref(), None)?;
            for t in &chosen {
                seed_target(root, t)?;
            }
            println!("seeded {} targets in {FUZZ_DIR}/corpus", chosen.len());
            Ok(ExitCode::SUCCESS)
        }
        FuzzCommand::Build(build) => {
            build_binary(root, build)?;
            Ok(ExitCode::SUCCESS)
        }
        FuzzCommand::Run {
            target,
            seconds,
            workers,
            run,
            build,
        } => {
            let t = find_target(target)?;
            let binary = ready_binary(root, build)?;
            let fuzzer = Fuzzer::new(root, binary, *run, sanitizer_env(run));
            let outcome = fuzzer.run_target(&t, *seconds, (*workers).max(1))?;
            println!("{}", row(&outcome));
            for finding in &outcome.findings {
                println!("  {} {}: {}", finding.kind, finding.path, finding.message);
            }
            Ok(exit_for(std::slice::from_ref(&outcome)))
        }
        FuzzCommand::RunAll {
            seconds_per_target,
            parallel,
            prefix,
            resume,
            summary,
            run,
            build,
        } => {
            let binary = ready_binary(root, build)?;
            let fuzzer = Fuzzer::new(root, binary, *run, sanitizer_env(run));
            let summary = summary
                .clone()
                .unwrap_or_else(|| root.join(FUZZ_DIR).join("logs/summary.json"));
            run_all(
                &fuzzer,
                &chosen_targets(None, prefix.as_deref())?,
                *seconds_per_target,
                (*parallel).max(1),
                &summary,
                *resume,
            )
        }
        FuzzCommand::Replay { pending, target } => replay(root, *pending, target.as_deref()),
    }
}

/// The registry, then the extra targets.
fn all_targets() -> Vec<FuzzTarget> {
    let mut all = fuzz_targets::targets().to_vec();
    all.extend(extra_targets::targets());
    all
}

fn find_target(name: &str) -> Result<FuzzTarget> {
    all_targets()
        .into_iter()
        .find(|t| t.name == name)
        .with_context(|| format!("no fuzz target {name}: see `cargo xtask fuzz list`"))
}

fn chosen_targets(name: Option<&str>, prefix: Option<&str>) -> Result<Vec<FuzzTarget>> {
    if let Some(name) = name {
        return Ok(vec![find_target(name)?]);
    }
    let chosen: Vec<FuzzTarget> = all_targets()
        .into_iter()
        .filter(|t| prefix.is_none_or(|p| t.name.starts_with(p)))
        .collect();
    if chosen.is_empty() {
        bail!("no fuzz target starts with {prefix:?}");
    }
    Ok(chosen)
}

fn list(as_json: bool) {
    let builtin = fuzz_targets::targets().len();
    let rows: Vec<Value> = all_targets()
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "name": t.name,
                "format": t.format,
                "input": t.input_kind(),
                "round_trip": t.round_trip,
                "source": if i < builtin { "builtin" } else { "extra" },
            })
        })
        .collect();
    if as_json {
        println!("{}", json!({ "count": rows.len(), "targets": rows }));
    } else {
        for row in &rows {
            println!(
                "{:<40} {:<6} {}",
                row["name"].as_str().unwrap_or(""),
                row["input"].as_str().unwrap_or(""),
                row["source"].as_str().unwrap_or("")
            );
        }
        println!("{} targets", rows.len());
    }
}

/// A fixture as the bytes the fuzzer reads.
fn fixture_bytes(target: &FuzzTarget) -> Vec<u8> {
    match target.input {
        FuzzInput::Text { fixture, .. } => fixture().into_bytes(),
        FuzzInput::Bytes { fixture, .. } => fixture(),
    }
}

/// The name a finding is stored under: 128 bits of BLAKE3, in hex.
fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..32].to_string()
}

/// Writes `bytes` through a temporary name, so a reader never sees half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("a file path has a parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

fn corpus_dir(root: &Path, target: &str) -> PathBuf {
    root.join(FUZZ_DIR).join("corpus").join(target)
}

fn findings_root(root: &Path) -> PathBuf {
    root.join(FUZZ_DIR).join("findings")
}

/// Writes the target's fixture into its corpus; returns the file. Rewriting it changes nothing (the name is its hash).
fn seed_target(root: &Path, target: &FuzzTarget) -> Result<PathBuf> {
    let bytes = fixture_bytes(target);
    let path = corpus_dir(root, target.name).join(format!("fixture-{}", content_hash(&bytes)));
    write_atomic(&path, &bytes)?;
    Ok(path)
}

/// libFuzzer's largest input: room to grow past the biggest seed (`mutate_text_values` adds up to 64 KiB), at most 1 MiB,
/// or at least the fixture when the fixture is bigger than that.
fn max_input_len(fixture_len: usize) -> usize {
    (fixture_len * 2)
        .max(128 * 1024)
        .clamp(fixture_len, (1 << 20).max(fixture_len))
}

/// The triple cargo-fuzz builds for by default.
fn default_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "windows" => format!("{arch}-pc-windows-msvc"),
        _ => format!("{arch}-unknown-linux-gnu"),
    }
}

fn fuzz_target_dir(root: &Path, build: &BuildOptions) -> PathBuf {
    build
        .target_dir
        .clone()
        .or_else(|| std::env::var_os("REGIA_FUZZ_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| root.join(FUZZ_DIR).join("target"))
}

fn binary_path(root: &Path, build: &BuildOptions) -> PathBuf {
    fuzz_target_dir(root, build)
        .join(default_triple())
        .join("release")
        .join(FUZZ_BIN)
}

/// `cargo +<toolchain> fuzz build`: debug assertions on, as in the gate's test builds (an overflow is a finding); 16
/// codegen units instead of cargo-fuzz's 1 (ADR-2723), for a crate graph this size.
fn build_binary(root: &Path, build: &BuildOptions) -> Result<()> {
    // The fuzz crate is its own workspace; its first resolve starts from the versions the gate tests.
    let lock = std::fs::read(root.join("Cargo.lock")).context("read Cargo.lock")?;
    write_atomic(&root.join(FUZZ_DIR).join("Cargo.lock"), &lock)?;
    let mut cmd = Command::new("cargo");
    cmd.current_dir(root)
        .arg(format!("+{}", build.toolchain))
        .args(["fuzz", "build", FUZZ_BIN])
        .args(["--debug-assertions", "--codegen-units", "16"])
        .arg("--target-dir")
        .arg(fuzz_target_dir(root, build));
    let status = cmd.status().with_context(|| {
        format!(
            "run cargo +{} fuzz: install cargo-fuzz with `cargo install cargo-fuzz --locked`, and the toolchain with `rustup toolchain install {} --profile minimal -c rust-src -c llvm-tools-preview`",
            build.toolchain, build.toolchain
        )
    })?;
    if !status.success() {
        bail!("cargo fuzz build failed: {status}");
    }
    Ok(())
}

fn ready_binary(root: &Path, build: &BuildOptions) -> Result<PathBuf> {
    if !build.no_build {
        build_binary(root, build)?;
    }
    let binary = binary_path(root, build);
    if !binary.is_file() {
        bail!("{} is not built: run `cargo xtask fuzz build`", binary.display());
    }
    Ok(binary)
}

/// The sanitizer's settings: cargo-fuzz's ODR setting (Rust's duplicate generic instances are not violations), a smaller
/// quarantine, because `run-all` keeps several processes going, and the dedup token libFuzzer's crash minimizer compares to
/// stop at a different bug. ASan finds `llvm-symbolizer` on `PATH` when one is
/// installed; without it a report has addresses instead of lines.
///
/// The cap on one allocation is the sanitizer's too (`max_allocation_size_mb`, an `allocation-size-too-big` report):
/// libFuzzer enforces its `-malloc_limit_mb` only through malloc hooks it installs when leak checks are on, and the registry
/// runs with them off. The `-malloc_limit_mb` flag stays for builds that keep leak checks.
fn sanitizer_env(run: &RunOptions) -> Vec<(String, String)> {
    vec![(
        "ASAN_OPTIONS".to_string(),
        format!(
            "quarantine_size_mb=64:detect_odr_violation=0:dedup_token_length=3:max_allocation_size_mb={}:allocator_may_return_null=0",
            run.malloc_limit_mb
        ),
    )]
}

/// The numbers libFuzzer prints at its end (`-print_final_stats=1`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Stats {
    executions: u64,
    new_units: u64,
    peak_rss_mb: u64,
}

impl Stats {
    fn add(&mut self, other: Stats) {
        self.executions += other.executions;
        self.new_units += other.new_units;
        self.peak_rss_mb = self.peak_rss_mb.max(other.peak_rss_mb);
    }
}

/// Reads the final stats, falling back on the last progress line (`#1234\t...`) of a process that died before printing them.
fn parse_stats(log: &str) -> Stats {
    let stat = |name: &str| {
        log.lines()
            .rev()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.trim_start_matches(':').trim().parse::<u64>().ok())
    };
    let executions = stat("stat::number_of_executed_units").or_else(|| {
        log.lines().rev().find_map(|line| {
            let rest = line.strip_prefix('#')?;
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
    });
    Stats {
        executions: executions.unwrap_or(0),
        new_units: stat("stat::new_units_added").unwrap_or(0),
        peak_rss_mb: stat("stat::peak_rss_mb").unwrap_or(0),
    }
}

/// The first lines of a crash log that say what happened.
fn crash_message(log: &str) -> String {
    const MARKS: &[&str] = &[
        "regia-fuzz finding in",
        "panicked at",
        "ERROR: AddressSanitizer",
        "ERROR: libFuzzer",
        "SUMMARY: ",
    ];
    let mut lines: Vec<&str> = Vec::new();
    for line in log.lines() {
        if MARKS.iter().any(|m| line.contains(m)) && !lines.contains(&line) {
            lines.push(line);
            if lines.len() == 3 {
                break;
            }
        }
    }
    if lines.is_empty() {
        "no message in the log".to_string()
    } else {
        lines.join(" | ")
    }
}

/// What failed in a crash log, without the details an input changes: the finding's kind and, for a panic, where; else the
/// sanitizer's or libFuzzer's name for the error. Empty when the log names no failure.
fn failure_signature(log: &str) -> String {
    let after = |mark: &str| {
        log.lines().rev().find_map(|line| {
            let (_, rest) = line.split_once(mark)?;
            Some(rest.trim().to_string())
        })
    };
    let first_word = |text: String| text.split_whitespace().next().unwrap_or_default().to_string();
    if let Some(finding) = after("regia-fuzz finding in ") {
        let kind = finding
            .split_once(": ")
            .map_or("", |(_, rest)| rest.split(':').next().unwrap_or_default())
            .to_string();
        return match after("panicked at ").filter(|_| kind == "panic") {
            Some(place) => format!("panic {}", place.trim_end_matches(':')),
            None => kind,
        };
    }
    if let Some(error) = after("ERROR: AddressSanitizer: ") {
        return format!("asan {}", first_word(error));
    }
    if let Some(error) = after("ERROR: libFuzzer: ") {
        return format!("libfuzzer {}", first_word(error));
    }
    after("panicked at ").map_or_else(String::new, |place| {
        format!("panic {}", place.trim_end_matches(':'))
    })
}

/// What libFuzzer named an artifact after: `crash`, `oom`, `timeout` or `leak`; `None` for anything else.
fn artifact_kind(file_name: &str) -> Option<&'static str> {
    ["crash", "oom", "timeout", "leak"]
        .into_iter()
        .find(|kind| file_name.starts_with(&format!("{kind}-")))
}

/// The tail of a file, at most [`LOG_TAIL_BYTES`], as text.
fn read_tail(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    if file
        .seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One finding of a run.
#[derive(Debug, Clone)]
struct RunFinding {
    /// `crash`, `oom`, `timeout` or `leak`; `flaky-<kind>` when it did not happen again on a rerun of the same input.
    kind: String,
    /// Where it is stored, relative to the workspace root (under `fuzz/logs/` when flaky: not committed).
    path: String,
    /// What the crash log says.
    message: String,
}

/// What one target's run did.
#[derive(Debug, Clone, Default)]
struct TargetOutcome {
    name: String,
    seconds: u64,
    stats: Stats,
    corpus_files: usize,
    findings: Vec<RunFinding>,
    /// A fuzzer process that ended badly and left no input to blame.
    abnormal: Vec<String>,
}

impl TargetOutcome {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "seconds": self.seconds,
            "executions": self.stats.executions,
            "new_units": self.stats.new_units,
            "peak_rss_mb": self.stats.peak_rss_mb,
            "corpus_files": self.corpus_files,
            "findings": self.findings.iter().map(|f| json!({
                "kind": f.kind, "path": f.path, "message": f.message,
            })).collect::<Vec<_>>(),
            "abnormal": self.abnormal,
        })
    }
}

fn row(o: &TargetOutcome) -> String {
    format!(
        "{:<40} {:>5} s {:>12} execs {:>6} new {:>6} corpus {:>5} MB  findings {}{}",
        o.name,
        o.seconds,
        o.stats.executions,
        o.stats.new_units,
        o.corpus_files,
        o.stats.peak_rss_mb,
        o.findings.len(),
        if o.abnormal.is_empty() {
            String::new()
        } else {
            format!("  abnormal: {}", o.abnormal.join("; "))
        }
    )
}

/// The one kind of finding that fails nothing: a timeout that did not repeat is reported, but on a loaded host it is usually
/// a slow call. A crash, out-of-memory or leak that did not repeat is still a bug that depends on what ran before it.
const EXCUSED_KIND: &str = "flaky-timeout";

/// Failure when any summary row holds a finding that is not excused, or ended badly.
fn exit_for_rows<'a>(mut rows: impl Iterator<Item = &'a Value>) -> ExitCode {
    let empty = Vec::new();
    let clean = rows.all(|row| {
        row["abnormal"].as_array().is_none_or(Vec::is_empty)
            && row["findings"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .all(|f| f["kind"] == EXCUSED_KIND)
    });
    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn exit_for(outcomes: &[TargetOutcome]) -> ExitCode {
    let rows: Vec<Value> = outcomes.iter().map(TargetOutcome::to_json).collect();
    exit_for_rows(rows.iter())
}

fn file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file())
            .count()
    })
}

/// The libFuzzer flags of a fuzzing process.
fn fuzz_flags(run: &RunOptions, seconds: u64, max_len: usize, artifact_prefix: &Path) -> Vec<String> {
    vec![
        format!("-max_total_time={}", seconds.max(1)),
        format!("-timeout={}", run.timeout_seconds),
        format!("-rss_limit_mb={}", run.rss_limit_mb),
        format!("-malloc_limit_mb={}", run.malloc_limit_mb),
        format!("-max_len={max_len}"),
        format!("-artifact_prefix={}/", artifact_prefix.display()),
        "-print_final_stats=1".to_string(),
        "-use_value_profile=1".to_string(),
        // Leak checks stay off for the registry (ADR-2723): many loaders build and drop whole worlds, and the check costs
        // far more than the input; growth still ends in the RSS limit. The media decode run keeps them.
        "-detect_leaks=0".to_string(),
        // The loaders' stdout is noise. Their stderr has the panic.
        "-close_fd_mask=1".to_string(),
    ]
}

/// Runs the fuzz binary.
struct Fuzzer {
    root: PathBuf,
    binary: PathBuf,
    run: RunOptions,
    env: Vec<(String, String)>,
}

impl Fuzzer {
    fn new(root: &Path, binary: PathBuf, run: RunOptions, env: Vec<(String, String)>) -> Self {
        Self {
            root: root.to_path_buf(),
            binary,
            run,
            env,
        }
    }

    fn logs_dir(&self, target: &str) -> PathBuf {
        self.root.join(FUZZ_DIR).join("logs").join(target)
    }

    fn command(&self, target: &str) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.root)
            .env("REGIA_FUZZ_TARGET", target)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        cmd
    }

    /// Runs the binary with `args`, its output in `log`; the exit status text when it failed.
    fn run_logged(&self, target: &str, args: &[String], log: &Path) -> Result<Option<String>> {
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let out = std::fs::File::create(log).with_context(|| format!("create {}", log.display()))?;
        let err = out.try_clone()?;
        let status = self
            .command(target)
            .args(args)
            .stdout(out)
            .stderr(err)
            .status()
            .with_context(|| format!("run {}", self.binary.display()))?;
        Ok((!status.success()).then(|| status.to_string()))
    }

    /// Ends a process started in its own group, and whatever it started: libFuzzer runs each minimizer step as a child.
    fn end_group(child: &mut std::process::Child) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-s", "KILL", "--", &format!("-{}", child.id())])
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Whether running `input` once fails, and the log of that run.
    fn reproduce(&self, target: &str, input: &Path, log: &Path) -> Result<(bool, String)> {
        let flags = [
            format!("-timeout={}", self.run.timeout_seconds),
            format!("-rss_limit_mb={}", self.run.rss_limit_mb),
            format!("-malloc_limit_mb={}", self.run.malloc_limit_mb),
            "-detect_leaks=0".to_string(),
            "-runs=1".to_string(),
            input.display().to_string(),
        ];
        let failed = self.run_logged(target, &flags, log)?.is_some();
        Ok((failed, read_tail(log)))
    }

    /// Shrinks `input` with libFuzzer's crash minimizer into `out`, for at most `minimize_seconds`. The log of the file it
    /// leaves when that file fails; `None` when it does not fail, or it left none.
    fn minimize(&self, target: &str, input: &Path, out: &Path, log: &Path) -> Result<Option<String>> {
        let flags = [
            "-minimize_crash=1".to_string(),
            "-max_total_time=60".to_string(),
            format!("-timeout={}", self.run.timeout_seconds),
            format!("-rss_limit_mb={}", self.run.rss_limit_mb),
            format!("-malloc_limit_mb={}", self.run.malloc_limit_mb),
            "-detect_leaks=0".to_string(),
            format!("-exact_artifact_path={}", out.display()),
            input.display().to_string(),
        ];
        let output = std::fs::File::create(log).with_context(|| format!("create {}", log.display()))?;
        let errors = output.try_clone()?;
        let mut command = self.command(target);
        command.args(flags).stdout(output).stderr(errors);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command
            .spawn()
            .with_context(|| format!("run {}", self.binary.display()))?;
        let started = Instant::now();
        while child.try_wait()?.is_none() {
            if started.elapsed().as_secs() >= self.run.minimize_seconds {
                Self::end_group(&mut child);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !out.is_file() {
            return Ok(None);
        }
        let (failed, text) = self.reproduce(target, out, log)?;
        Ok(failed.then_some(text))
    }

    /// Sorts one artifact: a reproducible one is minimized into the findings, the rest stays under the logs as flaky. A
    /// smaller input that fails some other way is a different bug, so the input as found stays the finding.
    fn take_artifact(&self, target: &FuzzTarget, kind: &str, artifact: &Path) -> Result<RunFinding> {
        let logs = self.logs_dir(target.name);
        let verify_log = logs.join("verify.log");
        let (failed, verify_text) = self.reproduce(target.name, artifact, &verify_log)?;
        let bytes = std::fs::read(artifact).with_context(|| format!("read {}", artifact.display()))?;
        let hash = content_hash(&bytes);
        if !failed {
            let kept = logs.join(format!("flaky-{hash}"));
            write_atomic(&kept, &bytes)?;
            let path = kept
                .strip_prefix(&self.root)
                .unwrap_or(&kept)
                .display()
                .to_string();
            return Ok(RunFinding {
                kind: format!("flaky-{kind}"),
                path,
                message: "the input did not fail again on a rerun".to_string(),
            });
        }
        let small = logs.join("minimized.tmp");
        let _ = std::fs::remove_file(&small);
        let minimized = self.minimize(target.name, artifact, &small, &logs.join("minimize.log"))?;
        let same_failure =
            minimized.is_some_and(|text| failure_signature(&text) == failure_signature(&verify_text));
        let final_bytes = if same_failure {
            std::fs::read(&small).unwrap_or(bytes)
        } else {
            bytes
        };
        let _ = std::fs::remove_file(&small);
        let hash = content_hash(&final_bytes);
        let stored = findings_root(&self.root).join(target.name).join(&hash);
        write_atomic(&stored, &final_bytes)?;
        // The log of the reproducing run, for the report; the stored file is what gets committed.
        let keep_log = logs.join(format!("{hash}.log"));
        let _ = std::fs::copy(&verify_log, &keep_log);
        Ok(RunFinding {
            kind: kind.to_string(),
            path: stored
                .strip_prefix(&self.root)
                .unwrap_or(&stored)
                .display()
                .to_string(),
            message: crash_message(&verify_text),
        })
    }

    /// Fuzzes `target` for `seconds` with `workers` processes. A run that ends early on a new crash starts again for the
    /// time left, until `max_findings` or until a round finds nothing new. libFuzzer picks and prints its own random seed.
    fn run_target(&self, target: &FuzzTarget, seconds: u64, workers: usize) -> Result<TargetOutcome> {
        let seed = seed_target(&self.root, target)?;
        let corpus = corpus_dir(&self.root, target.name);
        let findings_dir = findings_root(&self.root).join(target.name);
        let max_len = max_input_len(std::fs::metadata(&seed).map_or(0, |m| m.len()) as usize);
        let logs = self.logs_dir(target.name);
        let started = Instant::now();
        let mut outcome = TargetOutcome {
            name: target.name.to_string(),
            ..TargetOutcome::default()
        };
        let mut seen = BTreeSet::new();
        let mut first_round = true;
        loop {
            let left = seconds.saturating_sub(started.elapsed().as_secs());
            if left == 0 {
                break;
            }
            let round = std::thread::scope(|scope| -> Result<Vec<(String, Stats, Option<String>)>> {
                let mut handles = Vec::new();
                for worker in 0..workers {
                    let (logs, corpus, findings_dir) = (&logs, &corpus, &findings_dir);
                    handles.push(scope.spawn(move || -> Result<(String, Stats, Option<String>)> {
                        let artifacts = logs.join(format!("w{worker}"));
                        std::fs::create_dir_all(&artifacts)?;
                        let mut args = fuzz_flags(&self.run, left, max_len, &artifacts);
                        args.push(corpus.display().to_string());
                        // Committed findings are seeds once. One stored by this run crashes as a seed, and would end
                        // the next round at start-up instead of letting it look further.
                        if first_round && findings_dir.is_dir() {
                            args.push(findings_dir.display().to_string());
                        }
                        let log = logs.join(format!("w{worker}.log"));
                        let failed = self.run_logged(target.name, &args, &log)?;
                        let text = read_tail(&log);
                        // Keep the tail only: a chatty loader must not fill the disk across 200 targets.
                        write_atomic(&log, text.as_bytes())?;
                        Ok((text.clone(), parse_stats(&text), failed))
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_else(|_| bail_join()))
                    .collect()
            })?;
            first_round = false;
            let mut new_finding = false;
            for (worker, (text, stats, failed)) in round.iter().enumerate() {
                outcome.stats.add(*stats);
                let artifacts = logs.join(format!("w{worker}"));
                let mut found = false;
                let mut files: Vec<PathBuf> = std::fs::read_dir(&artifacts)
                    .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
                    .unwrap_or_default();
                files.sort();
                for file in files {
                    let Some(kind) = file.file_name().and_then(|n| n.to_str()).and_then(artifact_kind) else {
                        continue;
                    };
                    found = true;
                    let finding = self.take_artifact(target, kind, &file)?;
                    let _ = std::fs::remove_file(&file);
                    if seen.insert(finding.path.clone()) {
                        new_finding = true;
                        outcome.findings.push(finding);
                    }
                }
                if let (Some(status), false) = (failed, found) {
                    outcome
                        .abnormal
                        .push(format!("{status}: {}", crash_message(text)));
                }
            }
            // The same crash again means the fuzzer cannot get past it: more rounds would only repeat it.
            if !new_finding || outcome.findings.len() >= self.run.max_findings {
                break;
            }
        }
        // A target that ran nothing proves nothing, whatever the reason.
        if outcome.stats.executions == 0 && outcome.findings.is_empty() && outcome.abnormal.is_empty() {
            outcome.abnormal.push("no input ran".to_string());
        }
        outcome.seconds = started.elapsed().as_secs();
        outcome.corpus_files = file_count(&corpus);
        Ok(outcome)
    }
}

/// A fuzzer thread cannot panic on its own: this only turns the impossible join error into a run error.
fn bail_join<T>() -> Result<T> {
    bail!("a fuzzer thread panicked")
}

/// The summary a `run-all` keeps on disk, so a stopped run can continue and the report has its numbers.
fn summary_json(rows: &BTreeMap<String, Value>, seconds_per_target: u64) -> Value {
    let executions: u64 = rows.values().map(|r| r["executions"].as_u64().unwrap_or(0)).sum();
    let findings: usize = rows
        .values()
        .map(|r| r["findings"].as_array().map_or(0, Vec::len))
        .sum();
    json!({
        "format_version": 1,
        "seconds_per_target": seconds_per_target,
        "targets": rows.len(),
        "executions": executions,
        "findings": findings,
        "rows": rows.values().collect::<Vec<_>>(),
    })
}

/// Reads a summary back into its rows; an unreadable file holds none.
fn read_summary(path: &Path) -> BTreeMap<String, Value> {
    let parsed: Option<Value> = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let mut rows = BTreeMap::new();
    if let Some(list) = parsed.as_ref().and_then(|v| v["rows"].as_array()) {
        for row in list {
            if let Some(name) = row["name"].as_str() {
                rows.insert(name.to_string(), row.clone());
            }
        }
    }
    rows
}

fn run_all(
    fuzzer: &Fuzzer,
    targets: &[FuzzTarget],
    seconds: u64,
    parallel: usize,
    summary: &Path,
    resume: bool,
) -> Result<ExitCode> {
    let previous = if resume {
        read_summary(summary)
    } else {
        BTreeMap::new()
    };
    let todo: Vec<&FuzzTarget> = targets
        .iter()
        .filter(|t| !previous.contains_key(t.name))
        .collect();
    println!(
        "fuzzing {} targets for {seconds} s each, {parallel} at a time (about {} min)",
        todo.len(),
        (todo.len() as u64 * seconds).div_ceil(parallel as u64 * 60)
    );
    let next = AtomicUsize::new(0);
    let rows = Mutex::new(previous);
    let errors = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..parallel {
            scope.spawn(|| {
                while let Some(target) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    match fuzzer.run_target(target, seconds, 1) {
                        Ok(outcome) => {
                            println!("{}", row(&outcome));
                            let mut rows = rows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            rows.insert(outcome.name.clone(), outcome.to_json());
                            let text = summary_json(&rows, seconds).to_string();
                            if let Err(e) = write_atomic(summary, text.as_bytes()) {
                                eprintln!("summary: {e:#}");
                            }
                        }
                        Err(e) => errors
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("{}: {e:#}", target.name)),
                    }
                }
            });
        }
    });
    let errors = errors
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rows = rows
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let total = summary_json(&rows, seconds);
    println!(
        "{} targets, {} executions, {} findings; summary in {}",
        total["targets"],
        total["executions"],
        total["findings"],
        summary.display()
    );
    for e in &errors {
        eprintln!("error: {e}");
    }
    if !errors.is_empty() {
        return Ok(ExitCode::FAILURE);
    }
    // The rows, not this run's outcomes: `--resume` answers for what the earlier part found too.
    Ok(exit_for_rows(rows.values()))
}

/// One stored finding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredFinding {
    target: String,
    path: PathBuf,
    pending: bool,
}

/// The files directly under `dir`, sorted, skipping dot files; a missing directory holds none.
fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    paths.retain(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'))
    });
    paths.sort();
    paths
}

/// Every finding under `fuzz/findings/`: `<target>/<hash>`, and `pending/<target>/<hash>`. Files directly under the
/// directory (its README) are not findings.
fn stored_findings(root: &Path) -> Vec<StoredFinding> {
    let base = findings_root(root);
    let mut out = Vec::new();
    for dir in sorted_entries(&base).into_iter().filter(|p| p.is_dir()) {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if name == PENDING {
            for sub in sorted_entries(&dir).into_iter().filter(|p| p.is_dir()) {
                let Some(target) = sub.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                    continue;
                };
                out.extend(sorted_entries(&sub).into_iter().map(|path| StoredFinding {
                    target: target.clone(),
                    path,
                    pending: true,
                }));
            }
        } else {
            out.extend(sorted_entries(&dir).into_iter().map(|path| StoredFinding {
                target: name.clone(),
                path,
                pending: false,
            }));
        }
    }
    out
}

/// Runs one input on its own thread with the hang guard.
fn check_guarded(target: FuzzTarget, input: Vec<u8>) -> Result<one_input::Verdict, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(one_input::check_one(&target, &input));
    });
    match rx.recv_timeout(REPLAY_GUARD) {
        Ok(result) => result.map_err(|finding| finding.to_string()),
        Err(_) => Err(format!("hang: no answer in {} s", REPLAY_GUARD.as_secs())),
    }
}

/// Replays `findings`; failed ones with the reason. A directory that names no target is a failure too: it would otherwise
/// never be replayed again.
fn replay_findings(findings: &[StoredFinding], targets: &[FuzzTarget]) -> Vec<(StoredFinding, String)> {
    let mut failed = Vec::new();
    for finding in findings {
        let Some(target) = targets.iter().find(|t| t.name == finding.target) else {
            failed.push((finding.clone(), "no target has this name".to_string()));
            continue;
        };
        let bytes = match std::fs::read(&finding.path) {
            Ok(bytes) => bytes,
            Err(e) => {
                failed.push((finding.clone(), e.to_string()));
                continue;
            }
        };
        if let Err(reason) = check_guarded(*target, bytes) {
            failed.push((finding.clone(), reason));
        }
    }
    failed
}

fn replay(root: &Path, pending: bool, only: Option<&str>) -> Result<ExitCode> {
    let targets = all_targets();
    let findings: Vec<StoredFinding> = stored_findings(root)
        .into_iter()
        .filter(|f| only.is_none_or(|name| f.target == name))
        .filter(|f| pending || !f.pending)
        .collect();
    let failed = replay_findings(&findings, &targets);
    let failed_committed = failed.iter().filter(|(f, _)| !f.pending).count();
    for (finding, reason) in &failed {
        let state = if finding.pending {
            "still failing (pending)"
        } else {
            "FAILED"
        };
        println!("{state} {}: {reason}", finding.path.display());
    }
    println!(
        "replayed {} findings: {} failed, {} pending still failing",
        findings.len(),
        failed_committed,
        failed.len() - failed_committed
    );
    Ok(if failed_committed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_ok(text: &str) -> Result<String, String> {
        Ok(text.to_string())
    }
    fn load_panics(_: &str) -> Result<String, String> {
        panic!("boom")
    }
    fn load_empty_error(_: &str) -> Result<String, String> {
        Err(String::new())
    }
    fn load_bug(_: &str) -> Result<String, String> {
        Err(format!("{}generated code is invalid", fuzz_targets::BUG_PREFIX))
    }
    fn load_unstable(text: &str) -> Result<String, String> {
        Ok(format!("{text}x"))
    }
    fn load_rejects(_: &str) -> Result<String, String> {
        Err("bad input".to_string())
    }
    fn fixture() -> String {
        "(a: 1)".to_string()
    }

    fn text_target(name: &'static str, load: fn(&str) -> Result<String, String>) -> FuzzTarget {
        FuzzTarget {
            name,
            format: None,
            format_version: 0,
            round_trip: true,
            input: FuzzInput::Text { fixture, load },
        }
    }

    #[test]
    fn every_kind_of_failure_is_a_finding() {
        use fuzz_targets::FailureKind;
        let kind = |load| {
            one_input::check_one(&text_target("t", load), b"1")
                .expect_err("a finding")
                .kind
        };
        assert_eq!(kind(load_panics), FailureKind::Panic);
        assert_eq!(kind(load_empty_error), FailureKind::EmptyError);
        assert_eq!(kind(load_bug), FailureKind::Bug);
        assert_eq!(kind(load_unstable), FailureKind::RoundTrip);
    }

    #[test]
    fn accepted_and_rejected_are_not_findings() {
        assert_eq!(
            one_input::check_one(&text_target("t", load_ok), b"x"),
            Ok(one_input::Verdict::Accepted)
        );
        assert_eq!(
            one_input::check_one(&text_target("t", load_rejects), b"x"),
            Ok(one_input::Verdict::Rejected)
        );
    }

    #[test]
    fn a_target_without_a_writer_skips_the_round_trip() {
        let target = FuzzTarget {
            round_trip: false,
            ..text_target("t", load_unstable)
        };
        assert_eq!(
            one_input::check_one(&target, b"x"),
            Ok(one_input::Verdict::Accepted)
        );
    }

    /// Bad UTF-8 still reaches a text loader, which sees U+FFFD in place of the broken bytes.
    #[test]
    fn a_text_target_reads_any_bytes() {
        fn wants_replacement(text: &str) -> Result<String, String> {
            if text == "a\u{fffd}b" {
                panic!("the loader saw the input");
            }
            Err(format!("other input: {text:?}"))
        }
        let found = one_input::check_one(&text_target("t", wants_replacement), &[b'a', 0xFF, b'b'])
            .expect_err("reached the loader");
        assert!(found.message.contains("saw the input"), "{found}");
    }

    #[test]
    fn target_names_are_unique_and_safe_as_directories() {
        let all = all_targets();
        let mut names = BTreeSet::new();
        for t in &all {
            assert!(names.insert(t.name), "{} twice", t.name);
            assert_ne!(t.name, PENDING);
            assert!(
                !t.name.starts_with('.')
                    && t.name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
                "{}",
                t.name
            );
        }
        assert!(all.len() > fuzz_targets::targets().len());
    }

    /// The gate test (`crates/regia_agent/tests/fuzz_findings.rs`) replays what it can link and takes the rest by prefix.
    #[test]
    fn every_extra_target_has_a_prefix_the_gate_test_knows() {
        let test = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/regia_agent/tests/fuzz_findings.rs"),
        )
        .expect("the gate test");
        for t in extra_targets::targets() {
            let prefix = &t.name[..=t.name.find('.').expect("a dotted name")];
            assert!(
                test.contains(&format!("\"{prefix}\"")),
                "{} is not in the test's list",
                t.name
            );
        }
    }

    /// Every extra target's own fixture loads: a broken fixture would fuzz nothing but the first error.
    #[test]
    fn extra_fixtures_are_accepted() {
        for t in extra_targets::targets() {
            let bytes = fixture_bytes(&t);
            assert!(!bytes.is_empty(), "{}", t.name);
            assert_eq!(
                one_input::check_one(&t, &bytes),
                Ok(one_input::Verdict::Accepted),
                "{}",
                t.name
            );
        }
    }

    #[test]
    fn seeding_writes_the_fixture_under_its_hash_and_repeats() {
        let root = regia_test_utils::unique_temp_dir("xtask-fuzz-seed");
        let target = text_target("t.seed", load_ok);
        let first = seed_target(&root, &target).expect("seed");
        let second = seed_target(&root, &target).expect("seed again");
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&first).expect("read"), fixture().into_bytes());
        assert_eq!(file_count(&corpus_dir(&root, "t.seed")), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hash_is_a_stable_file_name() {
        assert_eq!(content_hash(b"abc").len(), 32);
        assert_eq!(content_hash(b"abc"), "6437b3ac38465133ffb63b75273a8db5");
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
    }

    #[test]
    fn max_len_leaves_room_and_never_cuts_a_fixture() {
        assert_eq!(max_input_len(10), 128 * 1024);
        assert_eq!(max_input_len(100_000), 200_000);
        assert_eq!(max_input_len(900_000), 1 << 20);
        assert_eq!(max_input_len(2 << 20), 2 << 20);
    }

    #[test]
    fn stats_come_from_the_final_lines_or_the_last_progress_line() {
        let log = "#1\tINITED\n#900\tpulse\nstat::number_of_executed_units: 1234\nstat::average_exec_per_sec:     13\n\
                   stat::new_units_added:          7\nstat::slowest_unit_time_sec:    0\nstat::peak_rss_mb:              88\n";
        assert_eq!(
            parse_stats(log),
            Stats {
                executions: 1234,
                new_units: 7,
                peak_rss_mb: 88
            }
        );
        assert_eq!(
            parse_stats("noise\n#10\tNEW cov: 1\n#42\tpulse cov: 1\nkilled\n").executions,
            42
        );
        assert_eq!(parse_stats("").executions, 0);
    }

    #[test]
    fn crash_message_picks_the_telling_lines() {
        let log = "INFO: seed\nthread 'main' panicked at crates/x.rs:1:2:\nindex out of bounds\n\
                   regia-fuzz finding in t: panic: index out of bounds\n==1== ERROR: libFuzzer: deadly signal\nmore\n";
        let message = crash_message(log);
        assert!(message.contains("panicked at crates/x.rs"), "{message}");
        assert!(message.contains("regia-fuzz finding in t"), "{message}");
        assert_eq!(crash_message("nothing here"), "no message in the log");
    }

    #[test]
    fn artifacts_are_sorted_by_prefix() {
        assert_eq!(artifact_kind("crash-abc"), Some("crash"));
        assert_eq!(artifact_kind("oom-abc"), Some("oom"));
        assert_eq!(artifact_kind("timeout-abc"), Some("timeout"));
        assert_eq!(artifact_kind("leak-abc"), Some("leak"));
        assert_eq!(artifact_kind("slow-unit-abc"), None);
        assert_eq!(artifact_kind("w0.log"), None);
    }

    #[test]
    fn stored_findings_reads_both_trees_and_skips_loose_files() {
        let root = regia_test_utils::unique_temp_dir("xtask-fuzz-stored");
        let base = findings_root(&root);
        for (rel, body) in [
            ("README.md", "not a finding"),
            ("t.one/aaa", "x"),
            ("t.one/.hidden", "x"),
            ("pending/t.two/bbb", "y"),
        ] {
            write_atomic(&base.join(rel), body.as_bytes()).expect("write");
        }
        let found = stored_findings(&root);
        let summary: Vec<(&str, bool)> = found.iter().map(|f| (f.target.as_str(), f.pending)).collect();
        assert_eq!(summary, vec![("t.two", true), ("t.one", false)]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_fails_a_finding_that_still_fails_and_a_stale_directory() {
        let root = regia_test_utils::unique_temp_dir("xtask-fuzz-replay");
        let base = findings_root(&root);
        for rel in ["t.good/aaa", "t.bad/bbb", "t.gone/ccc"] {
            write_atomic(&base.join(rel), b"input").expect("write");
        }
        let targets = [text_target("t.good", load_ok), text_target("t.bad", load_panics)];
        let failed = replay_findings(&stored_findings(&root), &targets);
        let names: Vec<&str> = failed.iter().map(|(f, _)| f.target.as_str()).collect();
        assert_eq!(names, vec!["t.bad", "t.gone"], "{failed:?}");
        assert!(failed[0].1.contains("boom"), "{}", failed[0].1);
        assert!(failed[1].1.contains("no target"), "{}", failed[1].1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn summary_sums_and_reads_back() {
        let root = regia_test_utils::unique_temp_dir("xtask-fuzz-summary");
        let path = root.join("summary.json");
        let mut rows = BTreeMap::new();
        for (name, executions) in [("a", 10u64), ("b", 32)] {
            let outcome = TargetOutcome {
                name: name.to_string(),
                stats: Stats {
                    executions,
                    ..Stats::default()
                },
                ..TargetOutcome::default()
            };
            rows.insert(name.to_string(), outcome.to_json());
        }
        let summary = summary_json(&rows, 90);
        assert_eq!(summary["executions"], 42);
        assert_eq!(summary["targets"], 2);
        write_atomic(&path, summary.to_string().as_bytes()).expect("write");
        assert_eq!(read_summary(&path).keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(read_summary(&root.join("missing.json")).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exit_code_follows_findings_and_abnormal_ends() {
        let clean = TargetOutcome::default();
        let mut found = TargetOutcome::default();
        found.findings.push(RunFinding {
            kind: "crash".into(),
            path: "p".into(),
            message: "m".into(),
        });
        let mut flaky = TargetOutcome::default();
        flaky.findings.push(RunFinding {
            kind: "flaky-timeout".into(),
            path: "p".into(),
            message: "m".into(),
        });
        assert_eq!(
            format!("{:?}", exit_for(std::slice::from_ref(&flaky))),
            format!("{:?}", ExitCode::SUCCESS)
        );
        // Only a slow call is excused: a crash or an out-of-memory that does not repeat is still a bug to look at.
        for kind in ["flaky-crash", "flaky-oom", "flaky-leak"] {
            let mut unsteady = TargetOutcome::default();
            unsteady.findings.push(RunFinding {
                kind: kind.into(),
                path: "p".into(),
                message: "m".into(),
            });
            assert_eq!(
                format!("{:?}", exit_for(std::slice::from_ref(&unsteady))),
                format!("{:?}", ExitCode::FAILURE),
                "{kind}"
            );
        }
        let mut abnormal = TargetOutcome::default();
        abnormal.abnormal.push("signal 9".into());
        assert_eq!(
            format!("{:?}", exit_for(std::slice::from_ref(&clean))),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!("{:?}", exit_for(&[clean.clone(), found])),
            format!("{:?}", ExitCode::FAILURE)
        );
        assert_eq!(
            format!("{:?}", exit_for(&[clean, abnormal])),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    /// libFuzzer's crash minimizer only tells two bugs apart by the sanitizer's dedup token, and prints none by default.
    /// libFuzzer's `-malloc_limit_mb` needs its malloc hooks, which it installs only with `-detect_leaks=1`; the registry
    /// runs with leak checks off, so the cap on one allocation is the sanitizer's (found by a probe loader asking for 3 GB
    /// under a 2048 MB limit: no finding).
    #[test]
    fn the_sanitizer_caps_one_allocation_at_the_malloc_limit() {
        let run = RunOptions {
            timeout_seconds: 120,
            rss_limit_mb: 4096,
            malloc_limit_mb: 1536,
            max_findings: 5,
            minimize_seconds: 180,
        };
        let env = sanitizer_env(&run);
        let (_, options) = env
            .iter()
            .find(|(k, _)| k == "ASAN_OPTIONS")
            .expect("ASAN_OPTIONS");
        assert!(options.contains("max_allocation_size_mb=1536"), "{options}");
        assert!(options.contains("allocator_may_return_null=0"), "{options}");
    }

    #[test]
    fn the_sanitizer_prints_the_token_the_minimizer_compares() {
        let env = sanitizer_env(&RunOptions {
            timeout_seconds: 120,
            rss_limit_mb: 4096,
            malloc_limit_mb: 2048,
            max_findings: 5,
            minimize_seconds: 180,
        });
        let (_, options) = env
            .iter()
            .find(|(k, _)| k == "ASAN_OPTIONS")
            .expect("ASAN_OPTIONS");
        assert!(options.contains("dedup_token_length=3"), "{options}");
    }

    #[test]
    fn a_failure_signature_names_the_failure_and_not_the_input() {
        let panic = |at: &str, message: &str| {
            format!(
                "thread 'main' panicked at {at}:\n{message}\nregia-fuzz finding in t: panic: {message}\n==1== ERROR: libFuzzer: deadly signal\n"
            )
        };
        assert_eq!(
            failure_signature(&panic("crates/x.rs:1:2", "index 5")),
            failure_signature(&panic("crates/x.rs:1:2", "index 900"))
        );
        assert_ne!(
            failure_signature(&panic("crates/x.rs:1:2", "index 5")),
            failure_signature(&panic("crates/y.rs:7:7", "index 5"))
        );
        // A caught panic earlier in the log does not name the failure that ended the run.
        let round_trip = "thread 'main' panicked at crates/caught.rs:3:3:\ncaught\n\
                          regia-fuzz finding in t: round_trip: differs\n==1== ERROR: libFuzzer: deadly signal\n";
        assert_eq!(failure_signature(round_trip), "round_trip");
        assert_eq!(
            failure_signature("==9==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x1\n"),
            "asan heap-buffer-overflow"
        );
        assert_eq!(
            failure_signature("==9== ERROR: libFuzzer: out-of-memory (malloc(99))\n"),
            "libfuzzer out-of-memory"
        );
        assert_eq!(failure_signature("nothing telling"), "");
    }

    /// The gate replays what `regia_agent`'s test cannot link (`pak.`, `cooked.`, `save.`, `import.`, `lua.`) here.
    #[test]
    fn every_committed_finding_replays_clean() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        assert!(
            findings_root(&root).join("README.md").is_file(),
            "the findings directory moved: this test would replay nothing"
        );
        let committed: Vec<StoredFinding> = stored_findings(&root)
            .into_iter()
            .filter(|f| !f.pending)
            .collect();
        let failed = replay_findings(&committed, &all_targets());
        assert!(failed.is_empty(), "{failed:?}");
    }

    /// The whole `run` path against a stand-in for the fuzz binary: libFuzzer's flags in, an artifact out, the artifact
    /// checked again, minimized and stored under its hash, with the executions counted.
    #[cfg(unix)]
    mod with_a_stand_in_fuzzer {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// A shell script that understands the libFuzzer flags this module passes. `$1 == crash` scripts write an artifact
        /// on a fuzzing run; replaying a file fails when the file contains `CRASH`; the minimizer writes `CRASH`.
        fn stand_in(dir: &Path, mode: &str) -> PathBuf {
            let path = dir.join("fake-fuzzer");
            let script = format!(
                r#"#!/bin/sh
prefix=""; out=""; minimize=0; runs=""; input=""
for a in "$@"; do
  case "$a" in
    -artifact_prefix=*) prefix="${{a#-artifact_prefix=}}" ;;
    -exact_artifact_path=*) out="${{a#-exact_artifact_path=}}" ;;
    -minimize_crash=1) minimize=1 ;;
    -runs=*) runs=1 ;;
    -*) ;;
    *) input="$a" ;;
  esac
done
if [ "$minimize" = 1 ]; then
  case "{mode}" in
    slowmin) exec sleep 60 ;;
    slowkids) sleep 60 & echo $! > "${{0%/*}}/child.pid"; wait; exit 0 ;;
    othermin) printf OTHER > "$out"; exit 0 ;;
    twobugs) cp "$input" "$out"; exit 0 ;;
  esac
  printf CRASH > "$out"; exit 0
fi
if [ -n "$runs" ]; then
  if grep -q OTHER "$input"; then echo "thread 'main' panicked at crates/other.rs:9:9:"; exit 77; fi
  if grep -q -e CRASH -e BUG "$input"; then echo "thread 'main' panicked at crates/x.rs:1:2:"; exit 77; fi
  exit 0
fi
echo "target is $REGIA_FUZZ_TARGET"
echo "$*" >> "${{0%/*}}/fuzz-args.log"
echo "stat::number_of_executed_units: 500"
echo "stat::new_units_added: 3"
echo "stat::peak_rss_mb: 12"
case "{mode}" in
  crash|slowmin|slowkids|othermin) printf 'xxCRASHxx' > "${{prefix}}crash-1"; exit 77 ;;
  twobugs) printf 'BUG-%s' "$(wc -l < "${{0%/*}}/fuzz-args.log" | tr -d ' ')" > "${{prefix}}crash-1"; exit 77 ;;
  flaky) printf 'no bug here' > "${{prefix}}timeout-1"; exit 70 ;;
  dies) exit 9 ;;
  *) sleep 1; exit 0 ;;
esac
"#
            );
            std::fs::write(&path, script).expect("write script");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        }

        fn fuzzer(root: &Path, mode: &str) -> Fuzzer {
            let run = RunOptions {
                timeout_seconds: 5,
                rss_limit_mb: 100,
                malloc_limit_mb: 100,
                max_findings: 1,
                minimize_seconds: 1,
            };
            Fuzzer::new(root, stand_in(root, mode), run, Vec::new())
        }

        #[test]
        fn a_crash_is_minimized_into_the_findings() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-crash");
            let outcome = fuzzer(&root, "crash")
                .run_target(&text_target("t.crash", load_ok), 60, 1)
                .expect("run");
            assert_eq!(outcome.stats.executions, 500);
            assert_eq!(outcome.stats.new_units, 3);
            assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
            let finding = &outcome.findings[0];
            assert_eq!(finding.kind, "crash");
            assert_eq!(
                finding.path,
                format!("fuzz/findings/t.crash/{}", content_hash(b"CRASH"))
            );
            assert_eq!(std::fs::read(root.join(&finding.path)).expect("stored"), b"CRASH");
            assert!(finding.message.contains("panicked at"), "{}", finding.message);
            let _ = std::fs::remove_dir_all(root);
        }

        /// A minimizer that outlasts its budget is ended, and the input as found is the finding.
        #[test]
        fn a_minimizer_past_its_budget_leaves_the_original_input() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-slowmin");
            let outcome = fuzzer(&root, "slowmin")
                .run_target(&text_target("t.slowmin", load_ok), 60, 1)
                .expect("run");
            assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
            assert_eq!(
                outcome.findings[0].path,
                format!("fuzz/findings/t.slowmin/{}", content_hash(b"xxCRASHxx"))
            );
            let _ = std::fs::remove_dir_all(root);
        }

        /// `ps` says whether a process is gone; a zombie is gone too. Polled as a guard only: the child ends at once.
        fn process_is_gone(pid: &str) -> bool {
            (0..500).any(|_| {
                let state = Command::new("ps")
                    .args(["-o", "stat=", "-p", pid])
                    .output()
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .unwrap_or_default();
                let gone = state.is_empty() || state.starts_with('Z');
                if !gone {
                    std::thread::sleep(Duration::from_millis(10));
                }
                gone
            })
        }

        /// libFuzzer runs each minimizer step as a child process, so ending only the parent would leave a step running
        /// that can still write the output file.
        #[test]
        fn a_minimizer_past_its_budget_leaves_no_child_running() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-slowkids");
            let outcome = fuzzer(&root, "slowkids")
                .run_target(&text_target("t.slowkids", load_ok), 60, 1)
                .expect("run");
            assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
            let pid = std::fs::read_to_string(root.join("child.pid")).expect("the step started");
            let gone = process_is_gone(pid.trim());
            if !gone {
                let _ = Command::new("kill").args(["-s", "KILL", pid.trim()]).status();
            }
            assert!(gone, "minimizer step {} still runs", pid.trim());
            let _ = std::fs::remove_dir_all(root);
        }

        /// A shrink that lands on a different failure must not replace the input that was found.
        #[test]
        fn a_minimized_input_with_another_failure_is_not_the_finding() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-othermin");
            let outcome = fuzzer(&root, "othermin")
                .run_target(&text_target("t.othermin", load_ok), 60, 1)
                .expect("run");
            assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
            assert_eq!(
                outcome.findings[0].path,
                format!("fuzz/findings/t.othermin/{}", content_hash(b"xxCRASHxx"))
            );
            let _ = std::fs::remove_dir_all(root);
        }

        fn fuzz_runs(root: &Path) -> Vec<String> {
            std::fs::read_to_string(root.join("fuzz-args.log"))
                .expect("the fuzzer ran")
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// A finding stored in this run crashes as a seed, so a later round must not load it; committed ones are seeds once.
        #[test]
        fn only_the_first_round_seeds_from_the_findings() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-rounds");
            write_atomic(&findings_root(&root).join("t.rounds").join("old"), b"fixed").expect("write");
            let mut f = fuzzer(&root, "crash");
            f.run.max_findings = 5;
            f.run_target(&text_target("t.rounds", load_ok), 60, 1)
                .expect("run");
            let runs = fuzz_runs(&root);
            assert_eq!(runs.len(), 2, "{runs:?}");
            assert!(runs[0].contains("findings/t.rounds"), "{}", runs[0]);
            assert!(!runs[1].contains("findings/t.rounds"), "{}", runs[1]);
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn a_run_goes_on_past_one_bug_up_to_the_limit() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-twobugs");
            let mut f = fuzzer(&root, "twobugs");
            f.run.max_findings = 2;
            let outcome = f
                .run_target(&text_target("t.twobugs", load_ok), 60, 1)
                .expect("run");
            let names: Vec<&str> = outcome.findings.iter().map(|x| x.path.as_str()).collect();
            assert_eq!(
                names,
                vec![
                    format!("fuzz/findings/t.twobugs/{}", content_hash(b"BUG-1")),
                    format!("fuzz/findings/t.twobugs/{}", content_hash(b"BUG-2")),
                ],
                "{outcome:?}"
            );
            assert_eq!(fuzz_runs(&root).len(), 2);
            let _ = std::fs::remove_dir_all(root);
        }

        /// A run that executed nothing proves nothing.
        #[test]
        fn a_run_of_no_inputs_is_abnormal() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-noruns");
            let outcome = fuzzer(&root, "clean")
                .run_target(&text_target("t.noruns", load_ok), 0, 1)
                .expect("run");
            assert_eq!(outcome.stats.executions, 0);
            assert_eq!(outcome.abnormal.len(), 1, "{outcome:?}");
            let _ = std::fs::remove_dir_all(root);
        }

        /// `--resume` answers for the whole summary, not only for the targets it ran this time.
        #[test]
        fn resume_still_fails_on_a_finding_from_the_earlier_part() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-resume");
            let summary = root.join("summary.json");
            let mut earlier = TargetOutcome {
                name: "t.a".to_string(),
                ..TargetOutcome::default()
            };
            earlier.findings.push(RunFinding {
                kind: "crash".into(),
                path: "p".into(),
                message: "m".into(),
            });
            let rows = BTreeMap::from([("t.a".to_string(), earlier.to_json())]);
            write_atomic(&summary, summary_json(&rows, 1).to_string().as_bytes()).expect("write");
            let both = [text_target("t.a", load_ok), text_target("t.b", load_ok)];
            let code = run_all(&fuzzer(&root, "clean"), &both, 1, 1, &summary, true).expect("resume");
            assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::FAILURE));
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn an_input_that_does_not_fail_again_stays_out_of_the_findings() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-flaky");
            let mut f = fuzzer(&root, "flaky");
            f.run.max_findings = 1;
            let outcome = f
                .run_target(&text_target("t.flaky", load_ok), 60, 1)
                .expect("run");
            assert_eq!(outcome.findings[0].kind, "flaky-timeout");
            assert!(outcome.findings[0].path.starts_with("fuzz/logs/t.flaky/flaky-"));
            assert!(!findings_root(&root).join("t.flaky").exists());
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn a_process_that_dies_without_an_artifact_is_abnormal() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-dies");
            let outcome = fuzzer(&root, "dies")
                .run_target(&text_target("t.dies", load_ok), 60, 1)
                .expect("run");
            assert!(outcome.findings.is_empty());
            assert_eq!(outcome.abnormal.len(), 1, "{outcome:?}");
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn a_clean_run_seeds_the_corpus_and_ends_on_time() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-clean");
            let outcome = fuzzer(&root, "clean")
                .run_target(&text_target("t.clean", load_ok), 1, 2)
                .expect("run");
            assert!(
                outcome.findings.is_empty() && outcome.abnormal.is_empty(),
                "{outcome:?}"
            );
            assert_eq!(outcome.stats.executions, 1000, "two workers");
            assert_eq!(outcome.corpus_files, 1);
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn run_all_writes_the_summary_and_resume_skips_what_it_holds() {
            let root = regia_test_utils::unique_temp_dir("xtask-fuzz-all");
            let f = fuzzer(&root, "clean");
            let summary = root.join("summary.json");
            let a = [text_target("t.a", load_ok)];
            // The exit code is not what this test checks; the summary file below is.
            let _ = run_all(&f, &a, 1, 1, &summary, false).expect("run all");
            assert_eq!(read_summary(&summary).len(), 1);
            let both = [text_target("t.a", load_ok), text_target("t.b", load_ok)];
            let _ = run_all(&f, &both, 1, 2, &summary, true).expect("resume");
            let rows = read_summary(&summary);
            assert_eq!(
                rows.keys().map(String::as_str).collect::<Vec<_>>(),
                vec!["t.a", "t.b"]
            );
            assert_eq!(summary_json(&rows, 1)["executions"], 1000);
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
