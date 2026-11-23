// SPDX-License-Identifier: MPL-2.0
//! Fresh-home install and the physical-key driver for `regia play-gate run` (ADR-8452).
//!
//! The installer unpacks a ustar archive, or copies a package directory, into a new home. The
//! home's XDG directories start empty, so a prior config is not visible. The driver sends
//! `play.inject_input` events that carry a scancode and a per-run token. It never sends text.
//! A package whose feature list has no `agent` entry stops with [`CODE_NO_AGENT`] and is not
//! given another way in. Expect steps match an ordered list of regular expressions. The deadline
//! is a frame count on this driver's clock, not wall time.
//!
//! Durations stored as milliseconds in a key track count as frames on this path. `regia play-gate
//! replay` still reads those fields as milliseconds.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use serde_json::{Value, json};

use crate::key_track::{self, Edge, KeyTrack, PreparedTrack, Step};

/// The installed build has no agent feature. The driver stops and injects nothing.
pub const CODE_NO_AGENT: &str = "play_gate_no_agent";
/// `run` was asked to install without a fresh home.
pub const CODE_FRESH_HOME: &str = "play_gate_fresh_home";
/// The archive could not be installed, or the package has no `player` file.
pub const CODE_INSTALL: &str = "play_gate_install";
/// A keymap value is not a scancode, or a track target has no scancode.
pub const CODE_KEYMAP: &str = "play_gate_keymap";
/// A required expect found no log line inside its frame budget.
pub const CODE_EXPECT_MISSED: &str = "play_gate_expect_missed";
/// An error-line pattern matched the player log.
pub const CODE_LOG_ERROR: &str = "play_gate_log_error";
/// The package's `script.json` is not the stand-in this driver reads.
pub const CODE_SCRIPT: &str = "play_gate_script";

/// Agent method the driver calls. There is no second method.
pub const INJECT_METHOD: &str = "play.inject_input";

/// Frames one expect may wait. The requirements counted 2400 as a play budget in seconds.
/// This driver counts the same number in frames.
pub const DEFAULT_WITHIN_FRAMES: u64 = 2400;
/// Install budget recorded on the report. The requirements used 7200 seconds. This driver
/// copies an archive and does not wait.
pub const DEFAULT_INSTALL_BUDGET: u64 = 7200;
/// Second play budget recorded on the report. The requirements used 2100 seconds for a
/// second scenario. This driver counts the same number in frames and does not wait.
pub const DEFAULT_SECOND_PLAY_FRAMES: u64 = 2100;
/// Frame budget recorded from the requirements' per-run wait (240 seconds), counted in frames.
pub const DEFAULT_PLAY_WAIT_FRAMES: u64 = 240;
/// Longest evidence cell, in Unicode scalar values. A longer cell keeps this many and `...`.
pub const EVIDENCE_CLIP: usize = 240;
/// Largest ustar entry this installer will write.
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

const BLOCK: usize = 512;

/// One player log line, stamped with the driver's frame clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLog {
    /// Frame the line became visible. The clock starts at 0.
    pub frame: u64,
    /// Line text, without a trailing newline.
    pub text: String,
}

/// One ordered expect. Every pattern must match a later unused line at or before the deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateExpect {
    /// Scenario name in the report table.
    pub scenario: String,
    /// Step name in the report table.
    pub step: String,
    /// Regular expressions, matched in order.
    pub patterns: Vec<String>,
    /// How many frames past the clock a line may be and still match.
    pub within_frames: u64,
    /// This step only searches lines after the named step has matched.
    pub after: Option<String>,
    /// Frame-file suffix `_TAG.png`. A missing file is a note, not a failure.
    pub frame_tag: Option<String>,
}

/// One row of the play report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRow {
    /// Scenario name.
    pub scenario: String,
    /// Step name.
    pub step: String,
    /// `PASS` or `FAIL`.
    pub result: String,
    /// The matching line, or why it missed. Clipped to [`EVIDENCE_CLIP`].
    pub evidence: String,
    /// Frame file path, or empty.
    pub frame: String,
}

/// One physical-key call. `text` is always `None`: the driver has no text field to fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalKeyCall {
    /// Always [`INJECT_METHOD`].
    pub method: &'static str,
    /// Per-run token. Every call in one run shares it.
    pub token: String,
    /// Physical key, as a scancode.
    pub scancode: u32,
    /// Down or up.
    pub edge: Edge,
    /// Driver frame at the call.
    pub frame: u64,
    /// Text payload. The driver leaves this `None`.
    pub text: Option<String>,
}

/// What `run` was asked to do. Owned so a caller can fill fields after [`GateRequest::new`].
#[derive(Debug, Clone)]
pub struct GateRequest {
    /// Ustar archive, or a directory that already holds `player` and `features.json`.
    pub archive: PathBuf,
    /// Directory for `home`, `play_gate.md` and `play_gate.json`.
    pub out: PathBuf,
    /// When false, the run stops before creating a home.
    pub fresh_home: bool,
    /// Key-track text. `None` grades `expects` against an empty drive.
    pub track_text: Option<String>,
    /// Ordered expects. When empty, one expect is built from each track `Expect` step.
    pub expects: Vec<GateExpect>,
    /// `name = scancode` lines. A non-numeric value is refused.
    pub keymap_text: String,
    /// Frame budget for expects derived from the track.
    pub within_frames: u64,
    /// Recorded install budget. Not a wait.
    pub install_budget: u64,
    /// Recorded second-play frame budget. Not a wait.
    pub second_play_frames: u64,
    /// Recorded play-wait frame budget. Not a wait.
    pub play_wait_frames: u64,
    /// Seed for the per-run token. The same seed is the same token.
    pub token_seed: u64,
    /// Parent environment. Keys that look like credentials are dropped and never logged.
    pub parent_env: Vec<(String, String)>,
    /// When set, a matching line fails the run. The pattern is a regular expression.
    pub error_pattern: Option<String>,
    /// Directory of `_TAG.png` frame files. A missing file is a note.
    pub frames_dir: Option<PathBuf>,
}

impl GateRequest {
    /// A fresh-home run with the default frame budgets.
    #[must_use]
    pub fn new(archive: impl Into<PathBuf>, out: impl Into<PathBuf>) -> Self {
        Self {
            archive: archive.into(),
            out: out.into(),
            fresh_home: true,
            track_text: None,
            expects: Vec::new(),
            keymap_text: String::new(),
            within_frames: DEFAULT_WITHIN_FRAMES,
            install_budget: DEFAULT_INSTALL_BUDGET,
            second_play_frames: DEFAULT_SECOND_PLAY_FRAMES,
            play_wait_frames: DEFAULT_PLAY_WAIT_FRAMES,
            token_seed: 1,
            parent_env: Vec::new(),
            error_pattern: None,
            frames_dir: None,
        }
    }
}

