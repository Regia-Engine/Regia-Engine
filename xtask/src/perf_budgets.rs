// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask perf-budgets`: ties every spec CPU budget to a runnable measurement (ADR-2704,
//! `docs/06-testing-and-ci.md` § 9, `docs/formats/perf-budgets-ron.md`).
//!
//! `docs/perf/budgets.ron` names, per budget: the spec section, a release `cargo test` or
//! `cargo bench` command, a regex for the metric in its output, the statistic to read and the budget.
//! The command runs once however many entries share it, and each entry's metric is read from its
//! output. Time is compared twice: with the spec's budget and with a per-host baseline
//! (`docs/perf/budget-baseline-<host>.json`), since a number only compares with one from the same
//! machine. Allocation counts (`perf-count <name>: ops= allocs= bytes=`, from a counting global
//! allocator in the measuring binary) do not depend on the machine, so they share one baseline
//! (`docs/perf/budget-counts.json`) and `--counts-only --enforce` can gate on any host.
//!
//! The exit code is non-zero only with `--enforce`, or when the registry itself is unusable.
//! Everything except the process spawn is a pure function, so it is unit-tested without running cargo.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

/// Version of `docs/perf/budgets.ron`.
pub const PERF_BUDGETS_FORMAT_VERSION: u32 = 1;
/// Version of the baseline files.
pub const BASELINE_VERSION: u32 = 1;
/// The registry, relative to the workspace root.
pub const REGISTRY_PATH: &str = "docs/perf/budgets.ron";
/// The machine-independent allocation count baseline, relative to the workspace root.
pub const COUNTS_BASELINE_PATH: &str = "docs/perf/budget-counts.json";

/// The registry file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// Format version ([`PERF_BUDGETS_FORMAT_VERSION`]).
    pub format_version: u32,
    /// The measurements, in report order.
    pub entries: Vec<Entry>,
}

/// One budgeted measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Unique name: lowercase ascii, digits, `_`, `.` and `-`.
    pub name: String,
    /// Where the budget is written, for example `spec 81 § 9`.
    pub spec: String,
    /// The crate measured; `-p` of the command.
    pub package: String,
    /// The release command, split on whitespace: `cargo test ... --release ...`, `cargo bench ...` or
    /// `cargo run --release ... --example ...`.
    pub command: String,
    /// Regex (multi-line mode) that must match exactly once in the command's combined output.
    pub metric: String,
    /// What to read from the match.
    pub stat: Stat,
    /// The budget.
    pub budget: Budget,
    /// Conditions a reader needs (reference machine, why it may miss on a shared host).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// The statistic an entry compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stat {
    /// `p50=<duration>` of a `perf-micro` line.
    P50,
    /// `p99=<duration>` of a `perf-micro` line.
    P99,
    /// `max=<duration>` of a `perf-micro` line.
    Max,
    /// The central estimate of criterion's `time: [low estimate high]`.
    CriterionEstimate,
    /// The regex's named groups `value` and `unit` (ns, us, µs, ms, s).
    Capture,
    /// `allocs / ops` of a `perf-count` line.
    AllocsPerOp,
    /// `bytes / ops` of a `perf-count` line.
    BytesPerOp,
}

impl Stat {
    /// A count (machine-independent) and not a time.
    #[must_use]
    pub fn is_count(self) -> bool {
        matches!(self, Stat::AllocsPerOp | Stat::BytesPerOp)
    }

    /// The name in reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Stat::P50 => "p50",
            Stat::P99 => "p99",
            Stat::Max => "max",
            Stat::CriterionEstimate => "estimate",
            Stat::Capture => "value",
            Stat::AllocsPerOp => "allocs/op",
            Stat::BytesPerOp => "bytes/op",
        }
    }
}

/// A budget: an upper limit, reached inclusively.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Budget {
    /// Nanoseconds.
    Ns(f64),
    /// Microseconds.
    Us(f64),
    /// Milliseconds.
    Ms(f64),
    /// Seconds.
    S(f64),
    /// A count, for the count statistics.
    Count(f64),
    /// No spec number: only the baseline compares it.
    Unbudgeted,
}

impl Budget {
    /// The limit in the unit rows use: nanoseconds for a time, the plain number for a count.
    #[must_use]
    pub fn limit(self) -> Option<f64> {
        match self {
            Budget::Ns(v) | Budget::Count(v) => Some(v),
            Budget::Us(v) => Some(v * 1e3),
            Budget::Ms(v) => Some(v * 1e6),
            Budget::S(v) => Some(v * 1e9),
            Budget::Unbudgeted => None,
        }
    }

    fn is_time(self) -> bool {
        matches!(self, Budget::Ns(_) | Budget::Us(_) | Budget::Ms(_) | Budget::S(_))
    }

    fn is_count(self) -> bool {
        matches!(self, Budget::Count(_))
    }
}

/// The registry's serde shape for `cargo xtask check-formats`.
#[must_use]
pub fn perf_budgets_schema_fingerprint() -> String {
    use regia_core::schema::{
        join_field_lists, serde_field_names, serde_variant_names, serde_variant_shapes,
    };
    join_field_lists(&[
        serde_field_names::<Registry>(),
        serde_field_names::<Entry>(),
        serde_variant_names::<Stat>(),
        serde_variant_names::<Budget>(),
        serde_variant_shapes::<Budget>(),
    ])
}

/// Parses registry text: refuses another format version and anything [`Registry::problems`] finds.
///
/// # Errors
///
/// Bad RON, an unknown version, or a list of invalid entries.
pub fn parse_registry(text: &str) -> Result<Registry> {
    let registry: Registry = ron::from_str(text).context("parsing the perf budget registry")?;
    if registry.format_version != PERF_BUDGETS_FORMAT_VERSION {
        bail!(
            "perf budget registry: format_version {} is not the supported {PERF_BUDGETS_FORMAT_VERSION}",
            registry.format_version
        );
    }
    let problems = registry.problems();
    if !problems.is_empty() {
        bail!("perf budget registry:\n  {}", problems.join("\n  "));
    }
    Ok(registry)
}

/// Compiles a metric regex in the mode the harness matches with.
fn metric_regex(pattern: &str) -> std::result::Result<Regex, regex::Error> {
    RegexBuilder::new(pattern).multi_line(true).build()
}

impl Registry {
    /// Every problem of the entries: names, commands, regexes, and a budget of the wrong kind.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut names = BTreeSet::new();
        for entry in &self.entries {
            let name = &entry.name;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
            {
                out.push(format!(
                    "`{name}`: a name is lowercase ascii, digits, `_`, `.` and `-`"
                ));
            }
            if !names.insert(name.as_str()) {
                out.push(format!("`{name}`: appears twice"));
            }
            if entry.spec.trim().is_empty() {
                out.push(format!("`{name}`: no spec reference"));
            }
            out.extend(command_problem(entry).map(|p| format!("`{name}`: {p}")));
            match metric_regex(&entry.metric) {
                Err(err) => out.push(format!("`{name}`: metric regex: {err}")),
                Ok(re) if entry.stat == Stat::Capture => {
                    let groups: Vec<&str> = re.capture_names().flatten().collect();
                    if !groups.contains(&"value") || !groups.contains(&"unit") {
                        out.push(format!(
                            "`{name}`: stat Capture needs the groups `value` and `unit`"
                        ));
                    }
                }
                Ok(_) => {}
            }
            let kind_ok = if entry.stat.is_count() {
                !entry.budget.is_time()
            } else {
                !entry.budget.is_count()
            };
            if !kind_ok {
                out.push(format!(
                    "`{name}`: budget {:?} does not fit stat {:?}",
                    entry.budget, entry.stat
                ));
            }
            if entry.budget.limit().is_some_and(|v| !v.is_finite() || v < 0.0) {
                out.push(format!("`{name}`: a budget is a finite number at or above zero"));
            }
        }
        out
    }
}

/// The command's problem: only release cargo measurements of the entry's own package may run.
fn command_problem(entry: &Entry) -> Option<String> {
    let args: Vec<&str> = entry.command.split_whitespace().collect();
    match args.as_slice() {
        ["cargo", "bench", ..] => {}
        ["cargo", verb @ ("test" | "run"), rest @ ..] => {
            let cargo_args = rest.split(|a| *a == "--").next().unwrap_or_default();
            if !cargo_args.contains(&"--release") {
                return Some(format!(
                    "`cargo {verb}` without `--release` measures a debug build"
                ));
            }
            if *verb == "run" && !cargo_args.contains(&"--example") {
                return Some("`cargo run` only runs an `--example` bench".into());
            }
        }
        _ => return Some("a command starts with `cargo test`, `cargo bench` or `cargo run`".into()),
    }
    let package = args.iter().position(|a| *a == "-p").and_then(|i| args.get(i + 1));
    if package != Some(&entry.package.as_str()) {
        return Some(format!("the command does not say `-p {}`", entry.package));
    }
    None
}

