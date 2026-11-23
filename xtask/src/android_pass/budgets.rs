// SPDX-License-Identifier: MPL-2.0
//! One verdict per budget from a finished `android-pass` run (`budgets/android-*.ron`, `docs/06`
//! § 4, ADR-1942).
//!
//! Everything here reads the run's output folder (`presets.jsonl`, `startup.tsv`, `thermal.tsv`),
//! never the device, so `cargo xtask android-budgets --run <dir>` gives the same table for an old
//! run and the tests need no phone. A row is `pass`, `fail`, or `not_measured` when the run has no
//! number for it (a skipped step). Rows whose budget is `None` are left out.
//!
//! `frame_p99` is the frame's work, not its presented interval (ADR-4947): `play.hitch_report`'s
//! `work.p99_ms`, per frame the larger of the CPU work and the GPU frame time. A vsync-paced frame
//! interval cannot be shorter than the panel's refresh interval, so the presented interval has its
//! own row, `presented_p99`: its p99 as a multiple of the refresh interval of the display the run
//! measured (`pacing.get` `refresh_hz`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::THERMAL_HEADER;
use super::phone::{STARTUP_HEADER, median};

/// The budgets of one device tier (`budgets/android-reference.ron`, `budgets/android-low.ron`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidBudgets {
    /// Tier name, for the report.
    pub tier: String,
    /// The preset the frame, hitch and memory rows are read from.
    pub preset: String,
    /// Frame work p99, ms: per frame the larger of CPU and GPU frame time (ADR-4947).
    pub frame_p99_ms: Option<f64>,
    /// Presented frame interval p99 as a multiple of the panel's refresh interval (ADR-4947).
    #[serde(default)]
    pub presented_p99_refresh_ratio: Option<f64>,
    /// Frame time p50, ms.
    pub frame_p50_ms: Option<f64>,
    /// CPU main thread p99, ms.
    pub cpu_p99_ms: Option<f64>,
    /// The longest frame of the preset window, ms (the hitch rule).
    pub longest_frame_ms: Option<f64>,
    /// Memory, MB of PSS.
    pub pss_mb: Option<f64>,
    /// Median cold start to the engine's first frame, ms.
    pub cold_start_ms: Option<f64>,
    /// Level load, ms.
    pub level_load_ms: Option<f64>,
    /// The worst thermal status allowed in the thermal run.
    pub thermal_status: Option<f64>,
    /// How far the frame p50 may grow from the first to the last thermal sample, percent.
    pub thermal_p50_growth_pct: Option<f64>,
    /// The share of frames allowed to miss their display slot, percent.
    pub pacing_late_pct: Option<f64>,
}

impl AndroidBudgets {
    /// Parses a budget file's text.
    pub fn parse(text: &str) -> Result<Self> {
        let budgets: Self = ron::from_str(text).context("parsing the budget file")?;
        let rows = budgets.limits();
        if let Some((name, limit, _)) = rows
            .iter()
            .find(|(_, l, _)| l.is_some_and(|v| !v.is_finite() || v < 0.0))
        {
            bail!("budget {name} is {limit:?}: a limit is a number >= 0");
        }
        Ok(budgets)
    }

    /// Reads a budget file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| path.display().to_string())
    }

    fn limits(&self) -> [(&'static str, Option<f64>, &'static str); 11] {
        [
            ("frame_p99", self.frame_p99_ms, "ms"),
            ("presented_p99", self.presented_p99_refresh_ratio, "x refresh"),
            ("frame_p50", self.frame_p50_ms, "ms"),
            ("cpu_main_p99", self.cpu_p99_ms, "ms"),
            ("longest_frame", self.longest_frame_ms, "ms"),
            ("memory_pss", self.pss_mb, "MB"),
            ("cold_start", self.cold_start_ms, "ms"),
            ("level_load", self.level_load_ms, "ms"),
            ("thermal_status", self.thermal_status, "status"),
            ("thermal_p50_growth", self.thermal_p50_growth_pct, "%"),
            ("pacing_late", self.pacing_late_pct, "%"),
        ]
    }
}

