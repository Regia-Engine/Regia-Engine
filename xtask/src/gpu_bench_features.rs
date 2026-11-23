// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask gpu-bench --suite features`: what each opt-in renderer feature costs on a real
//! card (ADR-2309).
//!
//! Each round launches, per tier (`--tiers`, `LOOK_R2_TIER` names), the plain chase frame and then
//! the frame with one feature on (`do27_chase --on <feature>`, [`lookdev::chase::ON_FEATURES`]),
//! all on the card `REGIA_GPU_INDEX` selects. The rounds interleave the plain and the feature
//! runs on one card in one session, so a drift of the card or the shared host lands on both sides
//! (two cards of one model differ by about 3 %, so a difference is only ever taken on one card).
//!
//! A feature's cost is the median over the rounds of the round's difference to the plain frame:
//! `gpu_frame` p50 and p99 (the GPU timestamps of the whole frame) and the CPU-inclusive frame
//! p50. Each run also writes `memory.json` (`do27_chase --memory-json`): the renderer's GPU
//! memory after the last frame, in `seeing.memory_report`'s fields, and the heap allocations per
//! measured frame (counted only by a `do27_chase` built with `--features alloc-count`).
//!
//! With `--bench-scenes`, each round also runs `regia bench <scenes> --json --on <feature>
//! --preset <tier>` (the public benchmark scenes, `book/agents/bench.md`), whose glTF scenes
//! carry blended materials the DO27 maps lack (and transmissive ones, read with the
//! `transmission` probes); each scene is its own row
//! (`<tier>@<scene>`), costed the same way. `regia bench` does not count heap allocations.
//!
//! The chase also takes the weather probes `rain`, `snow`, `vfx-layer` (their clear-weather
//! baseline) and `wet` (spec 99); `regia bench` takes
//! the material probes `thin-film`, `transmission` and `rough-transmission` ([`SCENE_FEATURES`]).
//! A `--features` name goes to the launches that know it: a weather name runs on the chase only, a
//! material name on the bench scenes only.
//!
//! [`BUDGETS`] records, per feature, the GPU budget its spec or ADR states, or that none is stated
//! (true of the first six on 2026-11-02: each ADR asks for the card cost before any preset turns the
//! feature on; spec 99 § 12 states the weather's). A feature whose p50 cost exceeds a stated budget
//! is flagged `OVER` in `features.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::gpu_bench::{
    AdapterInfo, BenchArgs, Job, LOCK_HELD_ENV, PassStat, Row, Stamp, Suite, base_args, flock_args, judge,
    launch_takes_lock, scenario_dir,
};
use crate::sweep::Metrics;

/// The flag that turns a feature on, in `do27_chase` and `regia bench`.
pub const ON_FLAG: &str = "--on";

/// The flag literal a `do27_chase` build that has [`ON_FLAG`] also contains: distinctive, where
/// `--on` alone also matches `--only` and the like in an older binary that would ignore it.
pub const CAPABILITY_FLAG: &str = "--memory-json";

/// The plain frame's name in scenario names and tables.
pub const BASE: &str = "base";

/// The features `regia bench --on` takes (the `regia` binary's `FEATURE_NAMES`): the chase's
/// first six and the material probes. `thin-film` films every opaque material, `transmission`
/// reads the glTF transmission, IOR and volume extensions (clear glass), and `rough-transmission`
/// does that with the material's roughness spreading what the glass shows. Rough transmission's
/// own cost is `rough-transmission` minus `transmission`.
pub const SCENE_FEATURES: [&str; 9] = [
    "radiance-cache",
    "blue-noise",
    "oit",
    "sss",
    "shadow-occlusion",
    "lens-effects",
    "thin-film",
    "transmission",
    "rough-transmission",
];

/// The tiers the suite accepts (`LOOK_R2_TIER` names of the desktop presets).
pub const TIERS: [&str; 3] = ["desktop-low", "desktop-high", "desktop-ultra"];

/// A feature's GPU budget as its spec or ADR states it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    /// `do27_chase --on` name.
    pub feature: &'static str,
    /// Milliseconds per frame, when a budget is stated.
    pub ms: Option<f64>,
    /// Where the budget (or the absence of one) is written.
    pub source: &'static str,
}

