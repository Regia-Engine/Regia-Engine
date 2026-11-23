// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask agent-bench`: the agent benchmark of `docs/specs/23-agent-native.md` § 8.
//!
//! The set is `bench/agent_tasks/<id>.toml` (format in `docs/formats/agent-task.md`): a prompt for a
//! model, the headless target to start, the setup calls that build the starting world, and
//! machine-checkable `[[check]]` rows. Next to each task sits `<id>.oracle.jsonl`, the scripted
//! reference calls that solve it.
//!
//! Three ways to run it:
//!
//! - `--oracle`: for every task, start its target, apply the setup, play the oracle, evaluate every
//!   check. `--empty` plays no oracle instead: every task must then FAIL, which proves the checks can
//!   fail.
//! - `--serve <task>`: start the target, apply the setup, record the baselines and wait. An outside
//!   MCP client (the owner's model run, `regia mcp --project <dir> --attach`) drives it.
//! - `--check-only --attach <project dir | ws url>`: evaluate the checks against the engine the
//!   client has just driven.
//!
//! This file holds the format, the predicates, the oracle player and the report. The headless
//! targets are in [`targets`].

pub mod targets;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The task format version (`docs/formats/agent-task.md`). A shape change needs a bump and a migrator.
pub const TASK_FORMAT_VERSION: u32 = 1;

/// The version of the `--json` report shape.
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// The number of tasks the set holds (spec 23 § 8).
pub const TASK_COUNT: usize = 25;

/// A protocol call: the unit of setup, oracle and checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The protocol method, e.g. `scene.spawn`.
    pub method: String,
    /// The parameters (`null` when the method takes none).
    #[serde(default)]
    pub params: Value,
    /// Names bound to values of the result (`name = "/json/pointer"`), usable as `"$name"` in later calls.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub save: BTreeMap<String, String>,
}

/// What the task starts from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// The headless target (see [`targets::TARGETS`]): which plugins the app has.
    pub target: String,
    /// Calls that build the starting world, before the model (or the oracle) starts.
    #[serde(default, rename = "call")]
    pub calls: Vec<Call>,
}

/// One success criterion: a protocol query and a predicate on a part of its result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// A short name for the report.
    pub name: String,
    /// The query method; read-only.
    pub method: String,
    /// The query parameters. A string that is exactly `$name` is replaced by the value the setup saved
    /// under that name (an entity id, say).
    #[serde(default)]
    pub params: Value,
    /// A JSON pointer into the result (`""` is the whole result).
    #[serde(default)]
    pub pointer: String,
    /// What the value at the pointer has to satisfy.
    pub expect: Predicate,
}

/// What a check requires of the value at its pointer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    /// The value equals `value` (numbers compare as numbers).
    Equals {
        /// The expected value.
        value: Value,
    },
    /// The value differs from `value`.
    NotEquals {
        /// The value it must not be.
        value: Value,
    },
    /// The array (or object, or string) has between `min` and `max` items.
    CountRange {
        /// Smallest count.
        min: u64,
        /// Largest count.
        max: u64,
    },
    /// The number is within `[min, max]`.
    NumberRange {
        /// Smallest value.
        min: f64,
        /// Largest value.
        max: f64,
    },
    /// The `[x, y, z]` position is within `max` meters of `point`.
    WithinDistance {
        /// The reference position.
        point: [f64; 3],
        /// The largest distance in meters.
        max: f64,
    },
    /// The pointer resolves to something (not `null`).
    Exists,
    /// The pointer resolves to nothing (or `null`).
    Absent,
    /// The string contains `text`, or the array has an item equal to the string `text`.
    Contains {
        /// The text to find.
        text: String,
    },
    /// The value (a screenshot's `png_base64`, or anything else) differs from what it was when the
    /// setup finished.
    ImageDiffers,
}

/// A benchmark task (`bench/agent_tasks/<id>.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Must be [`TASK_FORMAT_VERSION`].
    pub format_version: u32,
    /// Lowercase ascii, digits and `-`; the file stem.
    pub id: String,
    /// One line for the report.
    pub title: String,
    /// The natural-language request given to the model.
    pub prompt: String,
    /// The areas it exercises (scene, terrain, materials, lighting, ui, audio, nav, physics,
    /// sequencer, scripting).
    pub features: Vec<String>,
    /// The engine crates whose features it needs.
    pub crates: Vec<String>,
    /// The most protocol calls a model may use; the oracle fits it.
    pub call_budget: u32,
    /// The starting world.
    pub setup: Setup,
    /// The success criteria; all must hold.
    #[serde(rename = "check")]
    pub checks: Vec<Check>,
}

/// One line of an oracle file.
pub type OracleStep = Call;

/// The areas a task may name in `features` (spec 23 § 8 asks for this spread).
pub const FEATURE_AREAS: &[&str] = &[
    "scene",
    "terrain",
    "materials",
    "lighting",
    "ui",
    "audio",
    "nav",
    "physics",
    "sequencer",
    "scripting",
];

