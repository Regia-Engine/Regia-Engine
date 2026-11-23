// SPDX-License-Identifier: MPL-2.0
//! PASS / FAIL / N-A rows for `cargo xtask smoke`, and the JSON and Markdown summaries.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use regia_render::HitchReport;
#[cfg(test)]
use regia_render::{FramePathCounters, HitchFrame, HitchTargets};

/// One alpha area's engine-side verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Row number. 1–18 are the alpha smoke list. 19–21 are the v0.1.5 scene rows
    /// (ultrawide, black sky, many units).
    pub id: u8,
    /// Area name.
    pub area: String,
    /// `PASS`, `FAIL`, `N-A` or `REPORT`.
    pub status: Status,
    /// Number, log line or screenshot path.
    pub evidence: String,
    /// One line: why the row has that status.
    pub reason: String,
}

/// Verdict printed for one area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Status {
    /// The engine-side criterion holds.
    #[serde(rename = "PASS")]
    Pass,
    /// The engine-side criterion does not hold.
    #[serde(rename = "FAIL")]
    Fail,
    /// This row is not an engine check, or the GPU session was skipped.
    #[serde(rename = "N-A")]
    Na,
    /// The row was measured and printed. It does not fail the smoke run.
    ///
    /// Timing rows use this. A load spike on a shared machine is not a release
    /// failure; the cargo test for that measurement is the gate.
    #[serde(rename = "REPORT")]
    Report,
}

impl Status {
    /// Terminal and Markdown text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Na => "N-A",
            Self::Report => "REPORT",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The whole smoke run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SmokeReport {
    /// ISO-like local timestamp the run finished.
    pub finished: String,
    /// Wall time of the command, seconds.
    pub elapsed_s: f64,
    /// Adapter the GPU session used, or `skipped`.
    pub gpu: String,
    /// One row per alpha area, then the v0.1.5 scene rows, in list order.
    pub checks: Vec<Check>,
}

impl SmokeReport {
    /// True when any row failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }

    /// Counts of each status: pass, fail, n/a, report.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut pass = 0;
        let mut fail = 0;
        let mut na = 0;
        let mut report = 0;
        for check in &self.checks {
            match check.status {
                Status::Pass => pass += 1,
                Status::Fail => fail += 1,
                Status::Na => na += 1,
                Status::Report => report += 1,
            }
        }
        (pass, fail, na, report)
    }

    /// The terminal table and the one-line total.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "alpha smoke  gpu: {}  elapsed: {:.1} s\n",
            self.gpu, self.elapsed_s
        ));
        for check in &self.checks {
            out.push_str(&format!(
                "{:>2}  {:<16}  {:<4}  {}  — {}\n",
                check.id,
                check.area,
                check.status,
                one_line(&check.evidence),
                one_line(&check.reason)
            ));
        }
        let (pass, fail, na, report) = self.counts();
        out.push_str(&format!(
            "smoke: {pass} pass, {fail} fail, {na} n/a, {report} report\n"
        ));
        out
    }

    /// Markdown summary.
    #[must_use]
    pub fn markdown(&self) -> String {
        let (pass, fail, na, report) = self.counts();
        let mut out = String::new();
        out.push_str("# DO27 alpha engine smoke\n\n");
        out.push_str(&format!(
            "Finished {finished}. Elapsed {elapsed:.1} s. GPU: {gpu}.\n\n{pass} pass, {fail} fail, {na} n/a, {report} report.\n\n",
            finished = self.finished,
            elapsed = self.elapsed_s,
            gpu = self.gpu,
        ));
        out.push_str("| # | Area | Status | Evidence | Reason |\n|---|---|---|---|---|\n");
        for check in &self.checks {
            out.push_str(&format!(
                "| {id} | {area} | {status} | {evidence} | {reason} |\n",
                id = check.id,
                area = check.area,
                status = check.status,
                evidence = table_cell(&check.evidence),
                reason = table_cell(&check.reason),
            ));
        }
        out.push('\n');
        out
    }

    /// Pretty JSON.
    ///
    /// # Errors
    ///
    /// Serialization failed.
    pub fn json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Writes `alpha-smoke.json` and `alpha-smoke.md` under `dir`.
///
/// # Errors
///
/// The directory could not be created or a file could not be written.
pub fn write_summary(dir: &Path, report: &SmokeReport) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let json = report.json()?;
    let json_path = dir.join("alpha-smoke.json");
    let md_path = dir.join("alpha-smoke.md");
    std::fs::write(&json_path, json).with_context(|| format!("writing {}", json_path.display()))?;
    std::fs::write(&md_path, report.markdown()).with_context(|| format!("writing {}", md_path.display()))?;
    Ok(())
}

/// Collapses whitespace so a row stays one line.
#[must_use]
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn table_cell(text: &str) -> String {
    one_line(text).replace('|', "\\|")
}