/// What the specs and ADRs say each `--on` feature may cost (checked 2026-11-02). None states a
/// per-frame ms budget; each asks for the card cost first, which this suite measures.
pub const BUDGETS: &[Budget] = &[
    Budget {
        feature: "radiance-cache",
        ms: None,
        source: "ADR-2085: card cost before a preset; spec 79 § 12 sizes it (1.4 MB) only",
    },
    Budget {
        feature: "blue-noise",
        ms: None,
        source: "ADR-2075: one 64 KiB texture, one pipeline per pass; no ms",
    },
    Budget {
        feature: "oit",
        ms: None,
        source: "ADR-2065: card cost numbers before a preset",
    },
    Budget {
        feature: "sss",
        ms: None,
        source: "ADR-2049: card cost numbers before a preset",
    },
    Budget {
        feature: "shadow-occlusion",
        ms: None,
        source: "ADR-2072: about 11 MB of pyramids; no ms",
    },
    Budget {
        feature: "lens-effects",
        ms: None,
        source: "ADR-1863: no cost on presets that do not allow it; no ms",
    },
    Budget {
        feature: "rain",
        ms: Some(0.6),
        source: "spec 99 § 12: storm rain at the High budget <= 0.6 ms (5080, 1440p, Desktop High)",
    },
    Budget {
        feature: "snow",
        ms: Some(0.5),
        source: "spec 99 § 12: blizzard (30k flakes) <= 0.5 ms (5080, 1440p, Desktop High)",
    },
    Budget {
        feature: "vfx-layer",
        ms: None,
        source: "the clear-weather VFX layer: the baseline of rain and snow; no budget",
    },
    Budget {
        feature: "wet",
        ms: Some(0.15),
        source: "spec 99 § 12: surface response on <= 0.15 ms over lab and terrain",
    },
    Budget {
        feature: "thin-film",
        ms: None,
        source: "ADR-1977: costs materials without a film nothing; no ms",
    },
    Budget {
        feature: "transmission",
        ms: None,
        source: "the clear-glass baseline of rough-transmission; no budget",
    },
    Budget {
        feature: "rough-transmission",
        ms: None,
        source: "ADR-2075: no ms",
    },
];

/// The budget row of `feature`, when the table has one.
#[must_use]
pub fn budget_of(feature: &str) -> Option<&'static Budget> {
    BUDGETS.iter().find(|budget| budget.feature == feature)
}

/// `OVER` when `cost` exceeds a stated `budget_ms`, `ok` when within it, `-` when either is missing.
#[must_use]
pub fn verdict(cost: Option<f64>, budget_ms: Option<f64>) -> &'static str {
    match (budget_ms, cost) {
        (Some(budget), Some(cost)) if cost > budget => "OVER",
        (Some(_), Some(_)) => "ok",
        _ => "-",
    }
}

fn split_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The scenario name of one launch: `<tier>/<feature>/r<round>`.
#[must_use]
pub fn scenario_name(tier: &str, feature: &str, round: u32) -> String {
    format!("{tier}/{feature}/r{round}")
}

/// Splits a scenario name back into tier, feature and round.
#[must_use]
pub fn parse_scenario(name: &str) -> Option<(&str, &str, u32)> {
    let mut parts = name.split('/');
    let tier = parts.next()?;
    let feature = parts.next()?;
    let round = parts.next()?.strip_prefix('r')?.parse().ok()?;
    parts.next().is_none().then_some((tier, feature, round))
}

/// The launches: for each round, for each tier, the plain frame and then each feature.
///
/// # Errors
///
/// An unknown tier or feature, an empty list, or zero rounds.
pub fn plan(args: &BenchArgs) -> Result<Vec<Job>> {
    let (tiers, features) = tiers_and_features(args)?;
    let features = features.chase;
    if args.skip_chase || features.is_empty() {
        return Ok(Vec::new());
    }
    let mut jobs = Vec::new();
    for round in 1..=args.rounds {
        for tier in &tiers {
            for feature in std::iter::once(BASE).chain(features.iter().map(String::as_str)) {
                let scenario = scenario_name(tier, feature, round);
                let dir = scenario_dir(&args.out, Suite::Features, &format!("{tier}-{feature}-r{round}"));
                let mut list = base_args(args, &format!("bench-features-{tier}-{feature}"), &dir);
                if feature != BASE {
                    list.push(ON_FLAG.to_owned());
                    list.push(feature.to_owned());
                }
                list.push("--memory-json".to_owned());
                list.push(dir.join("memory.json").display().to_string());
                jobs.push(Job {
                    scenario,
                    dir,
                    args: list,
                    env: vec![("LOOK_R2_TIER".to_owned(), tier.clone())],
                });
            }
        }
    }
    Ok(jobs)
}

/// The `regia bench` launches of `--bench-scenes`: for each round, for each tier, the plain frame
/// and then each feature, every listed scene in one launch. Empty without `--bench-scenes`.
///
/// # Errors
///
/// As [`plan`], or an empty scene list.
pub fn plan_scenes(args: &BenchArgs) -> Result<Vec<Job>> {
    let Some(list) = args.bench_scenes.as_deref() else {
        return Ok(Vec::new());
    };
    let scenes = split_list(list);
    if scenes.is_empty() {
        bail!("--bench-scenes is empty");
    }
    let (tiers, features) = tiers_and_features(args)?;
    let features = features.scenes;
    if features.is_empty() {
        return Ok(Vec::new());
    }
    let mut jobs = Vec::new();
    for round in 1..=args.rounds {
        for tier in &tiers {
            for feature in std::iter::once(BASE).chain(features.iter().map(String::as_str)) {
                let dir = args
                    .out
                    .join("features-scenes")
                    .join(format!("{tier}-{feature}-r{round}"));
                let mut list = vec!["bench".to_owned()];
                list.extend(scenes.iter().cloned());
                list.extend([
                    "--json".to_owned(),
                    "--preset".to_owned(),
                    tier.clone(),
                    "--frames".to_owned(),
                    args.frames.to_string(),
                    "--warmup".to_owned(),
                    args.warmup.to_string(),
                    "--size".to_owned(),
                    SCENE_SIZE.to_owned(),
                ]);
                if let Some(assets) = args.assets.as_ref() {
                    list.push("--assets".to_owned());
                    list.push(assets.display().to_string());
                }
                if feature != BASE {
                    list.push(ON_FLAG.to_owned());
                    list.push(feature.to_owned());
                }
                jobs.push(Job {
                    scenario: scenario_name(&format!("{tier}@scenes"), feature, round),
                    dir,
                    args: list,
                    env: Vec::new(),
                });
            }
        }
    }
    Ok(jobs)
}