/// The markdown and JSON report of one run.
#[derive(Debug, Clone)]
pub struct PlayReport {
    /// True when every row passed and no code was set.
    pub ok: bool,
    /// Stable code, or `None` on a pass.
    pub code: Option<String>,
    /// Short reason.
    pub message: String,
    /// Process exit: 0 pass, 1 failed run, 2 refused request.
    pub exit_code: u8,
    /// Fresh home, when one was created.
    pub home: Option<PathBuf>,
    /// Installed package directory, when the archive unpacked.
    pub package_dir: Option<PathBuf>,
    /// 0 when the package is in place, 1 when install failed, `None` when install did not start.
    pub install_rc: Option<i32>,
    /// Per-run token, once the driver starts. `None` when it never starts.
    pub token: Option<String>,
    /// Child environment keys. Credential keys are absent. Values are not stored.
    pub env_keys: Vec<String>,
    /// Recorded budgets. They are counters, not waits.
    pub install_budget: u64,
    /// See [`DEFAULT_SECOND_PLAY_FRAMES`].
    pub second_play_frames: u64,
    /// See [`DEFAULT_PLAY_WAIT_FRAMES`].
    pub play_wait_frames: u64,
    /// Table rows.
    pub rows: Vec<GateRow>,
    /// Physical-key calls, in send order.
    pub calls: Vec<PhysicalKeyCall>,
    /// Steps the driver did not send (pointer and wheel).
    pub notes: Vec<String>,
}

/// Token for one run. The same seed always returns the same 32 hex digits.
#[must_use]
pub fn session_token(seed: u64) -> String {
    let mixed = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x6C62_272E_07BB_0142);
    format!("{seed:016x}{mixed:016x}")
}

/// True when a keymap line sets `input.setup_done`, ignoring comments.
#[must_use]
pub fn keymap_sets_setup_done(text: &str) -> bool {
    for line in text.lines() {
        let body = line.split('#').next().unwrap_or("").trim();
        if body.starts_with("input.setup_done") && body.contains('=') {
            return true;
        }
    }
    false
}

