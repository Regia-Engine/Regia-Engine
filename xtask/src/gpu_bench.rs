// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask gpu-bench` — v0.2.0 evidence runs on one card (ADR-1360).
//!
//! Runs a prebuilt `do27_chase` (this builds nothing) headless on the card `REGIA_GPU_INDEX`
//! selects, once per scenario of a suite, and writes `gpu-bench.json` and `gpu-bench.md` under
//! `--out`: frame-time p50 / p95 / p99, the GPU pass timings the chase records per frame, the
//! adapter name, the driver and the commit. Every launch holds `flock /tmp/regia-gpu-<n>.lock`
//! and then the pre-rename `/tmp/ferrite-gpu-<n>.lock` (ADR-8670) so two runs never share a card,
//! and GPU 6 is refused.
//!
//! Suites:
//!
//! - `bindless`: the texture-heavy chase with the census on. The renderer has no bindless path yet,
//!   so this is the binding-model baseline a bindless build is compared against.
//! - `gi`: GI terms on, probes off, probes and SSGI off.
//! - `baked-light`: baked GI off and on. Needs a binary that has the `--baked-gi` flag; without
//!   it every row is `not available` and the run still succeeds.
//! - `features`: every opt-in renderer feature (`do27_chase --on`, [`crate::gpu_bench_features`])
//!   against the plain frame, per tier (`--tiers`), interleaved in `--rounds` rounds on the one
//!   card, with the renderer's GPU memory and the heap allocations per frame of each run. Writes
//!   `features.md` and `features.json` (cost p50/p99 per feature and tier, allocations, VRAM)
//!   beside the usual files. Needs a binary with the `--on` flag.

use anyhow::{Context, Result, bail};
use clap::Args;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use crate::sweep::{Metrics, parse_metrics, write_atomic};

/// The card the farm never benches on (Vulkan order, `docs/06-testing-and-ci.md`).
pub const FORBIDDEN_GPU_INDEX: u32 = 6;

/// The flag a `do27_chase` build has when it can install a baked GI volume.
pub const BAKED_FLAG: &str = "--baked-gi";

/// Arguments for `cargo xtask gpu-bench`.
#[derive(Args, Debug)]
pub struct BenchArgs {
    /// Suite: `bindless`, `gi`, `baked-light` or `features`.
    #[arg(long)]
    pub suite: String,
    /// Directory for `gpu-bench.json`, `gpu-bench.md` and per-scenario files.
    #[arg(long)]
    pub out: PathBuf,
    /// Prebuilt `do27_chase`. Default: `target/release/do27_chase` (honors `CARGO_TARGET_DIR`).
    #[arg(long)]
    pub bin: Option<PathBuf>,
    /// DO27 content directory. When omitted, `REGIA_DO27_CONTENT` is used.
    #[arg(long)]
    pub content: Option<PathBuf>,
    /// Map the chase loads.
    #[arg(long, default_value = "switchbacks")]
    pub map: String,
    /// Measured frames per scenario.
    #[arg(long, default_value_t = 600)]
    pub frames: u32,
    /// Warm-up frames per scenario.
    #[arg(long, default_value_t = 60)]
    pub warmup: u32,
    /// Per-launch ceiling in seconds (`timeout`).
    #[arg(long, default_value_t = 900)]
    pub timeout_seconds: u64,
    /// Date stamped into the JSON. Default is `date +%F`.
    #[arg(long)]
    pub date: Option<String>,
    /// Write the planned rows without launching anything or needing a card.
    #[arg(long)]
    pub dry_run: bool,
    /// `features` suite: the `LOOK_R2_TIER` names to measure, comma-separated.
    #[arg(long, default_value = "desktop-high,desktop-ultra")]
    pub tiers: String,
    /// `features` suite: the `do27_chase --on` names, comma-separated (default: every one).
    #[arg(long)]
    pub features: Option<String>,
    /// `features` suite: interleaved rounds (each runs the plain frame and every feature once
    /// per tier); the cost is the median of the rounds' differences.
    #[arg(long, default_value_t = 3)]
    pub rounds: u32,
    /// `features` suite: also toggle every feature on these `regia bench` scenes,
    /// comma-separated (e.g. `synthetic_atrium,toy_car,a_beautiful_game`).
    #[arg(long)]
    pub bench_scenes: Option<String>,
    /// `features` suite: the built `regia` binary that runs `--bench-scenes`. Default:
    /// `target/release/regia` (honors `CARGO_TARGET_DIR`).
    #[arg(long)]
    pub regia: Option<PathBuf>,
    /// `features` suite: where `regia bench --fetch` put the glTF scenes.
    #[arg(long)]
    pub assets: Option<PathBuf>,
    /// `features` suite: run only `--bench-scenes`, not the DO27 chase (no content needed).
    #[arg(long)]
    pub skip_chase: bool,
}

