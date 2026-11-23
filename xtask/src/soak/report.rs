// SPDX-License-Identifier: MPL-2.0
//! Soak session and summary reports: JSON (checked by `cargo xtask budgets check`) and Markdown
//! (for review under `docs/evidence/`). Timestamps are RFC 3339 UTC.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::soak::budgets::{Measured, Violation};
use crate::soak::stats::Percentiles;

/// Rounds to milliseconds precision for readable reports.
pub(crate) fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// RFC 3339 UTC timestamp of now, second precision. The date comes from the civil-from-days
/// algorithm (Howard Hinnant, "chrono-Compatible Algorithms", 2021, public domain; see
/// `docs/provenance.md`), avoiding a date-library dependency for one format call.
pub fn utc_now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// (year, month, day) of a day count relative to 1970-01-01. Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_app_is_a_crash_and_the_expected_stop_is_not() {
        // A session that stopped because the app quit answering used to end as "killed by the
        // tool (expected)" and pass.
        assert!(AppExitKind::Froze.is_crash(false));
        assert!(AppExitKind::ExitedEarly.is_crash(false));
        assert!(AppExitKind::DidNotStop.is_crash(false));
        assert!(!AppExitKind::KilledByTool.is_crash(false));
        assert!(AppExitKind::KilledByTool.is_crash(true));
        assert!(!AppExitKind::NoEndpoint.is_crash(false));
    }

    #[test]
    fn epoch_formats() {
        assert_eq!(utc_from_unix(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_dates() {
        // Day numbers computed with `date -u -d <date> +%s` / 86400.
        assert_eq!(utc_from_unix(20_730 * 86_400), "2026-10-04T00:00:00Z");
        // Leap-year day: 2024-02-29.
        assert_eq!(utc_from_unix(19_782 * 86_400), "2024-02-29T00:00:00Z");
        // Year boundary: 2025-12-31 → 2026-01-01.
        assert_eq!(utc_from_unix(20_453 * 86_400), "2025-12-31T00:00:00Z");
        assert_eq!(utc_from_unix(20_454 * 86_400), "2026-01-01T00:00:00Z");
    }

    fn utc_from_unix(secs: u64) -> String {
        let days = (secs / 86_400) as i64;
        let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
        let (y, mo, d) = civil_from_days(days);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
    }

    #[test]
    fn old_p99_ms_field_maps_to_frame_ms() {
        // `docs/evidence/m18/session-1.json` predates the rename of the wall-clock frame-time
        // series from `p99_ms` to `frame_ms`; the serde alias keeps that evidence parseable so
        // `cargo xtask budgets check` still reads its frame times instead of dropping them.
        let json = r#"{
            "frames_first": 3,
            "frames_last": 1193,
            "avg_ms": null,
            "p99_ms": { "samples": 220, "p50": 127.2, "p95": 175.3, "p99": 175.3, "max": 175.3, "mean": 135.1 },
            "rss_max_mb": 186.063,
            "gpu_memory_mb": null,
            "entity_count": [125, 125, 125],
            "draw_calls_max": 0,
            "allocated_bytes_max": null
        }"#;
        let frames: FrameStats = serde_json::from_str(json).expect("old FrameStats shape parses");
        let series = frames.frame_ms.expect("p99_ms aliases to frame_ms");
        assert_eq!(series.p99, 175.3);
        assert_eq!(series.samples, 220);
        assert!(
            frames.rss_first_mb.is_none(),
            "absent rss_first_mb defaults to None"
        );
        assert!(
            frames.rss_last_mb.is_none(),
            "absent rss_last_mb defaults to None"
        );
    }
}

/// How the app process ended at the close of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppExitKind {
    /// The tool terminated the app after the requested duration — the expected end.
    KilledByTool,
    /// The app exited by itself before the duration ran out (unexpected in a soak).
    ExitedEarly,
    /// The app never advertised an agent endpoint; nothing could be sampled.
    NoEndpoint,
    /// The app survived the tool's terminate request (hang).
    DidNotStop,
    /// The app stopped answering the agent protocol mid-session (several failed calls in a row);
    /// the tool ended the session early and terminated it. A hang, so the session fails.
    Froze,
}

impl AppExitKind {
    /// Whether this end, or a panic in the process output, makes the session a crash.
    pub fn is_crash(self, panicked: bool) -> bool {
        panicked
            || matches!(
                self,
                AppExitKind::ExitedEarly | AppExitKind::DidNotStop | AppExitKind::Froze
            )
    }
}