/// The numbers a run produced; `None` for a step that did not run or reported nothing.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Measurements {
    /// Frame work p99 at the budget's preset (`hitch.work.p99_ms`, ADR-4947).
    pub frame_p99_ms: Option<f64>,
    /// Presented frame interval p99 at the budget's preset (`hitch.p99_ms`).
    pub presented_p99_ms: Option<f64>,
    /// The display's refresh rate during the preset's window (`pacing.refresh_hz`).
    pub refresh_hz: Option<f64>,
    /// Frame time p50 at the budget's preset.
    pub frame_p50_ms: Option<f64>,
    /// CPU main thread p99 at the budget's preset.
    pub cpu_p99_ms: Option<f64>,
    /// The longest frame at the budget's preset.
    pub longest_frame_ms: Option<f64>,
    /// PSS at the budget's preset, MB.
    pub pss_mb: Option<f64>,
    /// Median cold start.
    pub cold_start_ms: Option<f64>,
    /// The worst thermal status of the thermal run.
    pub thermal_status: Option<f64>,
    /// Frame p50 growth over the thermal run, percent.
    pub thermal_p50_growth_pct: Option<f64>,
    /// Frames that missed their display slot in the preset window, percent.
    pub pacing_late_pct: Option<f64>,
}

impl Measurements {
    /// The run's numbers for `budgets`, from the files in `out`. A missing file leaves its rows
    /// unmeasured; a file that does not parse is an error.
    pub fn from_run(out: &Path, budgets: &AndroidBudgets) -> Result<Self> {
        let mut m = Self::default();
        if let Some(text) = read_optional(&out.join("presets.jsonl"))? {
            m.read_presets(&text, &budgets.preset)?;
        }
        if let Some(text) = read_optional(&out.join("startup.tsv"))? {
            m.cold_start_ms = cold_start_ms(&text)?;
        }
        if let Some(text) = read_optional(&out.join("thermal.tsv"))? {
            (m.thermal_status, m.thermal_p50_growth_pct) = thermal_numbers(&text)?;
        }
        Ok(m)
    }

    fn read_presets(&mut self, text: &str, preset: &str) -> Result<()> {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let row: Value = serde_json::from_str(line).context("presets.jsonl")?;
            if row["preset"].as_str() != Some(preset) {
                continue;
            }
            let hitch = &row["hitch"];
            self.frame_p99_ms = hitch["work"]["p99_ms"].as_f64();
            self.presented_p99_ms = hitch["p99_ms"].as_f64();
            self.refresh_hz = row["pacing"]["refresh_hz"].as_f64().filter(|hz| *hz > 0.0);
            self.frame_p50_ms = hitch["p50_ms"].as_f64();
            self.cpu_p99_ms = hitch["cpu"]["p99_ms"].as_f64();
            self.longest_frame_ms = hitch["longest_ms"].as_f64();
            self.pss_mb = row["pss_kb"].as_f64().map(|kb| kb / 1024.0);
            self.pacing_late_pct = row["pacing"]["late_pct"].as_f64();
        }
        Ok(())
    }

    /// The presented interval p99 over the panel's refresh interval: 1.0 is one refresh.
    #[must_use]
    pub fn presented_p99_refresh_ratio(&self) -> Option<f64> {
        Some(self.presented_p99_ms? * self.refresh_hz? / 1000.0)
    }

    fn value(&self, name: &str) -> Option<f64> {
        match name {
            "frame_p99" => self.frame_p99_ms,
            "presented_p99" => self.presented_p99_refresh_ratio(),
            "frame_p50" => self.frame_p50_ms,
            "cpu_main_p99" => self.cpu_p99_ms,
            "longest_frame" => self.longest_frame_ms,
            "memory_pss" => self.pss_mb,
            "cold_start" => self.cold_start_ms,
            "thermal_status" => self.thermal_status,
            "thermal_p50_growth" => self.thermal_p50_growth_pct,
            "pacing_late" => self.pacing_late_pct,
            _ => None,
        }
    }

    /// The row's numbers behind its verdict, in words (`presented_p99`: the interval and the
    /// display's refresh rate it is held to).
    fn detail(&self, name: &str, limit: f64) -> Option<String> {
        match name {
            "frame_p99" => self
                .presented_p99_ms
                .map(|p99| format!("larger of CPU and GPU frame time per frame; presented p99 {p99:.2} ms")),
            "presented_p99" => {
                let (p99, hz) = (self.presented_p99_ms?, self.refresh_hz?);
                let refresh_ms = 1000.0 / hz;
                Some(format!(
                    "p99 {p99:.2} ms at {hz:.1} Hz: refresh {refresh_ms:.2} ms, limit {:.2} ms",
                    limit * refresh_ms
                ))
            }
            _ => None,
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| path.display().to_string()),
    }
}

