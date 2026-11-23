// SPDX-License-Identifier: MPL-2.0
//! `cargo xtask det-golden`: the cross-target runners of the determinism goldens (ADR-8081, plan
//! row F-03; the harness is ADR-8046).
//!
//! The lockstep policy (ADR-8012 A-4) is bit identity across architectures. The golden harness
//! (`regia_detsim::golden`) proves it one target at a time; this command runs the same test binary
//! (`regia_detsim --test det_golden`) on several targets and compares what they printed. A *leg* is
//! one target:
//!
//! | Leg | Target | Binary | Where it runs |
//! |---|---|---|---|
//! | `host` | x86_64-linux | built here | here (the gate host) |
//! | `qemu` | aarch64 (static Android ELF) | cross-built here with cargo-ndk, `+crt-static` | here, under `qemu-aarch64` user mode |
//! | `android` | aarch64-android | cross-built here with cargo-ndk | the device, over `adb -s <serial>` |
//! | `mac` | aarch64-macos | built on the Mac from the staged crate | `ssh` to the Mac |
//! | `windows` | x86_64-windows | built on the rig from the staged crate | `ssh` to the Windows rig (cmd) |
//! | `deck` | x86_64-linux (SteamOS) | the `host` binary | `ssh` to the Deck |
//!
//! Three steps, each its own sub-command so a host that may not build can still run a leg:
//!
//! 1. `stage` builds what the chosen legs need into `--out` (default `target/det-golden/stage`):
//!    `goldens/`, `bin/<kind>/det_golden`, `bundle/` (the crate as a standalone package for the
//!    native Mac and Windows builds) and `scripts/<leg>.sh`, one POSIX shell script per leg that
//!    needs only `sh`, `tar`, `ssh`, `adb`, `flock` or `qemu-aarch64`. A host that may not build
//!    runs the script by hand: `sh <out>/scripts/android.sh > <out>/logs/android.log 2>&1`.
//! 2. `run` stages, then runs each leg's script here and writes `logs/<leg>.log`.
//! 3. `verify <out>` reads `logs/*.log` and writes `summary.json` and `summary.md`: one row per
//!    scenario, one column per leg. It fails ([`Verdict::ok`]) unless every clean scenario passed
//!    and hashes equal on every leg, the planted scenario `planted_simd_vec3a` (glam SIMD types, see
//!    the test's `planted` module) is reported as differing with a first diverging tick on every
//!    aarch64 leg and as equal on every x86-64 leg, every `--require`d leg is there, and at least one
//!    aarch64 and one x86-64 leg ran.
//!
//! `gate` is `run --legs host,qemu`: the merge gate's aarch64 leg. Problem codes ([`Problem::code`])
//! are lowercase protocol codes, listed on the book page `agents/det-golden-runners.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// Prefix of a golden report line (`regia_detsim::golden::REPORT_PREFIX`).
pub const REPORT_PREFIX: &str = "regia-det-golden ";
/// Prefix of the glam operation probe line (`det_golden.rs`, `PROBE_PREFIX`).
pub const PROBE_PREFIX: &str = "regia-det-probe ";
/// Prefix of a leg script's own error line: `regia-det-leg-error <code>: <detail>`.
pub const LEG_ERROR_PREFIX: &str = "regia-det-leg-error ";
/// Prefix of an owed leg's line: `regia-det-leg-owed <code>: <detail>`. `run` and `gate` write it
/// for the qemu leg under `--qemu-when-present` when `qemu-aarch64` is missing (ADR-8838): the leg
/// is listed as owed in the summary instead of failing the run.
pub const OWED_PREFIX: &str = "regia-det-leg-owed ";
/// Prefix of a remote leg's exit line: `regia-det-exit=<code>`.
pub const EXIT_PREFIX: &str = "regia-det-exit=";
/// The engine's clean scenarios: they must hash equal on every leg. Every committed golden in
/// `crates/regia_detsim/goldens` except the plant is listed (ADR-8838): the `det_golden` binary
/// prints a report line for each, and a leg that does not report one fails with
/// `scenario_missing`. The order is the registry's (`engine_scenarios`), then the two goldens
/// checked by their own tests.
pub const CLEAN_SCENARIOS: [&str; 15] = [
    "steer_slalom",
    "slope_follow",
    "disc_collide",
    "hover_libm_1000",
    "hover_bad_input",
    "math_rng_fixed_vectors",
    "hover_mixed",
    "lowg_ballistic_mixed",
    "hover_fleet_terrain",
    "projectile_mix",
    "input_log_arena",
    "contact_pile",
    "ground_vehicle_mix",
    "spatial_queries",
    "match_rules_ctf_koth_3600",
];
/// The planted cross-architecture divergence (ADR-8081).
pub const PLANTED_SCENARIO: &str = "planted_simd_vec3a";
/// Package and test target every leg runs.
pub const PACKAGE: &str = "regia_detsim";
/// The test target.
pub const TEST_TARGET: &str = "det_golden";

/// One target the goldens run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Leg {
    /// This host (x86_64-linux on the gate).
    Host,
    /// aarch64 under `qemu-aarch64` user mode on this host.
    Qemu,
    /// The Android device over adb.
    Android,
    /// The Mac over ssh.
    Mac,
    /// The Windows rig over ssh.
    Windows,
    /// The Steam Deck over ssh.
    Deck,
}

/// Every leg, in report order.
pub const ALL_LEGS: [Leg; 6] = [
    Leg::Host,
    Leg::Qemu,
    Leg::Android,
    Leg::Mac,
    Leg::Windows,
    Leg::Deck,
];

impl Leg {
    /// The leg's name on the command line and in file names.
    pub const fn name(self) -> &'static str {
        match self {
            Leg::Host => "host",
            Leg::Qemu => "qemu",
            Leg::Android => "android",
            Leg::Mac => "mac",
            Leg::Windows => "windows",
            Leg::Deck => "deck",
        }
    }

    /// Parse a leg name.
    pub fn parse(name: &str) -> Option<Leg> {
        ALL_LEGS.into_iter().find(|l| l.name() == name)
    }

    /// The CPU architecture the leg's reports must name (`<arch>-<os>` prefix).
    pub const fn arch(self) -> &'static str {
        match self {
            Leg::Qemu | Leg::Android | Leg::Mac => "aarch64",
            Leg::Host | Leg::Windows | Leg::Deck => "x86_64",
        }
    }

    /// What `stage` builds for the leg.
    pub const fn payload(self) -> Payload {
        match self {
            Leg::Host | Leg::Deck => Payload::HostBin,
            Leg::Qemu => Payload::StaticAarch64,
            Leg::Android => Payload::AndroidBin,
            Leg::Mac | Leg::Windows => Payload::Bundle,
        }
    }
}

/// A staged artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Payload {
    /// `bin/host/det_golden`: the test binary for this host.
    HostBin,
    /// `bin/aarch64-static/det_golden`: a static aarch64 Android ELF, runnable by qemu-user.
    StaticAarch64,
    /// `bin/aarch64-android/det_golden`: the device binary.
    AndroidBin,
    /// `bundle/`: the crate as a standalone package.
    Bundle,
}

impl Payload {
    /// Directory under `bin/` (binaries only).
    pub const fn bin_dir(self) -> &'static str {
        match self {
            Payload::HostBin => "host",
            Payload::StaticAarch64 => "aarch64-static",
            Payload::AndroidBin => "aarch64-android",
            Payload::Bundle => "",
        }
    }
}

/// Parse a comma-separated leg list (`host,qemu`); duplicates are dropped, order kept.
pub fn parse_legs(list: &str) -> Result<Vec<Leg>> {
    let mut legs = Vec::new();
    for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let Some(leg) = Leg::parse(name) else {
            let known: Vec<&str> = ALL_LEGS.iter().map(|l| l.name()).collect();
            bail!("unknown leg `{name}` (known: {})", known.join(", "));
        };
        if !legs.contains(&leg) {
            legs.push(leg);
        }
    }
    Ok(legs)
}

// ---------------------------------------------------------------------------------------------
// Logs and the verdict.
// ---------------------------------------------------------------------------------------------

/// One `regia-det-golden` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportLine {
    /// Scenario name.
    pub scenario: String,
    /// `<arch>-<os>` the binary ran as.
    pub target: String,
    /// Status code (`ok`, `unrecorded_target`, `diverged`, ...).
    pub status: String,
    /// Run hash (16 hex digits).
    pub hash: String,
    /// First tick that differs from the golden's trace.
    pub first_diverging_tick: Option<u64>,
    /// Whether the run hash equals the reference target's.
    pub agrees_with_reference: Option<bool>,
}

/// One `regia-det-probe` line: how many seeded inputs of each glam operation left the scalar order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLine {
    /// Target the probe ran as.
    pub target: String,
    /// Whether glam ran a SIMD backend.
    pub glam_simd: bool,
    /// Operation name to count, in the line's order.
    pub counts: Vec<(String, u64)>,
}

/// What one leg's log says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegLog {
    /// The leg.
    pub leg: Leg,
    /// Report lines, in log order (a scenario reported twice keeps its last line).
    pub reports: Vec<ReportLine>,
    /// The probe line, if any.
    pub probe: Option<ProbeLine>,
    /// `test result: ok.` lines seen.
    pub results_ok: u32,
    /// `test result: FAILED.` lines seen.
    pub results_failed: u32,
    /// The remote exit code, if the script printed one.
    pub exit: Option<i64>,
    /// `regia-det-leg-error` lines: (code, detail).
    pub errors: Vec<(String, String)>,
    /// `regia-det-leg-owed` lines: (code, detail).
    pub owed: Vec<(String, String)>,
}

impl LegLog {
    /// The last report of `scenario`.
    pub fn report(&self, scenario: &str) -> Option<&ReportLine> {
        self.reports.iter().rev().find(|r| r.scenario == scenario)
    }

    /// The target the leg's reports name (the first report's).
    pub fn target(&self) -> Option<&str> {
        self.reports.first().map(|r| r.target.as_str())
    }
}

fn json_payload<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let at = line.find(prefix)?;
    Some(line[at + prefix.len()..].trim())
}