/// Whether the entry is selected by `--filter` (a substring of its name, spec or package).
fn selected(entry: &Entry, filter: Option<&str>, counts_only: bool) -> bool {
    if counts_only && !entry.stat.is_count() {
        return false;
    }
    filter.is_none_or(|f| entry.name.contains(f) || entry.spec.contains(f) || entry.package.contains(f))
}

/// The commands of `entries` in first-use order, each once.
#[must_use]
pub fn unique_commands(entries: &[&Entry]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    entries
        .iter()
        .filter(|e| seen.insert(e.command.as_str()))
        .map(|e| e.command.clone())
        .collect()
}

// ---- reading metrics -------------------------------------------------------------------------

/// Nanoseconds of a value and unit.
fn unit_ns(value: f64, unit: &str) -> Option<f64> {
    let scale = match unit {
        "ns" => 1.0,
        "us" | "µs" | "μs" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        _ => return None,
    };
    Some(value * scale)
}

/// Nanoseconds of a duration in `Debug` form or with a space: `12.5µs`, `3 ms`, `1.25s`, `800ns`.
#[must_use]
pub fn parse_duration_ns(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    unit_ns(number.parse().ok()?, unit.trim())
}

/// The text after `key=` up to the next space or comma.
fn key_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("{key}=");
    let at = text.match_indices(&pattern).find_map(|(i, _)| {
        let word_start = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        word_start.then_some(i + pattern.len())
    })?;
    let rest = &text[at..];
    let end = rest.find([' ', ',', '\n', '\r']).unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Why a metric could not be read. `code` is what the JSON report carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricError {
    /// Stable lowercase code: `metric_missing`, `metric_ambiguous` or `metric_unreadable`.
    pub code: &'static str,
    /// What was wrong.
    pub text: String,
}

fn unreadable(text: String) -> MetricError {
    MetricError {
        code: "metric_unreadable",
        text,
    }
}

/// Reads the entry's metric from a command's output: nanoseconds for a time, the number for a count.
///
/// # Errors
///
/// The regex matched no line or more than one, or the matched text does not hold the statistic.
pub fn read_metric(entry: &Entry, output: &str) -> std::result::Result<f64, MetricError> {
    let re = metric_regex(&entry.metric).map_err(|e| unreadable(format!("regex: {e}")))?;
    let mut matches = re.captures_iter(output);
    let Some(found) = matches.next() else {
        return Err(MetricError {
            code: "metric_missing",
            text: format!("no output matches `{}`", entry.metric),
        });
    };
    let extra = matches.count();
    if extra > 0 {
        return Err(MetricError {
            code: "metric_ambiguous",
            text: format!(
                "{} matches of `{}`; a metric matches once",
                extra + 1,
                entry.metric
            ),
        });
    }
    let text = found.get(0).map_or("", |m| m.as_str());
    let bad = || unreadable(format!("cannot read {:?} from `{text}`", entry.stat));
    match entry.stat {
        Stat::P50 | Stat::P99 | Stat::Max => {
            let key = entry.stat.label();
            key_value(text, key).and_then(parse_duration_ns).ok_or_else(bad)
        }
        Stat::CriterionEstimate => {
            let inner = text.split_once("time:").map(|(_, tail)| tail).ok_or_else(bad)?;
            let open = inner.find('[').ok_or_else(bad)?;
            let close = inner.find(']').ok_or_else(bad)?;
            let words: Vec<&str> = inner[open + 1..close].split_whitespace().collect();
            match words.as_slice() {
                [_, _, value, unit, _, _] => {
                    value.parse().ok().and_then(|v| unit_ns(v, unit)).ok_or_else(bad)
                }
                _ => Err(bad()),
            }
        }
        Stat::Capture => {
            let value = found.name("value").map(|m| m.as_str()).ok_or_else(bad)?;
            let unit = found.name("unit").map(|m| m.as_str()).ok_or_else(bad)?;
            value.parse().ok().and_then(|v| unit_ns(v, unit)).ok_or_else(bad)
        }
        Stat::AllocsPerOp | Stat::BytesPerOp => {
            let key = if entry.stat == Stat::AllocsPerOp {
                "allocs"
            } else {
                "bytes"
            };
            let value: f64 = key_value(text, key)
                .and_then(|v| v.parse().ok())
                .ok_or_else(bad)?;
            let ops: f64 = key_value(text, "ops")
                .and_then(|v| v.parse().ok())
                .ok_or_else(bad)?;
            if ops > 0.0 { Ok(value / ops) } else { Err(bad()) }
        }
    }
}

// ---- baselines ---------------------------------------------------------------------------------

/// A baseline file. Times are nanoseconds; counts are per operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// File version ([`BASELINE_VERSION`]).
    pub version: u32,
    /// Where and from what the numbers were recorded (free text).
    #[serde(default)]
    pub recorded_on: String,
    /// Value by entry name.
    pub entries: BTreeMap<String, f64>,
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            version: BASELINE_VERSION,
            recorded_on: String::new(),
            entries: BTreeMap::new(),
        }
    }
}

/// Reads a baseline; a missing file is an empty baseline, since a host's first run has none.
///
/// # Errors
///
/// A file that cannot be read or parsed, or another version.
pub fn read_baseline(path: &Path) -> Result<Baseline> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Baseline::default()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let baseline: Baseline =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if baseline.version != BASELINE_VERSION {
        bail!(
            "{}: unsupported baseline version {}",
            path.display(),
            baseline.version
        );
    }
    Ok(baseline)
}

/// Writes text atomically (temp file, then rename).
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn write_baseline(path: &Path, baseline: &Baseline) -> Result<()> {
    let mut text = serde_json::to_string_pretty(baseline)?;
    text.push('\n');
    write_atomic(path, &text)
}

/// The host name as a file-name part: lowercase ascii, digits and `-`.
#[must_use]
pub fn sanitize_host(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "unknown".into()
    } else {
        cleaned.into()
    }
}

/// This machine's name: `/etc/hostname`, else `HOSTNAME` or `COMPUTERNAME`.
fn detect_host() -> String {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default();
    sanitize_host(&raw)
}

/// The per-host baseline path, relative to the workspace root.
#[must_use]
pub fn host_baseline_path(host: &str) -> String {
    format!("docs/perf/budget-baseline-{host}.json")
}

// ---- comparing ---------------------------------------------------------------------------------

/// The measurement against the spec's budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetVerdict {
    /// At or under the budget.
    Within,
    /// Over the budget.
    Over,
    /// The entry has no budget.
    Unbudgeted,
    /// No number was read.
    NotMeasured,
}

/// The measurement against this host's baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineVerdict {
    /// Inside the tolerance.
    Ok,
    /// Worse than the tolerance allows.
    Regressed,
    /// Better than the tolerance allows: not a failure, but the baseline is stale.
    Improved,
    /// The baseline has no number.
    NoBaseline,
    /// No number was read.
    NotMeasured,
}

/// Compares with a budget. Reaching the budget exactly is within it.
#[must_use]
pub fn budget_verdict(measured: Option<f64>, limit: Option<f64>) -> BudgetVerdict {
    match (measured, limit) {
        (None, _) => BudgetVerdict::NotMeasured,
        (Some(_), None) => BudgetVerdict::Unbudgeted,
        (Some(m), Some(l)) if m <= l => BudgetVerdict::Within,
        (Some(_), Some(_)) => BudgetVerdict::Over,
    }
}