/// The fingerprint `check-formats` hashes: the field lists of every struct of the task file, so a
/// shape change fails the check until the version is bumped (`docs/specs/12-assets.md` § 4a).
#[must_use]
pub fn task_schema_fingerprint() -> String {
    [
        "Task{format_version,id,title,prompt,features,crates,call_budget,setup,check}",
        "Setup{target,call}",
        "Call{method,params,save}",
        "Check{name,method,params,pointer,expect}",
        "Predicate{equals(value),not_equals(value),count_range(min,max),number_range(min,max),\
         within_distance(point,max),exists,absent,contains(text),image_differs}",
    ]
    .join("|")
}

/// The `from` versions of the registered task migrators (none at v1).
#[must_use]
pub fn task_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Parses a task file. Fails on a wrong `format_version`, an unknown field, an id that is not the
/// expected one, an empty check list or a budget of zero.
///
/// # Errors
/// A message naming the problem.
pub fn parse_task(text: &str, expected_id: Option<&str>) -> Result<Task> {
    let task: Task = toml::from_str(text).context("parsing the task")?;
    if task.format_version != TASK_FORMAT_VERSION {
        bail!(
            "format_version {} is not supported (this tool reads {TASK_FORMAT_VERSION})",
            task.format_version
        );
    }
    if task.id.is_empty()
        || !task
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("id {:?} must be lowercase ascii, digits and '-'", task.id);
    }
    if let Some(expected) = expected_id
        && task.id != expected
    {
        bail!("id {:?} does not match the file name {expected:?}", task.id);
    }
    if task.prompt.trim().is_empty() || task.title.trim().is_empty() {
        bail!("title and prompt must not be empty");
    }
    if task.checks.is_empty() {
        bail!("a task needs at least one [[check]]");
    }
    if task.call_budget == 0 {
        bail!("call_budget must be at least 1");
    }
    if task.features.is_empty() {
        bail!("features must name at least one area");
    }
    for feature in &task.features {
        if !FEATURE_AREAS.contains(&feature.as_str()) {
            bail!(
                "unknown feature area {feature:?} (known: {})",
                FEATURE_AREAS.join(", ")
            );
        }
    }
    if !targets::TARGETS.iter().any(|t| t.name == task.setup.target) {
        bail!("unknown target {:?}", task.setup.target);
    }
    for check in &task.checks {
        if check.name.trim().is_empty() {
            bail!("a check has no name");
        }
        if !check.pointer.is_empty() && !check.pointer.starts_with('/') {
            bail!(
                "check {:?}: pointer {:?} must be empty or start with '/'",
                check.name,
                check.pointer
            );
        }
    }
    Ok(task)
}

/// Parses an oracle file: one JSON call per non-empty line; `#` lines are comments.
///
/// # Errors
/// The line number and the parse error.
pub fn parse_oracle(text: &str) -> Result<Vec<OracleStep>> {
    let mut steps = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let step: OracleStep =
            serde_json::from_str(line).with_context(|| format!("oracle line {}", index + 1))?;
        steps.push(step);
    }
    Ok(steps)
}

/// One loaded task with its oracle.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The task.
    pub task: Task,
    /// Its reference calls.
    pub oracle: Vec<OracleStep>,
}

/// Loads every task of `dir`, sorted by id, with its oracle.
///
/// # Errors
/// A problem with any file, naming it.
pub fn load_dir(dir: &Path) -> Result<Vec<Loaded>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(id) = name.strip_suffix(".toml") {
            ids.push(id.to_string());
        }
    }
    ids.sort();
    let mut out = Vec::new();
    for id in ids {
        let task_path = dir.join(format!("{id}.toml"));
        let text = std::fs::read_to_string(&task_path).with_context(|| task_path.display().to_string())?;
        let task = parse_task(&text, Some(&id)).with_context(|| task_path.display().to_string())?;
        let oracle_path = dir.join(format!("{id}.oracle.jsonl"));
        let oracle_text =
            std::fs::read_to_string(&oracle_path).with_context(|| oracle_path.display().to_string())?;
        let oracle = parse_oracle(&oracle_text).with_context(|| oracle_path.display().to_string())?;
        out.push(Loaded { task, oracle });
    }
    Ok(out)
}