/// Parse a leg's log. Lines may carry other output before the prefix (libtest prints
/// `test name ... ` on the same line).
pub fn parse_log(leg: Leg, text: &str) -> LegLog {
    let mut log = LegLog {
        leg,
        reports: Vec::new(),
        probe: None,
        results_ok: 0,
        results_failed: 0,
        exit: None,
        errors: Vec::new(),
        owed: Vec::new(),
    };
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(payload) = json_payload(line, REPORT_PREFIX) {
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                log.reports.push(ReportLine {
                    scenario: s("scenario"),
                    target: s("target"),
                    status: s("status"),
                    hash: s("hash"),
                    first_diverging_tick: v.get("first_diverging_tick").and_then(Value::as_u64),
                    agrees_with_reference: v.get("agrees_with_reference").and_then(Value::as_bool),
                });
            }
        } else if let Some(payload) = json_payload(line, PROBE_PREFIX) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(payload) {
                let target = map.get("target").and_then(Value::as_str).unwrap_or_default();
                let glam_simd = map.get("glam_simd").and_then(Value::as_bool).unwrap_or(false);
                let counts = map
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "target" | "glam_simd" | "samples"))
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n)))
                    .collect();
                log.probe = Some(ProbeLine {
                    target: target.to_string(),
                    glam_simd,
                    counts,
                });
            }
        } else if let Some(rest) = json_payload(line, LEG_ERROR_PREFIX) {
            let (code, detail) = rest.split_once(':').unwrap_or((rest, ""));
            log.errors
                .push((code.trim().to_string(), detail.trim().to_string()));
        } else if let Some(rest) = json_payload(line, OWED_PREFIX) {
            let (code, detail) = rest.split_once(':').unwrap_or((rest, ""));
            log.owed
                .push((code.trim().to_string(), detail.trim().to_string()));
        } else if let Some(code) = json_payload(line, EXIT_PREFIX) {
            log.exit = code.parse().ok();
        } else if line.starts_with("test result: ok.") {
            log.results_ok += 1;
        } else if line.starts_with("test result: FAILED.") {
            log.results_failed += 1;
        }
    }
    log
}

/// One reason a run fails, with a stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The protocol code ([`Problem::code`]).
    pub code: &'static str,
    /// The leg it concerns, if one.
    pub leg: Option<Leg>,
    /// A sentence for a human.
    pub detail: String,
}

impl Problem {
    /// The stable, lowercase problem code.
    pub const fn code(&self) -> &'static str {
        self.code
    }
}

/// The cross-target result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Legs that reported, in [`ALL_LEGS`] order.
    pub legs: Vec<Leg>,
    /// Every problem found; empty means the run passed.
    pub problems: Vec<Problem>,
    /// Legs that did not run and are owed (code `leg_owed`, ADR-8838); they never make the run
    /// fail or pass on their own, and the summary lists them.
    pub owed: Vec<Problem>,
    /// `scenario -> leg -> cell text` for the summary table.
    pub cells: BTreeMap<String, BTreeMap<Leg, String>>,
}

impl Verdict {
    /// Whether the run passed.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// The summary's one-word verdict: `PASS`, `OWED` (passed, with a leg owed) or `FAIL`.
    pub fn status(&self) -> &'static str {
        match (self.ok(), self.owed.is_empty()) {
            (false, _) => "FAIL",
            (true, false) => "OWED",
            (true, true) => "PASS",
        }
    }
}

const PASSING: [&str; 2] = ["ok", "unrecorded_target"];

/// Compare the legs' logs (see the module docs for the rules). `require` lists legs that must have
/// reported.
pub fn verify(logs: &[LegLog], require: &[Leg]) -> Verdict {
    let mut problems = Vec::new();
    let mut owed = Vec::new();
    // A log with an owed line and nothing else is an owed leg, not an empty one.
    let is_owed = |l: &LegLog| !l.owed.is_empty() && l.reports.is_empty() && l.errors.is_empty();
    for log in logs.iter().filter(|l| is_owed(l)) {
        for (code, detail) in &log.owed {
            owed.push(Problem {
                code: "leg_owed",
                leg: Some(log.leg),
                detail: format!("{}: {code}: {detail}", log.leg.name()),
            });
        }
    }
    let owed_arch = |arch: &str| logs.iter().any(|l| is_owed(l) && l.leg.arch() == arch);
    let mut push = |code: &'static str, leg: Option<Leg>, detail: String| {
        problems.push(Problem { code, leg, detail });
    };
    let mut legs: Vec<Leg> = logs.iter().map(|l| l.leg).collect();
    legs.sort();
    legs.dedup();
    for leg in require {
        if !legs.contains(leg) {
            push(
                "missing_leg",
                Some(*leg),
                format!("no log for the required `{}` leg", leg.name()),
            );
        }
    }
    let reporting: Vec<&LegLog> = logs.iter().filter(|l| !l.reports.is_empty()).collect();
    if !reporting.iter().any(|l| l.leg.arch() == "aarch64") && owed_arch("aarch64") {
        owed.push(Problem {
            code: "leg_owed",
            leg: None,
            detail: "no aarch64 leg reported: cross-architecture identity stays owed until the owed aarch64 leg runs"
                .into(),
        });
    } else if !reporting.iter().any(|l| l.leg.arch() == "aarch64") {
        push(
            "no_aarch64_leg",
            None,
            "no aarch64 leg reported: cross-architecture identity is unproven".into(),
        );
    }
    if !reporting.iter().any(|l| l.leg.arch() == "x86_64") {
        push(
            "no_x86_64_leg",
            None,
            "no x86-64 leg reported: there is nothing to compare the aarch64 bits with".into(),
        );
    }
    for log in logs {
        let leg = Some(log.leg);
        for (code, detail) in &log.errors {
            push("leg_error", leg, format!("{}: {code}: {detail}", log.leg.name()));
        }
        if log.reports.is_empty() {
            if log.errors.is_empty() && log.owed.is_empty() {
                push(
                    "no_reports",
                    leg,
                    format!(
                        "{}: no `{}` line in the log",
                        log.leg.name(),
                        REPORT_PREFIX.trim()
                    ),
                );
            }
            continue;
        }
        if let Some(bad) = log
            .reports
            .iter()
            .find(|r| !r.target.starts_with(&format!("{}-", log.leg.arch())))
        {
            push(
                "wrong_arch",
                leg,
                format!(
                    "{}: reports target `{}`, the leg is {}",
                    log.leg.name(),
                    bad.target,
                    log.leg.arch()
                ),
            );
        }
        if log.results_failed > 0 || log.exit.is_some_and(|c| c != 0) {
            push(
                "tests_failed",
                leg,
                format!(
                    "{}: the test binary failed ({} failed result line(s), exit {})",
                    log.leg.name(),
                    log.results_failed,
                    log.exit.map_or_else(|| "?".to_string(), |c| c.to_string())
                ),
            );
        }
    }

    let mut cells: BTreeMap<String, BTreeMap<Leg, String>> = BTreeMap::new();
    // The engine's scenarios must be on every leg; any other (a registered game scenario) is held
    // to the same rules wherever it is reported.
    let mut extra: Vec<&str> = reporting
        .iter()
        .flat_map(|l| l.reports.iter().map(|r| r.scenario.as_str()))
        .filter(|s| *s != PLANTED_SCENARIO && !CLEAN_SCENARIOS.contains(s))
        .collect();
    extra.sort_unstable();
    extra.dedup();
    for scenario in CLEAN_SCENARIOS.iter().copied().chain(extra) {
        let required = CLEAN_SCENARIOS.contains(&scenario);
        let mut hashes: BTreeMap<&str, Vec<Leg>> = BTreeMap::new();
        for log in &reporting {
            let leg = Some(log.leg);
            let Some(r) = log.report(scenario) else {
                if required {
                    push(
                        "scenario_missing",
                        leg,
                        format!("{}: no report for `{scenario}`", log.leg.name()),
                    );
                }
                continue;
            };
            cells
                .entry(scenario.to_string())
                .or_default()
                .insert(log.leg, format!("{} {}", short(&r.hash), r.status));
            if !PASSING.contains(&r.status.as_str()) {
                let tick = r
                    .first_diverging_tick
                    .map_or_else(String::new, |t| format!(", first diverging tick {t}"));
                push(
                    "scenario_failed",
                    leg,
                    format!("{}: `{scenario}` is {}{tick}", log.leg.name(), r.status),
                );
            }
            hashes.entry(r.hash.as_str()).or_default().push(log.leg);
        }
        if hashes.len() > 1 {
            let groups: Vec<String> = hashes
                .iter()
                .map(|(h, legs)| {
                    let names: Vec<&str> = legs.iter().map(|l| l.name()).collect();
                    format!("{} on {}", short(h), names.join("+"))
                })
                .collect();
            push(
                "clean_differs",
                None,
                format!("`{scenario}` hashes differ across legs: {}", groups.join("; ")),
            );
        }
    }

    for log in &reporting {
        let leg = Some(log.leg);
        let Some(r) = log.report(PLANTED_SCENARIO) else {
            push(
                "scenario_missing",
                leg,
                format!("{}: no report for `{PLANTED_SCENARIO}`", log.leg.name()),
            );
            continue;
        };
        let differs = r.status == "diverged" && r.agrees_with_reference == Some(false);
        let cell = match (differs, r.first_diverging_tick) {
            (true, Some(t)) => format!("{} differs @ tick {t}", short(&r.hash)),
            _ => format!("{} {}", short(&r.hash), r.status),
        };
        cells
            .entry(PLANTED_SCENARIO.to_string())
            .or_default()
            .insert(log.leg, cell);
        let simd = log.probe.as_ref().is_none_or(|p| p.glam_simd);
        if log.leg.arch() == "aarch64" {
            if !differs || r.first_diverging_tick.is_none() {
                let hint = if simd {
                    ""
                } else {
                    " (the probe says glam ran its scalar path)"
                };
                push(
                    "plant_not_caught",
                    leg,
                    format!(
                        "{}: `{PLANTED_SCENARIO}` is {} on aarch64; it must be reported as differing with a first diverging tick{hint}",
                        log.leg.name(),
                        r.status
                    ),
                );
            }
        } else if !PASSING.contains(&r.status.as_str()) || r.agrees_with_reference != Some(true) {
            push(
                "plant_moved_on_x86_64",
                leg,
                format!(
                    "{}: `{PLANTED_SCENARIO}` is {} on x86-64; SSE2 must keep the reference bits",
                    log.leg.name(),
                    r.status
                ),
            );
        }
    }

    Verdict {
        legs,
        problems,
        owed,
        cells,
    }
}