/// The image size of the `regia bench` launches: the DO27 chase's.
pub const SCENE_SIZE: &str = "2560x1440";

/// The `--features` names split by the launches that take them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureSplit {
    /// Names `do27_chase --on` takes ([`lookdev::chase::ON_FEATURES`]).
    pub chase: Vec<String>,
    /// Names `regia bench --on` takes ([`SCENE_FEATURES`]).
    pub scenes: Vec<String>,
}

/// Splits `--features` (`None`: every name of each list) by the launches that take each name.
///
/// # Errors
///
/// An empty list, a name neither launch takes, or only bench-scene names without
/// `--bench-scenes` (nothing would run them).
pub fn split_features(list: Option<&str>, bench_scenes: bool) -> Result<FeatureSplit> {
    fn owned(names: &[&str]) -> Vec<String> {
        names.iter().map(|f| (*f).to_owned()).collect()
    }
    fn chase_knows(name: &str) -> bool {
        lookdev::chase::ON_FEATURES.contains(&name)
    }
    fn scenes_know(name: &str) -> bool {
        SCENE_FEATURES.contains(&name)
    }
    let Some(list) = list else {
        return Ok(FeatureSplit {
            chase: owned(&lookdev::chase::ON_FEATURES),
            scenes: owned(&SCENE_FEATURES),
        });
    };
    let features = split_list(list);
    if features.is_empty() {
        bail!("--features is empty");
    }
    if let Some(feature) = features
        .iter()
        .find(|f| !chase_knows(f.as_str()) && !scenes_know(f.as_str()))
    {
        let mut known = owned(&lookdev::chase::ON_FEATURES);
        known.extend(
            SCENE_FEATURES
                .iter()
                .filter(|f| !chase_knows(f))
                .map(|f| (*f).to_owned()),
        );
        bail!("unknown feature `{feature}` (known: {})", known.join(", "));
    }
    let split = FeatureSplit {
        chase: features
            .iter()
            .filter(|f| chase_knows(f.as_str()))
            .cloned()
            .collect(),
        scenes: features
            .iter()
            .filter(|f| scenes_know(f.as_str()))
            .cloned()
            .collect(),
    };
    if split.chase.is_empty() && !bench_scenes {
        bail!(
            "--features {list}: bench-scene features only ({}); add --bench-scenes",
            split.scenes.join(", ")
        );
    }
    Ok(split)
}

fn tiers_and_features(args: &BenchArgs) -> Result<(Vec<String>, FeatureSplit)> {
    let tiers = split_list(&args.tiers);
    if tiers.is_empty() {
        bail!("--tiers is empty");
    }
    if let Some(tier) = tiers.iter().find(|tier| !TIERS.contains(&tier.as_str())) {
        bail!("unknown tier `{tier}` (known: {})", TIERS.join(", "));
    }
    let features = split_features(args.features.as_deref(), args.bench_scenes.is_some())?;
    if args.rounds == 0 {
        bail!("--rounds must be at least 1");
    }
    Ok((tiers, features))
}

