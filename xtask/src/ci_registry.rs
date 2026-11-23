// SPDX-License-Identifier: MPL-2.0
//! The CI registries under `docs/ci/` (ADR-1035):
//!
//! - `flaky-tests.toml` (`cargo xtask flaky`): every known flaky or load-sensitive test binary,
//!   how the gate treats it (run alone, quarantined on the real-GPU pass, exit-crash rule) and who
//!   owns the fix. `--gate-args <pass>` prints the gate test runner's `--solo` / `--quarantine`
//!   arguments, so the gate scripts read the list from here instead of keeping their own.
//! - `nightly-jobs.toml` (`cargo xtask nightly`): the nightly and periodic jobs (player sweep,
//!   look CI, soaks, Android lint, benches), where and when they run, and their in-repo command.
//!
//! `--check` validates both against the tree: the packages, test targets and test functions an
//! entry names exist, and an xtask sub-command a job runs exists, so a renamed test or a removed
//! command cannot leave a stale entry behind. The retry policy itself is in
//! `book/user-guide/flaky-tests.md`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args as ClapArgs, ValueEnum};
use serde::Deserialize;

use crate::preflight::{Member, members};

/// The flaky-test registry, relative to the workspace root.
pub const FLAKY_PATH: &str = "docs/ci/flaky-tests.toml";

/// The nightly job registry, relative to the workspace root.
pub const NIGHTLY_PATH: &str = "docs/ci/nightly-jobs.toml";

/// The `version` both registry files carry.
pub const REGISTRY_VERSION: u32 = 1;

/// Cargo target kinds a test key may name (`+`-joined, as cargo reports them).
const TARGET_KINDS: &[&str] = &[
    "lib",
    "rlib",
    "cdylib",
    "dylib",
    "staticlib",
    "proc-macro",
    "bin",
    "test",
    "bench",
    "example",
];

/// What a nightly job may need from its host.
const JOB_NEEDS: &[&str] = &[
    "gpu",
    "gpu-exclusive",
    "do27-content",
    "device",
    "display",
    "network",
    "windows",
    "mac",
    "deck",
];

/// Weekday names a weekly schedule may use.
const WEEKDAYS: &[&str] = &["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

// ---- flaky-test registry -------------------------------------------------------------------

/// `docs/ci/flaky-tests.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlakyRegistry {
    /// Registry format version; must be [`REGISTRY_VERSION`].
    pub version: u32,
    /// The entries, in file order.
    #[serde(default, rename = "flake")]
    pub flakes: Vec<Flake>,
}

/// One known flaky or load-sensitive test binary (or family of binaries).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flake {
    /// Short kebab-case name, unique in the file.
    pub id: String,
    /// The gate runner's binary keys, `package:target:kind` (as `fgate-runtests.py` prints them).
    pub tests: Vec<String>,
    /// Test functions inside those binaries; each must exist in the package.
    #[serde(default)]
    pub test_fns: Vec<String>,
    /// What a failure looks like.
    pub symptom: String,
    /// The root cause, as far as it is known.
    pub cause: String,
    /// Where the fix stands.
    pub status: FlakeStatus,
    /// What the gate does about it.
    pub handling: Handling,
    /// The gate passes `handling` applies to (required for `solo` and `quarantine`).
    #[serde(default)]
    pub passes: Vec<GatePass>,
    /// Who owns the fix (a lane, the lead).
    pub owner: String,
    /// Dates (`YYYY-MM-DD`) it was seen failing a gate or a run.
    #[serde(default)]
    pub seen: Vec<String>,
    /// When the entry must be looked at again (`YYYY-MM-DD`); required while open or mitigated.
    #[serde(default)]
    pub review_by: Option<String>,
    /// ADRs, commits and reports.
    #[serde(default)]
    pub refs: Vec<String>,
}

/// Where a flake's fix stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakeStatus {
    /// Seen failing; no fix yet.
    Open,
    /// A fix or workaround landed; not yet proven (a soak or a quiet week pending).
    Mitigated,
    /// Fixed and proven; kept for history.
    Fixed,
    /// Not seen failing, but the test has a known load-sensitive shape (wall clock, shared state).
    Latent,
}

impl FlakeStatus {
    /// The registry spelling.
    pub fn label(self) -> &'static str {
        match self {
            FlakeStatus::Open => "open",
            FlakeStatus::Mitigated => "mitigated",
            FlakeStatus::Fixed => "fixed",
            FlakeStatus::Latent => "latent",
        }
    }
}

/// What the gate does about a flake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Handling {
    /// Nothing per test: the runner's general rule (a failure is rerun once alone) applies.
    None,
    /// The binary runs alone after the parallel pool (`--solo`), as `cargo test` would run it.
    Solo,
    /// Reported but not counted on the named passes (`--quarantine`); real-GPU pass only.
    Quarantine,
    /// The runner's exit-crash rule applies (a signal at process exit after every test passed is
    /// counted as a pass and printed as `EXIT-CRASH`). Documentation only: the rule is global.
    ExitCrash,
}