/// Replaces every string that is exactly `$name` with the saved value, recursively.
///
/// # Errors
/// The name of an unbound variable.
pub fn substitute(value: &Value, vars: &BTreeMap<String, Value>) -> Result<Value> {
    Ok(match value {
        Value::String(text) => match text.strip_prefix('$') {
            Some(name) if !name.is_empty() => vars
                .get(name)
                .cloned()
                .with_context(|| format!("variable ${name} is not bound"))?,
            _ => value.clone(),
        },
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| substitute(v, vars)).collect::<Result<_>>()?)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| Ok((k.clone(), substitute(v, vars)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// The hash a baseline stores for a value (screenshots are large; the hash is what is compared).
#[must_use]
pub fn value_hash(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes()).to_hex().to_string()
}

/// Judges `value` (what the check's pointer resolved to, `None` when nothing) against `predicate`.
/// `baseline` is the hash recorded when the setup finished. Returns the verdict and a detail line.
#[must_use]
pub fn evaluate(predicate: &Predicate, value: Option<&Value>, baseline: Option<&str>) -> (bool, String) {
    let present = value.filter(|v| !v.is_null());
    match predicate {
        Predicate::Exists => (present.is_some(), describe(value)),
        Predicate::Absent => (present.is_none(), describe(value)),
        Predicate::Equals { value: want } => match value {
            Some(got) => (json_equal(got, want), format!("got {}, want {want}", short(got))),
            None => (false, format!("nothing at the pointer, want {want}")),
        },
        Predicate::NotEquals { value: avoid } => match value {
            Some(got) => (
                !json_equal(got, avoid),
                format!("got {}, must differ from {avoid}", short(got)),
            ),
            None => (true, "nothing at the pointer".into()),
        },
        Predicate::CountRange { min, max } => {
            let count = value.and_then(|v| match v {
                Value::Array(a) => Some(a.len() as u64),
                Value::Object(o) => Some(o.len() as u64),
                Value::String(s) => Some(s.chars().count() as u64),
                _ => None,
            });
            match count {
                Some(n) => (
                    (*min..=*max).contains(&n),
                    format!("count {n}, want {min}..={max}"),
                ),
                None => (false, format!("no array at the pointer ({})", describe(value))),
            }
        }
        Predicate::NumberRange { min, max } => match value.and_then(Value::as_f64) {
            Some(n) => (n >= *min && n <= *max, format!("{n}, want {min}..={max}")),
            None => (false, format!("no number at the pointer ({})", describe(value))),
        },
        Predicate::WithinDistance { point, max } => match position(value) {
            Some(p) => {
                let d = ((p[0] - point[0]).powi(2) + (p[1] - point[1]).powi(2) + (p[2] - point[2]).powi(2))
                    .sqrt();
                (
                    d <= *max,
                    format!("distance {d:.3} m from {point:?}, want <= {max}"),
                )
            }
            None => (
                false,
                format!("no [x, y, z] at the pointer ({})", describe(value)),
            ),
        },
        Predicate::Contains { text } => match value {
            Some(Value::String(s)) => (s.contains(text.as_str()), format!("string of {} chars", s.len())),
            Some(Value::Array(items)) => (
                items.iter().any(|i| i.as_str() == Some(text.as_str())),
                format!("array of {} items", items.len()),
            ),
            _ => (
                false,
                format!("no string or array at the pointer ({})", describe(value)),
            ),
        },
        Predicate::ImageDiffers => match (value, baseline) {
            (Some(now), Some(before)) => {
                let hash = value_hash(now);
                (hash != before, "compared with the setup baseline".into())
            }
            (None, _) => (false, "nothing at the pointer".into()),
            (_, None) => (false, "no baseline was recorded".into()),
        },
    }
}

/// Numbers compare as numbers (`1` equals `1.0`); everything else structurally.
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() <= f64::EPSILON * x.abs().max(y.abs()).max(1.0),
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => a == b,
    }
}

/// `[x, y, z]` of an array of three numbers.
fn position(value: Option<&Value>) -> Option<[f64; 3]> {
    let items = value?.as_array()?;
    if items.len() != 3 {
        return None;
    }
    Some([items[0].as_f64()?, items[1].as_f64()?, items[2].as_f64()?])
}

/// A short rendering of a value for a detail line.
fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 120 {
        format!("{}…", text.chars().take(120).collect::<String>())
    } else {
        text
    }
}

/// [`short`] of an optional value.
fn describe(value: Option<&Value>) -> String {
    value.map_or_else(|| "nothing".to_string(), short)
}

/// Where the calls go: a live engine, or a fake in the unit tests.
pub trait Session {
    /// Calls a protocol method.
    ///
    /// # Errors
    /// The error text (a protocol error, or a lost connection).
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// The verdict of one check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    /// The check's name.
    pub name: String,
    /// Whether the predicate held.
    pub pass: bool,
    /// Why, in one line.
    pub detail: String,
}

/// The verdict of one task.
#[derive(Debug, Clone, Serialize)]
pub struct TaskOutcome {
    /// Task id.
    pub id: String,
    /// Task title.
    pub title: String,
    /// Every check held and nothing failed on the way.
    pub pass: bool,
    /// Protocol calls the oracle made.
    pub oracle_calls: usize,
    /// The call budget of the task.
    pub call_budget: u32,
    /// A problem outside the checks (target would not start, an oracle call failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The checks, in order.
    pub checks: Vec<CheckOutcome>,
}

/// The baselines of the checks that need one, by check name: hashes of the values at setup end.
pub type Baselines = BTreeMap<String, String>;

/// Runs the query of a check (its params may use the setup's saved variables) and resolves its pointer.
fn query(
    session: &mut dyn Session,
    check: &Check,
    vars: &BTreeMap<String, Value>,
) -> Result<Option<Value>, String> {
    let params = substitute(&check.params, vars).map_err(|e| format!("{e:#}"))?;
    let result = session.call(&check.method, params)?;
    Ok(result.pointer(&check.pointer).cloned())
}

