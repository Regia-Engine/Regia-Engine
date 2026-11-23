// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask replay-soak`: record and replay N seeds of every replay-soak scene once per
//! compute-thread count and build profile, then compare the state-hash traces across those runs
//! (`book/agents/replay-soak.md`, ADR-867).
//!
//! Each run is one `cargo test -p regia_test_utils --test replay_soak` process
//! (`regia_test_utils::replay_soak`; xtask may not depend on that crate outside
//! dev-dependencies, `check-layers`). The test sizes the compute pool, records and replays every
//! `(scene, seed)` and writes `run-<label>.json` (format `regia-replay-soak-run` v1). This
//! command then merges the run files into `replay-soak.json` (format `regia-replay-soak` v1) and
//! `replay-soak.md`:
//!
//! - every run's **record trace** must equal the reference run's (the first build at the first
//!   thread count) at every checkpoint: the state may not depend on the thread count, the
//!   optimizer or a per-process hash seed;
//! - every run's **replay** must equal its own recording.
//!
//! `--baseline <replay-soak.json>` also compares the reference traces with a merged report from
//! another machine, OS or commit. `--no-run` merges the run files already in `--out` (copy run
//! files from other machines there first; their labels must differ, see `--tag`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};

/// The run-file format the test writes.
pub const RUN_FORMAT: &str = "regia-replay-soak-run";
/// The merged-report format this command writes.
pub const MERGED_FORMAT: &str = "regia-replay-soak";
/// Version of both formats.
pub const FORMAT_VERSION: u32 = 1;
/// Most entities listed for one trace divergence.
const MAX_ENTITIES: usize = 12;

/// Arguments of `cargo xtask replay-soak`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Seeds per scene.
    #[arg(long, default_value_t = 8)]
    pub seeds: u64,
    /// The first seed (seeds run `first-seed .. first-seed + seeds`).
    #[arg(long, default_value_t = 1)]
    pub first_seed: u64,
    /// Fixed steps per run (60 per second of game time).
    #[arg(long, default_value_t = 600)]
    pub steps: u32,
    /// Steps between checkpoints (1 hashes every step; the last step always is one).
    #[arg(long, default_value_t = 10)]
    pub every: u32,
    /// Scenes to run, comma-separated (`smoke`, `pile`, `combat`); all when omitted.
    #[arg(long, value_delimiter = ',')]
    pub scenes: Vec<String>,
    /// Compute thread counts, comma-separated, one run each; `auto` keeps the default formula.
    #[arg(long, value_delimiter = ',', default_values = ["1", "2", "8"])]
    pub threads: Vec<String>,
    /// Build profiles, comma-separated, one run each (`debug`, `release` or a custom profile).
    #[arg(long, value_delimiter = ',', default_values = ["debug", "release"])]
    pub builds: Vec<String>,
    /// Cargo features for the test build, e.g. `regia_physics/physics-cross-platform-determinism`.
    #[arg(long)]
    pub features: Option<String>,
    /// Prefix for run labels (the machine, e.g. `mac-`), so run files from several machines can
    /// be merged in one directory.
    #[arg(long, default_value = "")]
    pub tag: String,
    /// Output directory (relative paths are under the workspace root).
    #[arg(long, default_value = "target/replay-soak")]
    pub out: PathBuf,
    /// Run nothing: merge the `run-*.json` files already in `--out`.
    #[arg(long)]
    pub no_run: bool,
    /// A merged `replay-soak.json` from another machine, OS or commit to compare with.
    #[arg(long)]
    pub baseline: Option<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seeds: 8,
            first_seed: 1,
            steps: 600,
            every: 10,
            scenes: Vec::new(),
            threads: vec!["1".into(), "2".into(), "8".into()],
            builds: vec!["debug".into(), "release".into()],
            features: None,
            tag: String::new(),
            out: PathBuf::from("target/replay-soak"),
            no_run: false,
            baseline: None,
        }
    }
}

/// One planned test process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// `<tag><build>-t<threads|auto>`; the run file is `run-<label>.json`.
    pub label: String,
    /// Build profile.
    pub build: String,
    /// Compute threads (`None`: the default formula).
    pub threads: Option<usize>,
    /// `cargo` arguments.
    pub cargo: Vec<String>,
    /// Environment for the test.
    pub env: Vec<(String, String)>,
}

/// Parses one `--threads` entry.
pub fn parse_threads(text: &str) -> Result<Option<usize>> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    match text.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(Some(n)),
        _ => bail!("--threads: {text:?} is not a thread count (a number >= 1, or `auto`)"),
    }
}

fn profile_args(build: &str) -> Vec<String> {
    match build {
        "debug" | "dev" => Vec::new(),
        "release" => vec!["--release".into()],
        other => vec!["--profile".into(), other.into()],
    }
}