/// The keymap written into the fresh home. Appends `input.setup_done = 1` when the text
/// does not already set that name.
#[must_use]
pub fn installed_keymap(text: &str) -> String {
    if keymap_sets_setup_done(text) {
        return text.to_string();
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("input.setup_done = 1\n");
    out
}

/// Grades a finished log. `within_frames` is the frame deadline, not a wait.
///
/// # Errors
///
/// A pattern or `error_pattern` is not a regular expression.
pub fn grade_log(
    lines: &[FrameLog],
    steps: &[GateExpect],
    frames_dir: Option<&Path>,
    error_pattern: Option<&str>,
) -> Result<Vec<GateRow>, String> {
    let mut compiled = Vec::with_capacity(steps.len());
    for step in steps {
        let mut patterns = Vec::with_capacity(step.patterns.len());
        for pattern in &step.patterns {
            patterns.push(compile_pattern(pattern)?);
        }
        compiled.push(patterns);
    }
    let error_re = match error_pattern {
        Some(pattern) => Some(compile_pattern(pattern)?),
        None => None,
    };
    let mut rows = Vec::new();
    let mut anchors: BTreeMap<String, usize> = BTreeMap::new();
    let mut clock: u64 = 0;
    for (step, patterns) in steps.iter().zip(compiled.iter()) {
        let (frame_path, note) = find_frame(frames_dir, step.frame_tag.as_deref());
        if let Some(after) = &step.after
            && !anchors.contains_key(after)
        {
            rows.push(row(
                step,
                false,
                annotate(&format!("after {after} missing"), &note),
                &frame_path,
            ));
            continue;
        }
        let start = step
            .after
            .as_ref()
            .and_then(|after| anchors.get(after).map(|index| index.saturating_add(1)))
            .unwrap_or(0);
        let deadline = clock.saturating_add(step.within_frames);
        let mut mark = start;
        let mut missed: Option<String> = None;
        let mut last_index = None;
        for pattern in patterns {
            match search_line(lines, pattern, mark, deadline) {
                Some(index) => {
                    last_index = Some(index);
                    mark = index.saturating_add(1);
                }
                None => {
                    missed = Some(pattern.as_str().to_string());
                    break;
                }
            }
        }
        match (missed, last_index) {
            (Some(pattern), _) => {
                rows.push(row(
                    step,
                    false,
                    annotate(&format!("no matching log line: {pattern}"), &note),
                    &frame_path,
                ));
            }
            (None, Some(index)) => {
                clock = lines[index].frame;
                anchors.insert(step.step.clone(), index);
                rows.push(row(
                    step,
                    true,
                    annotate(lines[index].text.trim(), &note),
                    &frame_path,
                ));
            }
            (None, None) => {
                anchors.insert(step.step.clone(), start);
                rows.push(row(step, true, annotate("", &note), &frame_path));
            }
        }
    }
    if let Some(pattern) = error_re {
        let mut hits = Vec::new();
        for line in lines {
            if pattern.is_match(&line.text) {
                hits.push(line.text.trim().to_string());
            }
        }
        if let Some(first) = hits.first() {
            let extra = if hits.len() > 1 {
                format!(" (+{} more)", hits.len() - 1)
            } else {
                String::new()
            };
            rows.push(GateRow {
                scenario: "gate".to_string(),
                step: "error_line".to_string(),
                result: "FAIL".to_string(),
                evidence: clip(&format!("{first}{extra}")),
                frame: String::new(),
            });
        } else {
            rows.push(GateRow {
                scenario: "gate".to_string(),
                step: "error_line".to_string(),
                result: "PASS".to_string(),
                evidence: "no error line".to_string(),
                frame: String::new(),
            });
        }
    }
    Ok(rows)
}

/// Markdown table. `|` in a cell is escaped so the column stays put.
#[must_use]
pub fn render_markdown(report: &PlayReport) -> String {
    let mut lines = vec![
        "# Play gate".to_string(),
        String::new(),
        format!("Result: {}", if report.ok { "PASS" } else { "FAIL" }),
        String::new(),
        format!("- package_dir: `{}`", display_opt(report.package_dir.as_deref())),
        format!("- home: `{}`", display_opt(report.home.as_deref())),
        format!(
            "- install_rc: `{}`",
            report.install_rc.map(|rc| rc.to_string()).unwrap_or_default()
        ),
        format!("- token: `{}`", report.token.as_deref().unwrap_or("")),
    ];
    if !report.message.is_empty() {
        lines.push(format!("- message: {}", report.message));
    }
    if let Some(code) = &report.code {
        lines.push(format!("- code: `{code}`"));
    }
    lines.push(String::new());
    lines.push("| scenario | step | result | evidence | frame |".to_string());
    lines.push("| --- | --- | --- | --- | --- |".to_string());
    for row in &report.rows {
        lines.push(format!(
            "| {} | {} | {} | {} | {} |",
            row.scenario,
            row.step,
            row.result,
            escape_cell(&row.evidence),
            escape_cell(&row.frame),
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Writes `play_gate.md` and `play_gate.json` under `out`.
///
/// # Errors
///
/// A directory or a file could not be written.
pub fn write_reports(out: &Path, report: &PlayReport) -> Result<(), String> {
    fs::create_dir_all(out).map_err(|err| format!("creating {}: {err}", out.display()))?;
    let markdown = render_markdown(report);
    write_atomic(&out.join("play_gate.md"), markdown.as_bytes())?;
    let body = report_json(report);
    let text = serde_json::to_string_pretty(&body).map_err(|err| err.to_string())?;
    write_atomic(&out.join("play_gate.json"), format!("{text}\n").as_bytes())
}

/// Installs `request.archive` when `fresh_home` is set, drives physical keys, and writes the report.
#[must_use]
pub fn run_gate(request: &GateRequest) -> PlayReport {
    let mut report = PlayReport {
        ok: false,
        code: None,
        message: String::new(),
        exit_code: 1,
        home: None,
        package_dir: None,
        install_rc: None,
        token: None,
        env_keys: Vec::new(),
        install_budget: request.install_budget,
        second_play_frames: request.second_play_frames,
        play_wait_frames: request.play_wait_frames,
        rows: Vec::new(),
        calls: Vec::new(),
        notes: Vec::new(),
    };
    if !request.fresh_home {
        report.code = Some(CODE_FRESH_HOME.to_string());
        report.message = "play-gate run requires --fresh-home; nothing was installed".to_string();
        return finish(request, report);
    }
    let prepared = match request.track_text.as_deref() {
        Some(text) => match PreparedTrack::parse(text) {
            Ok(track) => Some(track),
            Err(err) => {
                report.code = Some(err.code().to_string());
                report.message = err.to_string();
                return finish(request, report);
            }
        },
        None => None,
    };
    let keymap_text = installed_keymap(&request.keymap_text);
    let keymap = match parse_keymap(&keymap_text) {
        Ok(map) => map,
        Err(err) => {
            report.code = Some(CODE_KEYMAP.to_string());
            report.message = err;
            return finish(request, report);
        }
    };
    let expects = if request.expects.is_empty() {
        prepared
            .as_ref()
            .map(|track| expects_from_track(track.track(), request.within_frames))
            .unwrap_or_default()
    } else {
        request.expects.clone()
    };
    let home = request.out.join("home");
    if let Err(err) = install_tree(&request.archive, &home, &mut report) {
        report.code = Some(CODE_INSTALL.to_string());
        report.message = err;
        report.install_rc = Some(1);
        report.rows = not_run_rows(&expects);
        return finish(request, report);
    }
    report.install_rc = Some(0);
    report.env_keys = child_env_keys(&request.parent_env);
    if let Err(err) = write_install_log(request, &report) {
        report.code = Some(CODE_INSTALL.to_string());
        report.message = err;
        report.rows = not_run_rows(&expects);
        return finish(request, report);
    }
    let Some(package) = report.package_dir.clone() else {
        report.code = Some(CODE_INSTALL.to_string());
        report.message = "installer produced no package".to_string();
        report.install_rc = Some(1);
        report.rows = not_run_rows(&expects);
        return finish(request, report);
    };
    if let Err(err) = write_atomic(package.join("keymap.txt").as_path(), keymap_text.as_bytes()) {
        report.code = Some(CODE_INSTALL.to_string());
        report.message = err;
        report.rows = not_run_rows(&expects);
        return finish(request, report);
    }
    let features = match read_features(&package) {
        Ok(list) => list,
        Err(err) => {
            report.code = Some(CODE_INSTALL.to_string());
            report.message = err;
            report.install_rc = Some(1);
            report.rows = not_run_rows(&expects);
            return finish(request, report);
        }
    };
    if !features.iter().any(|feature| feature == "agent") {
        report.code = Some(CODE_NO_AGENT.to_string());
        report.message = "the installed build has no agent feature; the driver stopped".to_string();
        report.rows.push(GateRow {
            scenario: "gate".to_string(),
            step: "agent".to_string(),
            result: "FAIL".to_string(),
            evidence: CODE_NO_AGENT.to_string(),
            frame: String::new(),
        });
        return finish(request, report);
    }
    let script = match read_script(&package) {
        Ok(script) => script,
        Err(err) => {
            report.code = Some(CODE_SCRIPT.to_string());
            report.message = err;
            report.rows = not_run_rows(&expects);
            return finish(request, report);
        }
    };
    let token = session_token(request.token_seed);
    report.token = Some(token.clone());
    let mut player = Player::new(script);
    if let Some(track) = prepared.as_ref() {
        match plan_keys(track.track(), &keymap) {
            Ok((planned, notes)) => {
                report.notes = notes;
                for item in planned {
                    player.ensure_frame(item.frame);
                    player.inject(PhysicalKeyCall {
                        method: INJECT_METHOD,
                        token: token.clone(),
                        scancode: item.scancode,
                        edge: item.edge,
                        frame: player.frame,
                        text: None,
                    });
                }
            }
            Err(err) => {
                report.code = Some(CODE_KEYMAP.to_string());
                report.message = err;
                report.exit_code = 2;
                report.calls.clear();
                return finish(request, report);
            }
        }
    }
    let mut deadline = 0u64;
    for step in &expects {
        deadline = deadline.max(step.within_frames);
    }
    player.ensure_frame(deadline);
    report.calls = player.calls;
    match grade_log(
        &player.log,
        &expects,
        request.frames_dir.as_deref(),
        request.error_pattern.as_deref(),
    ) {
        Ok(rows) => {
            report.rows = rows;
            if report.rows.iter().any(|row| row.result != "PASS") && report.code.is_none() {
                let missed = report
                    .rows
                    .iter()
                    .any(|row| row.evidence.starts_with("no matching log line"));
                report.code = Some(if missed && request.error_pattern.is_none() {
                    CODE_EXPECT_MISSED.to_string()
                } else if report
                    .rows
                    .iter()
                    .any(|row| row.step == "error_line" && row.result == "FAIL")
                {
                    CODE_LOG_ERROR.to_string()
                } else {
                    CODE_EXPECT_MISSED.to_string()
                });
                report.message = report
                    .rows
                    .iter()
                    .find(|row| row.result != "PASS")
                    .map(|row| format!("{}: {}", row.step, row.evidence))
                    .unwrap_or_default();
            }
        }
        Err(err) => {
            report.code = Some(key_track::CODE_PARSE.to_string());
            report.message = err;
            report.exit_code = 2;
        }
    }
    if report.message.is_empty() && report.code.is_none() {
        report.message = "installed".to_string();
    }
    finish(request, report)
}

fn finish(request: &GateRequest, mut report: PlayReport) -> PlayReport {
    let failed = report.rows.iter().any(|row| row.result != "PASS");
    report.ok = report.code.is_none() && !failed;
    if report.exit_code != 2 {
        report.exit_code = if report.ok {
            0
        } else if matches!(
            report.code.as_deref(),
            Some(
                CODE_FRESH_HOME
                    | CODE_KEYMAP
                    | key_track::CODE_PARSE
                    | key_track::CODE_VERSION
                    | key_track::CODE_IO
            )
        ) {
            2
        } else {
            1
        };
    }
    if let Err(err) = write_reports(&request.out, &report) {
        report.ok = false;
        report.exit_code = 1;
        if report.message.is_empty() {
            report.message = err;
        }
    }
    report
}

fn expects_from_track(track: &KeyTrack, within_frames: u64) -> Vec<GateExpect> {
    track
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::Expect { pattern, .. } => Some(GateExpect {
                scenario: "track".to_string(),
                step: pattern.clone(),
                patterns: vec![pattern.clone()],
                within_frames,
                after: None,
                frame_tag: None,
            }),
            _ => None,
        })
        .collect()
}

fn not_run_rows(expects: &[GateExpect]) -> Vec<GateRow> {
    if expects.is_empty() {
        return vec![GateRow {
            scenario: "gate".to_string(),
            step: "play".to_string(),
            result: "FAIL".to_string(),
            evidence: "not run: install failed".to_string(),
            frame: String::new(),
        }];
    }
    expects
        .iter()
        .map(|step| GateRow {
            scenario: step.scenario.clone(),
            step: step.step.clone(),
            result: "FAIL".to_string(),
            evidence: "not run: install failed".to_string(),
            frame: String::new(),
        })
        .collect()
}

struct Planned {
    scancode: u32,
    edge: Edge,
    frame: u64,
}

fn plan_keys(track: &KeyTrack, map: &BTreeMap<String, u32>) -> Result<(Vec<Planned>, Vec<String>), String> {
    let mut planned = Vec::new();
    let mut notes = Vec::new();
    let mut frame = 0u64;
    let mut held: Vec<(String, u32)> = Vec::new();
    for step in &track.steps {
        match step {
            Step::Section { .. } | Step::Note { .. } | Step::Expect { .. } => {}
            Step::Wait { millis } => frame = frame.saturating_add(*millis),
            Step::Tap { target, millis } => {
                let code = scancode_for(target, map)?;
                planned.push(Planned {
                    scancode: code,
                    edge: Edge::Down,
                    frame,
                });
                let up = frame.saturating_add(*millis);
                planned.push(Planned {
                    scancode: code,
                    edge: Edge::Up,
                    frame: up,
                });
                frame = up;
            }
            Step::Press { target } => {
                if !held.iter().any(|(name, _)| name == target) {
                    let code = scancode_for(target, map)?;
                    planned.push(Planned {
                        scancode: code,
                        edge: Edge::Down,
                        frame,
                    });
                    held.push((target.clone(), code));
                }
            }
            Step::Release { target } => {
                if let Some(index) = held.iter().position(|(name, _)| name == target) {
                    let (_, code) = held.remove(index);
                    planned.push(Planned {
                        scancode: code,
                        edge: Edge::Up,
                        frame,
                    });
                }
            }
            Step::ReleaseAll => {
                for (_, code) in held.drain(..) {
                    planned.push(Planned {
                        scancode: code,
                        edge: Edge::Up,
                        frame,
                    });
                }
            }
            Step::Hold { targets, millis } => {
                let mut pressed = Vec::new();
                for target in targets {
                    if !held.iter().any(|(name, _)| name == target) {
                        let code = scancode_for(target, map)?;
                        planned.push(Planned {
                            scancode: code,
                            edge: Edge::Down,
                            frame,
                        });
                        held.push((target.clone(), code));
                        pressed.push(code);
                    }
                }
                frame = frame.saturating_add(*millis);
                for code in pressed {
                    if let Some(index) = held.iter().position(|(_, held_code)| *held_code == code) {
                        held.remove(index);
                        planned.push(Planned {
                            scancode: code,
                            edge: Edge::Up,
                            frame,
                        });
                    }
                }
            }
            Step::TapN {
                target,
                count,
                gap_millis,
            } => {
                let code = scancode_for(target, map)?;
                let count = u64::from(*count);
                let hold = key_track::DEFAULT_TAP_MILLIS;
                for index in 0..count {
                    let start = frame.saturating_add(index.saturating_mul(*gap_millis));
                    planned.push(Planned {
                        scancode: code,
                        edge: Edge::Down,
                        frame: start,
                    });
                    planned.push(Planned {
                        scancode: code,
                        edge: Edge::Up,
                        frame: start.saturating_add(hold),
                    });
                }
                let gaps = count.saturating_sub(1).saturating_mul(*gap_millis);
                frame = frame.saturating_add(gaps).saturating_add(hold);
            }
            Step::Mouse { .. } | Step::Wheel { .. } => {
                notes.push("pointer or wheel step is not a physical key and was not sent".to_string());
            }
        }
    }
    Ok((planned, notes))
}

fn scancode_for(target: &str, map: &BTreeMap<String, u32>) -> Result<u32, String> {
    if let Some(rest) = target.strip_prefix("scancode:") {
        return rest
            .parse::<u32>()
            .map_err(|_| format!("`{target}` is not a scancode"));
    }
    map.get(target)
        .copied()
        .ok_or_else(|| format!("no scancode for `{target}`"))
}

struct Script {
    on_down: BTreeMap<u32, Vec<String>>,
    scheduled: Vec<FrameLog>,
}

struct Player {
    frame: u64,
    log: Vec<FrameLog>,
    on_down: BTreeMap<u32, Vec<String>>,
    scheduled: Vec<FrameLog>,
    calls: Vec<PhysicalKeyCall>,
}

impl Player {
    fn new(script: Script) -> Self {
        Self {
            frame: 0,
            log: Vec::new(),
            on_down: script.on_down,
            scheduled: script.scheduled,
            calls: Vec::new(),
        }
    }

    fn ensure_frame(&mut self, target: u64) {
        if self.scheduled.is_empty() {
            self.frame = self.frame.max(target);
            return;
        }
        while self.frame < target {
            self.frame = self.frame.saturating_add(1);
            for line in &self.scheduled {
                if line.frame == self.frame {
                    self.log.push(line.clone());
                }
            }
        }
    }

    fn inject(&mut self, call: PhysicalKeyCall) {
        if call.edge == Edge::Down
            && let Some(lines) = self.on_down.get(&call.scancode)
        {
            for text in lines {
                self.log.push(FrameLog {
                    frame: self.frame,
                    text: text.clone(),
                });
            }
        }
        self.calls.push(call);
    }
}

fn read_script(package: &Path) -> Result<Script, String> {
    let path = package.join("script.json");
    if !path.is_file() {
        return Ok(Script {
            on_down: BTreeMap::new(),
            scheduled: Vec::new(),
        });
    }
    let text = fs::read_to_string(&path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|err| format!("script.json: {err}"))?;
    let mut on_down = BTreeMap::new();
    if let Some(map) = value.get("on_down").and_then(Value::as_object) {
        for (key, lines) in map {
            let code: u32 = key
                .parse()
                .map_err(|_| format!("script.json on_down key `{key}` is not a scancode"))?;
            let list = lines
                .as_array()
                .ok_or_else(|| format!("script.json on_down `{key}` is not a list"))?;
            let mut texts = Vec::new();
            for line in list {
                let text = line
                    .as_str()
                    .ok_or_else(|| "script.json log line is not a string".to_string())?;
                texts.push(text.to_string());
            }
            on_down.insert(code, texts);
        }
    }
    let mut scheduled = Vec::new();
    if let Some(lines) = value.get("lines").and_then(Value::as_array) {
        for line in lines {
            let frame = line
                .get("frame")
                .and_then(Value::as_u64)
                .ok_or_else(|| "script.json line needs a frame".to_string())?;
            let text = line
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "script.json line needs text".to_string())?
                .to_string();
            scheduled.push(FrameLog { frame, text });
        }
    }
    Ok(Script { on_down, scheduled })
}

fn read_features(package: &Path) -> Result<Vec<String>, String> {
    let path = package.join("features.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&text).map_err(|err| format!("features.json: {err}"))?;
    let list = value
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| "features.json has no features list".to_string())?;
    let mut names = Vec::new();
    for item in list {
        let name = item
            .as_str()
            .ok_or_else(|| "features.json feature is not a string".to_string())?;
        names.push(name.to_string());
    }
    Ok(names)
}

fn install_tree(archive: &Path, home: &Path, report: &mut PlayReport) -> Result<(), String> {
    if home.exists() {
        fs::remove_dir_all(home).map_err(|err| format!("removing {}: {err}", home.display()))?;
    }
    let config = home.join(".config");
    let cache = home.join(".cache");
    let data = home.join(".local").join("share");
    let state = home.join(".local").join("state");
    let runtime = home.join("run");
    for dir in [&config, &cache, &data, &state, &runtime] {
        fs::create_dir_all(dir).map_err(|err| format!("creating {}: {err}", dir.display()))?;
    }
    report.home = Some(home.to_path_buf());
    let stage = home.join(".stage");
    fs::create_dir_all(&stage).map_err(|err| err.to_string())?;
    if archive.is_dir() {
        copy_tree(archive, &stage)?;
    } else {
        let bytes = fs::read(archive).map_err(|err| format!("reading {}: {err}", archive.display()))?;
        unpack_ustar(&bytes, &stage)?;
    }
    if !stage.join("player").is_file() {
        return Err("no package: the archive has no player file".to_string());
    }
    let package = data.join("regia-play").join("player");
    if let Some(parent) = package.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::rename(&stage, &package).map_err(|err| format!("placing the package: {err}"))?;
    report.package_dir = Some(package);
    Ok(())
}

fn child_env_keys(parent: &[(String, String)]) -> Vec<String> {
    let mut keys: Vec<String> = parent
        .iter()
        .map(|(key, _)| key.clone())
        .filter(|key| !is_credential_key(key) && key != "HOME" && !key.starts_with("XDG_"))
        .collect();
    keys.sort();
    keys.push("HOME".to_string());
    keys.push("XDG_CACHE_HOME".to_string());
    keys.push("XDG_CONFIG_HOME".to_string());
    keys.push("XDG_DATA_HOME".to_string());
    keys.push("XDG_RUNTIME_DIR".to_string());
    keys.push("XDG_STATE_HOME".to_string());
    keys
}

fn is_credential_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.ends_with("_TOKEN") || upper.contains("PASSWORD") || upper.contains("SECRET") || upper == "GH_TOKEN"
}

fn write_install_log(request: &GateRequest, report: &PlayReport) -> Result<(), String> {
    let home = report.home.as_deref().unwrap_or(Path::new(""));
    let mut text = format!("fresh-home\nhome={}\n", home.display());
    text.push_str("env_keys=");
    text.push_str(&report.env_keys.join(" "));
    text.push('\n');
    write_atomic(&request.out.join("install.log"), text.as_bytes())
}

fn parse_keymap(text: &str) -> Result<BTreeMap<String, u32>, String> {
    let mut map = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let body = line.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let Some((name, value)) = body.split_once('=') else {
            return Err(format!("keymap line {}: expected name = scancode", number + 1));
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return Err(format!("keymap line {}: empty name", number + 1));
        }
        let code: u32 = value
            .parse()
            .map_err(|_| format!("keymap line {}: `{value}` is not a scancode", number + 1))?;
        map.insert(name.to_string(), code);
    }
    Ok(map)
}