impl Handling {
    /// The registry spelling.
    pub fn label(self) -> &'static str {
        match self {
            Handling::None => "none",
            Handling::Solo => "solo",
            Handling::Quarantine => "quarantine",
            Handling::ExitCrash => "exit-crash",
        }
    }
}

/// One pass of the farm gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum GatePass {
    /// The lavapipe pass: every test, goldens compared. The reference; never quarantines.
    Lvp,
    /// The real-GPU pass on a shared RTX 5090 (rendering crates, goldens skipped).
    Gpu,
}

impl GatePass {
    /// The registry spelling.
    pub fn label(self) -> &'static str {
        match self {
            GatePass::Lvp => "lvp",
            GatePass::Gpu => "gpu",
        }
    }
}

/// `cargo xtask flaky`.
#[derive(ClapArgs, Debug, Clone)]
pub struct FlakyArgs {
    /// Only validate the registry (the default also prints it).
    #[arg(long)]
    pub check: bool,
    /// Print the gate test runner's `--solo` / `--quarantine` arguments for this pass, one per
    /// line, for `mapfile -t` in the gate script. Fails when the registry does not validate.
    #[arg(long, value_enum, value_name = "PASS")]
    pub gate_args: Option<GatePass>,
}

// ---- nightly job registry ------------------------------------------------------------------

/// `docs/ci/nightly-jobs.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NightlyRegistry {
    /// Registry format version; must be [`REGISTRY_VERSION`].
    pub version: u32,
    /// The jobs, in file order.
    #[serde(default, rename = "job")]
    pub jobs: Vec<NightlyJob>,
}

/// One nightly or periodic job.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NightlyJob {
    /// Short kebab-case name, unique in the file.
    pub id: String,
    /// What it checks or measures.
    pub what: String,
    /// The in-repo command, run from the workspace root.
    pub command: String,
    /// The farm script that schedules it, takes the locks and copies results (outside the repo).
    #[serde(default)]
    pub wrapper: Option<String>,
    /// Where it runs (machine, card, lock).
    pub host: String,
    /// `daily HH:MM`, `weekly <mon..sun> HH:MM`, `per-tag` or `on-demand`.
    pub schedule: String,
    /// Whether something runs it today.
    pub status: JobStatus,
    /// Whether a failure blocks a merge or a tag.
    #[serde(default)]
    pub blocking: bool,
    /// What the host must have (`gpu`, `gpu-exclusive`, `do27-content`, `device`, ...).
    #[serde(default)]
    pub needs: Vec<String>,
    /// Where the results go.
    pub results: String,
    /// Who looks at the results.
    pub owner: String,
    /// Test functions the command runs; each must exist in the `-p` package.
    #[serde(default)]
    pub test_fns: Vec<String>,
    /// ADRs, commits and reports.
    #[serde(default)]
    pub refs: Vec<String>,
}

/// Whether something runs a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// A cron or a CI schedule runs it.
    Scheduled,
    /// A person runs it (the lead, the owner) at the named times.
    Manual,
    /// Defined, not runnable or not scheduled yet; its xtask command may not exist in this tree.
    Planned,
}

impl JobStatus {
    /// The registry spelling.
    pub fn label(self) -> &'static str {
        match self {
            JobStatus::Scheduled => "scheduled",
            JobStatus::Manual => "manual",
            JobStatus::Planned => "planned",
        }
    }
}

/// `cargo xtask nightly`.
#[derive(ClapArgs, Debug, Clone)]
pub struct NightlyArgs {
    /// Only validate the registry (the default also prints it).
    #[arg(long)]
    pub check: bool,
}

// ---- the tree a registry is checked against ------------------------------------------------

/// The workspace a registry is checked against.
pub struct Tree<'a> {
    /// The workspace root.
    pub root: &'a Path,
    /// Its members (`cargo metadata`).
    pub members: &'a [Member],
}