fn short(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

/// The verdict as the `summary.md` table.
pub fn summary_markdown(verdict: &Verdict, logs: &[LegLog]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Determinism goldens across targets: {}\n", verdict.status());
    let _ = write!(s, "| Scenario |");
    for leg in &verdict.legs {
        let target = logs
            .iter()
            .find(|l| l.leg == *leg)
            .and_then(LegLog::target)
            .unwrap_or("?");
        let _ = write!(s, " {} ({target}) |", leg.name());
    }
    let _ = write!(s, "\n|---|");
    for _ in &verdict.legs {
        let _ = write!(s, "---|");
    }
    s.push('\n');
    let extra = verdict
        .cells
        .keys()
        .map(String::as_str)
        .filter(|s| *s != PLANTED_SCENARIO && !CLEAN_SCENARIOS.contains(s));
    let order = CLEAN_SCENARIOS
        .iter()
        .copied()
        .chain(extra)
        .chain([PLANTED_SCENARIO]);
    for scenario in order {
        let _ = write!(s, "| {scenario} |");
        for leg in &verdict.legs {
            let cell = verdict
                .cells
                .get(scenario)
                .and_then(|c| c.get(leg))
                .map_or("-", String::as_str);
            let _ = write!(s, " {cell} |");
        }
        s.push('\n');
    }
    let probes: Vec<&LegLog> = logs.iter().filter(|l| l.probe.is_some()).collect();
    if !probes.is_empty() {
        let _ = writeln!(
            s,
            "\nglam operations that left the scalar order (of the probe's samples):\n"
        );
        for log in probes {
            if let Some(p) = &log.probe {
                let counts: Vec<String> = p.counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
                let _ = writeln!(
                    s,
                    "- {} ({}, glam {}): {}",
                    log.leg.name(),
                    p.target,
                    if p.glam_simd { "SIMD" } else { "scalar" },
                    counts.join(", ")
                );
            }
        }
    }
    if !verdict.owed.is_empty() {
        let _ = writeln!(s, "\nOwed (not run; not a pass):\n");
        for p in &verdict.owed {
            let _ = writeln!(s, "- `{}`: {}", p.code, p.detail);
        }
    }
    if verdict.problems.is_empty() {
        let _ = writeln!(s, "\nNo problems.");
    } else {
        let _ = writeln!(s, "\nProblems:\n");
        for p in &verdict.problems {
            let _ = writeln!(s, "- `{}`: {}", p.code, p.detail);
        }
    }
    s
}

/// The verdict as `summary.json`.
pub fn summary_json(verdict: &Verdict, logs: &[LegLog]) -> Value {
    let legs: Vec<Value> = logs
        .iter()
        .map(|l| {
            json!({
                "leg": l.leg.name(),
                "target": l.target(),
                "reports": l.reports.iter().map(|r| json!({
                    "scenario": r.scenario,
                    "status": r.status,
                    "hash": r.hash,
                    "first_diverging_tick": r.first_diverging_tick,
                    "agrees_with_reference": r.agrees_with_reference,
                })).collect::<Vec<_>>(),
                "probe": l.probe.as_ref().map(|p| json!({
                    "target": p.target,
                    "glam_simd": p.glam_simd,
                    "counts": p.counts.iter().map(|(k, n)| (k.clone(), json!(n))).collect::<serde_json::Map<_, _>>(),
                })),
                "results_ok": l.results_ok,
                "results_failed": l.results_failed,
                "exit": l.exit,
            })
        })
        .collect();
    json!({
        "schema": 1,
        "ok": verdict.ok(),
        "status": verdict.status(),
        "legs": legs,
        "owed": verdict.owed.iter().map(|p| json!({
            "code": p.code,
            "leg": p.leg.map(Leg::name),
            "detail": p.detail,
        })).collect::<Vec<_>>(),
        "problems": verdict.problems.iter().map(|p| json!({
            "code": p.code,
            "leg": p.leg.map(Leg::name),
            "detail": p.detail,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------------------------
// Staging.
// ---------------------------------------------------------------------------------------------

/// Where the remote legs run; every field has a default and an environment override in the
/// scripts (`REGIA_ADB_SERIAL`, `REGIA_MAC_HOST`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosts {
    /// adb serial of the Android device.
    pub adb_serial: String,
    /// ssh destination of the Mac.
    pub mac: String,
    /// ssh destination of the Windows rig (its shell is cmd).
    pub windows: String,
    /// ssh destination of the Deck.
    pub deck: String,
}

impl Default for Hosts {
    fn default() -> Self {
        Hosts {
            adb_serial: String::new(),
            mac: "mac-host".into(),
            windows: "windows-host".into(),
            deck: "steamdeck".into(),
        }
    }
}

/// Single-quote `s` for POSIX sh.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

const SCRIPT_HEAD: &str = "#!/bin/sh\n# Generated by `cargo xtask det-golden stage` (ADR-8081). Run from anywhere; prints the test\n# binary's output, ending with its exit code.\nset -u\nSTAGE=$(cd \"$(dirname \"$0\")/..\" && pwd)\nARGS='--nocapture --test-threads=1'\n";

/// The shell script of `leg` (see the module docs). Pure, so the commands are unit tested.
pub fn leg_script(leg: Leg, hosts: &Hosts) -> String {
    let mut s = String::from(SCRIPT_HEAD);
    let bin = |p: Payload| format!("\"$STAGE/bin/{}/{TEST_TARGET}\"", p.bin_dir());
    match leg {
        Leg::Host => {
            let _ = write!(
                s,
                "mkdir -p \"$STAGE/tmp/host\"\n\
                 REGIA_DET_GOLDEN_DIR=\"$STAGE/goldens\" TMPDIR=\"$STAGE/tmp/host\" {} $ARGS\n\
                 rc=$?\necho \"{EXIT_PREFIX}$rc\"\nexit $rc\n",
                bin(Payload::HostBin)
            );
        }
        Leg::Qemu => {
            let _ = write!(
                s,
                "QEMU=${{QEMU_AARCH64:-qemu-aarch64}}\n\
                 if ! command -v \"$QEMU\" >/dev/null 2>&1; then\n\
                 \x20 echo \"{LEG_ERROR_PREFIX}qemu_missing: $QEMU is not on PATH (install qemu-user)\"\n\
                 \x20 exit 3\n\
                 fi\n\
                 mkdir -p \"$STAGE/tmp/qemu\"\n\
                 REGIA_DET_GOLDEN_DIR=\"$STAGE/goldens\" TMPDIR=\"$STAGE/tmp/qemu\" \"$QEMU\" {} $ARGS\n\
                 rc=$?\necho \"{EXIT_PREFIX}$rc\"\nexit $rc\n",
                bin(Payload::StaticAarch64)
            );
        }
        Leg::Android => {
            let _ = write!(
                s,
                "SERIAL=${{REGIA_ADB_SERIAL:-{serial}}}\n\
                 if [ -z \"$SERIAL\" ]; then echo \"{LEG_ERROR_PREFIX}adb_serial_missing: pass --adb <serial> or set REGIA_ADB_SERIAL\"; exit 3; fi\n\
                 if ! adb -s \"$SERIAL\" get-state >/dev/null 2>&1; then echo \"{LEG_ERROR_PREFIX}adb_device_missing: $SERIAL is not attached\"; exit 3; fi\n\
                 D=/data/local/tmp/regia-det-golden\n\
                 # One user of the device at a time (the farm's per-device lock; REGIA_ADB_LOCK= turns it off).\n\
                 LOCK=${{REGIA_ADB_LOCK-/tmp/ferrite-adb-$SERIAL.lock}}\n\
                 if [ -n \"$LOCK\" ] && command -v flock >/dev/null 2>&1; then exec 9>\"$LOCK\"; flock 9; fi\n\
                 adb -s \"$SERIAL\" shell rm -rf $D\n\
                 adb -s \"$SERIAL\" shell mkdir -p $D/tmp\n\
                 adb -s \"$SERIAL\" push {} $D/{TEST_TARGET} >/dev/null\n\
                 adb -s \"$SERIAL\" push \"$STAGE/goldens\" $D/ >/dev/null\n\
                 adb -s \"$SERIAL\" shell \"cd $D && chmod 755 {TEST_TARGET} && REGIA_DET_GOLDEN_DIR=$D/goldens TMPDIR=$D/tmp ./{TEST_TARGET} $ARGS; echo {EXIT_PREFIX}\\$?\"\n\
                 adb -s \"$SERIAL\" shell rm -rf $D\n",
                bin(Payload::AndroidBin),
                serial = hosts.adb_serial,
            );
        }
        Leg::Mac => {
            let _ = write!(
                s,
                "HOST=${{REGIA_MAC_HOST:-{host}}}\n\
                 D=${{REGIA_MAC_DIR:-regia-pass/det-golden}}\n\
                 tar -C \"$STAGE/bundle\" -cf - . | ssh -o BatchMode=yes \"$HOST\" \"rm -rf $D && mkdir -p $D && tar -xf - -C $D\" || {{ echo \"{LEG_ERROR_PREFIX}ssh_failed: cannot copy the crate to $HOST\"; exit 3; }}\n\
                 ssh -o BatchMode=yes \"$HOST\" \"cd $D && export PATH=\\$HOME/.cargo/bin:/opt/homebrew/bin:\\$PATH CARGO_BUILD_JOBS=4 && nice -n 19 cargo test --test {TEST_TARGET} -- $ARGS; echo {EXIT_PREFIX}\\$?; cd && rm -rf $D\"\n",
                host = hosts.mac,
            );
        }
        Leg::Windows => {
            let _ = write!(
                s,
                "HOST=${{REGIA_WINDOWS_HOST:-{host}}}\n\
                 W='C:\\regia-work\\regia-pass\\det-golden'\n\
                 # A shared rig: a reservation file names another user (REGIA_WINDOWS_RESERVE= turns it off).\n\
                 R=${{REGIA_WINDOWS_RESERVE-C:\\\\work\\\\RIG-IN-USE.txt}}\n\
                 if [ -n \"$R\" ]; then\n\
                 \x20 held=$(ssh -o BatchMode=yes \"$HOST\" \"if exist $R type $R\" 2>/dev/null | tr -d '\\r')\n\
                 \x20 if [ -n \"$held\" ]; then echo \"{LEG_ERROR_PREFIX}rig_busy: $held\"; exit 3; fi\n\
                 \x20 ssh -o BatchMode=yes \"$HOST\" \"echo regia det-golden runner, started %DATE% %TIME%, about 15 minutes> $R\"\n\
                 fi\n\
                 ssh -o BatchMode=yes \"$HOST\" \"if exist $W rmdir /s /q $W\" ; ssh -o BatchMode=yes \"$HOST\" \"mkdir $W\"\n\
                 tar -C \"$STAGE/bundle\" -cf - . | ssh -o BatchMode=yes \"$HOST\" \"tar -xf - -C $W\" || echo \"{LEG_ERROR_PREFIX}ssh_failed: cannot copy the crate to $HOST\"\n\
                 ssh -o BatchMode=yes \"$HOST\" \"cd /d $W && set CARGO_BUILD_JOBS=4&& cargo test --test {TEST_TARGET} -- $ARGS & call echo {EXIT_PREFIX}%^ERRORLEVEL%\"\n\
                 ssh -o BatchMode=yes \"$HOST\" \"cd /d C:\\regia-work && rmdir /s /q $W\"\n\
                 if [ -n \"$R\" ]; then ssh -o BatchMode=yes \"$HOST\" \"del $R\"; fi\n",
                host = hosts.windows,
            );
        }
        Leg::Deck => {
            let _ = write!(
                s,
                "HOST=${{REGIA_DECK_HOST:-{host}}}\n\
                 D=${{REGIA_DECK_DIR:-regia-test/det-golden}}\n\
                 # One user of the Deck at a time (REGIA_DECK_LOCK= turns it off).\n\
                 LOCK=${{REGIA_DECK_LOCK-/tmp/regia-deck.lock}}\n\
                 if [ -n \"$LOCK\" ] && command -v flock >/dev/null 2>&1; then exec 9>\"$LOCK\"; flock 9; fi\n\
                 tar -C \"$STAGE\" -cf - bin/{hostbin}/{TEST_TARGET} goldens | ssh -o BatchMode=yes \"$HOST\" \"rm -rf $D && mkdir -p $D/tmp && tar -xf - -C $D\" || {{ echo \"{LEG_ERROR_PREFIX}ssh_failed: cannot copy the binary to $HOST\"; exit 3; }}\n\
                 ssh -o BatchMode=yes \"$HOST\" \"cd $D && REGIA_DET_GOLDEN_DIR=\\$PWD/goldens TMPDIR=\\$PWD/tmp ./bin/{hostbin}/{TEST_TARGET} $ARGS; echo {EXIT_PREFIX}\\$?; cd && rm -rf $D\"\n",
                host = hosts.deck,
                hostbin = Payload::HostBin.bin_dir(),
            );
        }
    }
    s
}

/// The standalone `Cargo.toml` of the staged crate: the crate's own dependencies with the exact
/// versions in `Cargo.lock` and the workspace's dev profile, so a Mac or Windows host builds the
/// same sources without the workspace. Fails when the crate gains a dependency this does not
/// know, so the bundle never drifts silently.
pub fn bundle_manifest(root_manifest: &str, crate_manifest: &str, lock: &str) -> Result<String> {
    let root: toml::Value = toml::from_str(root_manifest).context("parse the workspace Cargo.toml")?;
    let krate: toml::Value = toml::from_str(crate_manifest).context("parse the crate Cargo.toml")?;
    let lock: toml::Value = toml::from_str(lock).context("parse Cargo.lock")?;
    let ws_pkg = root
        .get("workspace")
        .and_then(|w| w.get("package"))
        .context("no [workspace.package]")?;
    let ws_deps = root
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .context("no [workspace.dependencies]")?;
    let field = |k: &str| -> Result<String> {
        ws_pkg
            .get(k)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("no workspace.package.{k}"))
    };
    // The locked version that satisfies `req`: exact for `=x.y.z`, else the same major.minor.
    let locked = |name: &str, req: &str| -> Result<String> {
        let want = req.trim_start_matches(['=', '^', '~']);
        let exact = req.starts_with('=');
        let minor: String = want.split('.').take(2).collect::<Vec<_>>().join(".");
        lock.get("package")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.get("name").and_then(toml::Value::as_str) == Some(name))
            .filter_map(|p| p.get("version").and_then(toml::Value::as_str))
            .find(|v| {
                if exact {
                    *v == want
                } else {
                    v.starts_with(&format!("{minor}."))
                }
            })
            .map(str::to_string)
            .with_context(|| format!("Cargo.lock has no {name} {req}"))
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Generated by `cargo xtask det-golden stage` (ADR-8081): {PACKAGE} outside the workspace,\n\
         # with the workspace's locked dependency versions. Do not edit; restage."
    );
    let _ = writeln!(
        out,
        "[package]\nname = \"{PACKAGE}\"\nversion = \"{}\"\nedition = \"{}\"\npublish = false\n",
        field("version")?,
        field("edition")?
    );
    // One dependency table: `header` is the TOML table name, `section` the dependency kind.
    let emit = |out: &mut String,
                header: &str,
                section: &str,
                table: &toml::map::Map<String, toml::Value>|
     -> Result<()> {
        let _ = writeln!(out, "[{header}]");
        for (name, spec) in table {
            if name.as_str() == "regia_rules" {
                if spec.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
                    bail!("{PACKAGE} {section} `regia_rules` is not a workspace dependency");
                }
                let _ = writeln!(out, "regia_rules = {{ path = \"vendor/regia_rules\" }}");
                continue;
            }
            // libm (math) and glam (the planted SIMD test); serde, tracing and serde_json joined with the
            // desync hash (ADR-8047), the AI core (ADR-8152) and the sim harness (ADR-8151): all pure Rust.
            // libc and windows-sys: the tick profiler's thread clock, per target (ADR-8498). ron: the
            // utility catalogue's `.detutility.ron` (gap AI-7), pure Rust.
            if !matches!(
                name.as_str(),
                "libm" | "glam" | "serde" | "serde_json" | "tracing" | "libc" | "windows-sys" | "ron"
            ) {
                bail!(
                    "{PACKAGE} {section} gained `{name}`: teach det_golden::bundle_manifest about it (ADR-8081)"
                );
            }
            if spec.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
                bail!("{PACKAGE} {section} `{name}` is not a workspace dependency");
            }
            let ws = ws_deps
                .get(name.as_str())
                .with_context(|| format!("no workspace dependency `{name}`"))?;
            let mut dependency = match ws {
                toml::Value::String(req) => {
                    let mut table = toml::map::Map::new();
                    table.insert("version".into(), toml::Value::String(req.clone()));
                    table
                }
                toml::Value::Table(table) => table.clone(),
                _ => bail!("workspace `{name}` is not a dependency specification"),
            };
            let req = dependency
                .get("version")
                .and_then(toml::Value::as_str)
                .with_context(|| format!("workspace `{name}` has no version"))?;
            let version = locked(name, req)?;
            dependency.insert("version".into(), toml::Value::String(format!("={version}")));
            if let Some(extra) = spec.get("features").and_then(toml::Value::as_array) {
                let features = dependency
                    .entry("features".to_string())
                    .or_insert_with(|| toml::Value::Array(Vec::new()))
                    .as_array_mut()
                    .with_context(|| format!("workspace `{name}` features are not an array"))?;
                for feature in extra {
                    if !features.contains(feature) {
                        features.push(feature.clone());
                    }
                }
            }
            if let Some(optional) = spec.get("optional") {
                dependency.insert("optional".into(), optional.clone());
            }
            // Cargo permits a member to enable defaults, but a member cannot disable defaults
            // inherited from a workspace dependency that enables them.
            if spec.get("default-features").and_then(toml::Value::as_bool) == Some(true) {
                dependency.insert("default-features".into(), toml::Value::Boolean(true));
            }
            let _ = writeln!(out, "{name} = {}", toml::Value::Table(dependency));
        }
        out.push('\n');
        Ok(())
    };
    for section in ["dependencies", "dev-dependencies"] {
        let Some(table) = krate.get(section).and_then(toml::Value::as_table) else {
            continue;
        };
        emit(&mut out, section, section, table)?;
    }
    // Per-target tables (the profiler's libc / windows-sys): without them a staged bundle does not
    // build where the crate uses them.
    if let Some(targets) = krate.get("target").and_then(toml::Value::as_table) {
        for (cfg, target) in targets {
            for section in ["dependencies", "dev-dependencies"] {
                let Some(table) = target.get(section).and_then(toml::Value::as_table) else {
                    continue;
                };
                let header = format!("target.{}.{section}", toml::Value::String(cfg.clone()));
                emit(&mut out, &header, section, table)?;
            }
        }
    }
    // The crate's own features (the hover `serde` derives, ADR-8079) carry over as written.
    if let Some(features) = krate.get("features") {
        let _ = writeln!(out, "[features]");
        let features = features.as_table().context("crate features are not a table")?;
        for (name, value) in features {
            let _ = writeln!(out, "{name:?} = {value}");
        }
        out.push('\n');
    }
    let profile = root.get("profile").and_then(|p| p.get("dev"));
    if let Some(dev) = profile.and_then(toml::Value::as_table) {
        let _ = writeln!(out, "[profile.dev]");
        for (k, v) in dev {
            if k != "package" {
                let _ = writeln!(out, "{k} = {v}");
            }
        }
        if let Some(all) = dev
            .get("package")
            .and_then(|p| p.get("*"))
            .and_then(toml::Value::as_table)
        {
            let _ = writeln!(out, "\n[profile.dev.package.\"*\"]");
            for (k, v) in all {
                let _ = writeln!(out, "{k} = {v}");
            }
        }
        out.push('\n');
    }
    let _ = writeln!(out, "[workspace]");
    Ok(out)
}

/// Standalone manifest for the vendored `regia_rules` the Mac and Windows bundles compile.
///
/// Versions are the ones locked in `Cargo.lock`. `regia_detsim` points at the bundle root.
pub fn rules_vendor_manifest(lock: &str) -> Result<String> {
    for (name, version) in [
        ("serde", "1.0.229"),
        ("thiserror", "2.0.21"),
        ("tracing", "0.1.44"),
    ] {
        let needle = format!("name = \"{name}\"\nversion = \"{version}\"");
        if !lock.contains(&needle) {
            bail!("Cargo.lock has no {name} {version}");
        }
    }
    Ok(
        "# Generated by `cargo xtask det-golden stage` (ADR-8503). Do not edit; restage.\n\
         [package]\n\
         name = \"regia_rules\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [dependencies]\n\
         serde = { version = \"=1.0.229\", features = [\"derive\"] }\n\
         thiserror = \"=2.0.21\"\n\
         tracing = \"=0.1.44\"\n\
         regia_detsim = { path = \"../..\" }\n"
            .to_string(),
    )
}

fn stage_rules_vendor(root: &Path, bundle: &Path) -> Result<()> {
    let src = root.join("crates").join("regia_rules").join("src");
    let dest = bundle.join("vendor").join("regia_rules");
    copy_dir(&src, &dest.join("src"))?;
    let lock = fs::read_to_string(root.join("Cargo.lock"))?;
    fs::write(dest.join("Cargo.toml"), rules_vendor_manifest(&lock)?)
        .with_context(|| format!("write {}", dest.join("Cargo.toml").display()))?;
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&path, &dest)?;
        } else {
            fs::copy(&path, &dest).with_context(|| format!("copy {}", path.display()))?;
        }
    }
    Ok(())
}