/// Crash bookkeeping of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrashInfo {
    /// True when the app died early, froze, ignored termination, or panicked.
    pub crashed: bool,
    /// How the process ended.
    pub exit: AppExitKind,
    /// Seconds into the session the app died, when it died early.
    pub at_secs: Option<f64>,
    /// Exit code, when the app exited by itself.
    pub exit_code: Option<i32>,
    /// First panic line found in the captured process output, when any.
    pub panic_text: Option<String>,
}

/// Connection facts recorded once `agent.hello` answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectInfo {
    /// Seconds from process start to the first `agent.hello` answer (the cold-start budget).
    pub start_secs: f64,
    /// Protocol version the app reported.
    pub protocol: String,
    /// App name the app reported.
    pub app: String,
    /// OS the app reported.
    pub os: String,
    /// Process id of the app (as advertised in the endpoint file).
    pub pid: u32,
    /// How many protocol methods the app reported.
    pub methods: usize,
}

/// Percentile summary of one sample series, in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesStats {
    /// Number of samples behind the summary.
    pub samples: usize,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Largest sample.
    pub max: f64,
    /// Arithmetic mean.
    pub mean: f64,
}

impl SeriesStats {
    /// Summarises `samples` count and `p` (values already in ms).
    pub fn new(samples: usize, p: &Percentiles) -> Self {
        Self {
            samples,
            p50: round3(p.p50),
            p95: round3(p.p95),
            p99: round3(p.p99),
            max: round3(p.max),
            mean: round3(p.mean),
        }
    }

    fn line(&self) -> String {
        format!(
            "p50 {:.3} / p95 {:.3} / p99 {:.3} / max {:.3} / mean {:.3} ms ({} samples)",
            self.p50, self.p95, self.p99, self.max, self.mean, self.samples
        )
    }
}

/// Frame/memory counters of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameStats {
    /// First `frames` value the diagnostics reported, when it ever did.
    pub frames_first: Option<u64>,
    /// Last `frames` value the diagnostics reported.
    pub frames_last: Option<u64>,
    /// Windowed average frame times (`frame_time_avg_ms`), post-warmup.
    pub avg_ms: Option<SeriesStats>,
    /// Wall-clock frame times (ms) derived from the frame counter and the wall clock, post-warmup
    /// (`1000 · Δwall / Δframes` per diagnostics sample). Unclamped by the app's `MAX_DELTA`
    /// (250 ms), so — unlike `fps_1pct_low` — it is an honest frame-time series on a slow,
    /// software-rasterised run; percentiles are interval averages over the sampling window.
    /// `p99_ms` is the historical name of this series (older evidence files); the alias keeps them
    /// parseable.
    #[serde(alias = "p99_ms")]
    pub frame_ms: Option<SeriesStats>,
    /// Peak resident set size in MB.
    pub rss_max_mb: Option<f64>,
    /// First RSS sample after warmup, in MB (the leak-check baseline).
    #[serde(default)]
    pub rss_first_mb: Option<f64>,
    /// Last RSS sample of the session, in MB.
    #[serde(default)]
    pub rss_last_mb: Option<f64>,
    /// GPU memory in MB — `None` until the protocol exposes it.
    pub gpu_memory_mb: Option<f64>,
    /// Entity count range: `[min, max, last]`, when diagnostics ever answered.
    pub entity_count: Option<[u64; 3]>,
    /// Largest draw-call count seen.
    pub draw_calls_max: Option<u64>,
    /// Largest heap allocation reported by the counting allocator, when installed.
    pub allocated_bytes_max: Option<u64>,
}

/// Log counting of one session (via `logs.read`, records at `warn` or above).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogCounts {
    /// Error-level records.
    pub errors: u64,
    /// Warn-level records.
    pub warns: u64,
    /// Total records read (all warn-and-above).
    pub records_read: u64,
}

/// Input-script driving of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputStats {
    /// Script entries the schedule contains (one entry is one or more raw events).
    pub entries: u64,
    /// Entries delivered in accepted `play.inject_input` calls (entries fire on the sampling
    /// tick that contains their time; a tick's entries share one call).
    pub injected: u64,
    /// Entries lost to failed calls (app error or timeout).
    pub errors: u64,
}