fn compile_pattern(pattern: &str) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("expect pattern is empty".to_string());
    }
    Regex::new(pattern).map_err(|err| format!("expect pattern `{pattern}`: {err}"))
}

fn search_line(lines: &[FrameLog], pattern: &Regex, start: usize, deadline: u64) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, line)| (line.frame <= deadline && pattern.is_match(&line.text)).then_some(index))
}

fn find_frame(dir: Option<&Path>, tag: Option<&str>) -> (String, String) {
    let Some(tag) = tag else {
        return (String::new(), String::new());
    };
    let Some(dir) = dir else {
        return (String::new(), String::new());
    };
    if !dir.is_dir() {
        return (String::new(), "frame missing".to_string());
    }
    let suffix = format!("_{tag}.png");
    let mut hits = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return (String::new(), "frame missing".to_string()),
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.path().is_file() && name.ends_with(&suffix) {
            hits.push(entry.path());
        }
    }
    hits.sort();
    match hits.last() {
        Some(path) => (path.display().to_string(), String::new()),
        None => (String::new(), "frame missing".to_string()),
    }
}

fn annotate(evidence: &str, note: &str) -> String {
    if note.is_empty() {
        evidence.to_string()
    } else if evidence.is_empty() {
        note.to_string()
    } else {
        format!("{evidence} ({note})")
    }
}