/// Compares with a baseline at `tolerance` (the allowed ratio, at least 1). A zero baseline means an
/// operation that did not allocate: any count above it is a regression, and a time of zero is no
/// baseline at all.
#[must_use]
pub fn baseline_verdict(
    measured: Option<f64>,
    base: Option<f64>,
    tolerance: f64,
    count: bool,
) -> BaselineVerdict {
    let Some(m) = measured else {
        return BaselineVerdict::NotMeasured;
    };
    match base {
        None => BaselineVerdict::NoBaseline,
        Some(b) if b <= 0.0 => {
            if !count {
                BaselineVerdict::NoBaseline
            } else if m > 0.0 {
                BaselineVerdict::Regressed
            } else {
                BaselineVerdict::Ok
            }
        }
        Some(b) if m > b * tolerance => BaselineVerdict::Regressed,
        Some(b) if m * tolerance < b => BaselineVerdict::Improved,
        Some(_) => BaselineVerdict::Ok,
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    /// Entry name.
    pub name: String,
    /// Spec reference.
    pub spec: String,
    /// Crate.
    pub package: String,
    /// Statistic label.
    pub stat: String,
    /// `time` (values in nanoseconds) or `count` (values per operation).
    pub kind: String,
    /// The budget.
    pub budget: Option<f64>,
    /// The measurement.
    pub measured: Option<f64>,
    /// `(budget - measured) / budget`, percent: negative is over budget.
    pub margin_percent: Option<f64>,
    /// The baseline number.
    pub baseline: Option<f64>,
    /// `measured / baseline`.
    pub baseline_ratio: Option<f64>,
    /// Against the budget.
    pub budget_verdict: BudgetVerdict,
    /// Against the baseline.
    pub baseline_verdict: BaselineVerdict,
    /// `metric_missing`, `metric_ambiguous` or `metric_unreadable`, when no number was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// The error's text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The entry's note.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Row {
    /// Whether `--enforce` fails on this row.
    #[must_use]
    pub fn fails(&self) -> bool {
        self.budget_verdict == BudgetVerdict::Over
            || self.budget_verdict == BudgetVerdict::NotMeasured
            || self.baseline_verdict == BaselineVerdict::Regressed
    }
}

/// What a command left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutcome {
    /// Combined stdout and stderr.
    pub output: String,
    /// Exit code; `None` when it could not start or a signal ended it.
    pub code: Option<i32>,
}

impl CommandOutcome {
    fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A command that did not exit with 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailedCommand {
    /// The command.
    pub command: String,
    /// Its exit code.
    pub code: Option<i32>,
}

/// The full result of a run; `--json` prints it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// Version of this report ([`PERF_BUDGETS_FORMAT_VERSION`]).
    pub format_version: u32,
    /// The host the times belong to.
    pub host: String,
    /// One per selected entry.
    pub rows: Vec<Row>,
    /// Commands that exited non-zero.
    pub failed_commands: Vec<FailedCommand>,
    /// No row fails and no command failed; what `--enforce` turns into the exit code.
    pub pass: bool,
}

/// The comparison settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tolerances {
    /// Allowed ratio for a time.
    pub time: f64,
    /// Allowed ratio for a count.
    pub count: f64,
}

/// Builds the report from the commands' outcomes. Pure, so it is unit-tested.
#[must_use]
pub fn evaluate(
    host: &str,
    entries: &[&Entry],
    outcomes: &BTreeMap<String, CommandOutcome>,
    baseline_of: &dyn Fn(&Entry) -> Option<f64>,
    tolerances: Tolerances,
) -> Report {
    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| {
            let outcome = outcomes.get(&entry.command);
            let read = match outcome {
                Some(o) => read_metric(entry, &o.output),
                None => Err(MetricError {
                    code: "metric_missing",
                    text: "the command did not run".into(),
                }),
            };
            let (measured, error_code, error) = match read {
                Ok(value) => (Some(value), None, None),
                Err(err) => (None, Some(err.code.to_string()), Some(err.text)),
            };
            let limit = entry.budget.limit();
            let base = baseline_of(entry);
            let count = entry.stat.is_count();
            let tolerance = if count { tolerances.count } else { tolerances.time };
            Row {
                name: entry.name.clone(),
                spec: entry.spec.clone(),
                package: entry.package.clone(),
                stat: entry.stat.label().to_string(),
                kind: if count { "count" } else { "time" }.to_string(),
                budget: limit,
                measured,
                margin_percent: match (measured, limit) {
                    (Some(m), Some(l)) if l > 0.0 => Some((l - m) / l * 100.0),
                    _ => None,
                },
                baseline: base,
                baseline_ratio: match (measured, base) {
                    (Some(m), Some(b)) if b > 0.0 => Some(m / b),
                    _ => None,
                },
                budget_verdict: budget_verdict(measured, limit),
                baseline_verdict: baseline_verdict(measured, base, tolerance, count),
                error_code,
                error,
                note: entry.note.clone(),
            }
        })
        .collect();
    let failed_commands: Vec<FailedCommand> = outcomes
        .iter()
        .filter(|(_, outcome)| !outcome.success())
        .map(|(command, outcome)| FailedCommand {
            command: command.clone(),
            code: outcome.code,
        })
        .collect();
    let pass = failed_commands.is_empty() && !rows.iter().any(Row::fails);
    Report {
        format_version: PERF_BUDGETS_FORMAT_VERSION,
        host: host.to_string(),
        rows,
        failed_commands,
        pass,
    }
}

// ---- printing ----------------------------------------------------------------------------------

/// A time or count for the table.
#[must_use]
pub fn format_value(value: Option<f64>, kind: &str) -> String {
    let Some(v) = value else {
        return "-".into();
    };
    if kind == "count" {
        return if v.fract() == 0.0 {
            format!("{v:.0}")
        } else {
            format!("{v:.2}")
        };
    }
    if v >= 1e9 {
        format!("{:.2} s", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.2} ms", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.2} us", v / 1e3)
    } else {
        format!("{v:.0} ns")
    }
}

fn verdict_text(row: &Row) -> String {
    let mut parts = Vec::new();
    match row.budget_verdict {
        BudgetVerdict::Within => parts.push("within"),
        BudgetVerdict::Over => parts.push("OVER"),
        BudgetVerdict::Unbudgeted => parts.push("no budget"),
        BudgetVerdict::NotMeasured => parts.push("NOT MEASURED"),
    }
    match row.baseline_verdict {
        BaselineVerdict::Regressed => parts.push("REGRESSED"),
        BaselineVerdict::Improved => parts.push("improved (baseline is stale)"),
        BaselineVerdict::NoBaseline => parts.push("no baseline"),
        BaselineVerdict::Ok | BaselineVerdict::NotMeasured => {}
    }
    parts.join(", ")
}

/// The report as a markdown table, so a run pastes into `docs/perf-log.md` unchanged.
#[must_use]
pub fn format_table(report: &Report) -> String {
    let mut out = String::from(
        "| Measurement | Crate | Spec | Stat | Budget | Measured | Margin | Baseline | Ratio | Verdict |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for row in &report.rows {
        let margin = row
            .margin_percent
            .map_or_else(|| "-".to_string(), |m| format!("{m:+.0} %"));
        let ratio = row
            .baseline_ratio
            .map_or_else(|| "-".to_string(), |r| format!("{r:.2}x"));
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            row.name,
            row.package,
            row.spec,
            row.stat,
            format_value(row.budget, &row.kind),
            format_value(row.measured, &row.kind),
            margin,
            format_value(row.baseline, &row.kind),
            ratio,
            verdict_text(row),
        ));
    }
    out
}

// ---- running -----------------------------------------------------------------------------------

/// Arguments of `cargo xtask perf-budgets`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Only entries whose name, spec or crate contains this text.
    #[arg(long)]
    pub filter: Option<String>,
    /// Exit non-zero when an entry is over budget, regressed or not measured, or a command failed.
    #[arg(long)]
    pub enforce: bool,
    /// Baseline file: times, or counts with `--counts-only` (default
    /// `docs/perf/budget-baseline-<host>.json` and `docs/perf/budget-counts.json`).
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Also write this run's numbers into the baselines (the report still compares with the old ones).
    #[arg(long)]
    pub update_baseline: bool,
    /// Only the allocation counts: the same on every machine, so CI can gate on them.
    #[arg(long)]
    pub counts_only: bool,
    /// Print the report as JSON instead of the table.
    #[arg(long)]
    pub json: bool,
    /// Print the selected entries without running anything.
    #[arg(long)]
    pub list: bool,
    /// A time regressed when it is more than this many times its baseline.
    #[arg(long, default_value_t = 1.5)]
    pub tolerance: f64,
    /// A count regressed when it is more than this many times its baseline (1 means any growth).
    #[arg(long, default_value_t = 1.0)]
    pub count_tolerance: f64,
    /// The host name of the baseline file (default: this machine's name).
    #[arg(long)]
    pub host: Option<String>,
    /// Registry file (default `docs/perf/budgets.ron`).
    #[arg(long)]
    pub registry: Option<PathBuf>,
    /// Directory of the logs and `report.json` (default `<target>/perf-budgets`).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// One log file name per command: a short hash, since a command is no file name.