/// Rows of one `regia bench --json` report: one per scene, named `<tier>@<scene>/<feature>/r<n>`
/// after the launch's `<tier>@scenes/<feature>/r<n>`. A report that is not `ok` gives one `fail`
/// row for the launch.
#[must_use]
pub fn scene_rows(launch: &str, report: &str) -> Vec<Row> {
    let fail = |note: String| {
        vec![Row {
            scenario: launch.to_owned(),
            status: "fail".to_owned(),
            exit_code: None,
            metrics: Metrics::default(),
            passes: Vec::new(),
            memory: None,
            note,
        }]
    };
    let Some((tier, feature, round)) = parse_scenario(launch) else {
        return fail("not a scene launch".to_owned());
    };
    let tier = tier.strip_suffix("@scenes").unwrap_or(tier);
    let value: serde_json::Value = match serde_json::from_str(report) {
        Ok(value) => value,
        Err(err) => return fail(format!("no JSON report: {err}")),
    };
    if value["ok"] != true {
        return fail(format!(
            "regia bench: {} {}",
            value["code"].as_str().unwrap_or("?"),
            value["message"].as_str().unwrap_or("")
        ));
    }
    let empty = Vec::new();
    value["scenes"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .map(|scene| {
            let id = scene["id"].as_str().unwrap_or("?");
            let gpu = &scene["gpu_frame_ms"];
            let passes = match (gpu["p50"].as_f64(), gpu["p99"].as_f64()) {
                (Some(p50), Some(p99)) => vec![PassStat {
                    name: "gpu_frame".to_owned(),
                    p50,
                    p95: gpu["p95"].as_f64().unwrap_or(p99),
                    p99,
                }],
                _ => Vec::new(),
            };
            let memory = scene["gpu_memory"]["allocated_bytes"]
                .as_u64()
                .map(|allocated_bytes| RunMemory {
                    allocated_bytes,
                    reserved_bytes: scene["gpu_memory"]["reserved_bytes"].as_u64().unwrap_or(0),
                    allocs_per_frame: None,
                    rss_bytes: None,
                });
            Row {
                scenario: scenario_name(&format!("{tier}@{id}"), feature, round),
                status: "pass".to_owned(),
                exit_code: Some(0),
                metrics: Metrics {
                    p50_ms: scene["wall_ms"]["p50"].as_f64(),
                    p95_ms: scene["wall_ms"]["p95"].as_f64(),
                    p99_ms: scene["wall_ms"]["p99"].as_f64(),
                    ..Metrics::default()
                },
                passes,
                memory,
                note: String::new(),
            }
        })
        .collect()
}

/// Runs the `regia bench` launches on card `index` and returns one row per scene and launch,
/// with the adapter of the first report.
///
/// # Errors
///
/// `regia` is not a file, or a directory or file could not be written.
pub fn execute_scenes(
    args: &BenchArgs,
    index: u32,
    workspace: &Path,
    regia: &Path,
    jobs: &[Job],
) -> Result<(Vec<Row>, Option<AdapterInfo>)> {
    if !regia.is_file() {
        bail!(
            "{} is not a built regia (this command builds nothing: `cargo build --release -p regia_cli`)",
            regia.display()
        );
    }
    let timeout = args.timeout_seconds.max(1).to_string();
    let take_lock = launch_takes_lock(index, std::env::var(LOCK_HELD_ENV).ok().as_deref());
    let mut rows = Vec::new();
    let mut adapter = None;
    for job in jobs {
        fs::create_dir_all(&job.dir).with_context(|| format!("creating {}", job.dir.display()))?;
        let report_path = job.dir.join("bench.json");
        let _ = fs::remove_file(&report_path);
        let out =
            fs::File::create(&report_path).with_context(|| format!("creating {}", report_path.display()))?;
        let log_path = job.dir.join("run.log");
        let log = fs::File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
        let mut command = if take_lock {
            let mut flock = Command::new("flock");
            flock.args(flock_args(index)).arg("timeout");
            flock
        } else {
            Command::new("timeout")
        };
        let status = command
            .arg("--signal=TERM")
            .arg(&timeout)
            .arg(regia)
            .args(&job.args)
            .current_dir(workspace)
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(log))
            .status();
        let exit_code = status.ok().and_then(|status| status.code());
        let report = fs::read_to_string(&report_path).unwrap_or_default();
        if adapter.is_none()
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&report)
            && let Some(name) = value["adapter"]["name"].as_str()
        {
            adapter = Some(AdapterInfo {
                name: name.to_owned(),
                driver: value["adapter"]["driver"].as_str().unwrap_or_default().to_owned(),
            });
        }
        let mut launch = scene_rows(&job.scenario, &report);
        if exit_code != Some(0) {
            let (_, note) = judge(exit_code, None);
            for row in &mut launch {
                row.status = "fail".to_owned();
                row.exit_code = exit_code;
                // Keep `regia bench`'s own error code when the report had one.
                if row.note.is_empty() {
                    row.note.clone_from(&note);
                }
            }
        }
        rows.extend(launch);
    }
    Ok((rows, adapter))
}

/// What one run's `memory.json` says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunMemory {
    /// GPU bytes in live allocations after the last frame.
    pub allocated_bytes: u64,
    /// GPU bytes the allocator reserved.
    pub reserved_bytes: u64,
    /// Heap allocations per measured frame (`None` when the build did not count them).
    pub allocs_per_frame: Option<f64>,
    /// Process RSS after the last frame, bytes.
    pub rss_bytes: Option<u64>,
}

impl RunMemory {
    /// Reads `do27_chase --memory-json`'s document; `None` when it is not one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        // A backend with no allocator report writes zeros: no memory, not 0 bytes of it.
        if value.get("available") == Some(&serde_json::Value::Bool(false)) {
            return None;
        }
        Some(Self {
            allocated_bytes: value.get("allocated_bytes")?.as_u64()?,
            reserved_bytes: value.get("reserved_bytes")?.as_u64()?,
            allocs_per_frame: value.get("allocs_per_frame").and_then(serde_json::Value::as_f64),
            rss_bytes: value.get("rss_bytes").and_then(serde_json::Value::as_u64),
        })
    }

    /// The JSON shape in `gpu-bench.json`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "allocated_bytes": self.allocated_bytes,
            "reserved_bytes": self.reserved_bytes,
            "allocs_per_frame": self.allocs_per_frame,
            "rss_bytes": self.rss_bytes,
        })
    }
}

