// SPDX-License-Identifier: MPL-2.0
//! Soak budgets (`docs/06` § 4): the thresholds a soak report must meet, loaded from a RON file.
//!
//! The shipped thresholds live in `budgets/default.ron` and carry the Tier-1 desktop numbers from
//! `docs/06` § 4 (measured on `samples/sim_bridge` until `samples/third_person` exists, M20).

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Budget thresholds for a soak configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Budgets {
    /// Frame time p99 in ms (`docs/06` § 4: ≤ 6.9 ms at 1080p High, desktop reference).
    pub frame_p99_ms: f64,
    /// Largest single frame after warmup in ms (the hitch rule: no frame > 50 ms).
    pub frame_max_ms: f64,
    /// Warmup seconds excluded from the hitch rule ("after the first 10 s of play").
    pub warmup_secs: f64,
    /// Peak RSS in MB (`docs/06` § 4: ≤ 1.5 GB for a sample).
    pub rss_mb: f64,
    /// Seconds from process start to the first agent-protocol answer (cold-start proxy;
    /// `docs/06` § 4: ≤ 3 s to the first interactive frame).
    pub start_secs: f64,
    /// Maximum error-level log lines over a session (`error` = user-visible failure, `docs/04` § 6).
    pub errors: u64,
    /// Maximum warn-level log lines over a session.
    pub warns: u64,
    /// Maximum crashes over a session (M18: zero crashes).
    pub crashes: u64,
    /// Gate the pass/fail verdict on the frame-time budgets (`frame_p99_ms`/`frame_max_ms`).
    /// False when the run cannot measure real frame cost — e.g. a lavapipe / software-rasterised
    /// run, where frame times are still measured and reported but only crashes, leaks and errors
    /// gate. True on a reference GPU.
    #[serde(default = "default_check_frames")]
    pub check_frames: bool,
}

/// Default for [`Budgets::check_frames`]: frame budgets gate by default.
fn default_check_frames() -> bool {
    true
}

impl Budgets {
    /// Loads a budgets file from RON.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading budgets file {}", path.display()))?;
        ron::from_str(&text).with_context(|| format!("parsing budgets file {}", path.display()))
    }

    /// Default budgets file of the repository (`budgets/default.ron` below `root`).
    pub fn default_path(root: &Path) -> std::path::PathBuf {
        root.join("budgets").join("default.ron")
    }

    /// Checks `m` against every budget; an empty result is a pass. A metric the report does not
    /// carry (`None`) never violates — the soak tool only checks what it could measure.
    pub fn check(&self, m: &Measured) -> Vec<Violation> {
        let mut out = Vec::new();
        let mut push = |metric: &'static str, actual: Option<f64>, limit: f64| {
            if let Some(v) = actual
                && v > limit
            {
                out.push(Violation {
                    metric: metric.into(),
                    actual: format!("{v:.1}"),
                    limit: format!("{limit:.1}"),
                });
            }
        };
        if self.check_frames {
            push("frame_p99_ms", m.frame_p99_ms, self.frame_p99_ms);
            push("frame_max_ms", m.frame_max_ms, self.frame_max_ms);
        }
        push("rss_mb", m.rss_max_mb, self.rss_mb);
        push("start_secs", m.start_secs, self.start_secs);
        let mut push_count = |metric: &'static str, actual: u64, limit: u64| {
            if actual > limit {
                out.push(Violation {
                    metric: metric.into(),
                    actual: actual.to_string(),
                    limit: limit.to_string(),
                });
            }
        };
        push_count("errors", m.errors.unwrap_or(0), self.errors);
        push_count("warns", m.warns.unwrap_or(0), self.warns);
        push_count("crashes", m.crashes, self.crashes);
        out
    }
}

/// The measured side of a budget check; `None` means "not measured" (e.g. GPU memory, which the
/// protocol does not expose yet).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// Frame time p99 in ms (post-warmup).
    pub frame_p99_ms: Option<f64>,
    /// Largest frame after warmup in ms.
    pub frame_max_ms: Option<f64>,
    /// Peak RSS in MB.
    pub rss_max_mb: Option<f64>,
    /// Seconds from process start to the first protocol answer.
    pub start_secs: Option<f64>,
    /// Error-level log lines.
    pub errors: Option<u64>,
    /// Warn-level log lines.
    pub warns: Option<u64>,
    /// Crashes (unexpected exit or panic). Always measured: 0 or 1 per session.
    pub crashes: u64,
}