/// The test executable from cargo's `--message-format=json` output.
pub fn test_executable(messages: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("reason").and_then(Value::as_str) == Some("compiler-artifact"))
        .filter(|v| v.pointer("/target/name").and_then(Value::as_str) == Some(TEST_TARGET))
        .filter(|v| v.pointer("/profile/test").and_then(Value::as_bool) == Some(true))
        .filter_map(|v| v.get("executable").and_then(Value::as_str).map(PathBuf::from))
        .next_back()
}

/// The cargo invocation that builds `payload` (program, arguments, extra environment).
pub fn build_command(payload: Payload, target_dir: &Path) -> (String, Vec<String>, Vec<(String, String)>) {
    let mut args: Vec<String> = Vec::new();
    let mut env = Vec::new();
    match payload {
        Payload::HostBin => {}
        Payload::AndroidBin | Payload::StaticAarch64 => {
            args.extend(["ndk", "-t", "arm64-v8a"].map(String::from));
        }
        Payload::Bundle => return (String::new(), Vec::new(), Vec::new()),
    }
    args.extend(
        [
            "test",
            "-p",
            PACKAGE,
            "--test",
            TEST_TARGET,
            "--no-run",
            "--message-format=json",
        ]
        .map(String::from),
    );
    if payload == Payload::StaticAarch64 {
        // A static executable needs no Android linker, so qemu-user runs it on any Linux host. Its
        // own target dir keeps the RUSTFLAGS change from rebuilding the device binary each time.
        env.push(("RUSTFLAGS".into(), "-C target-feature=+crt-static".into()));
        args.push("--target-dir".into());
        args.push(target_dir.join("det-golden-static").display().to_string());
    }
    ("cargo".into(), args, env)
}