/// Header cell name to column index; `expected` is the header this build writes.
fn columns(text: &str, expected: &str) -> Result<BTreeMap<String, usize>> {
    let header = text.lines().next().unwrap_or_default();
    if header.split('\t').count() < 2 {
        bail!("no header (this build writes `{expected}`)");
    }
    Ok(header
        .split('\t')
        .enumerate()
        .map(|(i, c)| (c.to_owned(), i))
        .collect())
}

/// The median cold start: the engine's first frame, else `am start`'s `TotalTime`.
fn cold_start_ms(tsv: &str) -> Result<Option<f64>> {
    let cols = columns(tsv, STARTUP_HEADER).context("startup.tsv")?;
    let (Some(&kind), Some(&total), Some(&first)) =
        (cols.get("kind"), cols.get("total_ms"), cols.get("first_frame_ms"))
    else {
        bail!("startup.tsv: no kind, total_ms or first_frame_ms column");
    };
    let mut frames = Vec::new();
    let mut totals = Vec::new();
    for line in tsv.lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        if cells.get(kind) == Some(&"cold") {
            frames.push(cells.get(first).and_then(|c| c.parse().ok()));
            totals.push(cells.get(total).and_then(|c| c.parse().ok()));
        }
    }
    Ok(median(&frames).or_else(|| median(&totals)).map(|v| v as f64))
}

/// `(worst status, frame p50 growth in percent)` of `thermal.tsv`.
fn thermal_numbers(tsv: &str) -> Result<(Option<f64>, Option<f64>)> {
    let cols = columns(tsv, THERMAL_HEADER).context("thermal.tsv")?;
    let (Some(&status), Some(&p50)) = (cols.get("thermal_status"), cols.get("frame_p50_ms")) else {
        bail!("thermal.tsv: no thermal_status or frame_p50_ms column");
    };
    let mut worst: Option<f64> = None;
    let mut times = Vec::new();
    for line in tsv.lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        if let Some(v) = cells.get(status).and_then(|c| c.parse::<f64>().ok()) {
            worst = Some(worst.map_or(v, |w| w.max(v)));
        }
        if let Some(v) = cells.get(p50).and_then(|c| c.parse::<f64>().ok()) {
            times.push(v);
        }
    }
    let growth = match (times.first(), times.last()) {
        (Some(first), Some(last)) if times.len() >= 2 && *first > 0.0 => Some((last - first) * 100.0 / first),
        _ => None,
    };
    Ok((worst, growth))
}

/// A budget row's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Measured and within the budget.
    Pass,
    /// Measured and over the budget.
    Fail,
    /// The run has no number for this row.
    NotMeasured,
}

impl Verdict {
    /// The name in `budgets.json` and `budgets.tsv`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::NotMeasured => "not_measured",
        }
    }
}

/// One budget and what the run measured.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetRow {
    /// Row name (`frame_p99`, `memory_pss`, ...).
    pub name: &'static str,
    /// The budget.
    pub limit: f64,
    /// Unit of `limit` and `measured`.
    pub unit: &'static str,
    /// What the run measured.
    pub measured: Option<f64>,
    /// The outcome.
    pub verdict: Verdict,
    /// The numbers behind the verdict in words, when the row has more than one (ADR-4947).
    pub detail: Option<String>,
}

