// SPDX-License-Identifier: MPL-2.0
//! The test report: JSON shape, JUnit XML, summary counts and the exit-code mapping
//! (spec 41 § 2.7, § 4; format notes in `docs/formats/test-report.md`).

use serde::{Deserialize, Serialize};

use super::types::Verdict;

/// The `TestReport` JSON format version: v2 adds the optional per-test `notes` (ADR-7980).
pub const TEST_REPORT_FORMAT_VERSION: u32 = 2;

/// The `from` versions of the report's migration steps: v1 → v2 (a v1 report has no `notes`; it
/// reads as a v2 report whose tests logged none, ADR-7980).
#[must_use]
pub fn test_report_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Exit code: every test passed.
pub const EXIT_PASSED: i32 = 0;
/// Exit code: at least one test `Failed`.
pub const EXIT_FAILED: i32 = 1;
/// Exit code: at least one test `Aborted` or `Undecided`.
pub const EXIT_ABORTED: i32 = 2;
/// Exit code: no tests ran (an empty run is not a pass).
pub const EXIT_NO_TESTS: i32 = 3;
/// Exit code: harness or launch failure.
pub const EXIT_HARNESS: i32 = 4;

/// One pass of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassRecord {
    /// Pass label; `None` for the first pass.
    pub label: Option<String>,
    /// Verdict of the pass.
    pub verdict: Verdict,
    /// Message given at conclude, plus recorded failures.
    pub message: String,
    /// Frames from Begin to conclude.
    pub frames: u64,
}

/// One named step of a test.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    /// Step name.
    pub name: String,
    /// Frames the step took.
    pub frames: u64,
    /// Verdict of the step (`Passed` or `Failed`).
    pub verdict: Verdict,
}

/// One test of the report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestRecord {
    /// Test name.
    pub name: String,
    /// Level path the test was found in.
    pub level: String,
    /// Tags of the placed test.
    pub tags: Vec<String>,
    /// The run's verdict (the worst pass). `Pending` when `skipped`.
    pub verdict: Verdict,
    /// Run-level message (the first non-passing pass's message).
    pub message: String,
    /// A disabled test: listed, not run, not a pass.
    pub skipped: bool,
    /// Every pass.
    pub passes: Vec<PassRecord>,
    /// Every step across passes.
    pub steps: Vec<StepRecord>,
    /// Total frames across passes.
    pub frames: u64,
    /// Expected values were written by this run.
    pub blessed: bool,
    /// Notes the body logged with `TestContext::note` (spec 41 § 2.4), in order;
    /// empty when it logged none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Optional wall time, excluded from equality-based checks via [`TestReport::without_wall_time`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

/// Counts for a summary line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Passed tests.
    pub passed: usize,
    /// Failed tests.
    pub failed: usize,
    /// Aborted tests.
    pub aborted: usize,
    /// Undecided tests.
    pub undecided: usize,
    /// Skipped (disabled) tests.
    pub skipped: usize,
}

/// The report of a run set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TestReport {
    /// Report format version (`2`; a loaded v1 report is migrated to `2`).
    pub format_version: u32,
    /// Build description (engine version, set by the caller).
    pub build: String,
    /// Seed the run used.
    pub seed: u64,
    /// Tests in run order.
    pub tests: Vec<TestRecord>,
}

impl TestReport {
    /// An empty report for `build` and `seed`.
    pub fn new(build: impl Into<String>, seed: u64) -> Self {
        Self {
            format_version: TEST_REPORT_FORMAT_VERSION,
            build: build.into(),
            seed,
            tests: Vec::new(),
        }
    }