/// One feature's cost on one tier, from the interleaved rounds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeatureCost {
    /// Tier (`LOOK_R2_TIER` name).
    pub tier: String,
    /// Feature (`base` for the plain frame, whose deltas are 0).
    pub feature: String,
    /// Rounds in which both the feature and the plain frame passed.
    pub rounds: u32,
    /// Median `gpu_frame` p50 of the feature's runs, ms.
    pub gpu_p50: Option<f64>,
    /// Median over rounds of the `gpu_frame` p50 difference to the plain frame, ms.
    pub gpu_p50_delta: Option<f64>,
    /// Median over rounds of the `gpu_frame` p99 difference to the plain frame, ms.
    pub gpu_p99_delta: Option<f64>,
    /// Median over rounds of the CPU-inclusive frame p50 difference, ms.
    pub frame_p50_delta: Option<f64>,
    /// Median heap allocations per measured frame of the feature's runs.
    pub allocs_per_frame: Option<f64>,
    /// Median of the per-round difference in allocations per frame.
    pub allocs_delta: Option<f64>,
    /// Median GPU memory allocated after the last frame, MiB.
    pub vram_mib: Option<f64>,
    /// Median of the per-round difference in allocated GPU memory, MiB.
    pub vram_delta_mib: Option<f64>,
}

impl FeatureCost {
    /// [`verdict`] of the p50 cost against the feature's stated budget.
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        verdict(
            self.gpu_p50_delta,
            budget_of(&self.feature).and_then(|budget| budget.ms),
        )
    }
}

/// The median; `None` for no values.
#[must_use]
pub fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    })
}

/// One passed run's numbers.
#[derive(Clone, Copy, Debug, Default)]
struct RunNumbers {
    gpu_p50: Option<f64>,
    gpu_p99: Option<f64>,
    frame_p50: Option<f64>,
    allocs: Option<f64>,
    vram_mib: Option<f64>,
}

fn numbers(row: &Row) -> RunNumbers {
    let gpu = row.passes.iter().find(|pass| pass.name == "gpu_frame");
    RunNumbers {
        gpu_p50: gpu.map(|pass| pass.p50),
        gpu_p99: gpu.map(|pass| pass.p99),
        frame_p50: row.metrics.p50_ms,
        allocs: row.memory.as_ref().and_then(|memory| memory.allocs_per_frame),
        vram_mib: row
            .memory
            .as_ref()
            .map(|memory| memory.allocated_bytes as f64 / (1024.0 * 1024.0)),
    }
}

fn delta(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(a? - b?)
}

/// Each tier's and feature's cost from the suite's rows, in tier order of first appearance and
/// then feature order (the plain frame first). Failed runs are left out; a round counts for a
/// feature only when its plain frame passed too.
#[must_use]
pub fn summarize(rows: &[Row]) -> Vec<FeatureCost> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut runs: BTreeMap<(String, String, u32), RunNumbers> = BTreeMap::new();
    for row in rows {
        let Some((tier, feature, round)) = parse_scenario(&row.scenario) else {
            continue;
        };
        let key = (tier.to_owned(), feature.to_owned());
        if !order.contains(&key) {
            order.push(key);
        }
        if row.status == "pass" {
            runs.insert((tier.to_owned(), feature.to_owned(), round), numbers(row));
        }
    }
    order
        .into_iter()
        .map(|(tier, feature)| {
            let mut cols: [Vec<f64>; 8] = Default::default();
            let mut rounds = 0;
            for ((t, f, round), run) in &runs {
                if *t != tier || *f != feature {
                    continue;
                }
                let Some(base) = runs.get(&(tier.clone(), BASE.to_owned(), *round)) else {
                    continue;
                };
                rounds += 1;
                let values = [
                    run.gpu_p50,
                    delta(run.gpu_p50, base.gpu_p50),
                    delta(run.gpu_p99, base.gpu_p99),
                    delta(run.frame_p50, base.frame_p50),
                    run.allocs,
                    delta(run.allocs, base.allocs),
                    delta(run.vram_mib, base.vram_mib),
                    run.vram_mib,
                ];
                for (col, value) in cols.iter_mut().zip(values) {
                    col.extend(value);
                }
            }
            let [
                mut gpu,
                mut d50,
                mut d99,
                mut frame,
                mut allocs,
                mut dallocs,
                mut dvram,
                mut vram,
            ] = cols;
            FeatureCost {
                tier,
                feature,
                rounds,
                gpu_p50: median(&mut gpu),
                gpu_p50_delta: median(&mut d50),
                gpu_p99_delta: median(&mut d99),
                frame_p50_delta: median(&mut frame),
                allocs_per_frame: median(&mut allocs),
                allocs_delta: median(&mut dallocs),
                vram_mib: median(&mut vram),
                vram_delta_mib: median(&mut dvram),
            }
        })
        .collect()
}

fn num(value: Option<f64>, places: usize) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.places$}"))
}

fn signed(value: Option<f64>, places: usize) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:+.places$}"))
}