/// First present must be under one second (alpha boot / ready budget).
pub const BOOT_BUDGET_MS: f64 = 1_000.0;

/// Desktop High process RSS cap from the alpha list (3 GiB).
pub const RSS_BUDGET_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// `true` when `ms` is inside the one-second boot budget.
#[must_use]
pub fn within_boot_budget(ms: f64) -> bool {
    ms.is_finite() && ms < BOOT_BUDGET_MS
}

/// `true` when process RSS is at or under [`RSS_BUDGET_BYTES`].
#[must_use]
pub fn rss_within_budget(bytes: u64) -> bool {
    bytes <= RSS_BUDGET_BYTES
}

/// Hitch rows the alpha list holds the engine to, taken from [`HitchReport::failures`]
/// plus buffer and texture creates (those two are counted on the report but not listed
/// by `failures`).
///
/// Gated: no measured frames, frames over 50 ms, pipeline / bind-group / buffer / texture
/// creates, blocking polls, and inline sky bakes. The § 2.4 ratio rows stay in
/// [`HitchReport::summary`] so a busy box does not fail the alpha list on p99/p50.
#[must_use]
pub fn hitch_failures(report: &HitchReport) -> Vec<String> {
    if report.frames == 0 {
        return vec!["no measured frames".to_owned()];
    }
    let mut out: Vec<String> = report
        .failures()
        .into_iter()
        .filter(|line| {
            line.contains("pipeline creations")
                || line.contains("bind-group creations")
                || line.contains("blocking polls")
                || line.contains("sky bakes")
                || line.contains("frames over") && line.contains(" ms")
        })
        .collect();
    let counters = report.counters;
    if counters.buffers > 0 {
        out.push(format!(
            "{} buffer creations after warm-up (target 0)",
            counters.buffers
        ));
    }
    if counters.textures > 0 {
        out.push(format!(
            "{} texture creations after warm-up (target 0)",
            counters.textures
        ));
    }
    out
}

/// RGB luminance of one frame, alpha ignored.
///
/// Luminance is the BT.709 weights on the stored 8-bit channels (a display-referred
/// check, not a linear-light meter).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RgbFrame {
    /// Pixels sampled (RGB triples).
    pub pixels: usize,
    /// Mean BT.709 luminance, 0–255.
    pub mean: f32,
    /// Standard deviation of that luminance.
    pub stddev: f32,
    /// Share of pixels with luminance under 28 (a bore, a shadow, a hole).
    pub dark: f32,
    /// Share of pixels with luminance from 28 up to 210 (lit ground, rock, hull).
    pub mid: f32,
    /// Share of pixels with luminance at least 250 (a clipped sky).
    pub near_white: f32,
}

/// Luminance stats of tightly packed RGBA8. Alpha is not read.
#[must_use]
pub fn rgb_frame(pixels: &[u8]) -> RgbFrame {
    let mut count = 0usize;
    let mut sum = 0.0f64;
    let mut square = 0.0f64;
    let mut dark = 0usize;
    let mut mid = 0usize;
    let mut near_white = 0usize;
    for chunk in pixels.as_chunks::<4>().0 {
        let lum = 0.2126 * f64::from(chunk[0]) + 0.7152 * f64::from(chunk[1]) + 0.0722 * f64::from(chunk[2]);
        sum += lum;
        square += lum * lum;
        count += 1;
        if lum < 28.0 {
            dark += 1;
        } else if lum < 210.0 {
            mid += 1;
        }
        if lum >= 250.0 {
            near_white += 1;
        }
    }
    let n = count.max(1) as f64;
    let mean = sum / n;
    let var = (square / n - mean * mean).max(0.0);
    RgbFrame {
        pixels: count,
        mean: mean as f32,
        stddev: var.sqrt() as f32,
        dark: dark as f32 / n as f32,
        mid: mid as f32 / n as f32,
        near_white: near_white as f32 / n as f32,
    }
}

/// A presented frame with real colour: not empty, not flat, not a clipped white field.
///
/// An opaque black buffer used to pass when any byte, including alpha, was non-zero.
#[must_use]
pub fn frame_has_colour(frame: &RgbFrame) -> bool {
    frame.pixels >= 16
        && frame.stddev >= 8.0
        && frame.mean > 4.0
        && frame.mean < 245.0
        && frame.near_white < 0.85
}

/// The Switchbacks mouth plate: a dark opening and a lit surround, not a flat field.
#[must_use]
pub fn frame_shows_mouth(frame: &RgbFrame) -> bool {
    frame_has_colour(frame) && frame.dark >= 0.02 && frame.mid >= 0.08
}

/// A hitch report built from wall times and device-call counters.
#[cfg(test)]
#[must_use]
fn hitch_from_frames(frames: &[HitchFrame]) -> HitchReport {
    HitchReport::new(frames, HitchTargets::SPEC)
}