fn log_name(command: &str) -> String {
    let hash = blake3::hash(command.as_bytes()).to_hex();
    let short: String = hash.chars().take(12).collect();
    format!("{short}.log")
}

/// The directory of the logs and `report.json`.
fn out_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), PathBuf::from)
        .join("perf-budgets")
}

/// Spawns one command with stdout and stderr in `log`, then reads the log back.
pub(super) fn run_cargo(root: &Path, command: &str, log: &Path) -> Result<CommandOutcome> {
    let mut args = command.split_whitespace();
    let program = args.next().context("empty command")?;
    let program = if program == "cargo" {
        std::env::var_os("CARGO").unwrap_or_else(|| program.into())
    } else {
        program.into()
    };
    let stdout = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let stderr = stdout.try_clone().context("cloning the log handle")?;
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .status();
    let code = match status {
        Ok(status) => status.code(),
        Err(err) => {
            eprintln!("perf-budgets: cannot start `{command}`: {err}");
            None
        }
    };
    // Lossy: test output may hold anything, and a metric line is ascii or µs.
    let bytes = std::fs::read(log).with_context(|| format!("reading {}", log.display()))?;
    Ok(CommandOutcome {
        output: String::from_utf8_lossy(&bytes).into_owned(),
        code,
    })
}

/// Runs the harness on real commands.
///
/// # Errors
///
/// An unusable registry or baseline, or a log or report that cannot be written.
pub fn run(args: &Args, root: &Path) -> Result<ExitCode> {
    run_with(args, root, &mut |command, log| run_cargo(root, command, log))
}