/// A suite of scenarios.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suite {
    /// Binding-model baseline.
    Bindless,
    /// GI terms.
    Gi,
    /// Baked GI against the live terms.
    BakedLight,
    /// Opt-in renderer features against the plain frame.
    Features,
}

impl Suite {
    /// Name on the command line and in the output.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Suite::Bindless => "bindless",
            Suite::Gi => "gi",
            Suite::BakedLight => "baked-light",
            Suite::Features => "features",
        }
    }

    /// Parses `--suite`.
    ///
    /// # Errors
    ///
    /// The name is not a suite.
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim() {
            "bindless" => Ok(Suite::Bindless),
            "gi" => Ok(Suite::Gi),
            "baked-light" => Ok(Suite::BakedLight),
            "features" => Ok(Suite::Features),
            other => bail!("unknown suite `{other}` (known: bindless, gi, baked-light, features)"),
        }
    }

    /// The flag the binary must contain for this suite to run, when it needs one.
    #[must_use]
    pub fn required_flag(self) -> Option<&'static str> {
        match self {
            Suite::BakedLight => Some(BAKED_FLAG),
            Suite::Features => Some(crate::gpu_bench_features::CAPABILITY_FLAG),
            _ => None,
        }
    }

    /// Scenarios in run order: a name and the extra chase arguments.
    #[must_use]
    pub fn scenarios(self) -> &'static [(&'static str, &'static [&'static str])] {
        match self {
            Suite::Bindless => &[("census", &["--census"])],
            Suite::Gi => &[
                ("gi-full", &[]),
                ("gi-no-probes", &["--off", "gi-probes"]),
                ("gi-none", &["--off", "gi-probes", "--off", "ssgi"]),
            ],
            Suite::BakedLight => &[("live-gi", &[]), ("baked-gi", &["--baked-gi"])],
            // Planned per tier, feature and round: `crate::gpu_bench_features::plan`.
            Suite::Features => &[],
        }
    }
}

/// Reads and checks `REGIA_GPU_INDEX`.
///
/// # Errors
///
/// The value is unset, not a number, or the forbidden card.
pub fn validate_gpu_index(value: Option<&str>) -> Result<u32> {
    let Some(text) = value.map(str::trim).filter(|text| !text.is_empty()) else {
        bail!("REGIA_GPU_INDEX is not set: pick the card explicitly (see `do27_chase --list-gpus`)");
    };
    let index: u32 = text
        .parse()
        .with_context(|| format!("REGIA_GPU_INDEX `{text}` is not a number"))?;
    if index == FORBIDDEN_GPU_INDEX {
        bail!("REGIA_GPU_INDEX={index} is reserved and is never benched on");
    }
    Ok(index)
}

/// The lock file one card's runs take first (ADR-8670).
#[must_use]
pub fn lock_path(index: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/regia-gpu-{index}.lock"))
}

/// The card's pre-rename lock file, taken after [`lock_path`] (ADR-8670). Tools outside the repo
/// (the farm, older checkouts) still take only this one; the fixed order (new lock, then old
/// lock) means a run never holds the old lock while it waits for the new one, so it cannot
/// deadlock with them.
#[must_use]
pub fn legacy_lock_path(index: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/ferrite-gpu-{index}.lock"))
}

/// The arguments after `flock` that hold both of a card's locks, in order, around the rest of
/// the command line: `<lock_path> flock <legacy_lock_path>`.
#[must_use]
pub fn flock_args(index: u32) -> [std::ffi::OsString; 3] {
    [
        lock_path(index).into_os_string(),
        "flock".into(),
        legacy_lock_path(index).into_os_string(),
    ]
}

/// The shell prefix that holds both of a card's locks, in order, for the command after it:
/// `flock /tmp/regia-gpu-<n>.lock flock /tmp/ferrite-gpu-<n>.lock` (ADR-8670).
#[must_use]
pub fn flock_shell(index: u32) -> String {
    format!(
        "flock {} flock {}",
        lock_path(index).display(),
        legacy_lock_path(index).display()
    )
}

/// The environment variable a caller sets to the card's index when it already holds that card's
/// locks for the whole run (`flock /tmp/regia-gpu-N.lock flock /tmp/ferrite-gpu-N.lock env
/// REGIA_GPU_LOCK_HELD=N xtask gpu-bench ...`): each launch then skips its own `flock`, which
/// would wait on the caller forever, and nothing else gets the card between two launches of an
/// interleaved suite.
pub const LOCK_HELD_ENV: &str = "REGIA_GPU_LOCK_HELD";