    /// Counts by verdict.
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for t in &self.tests {
            if t.skipped {
                s.skipped += 1;
                continue;
            }
            match t.verdict {
                Verdict::Passed => s.passed += 1,
                Verdict::Failed => s.failed += 1,
                Verdict::Aborted => s.aborted += 1,
                Verdict::Undecided => s.undecided += 1,
                Verdict::Pending | Verdict::Active => s.aborted += 1,
            }
        }
        s
    }

    /// Exit code of `regia test`: 0 all passed, 1 a failure, 2 an abort or undecided test,
    /// 3 nothing ran. Skipped tests are not passes: a report of only skipped tests is 3.
    pub fn exit_code(&self) -> i32 {
        let s = self.summary();
        let ran = s.passed + s.failed + s.aborted + s.undecided;
        if ran == 0 {
            EXIT_NO_TESTS
        } else if s.aborted + s.undecided > 0 {
            EXIT_ABORTED
        } else if s.failed > 0 {
            EXIT_FAILED
        } else {
            EXIT_PASSED
        }
    }

    /// A copy with every `wall_ms` cleared, for equality checks between runs.
    pub fn without_wall_time(&self) -> Self {
        let mut copy = self.clone();
        for t in &mut copy.tests {
            t.wall_ms = None;
        }
        copy
    }

    /// Pretty JSON text.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parses JSON text and migrates an older report to the current version (v1 → v2: `notes`
    /// read as empty, ADR-7980); a newer `format_version` is an error.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut report: TestReport = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if report.format_version > TEST_REPORT_FORMAT_VERSION {
            return Err(format!(
                "test report format_version {} is newer than this build understands ({})",
                report.format_version, TEST_REPORT_FORMAT_VERSION
            ));
        }
        // v1 → v2 only adds the optional `notes` (serde default: empty), so the step is the
        // version stamp.
        report.format_version = TEST_REPORT_FORMAT_VERSION;
        Ok(report)
    }

    /// JUnit XML (one `testsuite`, one `testcase` per test), written by hand.
    #[tracing::instrument(skip_all, target = "regia_framework", name = "functional_report")]
    pub fn to_junit(&self) -> String {
        let s = self.summary();
        let mut out = String::with_capacity(256 + self.tests.len() * 160);
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str(&format!(
            "<testsuite name=\"regia\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
            self.tests.len(),
            s.failed,
            s.aborted + s.undecided,
            s.skipped
        ));
        for t in &self.tests {
            let class = xml_escape(&t.level);
            let name = xml_escape(&t.name);
            let time = t.wall_ms.map_or(0.0, |ms| ms as f64 / 1000.0);
            out.push_str(&format!(
                "  <testcase classname=\"{class}\" name=\"{name}\" time=\"{time:.3}\""
            ));
            let message = xml_escape(&t.message);
            if t.skipped {
                out.push_str(">\n    <skipped/>\n  </testcase>\n");
            } else {
                match t.verdict {
                    Verdict::Passed => out.push_str("/>\n"),
                    Verdict::Failed => out.push_str(&format!(
                        ">\n    <failure message=\"{message}\">{message}</failure>\n  </testcase>\n"
                    )),
                    _ => out.push_str(&format!(
                        ">\n    <error message=\"{message}\">{message}</error>\n  </testcase>\n"
                    )),
                }
            }
        }
        out.push_str("</testsuite>\n");
        out
    }
}