/// The test processes `args` asks for, builds outer and thread counts inner. `out` is the
/// absolute output directory.
pub fn plan(args: &Args, out: &Path) -> Result<Vec<RunPlan>> {
    if args.builds.is_empty() || args.threads.is_empty() {
        bail!("--builds and --threads need at least one entry each");
    }
    if args.seeds == 0 || args.steps == 0 {
        bail!("--seeds and --steps must be at least 1");
    }
    let threads = args
        .threads
        .iter()
        .map(|t| parse_threads(t))
        .collect::<Result<Vec<_>>>()?;
    let mut plans = Vec::new();
    for build in &args.builds {
        let build = build.trim();
        if build.is_empty() {
            bail!("--builds: empty profile name");
        }
        for count in &threads {
            let label = format!(
                "{}{build}-t{}",
                args.tag,
                count.map_or_else(|| "auto".to_string(), |n| n.to_string())
            );
            let mut cargo = vec!["test".to_string()];
            cargo.extend(profile_args(build));
            cargo.extend(
                ["-p", "regia_test_utils", "--test", "replay_soak"]
                    .iter()
                    .map(|s| (*s).to_string()),
            );
            if let Some(features) = &args.features {
                cargo.extend(["--features".to_string(), features.clone()]);
            }
            cargo.extend(
                [
                    "--",
                    "--exact",
                    "replay_soak_matrix",
                    "--nocapture",
                    "--test-threads=1",
                ]
                .iter()
                .map(|s| (*s).to_string()),
            );
            let mut env = vec![
                ("REGIA_REPLAY_SOAK_OUT".to_string(), out.display().to_string()),
                ("REGIA_REPLAY_SOAK_SEEDS".to_string(), args.seeds.to_string()),
                (
                    "REGIA_REPLAY_SOAK_FIRST_SEED".to_string(),
                    args.first_seed.to_string(),
                ),
                ("REGIA_REPLAY_SOAK_STEPS".to_string(), args.steps.to_string()),
                (
                    "REGIA_REPLAY_SOAK_EVERY".to_string(),
                    args.every.max(1).to_string(),
                ),
                ("REGIA_REPLAY_SOAK_LABEL".to_string(), label.clone()),
                ("REGIA_REPLAY_SOAK_BUILD".to_string(), build.to_string()),
            ];
            if !args.scenes.is_empty() {
                env.push(("REGIA_REPLAY_SOAK_SCENES".to_string(), args.scenes.join(",")));
            }
            if let Some(n) = count {
                env.push(("REGIA_REPLAY_SOAK_THREADS".to_string(), n.to_string()));
            }
            plans.push(RunPlan {
                label,
                build: build.to_string(),
                threads: *count,
                cargo,
                env,
            });
        }
    }
    Ok(plans)
}

// ---------------------------------------------------------------------------------------------
// Run files (written by `regia_test_utils::replay_soak::RunReport::to_json`)
// ---------------------------------------------------------------------------------------------

/// One test process's report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFile {
    /// `regia-replay-soak-run`.
    pub format: String,
    /// 1.
    pub version: u32,
    /// Run label.
    pub label: String,
    /// Build profile.
    pub build: String,
    /// Compute threads asked for (`null`: the default formula).
    pub threads_requested: Option<u64>,
    /// Compute threads the pool had.
    pub threads: u64,
    /// `std::env::consts::OS` of the test binary.
    pub os: String,
    /// `std::env::consts::ARCH` of the test binary.
    pub arch: String,
    /// Fixed steps per run.
    pub steps: u32,
    /// Steps between checkpoints.
    pub every: u32,
    /// One entry per `(scene, seed)`.
    pub runs: Vec<SeedRun>,
}

/// One `(scene, seed)` in a run file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeedRun {
    /// Scene name.
    pub scene: String,
    /// Seed.
    pub seed: u64,
    /// Entities in the last record snapshot.
    pub entities: u64,
    /// Values in the last record snapshot.
    pub values: u64,
    /// Non-zero generated inputs.
    pub active_inputs: u64,
    /// Record checkpoints (with per-entity hashes).
    pub record: Vec<Checkpoint>,
    /// Replay checkpoints.
    pub replay: Vec<Checkpoint>,
    /// Where the replay left its recording, if it did.
    pub replay_divergence: Option<Divergence>,
}

/// A checkpoint: step, whole-state hash, per-entity hashes (record traces only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Fixed steps run.
    pub step: u32,
    /// 16 hex digits.
    pub hash: String,
    /// Entity key → 16 hex digits.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entities: BTreeMap<String, String>,
}

/// Where a replay left its recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    /// First differing checkpoint.
    pub step: u32,
    /// Differing values at that step.
    pub differing: Vec<ValueDiff>,
}

/// One differing `<entity>/<Component>` value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueDiff {
    /// `<entity>/<Component>`.
    pub key: String,
    /// The recording's value.
    pub record: String,
    /// The replay's value.
    pub replay: String,
}

/// Parses and checks one run file.
pub fn parse_run_file(text: &str) -> Result<RunFile> {
    let file: RunFile = serde_json::from_str(text).context("parsing a replay-soak run file")?;
    if file.format != RUN_FORMAT || file.version != FORMAT_VERSION {
        bail!(
            "unsupported run file {} v{} (this xtask reads {RUN_FORMAT} v{FORMAT_VERSION})",
            file.format,
            file.version
        );
    }
    Ok(file)
}

// ---------------------------------------------------------------------------------------------
// Merged report
// ---------------------------------------------------------------------------------------------

/// One run's identity in the merged report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigInfo {
    /// Run label.
    pub label: String,
    /// Build profile.
    pub build: String,
    /// Compute threads the pool had.
    pub threads: u64,
    /// OS of the test binary.
    pub os: String,
    /// Architecture of the test binary.
    pub arch: String,
}

/// A checkpoint of a reference trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracePoint {
    /// Fixed steps run.
    pub step: u32,
    /// 16 hex digits.
    pub hash: String,
}