/// Whether the launches must take the card's lock themselves: unless `held` (the value of
/// [`LOCK_HELD_ENV`]) names this very card.
#[must_use]
pub fn launch_takes_lock(index: u32, held: Option<&str>) -> bool {
    held.and_then(|text| text.trim().parse::<u32>().ok()) != Some(index)
}

/// One planned launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Scenario name.
    pub scenario: String,
    /// Directory of the scenario's files.
    pub dir: PathBuf,
    /// Arguments after the binary.
    pub args: Vec<String>,
    /// Extra environment of the launch.
    pub env: Vec<(String, String)>,
}

/// The arguments every launch shares: scenario name, map, frame counts, summary and CSV in `dir`.
#[must_use]
pub fn base_args(args: &BenchArgs, scenario_label: &str, dir: &Path) -> Vec<String> {
    vec![
        "--scenario".to_owned(),
        scenario_label.to_owned(),
        "--map".to_owned(),
        args.map.clone(),
        "--frames".to_owned(),
        args.frames.to_string(),
        "--warmup".to_owned(),
        args.warmup.to_string(),
        "--summary".to_owned(),
        dir.join("summary.json").display().to_string(),
        "--csv".to_owned(),
        dir.join("frames.csv").display().to_string(),
    ]
}

/// Where a scenario's files go.
#[must_use]
pub fn scenario_dir(out: &Path, suite: Suite, scenario: &str) -> PathBuf {
    out.join(suite.name()).join(scenario)
}

/// The launches of `suite`.
///
/// # Errors
///
/// The `features` suite names an unknown tier or feature, or zero rounds.
pub fn plan(suite: Suite, args: &BenchArgs) -> Result<Vec<Job>> {
    if suite == Suite::Features {
        return crate::gpu_bench_features::plan(args);
    }
    Ok(suite
        .scenarios()
        .iter()
        .map(|(scenario, extra)| {
            let dir = scenario_dir(&args.out, suite, scenario);
            let mut list = base_args(args, &format!("bench-{}-{scenario}", suite.name()), &dir);
            list.extend(extra.iter().map(|arg| (*arg).to_owned()));
            Job {
                scenario: (*scenario).to_owned(),
                dir,
                args: list,
                env: Vec::new(),
            }
        })
        .collect())
}

/// Adapter and driver read from the chase's `selected:` line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Adapter name.
    pub name: String,
    /// Driver name and version.
    pub driver: String,
}

/// Finds `selected: NAME (pci BUS, driver D INFO; SELECTION)` in a log.
#[must_use]
pub fn parse_selected(log: &str) -> Option<AdapterInfo> {
    let line = log.lines().find_map(|line| line.strip_prefix("selected: "))?;
    let (name, rest) = line.split_once(" (pci ")?;
    let driver = rest.split_once(", driver ")?.1;
    let driver = driver.split(';').next().unwrap_or(driver).trim();
    Some(AdapterInfo {
        name: name.trim().to_owned(),
        driver: driver.to_owned(),
    })
}

/// The `frames.csv` columns that are GPU pass timings, in table order.
pub const PASS_COLUMNS: &[&str] = &[
    "gpu_frame",
    "shadows",
    "clusters",
    "prepass",
    "cull",
    "ssao",
    "forward",
    "ssgi",
    "ssr",
    "atmosphere",
    "clouds",
    "fog",
    "taau",
    "post",
    "rocks",
    "terrain_forward",
];

/// Percentiles of one GPU pass, ms.
#[derive(Clone, Debug, PartialEq)]
pub struct PassStat {
    /// Column name.
    pub name: String,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Percentiles of every pass column that has a nonzero sample, in `PASS_COLUMNS` order.
/// A pass the build does not run records 0 every frame and is left out.
#[must_use]
pub fn pass_stats(csv: &str) -> Vec<PassStat> {
    let mut lines = csv.lines();
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let columns: Vec<&str> = header.split(',').collect();
    let mut samples: Vec<Vec<f64>> = vec![Vec::new(); columns.len()];
    for line in lines {
        for (slot, field) in samples.iter_mut().zip(line.split(',')) {
            if let Ok(value) = field.trim().parse::<f64>() {
                slot.push(value);
            }
        }
    }
    let mut out = Vec::new();
    for name in PASS_COLUMNS {
        let Some(index) = columns.iter().position(|column| column == name) else {
            continue;
        };
        let mut values = std::mem::take(&mut samples[index]);
        if !values.iter().any(|value| *value > 0.0) {
            continue;
        }
        values.sort_by(f64::total_cmp);
        out.push(PassStat {
            name: (*name).to_owned(),
            p50: percentile(&values, 0.5),
            p95: percentile(&values, 0.95),
            p99: percentile(&values, 0.99),
        });
    }
    out
}

/// One scenario's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Scenario name.
    pub scenario: String,
    /// `pass`, `fail`, `planned` or `not available`.
    pub status: String,
    /// Process exit code, when it launched.
    pub exit_code: Option<i32>,
    /// Frame times from `summary.json`.
    pub metrics: Metrics,
    /// GPU pass timings from `frames.csv`.
    pub passes: Vec<PassStat>,
    /// GPU memory and allocations from `memory.json`, when the run wrote one.
    pub memory: Option<crate::gpu_bench_features::RunMemory>,
    /// Why the row has this status.
    pub note: String,
}