/// One soak session's full report; `measured` is the canonical input of the budgets check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReport {
    /// 1-based session index.
    pub session: u32,
    /// Executable name (without directories, so reports don't leak machine paths).
    pub exe: String,
    /// Arguments the app was launched with.
    pub app_args: Vec<String>,
    /// Whether the app ran on a visible window (true) or the tool's virtual display (false).
    pub windowed: bool,
    /// RFC 3339 UTC timestamp of the session start.
    pub started_utc: String,
    /// Requested session duration in seconds.
    pub duration_secs: f64,
    /// Warmup seconds excluded from frame-time statistics.
    pub warmup_secs: f64,
    /// Diagnostics samples per second the session targeted.
    pub sample_hz: f64,
    /// Diagnostics samples actually taken (including warmup).
    pub samples: usize,
    /// Connection facts, or `None` when the app never answered.
    pub connect: Option<ConnectInfo>,
    /// Frame and memory counters.
    pub frames: FrameStats,
    /// Log counts.
    pub logs: LogCounts,
    /// Input-script statistics.
    pub input: InputStats,
    /// Protocol calls that failed or timed out (connection hiccups, a frozen app).
    pub protocol_errors: u64,
    /// Crash bookkeeping.
    pub crash: CrashInfo,
    /// The canonical measured values every budget is checked against.
    pub measured: Measured,
    /// Budget breaches.
    pub violations: Vec<Violation>,
    /// Session verdict: no violations, no crash, and a working connection.
    pub passed: bool,
}

/// The summary over all sessions of one soak run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryReport {
    /// RFC 3339 UTC timestamp of the run start.
    pub started_utc: String,
    /// Executable name.
    pub exe: String,
    /// Requested minutes per session.
    pub minutes: f64,
    /// Number of sessions.
    pub sessions_count: u32,
    /// Sessions in run order.
    pub sessions: Vec<SessionReport>,
    /// True when every session passed.
    pub passed: bool,
}

impl SessionReport {
    /// Renders the Markdown review form of the session.
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        let verdict = if self.passed { "PASS" } else { "FAIL" };
        md.push_str(&format!(
            "# Soak session {} — {}\n\n**Verdict: {verdict}** — {} violation(s), {}.\n\n",
            self.session,
            self.exe,
            self.violations.len(),
            if self.crash.crashed {
                "app crashed"
            } else {
                "no crash"
            }
        ));
        md.push_str(&format!(
            "- started {} UTC, {:.0} s requested, {}, {:.1} Hz sampling, {} diagnostics samples\n",
            self.started_utc,
            self.duration_secs,
            if self.windowed { "windowed" } else { "headless" },
            self.sample_hz,
            self.samples
        ));
        match &self.connect {
            Some(c) => md.push_str(&format!(
                "- connect: {:.3} s to first `agent.hello` (protocol {}, pid {}, {} methods)\n",
                c.start_secs, c.protocol, c.pid, c.methods
            )),
            None => md.push_str("- connect: **never answered** (no endpoint)\n"),
        }
        if let (Some(first), Some(last)) = (self.frames.frames_first, self.frames.frames_last) {
            md.push_str(&format!("- frames rendered: {first} → {last}\n"));
        } else {
            md.push_str("- frames rendered: unknown (no diagnostics answers)\n");
        }
        match &self.frames.avg_ms {
            Some(s) => md.push_str(&format!("- frame time avg, windowed: {}\n", s.line())),
            None => md.push_str("- frame time avg, windowed: not sampled\n"),
        }
        match &self.frames.frame_ms {
            Some(s) => md.push_str(&format!(
                "- frame time (frame counter + wall clock): {}\n",
                s.line()
            )),
            None => md.push_str("- frame time (frame counter + wall clock): not sampled\n"),
        }
        match self.frames.rss_max_mb {
            Some(v) => md.push_str(&format!("- RSS peak: {v:.0} MB\n")),
            None => md.push_str("- RSS peak: not readable on this platform\n"),
        }
        match (self.frames.rss_first_mb, self.frames.rss_last_mb) {
            (Some(first), Some(last)) => md.push_str(&format!(
                "- RSS growth: {first:.0} → {last:.0} MB ({:+.0} MB over the session)\n",
                last - first
            )),
            _ => md.push_str("- RSS growth: not sampled\n"),
        }
        md.push_str("- GPU memory: not exposed by the protocol\n");
        if let Some(v) = self.frames.allocated_bytes_max {
            md.push_str(&format!(
                "- heap peak (counting allocator): {:.0} MB\n",
                v as f64 / 1e6
            ));
        }
        if let Some([min, max, last]) = self.frames.entity_count {
            md.push_str(&format!("- entities: {min}–{max} (last {last})\n"));
        }
        if let Some(v) = self.frames.draw_calls_max {
            md.push_str(&format!("- draw calls, max: {v}\n"));
        }
        md.push_str(&format!(
            "- logs: {} error / {} warn ({} records read at warn+)\n",
            self.logs.errors, self.logs.warns, self.logs.records_read
        ));
        md.push_str(&format!(
            "- input: {} of {} injections accepted, {} failed\n",
            self.input.injected, self.input.entries, self.input.errors
        ));
        md.push_str(&format!(
            "- protocol calls failed/timed out: {}\n",
            self.protocol_errors
        ));
        md.push_str(&format!("- exit: {}\n", self.exit_line()));
        if !self.violations.is_empty() {
            md.push_str("\n| metric | measured | budget |\n|---|---|---|\n");
            for v in &self.violations {
                md.push_str(&format!("| {} | {} | {} |\n", v.metric, v.actual, v.limit));
            }
        }
        md
    }

    fn exit_line(&self) -> String {
        match self.crash.exit {
            AppExitKind::KilledByTool => "killed by the tool after the session (expected)".into(),
            AppExitKind::ExitedEarly => format!(
                "exited by itself at t={:.1}s, code {}",
                self.crash.at_secs.unwrap_or(0.0),
                self.crash.exit_code.unwrap_or(0)
            ),
            AppExitKind::NoEndpoint => "never advertised an endpoint; killed by the tool".into(),
            AppExitKind::DidNotStop => "ignored the terminate request".into(),
            AppExitKind::Froze => format!(
                "stopped answering the agent protocol; the tool ended the session at t={:.1}s and terminated it",
                self.crash.at_secs.unwrap_or(0.0)
            ),
        }
    }
}