fn build_payload(root: &Path, payload: Payload, out: &Path, target_dir: &Path) -> Result<()> {
    if payload == Payload::Bundle {
        let bundle = out.join("bundle");
        let _ = fs::remove_dir_all(&bundle);
        let krate = root.join("crates").join(PACKAGE);
        for dir in ["src", "tests", "goldens"] {
            copy_dir(&krate.join(dir), &bundle.join(dir))?;
        }
        // Keep the dependency-free shared temp helpers available in a standalone bundle.
        // The workspace test uses a sibling crate's source; the bundle has no sibling crates.
        let test_path = bundle.join("tests/det_golden.rs");
        let tests = fs::read_to_string(&test_path)?;
        let shared_path = "../../regia_test_utils/src/temp.rs";
        if !tests.contains(shared_path) {
            bail!("det_golden test helper path changed; update the standalone bundle");
        }
        fs::create_dir_all(bundle.join("tests/support"))?;
        fs::copy(
            root.join("crates/regia_test_utils/src/temp.rs"),
            bundle.join("tests/support/temp.rs"),
        )?;
        fs::write(test_path, tests.replace(shared_path, "support/temp.rs"))?;
        let manifest = bundle_manifest(
            &fs::read_to_string(root.join("Cargo.toml"))?,
            &fs::read_to_string(krate.join("Cargo.toml"))?,
            &fs::read_to_string(root.join("Cargo.lock"))?,
        )?;
        fs::write(bundle.join("Cargo.toml"), manifest)?;
        stage_rules_vendor(root, &bundle)?;
        return Ok(());
    }
    let (program, args, env) = build_command(payload, target_dir);
    eprintln!("det-golden: building {payload:?}: {program} {}", args.join(" "));
    let output = Command::new(&program)
        .args(&args)
        .envs(env)
        .current_dir(root)
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        let hint = if payload == Payload::StaticAarch64 {
            " (a static link needs an NDK whose libdl.a this lld reads: NDK r27 works, r28 does not; set ANDROID_NDK_HOME)"
        } else {
            ""
        };
        bail!("building {payload:?} failed{hint}");
    }
    // cargo-ndk 4 does not pass `--message-format` on: look in the target's deps directory.
    let deps = match payload {
        Payload::StaticAarch64 => target_dir.join("det-golden-static"),
        _ => target_dir.to_path_buf(),
    }
    .join(ANDROID_TRIPLE)
    .join("debug")
    .join("deps");
    let exe = test_executable(&String::from_utf8_lossy(&output.stdout))
        .or_else(|| newest_test_executable(&deps))
        .with_context(|| format!("cargo printed no {TEST_TARGET} test executable"))?;
    let dir = out.join("bin").join(payload.bin_dir());
    fs::create_dir_all(&dir)?;
    fs::copy(&exe, dir.join(TEST_TARGET)).with_context(|| format!("copy {}", exe.display()))?;
    Ok(())
}

/// The Rust target cargo-ndk builds for `arm64-v8a`.
pub const ANDROID_TRIPLE: &str = "aarch64-linux-android";

/// The newest `det_golden-<hash>` file without an extension in `deps` (a test executable; the
/// `.d` and `.o` files beside it have extensions).
pub fn newest_test_executable(deps: &Path) -> Option<PathBuf> {
    let prefix = format!("{TEST_TARGET}-");
    fs::read_dir(deps)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(&prefix) && !name.contains('.')
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

fn cargo_target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

/// Build what `legs` need into `out` and write their scripts.
pub fn stage(root: &Path, legs: &[Leg], out: &Path, hosts: &Hosts) -> Result<()> {
    fs::create_dir_all(out.join("scripts"))?;
    fs::create_dir_all(out.join("logs"))?;
    let goldens = out.join("goldens");
    let _ = fs::remove_dir_all(&goldens);
    copy_dir(&root.join("crates").join(PACKAGE).join("goldens"), &goldens)?;
    let mut payloads: Vec<Payload> = legs.iter().map(|l| l.payload()).collect();
    payloads.sort();
    payloads.dedup();
    let target_dir = cargo_target_dir(root);
    for payload in payloads {
        build_payload(root, payload, out, &target_dir)?;
    }
    for leg in legs {
        fs::write(
            out.join("scripts").join(format!("{}.sh", leg.name())),
            leg_script(*leg, hosts),
        )?;
    }
    Ok(())
}

/// Run each leg's staged script here, one at a time, writing `logs/<leg>.log`.
pub fn run_legs(legs: &[Leg], out: &Path, hosts: &Hosts) -> Result<()> {
    for leg in legs {
        let script = out.join("scripts").join(format!("{}.sh", leg.name()));
        let log_path = out.join("logs").join(format!("{}.log", leg.name()));
        eprintln!("det-golden: running the {} leg", leg.name());
        let output = Command::new("sh")
            .arg(&script)
            .env("REGIA_ADB_SERIAL", &hosts.adb_serial)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("run {}", script.display()))?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        if !text.contains(EXIT_PREFIX) {
            let _ = writeln!(text, "{EXIT_PREFIX}{}", output.status.code().unwrap_or(-1));
        }
        fs::write(&log_path, text).with_context(|| format!("write {}", log_path.display()))?;
    }
    Ok(())
}

/// Whether `qemu-aarch64` can run here: `$QEMU_AARCH64` names an existing file, or a
/// `qemu-aarch64` file is in a `$PATH` directory (the leg script's own lookup).
pub fn qemu_available() -> bool {
    if let Some(named) = std::env::var_os("QEMU_AARCH64").filter(|v| !v.is_empty()) {
        let named = PathBuf::from(named);
        if named.components().count() > 1 {
            return named.is_file();
        }
        return path_has(&named.to_string_lossy());
    }
    path_has("qemu-aarch64")
}

fn path_has(exe: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(exe).is_file()))
}

/// The text of an owed leg's log: one `regia-det-leg-owed` line.
pub fn owed_log(code: &str, detail: &str) -> String {
    format!("{OWED_PREFIX}{code}: {detail}\n")
}

/// `--qemu-when-present` (ADR-8838): with `qemu_here` false, the qemu leg is taken out of the
/// legs to stage and run, and its owed log text is returned; otherwise the legs are unchanged.
pub fn qemu_when_present(legs: &[Leg], qemu_here: bool) -> (Vec<Leg>, Option<String>) {
    if qemu_here || !legs.contains(&Leg::Qemu) {
        return (legs.to_vec(), None);
    }
    let run = legs.iter().copied().filter(|l| *l != Leg::Qemu).collect();
    let log = owed_log(
        "qemu_missing",
        "qemu-aarch64 is not on PATH (install qemu-user); the qemu leg is owed and runs when it is there",
    );
    (run, Some(log))
}

/// Stage and run `legs` into `out`, then verify with every leg required. With `when_present`, a
/// missing `qemu-aarch64` makes the qemu leg owed (see [`qemu_when_present`]).
fn stage_run_verify(
    root: &Path,
    legs: &[Leg],
    out: &Path,
    hosts: &Hosts,
    when_present: bool,
) -> Result<ExitCode> {
    let _ = fs::remove_dir_all(out.join("logs"));
    let (run, owed) = if when_present {
        qemu_when_present(legs, qemu_available())
    } else {
        (legs.to_vec(), None)
    };
    stage(root, &run, out, hosts)?;
    run_legs(&run, out, hosts)?;
    if let Some(text) = owed {
        eprintln!("det-golden: qemu-aarch64 is missing: the qemu leg is owed");
        fs::write(out.join("logs").join("qemu.log"), text)?;
    }
    let (verdict, md) = verify_dir(out, legs)?;
    Ok(finish(&verdict, &md))
}

/// Read `out/logs/*.log` (each named after its leg) and write `summary.json` and `summary.md`.
pub fn verify_dir(out: &Path, require: &[Leg]) -> Result<(Verdict, String)> {
    let mut logs = Vec::new();
    let logs_dir = out.join("logs");
    let entries = fs::read_dir(&logs_dir).with_context(|| format!("read {}", logs_dir.display()))?;
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Some(leg) = Leg::parse(stem) else {
            eprintln!("det-golden: ignoring {} (not a leg name)", path.display());
            continue;
        };
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        logs.push(parse_log(leg, &text));
    }
    logs.sort_by_key(|l| l.leg);
    let verdict = verify(&logs, require);
    let md = summary_markdown(&verdict, &logs);
    fs::write(out.join("summary.md"), &md)?;
    fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&summary_json(&verdict, &logs))?,
    )?;
    Ok((verdict, md))
}

/// Command-line options of `cargo xtask det-golden`.
#[derive(clap::Args, Clone, Debug)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: DetGoldenCmd,
}

/// Options shared by `stage`, `run` and `gate`.
#[derive(clap::Args, Clone, Debug)]
pub struct LegArgs {
    /// Legs, comma separated: host, qemu, android, mac, windows, deck.
    #[arg(long, default_value = "host,qemu")]
    pub legs: String,
    /// Stage directory (default `<target>/det-golden/stage`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// adb serial of the Android device; adds the `android` leg.
    #[arg(long)]
    pub adb: Option<String>,
    /// ssh destination of the Mac.
    #[arg(long, default_value = "mac-host")]
    pub mac_host: String,
    /// ssh destination of the Windows rig.
    #[arg(long, default_value = "windows-host")]
    pub windows_host: String,
    /// ssh destination of the Steam Deck.
    #[arg(long, default_value = "steamdeck")]
    pub deck_host: String,
    /// Run the qemu leg only when `qemu-aarch64` is here; otherwise record it as owed (summary
    /// `OWED`, exit 0 if nothing else failed) instead of failing with `qemu_missing`.
    #[arg(long)]
    pub qemu_when_present: bool,
}