impl BudgetRow {
    /// Decimals the tables print: a ratio row's limit (1.05) is too close to one refresh for two.
    fn decimals(&self) -> usize {
        if self.unit == "x refresh" { 3 } else { 2 }
    }
}

/// The verdicts of one run.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetReport {
    /// The tier's name.
    pub tier: String,
    /// The preset the frame rows are read from.
    pub preset: String,
    /// One row per budget that is set.
    pub rows: Vec<BudgetRow>,
}

/// The header of `budgets.tsv`.
pub const BUDGET_HEADER: &str = "budget\tlimit\tunit\tmeasured\tverdict";

impl BudgetReport {
    /// Compares `measured` with `budgets`.
    #[must_use]
    pub fn evaluate(budgets: &AndroidBudgets, measured: &Measurements) -> Self {
        let rows = budgets
            .limits()
            .into_iter()
            .filter_map(|(name, limit, unit)| {
                let limit = limit?;
                let value = measured.value(name);
                let verdict = match value {
                    None => Verdict::NotMeasured,
                    Some(v) if v <= limit => Verdict::Pass,
                    Some(_) => Verdict::Fail,
                };
                Some(BudgetRow {
                    name,
                    limit,
                    unit,
                    measured: value,
                    verdict,
                    detail: measured.detail(name, limit),
                })
            })
            .collect();
        Self {
            tier: budgets.tier.clone(),
            preset: budgets.preset.clone(),
            rows,
        }
    }

    /// How many rows have `verdict`.
    #[must_use]
    pub fn count(&self, verdict: Verdict) -> usize {
        self.rows.iter().filter(|r| r.verdict == verdict).count()
    }

    /// The gate's verdict (`--enforce-budgets`): every row measured and within its budget. A row
    /// a skipped step left unmeasured does not pass a gate.
    #[must_use]
    pub fn enforced_pass(&self) -> bool {
        self.count(Verdict::Pass) == self.rows.len()
    }

    /// `budgets.json`.
    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "tier": self.tier,
            "preset": self.preset,
            "pass": self.count(Verdict::Pass),
            "fail": self.count(Verdict::Fail),
            "not_measured": self.count(Verdict::NotMeasured),
            "rows": self.rows.iter().map(|r| json!({
                "budget": r.name,
                "limit": r.limit,
                "unit": r.unit,
                "measured": r.measured,
                "verdict": r.verdict.name(),
                "detail": r.detail,
            })).collect::<Vec<_>>(),
        })
    }

    /// `budgets.tsv`.
    #[must_use]
    pub fn tsv(&self) -> String {
        let mut out = format!("{BUDGET_HEADER}\n");
        for r in &self.rows {
            let n = r.decimals();
            let measured = r.measured.map_or_else(|| "-".to_owned(), |v| format!("{v:.n$}"));
            let _ = writeln!(
                out,
                "{}\t{:.n$}\t{}\t{measured}\t{}",
                r.name,
                r.limit,
                r.unit,
                r.verdict.name()
            );
        }
        out
    }

    /// The summary lines: one per row, then the totals.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for r in &self.rows {
            let n = r.decimals();
            let measured = r
                .measured
                .map_or_else(|| "not measured".to_owned(), |v| format!("{v:.n$} {}", r.unit));
            let word = match r.verdict {
                Verdict::Pass => "PASS",
                Verdict::Fail => "FAIL",
                Verdict::NotMeasured => "NOT MEASURED",
            };
            let detail = r.detail.as_ref().map_or_else(String::new, |d| format!("; {d}"));
            let _ = writeln!(
                out,
                "budget {}: {word} ({measured}, limit {:.n$} {}{detail})",
                r.name, r.limit, r.unit
            );
        }
        let _ = writeln!(
            out,
            "budgets {} at {}: {} pass, {} fail, {} not measured",
            self.tier,
            self.preset,
            self.count(Verdict::Pass),
            self.count(Verdict::Fail),
            self.count(Verdict::NotMeasured)
        );
        out
    }

    /// Writes `budgets.json` and `budgets.tsv` into `out`.
    pub fn write(&self, out: &Path) -> Result<()> {
        std::fs::write(
            out.join("budgets.json"),
            serde_json::to_string_pretty(&self.json())?,
        )?;
        std::fs::write(out.join("budgets.tsv"), self.tsv())?;
        Ok(())
    }
}