/// Applies a call list in order, binding saved values.
fn run_calls(session: &mut dyn Session, calls: &[Call], vars: &mut BTreeMap<String, Value>) -> Result<usize> {
    for (index, call) in calls.iter().enumerate() {
        let params = substitute(&call.params, vars)
            .with_context(|| format!("call {} ({})", index + 1, call.method))?;
        let result = session
            .call(&call.method, params)
            .map_err(|e| anyhow::anyhow!("call {} ({}) failed: {e}", index + 1, call.method))?;
        for (name, pointer) in &call.save {
            let value = result.pointer(pointer).cloned().with_context(|| {
                format!(
                    "call {} ({}): no value at {pointer} to save as ${name}",
                    index + 1,
                    call.method
                )
            })?;
            vars.insert(name.clone(), value);
        }
    }
    Ok(calls.len())
}

/// Applies the setup, then records the baselines. The returned variables stay bound for the oracle
/// and for the checks.
///
/// # Errors
/// A failed setup call or baseline query.
pub fn prepare(session: &mut dyn Session, task: &Task) -> Result<(BTreeMap<String, Value>, Baselines)> {
    let mut vars = BTreeMap::new();
    run_calls(session, &task.setup.calls, &mut vars).context("setup")?;
    let mut baselines = Baselines::new();
    for check in &task.checks {
        if matches!(check.expect, Predicate::ImageDiffers) {
            let value = query(session, check, &vars)
                .map_err(|e| anyhow::anyhow!("baseline of {:?}: {e}", check.name))?
                .with_context(|| format!("baseline of {:?}: nothing at {}", check.name, check.pointer))?;
            baselines.insert(check.name.clone(), value_hash(&value));
        }
    }
    Ok((vars, baselines))
}

/// Evaluates every check of `task` against the engine behind `session`.
pub fn evaluate_checks(
    session: &mut dyn Session,
    task: &Task,
    baselines: &Baselines,
    vars: &BTreeMap<String, Value>,
) -> Vec<CheckOutcome> {
    task.checks
        .iter()
        .map(|check| match query(session, check, vars) {
            Ok(value) => {
                let (pass, detail) = evaluate(
                    &check.expect,
                    value.as_ref(),
                    baselines.get(&check.name).map(String::as_str),
                );
                CheckOutcome {
                    name: check.name.clone(),
                    pass,
                    detail,
                }
            }
            Err(e) => CheckOutcome {
                name: check.name.clone(),
                pass: false,
                detail: format!("query {} failed: {e}", check.method),
            },
        })
        .collect()
}

/// Plays one task: setup, baselines, the oracle (none for an empty run), checks.
pub fn play_task(session: &mut dyn Session, loaded: &Loaded, oracle: bool) -> TaskOutcome {
    let task = &loaded.task;
    let mut outcome = TaskOutcome {
        id: task.id.clone(),
        title: task.title.clone(),
        pass: false,
        oracle_calls: 0,
        call_budget: task.call_budget,
        error: None,
        checks: Vec::new(),
    };
    let (mut vars, baselines) = match prepare(session, task) {
        Ok(prepared) => prepared,
        Err(e) => {
            outcome.error = Some(format!("{e:#}"));
            return outcome;
        }
    };
    if oracle {
        match run_calls(session, &loaded.oracle, &mut vars) {
            Ok(n) => outcome.oracle_calls = n,
            Err(e) => {
                outcome.error = Some(format!("oracle: {e:#}"));
                return outcome;
            }
        }
        if outcome.oracle_calls > task.call_budget as usize {
            outcome.error = Some(format!(
                "the oracle needs {} calls, over the budget of {}",
                outcome.oracle_calls, task.call_budget
            ));
        }
    }
    outcome.checks = evaluate_checks(session, task, &baselines, &vars);
    outcome.pass = outcome.error.is_none() && outcome.checks.iter().all(|c| c.pass);
    outcome
}

/// The run's report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// [`REPORT_FORMAT_VERSION`].
    pub format_version: u32,
    /// `oracle`, `empty` or `check-only`.
    pub mode: String,
    /// One row per task.
    pub tasks: Vec<TaskOutcome>,
    /// Tasks that passed.
    pub passed: usize,
    /// Tasks run.
    pub total: usize,
    /// `passed / total`.
    pub success_rate: f64,
}

impl Report {
    /// Builds the report of `tasks`.
    #[must_use]
    pub fn new(mode: &str, tasks: Vec<TaskOutcome>) -> Self {
        let passed = tasks.iter().filter(|t| t.pass).count();
        let total = tasks.len();
        Self {
            format_version: REPORT_FORMAT_VERSION,
            mode: mode.to_string(),
            tasks,
            passed,
            total,
            success_rate: if total == 0 {
                0.0
            } else {
                passed as f64 / total as f64
            },
        }
    }