/// One run's result for one `(scene, seed)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellResult {
    /// Run label.
    pub label: String,
    /// Whether the run has this `(scene, seed)` at all.
    pub present: bool,
    /// Whether the record trace equals the reference run's at every checkpoint.
    pub matches_reference: bool,
    /// The first checkpoint step where the record trace leaves the reference.
    pub first_divergent_step: Option<u32>,
    /// Entities whose hash differs at that step (at most 12).
    pub divergent_entities: Vec<String>,
    /// Whether the replay equals the recording at every checkpoint.
    pub replay_matches: bool,
    /// Where the replay left the recording.
    pub replay_divergence: Option<Divergence>,
}

impl CellResult {
    /// True when present, equal to the reference and replayed cleanly.
    pub fn ok(&self) -> bool {
        self.present && self.matches_reference && self.replay_matches
    }
}

/// One `(scene, seed)` across every run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// Scene name.
    pub scene: String,
    /// Seed.
    pub seed: u64,
    /// The reference run's record trace.
    pub reference_trace: Vec<TracePoint>,
    /// The reference run's last hash.
    pub final_hash: String,
    /// One result per run, in run order.
    pub results: Vec<CellResult>,
}

/// One reference-trace difference against a baseline report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineMismatch {
    /// Scene name.
    pub scene: String,
    /// Seed.
    pub seed: u64,
    /// First common checkpoint step whose hash differs.
    pub step: u32,
}

/// The comparison with `--baseline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineResult {
    /// The baseline file.
    pub path: String,
    /// The baseline's reference run (`label os arch`).
    pub reference: String,
    /// Cells compared.
    pub compared: usize,
    /// Cells with no counterpart in the baseline.
    pub missing: usize,
    /// Cells whose reference trace differs.
    pub mismatches: Vec<BaselineMismatch>,
}

/// The merged report (`replay-soak.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merged {
    /// `regia-replay-soak`.
    pub format: String,
    /// 1.
    pub version: u32,
    /// The runs, reference first.
    pub configs: Vec<ConfigInfo>,
    /// The reference run's label.
    pub reference: String,
    /// Fixed steps per run (the reference's).
    pub steps: u32,
    /// Steps between checkpoints (the reference's).
    pub every: u32,
    /// True when every cell is ok, nothing is missing and no problem was found.
    pub pass: bool,
    /// One per `(scene, seed)`, sorted by scene then seed.
    pub cells: Vec<Cell>,
    /// Runs that failed or files that could not be used.
    pub problems: Vec<String>,
    /// The `--baseline` comparison.
    pub baseline: Option<BaselineResult>,
}

/// Compares a record trace with the reference's: the first differing step and its entities.
fn compare_traces(reference: &[Checkpoint], other: &[Checkpoint]) -> Option<(u32, Vec<String>)> {
    for (a, b) in reference.iter().zip(other) {
        if a.step != b.step {
            return Some((a.step.min(b.step), vec!["<checkpoint steps differ>".to_string()]));
        }
        if a.hash != b.hash {
            let mut entities: Vec<String> = a
                .entities
                .iter()
                .filter(|(k, v)| b.entities.get(*k) != Some(v))
                .map(|(k, _)| k.clone())
                .chain(
                    b.entities
                        .keys()
                        .filter(|k| !a.entities.contains_key(*k))
                        .cloned(),
                )
                .collect();
            entities.sort();
            entities.dedup();
            entities.truncate(MAX_ENTITIES);
            return Some((a.step, entities));
        }
    }
    if reference.len() != other.len() {
        let common = reference.len().min(other.len());
        let step = reference
            .get(common)
            .or_else(|| other.get(common))
            .map_or(0, |c| c.step);
        return Some((step, vec!["<trace lengths differ>".to_string()]));
    }
    None
}