impl Row {
    fn without_run(scenario: &str, status: &str, note: &str) -> Self {
        Row {
            scenario: scenario.to_owned(),
            status: status.to_owned(),
            exit_code: None,
            metrics: Metrics::default(),
            passes: Vec::new(),
            memory: None,
            note: note.to_owned(),
        }
    }
}

/// What the run is stamped with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    /// Date.
    pub date: String,
    /// Short commit hash.
    pub commit: String,
    /// Adapter name, empty when no scenario ran.
    pub adapter: String,
    /// Driver, empty when no scenario ran.
    pub driver: String,
    /// `REGIA_GPU_INDEX`, when a card was selected.
    pub gpu_index: Option<u32>,
}

fn ms(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.3}"))
}

fn gpu_frame_p50(row: &Row) -> String {
    ms(row
        .passes
        .iter()
        .find(|pass| pass.name == "gpu_frame")
        .map(|pass| pass.p50))
}

/// The markdown report: a stamp block, a frame-time table, then one pass table per run scenario.
#[must_use]
pub fn markdown(suite: Suite, stamp: &Stamp, rows: &[Row]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# gpu-bench: {}\n", suite.name());
    let _ = writeln!(out, "- date: {}", stamp.date);
    let _ = writeln!(out, "- commit: {}", stamp.commit);
    let _ = writeln!(
        out,
        "- adapter: {}",
        if stamp.adapter.is_empty() {
            "unknown"
        } else {
            &stamp.adapter
        }
    );
    let _ = writeln!(
        out,
        "- driver: {}",
        if stamp.driver.is_empty() {
            "unknown"
        } else {
            &stamp.driver
        }
    );
    if let Some(index) = stamp.gpu_index {
        let _ = writeln!(out, "- REGIA_GPU_INDEX: {index}");
    }
    out.push_str("\n| scenario | status | p50 ms | p95 ms | p99 ms | gpu_frame p50 ms | note |\n");
    out.push_str("|---|---|---|---|---|---|---|\n");
    for row in rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            row.scenario,
            row.status,
            ms(row.metrics.p50_ms),
            ms(row.metrics.p95_ms),
            ms(row.metrics.p99_ms),
            gpu_frame_p50(row),
            row.note.replace('|', "/"),
        );
    }
    for row in rows.iter().filter(|row| !row.passes.is_empty()) {
        let _ = writeln!(out, "\n## GPU passes: {}\n", row.scenario);
        out.push_str("| pass | p50 ms | p95 ms | p99 ms |\n|---|---|---|---|\n");
        for pass in &row.passes {
            let _ = writeln!(
                out,
                "| {} | {:.3} | {:.3} | {:.3} |",
                pass.name, pass.p50, pass.p95, pass.p99
            );
        }
    }
    out
}

/// The JSON report, pretty-printed with a final newline.
#[must_use]
pub fn json(suite: Suite, stamp: &Stamp, rows: &[Row]) -> String {
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let passes: Vec<serde_json::Value> = row
                .passes
                .iter()
                .map(|pass| {
                    serde_json::json!({
                        "pass": pass.name,
                        "p50_ms": pass.p50,
                        "p95_ms": pass.p95,
                        "p99_ms": pass.p99,
                    })
                })
                .collect();
            serde_json::json!({
                "scenario": row.scenario,
                "status": row.status,
                "exit_code": row.exit_code,
                "p50_ms": row.metrics.p50_ms,
                "p95_ms": row.metrics.p95_ms,
                "p99_ms": row.metrics.p99_ms,
                "hitches": row.metrics.hitches,
                "passes": passes,
                "memory": row.memory.as_ref().map(crate::gpu_bench_features::RunMemory::to_json),
                "note": row.note,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "schema": 1,
        "suite": suite.name(),
        "date": stamp.date,
        "commit": stamp.commit,
        "adapter": stamp.adapter,
        "driver": stamp.driver,
        "gpu_index": stamp.gpu_index,
        "rows": rows,
    });
    let mut text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

/// True when the file at `path` contains `needle` (a flag literal the build embeds).
///
/// # Errors
///
/// The file could not be read.
pub fn binary_contains(path: &Path, needle: &str) -> Result<bool> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let needle = needle.as_bytes();
    Ok(!needle.is_empty() && bytes.windows(needle.len()).any(|window| window == needle))
}