/// A measured frame with no device calls. Test helper for the hitch gate.
#[cfg(test)]
#[must_use]
fn quiet_frame(frame_ms: f32) -> HitchFrame {
    HitchFrame {
        frame_ms,
        cpu_ms: frame_ms,
        gpu_ms: 0.0,
        counters: FramePathCounters::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SmokeReport {
        SmokeReport {
            finished: "2026-10-09T00:00:00".into(),
            elapsed_s: 1.5,
            gpu: "skipped".into(),
            checks: vec![Check {
                id: 14,
                area: "Hulls".into(),
                status: Status::Na,
                evidence: "content".into(),
                reason: "not an engine check".into(),
            }],
        }
    }

    #[test]
    fn markdown_and_json_name_every_status() {
        let report = sample();
        let md = report.markdown();
        assert!(md.contains("| 14 | Hulls | N-A | content | not an engine check |"));
        let json = report.json().expect("json");
        let back: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(back["checks"][0]["status"], "N-A");
        assert!(!report.failed());
        assert_eq!(report.counts(), (0, 0, 1, 0));
    }

    #[test]
    fn a_report_row_does_not_fail_the_run() {
        let mut report = sample();
        report.checks.push(Check {
            id: 21,
            area: "Many units".into(),
            status: Status::Report,
            evidence: "encode p50 1.20 ms".into(),
            reason: "report only; the cargo test gates the slack".into(),
        });
        assert!(!report.failed(), "a report row is not a smoke failure");
        assert_eq!(report.counts(), (0, 0, 1, 1));
        assert!(report.text().contains("REPORT"));
        assert!(report.markdown().contains("| 21 | Many units | REPORT |"));
        let json = report.json().expect("json");
        assert!(json.contains("\"REPORT\""));
    }

    #[test]
    fn boot_budget_is_under_one_second() {
        assert!(within_boot_budget(999.0));
        assert!(!within_boot_budget(1000.0));
        assert!(!within_boot_budget(f64::NAN));
        assert!(rss_within_budget(RSS_BUDGET_BYTES));
        assert!(!rss_within_budget(RSS_BUDGET_BYTES + 1));
    }

    #[test]
    fn hitch_gate_is_frames_over_50ms_and_creates() {
        let quiet = hitch_from_frames(&[quiet_frame(8.0), quiet_frame(9.0), quiet_frame(11.0)]);
        assert!(hitch_failures(&quiet).is_empty(), "{}", quiet.summary());
        let mut slow = quiet_frame(8.0);
        slow.frame_ms = 80.0;
        let over = hitch_from_frames(&[quiet_frame(8.0), slow, quiet_frame(9.0)]);
        let failures = hitch_failures(&over);
        assert!(failures.iter().any(|f| f.contains("over 50")), "{failures:?}");
        let mut created = quiet_frame(8.0);
        created.counters.pipelines = 1;
        created.counters.bind_groups = 2;
        created.counters.blocking_polls = 3;
        created.counters.buffers = 4;
        created.counters.textures = 5;
        created.counters.inline_bakes = 1;
        let creates = hitch_from_frames(&[quiet_frame(8.0), created]);
        let failures = hitch_failures(&creates);
        assert!(failures.iter().any(|f| f.contains("pipeline")), "{failures:?}");
        assert!(failures.iter().any(|f| f.contains("bind-group")), "{failures:?}");
        assert!(failures.iter().any(|f| f.contains("blocking")), "{failures:?}");
        assert!(failures.iter().any(|f| f.contains("buffer")), "{failures:?}");
        assert!(failures.iter().any(|f| f.contains("texture")), "{failures:?}");
        assert!(failures.iter().any(|f| f.contains("sky bakes")), "{failures:?}");
    }

    #[test]
    fn rgb_frame_ignores_alpha_and_rejects_a_flat_field() {
        let black: Vec<u8> = [0, 0, 0, 255].repeat(64);
        let flat = rgb_frame(&black);
        assert!(!frame_has_colour(&flat), "{flat:?}");
        assert_eq!(flat.mean, 0.0);
        let white: Vec<u8> = [255, 255, 255, 255].repeat(64);
        assert!(!frame_has_colour(&rgb_frame(&white)));
        let mut varied = Vec::new();
        for i in 0..64 {
            let c = (i * 4) as u8;
            varied.extend_from_slice(&[c, 40, 20, 255]);
        }
        let frame = rgb_frame(&varied);
        assert!(frame_has_colour(&frame), "{frame:?}");
        assert!(frame.stddev >= 8.0, "{frame:?}");
    }

    #[test]
    fn one_line_flattens_evidence() {
        assert_eq!(one_line("a\nb  c"), "a b c");
    }
}