impl Tree<'_> {
    fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    fn member_dir(&self, member: &Member) -> PathBuf {
        self.root.join(&member.dir)
    }

    /// `None` when `package` has a `kind` target named `target`, else the problem.
    fn target_problem(&self, package: &str, target: &str, kind: &str) -> Option<String> {
        let Some(member) = self.member(package) else {
            return Some(format!("package `{package}` is not a workspace member"));
        };
        let kinds: Vec<&str> = kind.split('+').collect();
        if let Some(bad) = kinds.iter().find(|k| !TARGET_KINDS.contains(k)) {
            return Some(format!("target kind `{bad}` is not a cargo target kind"));
        }
        let dir = self.member_dir(member);
        let candidates: Vec<PathBuf> = if kinds.contains(&"test") {
            vec![
                dir.join("tests").join(format!("{target}.rs")),
                dir.join("tests").join(target).join("main.rs"),
            ]
        } else if kinds.contains(&"bin") {
            let mut c = vec![
                dir.join("src/bin").join(format!("{target}.rs")),
                dir.join("src/bin").join(target).join("main.rs"),
            ];
            if target.replace('-', "_") == package.replace('-', "_") {
                c.push(dir.join("src/main.rs"));
            }
            c
        } else if kinds.contains(&"example") {
            vec![
                dir.join("examples").join(format!("{target}.rs")),
                dir.join("examples").join(target).join("main.rs"),
            ]
        } else if kinds.contains(&"bench") {
            vec![
                dir.join("benches").join(format!("{target}.rs")),
                dir.join("benches").join(target).join("main.rs"),
            ]
        } else {
            vec![dir.join("src/lib.rs")]
        };
        if candidates.iter().any(|c| c.is_file()) {
            return None;
        }
        // A target declared in the manifest's table for its kind (`[[test]] name = "x"`, with its own
        // `path`); the `[package]` name and other kinds' targets do not count.
        let section = ["test", "bin", "example", "bench"]
            .into_iter()
            .find(|k| kinds.contains(k))
            .unwrap_or("lib");
        let manifest: toml::Table = std::fs::read_to_string(dir.join("Cargo.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default();
        let named = |t: &toml::Value| {
            t.as_table()
                .and_then(|table| table.get("name"))
                .and_then(toml::Value::as_str)
                == Some(target)
        };
        let declared = match manifest.get(section) {
            Some(toml::Value::Array(targets)) => targets.iter().any(named),
            Some(lib @ toml::Value::Table(_)) => named(lib),
            _ => false,
        };
        if declared {
            return None;
        }
        Some(format!(
            "package `{package}` has no {kind} target `{target}` (looked for {})",
            candidates
                .iter()
                .map(|c| c.strip_prefix(self.root).unwrap_or(c).display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// Whether some `.rs` file of `package` defines `fn name(`.
    fn package_has_fn(&self, package: &str, name: &str) -> bool {
        let Some(member) = self.member(package) else {
            return false;
        };
        let mut files = Vec::new();
        collect_rs(&self.member_dir(member), &mut files);
        let needle = format!("fn {name}(");
        files
            .iter()
            .any(|f| std::fs::read_to_string(f).is_ok_and(|text| text.contains(&needle)))
    }
}

/// Every `.rs` file under `dir`, skipping `target` and dot directories.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                collect_rs(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Splits a runner key `package:target:kind[:args]`.
pub fn parse_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(4, ':');
    let package = parts.next().filter(|p| !p.is_empty())?;
    let target = parts.next().filter(|p| !p.is_empty())?;
    let kind = parts.next().filter(|p| !p.is_empty())?;
    Some((package, target, kind))
}

/// `YYYY-MM-DD` with a month 01-12 and a day 01-31.
pub fn is_iso_date(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return false;
    }
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

fn is_id(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

// ---- flaky checks --------------------------------------------------------------------------

/// Parses the flaky registry text.
pub fn parse_flaky(text: &str) -> Result<FlakyRegistry> {
    toml::from_str(text).with_context(|| format!("parsing {FLAKY_PATH}"))
}

/// Every problem with the flaky registry. Empty when it is valid.
pub fn check_flaky(reg: &FlakyRegistry, tree: &Tree<'_>) -> Vec<String> {
    let mut errors = Vec::new();
    if reg.version != REGISTRY_VERSION {
        errors.push(format!(
            "{FLAKY_PATH}: version {} (expected {REGISTRY_VERSION})",
            reg.version
        ));
    }
    let mut ids = BTreeSet::new();
    for f in &reg.flakes {
        let mut err = |msg: String| errors.push(format!("{FLAKY_PATH}: flake `{}`: {msg}", f.id));
        if !is_id(&f.id) {
            err("the id must be lowercase ascii, digits and `-`".into());
        }
        if !ids.insert(f.id.as_str()) {
            err("the id is used twice".into());
        }
        if f.tests.is_empty() {
            err("`tests` is empty".into());
        }
        let mut packages = BTreeSet::new();
        for key in &f.tests {
            match parse_key(key) {
                None => err(format!("`{key}` is not `package:target:kind`")),
                Some((package, target, kind)) => {
                    packages.insert(package);
                    if let Some(problem) = tree.target_problem(package, target, kind) {
                        err(format!("`{key}`: {problem}"));
                    }
                }
            }
        }
        for name in &f.test_fns {
            if !packages.iter().any(|p| tree.package_has_fn(p, name)) {
                err(format!("test fn `{name}` is not defined in {packages:?}"));
            }
        }
        for (field, value) in [("symptom", &f.symptom), ("cause", &f.cause), ("owner", &f.owner)] {
            if value.trim().is_empty() {
                err(format!("`{field}` is empty"));
            }
        }
        for date in f.seen.iter().chain(&f.review_by) {
            if !is_iso_date(date) {
                err(format!("`{date}` is not a YYYY-MM-DD date"));
            }
        }
        if matches!(f.status, FlakeStatus::Open | FlakeStatus::Mitigated) && f.review_by.is_none() {
            err("an open or mitigated flake needs `review_by`".into());
        }
        match f.handling {
            Handling::Solo | Handling::Quarantine if f.passes.is_empty() => {
                err("`solo` and `quarantine` need `passes`".into());
            }
            Handling::Quarantine if f.passes.contains(&GatePass::Lvp) => {
                err("the lavapipe pass is the reference and never quarantines (`passes = [\"gpu\"]`)".into());
            }
            Handling::Quarantine if !matches!(f.status, FlakeStatus::Open | FlakeStatus::Mitigated) => {
                err("only an open or mitigated flake may be quarantined; remove the quarantine".into());
            }
            _ => {}
        }
    }
    errors
}

/// Non-fatal notes as of `today` (`YYYY-MM-DD`): overdue reviews, and open flakes seen twice (a P0
/// ticket by the roadmap's process rule).
pub fn flaky_warnings(reg: &FlakyRegistry, today: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for f in &reg.flakes {
        if let Some(date) = &f.review_by
            && matches!(f.status, FlakeStatus::Open | FlakeStatus::Mitigated)
            && date.as_str() < today
        {
            notes.push(format!(
                "flake `{}`: review was due {date} (owner: {})",
                f.id, f.owner
            ));
        }
        if f.status == FlakeStatus::Open && f.seen.len() >= 2 {
            notes.push(format!(
                "flake `{}`: open and seen {} times: a P0 ticket (docs/roadmap-post-alpha.md, Process)",
                f.id,
                f.seen.len()
            ));
        }
    }
    notes
}

/// The gate runner's arguments for `pass`, in registry order: `--solo KEY` for each solo binary
/// and `--quarantine KEY=REASON` for each quarantined one.
pub fn gate_args(reg: &FlakyRegistry, pass: GatePass) -> Vec<String> {
    let mut args = Vec::new();
    for f in reg.flakes.iter().filter(|f| f.passes.contains(&pass)) {
        for key in &f.tests {
            match f.handling {
                Handling::Solo => {
                    args.push("--solo".to_string());
                    args.push(key.clone());
                }
                Handling::Quarantine => {
                    args.push("--quarantine".to_string());
                    let review = f.review_by.as_deref().unwrap_or("?");
                    args.push(format!("{key}={}: {}, review by {review}", f.id, f.owner));
                }
                Handling::None | Handling::ExitCrash => {}
            }
        }
    }
    args
}

fn flaky_table(reg: &FlakyRegistry) -> String {
    let mut out = format!("{FLAKY_PATH}: {} entries\n", reg.flakes.len());
    for f in &reg.flakes {
        let passes: Vec<&str> = f.passes.iter().map(|p| p.label()).collect();
        out.push_str(&format!(
            "  {:<28} {:<10} {:<11} {:<8} seen {:<2} review {:<10}  {}\n",
            f.id,
            f.status.label(),
            f.handling.label(),
            passes.join(","),
            f.seen.len(),
            f.review_by.as_deref().unwrap_or("-"),
            f.owner
        ));
    }
    out
}

// ---- nightly checks ------------------------------------------------------------------------

/// Parses the nightly registry text.
pub fn parse_nightly(text: &str) -> Result<NightlyRegistry> {
    toml::from_str(text).with_context(|| format!("parsing {NIGHTLY_PATH}"))
}

/// `None` when `schedule` is valid, else the problem.
pub fn schedule_problem(schedule: &str) -> Option<String> {
    let parts: Vec<&str> = schedule.split_whitespace().collect();
    let ok = match parts.as_slice() {
        ["on-demand"] | ["per-tag"] => true,
        ["daily", time] => is_hh_mm(time),
        ["weekly", day, time] => WEEKDAYS.contains(day) && is_hh_mm(time),
        _ => false,
    };
    (!ok).then(|| {
        format!(
            "schedule `{schedule}` is not `daily HH:MM`, `weekly <mon..sun> HH:MM`, `per-tag` or `on-demand`"
        )
    })
}

/// `HH:MM`, 24-hour, digits only.
fn is_hh_mm(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 5
        && b[2] == b':'
        && [0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit())
        && text[..2].parse::<u32>().is_ok_and(|h| h < 24)
        && text[3..].parse::<u32>().is_ok_and(|m| m < 60)
}

/// The value after the first of `flags` in `tokens`.
fn flag_value<'a>(tokens: &[&'a str], flags: &[&str]) -> Option<&'a str> {
    flag_values(tokens, flags).into_iter().next()
}

/// The value after every occurrence of `flags` in `tokens` (`--bin a --bin b`).
fn flag_values<'a>(tokens: &[&'a str], flags: &[&str]) -> Vec<&'a str> {
    tokens
        .windows(2)
        .filter(|pair| flags.contains(&pair[0]))
        .map(|pair| pair[1])
        .collect()
}