pub(crate) fn command_text(program: &str, args: &[&str], dir: &Path) -> Option<String> {
    let mut command = Command::new(program);
    // A missing directory (a copied xtask's compile-time workspace) would fail the spawn and
    // lose the stamp (`nvidia-smi` driver, date): run where this process runs instead.
    if let Some(dir) = crate::sweep::launch_dir(dir) {
        command.current_dir(dir);
    }
    command
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn default_bin(workspace: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"))
        .join("release/do27_chase")
}

/// Runs the suite and writes `gpu-bench.json` and `gpu-bench.md` under `--out`.
///
/// # Errors
///
/// The suite is unknown, `--frames` is zero, the card is unset or forbidden, the binary or
/// content is missing, or an output file could not be written. A failed scenario is a `fail`
/// row and a failing exit code, not an error.
pub fn run(args: &BenchArgs, workspace: &Path) -> Result<ExitCode> {
    let _span = tracing::info_span!("gpu_bench").entered();
    let suite = Suite::parse(&args.suite)?;
    if args.frames == 0 {
        bail!("--frames must be at least 1");
    }
    let jobs = plan(suite, args)?;
    let mut stamp = Stamp {
        date: args
            .date
            .clone()
            .filter(|date| !date.is_empty())
            .or_else(|| command_text("date", &["+%F"], workspace))
            .unwrap_or_else(|| "unknown-date".to_owned()),
        commit: command_text("git", &["rev-parse", "--short", "HEAD"], workspace)
            .unwrap_or_else(|| "unknown".to_owned()),
        ..Stamp::default()
    };
    let scene_jobs = if suite == Suite::Features {
        crate::gpu_bench_features::plan_scenes(args)?
    } else {
        Vec::new()
    };
    let regia = args.regia.clone().unwrap_or_else(|| {
        std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("target"))
            .join("release/regia")
    });
    // Before any launch: a missing `regia` must not end hours of chase runs without their tables.
    if !args.dry_run && !scene_jobs.is_empty() && !regia.is_file() {
        bail!(
            "{} is not a built regia (this command builds nothing: `cargo build --release -p regia_cli --bin regia`)",
            regia.display()
        );
    }
    let mut rows = Vec::new();
    if args.dry_run {
        for job in jobs.iter().chain(&scene_jobs) {
            rows.push(Row::without_run(&job.scenario, "planned", "dry-run"));
        }
    } else if jobs.is_empty() {
        let index = validate_gpu_index(std::env::var("REGIA_GPU_INDEX").ok().as_deref())?;
        stamp.gpu_index = Some(index);
    } else {
        let index = validate_gpu_index(std::env::var("REGIA_GPU_INDEX").ok().as_deref())?;
        stamp.gpu_index = Some(index);
        let bin = args.bin.clone().unwrap_or_else(|| default_bin(workspace));
        if !bin.is_file() {
            bail!(
                "{} is not a built do27_chase (this command builds nothing: `cargo build --release -p lookdev --bin do27_chase`)",
                bin.display()
            );
        }
        let supported = match suite.required_flag() {
            Some(flag) => binary_contains(&bin, flag)?,
            None => true,
        };
        if supported {
            let content = args
                .content
                .clone()
                .or_else(|| std::env::var_os("REGIA_DO27_CONTENT").map(PathBuf::from))
                .filter(|path| path.is_dir());
            let Some(content) = content else {
                bail!("no DO27 content directory: pass --content or set REGIA_DO27_CONTENT");
            };
            let (done, adapter) = execute(args, suite, index, workspace, &bin, &content, &jobs)?;
            rows = done;
            if let Some(info) = adapter {
                stamp.adapter = info.name;
                stamp.driver = info.driver;
            }
        } else {
            let flag = suite.required_flag().unwrap_or_default();
            for job in &jobs {
                rows.push(Row::without_run(
                    &job.scenario,
                    "not available",
                    &format!("this build lacks the suite's `{flag}` flag"),
                ));
            }
        }
    }
    if !args.dry_run && !scene_jobs.is_empty() {
        let index = validate_gpu_index(std::env::var("REGIA_GPU_INDEX").ok().as_deref())?;
        let (scene_rows, adapter) =
            crate::gpu_bench_features::execute_scenes(args, index, workspace, &regia, &scene_jobs)?;
        rows.extend(scene_rows);
        if stamp.adapter.is_empty()
            && let Some(info) = adapter
        {
            stamp.adapter = info.name;
            stamp.driver = info.driver;
        }
    }
    write_atomic(&args.out.join("gpu-bench.json"), &json(suite, &stamp, &rows))?;
    write_atomic(&args.out.join("gpu-bench.md"), &markdown(suite, &stamp, &rows))?;
    if suite == Suite::Features {
        let costs = crate::gpu_bench_features::summarize(&rows);
        write_atomic(
            &args.out.join("features.md"),
            &crate::gpu_bench_features::markdown(&stamp, &costs),
        )?;
        write_atomic(
            &args.out.join("features.json"),
            &crate::gpu_bench_features::json(&stamp, &costs),
        )?;
    }
    let failed = rows.iter().filter(|row| row.status == "fail").count();
    println!(
        "gpu-bench {}: {} rows, {failed} failed, wrote {}",
        suite.name(),
        rows.len(),
        args.out.display()
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn execute(
    args: &BenchArgs,
    _suite: Suite,
    index: u32,
    workspace: &Path,
    bin: &Path,
    content: &Path,
    jobs: &[Job],
) -> Result<(Vec<Row>, Option<AdapterInfo>)> {
    let mut rows = Vec::new();
    let mut adapter = None;
    let timeout = args.timeout_seconds.max(1).to_string();
    let take_lock = launch_takes_lock(index, std::env::var(LOCK_HELD_ENV).ok().as_deref());
    for job in jobs {
        let dir = &job.dir;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let _ = fs::remove_file(dir.join("summary.json"));
        let _ = fs::remove_file(dir.join("frames.csv"));
        let _ = fs::remove_file(dir.join("memory.json"));
        let log_path = dir.join("run.log");
        let log = fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
        let err = log.try_clone()?;
        let mut command = if take_lock {
            let mut flock = Command::new("flock");
            flock.args(flock_args(index)).arg("timeout");
            flock
        } else {
            Command::new("timeout")
        };
        // A copied xtask on a test box has no compile-time workspace: the host inherits this
        // process's working directory instead (`sweep::launch_dir`).
        if let Some(dir) = crate::sweep::launch_dir(workspace) {
            command.current_dir(dir);
        }
        let status = command
            .arg("--signal=TERM")
            .arg(&timeout)
            .arg(bin)
            .args(&job.args)
            .env("REGIA_DO27_CONTENT", content)
            .envs(job.env.iter().map(|(key, value)| (key, value)))
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .status();
        let exit_code = status.ok().and_then(|status| status.code());
        let log_text = fs::read_to_string(&log_path).unwrap_or_default();
        if adapter.is_none() {
            adapter = parse_selected(&log_text);
        }
        let metrics = fs::read_to_string(dir.join("summary.json"))
            .ok()
            .and_then(|text| parse_metrics(&text).ok());
        let passes = fs::read_to_string(dir.join("frames.csv"))
            .map(|text| pass_stats(&text))
            .unwrap_or_default();
        let memory = fs::read_to_string(dir.join("memory.json"))
            .ok()
            .and_then(|text| crate::gpu_bench_features::RunMemory::parse(&text));
        let (status, note) = judge(exit_code, metrics.as_ref());
        rows.push(Row {
            scenario: job.scenario.clone(),
            status,
            exit_code,
            metrics: metrics.unwrap_or_default(),
            passes,
            memory,
            note,
        });
    }
    Ok((rows, adapter))
}

/// Status and note for a finished scenario.
#[must_use]
pub fn judge(exit_code: Option<i32>, metrics: Option<&Metrics>) -> (String, String) {
    match (exit_code, metrics) {
        (Some(124), _) => ("fail".to_owned(), "timed out".to_owned()),
        (None, _) => (
            "fail".to_owned(),
            "did not launch (is flock installed?)".to_owned(),
        ),
        (Some(code), _) if code != 0 => ("fail".to_owned(), format!("exit {code}")),
        (Some(_), None) => ("fail".to_owned(), "no readable summary.json".to_owned()),
        (Some(_), Some(_)) => ("pass".to_owned(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_read_even_when_the_workspace_is_not_on_this_machine() {
        let gone = Path::new(env!("CARGO_MANIFEST_DIR")).join("never-created-workspace-dir");
        // cargo sets CARGO for the tests it runs: a program present on every host.
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let text = command_text(&cargo, &["--version"], &gone).unwrap_or_default();
        assert!(text.starts_with("cargo "), "{text:?}");
    }

    fn args(suite: &str, out: &Path) -> BenchArgs {
        BenchArgs {
            suite: suite.to_owned(),
            out: out.to_path_buf(),
            bin: None,
            content: None,
            map: "switchbacks".to_owned(),
            frames: 100,
            warmup: 10,
            timeout_seconds: 60,
            date: Some("2026-10-21".to_owned()),
            dry_run: true,
            tiers: "desktop-high,desktop-ultra".to_owned(),
            features: None,
            rounds: 3,
            bench_scenes: None,
            regia: None,
            assets: None,
            skip_chase: false,
        }
    }

    #[test]
    fn suite_names_parse_and_unknown_is_an_error() {
        assert_eq!(Suite::parse("bindless").ok(), Some(Suite::Bindless));
        assert_eq!(Suite::parse(" gi ").ok(), Some(Suite::Gi));
        assert_eq!(Suite::parse("baked-light").ok(), Some(Suite::BakedLight));
        assert!(Suite::parse("lightmaps").is_err());
        assert_eq!(Suite::BakedLight.required_flag(), Some(BAKED_FLAG));
        assert_eq!(Suite::Gi.required_flag(), None);
    }

    #[test]
    fn gpu_index_must_be_set_numeric_and_not_six() {
        assert_eq!(validate_gpu_index(Some("2")).ok(), Some(2));
        assert_eq!(validate_gpu_index(Some(" 5 ")).ok(), Some(5));
        assert!(validate_gpu_index(None).is_err());
        assert!(validate_gpu_index(Some("")).is_err());
        assert!(validate_gpu_index(Some("abc")).is_err());
        assert!(validate_gpu_index(Some("6")).is_err());
        assert_eq!(lock_path(3), PathBuf::from("/tmp/regia-gpu-3.lock"));
        // ADR-8670: the pre-rename lock too, always second.
        assert_eq!(
            flock_shell(3),
            "flock /tmp/regia-gpu-3.lock flock /tmp/ferrite-gpu-3.lock"
        );
        assert_eq!(legacy_lock_path(3), PathBuf::from("/tmp/ferrite-gpu-3.lock"));
        assert_eq!(
            flock_args(3),
            [
                std::ffi::OsString::from("/tmp/regia-gpu-3.lock"),
                "flock".into(),
                "/tmp/ferrite-gpu-3.lock".into()
            ]
        );
        // A caller that holds card 2's lock for the whole run: launches on card 2 skip theirs.
        assert!(!launch_takes_lock(2, Some("2")));
        assert!(launch_takes_lock(2, Some("0")));
        assert!(launch_takes_lock(2, None));
        assert!(launch_takes_lock(2, Some("all")));
    }

    #[test]
    fn plan_has_one_job_per_scenario_with_paths_under_out() {
        let out = PathBuf::from("/o");
        let jobs = plan(Suite::Gi, &args("gi", &out)).expect("plan");
        let names: Vec<&str> = jobs.iter().map(|job| job.scenario.as_str()).collect();
        assert_eq!(names, ["gi-full", "gi-no-probes", "gi-none"]);
        let none = &jobs[2].args;
        assert!(none.windows(2).any(|w| w == ["--off", "ssgi"]));
        assert!(none.windows(2).any(|w| w == ["--frames", "100"]));
        // Paths go to the child process, so they are the host's native paths.
        let under = |file: &str| out.join("gi").join("gi-none").join(file).display().to_string();
        assert!(none.contains(&under("summary.json")));
        assert!(none.contains(&under("frames.csv")));
        let baked = plan(Suite::BakedLight, &args("baked-light", &out)).expect("plan");
        assert!(baked[1].args.iter().any(|arg| arg == BAKED_FLAG));
        assert!(!baked[0].args.iter().any(|arg| arg == BAKED_FLAG));
    }

    #[test]
    fn selected_line_gives_adapter_and_driver() {
        let log = "noise\nselected: NVIDIA GeForce RTX 5090 (pci 0000:01:00.0, driver NVIDIA 580.65.06; ByIndex(2))\nadapter: x\n";
        let info = parse_selected(log).expect("selected line");
        assert_eq!(info.name, "NVIDIA GeForce RTX 5090");
        assert_eq!(info.driver, "NVIDIA 580.65.06");
        assert_eq!(parse_selected("nothing here"), None);
    }

    #[test]
    fn pass_stats_use_nearest_rank_and_skip_idle_passes() {
        let mut csv = String::from("map,frame,wall,gpu_frame,ssgi,forward\n");
        for i in 1..=100 {
            let _ = writeln!(csv, "mars,{i},9.0,{i}.0,0,2.5");
        }
        let stats = pass_stats(&csv);
        let names: Vec<&str> = stats.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gpu_frame", "forward"]);
        assert_eq!(stats[0].p50, 51.0);
        assert_eq!(stats[0].p95, 95.0);
        assert_eq!(stats[0].p99, 99.0);
        assert_eq!(stats[1].p99, 2.5);
        assert!(pass_stats("").is_empty());
    }

    fn sample_rows() -> Vec<Row> {
        vec![
            Row {
                scenario: "gi-full".to_owned(),
                status: "pass".to_owned(),
                exit_code: Some(0),
                metrics: Metrics {
                    p50_ms: Some(8.5),
                    p95_ms: Some(9.25),
                    p99_ms: Some(11.0),
                    ..Metrics::default()
                },
                passes: vec![PassStat {
                    name: "gpu_frame".to_owned(),
                    p50: 3.125,
                    p95: 3.5,
                    p99: 4.0,
                }],
                memory: None,
                note: String::new(),
            },
            Row::without_run("baked-gi", "not available", "no | runtime"),
        ]
    }

    fn stamp() -> Stamp {
        Stamp {
            date: "2026-10-21".to_owned(),
            commit: "abc1234".to_owned(),
            adapter: "Test GPU".to_owned(),
            driver: "Test 1.0".to_owned(),
            gpu_index: Some(2),
        }
    }

    #[test]
    fn markdown_table_has_stamp_rows_and_pass_section() {
        let text = markdown(Suite::Gi, &stamp(), &sample_rows());
        assert!(text.contains("- adapter: Test GPU"));
        assert!(text.contains("- driver: Test 1.0"));
        assert!(text.contains("- commit: abc1234"));
        assert!(text.contains("| gi-full | pass | 8.500 | 9.250 | 11.000 | 3.125 |  |"));
        assert!(text.contains("| baked-gi | not available | - | - | - | - | no / runtime |"));
        assert!(text.contains("## GPU passes: gi-full"));
        assert!(text.contains("| gpu_frame | 3.125 | 3.500 | 4.000 |"));
        assert!(!text.contains("## GPU passes: baked-gi"));
    }

    #[test]
    fn json_round_trips_the_fields() {
        let text = json(Suite::Gi, &stamp(), &sample_rows());
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["suite"], "gi");
        assert_eq!(value["commit"], "abc1234");
        assert_eq!(value["gpu_index"], 2);
        assert_eq!(value["rows"][0]["p95_ms"], 9.25);
        assert_eq!(value["rows"][0]["passes"][0]["pass"], "gpu_frame");
        assert_eq!(value["rows"][1]["status"], "not available");
    }

    #[test]
    fn judge_maps_exit_states() {
        let metrics = Metrics::default();
        assert_eq!(judge(Some(0), Some(&metrics)).0, "pass");
        assert_eq!(judge(Some(124), None).1, "timed out");
        assert_eq!(judge(Some(1), Some(&metrics)).1, "exit 1");
        assert_eq!(judge(Some(0), None).1, "no readable summary.json");
        assert_eq!(judge(None, None).0, "fail");
    }

    #[test]
    fn binary_probe_finds_the_flag_only_when_present() {
        let dir = regia_test_utils::unique_temp_dir("gpu-bench-probe");
        let with = dir.join("with");
        let without = dir.join("without");
        fs::write(&with, b"\x7fELF....--baked-gi....").expect("write");
        fs::write(&without, b"\x7fELF....--off....").expect("write");
        assert!(binary_contains(&with, BAKED_FLAG).expect("read"));
        assert!(!binary_contains(&without, BAKED_FLAG).expect("read"));
        assert!(binary_contains(&dir.join("missing"), BAKED_FLAG).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_writes_planned_rows_without_a_card_or_binary() {
        let dir = regia_test_utils::unique_temp_dir("gpu-bench-dry");
        let a = args("baked-light", &dir);
        assert!(run(&a, &dir).is_ok());
        let text = fs::read_to_string(dir.join("gpu-bench.json")).expect("json");
        let value: serde_json::Value = serde_json::from_str(&text).expect("parse");
        assert_eq!(value["rows"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["rows"][0]["status"], "planned");
        assert!(dir.join("gpu-bench.md").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_suite_and_zero_frames_are_errors() {
        let dir = PathBuf::from("/nonexistent-gpu-bench");
        assert!(run(&args("nope", &dir), &dir).is_err());
        let mut a = args("gi", &dir);
        a.frames = 0;
        assert!(run(&a, &dir).is_err());
    }
}