/// `cargo xtask det-golden` sub-commands.
#[derive(clap::Subcommand, Clone, Debug)]
pub enum DetGoldenCmd {
    /// Build the legs' binaries and scripts into the stage directory.
    Stage(LegArgs),
    /// Stage, run every leg here, then verify.
    Run(LegArgs),
    /// The merge gate's leg: `run --legs host,qemu`.
    Gate {
        /// Stage directory (default `<target>/det-golden/gate`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Run the qemu leg only when `qemu-aarch64` is here; otherwise it is owed (ADR-8838).
        #[arg(long)]
        qemu_when_present: bool,
    },
    /// Compare the legs' logs in a stage directory and write `summary.json` / `summary.md`.
    Verify {
        /// The stage directory (with `logs/<leg>.log`).
        dir: PathBuf,
        /// Legs that must have reported, comma separated.
        #[arg(long, default_value = "")]
        require: String,
    },
}

impl LegArgs {
    fn legs(&self) -> Result<Vec<Leg>> {
        let mut legs = parse_legs(&self.legs)?;
        if self.adb.is_some() && !legs.contains(&Leg::Android) {
            legs.push(Leg::Android);
        }
        Ok(legs)
    }

    fn hosts(&self) -> Hosts {
        Hosts {
            adb_serial: self.adb.clone().unwrap_or_default(),
            mac: self.mac_host.clone(),
            windows: self.windows_host.clone(),
            deck: self.deck_host.clone(),
        }
    }
}