/// The problems with the nightly registry (fatal) and notes (not fatal). `subcommands` are the
/// xtask sub-command names and aliases this binary has.
pub fn check_nightly(
    reg: &NightlyRegistry,
    tree: &Tree<'_>,
    subcommands: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut notes = Vec::new();
    if reg.version != REGISTRY_VERSION {
        errors.push(format!(
            "{NIGHTLY_PATH}: version {} (expected {REGISTRY_VERSION})",
            reg.version
        ));
    }
    let mut ids = BTreeSet::new();
    for job in &reg.jobs {
        let prefix = format!("{NIGHTLY_PATH}: job `{}`", job.id);
        let mut err = |msg: String| errors.push(format!("{prefix}: {msg}"));
        if !is_id(&job.id) {
            err("the id must be lowercase ascii, digits and `-`".into());
        }
        if !ids.insert(job.id.as_str()) {
            err("the id is used twice".into());
        }
        for (field, value) in [
            ("what", &job.what),
            ("command", &job.command),
            ("host", &job.host),
            ("results", &job.results),
            ("owner", &job.owner),
        ] {
            if value.trim().is_empty() {
                err(format!("`{field}` is empty"));
            }
        }
        if let Some(problem) = schedule_problem(&job.schedule) {
            err(problem);
        }
        if job.status == JobStatus::Scheduled && job.schedule == "on-demand" {
            err("a scheduled job needs a schedule other than `on-demand`".into());
        }
        for need in &job.needs {
            if !JOB_NEEDS.contains(&need.as_str()) {
                err(format!("need `{need}` is not one of {JOB_NEEDS:?}"));
            }
        }
        let tokens: Vec<&str> = job.command.split_whitespace().collect();
        match tokens.as_slice() {
            ["cargo", "xtask", sub, ..] => {
                let known = subcommands.iter().any(|s| s == sub);
                match (known, job.status) {
                    (false, JobStatus::Planned) => notes.push(format!(
                        "{prefix}: planned; `cargo xtask {sub}` is not in this tree yet"
                    )),
                    (false, _) => err(format!("`cargo xtask {sub}` is not an xtask sub-command")),
                    (true, JobStatus::Planned) => notes.push(format!(
                        "{prefix}: planned, and `cargo xtask {sub}` exists: set `status` once something runs it"
                    )),
                    (true, _) => {}
                }
                if !job.test_fns.is_empty() {
                    err("`test_fns` needs a `cargo test -p <package>` command".into());
                }
            }
            ["cargo", "test" | "run" | "build", ..] => {
                let Some(package) = flag_value(&tokens, &["-p", "--package"]) else {
                    err("a cargo command must name its package with `-p`".into());
                    continue;
                };
                if tree.member(package).is_none() {
                    err(format!("package `{package}` is not a workspace member"));
                    continue;
                }
                for (flag, kind) in [("--test", "test"), ("--bin", "bin"), ("--example", "example")] {
                    for target in flag_values(&tokens, &[flag]) {
                        if let Some(problem) = tree.target_problem(package, target, kind) {
                            err(problem);
                        }
                    }
                }
                for name in &job.test_fns {
                    if !tree.package_has_fn(package, name) {
                        err(format!("test fn `{name}` is not defined in `{package}`"));
                    }
                }
            }
            _ => {
                if !job.test_fns.is_empty() {
                    err("`test_fns` needs a `cargo test -p <package>` command".into());
                }
            }
        }
    }
    (errors, notes)
}