/// Merges run files; the first is the reference. `problems` are carried into the report (a run
/// that wrote no file, a file that did not parse).
pub fn merge(runs: &[RunFile], mut problems: Vec<String>) -> Merged {
    let Some(reference) = runs.first() else {
        problems.push("no run file to merge".to_string());
        return Merged {
            format: MERGED_FORMAT.to_string(),
            version: FORMAT_VERSION,
            configs: Vec::new(),
            reference: String::new(),
            steps: 0,
            every: 0,
            pass: false,
            cells: Vec::new(),
            problems,
            baseline: None,
        };
    };
    for run in &runs[1..] {
        if run.steps != reference.steps || run.every != reference.every {
            problems.push(format!(
                "{} ran {} steps every {}, the reference {} ran {} every {}: traces are not comparable",
                run.label, run.steps, run.every, reference.label, reference.steps, reference.every
            ));
        }
    }
    let mut labels: Vec<&str> = Vec::new();
    for run in runs {
        if labels.contains(&run.label.as_str()) {
            problems.push(format!("two run files are labelled {}", run.label));
        }
        labels.push(&run.label);
    }
    let mut keys: Vec<(String, u64)> = runs
        .iter()
        .flat_map(|run| run.runs.iter().map(|r| (r.scene.clone(), r.seed)))
        .collect();
    keys.sort();
    keys.dedup();
    let find = |run: &RunFile, scene: &str, seed: u64| -> Option<SeedRun> {
        run.runs
            .iter()
            .find(|r| r.scene == scene && r.seed == seed)
            .cloned()
    };
    let cells = keys
        .into_iter()
        .map(|(scene, seed)| {
            let base = find(reference, &scene, seed);
            let reference_trace: Vec<Checkpoint> =
                base.as_ref().map(|b| b.record.clone()).unwrap_or_default();
            let results = runs
                .iter()
                .map(|run| match find(run, &scene, seed) {
                    None => CellResult {
                        label: run.label.clone(),
                        present: false,
                        matches_reference: false,
                        first_divergent_step: None,
                        divergent_entities: Vec::new(),
                        replay_matches: false,
                        replay_divergence: None,
                    },
                    Some(seed_run) => {
                        let divergence = if base.is_some() {
                            compare_traces(&reference_trace, &seed_run.record)
                        } else {
                            Some((0, vec!["<the reference has no such run>".to_string()]))
                        };
                        CellResult {
                            label: run.label.clone(),
                            present: true,
                            matches_reference: divergence.is_none(),
                            first_divergent_step: divergence.as_ref().map(|d| d.0),
                            divergent_entities: divergence.map(|d| d.1).unwrap_or_default(),
                            replay_matches: seed_run.replay_divergence.is_none(),
                            replay_divergence: seed_run.replay_divergence,
                        }
                    }
                })
                .collect();
            Cell {
                final_hash: reference_trace.last().map(|c| c.hash.clone()).unwrap_or_default(),
                reference_trace: reference_trace
                    .iter()
                    .map(|c| TracePoint {
                        step: c.step,
                        hash: c.hash.clone(),
                    })
                    .collect(),
                scene,
                seed,
                results,
            }
        })
        .collect::<Vec<_>>();
    let pass = problems.is_empty() && cells.iter().all(|c| c.results.iter().all(CellResult::ok));
    Merged {
        format: MERGED_FORMAT.to_string(),
        version: FORMAT_VERSION,
        configs: runs
            .iter()
            .map(|r| ConfigInfo {
                label: r.label.clone(),
                build: r.build.clone(),
                threads: r.threads,
                os: r.os.clone(),
                arch: r.arch.clone(),
            })
            .collect(),
        reference: reference.label.clone(),
        steps: reference.steps,
        every: reference.every,
        pass,
        cells,
        problems,
        baseline: None,
    }
}

/// Compares `merged`'s reference traces with a baseline report's, at the checkpoint steps both
/// have. A mismatch fails the merged report.
pub fn compare_baseline(merged: &mut Merged, baseline: &Merged, path: &str) {
    let base_config = baseline.configs.first();
    let mut result = BaselineResult {
        path: path.to_string(),
        reference: base_config.map_or_else(
            || baseline.reference.clone(),
            |c| format!("{} {} {}", c.label, c.os, c.arch),
        ),
        compared: 0,
        missing: 0,
        mismatches: Vec::new(),
    };
    for cell in &merged.cells {
        let Some(other) = baseline
            .cells
            .iter()
            .find(|c| c.scene == cell.scene && c.seed == cell.seed)
        else {
            result.missing += 1;
            continue;
        };
        result.compared += 1;
        let theirs: BTreeMap<u32, &str> = other
            .reference_trace
            .iter()
            .map(|p| (p.step, p.hash.as_str()))
            .collect();
        if let Some(point) = cell
            .reference_trace
            .iter()
            .find(|p| theirs.get(&p.step).is_some_and(|h| *h != p.hash))
        {
            result.mismatches.push(BaselineMismatch {
                scene: cell.scene.clone(),
                seed: cell.seed,
                step: point.step,
            });
        }
    }
    if !result.mismatches.is_empty() || result.compared == 0 {
        merged.pass = false;
    }
    merged.baseline = Some(result);
}

fn cell_text(result: &CellResult) -> String {
    if !result.present {
        return "missing".to_string();
    }
    let mut parts = Vec::new();
    if let Some(step) = result.first_divergent_step {
        parts.push(format!("trace @{step}"));
    }
    if let Some(divergence) = &result.replay_divergence {
        parts.push(format!("replay @{}", divergence.step));
    }
    if parts.is_empty() {
        "ok".to_string()
    } else {
        parts.join(", ")
    }
}