fn finish(verdict: &Verdict, md: &str) -> ExitCode {
    println!("{md}");
    if verdict.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Entry point of `cargo xtask det-golden`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let default_out = |name: &str| cargo_target_dir(root).join("det-golden").join(name);
    match &args.cmd {
        DetGoldenCmd::Stage(a) => {
            let out = a.out.clone().unwrap_or_else(|| default_out("stage"));
            stage(root, &a.legs()?, &out, &a.hosts())?;
            println!("staged {} in {}", a.legs, out.display());
            Ok(ExitCode::SUCCESS)
        }
        DetGoldenCmd::Run(a) => {
            let out = a.out.clone().unwrap_or_else(|| default_out("stage"));
            stage_run_verify(root, &a.legs()?, &out, &a.hosts(), a.qemu_when_present)
        }
        DetGoldenCmd::Gate {
            out,
            qemu_when_present,
        } => {
            let out = out.clone().unwrap_or_else(|| default_out("gate"));
            let legs = [Leg::Host, Leg::Qemu];
            stage_run_verify(root, &legs, &out, &Hosts::default(), *qemu_when_present)
        }
        DetGoldenCmd::Verify { dir, require } => {
            let (verdict, md) = verify_dir(dir, &parse_legs(require)?)?;
            Ok(finish(&verdict, &md))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        scenario: &str,
        target: &str,
        status: &str,
        hash: &str,
        tick: Option<u32>,
        agrees: bool,
    ) -> String {
        let tick = tick.map_or_else(|| "null".to_string(), |t| t.to_string());
        format!(
            "test x ... {REPORT_PREFIX}{{\"scenario\":\"{scenario}\",\"target\":\"{target}\",\"seed\":\"0x1\",\"ticks\":9,\
             \"rate_hz\":60,\"status\":\"{status}\",\"hash\":\"{hash}\",\"expected\":null,\"first_diverging_tick\":{tick},\
             \"expected_tick_hash\":null,\"actual_tick_hash\":null,\"agrees_with_reference\":{agrees},\"golden\":\"g\",\"detail\":\"d\"}}"
        )
    }

    /// The fixture run hash of the clean scenario at `index` in [`CLEAN_SCENARIOS`].
    fn fixture_hash(index: usize) -> String {
        const FIRST: [&str; 4] = [
            "536e1716468a80e3",
            "09e1674f2e6d6807",
            "c671fe6f58ca5048",
            "a4c0ffee00d4e001",
        ];
        FIRST.get(index).map_or_else(
            || format!("{:016x}", 0x0c1e_a400_0000_0000_u64 + index as u64),
            |h| (*h).to_string(),
        )
    }

    /// A clean log for `leg`: every engine scenario with a fixture hash, and the plant.
    fn leg_log(leg: Leg, target: &str) -> String {
        let mut s = String::new();
        for (index, scenario) in CLEAN_SCENARIOS.iter().enumerate() {
            let hash = fixture_hash(index);
            let hash = hash.as_str();
            let status = if target == "x86_64-linux" {
                "ok"
            } else {
                "unrecorded_target"
            };
            s.push_str(&line(scenario, target, status, hash, None, true));
            s.push('\n');
        }
        if leg.arch() == "aarch64" {
            s.push_str(&line(
                PLANTED_SCENARIO,
                target,
                "diverged",
                "5d95f9adf25bd5a6",
                Some(0),
                false,
            ));
        } else {
            s.push_str(&line(
                PLANTED_SCENARIO,
                target,
                "ok",
                "9c84eae2302f2e21",
                None,
                true,
            ));
        }
        s.push_str("\ntest result: ok. 16 passed; 0 failed\nregia-det-exit=0\n");
        s
    }

    fn codes(v: &Verdict) -> Vec<&'static str> {
        v.problems.iter().map(|p| p.code).collect()
    }

    #[test]
    fn the_base_has_no_aarch64_leg_and_fails() {
        // The base runs the goldens on x86-64 only: the gate's `--require host,qemu` fails.
        let logs = [parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"))];
        let v = verify(&logs, &[Leg::Host, Leg::Qemu]);
        assert_eq!(codes(&v), ["missing_leg", "no_aarch64_leg"], "{v:?}");
        assert!(!v.ok());
    }

    #[test]
    fn five_targets_agree_and_the_plant_is_caught_on_aarch64() {
        let logs: Vec<LegLog> = [
            (Leg::Host, "x86_64-linux"),
            (Leg::Qemu, "aarch64-android"),
            (Leg::Android, "aarch64-android"),
            (Leg::Mac, "aarch64-macos"),
            (Leg::Windows, "x86_64-windows"),
            (Leg::Deck, "x86_64-linux"),
        ]
        .iter()
        .map(|&(leg, target)| parse_log(leg, &leg_log(leg, target)))
        .collect();
        let v = verify(&logs, &ALL_LEGS);
        assert!(v.ok(), "{:?}", v.problems);
        let md = summary_markdown(&v, &logs);
        assert!(md.contains("5d95f9ad differs @ tick 0"), "{md}");
        assert!(
            md.starts_with("# Determinism goldens across targets: PASS"),
            "{md}"
        );
        let j = summary_json(&v, &logs);
        assert_eq!(j["ok"], true);
        assert_eq!(j["legs"].as_array().map(Vec::len), Some(6));
    }

    #[test]
    fn clean_reference_failure_codes_reach_the_runner_verdict() {
        for status in ["disagrees_with_reference", "bless_refused_clean_differs"] {
            let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
            let mut mac = parse_log(Leg::Mac, &leg_log(Leg::Mac, "aarch64-macos"));
            let report = mac
                .reports
                .iter_mut()
                .find(|r| r.scenario == CLEAN_SCENARIOS[0])
                .expect("clean report");
            report.status = status.into();
            report.agrees_with_reference = Some(false);
            report.first_diverging_tick = Some(7);
            let logs = [host, mac];
            let verdict = verify(&logs, &[Leg::Host, Leg::Mac]);
            assert!(!verdict.ok());
            assert!(
                verdict
                    .problems
                    .iter()
                    .any(|p| { p.code == "scenario_failed" && p.detail.contains(status) })
            );
            assert!(summary_markdown(&verdict, &logs).contains(status));
            assert_eq!(summary_json(&verdict, &logs)["ok"], false);
        }
    }

    #[test]
    fn the_standalone_bundle_keeps_the_shared_temp_helper() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let out = regia_test_utils::TempDir::new("det-golden-bundle-helper");
        build_payload(root, Payload::Bundle, out.path(), &out.join("target")).expect("bundle");
        let tests = fs::read_to_string(out.join("bundle/tests/det_golden.rs")).expect("tests");
        assert!(tests.contains("support/temp.rs"));
        assert!(!tests.contains("../../regia_test_utils/"));
        assert_eq!(
            fs::read(out.join("bundle/tests/support/temp.rs")).expect("bundled helper"),
            fs::read(root.join("crates/regia_test_utils/src/temp.rs")).expect("shared helper"),
        );
    }

    #[test]
    fn a_plant_equal_on_aarch64_is_not_caught() {
        let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
        // A "qemu" leg that really ran an x86-64 binary, and a Mac leg built with glam scalar-math.
        let qemu = parse_log(Leg::Qemu, &leg_log(Leg::Host, "x86_64-linux"));
        let mut scalar = String::new();
        for (index, scenario) in CLEAN_SCENARIOS.iter().enumerate() {
            scalar.push_str(&line(
                scenario,
                "aarch64-macos",
                "unrecorded_target",
                &fixture_hash(index),
                None,
                true,
            ));
            scalar.push('\n');
        }
        scalar.push_str(&line(
            PLANTED_SCENARIO,
            "aarch64-macos",
            "unrecorded_target",
            "9c84eae2302f2e21",
            None,
            true,
        ));
        scalar.push_str("\nregia-det-probe {\"target\":\"aarch64-macos\",\"glam_simd\":false,\"samples\":4,\"vec3a_cross\":0}\n");
        let mac = parse_log(Leg::Mac, &scalar);
        let v = verify(&[host, qemu, mac], &[Leg::Host]);
        let got = codes(&v);
        assert!(got.contains(&"wrong_arch"), "{got:?}");
        assert!(got.contains(&"plant_not_caught"), "{got:?}");
        let scalar_hint = v
            .problems
            .iter()
            .any(|p| p.code == "plant_not_caught" && p.detail.contains("scalar path"));
        assert!(scalar_hint, "{:?}", v.problems);
    }

    #[test]
    fn a_clean_scenario_that_differs_names_the_legs() {
        let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
        let mac_text = leg_log(Leg::Mac, "aarch64-macos").replace(
            &line(
                "slope_follow",
                "aarch64-macos",
                "unrecorded_target",
                "09e1674f2e6d6807",
                None,
                true,
            ),
            &line(
                "slope_follow",
                "aarch64-macos",
                "diverged",
                "ffffffff00000000",
                Some(311),
                false,
            ),
        );
        let mac = parse_log(Leg::Mac, &mac_text);
        let v = verify(&[host, mac], &[]);
        let got = codes(&v);
        assert_eq!(got, ["scenario_failed", "clean_differs"], "{:?}", v.problems);
        assert!(
            v.problems[0].detail.contains("first diverging tick 311"),
            "{:?}",
            v.problems
        );
        assert!(
            v.problems[1].detail.contains("09e1674f on host"),
            "{:?}",
            v.problems
        );
        assert!(
            v.problems[1].detail.contains("ffffffff on mac"),
            "{:?}",
            v.problems
        );
    }

    #[test]
    fn a_registered_scenario_is_held_to_the_clean_rules() {
        let mut host = leg_log(Leg::Host, "x86_64-linux");
        host.push_str(&line(
            "game_steer",
            "x86_64-linux",
            "ok",
            "1111111111111111",
            None,
            true,
        ));
        let mut mac = leg_log(Leg::Mac, "aarch64-macos");
        mac.push_str(&line(
            "game_steer",
            "aarch64-macos",
            "unrecorded_target",
            "2222222222222222",
            None,
            true,
        ));
        let logs = [parse_log(Leg::Host, &host), parse_log(Leg::Mac, &mac)];
        let v = verify(&logs, &[]);
        assert_eq!(codes(&v), ["clean_differs"], "{:?}", v.problems);
        assert!(v.problems[0].detail.contains("`game_steer`"), "{:?}", v.problems);
        // Reported on one leg only: compared with nothing, so it passes.
        let only_host = [
            parse_log(Leg::Host, &host),
            parse_log(Leg::Mac, &leg_log(Leg::Mac, "aarch64-macos")),
        ];
        let v = verify(&only_host, &[]);
        assert!(v.ok(), "{:?}", v.problems);
        assert!(summary_markdown(&v, &only_host).contains("| game_steer |"));
    }

    #[test]
    fn failed_tests_errors_and_empty_logs_have_codes() {
        let host = parse_log(
            Leg::Host,
            &leg_log(Leg::Host, "x86_64-linux").replace("test result: ok.", "test result: FAILED."),
        );
        let qemu = parse_log(
            Leg::Qemu,
            "regia-det-leg-error qemu_missing: qemu-aarch64 is not on PATH (install qemu-user)\nregia-det-exit=3\n",
        );
        let deck = parse_log(
            Leg::Deck,
            "ssh: connect to host steamdeck port 22: No route to host\n",
        );
        let v = verify(&[host, qemu, deck], &[]);
        let got = codes(&v);
        for code in ["tests_failed", "leg_error", "no_reports", "no_aarch64_leg"] {
            assert!(got.contains(&code), "{code} missing from {got:?}");
        }
        assert!(v.problems.iter().any(|p| p.detail.contains("qemu_missing")));
    }

    #[test]
    fn the_probe_line_is_parsed() {
        let log = parse_log(
            Leg::Mac,
            "test vec3a ... regia-det-probe {\"target\":\"aarch64-macos\",\"glam_simd\":true,\"samples\":4096,\"vec3a_dot\":0,\"vec3a_cross\":2460}\n",
        );
        let p = log.probe.expect("probe");
        assert!(p.glam_simd);
        // serde_json keeps object keys sorted.
        assert_eq!(
            p.counts,
            [("vec3a_cross".to_string(), 2460), ("vec3a_dot".to_string(), 0)]
        );
    }

    #[test]
    fn legs_parse_and_map_to_payloads() {
        assert_eq!(
            parse_legs("host, qemu,host").expect("legs"),
            [Leg::Host, Leg::Qemu]
        );
        assert!(parse_legs("host,ps5").is_err());
        assert_eq!(Leg::Deck.payload(), Payload::HostBin);
        assert_eq!(Leg::Qemu.payload(), Payload::StaticAarch64);
        assert_eq!(Leg::Windows.payload(), Payload::Bundle);
        for leg in ALL_LEGS {
            assert_eq!(Leg::parse(leg.name()), Some(leg));
        }
    }

    #[test]
    fn the_qemu_leg_runs_a_static_aarch64_binary_under_qemu_user() {
        let (program, args, env) = build_command(Payload::StaticAarch64, Path::new("/t"));
        assert_eq!(program, "cargo");
        assert_eq!(&args[..3], ["ndk", "-t", "arm64-v8a"]);
        assert!(args.windows(2).any(|w| w == ["--test", TEST_TARGET]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--target-dir", "/t/det-golden-static"])
        );
        assert_eq!(
            env,
            [(
                "RUSTFLAGS".to_string(),
                "-C target-feature=+crt-static".to_string()
            )]
        );
        let script = leg_script(Leg::Qemu, &Hosts::default());
        assert!(script.contains("QEMU=${QEMU_AARCH64:-qemu-aarch64}"), "{script}");
        assert!(
            script.contains("\"$QEMU\" \"$STAGE/bin/aarch64-static/det_golden\" $ARGS"),
            "{script}"
        );
        assert!(script.contains("regia-det-leg-error qemu_missing"), "{script}");
        assert!(
            script.contains("REGIA_DET_GOLDEN_DIR=\"$STAGE/goldens\""),
            "{script}"
        );
    }

    #[test]
    fn the_adb_leg_names_the_serial_everywhere_and_cleans_up() {
        let hosts = Hosts {
            adb_serial: "SERIAL1".into(),
            ..Hosts::default()
        };
        let script = leg_script(Leg::Android, &hosts);
        assert!(script.contains("SERIAL=${REGIA_ADB_SERIAL:-SERIAL1}"), "{script}");
        let adb_lines: Vec<&str> = script
            .lines()
            .filter(|l| l.starts_with("adb ") || l.contains(" adb "))
            .collect();
        assert!(adb_lines.len() >= 6, "{script}");
        assert!(
            adb_lines.iter().all(|l| l.contains("adb -s \"$SERIAL\"")),
            "every adb call names the device: {script}"
        );
        assert!(
            script.trim_end().ends_with("adb -s \"$SERIAL\" shell rm -rf $D"),
            "{script}"
        );
    }

    #[test]
    fn remote_scripts_delete_what_they_copied() {
        let hosts = Hosts::default();
        let mac = leg_script(Leg::Mac, &hosts);
        assert!(mac.contains("HOST=${REGIA_MAC_HOST:-mac-host}"));
        assert!(mac.contains("cargo test --test det_golden"));
        assert!(mac.contains("cd && rm -rf $D"));
        let win = leg_script(Leg::Windows, &hosts);
        assert!(win.contains("CARGO_BUILD_JOBS=4"));
        assert!(win.contains("rmdir /s /q $W"));
        assert!(win.contains("rig_busy"));
        let deck = leg_script(Leg::Deck, &hosts);
        assert!(deck.contains("bin/host/det_golden goldens"));
        assert!(deck.contains("cd && rm -rf $D"));
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn the_bundle_manifest_pins_the_locked_versions() {
        let root = "[workspace.package]\nversion = \"0.2.23\"\nedition = \"2024\"\n\
                    [workspace.dependencies]\nlibm = \"=0.2.16\"\nglam = { version = \"0.33.12\", features = [\"serde\"] }\n\
                    [profile.dev]\nopt-level = 1\n[profile.dev.package.\"*\"]\nopt-level = 3\n";
        let krate = "[package]\nname = \"regia_detsim\"\n[dependencies]\nlibm = { workspace = true }\n\
                     [dev-dependencies]\nglam = { workspace = true }\n";
        let lock = "[[package]]\nname = \"glam\"\nversion = \"0.30.10\"\n[[package]]\nname = \"glam\"\nversion = \"0.33.12\"\n\
                    [[package]]\nname = \"libm\"\nversion = \"0.2.16\"\n";
        let m = bundle_manifest(root, krate, lock).expect("manifest");
        assert!(m.contains("[profile.dev.package.\"*\"]\nopt-level = 3"), "{m}");
        assert!(m.trim_end().ends_with("[workspace]"), "{m}");
        let parsed: toml::Value = toml::from_str(&m).expect("valid toml");
        assert_eq!(parsed["package"]["edition"].as_str(), Some("2024"));
        assert_eq!(
            parsed["dependencies"]["libm"]["version"].as_str(),
            Some("=0.2.16")
        );
        assert_eq!(
            parsed["dev-dependencies"]["glam"]["version"].as_str(),
            Some("=0.33.12")
        );
        assert_eq!(
            parsed["dev-dependencies"]["glam"]["features"][0].as_str(),
            Some("serde")
        );

        let grown = format!("{krate}unexpected = {{ workspace = true }}\n");
        let err = bundle_manifest(root, &grown, lock).expect_err("unknown dependency");
        assert!(err.to_string().contains("gained `unexpected`"), "{err}");
    }

    #[test]
    fn the_bundle_preserves_crate_features_and_dependency_options() {
        let root = r#"
[workspace.package]
version = "0.2.23"
edition = "2024"
[workspace.dependencies]
serde = { version = "1.0.228", default-features = false, features = ["derive"] }
"#;
        let krate = r#"
[features]
default = []
serde = []
[dependencies]
serde = { workspace = true, features = ["alloc"] }
"#;
        let lock = "[[package]]\nname = \"serde\"\nversion = \"1.0.228\"\n";
        let manifest = bundle_manifest(root, krate, lock).expect("bundle");
        let parsed: toml::Value = toml::from_str(&manifest).expect("valid manifest");
        let original: toml::Value = toml::from_str(krate).expect("crate manifest");
        assert_eq!(parsed.get("features"), original.get("features"));
        assert_eq!(
            parsed["dependencies"]["serde"]["default-features"].as_bool(),
            Some(false)
        );
        let features = parsed["dependencies"]["serde"]["features"]
            .as_array()
            .expect("features");
        assert!(features.iter().any(|f| f.as_str() == Some("derive")));
        assert!(features.iter().any(|f| f.as_str() == Some("alloc")));
    }

    #[test]
    fn the_bundle_keeps_per_target_dependency_tables() {
        let root = r#"
[workspace.package]
version = "0.2.23"
edition = "2024"
[workspace.dependencies]
libm = "=0.2.16"
libc = "0.2"
windows-sys = { version = "0.61", features = ["Win32_System_Threading"] }
"#;
        let krate = r#"
[dependencies]
libm = { workspace = true }
[target.'cfg(target_os = "linux")'.dependencies]
libc = { workspace = true }
[target.'cfg(target_os = "windows")'.dependencies]
windows-sys = { workspace = true }
"#;
        let lock = "[[package]]\nname = \"libm\"\nversion = \"0.2.16\"\n[[package]]\nname = \"libc\"\nversion = \"0.2.180\"\n\
                    [[package]]\nname = \"windows-sys\"\nversion = \"0.59.0\"\n[[package]]\nname = \"windows-sys\"\nversion = \"0.61.2\"\n";
        let manifest = bundle_manifest(root, krate, lock).expect("bundle");
        let parsed: toml::Value = toml::from_str(&manifest).expect("valid manifest");
        let target = &parsed["target"];
        assert_eq!(
            target["cfg(target_os = \"linux\")"]["dependencies"]["libc"]["version"].as_str(),
            Some("=0.2.180"),
            "{manifest}"
        );
        let win = &target["cfg(target_os = \"windows\")"]["dependencies"]["windows-sys"];
        assert_eq!(win["version"].as_str(), Some("=0.61.2"), "{manifest}");
        assert_eq!(win["features"][0].as_str(), Some("Win32_System_Threading"));
        // An unknown per-target dependency is refused like a plain one.
        let grown = format!("{krate}winapi = {{ workspace = true }}\n");
        let err = bundle_manifest(root, &grown, lock).expect_err("unknown dependency");
        assert!(err.to_string().contains("gained `winapi`"), "{err}");
    }

    #[test]
    fn the_real_crate_bundles() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root");
        let read = |p: &str| fs::read_to_string(root.join(p)).expect("read");
        let m = bundle_manifest(
            &read("Cargo.toml"),
            &read("crates/regia_detsim/Cargo.toml"),
            &read("Cargo.lock"),
        )
        .expect("the crate's dependencies are all known to the bundle");
        let parsed: toml::Value = toml::from_str(&m).expect("valid manifest");
        // The profiler's per-target thread clock (ADR-8498) travels with the bundle.
        let targets = parsed["target"].as_table().expect("per-target tables");
        assert!(
            targets
                .values()
                .any(|t| t.get("dependencies").and_then(|d| d.get("libc")).is_some()),
            "{m}"
        );
        assert!(
            targets
                .values()
                .any(|t| t.get("dependencies").and_then(|d| d.get("windows-sys")).is_some()),
            "{m}"
        );
        assert_eq!(
            parsed["dependencies"]["libm"]["version"].as_str(),
            Some("=0.2.16")
        );
        assert!(
            parsed["dev-dependencies"]["glam"]["version"]
                .as_str()
                .expect("version")
                .starts_with("=0.33.")
        );
        // serde (DetRng and the integer-millimeter types) and serde_json (their round-trip test): ADR-8483.
        let dep_version = |section: &str, name: &str| {
            parsed
                .get(section)
                .and_then(|t| t.get(name))
                .and_then(|d| d.get("version").and_then(|v| v.as_str()).or(d.as_str()))
                .map(str::to_owned)
        };
        assert!(
            dep_version("dependencies", "serde").is_some_and(|v| v.starts_with("=1.0.")),
            "{m}"
        );
        assert!(
            dep_version("dev-dependencies", "serde_json").is_some_and(|v| v.starts_with("=1.")),
            "{m}"
        );
        let original: toml::Value =
            toml::from_str(&read("crates/regia_detsim/Cargo.toml")).expect("crate manifest");
        assert_eq!(parsed.get("features"), original.get("features"));
        assert!(
            m.contains("regia_rules = { path = \"vendor/regia_rules\" }"),
            "{m}"
        );
        let vendor = rules_vendor_manifest(&read("Cargo.lock")).expect("vendor manifest");
        assert!(
            vendor.contains("serde = { version = \"=1.0.229\", features = [\"derive\"] }"),
            "{vendor}"
        );
        assert!(vendor.contains("regia_detsim = { path = \"../..\" }"), "{vendor}");
    }

    #[test]
    fn cargo_messages_give_the_test_executable() {
        let msgs = "{\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"regia_detsim\"},\"profile\":{\"test\":false},\"executable\":null}\n\
                    {\"reason\":\"compiler-artifact\",\"target\":{\"name\":\"det_golden\"},\"profile\":{\"test\":true},\"executable\":\"/t/debug/deps/det_golden-1\"}\n\
                    {\"reason\":\"build-finished\",\"success\":true}\n";
        assert_eq!(
            test_executable(msgs),
            Some(PathBuf::from("/t/debug/deps/det_golden-1"))
        );
        assert_eq!(test_executable("{\"reason\":\"build-finished\"}"), None);
    }

    #[test]
    fn the_newest_extensionless_test_file_is_the_executable() {
        let dir = std::env::temp_dir().join(format!("xtask-det-golden-deps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        for name in [
            "det_golden-aa.d",
            "det_golden-aa.rcgu.o",
            "other-bb",
            "det_golden-cc",
        ] {
            fs::write(dir.join(name), "x").expect("write");
        }
        assert_eq!(newest_test_executable(&dir), Some(dir.join("det_golden-cc")));
        assert_eq!(newest_test_executable(&dir.join("missing")), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_dir_reads_logs_by_leg_name_and_writes_summaries() {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xtask-det-golden-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("logs")).expect("mkdir");
        fs::write(dir.join("logs/host.log"), leg_log(Leg::Host, "x86_64-linux")).expect("write");
        fs::write(dir.join("logs/mac.log"), leg_log(Leg::Mac, "aarch64-macos")).expect("write");
        fs::write(dir.join("logs/notes.txt"), "ignored").expect("write");
        let (v, md) = verify_dir(&dir, &[Leg::Host, Leg::Mac]).expect("verify");
        assert!(v.ok(), "{:?}", v.problems);
        assert!(md.contains("| mac (aarch64-macos) |"), "{md}");
        let json: Value = serde_json::from_str(&fs::read_to_string(dir.join("summary.json")).expect("json"))
            .expect("parse");
        assert_eq!(json["ok"], true);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every committed engine golden is a clean scenario (ADR-8838, KI-GP-001): the runners
    /// require it on every leg. Fails if a golden is added without joining `CLEAN_SCENARIOS`.
    #[test]
    fn every_committed_engine_golden_is_a_clean_scenario() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/regia_detsim/goldens");
        let mut committed: Vec<String> = fs::read_dir(&dir)
            .expect("the engine goldens")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("detgold"))
            .filter_map(|p| {
                let text = fs::read_to_string(&p).ok()?;
                let name = text.lines().find_map(|l| l.strip_prefix("scenario="))?;
                Some(name.trim().to_string())
            })
            .filter(|name| name != PLANTED_SCENARIO)
            .collect();
        committed.sort();
        let mut clean: Vec<String> = CLEAN_SCENARIOS.iter().map(|s| (*s).to_string()).collect();
        clean.sort();
        let missing: Vec<&String> = committed.iter().filter(|n| !clean.contains(n)).collect();
        assert!(
            missing.is_empty(),
            "committed engine goldens the runners do not require on every leg: {missing:?}"
        );
        assert_eq!(
            clean, committed,
            "CLEAN_SCENARIOS names a golden that is not committed"
        );
    }

    /// A leg that reports the four old scenarios but not `math_rng_fixed_vectors` fails (KI-GP-001:
    /// it was reported but never required).
    #[test]
    fn a_leg_without_math_rng_fixed_vectors_fails() {
        let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
        let mac_text: String = leg_log(Leg::Mac, "aarch64-macos")
            .lines()
            .filter(|l| !l.contains("\"scenario\":\"math_rng_fixed_vectors\""))
            .map(|l| format!("{l}\n"))
            .collect();
        let mac = parse_log(Leg::Mac, &mac_text);
        let v = verify(&[host, mac], &[Leg::Host, Leg::Mac]);
        assert!(!v.ok());
        assert!(
            v.problems.iter().any(|p| p.code == "scenario_missing"
                && p.leg == Some(Leg::Mac)
                && p.detail.contains("math_rng_fixed_vectors")),
            "{:?}",
            v.problems
        );
    }

    #[test]
    fn qemu_when_present_runs_qemu_when_it_is_here_and_owes_it_when_not() {
        let legs = [Leg::Host, Leg::Qemu];
        assert_eq!(qemu_when_present(&legs, true), (legs.to_vec(), None));
        let (run, owed) = qemu_when_present(&legs, false);
        assert_eq!(run, [Leg::Host]);
        let owed = owed.expect("an owed log");
        assert!(owed.starts_with("regia-det-leg-owed qemu_missing: "), "{owed}");
        // Without a qemu leg there is nothing to owe.
        assert_eq!(qemu_when_present(&[Leg::Host], false), (vec![Leg::Host], None));
    }

    #[test]
    fn an_owed_qemu_leg_is_listed_and_does_not_fail_or_pass_silently() {
        let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
        let (_, owed) = qemu_when_present(&[Leg::Host, Leg::Qemu], false);
        let qemu = parse_log(Leg::Qemu, &owed.expect("owed log"));
        let logs = [host, qemu];
        let v = verify(&logs, &[Leg::Host, Leg::Qemu]);
        assert!(v.ok(), "{:?}", v.problems);
        assert_eq!(v.status(), "OWED");
        assert!(v.owed.iter().all(|p| p.code == "leg_owed"), "{:?}", v.owed);
        assert!(
            v.owed.iter().any(|p| p.detail.contains("qemu_missing")),
            "{:?}",
            v.owed
        );
        assert!(
            v.owed
                .iter()
                .any(|p| p.detail.contains("no aarch64 leg reported")),
            "{:?}",
            v.owed
        );
        let md = summary_markdown(&v, &logs);
        assert!(
            md.starts_with("# Determinism goldens across targets: OWED"),
            "{md}"
        );
        assert!(md.contains("Owed (not run; not a pass)"), "{md}");
        let j = summary_json(&v, &logs);
        assert_eq!(j["status"], "OWED");
        assert_eq!(j["owed"].as_array().map(Vec::len), Some(2));
        // The same missing qemu without the flag stays a failure.
        let error = parse_log(
            Leg::Qemu,
            "regia-det-leg-error qemu_missing: qemu-aarch64 is not on PATH (install qemu-user)\nregia-det-exit=3\n",
        );
        let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
        let v = verify(&[host, error], &[Leg::Host, Leg::Qemu]);
        assert_eq!(v.status(), "FAIL");
        assert!(v.owed.is_empty());
        // An owed line never hides a leg that reported: a qemu leg that ran and diverged fails.
        let mut ran = leg_log(Leg::Qemu, "aarch64-android").replace(
            &line(
                "steer_slalom",
                "aarch64-android",
                "unrecorded_target",
                "536e1716468a80e3",
                None,
                true,
            ),
            &line(
                "steer_slalom",
                "aarch64-android",
                "diverged",
                "0000000000000001",
                Some(3),
                false,
            ),
        );
        ran.push_str(&owed_log("qemu_missing", "stale"));
        let host = parse_log(Leg::Host, &leg_log(Leg::Host, "x86_64-linux"));
        let v = verify(&[host, parse_log(Leg::Qemu, &ran)], &[]);
        assert_eq!(v.status(), "FAIL");
        assert!(v.owed.is_empty(), "{:?}", v.owed);
    }
}