/// The commit, for the baseline's `recorded_on`.
fn head_commit(root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// [`run`] with the process spawn injected, so the whole flow is testable without cargo.
///
/// # Errors
///
/// See [`run`].
pub fn run_with(
    args: &Args,
    root: &Path,
    runner: &mut dyn FnMut(&str, &Path) -> Result<CommandOutcome>,
) -> Result<ExitCode> {
    if args.tolerance < 1.0 || args.count_tolerance < 1.0 {
        bail!("--tolerance and --count-tolerance are ratios of at least 1");
    }
    let registry_path = args.registry.clone().unwrap_or_else(|| root.join(REGISTRY_PATH));
    let text = std::fs::read_to_string(&registry_path)
        .with_context(|| format!("reading {}", registry_path.display()))?;
    let registry = parse_registry(&text)?;
    let entries: Vec<&Entry> = registry
        .entries
        .iter()
        .filter(|e| selected(e, args.filter.as_deref(), args.counts_only))
        .collect();
    if entries.is_empty() {
        // A typo in a CI line must not read as a green run.
        bail!("no registry entry is selected");
    }
    if args.list {
        for entry in &entries {
            println!("{}  [{}]  {}", entry.name, entry.spec, entry.command);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let host = args.host.as_deref().map_or_else(detect_host, sanitize_host);
    let (times_path, counts_path) = if args.counts_only {
        (
            root.join(host_baseline_path(&host)),
            args.baseline
                .clone()
                .unwrap_or_else(|| root.join(COUNTS_BASELINE_PATH)),
        )
    } else {
        (
            args.baseline
                .clone()
                .unwrap_or_else(|| root.join(host_baseline_path(&host))),
            root.join(COUNTS_BASELINE_PATH),
        )
    };
    let times = read_baseline(&times_path)?;
    let counts = read_baseline(&counts_path)?;

    let dir = args.out.clone().unwrap_or_else(|| out_dir(root));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut outcomes = BTreeMap::new();
    for command in unique_commands(&entries) {
        let log = dir.join(log_name(&command));
        eprintln!("perf-budgets: running `{command}` (log {})", log.display());
        let outcome = runner(&command, &log)?;
        if !outcome.success() {
            eprintln!("perf-budgets: `{command}` exited with {:?}", outcome.code);
        }
        outcomes.insert(command, outcome);
    }

    let baseline_of = |entry: &Entry| {
        let file = if entry.stat.is_count() { &counts } else { &times };
        file.entries.get(&entry.name).copied()
    };
    let report = evaluate(
        &host,
        &entries,
        &outcomes,
        &baseline_of,
        Tolerances {
            time: args.tolerance,
            count: args.count_tolerance,
        },
    );
    let json = serde_json::to_string_pretty(&report)?;
    write_atomic(&dir.join("report.json"), &format!("{json}\n"))?;
    if args.json {
        println!("{json}");
    } else {
        print!("{}", format_table(&report));
        for failed in &report.failed_commands {
            println!("command failed ({:?}): {}", failed.code, failed.command);
        }
        for row in report.rows.iter().filter(|r| r.error.is_some()) {
            println!(
                "{}: {} ({})",
                row.name,
                row.error.as_deref().unwrap_or_default(),
                row.error_code.as_deref().unwrap_or_default()
            );
        }
    }

    if args.update_baseline {
        // Outside a git checkout (an exported tree) the commit is empty: no trailing space then.
        let recorded = format!("{host} {}", head_commit(root)).trim_end().to_string();
        update_baseline(&times_path, &report, &registry, "time", &recorded)?;
        update_baseline(&counts_path, &report, &registry, "count", &recorded)?;
    }
    Ok(if args.enforce && !report.pass {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Merges one run into a baseline: rows of `kind` replace their numbers, numbers outside this run
/// stay (`--filter` must not lose them), and names no longer in the registry go. A file no row of
/// `kind` touches is left as it is.
fn update_baseline(
    path: &Path,
    report: &Report,
    registry: &Registry,
    kind: &str,
    recorded: &str,
) -> Result<()> {
    let measured: Vec<(&str, f64)> = report
        .rows
        .iter()
        .filter(|r| r.kind == kind)
        .filter_map(|r| r.measured.map(|m| (r.name.as_str(), m)))
        .collect();
    if measured.is_empty() {
        return Ok(());
    }
    let mut baseline = read_baseline(path)?;
    let known: BTreeSet<&str> = registry.entries.iter().map(|e| e.name.as_str()).collect();
    baseline.entries.retain(|name, _| known.contains(name.as_str()));
    for (name, value) in measured {
        baseline.entries.insert(name.to_string(), value);
    }
    baseline.recorded_on = recorded.to_string();
    write_baseline(path, &baseline)?;
    eprintln!("perf-budgets: baseline written to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A fresh directory per call (pid + counter), as `perf_smoke`'s tests.
    fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    const CMD: &str = "cargo test -p demo --release --test perf -- --ignored --nocapture";
    const COUNT_CMD: &str = "cargo test -p demo --release --test perf -- --ignored --nocapture perf_count";

    fn entry(name: &str, stat: Stat, metric: &str, budget: Budget) -> Entry {
        Entry {
            name: name.into(),
            spec: "spec 1 § 1".into(),
            package: "demo".into(),
            command: if stat.is_count() { COUNT_CMD } else { CMD }.into(),
            metric: metric.into(),
            stat,
            budget,
            note: String::new(),
        }
    }

    fn micro(name: &str, stat: Stat, budget: Budget) -> Entry {
        entry(name, stat, &format!("^perf-micro {name}: .*$"), budget)
    }

    fn count(name: &str, stat: Stat, budget: Budget) -> Entry {
        entry(name, stat, &format!("^perf-count {name}: .*$"), budget)
    }

    fn one_outcome(output: &str, code: i32) -> BTreeMap<String, CommandOutcome> {
        BTreeMap::from([(
            CMD.to_string(),
            CommandOutcome {
                output: output.into(),
                code: Some(code),
            },
        )])
    }

    const TOLERANCES: Tolerances = Tolerances {
        time: 1.5,
        count: 1.0,
    };

    // ---- registry ------------------------------------------------------------------------------

    const REGISTRY: &str = r#"(
        format_version: 1,
        entries: [
            (
                name: "bindings_200_unchanged",
                spec: "spec 81 § 9",
                package: "demo",
                command: "cargo bench -p demo --bench bindings_200 -- --noplot",
                metric: "^bindings_200_unchanged\\s+time:\\s+\\[.*\\]",
                stat: CriterionEstimate,
                budget: Ms(0.1),
            ),
            (
                name: "panel_frame",
                spec: "spec 67 § 9",
                package: "demo",
                command: "cargo test -p demo --release --test panel -- --ignored --nocapture",
                metric: "panel frame: (?P<value>[0-9.]+) (?P<unit>ms)",
                stat: Capture,
                budget: Unbudgeted,
                note: "desktop reference",
            ),
            (
                name: "load_allocs",
                spec: "ADR-2704",
                package: "demo",
                command: "cargo test -p demo --release --test panel -- --ignored --nocapture perf_count",
                metric: "^perf-count load: .*$",
                stat: AllocsPerOp,
                budget: Count(12.0),
            ),
        ],
    )"#;

    #[test]
    fn registry_parses_every_stat_and_budget_shape() {
        let registry = parse_registry(REGISTRY).expect("parses");
        assert_eq!(registry.format_version, 1);
        assert_eq!(registry.entries.len(), 3);
        assert_eq!(registry.entries[0].stat, Stat::CriterionEstimate);
        assert_eq!(registry.entries[0].budget, Budget::Ms(0.1));
        assert_eq!(registry.entries[1].budget, Budget::Unbudgeted);
        assert_eq!(registry.entries[1].note, "desktop reference");
        assert_eq!(registry.entries[2].budget, Budget::Count(12.0));
    }

    /// The frozen `perf-budgets` format's fuzz coverage (`regia_agent` `tests/fuzz_targets.rs`
    /// `COVERED_ELSEWHERE`): a mutated registry parses or is refused with a message, never panics.
    #[test]
    fn perf_budgets_ron_never_panics() {
        parse_registry(REGISTRY).expect("the test registry parses");
        regia_test_utils::fuzz::fuzz_text_loader(REGISTRY, 0xB0D6, 2000, parse_registry);
    }

    #[test]
    fn another_format_version_is_refused() {
        let text = REGISTRY.replace("format_version: 1", "format_version: 2");
        let err = format!("{:#}", parse_registry(&text).expect_err("refused"));
        assert!(err.contains("format_version 2"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let text = REGISTRY.replace("note: \"desktop reference\"", "notes: \"typo\"");
        assert!(parse_registry(&text).is_err());
    }

    #[test]
    fn budget_units_convert_to_one_scale() {
        assert_eq!(Budget::Ns(5.0).limit(), Some(5.0));
        assert_eq!(Budget::Us(5.0).limit(), Some(5e3));
        assert_eq!(Budget::Ms(5.0).limit(), Some(5e6));
        assert_eq!(Budget::S(5.0).limit(), Some(5e9));
        assert_eq!(Budget::Count(5.0).limit(), Some(5.0));
        assert_eq!(Budget::Unbudgeted.limit(), None);
    }

    /// One problem per kind of bad entry, each named.
    #[test]
    fn invalid_entries_are_each_reported() {
        let mut debug_run = micro("debug_run", Stat::P50, Budget::Ms(1.0));
        debug_run.command = "cargo run -p demo --example x".into();
        let mut any_binary = micro("any_binary", Stat::P50, Budget::Ms(1.0));
        any_binary.command = "cargo run --release -p demo --bin tool".into();
        let mut debug_build = micro("debug_build", Stat::P50, Budget::Ms(1.0));
        debug_build.command = "cargo test -p demo --test perf -- --release".into();
        let mut shell = micro("shell", Stat::P50, Budget::Ms(1.0));
        shell.command = "sh -c reboot -p demo".into();
        let mut package = micro("package", Stat::P50, Budget::Ms(1.0));
        package.command = "cargo bench -p other --bench x".into();
        let mut no_spec = micro("no_spec", Stat::P50, Budget::Ms(1.0));
        no_spec.spec = " ".into();
        let registry = Registry {
            format_version: 1,
            entries: vec![
                debug_run,
                any_binary,
                debug_build,
                shell,
                package,
                no_spec,
                micro("Bad Name", Stat::P50, Budget::Ms(1.0)),
                micro("twice", Stat::P50, Budget::Ms(1.0)),
                micro("twice", Stat::P50, Budget::Ms(1.0)),
                entry("regex", Stat::P50, "(", Budget::Ms(1.0)),
                entry("capture", Stat::Capture, "x (?P<value>1)", Budget::Ms(1.0)),
                micro("count_budget_on_time", Stat::P50, Budget::Count(1.0)),
                count("time_budget_on_count", Stat::AllocsPerOp, Budget::Ms(1.0)),
                micro("negative", Stat::P50, Budget::Ms(-1.0)),
            ],
        };
        let problems = registry.problems();
        for needle in [
            "`debug_run`: `cargo run` without `--release`",
            "`any_binary`: `cargo run` only runs an `--example`",
            "`debug_build`: `cargo test` without `--release`",
            "`shell`: a command starts with",
            "`package`: the command does not say `-p demo`",
            "`no_spec`: no spec reference",
            "`Bad Name`: a name is",
            "`twice`: appears twice",
            "`regex`: metric regex",
            "`capture`: stat Capture needs",
            "`count_budget_on_time`: budget",
            "`time_budget_on_count`: budget",
            "`negative`: a budget",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "{needle} not in {problems:#?}"
            );
        }
        assert_eq!(problems.len(), 13, "{problems:#?}");
    }

    #[test]
    fn the_checked_in_registry_is_valid_and_its_targets_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let text = std::fs::read_to_string(root.join(REGISTRY_PATH)).expect("registry");
        let registry = parse_registry(&text).expect("valid");
        assert!(!registry.entries.is_empty());
        for entry in &registry.entries {
            // A crate, or a sample (the tunnels showcase rows, ADR-8873).
            let dir = ["crates", "samples"]
                .iter()
                .map(|parent| root.join(parent).join(&entry.package))
                .find(|dir| dir.join("Cargo.toml").is_file());
            let Some(dir) = dir else {
                panic!("{}: no crate or sample `{}`", entry.name, entry.package);
            };
            let args: Vec<&str> = entry.command.split_whitespace().collect();
            for (flag, folder) in [
                ("--test", "tests"),
                ("--bench", "benches"),
                ("--example", "examples"),
            ] {
                if let Some(target) = args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)) {
                    assert!(
                        dir.join(folder).join(format!("{target}.rs")).is_file(),
                        "{}: no {folder}/{target}.rs",
                        entry.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_checked_in_baselines_name_registry_entries_of_the_right_kind() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let text = std::fs::read_to_string(root.join(REGISTRY_PATH)).expect("registry");
        let registry = parse_registry(&text).expect("valid");
        let of_kind = |count: bool| -> BTreeSet<&str> {
            registry
                .entries
                .iter()
                .filter(|e| e.stat.is_count() == count)
                .map(|e| e.name.as_str())
                .collect()
        };
        let counts = read_baseline(&root.join(COUNTS_BASELINE_PATH)).expect("counts baseline");
        for name in counts.entries.keys() {
            assert!(
                of_kind(true).contains(name.as_str()),
                "{name} is not a count entry"
            );
        }
        for file in std::fs::read_dir(root.join("docs/perf")).expect("docs/perf") {
            let path = file.expect("entry").path();
            let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if stem.starts_with("budget-baseline-") {
                let baseline = read_baseline(&path).expect("baseline");
                for name in baseline.entries.keys() {
                    assert!(
                        of_kind(false).contains(name.as_str()),
                        "{stem}: {name} is not a time entry"
                    );
                }
            }
        }
    }

    // ---- metric regex and statistics -----------------------------------------------------------

    #[test]
    fn durations_parse_in_debug_and_spaced_forms() {
        assert_eq!(parse_duration_ns("800ns"), Some(800.0));
        assert_eq!(parse_duration_ns("12.5µs"), Some(12_500.0));
        assert_eq!(parse_duration_ns("12.5us"), Some(12_500.0));
        assert_eq!(parse_duration_ns("3 ms"), Some(3e6));
        assert_eq!(parse_duration_ns("1.25s"), Some(1.25e9));
        assert_eq!(parse_duration_ns("7"), None);
        assert_eq!(parse_duration_ns("7 parsecs"), None);
        assert_eq!(parse_duration_ns("ms"), None);
    }

    const MICRO: &str = "running 2 tests\n\
        perf-micro crowd_step_50_agents: n=600 p50=12.5µs p99=1.25ms max=2s\n\
        perf-micro crowd_step_500_agents: n=600 p50=800ns p99=900ns max=1µs\n\
        test perf_micro_crowd_step ... ok\n";

    #[test]
    fn perf_micro_statistics_read_from_the_matched_line_only() {
        let read = |stat| read_metric(&micro("crowd_step_50_agents", stat, Budget::Unbudgeted), MICRO);
        assert_eq!(read(Stat::P50), Ok(12_500.0));
        assert_eq!(read(Stat::P99), Ok(1.25e6));
        assert_eq!(read(Stat::Max), Ok(2e9));
        let second = micro("crowd_step_500_agents", Stat::P50, Budget::Unbudgeted);
        assert_eq!(read_metric(&second, MICRO), Ok(800.0));
    }

    /// With `--test-threads=1` libtest prints `test name ... ` and the test's first line follows on
    /// the same line, so a registry regex must not anchor a `perf-micro` line at the line start.
    #[test]
    fn a_line_after_the_libtest_prefix_reads() {
        let output = "test perf_micro_a ... perf-micro a: n=4 p50=2ms p99=3ms max=4ms\nok\n";
        let e = entry("a", Stat::P99, "perf-micro a: .*$", Budget::Unbudgeted);
        assert_eq!(read_metric(&e, output), Ok(3e6));
    }

    #[test]
    fn a_header_and_a_row_can_give_the_unit_and_the_value() {
        let output =
            "profile   encode_us decode_us\nnarrow        1.0       2.0\nstandard    210.5      80.0\n";
        let e = entry(
            "encode",
            Stat::Capture,
            r"^profile\s+encode_(?P<unit>us)\s(?s:.*?)^standard\s+(?P<value>[0-9.]+)",
            Budget::Ms(0.25),
        );
        let got = read_metric(&e, output).expect("reads");
        assert!((got - 210_500.0).abs() < 1e-6, "{got}");
    }

    #[test]
    fn procgen_style_decimal_ms_read_too() {
        let line = "perf-micro compile_200_nodes: n=9 p50=1.70ms p99=1.90ms max=2.00ms\n";
        let e = micro("compile_200_nodes", Stat::P99, Budget::Unbudgeted);
        let got = read_metric(&e, line).expect("reads");
        assert!((got - 1.9e6).abs() < 1.0, "{got}");
    }

    #[test]
    fn a_key_only_matches_as_a_whole_word() {
        // `xp50=` is another key; the real one comes after it.
        let line = "perf-micro a: xp50=1ms p50=2ms\n";
        assert_eq!(
            read_metric(&micro("a", Stat::P50, Budget::Unbudgeted), line),
            Ok(2e6)
        );
    }

    #[test]
    fn criterion_estimate_is_the_middle_of_three_even_on_a_second_line() {
        let output = "Benchmarking caption/shaped_miss: Analyzing\n\
            caption/shaped_miss\n                        time:   [270.12 µs 279.10 µs 285.30 µs]\n\
            bindings_200_unchanged  time:   [12.300 ns 12.400 ns 12.500 ns]\n\
            \x20                       change: [-1.0% +0.2% +1.0%] (p = 0.50 > 0.05)\n";
        let long = entry(
            "caption_miss",
            Stat::CriterionEstimate,
            r"^caption/shaped_miss\s+time:\s+\[[^\]]*\]",
            Budget::Unbudgeted,
        );
        let got = read_metric(&long, output).expect("reads");
        assert!((got - 279_100.0).abs() < 1e-3, "{got}");
        let short = entry(
            "bindings",
            Stat::CriterionEstimate,
            r"^bindings_200_unchanged\s+time:\s+\[[^\]]*\]",
            Budget::Unbudgeted,
        );
        assert!((read_metric(&short, output).expect("reads") - 12.4).abs() < 1e-9);
    }

    #[test]
    fn capture_reads_named_groups_of_any_line() {
        let e = entry(
            "panel",
            Stat::Capture,
            r"procgen panel frame, 300 nodes: (?P<value>[0-9.]+) (?P<unit>ms)",
            Budget::Ms(2.0),
        );
        let line = "procgen panel frame, 300 nodes: 6.700 ms (budget 2 ms on the desktop reference)\n";
        let got = read_metric(&e, line).expect("reads");
        assert!((got - 6.7e6).abs() < 1.0, "{got}");
    }

    #[test]
    fn counts_divide_by_the_operations() {
        let line = "perf-count load: ops=4 allocs=100 bytes=4000\n";
        assert_eq!(
            read_metric(&count("load", Stat::AllocsPerOp, Budget::Unbudgeted), line),
            Ok(25.0)
        );
        assert_eq!(
            read_metric(&count("load", Stat::BytesPerOp, Budget::Unbudgeted), line),
            Ok(1000.0)
        );
        let none = "perf-count load: ops=0 allocs=1 bytes=1\n";
        let err =
            read_metric(&count("load", Stat::AllocsPerOp, Budget::Unbudgeted), none).expect_err("no ops");
        assert_eq!(err.code, "metric_unreadable");
    }

    #[test]
    fn a_missing_a_doubled_and_an_unreadable_metric_are_typed_errors() {
        let e = micro("a", Stat::P50, Budget::Unbudgeted);
        assert_eq!(
            read_metric(&e, "nothing\n").expect_err("missing").code,
            "metric_missing"
        );
        let twice = "perf-micro a: p50=1ms\nperf-micro a: p50=2ms\n";
        assert_eq!(
            read_metric(&e, twice).expect_err("twice").code,
            "metric_ambiguous"
        );
        let bad = "perf-micro a: p50=soon\n";
        assert_eq!(read_metric(&e, bad).expect_err("bad").code, "metric_unreadable");
        let absent = "perf-micro a: n=4\n";
        assert_eq!(
            read_metric(&e, absent).expect_err("absent").code,
            "metric_unreadable"
        );
    }

    // ---- comparison ----------------------------------------------------------------------------

    #[test]
    fn budget_comparison_is_inclusive() {
        assert_eq!(budget_verdict(Some(10.0), Some(10.0)), BudgetVerdict::Within);
        assert_eq!(budget_verdict(Some(10.5), Some(10.0)), BudgetVerdict::Over);
        assert_eq!(budget_verdict(Some(10.5), None), BudgetVerdict::Unbudgeted);
        assert_eq!(budget_verdict(None, Some(10.0)), BudgetVerdict::NotMeasured);
    }

    #[test]
    fn baseline_comparison_has_a_band_around_the_tolerance() {
        let v = |m, b| baseline_verdict(Some(m), b, 1.5, false);
        assert_eq!(v(150.0, Some(100.0)), BaselineVerdict::Ok);
        assert_eq!(v(151.0, Some(100.0)), BaselineVerdict::Regressed);
        assert_eq!(v(67.0, Some(100.0)), BaselineVerdict::Ok);
        assert_eq!(v(66.0, Some(100.0)), BaselineVerdict::Improved);
        assert_eq!(v(5.0, None), BaselineVerdict::NoBaseline);
        assert_eq!(v(5.0, Some(0.0)), BaselineVerdict::NoBaseline);
        assert_eq!(
            baseline_verdict(None, Some(1.0), 1.5, false),
            BaselineVerdict::NotMeasured
        );
    }

    #[test]
    fn counts_at_tolerance_one_catch_any_growth_and_a_zero_baseline_is_a_real_number() {
        let v = |m, b| baseline_verdict(Some(m), Some(b), 1.0, true);
        assert_eq!(v(100.0, 100.0), BaselineVerdict::Ok);
        assert_eq!(v(101.0, 100.0), BaselineVerdict::Regressed);
        assert_eq!(v(99.0, 100.0), BaselineVerdict::Improved);
        assert_eq!(v(0.0, 0.0), BaselineVerdict::Ok);
        assert_eq!(v(1.0, 0.0), BaselineVerdict::Regressed);
    }

    #[test]
    fn evaluate_computes_margin_and_ratio_and_passes_when_all_is_within() {
        let e = micro("a", Stat::P50, Budget::Ms(2.0));
        let report = evaluate(
            "h",
            &[&e],
            &one_outcome("perf-micro a: p50=1ms\n", 0),
            &|_| Some(800_000.0),
            TOLERANCES,
        );
        let row = &report.rows[0];
        assert_eq!(row.measured, Some(1e6));
        assert_eq!(row.margin_percent, Some(50.0));
        assert_eq!(row.baseline_ratio, Some(1.25));
        assert_eq!(row.budget_verdict, BudgetVerdict::Within);
        assert_eq!(row.baseline_verdict, BaselineVerdict::Ok);
        assert!(report.pass);
    }

    #[test]
    fn an_over_budget_a_regression_a_lost_metric_or_a_failed_command_each_fail_the_report() {
        let e = micro("a", Stat::P50, Budget::Ms(1.0));
        let line = "perf-micro a: p50=2ms\n";
        let over = evaluate("h", &[&e], &one_outcome(line, 0), &|_| None, TOLERANCES);
        assert_eq!(over.rows[0].margin_percent, Some(-100.0));
        assert!(!over.pass);

        let free = micro("a", Stat::P50, Budget::Unbudgeted);
        let regressed = evaluate("h", &[&free], &one_outcome(line, 0), &|_| Some(1e6), TOLERANCES);
        assert_eq!(regressed.rows[0].baseline_verdict, BaselineVerdict::Regressed);
        assert!(!regressed.pass);

        let lost = evaluate(
            "h",
            &[&free],
            &one_outcome("no metric\n", 0),
            &|_| None,
            TOLERANCES,
        );
        assert_eq!(lost.rows[0].budget_verdict, BudgetVerdict::NotMeasured);
        assert_eq!(lost.rows[0].error_code.as_deref(), Some("metric_missing"));
        assert!(!lost.pass);

        // The number is kept, and the command's failure still fails the run.
        let failed = evaluate("h", &[&free], &one_outcome(line, 101), &|_| None, TOLERANCES);
        assert_eq!(failed.rows[0].measured, Some(2e6));
        assert_eq!(failed.failed_commands.len(), 1);
        assert!(!failed.pass);

        // No budget and no baseline: nothing to fail on.
        let quiet = evaluate("h", &[&free], &one_outcome(line, 0), &|_| None, TOLERANCES);
        assert!(quiet.pass);
    }

    #[test]
    fn the_table_shows_budget_measured_and_margin() {
        let e = micro("a", Stat::P50, Budget::Ms(1.0));
        let report = evaluate(
            "h",
            &[&e],
            &one_outcome("perf-micro a: p50=2ms\n", 0),
            &|_| None,
            TOLERANCES,
        );
        let table = format_table(&report);
        assert!(table.starts_with("| Measurement |"), "{table}");
        assert!(table.contains("| 1.00 ms | 2.00 ms | -100 % |"), "{table}");
        assert!(table.contains("OVER, no baseline"), "{table}");
    }

    #[test]
    fn values_format_in_a_readable_unit() {
        assert_eq!(format_value(None, "time"), "-");
        assert_eq!(format_value(Some(12.0), "time"), "12 ns");
        assert_eq!(format_value(Some(12_500.0), "time"), "12.50 us");
        assert_eq!(format_value(Some(1.5e6), "time"), "1.50 ms");
        assert_eq!(format_value(Some(2.5e9), "time"), "2.50 s");
        assert_eq!(format_value(Some(12.0), "count"), "12");
        assert_eq!(format_value(Some(12.25), "count"), "12.25");
    }

    #[test]
    fn host_names_are_safe_in_a_file_name() {
        assert_eq!(sanitize_host("buildhost\n"), "buildhost");
        assert_eq!(sanitize_host("workstation"), "workstation");
        assert_eq!(sanitize_host("../../etc"), "etc");
        assert_eq!(sanitize_host("  "), "unknown");
        assert_eq!(
            host_baseline_path("buildhost"),
            "docs/perf/budget-baseline-buildhost.json"
        );
    }

    // ---- the whole flow, with the spawn stubbed --------------------------------------------------

    fn args() -> Args {
        Args {
            filter: None,
            enforce: false,
            baseline: None,
            update_baseline: false,
            counts_only: false,
            json: false,
            list: false,
            tolerance: 1.5,
            count_tolerance: 1.0,
            host: Some("testhost".into()),
            registry: None,
            out: None,
        }
    }

    /// A root holding a registry of one time and one count entry over two commands.
    fn root_with_registry(tag: &str, time_budget: Budget) -> PathBuf {
        let root = temp_dir(tag);
        let registry = Registry {
            format_version: 1,
            entries: vec![
                micro("load", Stat::P50, time_budget),
                count("load_allocs", Stat::AllocsPerOp, Budget::Unbudgeted),
            ],
        };
        let text = ron::ser::to_string_pretty(&registry, regia_core::ron_pretty()).expect("ron");
        std::fs::create_dir_all(root.join("docs/perf")).expect("dir");
        std::fs::write(root.join(REGISTRY_PATH), text).expect("registry");
        root
    }

    /// Stands in for cargo; remembers the commands it was given.
    fn stub<'a>(
        ran: &'a mut Vec<String>,
        load_ms: u32,
        allocs: u32,
    ) -> impl FnMut(&str, &Path) -> Result<CommandOutcome> + 'a {
        move |command, _log| {
            ran.push(command.to_string());
            Ok(CommandOutcome {
                output: format!(
                    "perf-micro load: n=9 p50={load_ms}ms p99={load_ms}ms max={load_ms}ms\n\
                     perf-count load_allocs: ops=2 allocs={allocs} bytes=64\n"
                ),
                code: Some(0),
            })
        }
    }

    fn run_stubbed(args: &Args, root: &Path, load_ms: u32, allocs: u32) -> (ExitCode, Vec<String>) {
        let mut ran = Vec::new();
        let args = Args {
            out: Some(root.join("out")),
            ..args.clone()
        };
        let code = run_with(&args, root, &mut stub(&mut ran, load_ms, allocs)).expect("runs");
        (code, ran)
    }

    #[test]
    fn enforce_alone_turns_a_miss_into_a_failing_exit_code() {
        let root = root_with_registry("perf_budgets_enforce", Budget::Ms(1.0));
        let (code, _) = run_stubbed(&args(), &root, 5, 10);
        assert_eq!(
            code,
            ExitCode::SUCCESS,
            "a miss without --enforce is only reported"
        );
        let enforce = Args {
            enforce: true,
            ..args()
        };
        let (code, _) = run_stubbed(&enforce, &root, 5, 10);
        assert_eq!(code, ExitCode::FAILURE, "--enforce fails on a miss");
        let (code, _) = run_stubbed(&enforce, &root, 1, 10);
        assert_eq!(code, ExitCode::SUCCESS, "--enforce passes when within budget");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn enforce_fails_when_a_command_exits_non_zero() {
        let root = root_with_registry("perf_budgets_cmd_fail", Budget::Unbudgeted);
        let plain = Args {
            out: Some(root.join("out")),
            ..args()
        };
        let enforce = Args {
            enforce: true,
            ..plain.clone()
        };
        let mut runner = |_: &str, _: &Path| {
            Ok(CommandOutcome {
                output: "error: could not compile\n".into(),
                code: Some(101),
            })
        };
        assert_eq!(
            run_with(&enforce, &root, &mut runner).expect("runs"),
            ExitCode::FAILURE
        );
        assert_eq!(
            run_with(&args(), &root, &mut runner).expect("runs"),
            ExitCode::SUCCESS
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn counts_mode_runs_only_count_commands_and_gates_on_the_shared_baseline() {
        let root = root_with_registry("perf_budgets_counts", Budget::Ms(1.0));
        let counts_only = Args {
            counts_only: true,
            enforce: true,
            ..args()
        };
        // First run: no baseline, nothing to compare, and the time budget is not in this mode.
        let (code, ran) = run_stubbed(&counts_only, &root, 500, 10);
        assert_eq!(ran, vec![COUNT_CMD.to_string()]);
        assert_eq!(code, ExitCode::SUCCESS);

        let record = Args {
            update_baseline: true,
            ..counts_only.clone()
        };
        // The exit code is not what this test checks; the written baseline below is.
        let _ = run_stubbed(&record, &root, 500, 10);
        let base = read_baseline(&root.join(COUNTS_BASELINE_PATH)).expect("counts baseline");
        assert_eq!(base.entries.get("load_allocs"), Some(&5.0));
        assert!(
            !root.join(host_baseline_path("testhost")).exists(),
            "counts mode writes no per-host time file"
        );

        // A different host compares with the same file: counts do not depend on the machine.
        let other_host = Args {
            host: Some("otherhost".into()),
            ..counts_only.clone()
        };
        assert_eq!(run_stubbed(&other_host, &root, 500, 10).0, ExitCode::SUCCESS);
        assert_eq!(run_stubbed(&other_host, &root, 500, 12).0, ExitCode::FAILURE);
        assert_eq!(
            run_stubbed(&other_host, &root, 500, 8).0,
            ExitCode::SUCCESS,
            "fewer is not a failure"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn update_baseline_writes_times_per_host_then_a_regression_is_caught() {
        let root = root_with_registry("perf_budgets_update", Budget::Unbudgeted);
        let record = Args {
            update_baseline: true,
            ..args()
        };
        let (_, ran) = run_stubbed(&record, &root, 10, 10);
        assert_eq!(ran.len(), 2, "two commands, each once: {ran:?}");
        let times = read_baseline(&root.join(host_baseline_path("testhost"))).expect("times");
        assert_eq!(times.entries.get("load"), Some(&1e7));
        assert!(times.recorded_on.starts_with("testhost"), "{}", times.recorded_on);
        assert_eq!(
            times.recorded_on,
            times.recorded_on.trim_end(),
            "no trailing space without a commit"
        );

        let enforce = Args {
            enforce: true,
            ..args()
        };
        assert_eq!(
            run_stubbed(&enforce, &root, 15, 10).0,
            ExitCode::SUCCESS,
            "inside 1.5x"
        );
        assert_eq!(
            run_stubbed(&enforce, &root, 16, 10).0,
            ExitCode::FAILURE,
            "past 1.5x"
        );
        let wider = Args {
            tolerance: 2.0,
            ..enforce.clone()
        };
        assert_eq!(run_stubbed(&wider, &root, 16, 10).0, ExitCode::SUCCESS);

        // Another host has its own file, so this machine's numbers do not apply there.
        let other = Args {
            host: Some("otherhost".into()),
            ..enforce
        };
        assert_eq!(
            run_stubbed(&other, &root, 16, 10).0,
            ExitCode::SUCCESS,
            "no baseline there"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_baseline_file_argument_replaces_the_host_file() {
        let root = root_with_registry("perf_budgets_file", Budget::Unbudgeted);
        let path = root.join("my-baseline.json");
        let file = Baseline {
            entries: BTreeMap::from([("load".to_string(), 1e6)]),
            ..Baseline::default()
        };
        write_baseline(&path, &file).expect("write");
        let enforce = Args {
            enforce: true,
            baseline: Some(path),
            ..args()
        };
        assert_eq!(run_stubbed(&enforce, &root, 5, 10).0, ExitCode::FAILURE);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_filtered_update_keeps_other_numbers_and_drops_names_the_registry_lost() {
        let root = root_with_registry("perf_budgets_merge", Budget::Unbudgeted);
        let path = root.join(host_baseline_path("testhost"));
        let old = Baseline {
            entries: BTreeMap::from([("load".to_string(), 1.0), ("removed".to_string(), 2.0)]),
            ..Baseline::default()
        };
        write_baseline(&path, &old).expect("write");
        let mut registry: Registry =
            parse_registry(&std::fs::read_to_string(root.join(REGISTRY_PATH)).expect("read")).expect("parse");
        registry
            .entries
            .push(micro("other", Stat::P50, Budget::Unbudgeted));
        let text = ron::ser::to_string_pretty(&registry, regia_core::ron_pretty()).expect("ron");
        std::fs::write(root.join(REGISTRY_PATH), text).expect("registry");

        let record = Args {
            update_baseline: true,
            filter: Some("load".into()),
            ..args()
        };
        // The exit code is not what this test checks; the written baseline below is.
        let _ = run_stubbed(&record, &root, 3, 10);
        let after = read_baseline(&path).expect("read");
        assert_eq!(after.entries.get("load"), Some(&3e6));
        assert!(!after.entries.contains_key("removed"), "{after:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_filter_selects_by_name_spec_or_crate_and_an_empty_selection_is_an_error() {
        let e = micro("load", Stat::P50, Budget::Unbudgeted);
        assert!(selected(&e, Some("loa"), false));
        assert!(selected(&e, Some("§ 1"), false));
        assert!(selected(&e, Some("demo"), false));
        assert!(!selected(&e, Some("zzz"), false));
        assert!(!selected(&e, None, true), "a time entry is not in counts mode");

        let root = root_with_registry("perf_budgets_empty", Budget::Unbudgeted);
        let typo = Args {
            filter: Some("no such entry".into()),
            ..args()
        };
        let mut ran = Vec::new();
        let err = run_with(&typo, &root, &mut stub(&mut ran, 1, 1)).expect_err("nothing selected");
        assert!(err.to_string().contains("no registry entry"), "{err}");
        assert!(ran.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_runs_nothing_and_shared_commands_run_once() {
        let root = root_with_registry("perf_budgets_list", Budget::Unbudgeted);
        let list = Args { list: true, ..args() };
        let (code, ran) = run_stubbed(&list, &root, 1, 1);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(ran.is_empty());
        std::fs::remove_dir_all(&root).ok();

        let a = micro("a", Stat::P50, Budget::Unbudgeted);
        let b = micro("b", Stat::P99, Budget::Unbudgeted);
        assert_eq!(unique_commands(&[&a, &b]), vec![CMD.to_string()]);
    }

    #[test]
    fn a_malformed_baseline_is_an_error_and_a_missing_one_is_empty() {
        let root = temp_dir("perf_budgets_bad_base");
        let path = root.join("base.json");
        assert_eq!(
            read_baseline(&path).expect("missing is empty"),
            Baseline::default()
        );
        std::fs::write(&path, "{ not json").expect("write");
        assert!(read_baseline(&path).is_err());
        std::fs::write(&path, r#"{"version":9,"entries":{}}"#).expect("write");
        assert!(read_baseline(&path).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_report_serializes_with_stable_fields() {
        let e = micro("a", Stat::P50, Budget::Ms(1.0));
        let report = evaluate("h", &[&e], &one_outcome("nothing\n", 0), &|_| None, TOLERANCES);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).expect("json")).expect("parse");
        assert_eq!(json["format_version"], 1);
        assert_eq!(json["host"], "h");
        assert_eq!(json["pass"], false);
        let row = &json["rows"][0];
        assert_eq!(row["name"], "a");
        assert_eq!(row["kind"], "time");
        assert_eq!(row["budget_verdict"], "not_measured");
        assert_eq!(row["baseline_verdict"], "not_measured");
        assert_eq!(row["error_code"], "metric_missing");
    }

    #[test]
    fn the_schema_fingerprint_names_the_fields_and_variants() {
        let print = perf_budgets_schema_fingerprint();
        for needle in [
            "format_version",
            "metric",
            "budget",
            "CriterionEstimate",
            "Unbudgeted",
        ] {
            assert!(print.contains(needle), "{needle} not in {print}");
        }
    }

    /// Plan § 4.7 (row W-31, ADR-8662): the mixed sim-query tick has a recorded budget per device
    /// (the build server 0.25 ms, the Deck 0.5 ms, the Android device 0.8 ms; the N150 a baseline only),
    /// zero allocations, the index build its 50 ms and a D1 crater its 0.2 ms, all read from the
    /// one bench, with spec 121 § 11 as their `spec`.
    #[test]
    fn query_bench_1000_mixed_calls_has_a_budget_per_device() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let text = std::fs::read_to_string(root.join(REGISTRY_PATH)).expect("registry");
        let registry = parse_registry(&text).expect("valid");
        let find = |name: &str| {
            registry
                .entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("no budget entry {name}"))
        };
        let command = "cargo bench -p regia_world_geom --features stage_b --bench mixed_calls";
        for (device, budget) in [
            ("buildhost", Budget::Ms(0.25)),
            ("deck", Budget::Ms(0.5)),
            ("android", Budget::Ms(0.8)),
            ("n150", Budget::Unbudgeted),
        ] {
            let e = find(&format!("query_bench_1000_mixed_calls.{device}"));
            assert_eq!(e.package, "regia_world_geom");
            assert_eq!(e.command, command, "{}", e.name);
            assert_eq!(e.stat, Stat::P50, "{}", e.name);
            assert_eq!(e.budget, budget, "{}", e.name);
            assert_eq!(e.spec, "spec 121 § 11", "{}", e.name);
        }
        let allocs = find("query_bench_1000_mixed_calls_allocs");
        assert_eq!(
            (allocs.stat, allocs.budget),
            (Stat::AllocsPerOp, Budget::Count(0.0))
        );
        assert_eq!(allocs.command, command);
        let build = find("query_index_build_switchbacks");
        assert_eq!((build.stat, build.budget), (Stat::P50, Budget::Ms(50.0)));
        assert_eq!(build.command, command);
        let crater = find("ground_edit_crater_6m");
        assert_eq!((crater.stat, crater.budget), (Stat::P50, Budget::Ms(0.2)));
        assert_eq!(crater.command, command);
        for e in [allocs, build, crater] {
            assert_eq!(e.spec, "spec 121 § 11", "{}", e.name);
        }
    }
}