/// One metric that breached its budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// Metric name as it appears in reports.
    pub metric: String,
    /// Measured value.
    pub actual: String,
    /// Budget.
    pub limit: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_budgets() -> Budgets {
        Budgets {
            frame_p99_ms: 6.9,
            frame_max_ms: 50.0,
            warmup_secs: 10.0,
            rss_mb: 1536.0,
            start_secs: 3.0,
            errors: 0,
            warns: 0,
            crashes: 0,
            check_frames: true,
        }
    }

    #[test]
    fn parses_default_ron() {
        // The committed budgets file must parse and carry the docs/06 § 4 numbers.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = manifest
            .parent()
            .expect("xtask has a parent")
            .join("budgets/default.ron");
        let b = Budgets::load(&path).expect("default budgets parse");
        assert_eq!(b.frame_p99_ms, 6.9);
        assert_eq!(b.frame_max_ms, 50.0);
        assert_eq!(b.rss_mb, 1536.0);
        assert_eq!(b.start_secs, 3.0);
        assert_eq!(b.errors, 0);
        // The default file omits the flag; it must default to gating frame budgets.
        assert!(b.check_frames);
    }

    #[test]
    fn parses_lavapipe_ron_with_frame_gating_off() {
        // The headless/lavapipe budgets file must parse and not gate on frame times.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = manifest
            .parent()
            .expect("xtask has a parent")
            .join("budgets/lavapipe-soak.ron");
        let b = Budgets::load(&path).expect("lavapipe budgets parse");
        assert!(
            !b.check_frames,
            "lavapipe runs report frame times but do not gate on them"
        );
        assert_eq!(b.crashes, 0);
        assert_eq!(b.errors, 0);
        // Exactly the two Xvfb warnings are tolerated; a cpal underrun is not expected any more
        // (ADR-028 pauses the stream on hidden surfaces and on a starved frame loop).
        assert_eq!(b.warns, 2);
    }

    #[test]
    fn frame_budgets_do_not_gate_when_check_frames_is_off() {
        // On lavapipe frame times are real but not representative; the same breach that fails a
        // GPU soak must be reported, not gated, when `check_frames` is false.
        let mut budgets = test_budgets();
        budgets.check_frames = false;
        let m = Measured {
            frame_p99_ms: Some(999.0),
            frame_max_ms: Some(999.0),
            rss_max_mb: Some(1000.0),
            start_secs: Some(1.0),
            errors: Some(0),
            warns: Some(0),
            crashes: 0,
        };
        assert!(budgets.check(&m).is_empty(), "frames off → no frame violations");
        budgets.check_frames = true;
        let violations = budgets.check(&m);
        let names: Vec<&str> = violations.iter().map(|v| v.metric.as_str()).collect();
        assert_eq!(names, vec!["frame_p99_ms", "frame_max_ms"]);
    }

    #[test]
    fn clean_measurement_passes() {
        let m = Measured {
            frame_p99_ms: Some(6.9),
            frame_max_ms: Some(50.0),
            rss_max_mb: Some(1536.0),
            start_secs: Some(3.0),
            errors: Some(0),
            warns: Some(0),
            crashes: 0,
        };
        assert!(test_budgets().check(&m).is_empty());
    }

    #[test]
    fn breaches_are_listed_per_metric() {
        let m = Measured {
            frame_p99_ms: Some(9.9),
            frame_max_ms: Some(80.0),
            rss_max_mb: Some(2000.0),
            start_secs: Some(5.0),
            errors: Some(2),
            warns: Some(1),
            crashes: 1,
        };
        let violations = test_budgets().check(&m);
        let names: Vec<&str> = violations.iter().map(|v| v.metric.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "frame_p99_ms",
                "frame_max_ms",
                "rss_mb",
                "start_secs",
                "errors",
                "warns",
                "crashes"
            ]
        );
    }

    #[test]
    fn at_the_limit_is_a_pass() {
        let m = Measured {
            frame_p99_ms: Some(6.8),
            frame_max_ms: Some(49.9),
            rss_max_mb: Some(1535.9),
            start_secs: Some(2.9),
            errors: Some(0),
            warns: Some(0),
            crashes: 0,
        };
        assert!(test_budgets().check(&m).is_empty());
    }

    #[test]
    fn unmeasured_metrics_never_violate() {
        let m = Measured::default();
        assert!(test_budgets().check(&m).is_empty());
    }
}