fn nightly_table(reg: &NightlyRegistry) -> String {
    let mut out = format!("{NIGHTLY_PATH}: {} jobs\n", reg.jobs.len());
    for job in &reg.jobs {
        out.push_str(&format!(
            "  {:<22} {:<9} {:<18} {:<8} {}\n      {}\n",
            job.id,
            job.status.label(),
            job.schedule,
            if job.blocking { "blocking" } else { "report" },
            job.host,
            job.command
        ));
    }
    out
}

// ---- commands ------------------------------------------------------------------------------

fn read(root: &Path, rel: &str) -> Result<String> {
    let path = root.join(rel);
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn today() -> String {
    crate::soak::report::utc_now_rfc3339()
        .get(..10)
        .unwrap_or_default()
        .to_string()
}

fn finish(name: &str, errors: &[String]) -> ExitCode {
    if errors.is_empty() {
        println!("{name}: ok");
        return ExitCode::SUCCESS;
    }
    for e in errors {
        eprintln!("error: {e}");
    }
    eprintln!("{name}: {} problem(s)", errors.len());
    ExitCode::FAILURE
}

/// `cargo xtask flaky`.
pub fn run_flaky(root: &Path, args: &FlakyArgs) -> Result<ExitCode> {
    let reg = parse_flaky(&read(root, FLAKY_PATH)?)?;
    let members = members(root)?;
    let errors = check_flaky(
        &reg,
        &Tree {
            root,
            members: &members,
        },
    );
    if let Some(pass) = args.gate_args {
        if !errors.is_empty() {
            return Ok(finish("flaky", &errors));
        }
        for arg in gate_args(&reg, pass) {
            println!("{arg}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !args.check {
        print!("{}", flaky_table(&reg));
    }
    for note in flaky_warnings(&reg, &today()) {
        println!("note: {note}");
    }
    Ok(finish("flaky", &errors))
}

/// `cargo xtask nightly`. `subcommands` are the xtask sub-command names and aliases.
pub fn run_nightly(root: &Path, args: &NightlyArgs, subcommands: &[String]) -> Result<ExitCode> {
    let reg = parse_nightly(&read(root, NIGHTLY_PATH)?)?;
    let members = members(root)?;
    let (errors, notes) = check_nightly(
        &reg,
        &Tree {
            root,
            members: &members,
        },
        subcommands,
    );
    if !args.check {
        print!("{}", nightly_table(&reg));
    }
    for note in notes {
        println!("note: {note}");
    }
    Ok(finish("nightly", &errors))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A fixture workspace: package `demo` (lib, test `load`, bin `demo`) with test fn `loads_fast`.
    struct Fixture {
        root: PathBuf,
        members: Vec<Member>,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "regia-ci-registry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            let dir = root.join("crates/demo");
            std::fs::create_dir_all(dir.join("tests")).expect("mkdir");
            std::fs::create_dir_all(dir.join("src")).expect("mkdir");
            std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("write");
            std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}\n").expect("write");
            std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").expect("write");
            std::fs::write(dir.join("tests/load.rs"), "#[test]\nfn loads_fast() {}\n").expect("write");
            let members = vec![Member {
                name: "demo".into(),
                dir: "crates/demo".into(),
                deps: Vec::new(),
            }];
            Fixture { root, members }
        }

        fn tree(&self) -> Tree<'_> {
            Tree {
                root: &self.root,
                members: &self.members,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const GOOD_FLAKE: &str = r#"
version = 1

[[flake]]
id = "demo-load-budget"
tests = ["demo:load:test"]
test_fns = ["loads_fast"]
symptom = "fails under load"
cause = "wall clock"
status = "open"
handling = "solo"
passes = ["lvp"]
owner = "the lead"
seen = ["2026-10-08"]
review_by = "2026-10-28"

[[flake]]
id = "demo-gpu-exit"
tests = ["demo:demo:bin", "demo:demo:lib"]
symptom = "signal at exit"
cause = "driver"
status = "mitigated"
handling = "quarantine"
passes = ["gpu"]
owner = "lane 05"
review_by = "2026-10-25"
"#;

    fn flaky_with(extra: &str) -> FlakyRegistry {
        parse_flaky(&format!("{GOOD_FLAKE}{extra}")).expect("parse")
    }

    #[test]
    fn a_valid_flaky_registry_passes() {
        let fx = Fixture::new();
        let errors = check_flaky(&flaky_with(""), &fx.tree());
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn stale_names_and_policy_breaks_are_reported() {
        let fx = Fixture::new();
        let reg = flaky_with(
            r#"
[[flake]]
id = "Bad_Id"
tests = ["demo:gone:test", "nope:load:test", "demo:load", "demo:load:widget"]
test_fns = ["renamed_away"]
symptom = ""
cause = "x"
status = "fixed"
handling = "quarantine"
passes = ["lvp"]
owner = "x"
seen = ["2026-13-01"]

[[flake]]
id = "demo-load-budget"
tests = ["demo:load:test"]
symptom = "x"
cause = "x"
status = "open"
handling = "solo"
owner = "x"
"#,
        );
        let errors = check_flaky(&reg, &fx.tree()).join("\n");
        for expected in [
            "`Bad_Id`: the id must be lowercase",
            "`demo:gone:test`: package `demo` has no test target `gone`",
            "`nope:load:test`: package `nope` is not a workspace member",
            "`demo:load` is not `package:target:kind`",
            "target kind `widget` is not a cargo target kind",
            "test fn `renamed_away` is not defined",
            "`symptom` is empty",
            "`2026-13-01` is not a YYYY-MM-DD date",
            "never quarantines",
            "the id is used twice",
            "an open or mitigated flake needs `review_by`",
            "`solo` and `quarantine` need `passes`",
        ] {
            assert!(errors.contains(expected), "missing `{expected}` in:\n{errors}");
        }
    }

    #[test]
    fn only_the_manifest_table_of_the_kind_declares_a_target() {
        let fx = Fixture::new();
        let dir = fx.root.join("crates/demo");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n\n[[test]]\nname = \"custom\"\npath = \"checks/custom.rs\"\n",
        )
        .expect("write");
        let tree = fx.tree();
        assert_eq!(tree.target_problem("demo", "custom", "test"), None);
        // The package name is not a test target, and a test is not a bin.
        assert!(tree.target_problem("demo", "demo", "test").is_some());
        assert!(tree.target_problem("demo", "custom", "bin").is_some());
    }

    #[test]
    fn every_repeated_target_flag_is_checked() {
        let tokens = [
            "cargo", "build", "-p", "demo", "--bin", "a", "--bin", "b", "--test", "t",
        ];
        assert_eq!(flag_values(&tokens, &["--bin"]), ["a", "b"]);
        assert_eq!(flag_value(&tokens, &["-p", "--package"]), Some("demo"));
        assert!(flag_values(&tokens, &["--example"]).is_empty());
    }

    #[test]
    fn a_fixed_flake_may_not_stay_quarantined() {
        let fx = Fixture::new();
        let reg = parse_flaky(
            r#"
version = 1
[[flake]]
id = "done"
tests = ["demo:load:test"]
symptom = "x"
cause = "x"
status = "fixed"
handling = "quarantine"
passes = ["gpu"]
owner = "x"
"#,
        )
        .expect("parse");
        let errors = check_flaky(&reg, &fx.tree());
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("remove the quarantine"), "{}", errors[0]);
    }

    #[test]
    fn unknown_fields_and_values_do_not_parse() {
        assert!(parse_flaky("version = 1\n[[flake]]\nid = \"x\"\nflaky = true\n").is_err());
        let bad_handling = GOOD_FLAKE.replace("handling = \"solo\"", "handling = \"retry\"");
        assert!(parse_flaky(&bad_handling).is_err());
    }

    #[test]
    fn gate_args_list_solo_and_quarantine_keys_per_pass() {
        let reg = flaky_with("");
        assert_eq!(gate_args(&reg, GatePass::Lvp), ["--solo", "demo:load:test"]);
        assert_eq!(
            gate_args(&reg, GatePass::Gpu),
            [
                "--quarantine",
                "demo:demo:bin=demo-gpu-exit: lane 05, review by 2026-10-25",
                "--quarantine",
                "demo:demo:lib=demo-gpu-exit: lane 05, review by 2026-10-25",
            ]
        );
    }

    #[test]
    fn warnings_name_overdue_reviews_and_open_flakes_seen_twice() {
        let reg = flaky_with("");
        assert!(flaky_warnings(&reg, "2026-10-21").is_empty());
        let late = flaky_warnings(&reg, "2026-10-26");
        assert_eq!(late.len(), 1, "{late:?}");
        assert!(late[0].contains("`demo-gpu-exit`: review was due 2026-10-25"));
        let twice = parse_flaky(&GOOD_FLAKE.replace(
            "seen = [\"2026-10-08\"]",
            "seen = [\"2026-10-08\", \"2026-10-20\"]",
        ))
        .expect("parse");
        let notes = flaky_warnings(&twice, "2026-10-21");
        assert!(
            notes.iter().any(|n| n.contains("seen 2 times: a P0 ticket")),
            "{notes:?}"
        );
    }

    #[test]
    fn dates_and_schedules_are_validated() {
        assert!(is_iso_date("2026-10-21"));
        for bad in [
            "2026-1-21",
            "2026-00-10",
            "2026-10-32",
            "26-10-21xx",
            "2026/10/21",
        ] {
            assert!(!is_iso_date(bad), "{bad}");
        }
        for good in ["daily 03:30", "weekly mon 06:00", "per-tag", "on-demand"] {
            assert_eq!(schedule_problem(good), None, "{good}");
        }
        for bad in [
            "nightly",
            "daily 3:30",
            "daily 24:00",
            "daily +1:00",
            "weekly monday 06:00",
            "weekly mon",
        ] {
            assert!(schedule_problem(bad).is_some(), "{bad}");
        }
    }

    const JOBS: &str = r#"
version = 1

[[job]]
id = "sweep"
what = "player sweep"
command = "cargo xtask player-sweep --replay-seconds 120"
host = "buildhost"
schedule = "daily 03:30"
status = "scheduled"
needs = ["gpu-exclusive", "do27-content"]
results = "reports/"
owner = "lead"

[[job]]
id = "perf-check"
what = "ignored perf check"
command = "cargo test -p demo --test load -- --ignored"
host = "any quiet box"
schedule = "weekly mon 06:00"
status = "manual"
results = "stdout"
owner = "lead"
test_fns = ["loads_fast"]
"#;

    fn subcommands() -> Vec<String> {
        vec!["player-sweep".into(), "soak".into()]
    }

    #[test]
    fn a_valid_nightly_registry_passes() {
        let fx = Fixture::new();
        let reg = parse_nightly(JOBS).expect("parse");
        let (errors, notes) = check_nightly(&reg, &fx.tree(), &subcommands());
        assert!(errors.is_empty(), "{errors:?}");
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_job_naming_a_missing_command_target_or_test_fails() {
        let fx = Fixture::new();
        let reg = parse_nightly(&format!(
            r#"{JOBS}
[[job]]
id = "gone-sub"
what = "x"
command = "cargo xtask nightly-sweep"
host = "x"
schedule = "on-demand"
status = "scheduled"
results = "x"
owner = "x"
needs = ["coffee"]

[[job]]
id = "gone-test"
what = "x"
command = "cargo test -p demo --test missing -- --ignored"
host = "x"
schedule = "on-demand"
status = "manual"
results = "x"
owner = "x"
test_fns = ["not_there"]

[[job]]
id = "no-package"
what = "x"
command = "cargo test --test load"
host = "x"
schedule = "on-demand"
status = "manual"
results = "x"
owner = "x"
"#
        ))
        .expect("parse");
        let (errors, _) = check_nightly(&reg, &fx.tree(), &subcommands());
        let errors = errors.join("\n");
        for expected in [
            "`cargo xtask nightly-sweep` is not an xtask sub-command",
            "a scheduled job needs a schedule other than `on-demand`",
            "need `coffee` is not one of",
            "package `demo` has no test target `missing`",
            "test fn `not_there` is not defined in `demo`",
            "must name its package with `-p`",
        ] {
            assert!(errors.contains(expected), "missing `{expected}` in:\n{errors}");
        }
    }

    #[test]
    fn a_planned_job_may_name_a_command_another_branch_adds() {
        let fx = Fixture::new();
        let planned = JOBS
            .replace("player-sweep --replay", "nightly-sweep --replay")
            .replacen("status = \"scheduled\"", "status = \"planned\"", 1);
        let reg = parse_nightly(&planned).expect("parse");
        let (errors, notes) = check_nightly(&reg, &fx.tree(), &subcommands());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("not in this tree yet"), "{}", notes[0]);
        // Once the command exists, the note asks for the status to be updated.
        let with_sub = [subcommands(), vec!["nightly-sweep".into()]].concat();
        let (_, notes) = check_nightly(&reg, &fx.tree(), &with_sub);
        assert!(notes[0].contains("set `status`"), "{notes:?}");
    }

    /// The workspace root of this checkout.
    fn workspace() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf()
    }

    #[test]
    fn the_committed_registry_reproduces_the_gate_runner_lists() {
        // `fgate-remote.sh` (2026-10-21) runs these binaries alone on the lavapipe pass and
        // quarantines nothing on the real-GPU pass. The registry is now where they are listed.
        let text = std::fs::read_to_string(workspace().join(FLAKY_PATH)).expect("read registry");
        let reg = parse_flaky(&text).expect("parse");
        assert_eq!(
            gate_args(&reg, GatePass::Lvp),
            [
                "--solo",
                "regia_scene:formats:test",
                // ADR-5908: its landing-frame bar is wall-clock.
                "--solo",
                "regia_render:emissive_hitch:test",
                "--solo",
                "third_person:agent_expect:test",
                "--solo",
                "regia_render:desktop_high_timings:test",
                "--solo",
                "regia_editor:regia_editor:lib",
                // ADR-8599: its time follows host load.
                "--solo",
                "arena:repro_check:test",
            ]
        );
        // ADR-8599: the render lib binary fills a 5090's BAR1, so it has the card to itself. The
        // registry's `emissive-hitch-wall-clock`, `spawn-hitch-wall-clock` and `dispatch-fuzz-cpu-time`
        // entries (v06-pregate-reds-d/-f, 2026-11-18) run those binaries alone on the GPU pass too.
        assert_eq!(
            gate_args(&reg, GatePass::Gpu),
            [
                "--solo",
                "regia_render:emissive_hitch:test",
                "--solo",
                "regia_render:regia_render:lib",
                "--solo",
                "arena:repro_check:test",
                "--solo",
                "arena:spawn_hitch:test",
                "--solo",
                "regia_agent:dispatch_fuzz:test",
            ]
        );
    }

    #[test]
    fn the_committed_nightly_registry_parses() {
        let text = std::fs::read_to_string(workspace().join(NIGHTLY_PATH)).expect("read registry");
        let reg = parse_nightly(&text).expect("parse");
        assert!(reg.jobs.iter().any(|j| j.id == "player-sweep"), "{reg:?}");
    }
}