impl SummaryReport {
    /// Renders the Markdown review form of the summary.
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str(&format!(
            "# Soak summary — {} × {:.1} min, {}\n\n**Verdict: {}**\n\n",
            self.sessions_count,
            self.minutes,
            self.exe,
            if self.passed { "PASS" } else { "FAIL" }
        ));
        md.push_str(
            "| session | verdict | connect s | p99 ms* | max ms* | RSS MB | errors | warns | crashes |\n",
        );
        md.push_str("|---|---|---|---|---|---|---|---|---|\n");
        for s in &self.sessions {
            let (p99, max) = match &s.frames.frame_ms {
                Some(v) => (format!("{:.3}", v.p99), format!("{:.3}", v.max)),
                None => ("—".into(), "—".into()),
            };
            md.push_str(&format!(
                "| {} | {} | {} | {p99} | {max} | {} | {} | {} | {} |\n",
                s.session,
                if s.passed { "pass" } else { "fail" },
                s.connect
                    .as_ref()
                    .map(|c| format!("{:.3}", c.start_secs))
                    .unwrap_or("—".into()),
                s.frames
                    .rss_max_mb
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or("—".into()),
                s.logs.errors,
                s.logs.warns,
                u64::from(s.crash.crashed)
            ));
        }
        md.push_str(
            "\n\\* p99/max of the wall-clock frame times (frame counter ÷ wall clock), post-warmup.\n",
        );
        if !self.passed {
            md.push_str("\nFailed sessions:\n");
            for s in &self.sessions {
                if !s.passed {
                    md.push_str(&format!(
                        "\n- session {} ({}): {}\n",
                        s.session,
                        s.exe,
                        if s.crash.crashed {
                            "crashed"
                        } else {
                            "budget breach"
                        }
                    ));
                    for v in &s.violations {
                        md.push_str(&format!("  - {} = {} > {}\n", v.metric, v.actual, v.limit));
                    }
                }
            }
        }
        md
    }
}

/// Writes `session-<n>.json` and `session-<n>.md` into `dir`; returns the JSON path.
pub fn write_session(dir: &Path, report: &SessionReport) -> Result<std::path::PathBuf> {
    let json = dir.join(format!("session-{}.json", report.session));
    let md = dir.join(format!("session-{}.md", report.session));
    std::fs::write(&json, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {}", json.display()))?;
    std::fs::write(&md, report.to_markdown()).with_context(|| format!("writing {}", md.display()))?;
    Ok(json)
}

/// Writes `summary.json` and `summary.md` into `dir`; returns the JSON path.
pub fn write_summary(dir: &Path, report: &SummaryReport) -> Result<std::path::PathBuf> {
    let json = dir.join("summary.json");
    let md = dir.join("summary.md");
    std::fs::write(&json, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {}", json.display()))?;
    std::fs::write(&md, report.to_markdown()).with_context(|| format!("writing {}", md.display()))?;
    Ok(json)
}

/// Reads a report file: a session report, or a summary whose sessions are returned.
pub fn load_sessions(path: &Path) -> Result<Vec<SessionReport>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if value.get("sessions").is_some() {
        let summary: SummaryReport =
            serde_json::from_value(value).with_context(|| format!("parsing {}", path.display()))?;
        Ok(summary.sessions)
    } else {
        let session: SessionReport =
            serde_json::from_value(value).with_context(|| format!("parsing {}", path.display()))?;
        Ok(vec![session])
    }
}