fn row(step: &GateExpect, ok: bool, evidence: String, frame: &str) -> GateRow {
    GateRow {
        scenario: step.scenario.clone(),
        step: step.step.clone(),
        result: if ok {
            "PASS".to_string()
        } else {
            "FAIL".to_string()
        },
        evidence: clip(&evidence),
        frame: frame.to_string(),
    }
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = flat.chars().count();
    if count <= EVIDENCE_CLIP {
        return flat;
    }
    let keep = EVIDENCE_CLIP.saturating_sub(3);
    let trimmed: String = flat.chars().take(keep).collect();
    format!("{trimmed}...")
}

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

fn display_opt(path: Option<&Path>) -> String {
    path.map(|path| path.display().to_string()).unwrap_or_default()
}

/// The JSON object written to `play_gate.json`.
#[must_use]
pub fn report_json(report: &PlayReport) -> Value {
    let rows: Vec<Value> = report
        .rows
        .iter()
        .map(|row| {
            json!({
                "scenario": row.scenario,
                "step": row.step,
                "result": row.result,
                "evidence": row.evidence,
                "frame": row.frame,
            })
        })
        .collect();
    let calls: Vec<Value> = report
        .calls
        .iter()
        .map(|call| {
            json!({
                "method": call.method,
                "token": call.token,
                "scancode": call.scancode,
                "edge": match call.edge {
                    Edge::Down => "down",
                    Edge::Up => "up",
                },
                "frame": call.frame,
                "text": call.text,
            })
        })
        .collect();
    json!({
        "ok": report.ok,
        "code": report.code,
        "message": report.message,
        "exit_code": report.exit_code,
        "package_dir": report.package_dir.as_ref().map(|path| path.display().to_string()),
        "home": report.home.as_ref().map(|path| path.display().to_string()),
        "install_rc": report.install_rc,
        "install_budget": report.install_budget,
        "second_play_frames": report.second_play_frames,
        "play_wait_frames": report.play_wait_frames,
        "token": report.token,
        "env_keys": report.env_keys,
        "rows": rows,
        "calls": calls,
        "notes": report.notes,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("creating {}: {err}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|err| format!("writing {}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("renaming {}: {err}", path.display()))
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|err| err.to_string())?;
    let entries = fs::read_dir(src).map_err(|err| format!("reading {}: {err}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let meta = path.symlink_metadata().map_err(|err| err.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !safe_rel(&name) {
            return Err(format!("refusing `{name}`"));
        }
        let target = dst.join(entry.file_name());
        if meta.file_type().is_symlink() {
            return Err(format!("refusing symlink `{}`", path.display()));
        } else if meta.is_dir() {
            copy_tree(&path, &target)?;
        } else if meta.is_file() {
            fs::copy(&path, &target).map_err(|err| format!("copying {}: {err}", path.display()))?;
        } else {
            return Err(format!("refusing special file `{}`", path.display()));
        }
    }
    Ok(())
}

fn safe_rel(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn unpack_ustar(bytes: &[u8], dest: &Path) -> Result<(), String> {
    if bytes.len() < BLOCK {
        return Err("archive is shorter than one header".to_string());
    }
    let mut offset = 0usize;
    while offset + BLOCK <= bytes.len() {
        let header = &bytes[offset..offset + BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        if &header[257..262] != b"ustar" {
            return Err("not a ustar archive".to_string());
        }
        let stored = parse_octal(&header[148..156])?;
        let sum = header_sum(header);
        if stored != u64::from(sum) {
            return Err("archive checksum does not match".to_string());
        }
        let name = cstr(&header[..100]);
        let prefix = cstr(&header[345..500]);
        let full = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if !safe_rel(&full) {
            return Err(format!("refusing archive path `{full}`"));
        }
        let typeflag = header[156];
        let size = parse_octal(&header[124..136])?;
        if size > MAX_ENTRY_BYTES {
            return Err(format!("archive entry `{full}` is larger than the limit"));
        }
        offset += BLOCK;
        let size_us = usize::try_from(size).map_err(|_| "entry size does not fit".to_string())?;
        if offset.saturating_add(size_us) > bytes.len() {
            return Err(format!("archive entry `{full}` is truncated"));
        }
        let data = &bytes[offset..offset + size_us];
        let pad = (BLOCK - (size_us % BLOCK)) % BLOCK;
        offset = offset.saturating_add(size_us).saturating_add(pad);
        if typeflag == b'5' {
            fs::create_dir_all(dest.join(&full)).map_err(|err| err.to_string())?;
            continue;
        }
        if typeflag != b'0' && typeflag != 0 {
            return Err(format!("archive entry `{full}` is not a regular file"));
        }
        let path = dest.join(&full);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        fs::write(&path, data).map_err(|err| format!("writing {}: {err}", path.display()))?;
    }
    Ok(())
}

fn header_sum(header: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (index, byte) in header.iter().take(BLOCK).enumerate() {
        if (148..156).contains(&index) {
            sum += u32::from(b' ');
        } else {
            sum += u32::from(*byte);
        }
    }
    sum
}

fn parse_octal(field: &[u8]) -> Result<u64, String> {
    let text: String = field
        .iter()
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .map(|byte| char::from(*byte))
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| format!("bad octal `{text}`"))
}

fn cstr(field: &[u8]) -> String {
    let end = field.iter().position(|byte| *byte == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).trim().to_string()
}

#[cfg(test)]
fn write_octal(dest: &mut [u8], value: u64) -> Result<(), String> {
    dest.fill(0);
    let digits = dest.len().saturating_sub(1);
    let text = format!("{value:0digits$o}");
    if text.len() > digits {
        return Err("octal field overflow".to_string());
    }
    for byte in dest.iter_mut().take(digits) {
        *byte = b'0';
    }
    let start = digits - text.len();
    dest[start..digits].copy_from_slice(text.as_bytes());
    dest[digits] = 0;
    Ok(())
}

#[cfg(test)]
fn pack_ustar(files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (name, data) in files {
        if !safe_rel(name) || name.len() >= 100 {
            return Err(format!("refusing archive name `{name}`"));
        }
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        write_octal(&mut header[100..108], 0o644)?;
        write_octal(&mut header[108..116], 0)?;
        write_octal(&mut header[116..124], 0)?;
        write_octal(&mut header[124..136], data.len() as u64)?;
        write_octal(&mut header[136..148], 0)?;
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum = header_sum(&header);
        let text = format!("{sum:06o}");
        header[148..154].copy_from_slice(text.as_bytes());
        header[154] = 0;
        header[155] = b' ';
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let pad = (BLOCK - (data.len() % BLOCK)) % BLOCK;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend_from_slice(&[0u8; 1024]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK: &str = r#"(
        format_version: 1,
        steps: [
            Tap(target: "input.confirm", millis: 1),
            Expect(pattern: "player ready", within_millis: 1000, optional: false),
        ],
    )"#;

    fn script_ready() -> &'static str {
        r#"{"on_down":{"28":["player ready"]}}"#
    }

    fn write_tar(dir: &Path, features: &str, script: &str) -> PathBuf {
        let bytes = pack_ustar(&[
            ("features.json", features.as_bytes()),
            ("player", b"player\n"),
            ("script.json", script.as_bytes()),
        ])
        .expect("tar");
        let path = dir.join("player.tar");
        fs::write(&path, bytes).expect("write tar");
        path
    }

    fn request(archive: &Path, out: &Path) -> GateRequest {
        let mut request = GateRequest::new(archive, out);
        request.track_text = Some(TRACK.to_string());
        request.keymap_text = "input.confirm = 28\n".to_string();
        request.within_frames = 4;
        request.token_seed = 7;
        request
    }

    #[test]
    fn installed_player_passes_track() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-pass");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, script_ready());
        let out = dir.join("out");
        let report = run_gate(&request(&archive, &out));
        assert!(report.ok, "{}", report.message);
        assert_eq!(report.exit_code, 0);
        assert!(
            report
                .calls
                .iter()
                .any(|call| call.scancode == 28 && call.edge == Edge::Down)
        );
        assert!(out.join("play_gate.md").is_file());
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(out.join("play_gate.json")).expect("json"))
                .expect("parse");
        assert_eq!(saved["ok"], true);
        assert_eq!(saved["rows"][0]["result"], "PASS");
    }

    #[test]
    fn fresh_home_has_no_prior_config() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-home");
        let prior = dir.join("prior");
        fs::create_dir_all(prior.join(".config")).expect("prior");
        fs::write(prior.join(".config").join("old.toml"), b"secret-prior").expect("prior file");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, script_ready());
        let out = dir.join("out");
        let mut request = request(&archive, &out);
        request.parent_env = vec![
            ("GH_TOKEN".to_string(), "not-a-credential".to_string()),
            ("GITHUB_TOKEN".to_string(), "not-a-credential".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ];
        let report = run_gate(&request);
        assert!(report.ok, "{}", report.message);
        let home = report.home.expect("home");
        assert_ne!(home, prior);
        let config = home.join(".config");
        let names: Vec<_> = fs::read_dir(&config).expect("config").collect();
        assert!(names.is_empty(), "fresh config is empty: {names:?}");
        assert!(!home.join(".config").join("old.toml").exists());
        let walked = walk_text(&home);
        assert!(!walked.contains("secret-prior"));
        assert!(!walked.contains("not-a-credential"));
        let log = fs::read_to_string(out.join("install.log")).expect("log");
        assert!(!log.contains("not-a-credential"));
        assert!(!log.contains("secret-prior"));
        assert!(
            !report
                .env_keys
                .iter()
                .any(|key| key == "GH_TOKEN" || key == "GITHUB_TOKEN")
        );
        assert!(report.env_keys.iter().any(|key| key == "HOME"));
        assert!(report.env_keys.iter().any(|key| key == "XDG_CONFIG_HOME"));
        assert!(home.join(".cache").is_dir());
        assert!(home.join(".local").join("share").is_dir());
        assert!(home.join(".local").join("state").is_dir());
        assert!(home.join("run").is_dir());
    }

    #[test]
    fn a_physical_key_not_text_reaches_the_player() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-key");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, script_ready());
        let report = run_gate(&request(&archive, &dir.join("out")));
        assert!(report.ok, "{}", report.message);
        let down = report
            .calls
            .iter()
            .find(|call| call.edge == Edge::Down)
            .expect("down");
        assert_eq!(down.scancode, 28);
        assert!(down.text.is_none());
        assert_eq!(down.method, INJECT_METHOD);
        assert_eq!(down.token, report.token.clone().expect("token"));
        assert!(report.calls.iter().all(|call| call.text.is_none()));
        assert!(report.calls.iter().all(|call| call.token == down.token));
        assert!(!report.calls.iter().any(|call| call.method.contains("capture")));
    }

    #[test]
    fn a_missing_log_line_is_named_in_the_report() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-miss");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, r#"{"on_down":{}}"#);
        let report = run_gate(&request(&archive, &dir.join("out")));
        assert!(!report.ok);
        assert_eq!(report.exit_code, 1);
        assert_eq!(report.code.as_deref(), Some(CODE_EXPECT_MISSED));
        let row = report
            .rows
            .iter()
            .find(|row| row.result == "FAIL")
            .expect("fail row");
        assert!(row.evidence.contains("player ready"), "{}", row.evidence);
        assert!(row.step.contains("player ready"), "{}", row.step);
    }

    #[test]
    fn a_build_without_the_agent_is_refused_not_backdoored() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-no-agent");
        let script = script_ready();
        let archive = write_tar(&dir, r#"{"features":["input"]}"#, script);
        let out = dir.join("out");
        let report = run_gate(&request(&archive, &out));
        assert!(!report.ok);
        assert_eq!(report.code.as_deref(), Some(CODE_NO_AGENT));
        assert_eq!(report.exit_code, 1);
        assert!(report.calls.is_empty(), "no injection without the agent feature");
        assert!(report.token.is_none());
        let package = report.package_dir.expect("package");
        let saved = fs::read_to_string(package.join("script.json")).expect("script");
        assert_eq!(saved, script);
        assert!(!out.join("inject.txt").exists());
        assert!(!package.join("inject.txt").exists());
    }

    #[test]
    fn the_driver_never_sends_text_or_a_capture_flag() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-capture");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, script_ready());
        let report = run_gate(&request(&archive, &dir.join("out")));
        assert!(!report.calls.is_empty());
        for call in &report.calls {
            assert!(call.text.is_none());
            assert_eq!(call.method, "play.inject_input");
            assert!(!format!("{call:?}").contains("play-capture"));
        }
    }

    #[test]
    fn a_named_key_is_refused_instead_of_sent_as_text() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-name");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, script_ready());
        let out = dir.join("out");
        let mut request = request(&archive, &out);
        request.keymap_text = "input.confirm = Space\n".to_string();
        let report = run_gate(&request);
        assert_eq!(report.exit_code, 2);
        assert_eq!(report.code.as_deref(), Some(CODE_KEYMAP));
        assert!(report.calls.is_empty());
        assert!(!out.join("home").exists());
    }

    #[test]
    fn run_without_fresh_home_creates_nothing() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-nofresh");
        let archive = write_tar(&dir, r#"{"features":["agent"]}"#, script_ready());
        let out = dir.join("out");
        let mut request = request(&archive, &out);
        request.fresh_home = false;
        let report = run_gate(&request);
        assert_eq!(report.exit_code, 2);
        assert_eq!(report.code.as_deref(), Some(CODE_FRESH_HOME));
        assert!(!out.join("home").exists());
        assert!(out.join("play_gate.json").is_file());
    }

    #[test]
    fn failed_install_skips_play() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-fail-install");
        let out = dir.join("out");
        let report = run_gate(&request(&dir.join("missing.tar"), &out));
        assert!(!report.ok);
        assert_eq!(report.exit_code, 1);
        assert_eq!(report.install_rc, Some(1));
        assert!(report.calls.is_empty());
        assert_eq!(report.rows[0].evidence, "not run: install failed");
    }

    #[test]
    fn a_directory_package_installs_into_the_fresh_home() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-dir");
        let package = dir.join("pkg");
        fs::create_dir_all(&package).expect("pkg");
        fs::write(package.join("features.json"), r#"{"features":["agent"]}"#).expect("features");
        fs::write(package.join("player"), b"player\n").expect("player");
        fs::write(package.join("script.json"), script_ready()).expect("script");
        let report = run_gate(&request(&package, &dir.join("out")));
        assert!(report.ok, "{}", report.message);
        let installed = report.package_dir.expect("installed");
        assert!(installed.ends_with("regia-play/player"));
        assert!(installed.join("player").is_file());
    }

    #[test]
    fn keymap_appends_setup_done_unless_the_file_sets_it() {
        let added = installed_keymap("input.confirm = 28\n");
        assert!(added.contains("input.setup_done = 1"));
        let pinned = installed_keymap("input.setup_done = 0\ninput.confirm = 28\n");
        assert!(!pinned.contains("input.setup_done = 1"));
        assert!(keymap_sets_setup_done("input.setup_done = 0\n"));
        assert!(!keymap_sets_setup_done("# input.setup_done = 1\n"));
    }

    #[test]
    fn pass_rows_and_a_missing_middle_step_is_named() {
        let steps = vec![
            expect_step("step_0", r"step 0 done", None),
            expect_step("marked", r"step 2 done", None),
            expect_step("step_3", r"step 3 done", None),
        ];
        let full = log_lines(&["step 0 done", "step 2 done", "step 3 done"]);
        let rows = grade_log(&full, &steps, None, None).expect("grade");
        assert!(rows.iter().all(|row| row.result == "PASS"));
        let dropped = log_lines(&["step 0 done", "step 3 done"]);
        let rows = grade_log(&dropped, &steps, None, None).expect("grade");
        assert_eq!(row_named(&rows, "step_0").result, "PASS");
        assert_eq!(row_named(&rows, "marked").result, "FAIL");
        assert!(row_named(&rows, "marked").evidence.contains("step 2 done"));
        assert_eq!(row_named(&rows, "step_3").result, "PASS");
    }

    #[test]
    fn key_sent_lines_do_not_pass_a_step() {
        let steps = vec![expect_step("marked", r"step 2 done", None)];
        let lines = log_lines(&["tap key:Enter", "key sent input.jump"]);
        let rows = grade_log(&lines, &steps, None, None).expect("grade");
        let marked = row_named(&rows, "marked");
        assert_eq!(marked.result, "FAIL");
        assert!(!marked.evidence.contains("key sent"));
    }

    #[test]
    fn a_line_before_its_anchor_does_not_count() {
        let steps = vec![
            expect_step("done", r"run complete", None),
            expect_step("exit_menu", r"page Main\b", Some("done")),
        ];
        let lines = log_lines(&["page Main", "run complete"]);
        let rows = grade_log(&lines, &steps, None, None).expect("grade");
        assert_eq!(row_named(&rows, "done").result, "PASS");
        assert_eq!(row_named(&rows, "exit_menu").result, "FAIL");
    }

    #[test]
    fn a_match_before_the_predecessor_does_not_count() {
        let steps = vec![
            expect_step("build_marker", r"queued marker", None),
            expect_step("deploy_marker", r"deployed", Some("build_marker")),
        ];
        let lines = log_lines(&["deployed 1", "queued marker"]);
        let rows = grade_log(&lines, &steps, None, None).expect("grade");
        assert_eq!(row_named(&rows, "build_marker").result, "PASS");
        assert_eq!(row_named(&rows, "deploy_marker").result, "FAIL");
    }

    #[test]
    fn ok_text_that_mentions_err_is_not_an_error() {
        let pattern = r"menu-apply:\s*Err\b";
        let ok_line = FrameLog {
            frame: 0,
            text: r#"menu-apply: Ok("see Err later") jobs=1"#.to_string(),
        };
        let err_line = FrameLog {
            frame: 0,
            text: r#"menu-apply: Err("missing") jobs=0"#.to_string(),
        };
        let steps = vec![expect_step("menu", "menu-apply", None)];
        let rows = grade_log(std::slice::from_ref(&ok_line), &steps, None, Some(pattern)).expect("grade");
        assert_eq!(row_named(&rows, "error_line").result, "PASS");
        let rows = grade_log(&[ok_line, err_line], &steps, None, Some(pattern)).expect("grade");
        let error = row_named(&rows, "error_line");
        assert_eq!(error.result, "FAIL");
        assert!(error.evidence.contains(r#"Err("missing")"#), "{}", error.evidence);
    }

    #[test]
    fn a_missing_frame_is_a_note_not_a_fail() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-frame");
        let frames = dir.join("frames");
        fs::create_dir_all(&frames).expect("frames");
        fs::write(frames.join("0004_t0012.0_marked.png"), b"").expect("png");
        let mut marked = expect_step("marked", "step 2 done", None);
        marked.frame_tag = Some("marked".to_string());
        let mut done = expect_step("done", "run complete", None);
        done.frame_tag = Some("done".to_string());
        let lines = log_lines(&["step 2 done", "run complete"]);
        let rows = grade_log(&lines, &[marked, done], Some(&frames), None).expect("grade");
        let marked = row_named(&rows, "marked");
        assert_eq!(marked.result, "PASS");
        assert!(marked.frame.ends_with("_marked.png"), "{}", marked.frame);
        let done = row_named(&rows, "done");
        assert_eq!(done.result, "PASS");
        assert!(done.evidence.contains("frame missing"), "{}", done.evidence);
        assert_eq!(done.frame, "");
    }

    #[test]
    fn markdown_escapes_pipes_and_json_keeps_them() {
        let dir = regia_test_utils::unique_temp_dir("play-gate-md");
        let report = PlayReport {
            ok: false,
            code: Some(CODE_EXPECT_MISSED.to_string()),
            message: "check".to_string(),
            exit_code: 1,
            home: None,
            package_dir: None,
            install_rc: None,
            token: None,
            env_keys: Vec::new(),
            install_budget: DEFAULT_INSTALL_BUDGET,
            second_play_frames: DEFAULT_SECOND_PLAY_FRAMES,
            play_wait_frames: DEFAULT_PLAY_WAIT_FRAMES,
            rows: vec![GateRow {
                scenario: "exercise_1".to_string(),
                step: "menu".to_string(),
                result: "FAIL".to_string(),
                evidence: "a|b".to_string(),
                frame: String::new(),
            }],
            calls: Vec::new(),
            notes: Vec::new(),
        };
        let text = render_markdown(&report);
        assert!(text.contains(r"a\|b"), "{text}");
        assert!(text.contains("| exercise_1 | menu | FAIL |"), "{text}");
        write_reports(&dir, &report).expect("write");
        let saved: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("play_gate.json")).expect("json"))
                .expect("parse");
        assert_eq!(saved["ok"], false);
        assert_eq!(saved["rows"][0]["evidence"], "a|b");
    }

    #[test]
    fn evidence_is_clipped_at_the_default_limit() {
        let long = "x".repeat(300);
        assert_eq!(clip(&long).chars().count(), EVIDENCE_CLIP);
        assert!(clip(&long).ends_with("..."));
    }

    #[test]
    fn an_archive_path_that_escapes_is_refused() {
        let mut header = [0u8; BLOCK];
        let name = b"../outside";
        header[..name.len()].copy_from_slice(name);
        write_octal(&mut header[100..108], 0o644).expect("mode");
        write_octal(&mut header[124..136], 1).expect("size");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let sum = header_sum(&header);
        let text = format!("{sum:06o}");
        header[148..154].copy_from_slice(text.as_bytes());
        header[154] = 0;
        header[155] = b' ';
        let mut bytes = header.to_vec();
        bytes.push(b'Z');
        bytes.extend(std::iter::repeat_n(0u8, 511));
        bytes.extend_from_slice(&[0u8; 1024]);
        let dir = regia_test_utils::unique_temp_dir("play-gate-escape");
        let err = unpack_ustar(&bytes, &dir).expect_err("escape");
        assert!(err.contains("refusing"), "{err}");
        assert!(!dir.join("outside").exists());
    }

    fn expect_step(step: &str, pattern: &str, after: Option<&str>) -> GateExpect {
        GateExpect {
            scenario: "track".to_string(),
            step: step.to_string(),
            patterns: vec![pattern.to_string()],
            within_frames: 4,
            after: after.map(str::to_string),
            frame_tag: None,
        }
    }

    fn log_lines(lines: &[&str]) -> Vec<FrameLog> {
        lines
            .iter()
            .map(|text| FrameLog {
                frame: 0,
                text: (*text).to_string(),
            })
            .collect()
    }

    fn row_named<'a>(rows: &'a [GateRow], step: &str) -> &'a GateRow {
        rows.iter()
            .find(|row| row.step == step)
            .unwrap_or_else(|| panic!("missing {step}"))
    }

    fn walk_text(root: &Path) -> String {
        let mut out = String::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(text) = fs::read_to_string(&path) {
                    out.push_str(&text);
                }
            }
        }
        out
    }
}