impl Merged {
    /// The report as markdown: verdict, matrix table, every divergence, baseline, problems.
    pub fn to_markdown(&self) -> String {
        let mut out = String::from("# Replay soak\n\n");
        let _ = writeln!(out, "- Verdict: **{}**", if self.pass { "PASS" } else { "FAIL" });
        let configs: Vec<String> = self
            .configs
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "`{}`{} ({} {}, {} compute threads)",
                    c.label,
                    if i == 0 { " (reference)" } else { "" },
                    c.os,
                    c.arch,
                    c.threads
                )
            })
            .collect();
        let _ = writeln!(out, "- Runs: {}", configs.join(", "));
        let scenes: std::collections::BTreeSet<&str> = self.cells.iter().map(|c| c.scene.as_str()).collect();
        let seeds: std::collections::BTreeSet<u64> = self.cells.iter().map(|c| c.seed).collect();
        let _ = writeln!(
            out,
            "- Matrix: {} scenes ({}) x {} seeds, {} steps, a checkpoint every {} steps",
            scenes.len(),
            scenes.into_iter().collect::<Vec<_>>().join(", "),
            seeds.len(),
            self.steps,
            self.every
        );
        let results: Vec<&CellResult> = self.cells.iter().flat_map(|c| &c.results).collect();
        let traces = results
            .iter()
            .filter(|r| r.present && r.matches_reference)
            .count();
        let replays = results.iter().filter(|r| r.present && r.replay_matches).count();
        let _ = writeln!(
            out,
            "- Record traces equal to the reference: {traces}/{}; replays equal to their recording: {replays}/{}\n",
            results.len(),
            results.len()
        );

        out.push_str("| Scene | Seed | Final hash (reference) |");
        for config in &self.configs {
            let _ = write!(out, " {} |", config.label);
        }
        out.push_str("\n|---|---|---|");
        for _ in &self.configs {
            out.push_str("---|");
        }
        out.push('\n');
        for cell in &self.cells {
            let _ = write!(out, "| {} | {} | `{}` |", cell.scene, cell.seed, cell.final_hash);
            for result in &cell.results {
                let _ = write!(out, " {} |", cell_text(result));
            }
            out.push('\n');
        }
        out.push_str(
            "\n`ok`: the record trace equals the reference run's and the replay equals the recording. \
             `trace @S`: the record trace leaves the reference at step S. `replay @S`: the replay \
             leaves its own recording at step S. `missing`: the run has no result.\n",
        );

        let failing: Vec<(&Cell, &CellResult)> = self
            .cells
            .iter()
            .flat_map(|cell| cell.results.iter().map(move |r| (cell, r)))
            .filter(|(_, r)| !r.ok())
            .collect();
        if !failing.is_empty() {
            out.push_str("\n## Divergences\n");
            for (cell, result) in failing {
                let _ = writeln!(out, "\n### {} seed {}, {}\n", cell.scene, cell.seed, result.label);
                if !result.present {
                    out.push_str("- no result in this run\n");
                }
                if let Some(step) = result.first_divergent_step {
                    let _ = writeln!(
                        out,
                        "- record trace leaves `{}` at step {step}; entities: {}",
                        self.reference,
                        if result.divergent_entities.is_empty() {
                            "(no per-entity hashes)".to_string()
                        } else {
                            result.divergent_entities.join(", ")
                        }
                    );
                }
                if let Some(divergence) = &result.replay_divergence {
                    let _ = writeln!(out, "- replay leaves its recording at step {}:", divergence.step);
                    for diff in &divergence.differing {
                        let _ = writeln!(
                            out,
                            "  - `{}`\n    - record: `{}`\n    - replay: `{}`",
                            diff.key,
                            diff.record.replace('`', "'"),
                            diff.replay.replace('`', "'")
                        );
                    }
                }
            }
        }
        if let Some(baseline) = &self.baseline {
            let _ = writeln!(
                out,
                "\n## Baseline\n\n- `{}` (reference {}): {} cells compared, {} not in the baseline, {} differ",
                baseline.path,
                baseline.reference,
                baseline.compared,
                baseline.missing,
                baseline.mismatches.len()
            );
            for mismatch in &baseline.mismatches {
                let _ = writeln!(
                    out,
                    "  - {} seed {}: first differing checkpoint at step {}",
                    mismatch.scene, mismatch.seed, mismatch.step
                );
            }
        }
        if !self.problems.is_empty() {
            out.push_str("\n## Problems\n\n");
            for problem in &self.problems {
                let _ = writeln!(out, "- {problem}");
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------------------------

fn read_run_files(out: &Path, labels: Option<&[String]>, problems: &mut Vec<String>) -> Vec<RunFile> {
    let mut paths: Vec<PathBuf> = match labels {
        Some(labels) => labels
            .iter()
            .map(|label| out.join(format!("run-{label}.json")))
            .collect(),
        None => match std::fs::read_dir(out) {
            Ok(entries) => {
                let mut found: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("run-") && n.ends_with(".json"))
                    })
                    .collect();
                found.sort();
                found
            }
            Err(err) => {
                problems.push(format!("reading {}: {err}", out.display()));
                Vec::new()
            }
        },
    };
    paths.dedup();
    let mut runs = Vec::new();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_run_file(&text) {
                Ok(run) => runs.push(run),
                Err(err) => problems.push(format!("{}: {err:#}", path.display())),
            },
            Err(err) => problems.push(format!(
                "{}: {err} (the run wrote no report: a build or test failure, see the output above)",
                path.display()
            )),
        }
    }
    runs
}

/// Runs the soak (unless `--no-run`), merges the run files and writes `replay-soak.json` and
/// `replay-soak.md` into `--out`. Exit code 1 on any divergence, missing run or baseline mismatch.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = if args.out.is_absolute() {
        args.out.clone()
    } else {
        root.join(&args.out)
    };
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let mut problems = Vec::new();
    let runs = if args.no_run {
        read_run_files(&out, None, &mut problems)
    } else {
        let plans = plan(args, &out)?;
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        for plan in &plans {
            let file = out.join(format!("run-{}.json", plan.label));
            if file.exists() {
                std::fs::remove_file(&file)
                    .with_context(|| format!("removing the stale {}", file.display()))?;
            }
            println!("replay-soak: {} (cargo {})", plan.label, plan.cargo.join(" "));
            let status = Command::new(&cargo)
                .current_dir(root)
                .args(&plan.cargo)
                .envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .status()
                .with_context(|| format!("running cargo test for replay-soak run {}", plan.label))?;
            if !status.success() {
                problems.push(format!(
                    "run {} exited with {status} (a replay divergence, or a build or test failure)",
                    plan.label
                ));
            }
        }
        let labels: Vec<String> = plans.iter().map(|p| p.label.clone()).collect();
        read_run_files(&out, Some(labels.as_slice()), &mut problems)
    };
    // A failing test exit with a complete run file is a replay divergence the merge reports in
    // full; keep the exit note only for runs that left no file.
    let reported: Vec<&str> = runs.iter().map(|r| r.label.as_str()).collect();
    problems.retain(|p| {
        !reported
            .iter()
            .any(|label| p.starts_with(&format!("run {label} exited")))
    });
    let mut merged = merge(&runs, problems);
    if let Some(path) = &args.baseline {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the baseline {}", path.display()))?;
        let baseline: Merged = serde_json::from_str(&text)
            .with_context(|| format!("parsing the baseline {}", path.display()))?;
        if baseline.format != MERGED_FORMAT || baseline.version != FORMAT_VERSION {
            bail!(
                "the baseline {} is {} v{}, not {MERGED_FORMAT} v{FORMAT_VERSION}",
                path.display(),
                baseline.format,
                baseline.version
            );
        }
        compare_baseline(&mut merged, &baseline, &path.display().to_string());
    }
    let markdown = merged.to_markdown();
    let json = serde_json::to_string_pretty(&merged).context("serializing the merged report")?;
    write_file(&out.join("replay-soak.json"), json.as_bytes())?;
    write_file(&out.join("replay-soak.md"), markdown.as_bytes())?;
    println!("{markdown}");
    println!("replay-soak: report in {}", out.join("replay-soak.md").display());
    Ok(if merged.pass {
        println!("replay-soak: ok");
        ExitCode::SUCCESS
    } else {
        eprintln!("replay-soak: FAILED (see the divergences above)");
        ExitCode::FAILURE
    })
}