/// `cargo xtask android-budgets`: the verdicts of a finished run.
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// The run's output folder (`target/android-pass/<timestamp>`).
    #[arg(long)]
    pub run: std::path::PathBuf,
    /// The budget file.
    #[arg(long, default_value = DEFAULT_BUDGETS)]
    pub budgets: std::path::PathBuf,
}

/// The reference tier's budget file, relative to the workspace root.
pub const DEFAULT_BUDGETS: &str = "budgets/android-reference.ron";

/// Evaluates the run in `out` and writes its budget files.
pub fn evaluate_run(out: &Path, budgets: &AndroidBudgets) -> Result<BudgetReport> {
    let measured = Measurements::from_run(out, budgets)?;
    let report = BudgetReport::evaluate(budgets, &measured);
    report.write(out)?;
    Ok(report)
}

/// Runs `cargo xtask android-budgets`: fails on a row over its budget.
pub fn run(args: &Args, root: &Path) -> Result<std::process::ExitCode> {
    let path = if args.budgets.is_absolute() {
        args.budgets.clone()
    } else {
        root.join(&args.budgets)
    };
    let budgets = AndroidBudgets::load(&path)?;
    let report = evaluate_run(&args.run, &budgets)?;
    print!("{}", report.summary());
    Ok(if report.count(Verdict::Fail) == 0 {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use regia_test_utils::unique_temp_dir;

    use super::*;

    fn reference() -> AndroidBudgets {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("root");
        AndroidBudgets::load(&root.join(DEFAULT_BUDGETS)).expect("reference budgets")
    }

    const PRESETS: &str = concat!(
        r#"{"preset":"mobile_low","hitch":{"p50_ms":11.0,"p99_ms":14.0,"longest_ms":20.0,"cpu":{"p99_ms":3.0}},"pss_kb":512000}"#,
        "\n",
        r#"{"preset":"mobile_high","hitch":{"p50_ms":16.67,"p99_ms":17.3,"longest_ms":49.0,"cpu":{"p50_ms":4.0,"p99_ms":7.5},"work":{"p50_ms":10.7,"p99_ms":13.4}},"pss_kb":650240,"pacing":{"late_pct":1.5,"refresh_hz":60.0}}"#,
        "\n"
    );
    const STARTUP: &str = "run\tkind\tlaunch_state\ttotal_ms\twait_ms\tfirst_frame_ms\n1\tcold\tCOLD\t900\t950\t4100\n2\tcold\tCOLD\t920\t960\t4300\n3\tcold\tCOLD\t910\t955\t4200\n4\twarm\tWARM\t100\t120\t300\n";

    fn thermal(first: f64, last: f64, worst: i32) -> String {
        format!(
            "{THERMAL_HEADER}\n0\t{first:.2}\t20.00\t10.00\t1.00\t0\t30.00\t40.00\t600.00\n15\t{last:.2}\t21.00\t11.00\t1.00\t{worst}\t31.00\t41.00\t610.00\n"
        )
    }

    fn full_run(presets: &str, startup: &str, thermal: &str) -> std::path::PathBuf {
        let out = unique_temp_dir("android-budgets");
        std::fs::write(out.join("presets.jsonl"), presets).expect("presets");
        std::fs::write(out.join("startup.tsv"), startup).expect("startup");
        std::fs::write(out.join("thermal.tsv"), thermal).expect("thermal");
        out
    }

    fn row<'a>(report: &'a BudgetReport, name: &str) -> &'a BudgetRow {
        report.rows.iter().find(|r| r.name == name).expect(name)
    }

    #[test]
    fn the_budget_files_match_the_docs_table() {
        let b = reference();
        assert_eq!(b.preset, "mobile_high");
        assert_eq!(b.frame_p99_ms, Some(16.6));
        assert_eq!(b.presented_p99_refresh_ratio, Some(1.05), "ADR-4947");
        assert_eq!(b.cpu_p99_ms, Some(8.0));
        assert_eq!(b.longest_frame_ms, Some(50.0));
        assert_eq!(b.pss_mb, Some(700.0));
        assert_eq!(b.cold_start_ms, Some(5000.0));
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("root");
        let low = AndroidBudgets::load(&root.join("budgets/android-low.ron")).expect("low budgets");
        assert_eq!(low.preset, "mobile_low");
        assert_eq!(low.frame_p50_ms, Some(33.3), "Mobile Low >= 30 fps");
        assert_eq!(
            low.presented_p99_refresh_ratio, None,
            "Mobile Low runs two refreshes a frame on a 60 Hz panel"
        );
    }

    #[test]
    fn a_run_inside_every_budget_passes_each_row() {
        let out = full_run(PRESETS, STARTUP, &thermal(15.0, 16.0, 2));
        let report = evaluate_run(&out, &reference()).expect("report");
        assert_eq!(report.count(Verdict::Fail), 0, "{report:?}");
        assert_eq!(report.count(Verdict::NotMeasured), 0, "{report:?}");
        assert!(report.enforced_pass());
        let names: Vec<&str> = report.rows.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "frame_p99",
                "presented_p99",
                "cpu_main_p99",
                "longest_frame",
                "memory_pss",
                "cold_start",
                "thermal_status",
                "thermal_p50_growth",
                "pacing_late"
            ],
            "a None budget has no row"
        );
        assert_eq!(row(&report, "memory_pss").measured, Some(635.0));
        assert_eq!(
            row(&report, "cold_start").measured,
            Some(4200.0),
            "median of the cold rows"
        );
        assert!(out.join("budgets.json").is_file());
        assert!(out.join("budgets.tsv").is_file());
    }

    #[test]
    fn each_row_fails_on_its_own_number() {
        let cases: [(&str, String, String, String); 9] = [
            (
                "frame_p99",
                PRESETS.replace("\"p99_ms\":13.4", "\"p99_ms\":16.7"),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 2),
            ),
            (
                "presented_p99",
                PRESETS.replace("\"p99_ms\":17.3", "\"p99_ms\":17.6"),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 2),
            ),
            (
                "cpu_main_p99",
                PRESETS.replace("\"p99_ms\":7.5", "\"p99_ms\":8.1"),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 2),
            ),
            (
                "longest_frame",
                PRESETS.replace("\"longest_ms\":49.0", "\"longest_ms\":50.1"),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 2),
            ),
            (
                "memory_pss",
                PRESETS.replace("650240", "720000"),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 2),
            ),
            (
                "cold_start",
                PRESETS.to_owned(),
                STARTUP
                    .replace("4100", "6100")
                    .replace("4300", "6300")
                    .replace("4200", "6200"),
                thermal(15.0, 16.0, 2),
            ),
            (
                "thermal_status",
                PRESETS.to_owned(),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 3),
            ),
            (
                "thermal_p50_growth",
                PRESETS.to_owned(),
                STARTUP.to_owned(),
                thermal(15.0, 19.0, 2),
            ),
            (
                "pacing_late",
                PRESETS.replace("\"late_pct\":1.5", "\"late_pct\":9.0"),
                STARTUP.to_owned(),
                thermal(15.0, 16.0, 2),
            ),
        ];
        for (name, presets, startup, thermal) in cases {
            let out = full_run(&presets, &startup, &thermal);
            let report = evaluate_run(&out, &reference()).expect("report");
            let failed: Vec<&str> = report
                .rows
                .iter()
                .filter(|r| r.verdict == Verdict::Fail)
                .map(|r| r.name)
                .collect();
            assert_eq!(failed, [name], "{report:?}");
            assert!(!report.enforced_pass());
        }
    }

    #[test]
    fn a_limit_is_inclusive() {
        let m = Measurements {
            frame_p99_ms: Some(16.6),
            ..Measurements::default()
        };
        let report = BudgetReport::evaluate(&reference(), &m);
        assert_eq!(row(&report, "frame_p99").verdict, Verdict::Pass);
    }

    /// ADR-4947: the TEST Pixel 9 Pro's arena run (60 Hz panel, 60 fps). Every presented interval
    /// is one refresh plus jitter, so the old `frame_p99` (presented p99 <= 16.6 ms) failed by
    /// construction; the frame's work fits the budget, and the presented row holds the interval to
    /// 1.05 refreshes of the display the run measured.
    #[test]
    fn frame_p99_reads_the_work_and_presented_p99_the_panel() {
        let at = |presented: f64, work: f64, hz: f64| Measurements {
            frame_p99_ms: Some(work),
            presented_p99_ms: Some(presented),
            refresh_hz: Some(hz),
            ..Measurements::default()
        };
        let verdicts = |m: &Measurements| {
            let report = BudgetReport::evaluate(&reference(), m);
            (
                row(&report, "frame_p99").verdict,
                row(&report, "presented_p99").verdict,
            )
        };
        assert_eq!(
            verdicts(&at(17.4, 13.5, 60.0)),
            (Verdict::Pass, Verdict::Pass),
            "17.4 ms is 1.044 refreshes at 60 Hz"
        );
        assert_eq!(
            verdicts(&at(17.7, 13.5, 60.0)),
            (Verdict::Pass, Verdict::Fail),
            "17.7 ms is 1.062 refreshes"
        );
        assert_eq!(
            verdicts(&at(17.4, 16.7, 60.0)),
            (Verdict::Fail, Verdict::Pass),
            "work over 16.6 ms fails however the panel paces it"
        );
        assert_eq!(
            verdicts(&at(8.6, 8.0, 120.0)),
            (Verdict::Pass, Verdict::Pass),
            "the refresh rate is the display's own"
        );
        assert_eq!(
            verdicts(&at(16.7, 8.0, 120.0)),
            (Verdict::Pass, Verdict::Fail),
            "every other refresh at 120 Hz is two refreshes"
        );
        let report = BudgetReport::evaluate(&reference(), &at(17.3, 13.4, 60.0));
        let presented = row(&report, "presented_p99");
        assert!((presented.measured.expect("ratio") - 1.038).abs() < 1e-9);
        assert_eq!(presented.unit, "x refresh");
        let detail = presented.detail.as_deref().expect("detail");
        assert!(
            detail.contains("17.30 ms at 60.0 Hz") && detail.contains("limit 17.50 ms"),
            "{detail}"
        );
        assert!(
            report
                .summary()
                .contains("budget presented_p99: PASS (1.038 x refresh, limit 1.050 x refresh; p99 17.30 ms"),
            "{}",
            report.summary()
        );
    }

    /// ADR-4947: a run from before `work` existed, or with no refresh rate, leaves the rows
    /// unmeasured; `frame_p99` never falls back to the presented interval.
    #[test]
    fn a_run_without_work_or_refresh_rate_leaves_the_rows_unmeasured() {
        let old = PRESETS
            .replace(r#","work":{"p50_ms":10.7,"p99_ms":13.4}"#, "")
            .replace(r#","refresh_hz":60.0"#, "");
        assert_ne!(old, PRESETS);
        let out = full_run(&old, STARTUP, &thermal(15.0, 16.0, 2));
        let report = evaluate_run(&out, &reference()).expect("report");
        assert_eq!(row(&report, "frame_p99").verdict, Verdict::NotMeasured);
        assert_eq!(row(&report, "presented_p99").verdict, Verdict::NotMeasured);
        assert_eq!(row(&report, "cpu_main_p99").verdict, Verdict::Pass);
        assert!(!report.enforced_pass());
        let zero = PRESETS.replace(r#""refresh_hz":60.0"#, r#""refresh_hz":0.0"#);
        let out = full_run(&zero, STARTUP, &thermal(15.0, 16.0, 2));
        let report = evaluate_run(&out, &reference()).expect("report");
        assert_eq!(
            row(&report, "presented_p99").verdict,
            Verdict::NotMeasured,
            "0 Hz is no rate"
        );
    }

    #[test]
    fn a_skipped_step_is_not_measured_and_does_not_pass_a_gate() {
        let out = unique_temp_dir("android-budgets-empty");
        let report = evaluate_run(&out, &reference()).expect("report");
        assert_eq!(report.count(Verdict::Fail), 0);
        assert_eq!(report.count(Verdict::NotMeasured), report.rows.len());
        assert!(!report.enforced_pass(), "an empty run passes no gate");
        assert!(
            report.summary().contains("budget frame_p99: NOT MEASURED"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn the_cold_start_falls_back_to_am_start() {
        let startup =
            "run\tkind\tlaunch_state\ttotal_ms\twait_ms\tfirst_frame_ms\n1\tcold\tCOLD\t900\t950\t-\n";
        assert_eq!(cold_start_ms(startup).expect("parses"), Some(900.0));
        assert_eq!(cold_start_ms(STARTUP_HEADER).expect("no rows"), None);
    }

    #[test]
    fn one_thermal_sample_has_no_growth() {
        let tsv = format!("{THERMAL_HEADER}\n0\t15.00\t-\t-\t-\t1\t-\t-\t-\n");
        assert_eq!(thermal_numbers(&tsv).expect("parses"), (Some(1.0), None));
        let blank = format!("{THERMAL_HEADER}\n0\t-\t-\t-\t-\t-\t-\t-\t-\n");
        assert_eq!(thermal_numbers(&blank).expect("parses"), (None, None));
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_silent_row() {
        let out = full_run("not json\n", STARTUP, &thermal(15.0, 16.0, 2));
        assert!(evaluate_run(&out, &reference()).is_err());
        let out = full_run(PRESETS, "not a table\n", &thermal(15.0, 16.0, 2));
        assert!(evaluate_run(&out, &reference()).is_err());
        assert!(
            AndroidBudgets::parse("(tier: \"x\")").is_err(),
            "every row is named"
        );
        let bad =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../budgets/android-low.ron"))
                .expect("low")
                .replace("Some(33.3)", "Some(-1.0)");
        assert!(AndroidBudgets::parse(&bad).is_err(), "a negative limit");
    }

    #[test]
    fn the_command_fails_on_a_row_over_budget_only() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("root");
        let exit = |presets: &str| {
            let args = Args {
                run: full_run(presets, STARTUP, &thermal(15.0, 16.0, 2)),
                budgets: DEFAULT_BUDGETS.into(),
            };
            run(&args, root).expect("runs") == std::process::ExitCode::SUCCESS
        };
        assert!(exit(PRESETS));
        assert!(!exit(&PRESETS.replace("\"p99_ms\":13.4", "\"p99_ms\":17.0")));
        let unmeasured = Args {
            run: unique_temp_dir("android-budgets-none"),
            budgets: DEFAULT_BUDGETS.into(),
        };
        assert!(
            run(&unmeasured, root).expect("runs") == std::process::ExitCode::SUCCESS,
            "nothing measured is reported, not a failure, outside --enforce-budgets"
        );
    }

    #[test]
    fn the_tables_have_one_cell_per_header_column() {
        let out = full_run(PRESETS, STARTUP, &thermal(15.0, 16.0, 2));
        let report = evaluate_run(&out, &reference()).expect("report");
        let tsv = report.tsv();
        let width = BUDGET_HEADER.split('\t').count();
        assert!(tsv.lines().all(|l| l.split('\t').count() == width), "{tsv}");
        let json = report.json();
        assert_eq!(json["rows"].as_array().expect("rows").len(), report.rows.len());
        assert_eq!(json["rows"][0]["verdict"], "pass");
        assert_eq!(json["rows"][1]["budget"], "presented_p99");
        assert!(
            json["rows"][1]["detail"]
                .as_str()
                .is_some_and(|d| d.contains("60.0 Hz"))
        );
        assert_eq!(json["fail"], 0);
    }
}