/// Escapes `& < > " '` for XML attribute and text content, and drops characters XML 1.0 forbids.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, verdict: Verdict, skipped: bool, message: &str) -> TestRecord {
        TestRecord {
            name: name.into(),
            level: "levels/a.level.ron".into(),
            tags: vec![],
            verdict,
            message: message.into(),
            skipped,
            passes: vec![],
            steps: vec![],
            frames: 3,
            blessed: false,
            notes: vec![],
            wall_ms: Some(7),
        }
    }

    #[test]
    fn exit_codes() {
        let mut r = TestReport::new("b", 0);
        assert_eq!(r.exit_code(), EXIT_NO_TESTS);
        r.tests.push(record("s", Verdict::Pending, true, ""));
        assert_eq!(r.exit_code(), EXIT_NO_TESTS, "a skipped test is not a pass");
        r.tests.push(record("p", Verdict::Passed, false, ""));
        assert_eq!(r.exit_code(), EXIT_PASSED);
        r.tests.push(record("f", Verdict::Failed, false, "x"));
        assert_eq!(r.exit_code(), EXIT_FAILED);
        r.tests.push(record("u", Verdict::Undecided, false, ""));
        assert_eq!(r.exit_code(), EXIT_ABORTED);
    }

    #[test]
    fn json_round_trip_and_newer_version_is_an_error() {
        let mut r = TestReport::new("b", 9);
        r.tests.push(record("p", Verdict::Passed, false, "ok"));
        let text = r.to_json().expect("serializes");
        assert_eq!(TestReport::from_json(&text).expect("parses"), r);
        let current = format!("\"format_version\": {TEST_REPORT_FORMAT_VERSION}");
        assert!(text.contains(&current), "{text}");
        let newer = text.replace(
            &current,
            &format!("\"format_version\": {}", TEST_REPORT_FORMAT_VERSION + 1),
        );
        assert!(
            TestReport::from_json(&newer)
                .expect_err("newer")
                .contains("newer")
        );
    }

    /// A v1 report (no `notes` field) loads as a current-version report whose tests logged no
    /// notes, and saves at the current version (the v1 → v2 step, ADR-7980).
    #[test]
    fn a_v1_report_migrates_to_the_current_version() {
        let v1 = r#"{ "format_version": 1, "build": "0.2.4", "seed": 0,
  "tests": [ { "name": "door_opens", "level": "levels/arena.level.ron", "tags": ["smoke"],
               "verdict": "Failed", "message": "x", "skipped": false,
               "passes": [ { "label": null, "verdict": "Failed", "message": "...", "frames": 120 } ],
               "steps": [], "frames": 120, "blessed": false } ] }"#;
        let report = TestReport::from_json(v1).expect("a v1 report loads");
        assert_eq!(report.format_version, TEST_REPORT_FORMAT_VERSION);
        assert_eq!(TEST_REPORT_FORMAT_VERSION, 2);
        assert!(report.tests[0].notes.is_empty());
        let saved = report.to_json().expect("serializes");
        assert!(saved.contains("\"format_version\": 2"), "{saved}");
        assert_eq!(TestReport::from_json(&saved).expect("reloads"), report);
        assert_eq!(test_report_migration_sources(), vec![1]);
    }

    #[test]
    fn junit_is_escaped_and_well_formed() {
        let mut r = TestReport::new("b", 0);
        r.tests
            .push(record("a<b>&\"'", Verdict::Failed, false, "x < y & \"z\""));
        r.tests.push(record("ok", Verdict::Passed, false, ""));
        r.tests.push(record("skip", Verdict::Pending, true, ""));
        let xml = r.to_junit();
        assert!(xml.contains("name=\"a&lt;b&gt;&amp;&quot;&apos;\""), "{xml}");
        assert!(
            xml.contains("<failure message=\"x &lt; y &amp; &quot;z&quot;\">"),
            "{xml}"
        );
        assert!(
            xml.contains("tests=\"3\" failures=\"1\" errors=\"0\" skipped=\"1\""),
            "{xml}"
        );
        assert!(xml.contains("<skipped/>"));
        assert_eq!(xml.matches("<testcase").count(), 3);
        assert_eq!(xml.matches("</testsuite>").count(), 1);
        assert_eq!(xml_escape("a\u{1}b"), "ab");
    }

    #[test]
    fn wall_time_is_excluded_from_equality() {
        let mut a = TestReport::new("b", 0);
        a.tests.push(record("p", Verdict::Passed, false, ""));
        let mut b = a.clone();
        b.tests[0].wall_ms = Some(900);
        assert_ne!(a, b);
        assert_eq!(a.without_wall_time(), b.without_wall_time());
    }

    /// § 10: a 1,000-test report counts every test (the budget asserts counts,
    /// not wall time): the summary, the exit code, the JSON round trip and the
    /// derived JUnit XML all agree on 1,000.
    #[test]
    fn a_thousand_test_report_counts_every_test() {
        let mut r = TestReport::new("b", 0);
        for i in 0..1000 {
            r.tests
                .push(record(&format!("test_{i:04}"), Verdict::Passed, false, ""));
        }
        let s = r.summary();
        assert_eq!((s.passed, s.failed, s.skipped), (1000, 0, 0));
        assert_eq!(r.exit_code(), EXIT_PASSED);
        let json = r.to_json().expect("json");
        let back = TestReport::from_json(&json).expect("parse");
        assert_eq!(back.tests.len(), 1000);
        let xml = r.to_junit();
        assert_eq!(xml.matches("<testcase").count(), 1000);
        assert!(xml.contains("tests=\"1000\""), "{xml}");
    }
}