    /// The table for a terminal: one row per task, failures with their first failing check.
    #[must_use]
    pub fn table(&self) -> String {
        let width = self.tasks.iter().map(|t| t.id.len()).max().unwrap_or(2).max(2);
        let mut out = format!("{:<width$}  {:<4}  {:>5}  {}\n", "id", "ok", "calls", "title");
        for t in &self.tasks {
            out.push_str(&format!(
                "{:<width$}  {:<4}  {:>5}  {}\n",
                t.id,
                if t.pass { "ok" } else { "FAIL" },
                t.oracle_calls,
                t.title
            ));
            if !t.pass {
                if let Some(e) = &t.error {
                    out.push_str(&format!("{:width$}    error: {e}\n", ""));
                }
                for c in t.checks.iter().filter(|c| !c.pass) {
                    out.push_str(&format!("{:width$}    check {:?}: {}\n", "", c.name, c.detail));
                }
            }
        }
        out.push_str(&format!(
            "{} of {} tasks passed ({:.0}% success)\n",
            self.passed,
            self.total,
            self.success_rate * 100.0
        ));
        out
    }
}

/// Arguments of `cargo xtask agent-bench`.
#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Start each task's headless target, play its oracle and evaluate its checks.
    #[arg(long, conflicts_with_all = ["check_only", "serve"])]
    pub oracle: bool,
    /// With `--oracle`: play no oracle. Every task must then fail (the checks can fail).
    #[arg(long, requires = "oracle")]
    pub empty: bool,
    /// Start one task's target, apply its setup and wait for an outside MCP client.
    #[arg(long, value_name = "TASK", conflicts_with = "check_only")]
    pub serve: Option<String>,
    /// Evaluate the checks against an engine a client has driven (needs `--attach`).
    #[arg(long, requires = "attach")]
    pub check_only: bool,
    /// The engine to check: a project directory (its `.regia/agent` endpoint) or a `ws://` URL.
    #[arg(long, value_name = "DIR|URL")]
    pub attach: Option<String>,
    /// With a `ws://` URL: the file holding the bearer token.
    #[arg(long, value_name = "FILE")]
    pub token_file: Option<PathBuf>,
    /// Dev aid for writing oracles: run the JSONL calls on stdin against a fresh TARGET and print
    /// every answer.
    #[arg(long, value_name = "TARGET", conflicts_with_all = ["oracle", "check_only", "serve"])]
    pub probe: Option<String>,
    /// Limit to these task ids (repeatable; default all).
    #[arg(long = "task", value_name = "ID")]
    pub tasks: Vec<String>,
    /// The task directory (default `bench/agent_tasks`).
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// Print the report as JSON only.
    #[arg(long)]
    pub json: bool,
    /// Also write the JSON report to this file.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

/// Runs the command.
///
/// # Errors
/// An unreadable task set, an engine that would not start or connect.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if let Some(target) = &args.probe {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading stdin")?;
        return targets::probe(target, &text);
    }
    let dir = args.dir.clone().unwrap_or_else(|| root.join("bench/agent_tasks"));
    let mut loaded = load_dir(&dir)?;
    let served = if args.check_only && args.tasks.is_empty() {
        args.attach.as_deref().and_then(targets::served_task)
    } else {
        None
    };
    let wanted: Vec<String> = if args.tasks.is_empty() {
        served.into_iter().collect()
    } else {
        args.tasks.clone()
    };
    if !wanted.is_empty() {
        for id in &wanted {
            if !loaded.iter().any(|l| &l.task.id == id) {
                bail!("no task {id:?} in {}", dir.display());
            }
        }
        loaded.retain(|l| wanted.contains(&l.task.id));
    }
    if let Some(id) = &args.serve {
        let Some(task) = loaded.iter().find(|l| &l.task.id == id) else {
            bail!("no task {id:?} in {}", dir.display());
        };
        return targets::serve(task);
    }
    if args.check_only {
        return check_only(&loaded, args);
    }
    if !args.oracle {
        bail!("choose a mode: --oracle [--empty], --serve <task>, or --check-only --attach <DIR|URL>");
    }
    let mut outcomes = Vec::new();
    for item in &loaded {
        eprintln!("agent-bench: {}", item.task.id);
        outcomes.push(targets::run_in_process(item, !args.empty));
    }
    let mode = if args.empty { "empty" } else { "oracle" };
    finish(&Report::new(mode, outcomes), args, args.empty)
}

/// `--check-only`: the checks against an attached engine; the baselines are the ones `--serve` saved.
fn check_only(loaded: &[Loaded], args: &Args) -> Result<ExitCode> {
    let Some(attach) = &args.attach else {
        bail!("--check-only needs --attach");
    };
    let mut outcomes = Vec::new();
    let mut client = targets::attach(attach, args.token_file.as_deref())?;
    for item in loaded {
        let (baselines, vars) = targets::read_served(attach, &item.task.id);
        let checks = evaluate_checks(&mut client, &item.task, &baselines, &vars);
        outcomes.push(TaskOutcome {
            id: item.task.id.clone(),
            title: item.task.title.clone(),
            pass: checks.iter().all(|c| c.pass),
            oracle_calls: 0,
            call_budget: item.task.call_budget,
            error: None,
            checks,
        });
    }
    finish(&Report::new("check-only", outcomes), args, false)
}