/// `features.md`: the stamp, then one row per tier and feature.
#[must_use]
pub fn markdown(stamp: &Stamp, costs: &[FeatureCost]) -> String {
    let mut out = String::new();
    out.push_str("# gpu-bench: features\n\n");
    let _ = writeln!(out, "- date: {}", stamp.date);
    let _ = writeln!(out, "- commit: {}", stamp.commit);
    let _ = writeln!(out, "- adapter: {}", stamp.adapter);
    let _ = writeln!(out, "- driver: {}", stamp.driver);
    if let Some(index) = stamp.gpu_index {
        let _ = writeln!(out, "- REGIA_GPU_INDEX: {index}");
    }
    out.push_str(
        "\nCost = median over the interleaved rounds of (feature run - plain run of the same round), \
         one card. gpu = the frame's GPU timestamps; frame = CPU-inclusive frame time.\n\n",
    );
    out.push_str(
        "| tier | feature | rounds | gpu p50 ms | cost p50 ms | cost p99 ms | frame p50 delta ms | \
         allocs/frame | allocs delta | VRAM MiB | VRAM delta MiB | budget ms | verdict |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for cost in costs {
        let budget = match budget_of(&cost.feature) {
            Some(Budget {
                ms: Some(ms), source, ..
            }) => format!("{ms} ({source})"),
            Some(Budget { ms: None, .. }) => "none stated".to_owned(),
            None => "-".to_owned(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            cost.tier,
            cost.feature,
            cost.rounds,
            num(cost.gpu_p50, 3),
            signed(cost.gpu_p50_delta, 3),
            signed(cost.gpu_p99_delta, 3),
            signed(cost.frame_p50_delta, 3),
            num(cost.allocs_per_frame, 1),
            signed(cost.allocs_delta, 1),
            num(cost.vram_mib, 1),
            signed(cost.vram_delta_mib, 1),
            budget,
            cost.verdict(),
        );
    }
    out
}

/// `features.json`: the stamp and one object per tier and feature.
#[must_use]
pub fn json(stamp: &Stamp, costs: &[FeatureCost]) -> String {
    let rows: Vec<serde_json::Value> = costs
        .iter()
        .map(|cost| {
            serde_json::json!({
                "tier": cost.tier,
                "feature": cost.feature,
                "rounds": cost.rounds,
                "gpu_p50_ms": cost.gpu_p50,
                "cost_p50_ms": cost.gpu_p50_delta,
                "cost_p99_ms": cost.gpu_p99_delta,
                "frame_p50_delta_ms": cost.frame_p50_delta,
                "allocs_per_frame": cost.allocs_per_frame,
                "allocs_delta": cost.allocs_delta,
                "vram_mib": cost.vram_mib,
                "vram_delta_mib": cost.vram_delta_mib,
                "budget_ms": budget_of(&cost.feature).and_then(|budget| budget.ms),
                "verdict": cost.verdict(),
            })
        })
        .collect();
    let doc = serde_json::json!({
        "schema": 1,
        "suite": "features",
        "date": stamp.date,
        "commit": stamp.commit,
        "adapter": stamp.adapter,
        "driver": stamp.driver,
        "gpu_index": stamp.gpu_index,
        "costs": rows,
    });
    let mut text = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(out: &Path) -> BenchArgs {
        BenchArgs {
            suite: "features".to_owned(),
            out: out.to_path_buf(),
            bin: None,
            content: None,
            map: "switchbacks".to_owned(),
            frames: 100,
            warmup: 10,
            timeout_seconds: 60,
            date: Some("2026-11-02".to_owned()),
            dry_run: true,
            tiers: "desktop-high,desktop-ultra".to_owned(),
            features: Some("oit,radiance-cache".to_owned()),
            rounds: 2,
            bench_scenes: None,
            regia: None,
            assets: None,
            skip_chase: false,
        }
    }

    #[test]
    fn plan_interleaves_plain_and_feature_runs_per_round_and_tier() {
        let out = PathBuf::from("/o");
        let jobs = plan(&args(&out)).expect("plan");
        let names: Vec<&str> = jobs.iter().map(|job| job.scenario.as_str()).collect();
        assert_eq!(
            names,
            [
                "desktop-high/base/r1",
                "desktop-high/oit/r1",
                "desktop-high/radiance-cache/r1",
                "desktop-ultra/base/r1",
                "desktop-ultra/oit/r1",
                "desktop-ultra/radiance-cache/r1",
                "desktop-high/base/r2",
                "desktop-high/oit/r2",
                "desktop-high/radiance-cache/r2",
                "desktop-ultra/base/r2",
                "desktop-ultra/oit/r2",
                "desktop-ultra/radiance-cache/r2",
            ]
        );
        let oit = &jobs[1];
        assert!(oit.args.windows(2).any(|w| w == ["--on", "oit"]));
        assert_eq!(oit.env, [("LOOK_R2_TIER".to_owned(), "desktop-high".to_owned())]);
        let memory = out
            .join("features")
            .join("desktop-high-oit-r1")
            .join("memory.json")
            .display()
            .to_string();
        assert!(
            oit.args
                .windows(2)
                .any(|w| w[0] == "--memory-json" && w[1] == memory)
        );
        assert!(!jobs[0].args.iter().any(|arg| arg == ON_FLAG));
    }

    #[test]
    fn every_known_feature_is_planned_by_default_and_unknown_names_fail() {
        let out = PathBuf::from("/o");
        let mut a = args(&out);
        a.features = None;
        a.rounds = 1;
        a.tiers = "desktop-high".to_owned();
        let jobs = plan(&a).expect("plan");
        assert_eq!(jobs.len(), 1 + lookdev::chase::ON_FEATURES.len());
        a.features = Some("raytraced-everything".to_owned());
        assert!(plan(&a).is_err());
        a.features = None;
        a.tiers = "mobile-low".to_owned();
        assert!(plan(&a).is_err());
        a.tiers = "desktop-high".to_owned();
        a.rounds = 0;
        assert!(plan(&a).is_err());
    }

    #[test]
    fn scenario_names_round_trip() {
        let name = scenario_name("desktop-ultra", "lens-effects", 3);
        assert_eq!(parse_scenario(&name), Some(("desktop-ultra", "lens-effects", 3)));
        assert_eq!(parse_scenario("gi-full"), None);
        assert_eq!(parse_scenario("a/b/x1"), None);
    }

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }

    fn row(scenario: &str, status: &str, gpu: (f64, f64), frame: f64, allocs: f64, vram_mib: f64) -> Row {
        Row {
            scenario: scenario.to_owned(),
            status: status.to_owned(),
            exit_code: Some(0),
            metrics: Metrics {
                p50_ms: Some(frame),
                ..Metrics::default()
            },
            passes: vec![PassStat {
                name: "gpu_frame".to_owned(),
                p50: gpu.0,
                p95: gpu.0,
                p99: gpu.1,
            }],
            memory: Some(RunMemory {
                allocated_bytes: (vram_mib * 1024.0 * 1024.0) as u64,
                reserved_bytes: 0,
                allocs_per_frame: Some(allocs),
                rss_bytes: None,
            }),
            note: String::new(),
        }
    }

    #[test]
    fn summary_takes_the_median_of_per_round_differences_on_passed_pairs() {
        let rows = vec![
            row("desktop-high/base/r1", "pass", (5.0, 6.0), 7.0, 100.0, 1000.0),
            row(
                "desktop-high/radiance-cache/r1",
                "pass",
                (5.5, 6.75),
                7.25,
                104.0,
                1032.0,
            ),
            row("desktop-high/base/r2", "pass", (5.25, 6.0), 7.0, 100.0, 1000.0),
            row(
                "desktop-high/radiance-cache/r2",
                "pass",
                (5.875, 7.0),
                7.5,
                106.0,
                1032.0,
            ),
            // The third round's plain frame failed: its feature run does not count.
            row("desktop-high/base/r3", "fail", (0.0, 0.0), 0.0, 0.0, 0.0),
            row(
                "desktop-high/radiance-cache/r3",
                "pass",
                (9.0, 9.0),
                9.0,
                0.0,
                0.0,
            ),
        ];
        let costs = summarize(&rows);
        assert_eq!(costs.len(), 2);
        let base = &costs[0];
        assert_eq!((base.feature.as_str(), base.rounds), ("base", 2));
        assert_eq!(base.gpu_p50_delta, Some(0.0));
        let cache = &costs[1];
        assert_eq!(cache.rounds, 2);
        // Round deltas 0.5 and 0.625.
        assert_eq!(cache.gpu_p50_delta, Some(0.5625));
        assert_eq!(cache.gpu_p99_delta, Some(0.875));
        assert_eq!(cache.frame_p50_delta, Some(0.375));
        assert_eq!(cache.allocs_delta, Some(5.0));
        assert_eq!(cache.vram_delta_mib, Some(32.0));
        // No budget is stated for the cache: reported, not flagged.
        assert_eq!(cache.verdict(), "-");
        let text = markdown(&Stamp::default(), &costs);
        assert!(text.contains("| desktop-high | radiance-cache | 2 |"));
        assert!(text.contains("none stated"));
        let value: serde_json::Value = serde_json::from_str(&json(&Stamp::default(), &costs)).expect("json");
        assert_eq!(value["costs"][1]["cost_p50_ms"], 0.5625);
        assert!(value["costs"][1]["budget_ms"].is_null());
    }

    #[test]
    fn verdict_flags_only_a_cost_over_a_stated_budget() {
        assert_eq!(verdict(Some(0.6), Some(0.5)), "OVER");
        assert_eq!(verdict(Some(0.5), Some(0.5)), "ok");
        assert_eq!(verdict(Some(9.0), None), "-");
        assert_eq!(verdict(None, Some(0.5)), "-");
        // Every `--on` feature has a row, so the table never silently drops one.
        for feature in lookdev::chase::ON_FEATURES.iter().chain(&SCENE_FEATURES) {
            assert!(budget_of(feature).is_some(), "{feature}");
        }
        // The weather has spec 99's budgets.
        assert_eq!(budget_of("rain").and_then(|b| b.ms), Some(0.6));
        assert_eq!(budget_of("snow").and_then(|b| b.ms), Some(0.5));
        assert_eq!(budget_of("wet").and_then(|b| b.ms), Some(0.15));
    }

    #[test]
    fn features_go_to_the_launches_that_take_them() {
        let split = split_features(Some("oit,rain,thin-film"), true).expect("split");
        assert_eq!(split.chase, ["oit", "rain"]);
        assert_eq!(split.scenes, ["oit", "thin-film"]);
        // A material probe alone needs the bench scenes; the chase would run only its plain frame.
        assert!(split_features(Some("thin-film"), false).is_err());
        let scenes_only = split_features(Some("rough-transmission,transmission"), true).expect("split");
        assert!(scenes_only.chase.is_empty());
        let out = PathBuf::from("/o");
        let mut a = args(&out);
        a.features = Some("transmission".to_owned());
        a.bench_scenes = Some("toy_car".to_owned());
        a.rounds = 1;
        a.tiers = "desktop-high".to_owned();
        assert!(
            plan(&a).expect("plan").is_empty(),
            "no chase launch for a scene-only list"
        );
        let names: Vec<String> = plan_scenes(&a)
            .expect("plan")
            .into_iter()
            .map(|job| job.scenario)
            .collect();
        assert_eq!(
            names,
            [
                "desktop-high@scenes/base/r1",
                "desktop-high@scenes/transmission/r1"
            ]
        );
        // Defaults: every name of each list.
        let all = split_features(None, false).expect("split");
        assert_eq!(all.chase.len(), lookdev::chase::ON_FEATURES.len());
        assert_eq!(all.scenes.len(), SCENE_FEATURES.len());
        assert!(split_features(Some("snowplough"), true).is_err());
    }

    #[test]
    fn run_memory_reads_the_chase_document() {
        let text = r#"{"schema":1,"allocated_bytes":2048,"reserved_bytes":4096,"rss_bytes":null,"allocs_per_frame":12.5}"#;
        let memory = RunMemory::parse(text).expect("parse");
        let unavailable = r#"{"available":false,"allocated_bytes":0,"reserved_bytes":0}"#;
        assert_eq!(RunMemory::parse(unavailable), None);
        assert_eq!(memory.allocated_bytes, 2048);
        assert_eq!(memory.allocs_per_frame, Some(12.5));
        assert_eq!(RunMemory::parse("{}"), None);
        assert_eq!(memory.to_json()["reserved_bytes"], 4096);
    }

    #[test]
    fn scene_launches_run_every_scene_per_tier_feature_and_round() {
        let out = PathBuf::from("/o");
        let mut a = args(&out);
        a.bench_scenes = Some("synthetic_atrium, toy_car".to_owned());
        a.assets = Some(PathBuf::from("/a"));
        a.skip_chase = true;
        assert!(plan(&a).expect("plan").is_empty());
        let jobs = plan_scenes(&a).expect("scenes");
        assert_eq!(jobs.len(), 2 * 2 * 3);
        assert_eq!(jobs[1].scenario, "desktop-high@scenes/oit/r1");
        let list = &jobs[1].args;
        assert_eq!(&list[..3], ["bench", "synthetic_atrium", "toy_car"]);
        assert!(list.windows(2).any(|w| w == ["--on", "oit"]));
        assert!(list.windows(2).any(|w| w == ["--preset", "desktop-high"]));
        assert!(list.windows(2).any(|w| w == ["--assets", "/a"]));
        assert!(list.windows(2).any(|w| w == ["--size", SCENE_SIZE]));
        assert!(!jobs[0].args.iter().any(|arg| arg == ON_FLAG));
        a.bench_scenes = None;
        assert!(plan_scenes(&a).expect("none").is_empty());
        a.bench_scenes = Some(" , ".to_owned());
        assert!(plan_scenes(&a).is_err());
    }

    #[test]
    fn a_bench_report_becomes_one_row_per_scene() {
        let report = r#"{"ok": true, "adapter": {"name": "card", "driver": "d"}, "scenes": [
            {"id": "toy_car", "wall_ms": {"p50": 3.0, "p95": 3.5, "p99": 4.0},
             "gpu_frame_ms": {"p50": 1.5, "p95": 1.75, "p99": 2.0},
             "gpu_memory": {"allocated_bytes": 1048576, "reserved_bytes": 2097152}},
            {"id": "synthetic_atrium", "wall_ms": {"p50": 5.0}, "gpu_frame_ms": null, "gpu_memory": null}
        ]}"#;
        let rows = scene_rows("desktop-ultra@scenes/sss/r2", report);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].scenario, "desktop-ultra@toy_car/sss/r2");
        assert_eq!(rows[0].status, "pass");
        assert_eq!(rows[0].passes[0].p99, 2.0);
        assert_eq!(
            rows[0].memory.as_ref().map(|m| m.allocated_bytes),
            Some(1_048_576)
        );
        assert!(rows[1].passes.is_empty());
        assert!(rows[1].memory.is_none());
        let refused = scene_rows(
            "desktop-high@scenes/oit/r1",
            r#"{"ok": false, "code": "bench_scene_missing"}"#,
        );
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].status, "fail");
        assert!(refused[0].note.contains("bench_scene_missing"));
        assert_eq!(
            scene_rows("desktop-high@scenes/oit/r1", "not json")[0].status,
            "fail"
        );
    }

    #[test]
    fn dry_run_writes_the_features_tables() {
        let dir = regia_test_utils::unique_temp_dir("gpu-bench-features-dry");
        let a = args(&dir);
        assert!(crate::gpu_bench::run(&a, &dir).is_ok());
        let text = std::fs::read_to_string(dir.join("features.md")).expect("features.md");
        assert!(text.contains("| desktop-ultra | oit | 0 |"));
        assert!(dir.join("features.json").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