/// Writes through a temporary file and a rename, so a reader never sees half a report.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run file exactly as `RunReport::to_json` writes it (one run, a replay divergence).
    const WRITER_SAMPLE: &str = r#"{
  "format": "regia-replay-soak-run",
  "version": 1,
  "label": "release-t8",
  "build": "release",
  "threads_requested": 8,
  "threads": 8,
  "os": "linux",
  "arch": "x86_64",
  "steps": 20,
  "every": 10,
  "runs": [
    {"scene": "combat", "seed": 5, "entities": 3, "values": 9, "active_inputs": 17,
      "record": [{"step": 10, "hash": "00000000000000aa", "entities": {"tally": "0000000000000001", "turret": "0000000000000002"}}, {"step": 20, "hash": "00000000000000bb", "entities": {"tally": "0000000000000003", "turret": "0000000000000004"}}],
      "replay": [{"step": 10, "hash": "00000000000000aa"}, {"step": 20, "hash": "00000000000000cc"}],
      "replay_divergence": {"step": 20, "differing": [{"key": "turret/SoakTurret", "record": "SoakTurret { yaw: 0.5, cooldown: 0 }", "replay": "SoakTurret { yaw: 0.25, cooldown: 0 }"}]}}
  ]
}
"#;

    fn checkpoint(step: u32, hash: &str, entities: &[(&str, &str)]) -> Checkpoint {
        Checkpoint {
            step,
            hash: hash.to_string(),
            entities: entities
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn run_file(label: &str, record: Vec<Checkpoint>, replay_divergence: Option<Divergence>) -> RunFile {
        RunFile {
            format: RUN_FORMAT.to_string(),
            version: FORMAT_VERSION,
            label: label.to_string(),
            build: "debug".to_string(),
            threads_requested: Some(1),
            threads: 1,
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            steps: 20,
            every: 10,
            runs: vec![SeedRun {
                scene: "pile".to_string(),
                seed: 1,
                entities: 2,
                values: 4,
                active_inputs: 9,
                replay: record.iter().map(|c| checkpoint(c.step, &c.hash, &[])).collect(),
                record,
                replay_divergence,
            }],
        }
    }

    fn trace(second: &str, body: &str) -> Vec<Checkpoint> {
        vec![
            checkpoint(10, "0a", &[("body01", "1"), ("striker", "2")]),
            checkpoint(20, second, &[("body01", body), ("striker", "4")]),
        ]
    }

    #[test]
    fn the_plan_runs_one_test_process_per_build_and_thread_count() {
        let args = Args {
            threads: vec!["1".into(), "auto".into()],
            scenes: vec!["pile".into(), "combat".into()],
            tag: "mac-".into(),
            features: Some("regia_physics/physics-cross-platform-determinism".into()),
            ..Args::default()
        };
        let plans = plan(&args, Path::new("/abs/out")).expect("plan");
        let labels: Vec<&str> = plans.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "mac-debug-t1",
                "mac-debug-tauto",
                "mac-release-t1",
                "mac-release-tauto"
            ]
        );
        let release = &plans[2];
        assert!(release.cargo.contains(&"--release".to_string()));
        assert!(!plans[0].cargo.contains(&"--release".to_string()));
        assert!(release.cargo.ends_with(&[
            "--".to_string(),
            "--exact".to_string(),
            "replay_soak_matrix".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ]));
        assert!(
            release
                .cargo
                .windows(2)
                .any(|w| w[0] == "--features" && w[1].starts_with("regia_physics/"))
        );
        let env: BTreeMap<&str, &str> = release
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(env["REGIA_REPLAY_SOAK_OUT"], "/abs/out");
        assert_eq!(env["REGIA_REPLAY_SOAK_THREADS"], "1");
        assert_eq!(env["REGIA_REPLAY_SOAK_SCENES"], "pile,combat");
        assert_eq!(env["REGIA_REPLAY_SOAK_LABEL"], "mac-release-t1");
        assert_eq!(env["REGIA_REPLAY_SOAK_BUILD"], "release");
        assert!(
            !plans[3].env.iter().any(|(k, _)| k == "REGIA_REPLAY_SOAK_THREADS"),
            "auto leaves the pool size to the default formula"
        );
        let custom = plan(
            &Args {
                builds: vec!["ship".into()],
                ..Args::default()
            },
            Path::new("/o"),
        )
        .expect("plan");
        assert!(custom[0].cargo.windows(2).any(|w| w == ["--profile", "ship"]));
    }

    /// The command line as `cargo xtask replay-soak` parses it.
    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn the_command_line_defaults_match_args_default_and_lists_split_on_commas() {
        use clap::Parser as _;
        let parsed = Cli::try_parse_from(["replay-soak"])
            .expect("no arguments parse")
            .args;
        let defaults = Args::default();
        assert_eq!(parsed.threads, defaults.threads);
        assert_eq!(parsed.builds, defaults.builds);
        assert_eq!(parsed.seeds, defaults.seeds);
        assert_eq!(parsed.steps, defaults.steps);
        assert_eq!(parsed.every, defaults.every);
        assert_eq!(parsed.out, defaults.out);
        assert!(parsed.scenes.is_empty());
        let parsed = Cli::try_parse_from([
            "replay-soak",
            "--threads",
            "1,auto",
            "--builds",
            "release",
            "--scenes",
            "pile,crowd",
            "--tag",
            "mac-",
            "--no-run",
        ])
        .expect("lists parse")
        .args;
        assert_eq!(parsed.threads, ["1", "auto"]);
        assert_eq!(parsed.builds, ["release"]);
        assert_eq!(parsed.scenes, ["pile", "crowd"]);
        assert_eq!(parsed.tag, "mac-");
        assert!(parsed.no_run);
    }

    #[test]
    fn bad_thread_counts_and_empty_lists_are_rejected() {
        assert_eq!(parse_threads("4").expect("4"), Some(4));
        assert_eq!(parse_threads("AUTO").expect("auto"), None);
        assert!(parse_threads("0").is_err());
        assert!(parse_threads("lots").is_err());
        let no_builds = Args {
            builds: Vec::new(),
            ..Args::default()
        };
        assert!(plan(&no_builds, Path::new("/o")).is_err());
    }

    #[test]
    fn run_files_in_the_writer_format_parse() {
        let run = parse_run_file(WRITER_SAMPLE).expect("the writer's format parses");
        assert_eq!(run.label, "release-t8");
        assert_eq!(run.threads_requested, Some(8));
        assert_eq!(run.runs.len(), 1);
        let seed = &run.runs[0];
        assert_eq!(seed.record[0].entities.len(), 2);
        assert!(seed.replay[0].entities.is_empty());
        let divergence = seed.replay_divergence.as_ref().expect("divergence");
        assert_eq!(divergence.step, 20);
        assert_eq!(divergence.differing[0].key, "turret/SoakTurret");
        let wrong = WRITER_SAMPLE.replace("\"version\": 1", "\"version\": 2");
        assert!(parse_run_file(&wrong).is_err(), "other versions are refused");
    }

    /// The real writer, not a copy of its output (ADR-1378): `RunReport::to_json` in
    /// `regia_test_utils` hand-writes the JSON this module reads with serde, so a renamed field,
    /// a changed hash width or an escaping slip there fails here, field for field.
    #[test]
    fn the_test_utils_writer_round_trips_through_this_reader() {
        use regia_test_utils::replay_soak as soak;
        assert_eq!(
            (soak::RUN_FORMAT, soak::RUN_FORMAT_VERSION),
            (RUN_FORMAT, FORMAT_VERSION)
        );
        let odd_key = "tally \"q\" \\ é".to_string();
        let entities: BTreeMap<String, u64> = [("body01".to_string(), 0xaa), (odd_key.clone(), u64::MAX)]
            .into_iter()
            .collect();
        let report = soak::RunReport {
            label: "release-t8".into(),
            build: "release".into(),
            threads_requested: None,
            threads: 8,
            config: soak::SoakConfig { steps: 20, every: 10 },
            runs: vec![soak::SeedReport {
                scene: "combat".into(),
                seed: u64::MAX,
                entities: 2,
                values: 9,
                active_inputs: 17,
                record: vec![
                    soak::Checkpoint {
                        step: 10,
                        hash: 1,
                        entities: entities.clone(),
                    },
                    soak::Checkpoint {
                        step: 20,
                        hash: 0xdead_beef,
                        entities,
                    },
                ],
                replay: vec![soak::Checkpoint {
                    step: 10,
                    hash: 1,
                    entities: BTreeMap::new(),
                }],
                replay_divergence: Some(soak::Divergence {
                    step: 20,
                    differing: vec![soak::ValueDiff {
                        key: "turret/SoakTurret".into(),
                        record: "yaw: \"0.5\"\n\ttab \u{1}".into(),
                        replay: "<absent>".into(),
                    }],
                }),
            }],
        };
        let run = parse_run_file(&report.to_json()).expect("the writer's output parses");
        assert_eq!(
            (
                run.label.as_str(),
                run.build.as_str(),
                run.threads_requested,
                run.threads
            ),
            ("release-t8", "release", None, 8)
        );
        assert_eq!(
            (run.os.as_str(), run.arch.as_str()),
            (std::env::consts::OS, std::env::consts::ARCH)
        );
        assert_eq!((run.steps, run.every, run.runs.len()), (20, 10, 1));
        let seed = &run.runs[0];
        assert_eq!(
            (
                seed.scene.as_str(),
                seed.seed,
                seed.entities,
                seed.values,
                seed.active_inputs
            ),
            ("combat", u64::MAX, 2, 9, 17)
        );
        assert_eq!(
            seed.record[0],
            checkpoint(
                10,
                "0000000000000001",
                &[
                    ("body01", "00000000000000aa"),
                    (odd_key.as_str(), "ffffffffffffffff")
                ]
            )
        );
        assert_eq!(seed.record[1].hash, "00000000deadbeef");
        assert_eq!(seed.replay, vec![checkpoint(10, "0000000000000001", &[])]);
        let divergence = seed.replay_divergence.as_ref().expect("divergence");
        assert_eq!(divergence.step, 20);
        assert_eq!(
            divergence.differing,
            vec![ValueDiff {
                key: "turret/SoakTurret".into(),
                record: "yaw: \"0.5\"\n\ttab \u{1}".into(),
                replay: "<absent>".into(),
            }]
        );
    }

    #[test]
    fn identical_runs_merge_to_a_pass() {
        let runs = [
            run_file("debug-t1", trace("0b", "3"), None),
            run_file("debug-t8", trace("0b", "3"), None),
        ];
        let merged = merge(&runs, Vec::new());
        assert!(merged.pass, "{}", merged.to_markdown());
        assert_eq!(merged.reference, "debug-t1");
        assert_eq!(merged.cells.len(), 1);
        assert_eq!(merged.cells[0].final_hash, "0b");
        let markdown = merged.to_markdown();
        assert!(markdown.contains("**PASS**"), "{markdown}");
        assert!(markdown.contains("| pile | 1 | `0b` | ok | ok |"), "{markdown}");
    }

    #[test]
    fn a_run_whose_trace_leaves_the_reference_fails_with_its_step_and_entities() {
        let runs = [
            run_file("debug-t1", trace("0b", "3"), None),
            run_file("release-t8", trace("0c", "5"), None),
        ];
        let merged = merge(&runs, Vec::new());
        assert!(!merged.pass);
        let result = &merged.cells[0].results[1];
        assert_eq!(result.first_divergent_step, Some(20));
        assert_eq!(result.divergent_entities, vec!["body01".to_string()]);
        assert!(result.replay_matches);
        let markdown = merged.to_markdown();
        assert!(markdown.contains("trace @20"), "{markdown}");
        assert!(markdown.contains("entities: body01"), "{markdown}");
    }

    #[test]
    fn replay_divergences_and_missing_results_fail_the_merge() {
        let divergence = Divergence {
            step: 10,
            differing: vec![ValueDiff {
                key: "striker/Velocity".into(),
                record: "Velocity { linear: Vec3(1.0, 0.0, 0.0) }".into(),
                replay: "Velocity { linear: Vec3(1.5, 0.0, 0.0) }".into(),
            }],
        };
        let mut empty = run_file("debug-t2", trace("0b", "3"), None);
        empty.runs.clear();
        let runs = [
            run_file("debug-t1", trace("0b", "3"), None),
            run_file("debug-t4", trace("0b", "3"), Some(divergence)),
            empty,
        ];
        let merged = merge(&runs, vec!["run debug-t16 exited with exit status: 101".into()]);
        assert!(!merged.pass);
        let markdown = merged.to_markdown();
        assert!(markdown.contains("replay @10"), "{markdown}");
        assert!(markdown.contains("striker/Velocity"), "{markdown}");
        assert!(markdown.contains("missing"), "{markdown}");
        assert!(markdown.contains("## Problems"), "{markdown}");
    }

    #[test]
    fn mismatched_run_sizes_and_duplicate_labels_are_problems() {
        let mut longer = run_file("debug-t2", trace("0b", "3"), None);
        longer.steps = 40;
        let runs = [
            run_file("debug-t1", trace("0b", "3"), None),
            longer,
            run_file("debug-t1", trace("0b", "3"), None),
        ];
        let merged = merge(&runs, Vec::new());
        assert!(!merged.pass);
        assert!(merged.problems.iter().any(|p| p.contains("not comparable")));
        assert!(merged.problems.iter().any(|p| p.contains("two run files")));
        assert!(!merge(&[], Vec::new()).pass, "nothing to merge is a failure");
    }

    #[test]
    fn a_baseline_with_another_trace_fails_and_names_the_step() {
        let ours = merge(&[run_file("debug-t1", trace("0b", "3"), None)], Vec::new());
        let mut theirs = merge(&[run_file("mac-debug-t1", trace("0b", "3"), None)], Vec::new());
        let mut same = ours.clone();
        compare_baseline(&mut same, &theirs, "mac/replay-soak.json");
        assert!(same.pass, "{}", same.to_markdown());
        assert_eq!(same.baseline.as_ref().map(|b| b.compared), Some(1));

        theirs.cells[0].reference_trace[1].hash = "0f".into();
        let json = serde_json::to_string(&theirs).expect("serialize");
        let theirs: Merged = serde_json::from_str(&json).expect("the merged report round-trips");
        let mut differs = ours.clone();
        compare_baseline(&mut differs, &theirs, "mac/replay-soak.json");
        assert!(!differs.pass);
        let baseline = differs.baseline.as_ref().expect("baseline result");
        assert_eq!(
            baseline.mismatches,
            vec![BaselineMismatch {
                scene: "pile".into(),
                seed: 1,
                step: 20
            }]
        );
        assert!(differs.to_markdown().contains("## Baseline"));
    }
}