/// Prints and saves the report. `expect_failure`: the empty run succeeds when every task fails.
fn finish(report: &Report, args: &Args, expect_failure: bool) -> Result<ExitCode> {
    let json = serde_json::to_string_pretty(&json!(report)).context("serializing the report")?;
    if let Some(out) = &args.out {
        std::fs::write(out, format!("{json}\n")).with_context(|| out.display().to_string())?;
    }
    if args.json {
        println!("{json}");
    } else {
        print!("{}", report.table());
    }
    let ok = if expect_failure {
        let wrongly_passing: Vec<&str> = report
            .tasks
            .iter()
            .filter(|t| t.pass)
            .map(|t| t.id.as_str())
            .collect();
        if !wrongly_passing.is_empty() {
            eprintln!(
                "agent-bench: with no oracle these tasks still pass (their checks cannot fail): {}",
                wrongly_passing.join(", ")
            );
        }
        // A task that errored before its checks ran proves nothing about them.
        let errored: Vec<&str> = report
            .tasks
            .iter()
            .filter(|t| t.error.is_some())
            .map(|t| t.id.as_str())
            .collect();
        if !errored.is_empty() {
            eprintln!(
                "agent-bench: these tasks failed outside their checks: {}",
                errored.join(", ")
            );
        }
        wrongly_passing.is_empty() && errored.is_empty()
    } else {
        report.passed == report.total
    };
    Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = r#"
format_version = 1
id = "demo"
title = "Demo"
prompt = "Do the thing."
features = ["scene"]
crates = ["regia_agent"]
call_budget = 5

[setup]
target = "scene"

[[setup.call]]
method = "scene.spawn"
params = { name = "Seed" }
save = { seed = "/entity" }

[[check]]
name = "one"
method = "scene.query_entities"
params = { name = "Seed" }
pointer = "/count"
expect = { kind = "equals", value = 1 }
"#;

    #[test]
    fn a_task_parses_with_setup_and_check() {
        let task = parse_task(TASK, Some("demo")).expect("parses");
        assert_eq!(task.setup.calls.len(), 1);
        assert_eq!(task.setup.calls[0].save["seed"], "/entity");
        assert_eq!(task.checks[0].expect, Predicate::Equals { value: json!(1) });
    }

    /// The frozen `agent-task` format's fuzz coverage (`regia_agent` `tests/fuzz_targets.rs`
    /// `COVERED_ELSEWHERE`): mutated task files parse or are refused with a message, never panic.
    #[test]
    fn agent_task_toml_never_panics() {
        parse_task(TASK, None).expect("the test task parses");
        regia_test_utils::fuzz::fuzz_text_loader(TASK, 0xA6E7, 2000, |text: &str| parse_task(text, None));
    }

    #[test]
    fn bad_tasks_are_refused() {
        let wrong_version = TASK.replace("format_version = 1", "format_version = 2");
        assert!(parse_task(&wrong_version, None).is_err());
        assert!(parse_task(&TASK.replace("id = \"demo\"", "id = \"Demo\""), None).is_err());
        assert!(parse_task(TASK, Some("other")).is_err());
        assert!(parse_task(&TASK.replace("target = \"scene\"", "target = \"nope\""), None).is_err());
        assert!(parse_task(&TASK.replace("[\"scene\"]", "[\"cooking\"]"), None).is_err());
        assert!(parse_task(&TASK.replace("call_budget = 5", "call_budget = 0"), None).is_err());
        assert!(parse_task(&TASK.replace("pointer = \"/count\"", "pointer = \"count\""), None).is_err());
        assert!(
            parse_task(&format!("{TASK}\nextra = 1\n"), None).is_err(),
            "unknown fields are refused"
        );
        let no_check = TASK.split("[[check]]").next().expect("head");
        assert!(parse_task(no_check, None).is_err());
    }

    #[test]
    fn oracle_lines_parse_and_comments_are_skipped() {
        let text = "# comment\n\n{\"method\":\"play.pause\"}\n{\"method\":\"scene.spawn\",\"params\":{\"name\":\"A\"},\"save\":{\"a\":\"/entity\"}}\n";
        let steps = parse_oracle(text).expect("parses");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].save["a"], "/entity");
        assert!(parse_oracle("{not json}").is_err());
        assert!(parse_oracle("{\"method\":\"x\",\"bogus\":1}").is_err());
    }

    #[test]
    fn variables_are_substituted_deeply() {
        let mut vars = BTreeMap::new();
        vars.insert("e".to_string(), json!(42));
        let out =
            substitute(&json!({"entity": "$e", "list": ["$e", "plain"], "n": 1}), &vars).expect("bound");
        assert_eq!(out, json!({"entity": 42, "list": [42, "plain"], "n": 1}));
        assert!(substitute(&json!("$missing"), &vars).is_err());
        assert_eq!(substitute(&json!("$"), &vars).expect("lone dollar"), json!("$"));
    }

    #[test]
    fn predicates_judge_values() {
        let p = |pred: Predicate, v: Option<Value>| evaluate(&pred, v.as_ref(), None).0;
        assert!(p(Predicate::Equals { value: json!(1) }, Some(json!(1.0))));
        assert!(!p(Predicate::Equals { value: json!(1) }, Some(json!(2))));
        assert!(!p(Predicate::Equals { value: json!(1) }, None));
        assert!(p(Predicate::NotEquals { value: json!("a") }, Some(json!("b"))));
        assert!(p(Predicate::CountRange { min: 2, max: 3 }, Some(json!([1, 2]))));
        assert!(!p(
            Predicate::CountRange { min: 2, max: 3 },
            Some(json!([1, 2, 3, 4]))
        ));
        assert!(
            !p(Predicate::CountRange { min: 0, max: 3 }, Some(json!(7))),
            "a number has no count"
        );
        assert!(p(
            Predicate::NumberRange { min: 0.5, max: 1.0 },
            Some(json!(0.75))
        ));
        assert!(!p(
            Predicate::NumberRange { min: 0.5, max: 1.0 },
            Some(json!("x"))
        ));
        let near = Predicate::WithinDistance {
            point: [1.0, 0.0, 0.0],
            max: 0.5,
        };
        assert!(p(near.clone(), Some(json!([1.2, 0.0, 0.0]))));
        assert!(!p(near.clone(), Some(json!([2.0, 0.0, 0.0]))));
        assert!(!p(near, Some(json!([1.0, 0.0]))), "needs three numbers");
        assert!(p(Predicate::Exists, Some(json!(0))));
        assert!(!p(Predicate::Exists, Some(Value::Null)));
        assert!(p(Predicate::Absent, None));
        assert!(p(Predicate::Absent, Some(Value::Null)));
        assert!(!p(Predicate::Absent, Some(json!(false))));
        let has = Predicate::Contains { text: "ok".into() };
        assert!(p(has.clone(), Some(json!("looks ok"))));
        assert!(p(has.clone(), Some(json!(["ok", "no"]))));
        assert!(!p(has, Some(json!(["okay"]))));
    }

    #[test]
    fn image_differs_needs_a_baseline_and_a_change() {
        let before = json!("AAAA");
        let hash = value_hash(&before);
        let differs = Predicate::ImageDiffers;
        assert!(!evaluate(&differs, Some(&before), Some(&hash)).0, "same bytes");
        assert!(
            evaluate(&differs, Some(&json!("BBBB")), Some(&hash)).0,
            "changed bytes"
        );
        assert!(!evaluate(&differs, Some(&before), None).0, "no baseline fails");
        assert!(!evaluate(&differs, None, Some(&hash)).0);
    }

    /// A fake engine: a counter the calls change.
    struct Fake {
        entities: Vec<String>,
        calls: usize,
    }

    impl Session for Fake {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.calls += 1;
            match method {
                "scene.spawn" => {
                    self.entities
                        .push(params["name"].as_str().unwrap_or("").to_string());
                    Ok(json!({"entity": self.entities.len() as u64 + 100}))
                }
                "scene.query_entities" => {
                    let n = self
                        .entities
                        .iter()
                        .filter(|e| Some(e.as_str()) == params["name"].as_str())
                        .count();
                    Ok(json!({"count": n}))
                }
                "scene.despawn" => {
                    self.entities.clear();
                    Ok(json!({}))
                }
                other => Err(format!("unknown method {other}")),
            }
        }
    }

    fn loaded(oracle: &str) -> Loaded {
        Loaded {
            task: parse_task(TASK, Some("demo")).expect("parses"),
            oracle: parse_oracle(oracle).expect("oracle"),
        }
    }

    #[test]
    fn a_task_passes_with_its_oracle_and_fails_without() {
        let item = loaded("{\"method\":\"scene.spawn\",\"params\":{\"name\":\"Seed\"}}\n");
        // The setup spawns one `Seed`; the check wants exactly one. The oracle's second one breaks it.
        let mut engine = Fake {
            entities: Vec::new(),
            calls: 0,
        };
        let empty = play_task(&mut engine, &item, false);
        assert!(empty.pass, "setup alone satisfies this toy check: {empty:?}");
        let mut engine = Fake {
            entities: Vec::new(),
            calls: 0,
        };
        let solved = play_task(&mut engine, &item, true);
        assert!(!solved.pass, "two seeds break the check");
        assert_eq!(solved.oracle_calls, 1);
        assert!(solved.checks[0].detail.contains("count 2") || solved.checks[0].detail.contains("got 2"));
    }

    #[test]
    fn saved_variables_reach_the_oracle() {
        let item = loaded("{\"method\":\"scene.despawn\",\"params\":{\"entity\":\"$seed\"}}\n");
        let mut engine = Fake {
            entities: Vec::new(),
            calls: 0,
        };
        let outcome = play_task(&mut engine, &item, true);
        assert!(outcome.error.is_none(), "{outcome:?}");
        assert!(!outcome.pass, "the despawn removed the seed, so the count is 0");
    }

    #[test]
    fn a_failing_oracle_call_is_a_task_error() {
        let item = loaded("{\"method\":\"nope\"}\n");
        let mut engine = Fake {
            entities: Vec::new(),
            calls: 0,
        };
        let outcome = play_task(&mut engine, &item, true);
        assert!(!outcome.pass);
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|e| e.contains("oracle") && e.contains("nope"))
        );
    }

    #[test]
    fn an_oracle_over_budget_fails_the_task() {
        let mut item = loaded("{\"method\":\"scene.query_entities\"}\n");
        item.task.call_budget = 1;
        item.oracle = vec![item.oracle[0].clone(), item.oracle[0].clone()];
        let mut engine = Fake {
            entities: Vec::new(),
            calls: 0,
        };
        let outcome = play_task(&mut engine, &item, true);
        assert!(outcome.error.as_deref().is_some_and(|e| e.contains("budget")));
        assert!(!outcome.pass);
    }

    #[test]
    fn the_report_counts_and_prints_failures() {
        let ok = TaskOutcome {
            id: "a".into(),
            title: "A".into(),
            pass: true,
            oracle_calls: 2,
            call_budget: 5,
            error: None,
            checks: Vec::new(),
        };
        let bad = TaskOutcome {
            id: "b".into(),
            title: "B".into(),
            pass: false,
            oracle_calls: 0,
            call_budget: 5,
            error: None,
            checks: vec![CheckOutcome {
                name: "c".into(),
                pass: false,
                detail: "count 1, want 2..=3".into(),
            }],
        };
        let report = Report::new("oracle", vec![ok, bad]);
        assert_eq!((report.passed, report.total), (1, 2));
        assert!((report.success_rate - 0.5).abs() < 1e-9);
        let table = report.table();
        assert!(
            table.contains("FAIL") && table.contains("count 1, want 2..=3"),
            "{table}"
        );
        assert!(table.contains("1 of 2 tasks passed (50% success)"), "{table}");
    }

    /// The shipped set, read from the repository.
    fn shipped() -> Vec<Loaded> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bench/agent_tasks");
        load_dir(&dir).expect("bench/agent_tasks loads")
    }

    #[test]
    fn the_shipped_set_has_25_sound_tasks() {
        let set = shipped();
        assert_eq!(set.len(), TASK_COUNT, "spec 23 § 8 asks for 25 tasks");
        let mut ids: Vec<&str> = set.iter().map(|l| l.task.id.as_str()).collect();
        ids.dedup();
        assert_eq!(ids.len(), TASK_COUNT, "ids are unique");
        for item in &set {
            let task = &item.task;
            assert!(!item.oracle.is_empty(), "{}: an oracle has calls", task.id);
            assert!(
                item.oracle.len() <= task.call_budget as usize,
                "{}: the oracle ({} calls) fits the budget ({})",
                task.id,
                item.oracle.len(),
                task.call_budget
            );
            // A task whose setup and checks use variables only uses ones something binds.
            let mut bound: Vec<&str> = Vec::new();
            for call in &task.setup.calls {
                bound.extend(call.save.keys().map(String::as_str));
            }
            for step in &item.oracle {
                for name in referenced(&step.params) {
                    assert!(
                        bound.contains(&name.as_str()),
                        "{}: oracle uses unbound ${name}",
                        task.id
                    );
                }
                bound.extend(step.save.keys().map(String::as_str));
            }
            for check in &task.checks {
                for name in referenced(&check.params) {
                    assert!(
                        task.setup.calls.iter().any(|c| c.save.contains_key(&name)),
                        "{}: check {:?} uses ${name}, which the setup does not save",
                        task.id,
                        check.name
                    );
                }
            }
        }
    }

    /// The `$name` variables a JSON value mentions.
    fn referenced(value: &Value) -> Vec<String> {
        match value {
            Value::String(s) => s
                .strip_prefix('$')
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .into_iter()
                .collect(),
            Value::Array(items) => items.iter().flat_map(referenced).collect(),
            Value::Object(map) => map.values().flat_map(referenced).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn the_shipped_set_spans_every_area() {
        let set = shipped();
        for area in FEATURE_AREAS {
            let n = set
                .iter()
                .filter(|l| l.task.features.iter().any(|f| f == area))
                .count();
            assert!(n >= 1, "no task exercises {area}");
        }
        for target in targets::TARGETS {
            assert!(
                set.iter().any(|l| l.task.setup.target == target.name),
                "no task uses the target {}",
                target.name
            );
        }
    }

    #[test]
    fn the_shipped_checks_use_known_predicate_shapes() {
        // Every check names a method the protocol schema has.
        for item in shipped() {
            for check in &item.task.checks {
                assert!(
                    regia_agent::schema::method(&check.method).is_some(),
                    "{}: check {:?} calls unknown method {}",
                    item.task.id,
                    check.name,
                    check.method
                );
            }
            for call in item.task.setup.calls.iter().chain(&item.oracle) {
                assert!(
                    regia_agent::schema::method(&call.method).is_some(),
                    "{}: unknown method {}",
                    item.task.id,
                    call.method
                );
            }
        }
    }

    #[test]
    fn the_fingerprint_names_every_predicate() {
        let f = task_schema_fingerprint();
        for kind in ["equals", "count_range", "within_distance", "image_differs"] {
            assert!(f.contains(kind), "{kind}");
        }
    }
}
