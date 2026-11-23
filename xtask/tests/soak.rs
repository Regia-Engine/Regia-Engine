// SPDX-License-Identifier: MPL-2.0
//! Integration tests of the soak tooling's report path (`cargo xtask budgets check`): session
//! and summary JSON round-trips through the command without launching an app. The end-to-end
//! *run* (launch, connect, sample) is exercised by the committed soak evidence under
//! `docs/evidence/m18/` (see the M18 progress notes).

use std::path::PathBuf;
use std::process::ExitCode;

use xtask::soak::budgets::Measured;
use xtask::soak::report::{
    AppExitKind, ConnectInfo, CrashInfo, FrameStats, InputStats, LogCounts, SessionReport, SummaryReport,
};
use xtask::soak::{BudgetsCommand, budgets_command};

/// Workspace root (the parent of `xtask`).
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .expect("xtask has a parent directory")
}

fn session(index: u32, p99_ms: f64) -> SessionReport {
    let measured = Measured {
        frame_p99_ms: Some(p99_ms),
        frame_max_ms: Some(40.0),
        rss_max_mb: Some(800.0),
        start_secs: Some(1.0),
        errors: Some(0),
        warns: Some(0),
        crashes: 0,
    };
    SessionReport {
        session: index,
        exe: "sim_bridge".into(),
        app_args: vec![],
        windowed: true,
        started_utc: "2026-10-04T12:00:00Z".into(),
        duration_secs: 120.0,
        warmup_secs: 10.0,
        sample_hz: 2.0,
        samples: 240,
        connect: Some(ConnectInfo {
            start_secs: 1.0,
            protocol: "0.2".into(),
            app: "sim_bridge".into(),
            os: "linux".into(),
            pid: 42,
            methods: 50,
        }),
        frames: FrameStats {
            frames_first: Some(10),
            frames_last: Some(14_500),
            avg_ms: None,
            frame_ms: None,
            rss_max_mb: Some(800.0),
            rss_first_mb: Some(700.0),
            rss_last_mb: Some(800.0),
            gpu_memory_mb: None,
            entity_count: None,
            draw_calls_max: None,
            allocated_bytes_max: None,
        },
        logs: LogCounts {
            errors: 0,
            warns: 0,
            records_read: 40,
        },
        input: InputStats {
            entries: 0,
            injected: 0,
            errors: 0,
        },
        protocol_errors: 0,
        crash: CrashInfo {
            crashed: false,
            exit: AppExitKind::KilledByTool,
            at_secs: None,
            exit_code: None,
            panic_text: None,
        },
        measured,
        violations: vec![],
        passed: true,
    }
}

fn write_reports() -> (PathBuf, PathBuf) {
    // One directory per call: tests run in parallel and each rewrites summary.json.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("xtask-soak-integration-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("report dir");
    let clean = dir.join("session-clean.json");
    let summary = dir.join("summary.json");
    std::fs::write(
        &clean,
        serde_json::to_string_pretty(&session(1, 6.9)).expect("serialise"),
    )
    .expect("write clean report");
    // The summary's second session breaches frame_p99 (9.9 > 6.9) and carries a crash.
    let mut bad = session(2, 9.9);
    bad.measured.crashes = 1;
    bad.passed = false;
    let summary_report = SummaryReport {
        started_utc: "2026-10-04T12:00:00Z".into(),
        exe: "sim_bridge".into(),
        minutes: 2.0,
        sessions_count: 2,
        sessions: vec![session(1, 6.9), bad],
        passed: false,
    };
    std::fs::write(
        &summary,
        serde_json::to_string_pretty(&summary_report).expect("serialise"),
    )
    .expect("write summary");
    (clean, summary)
}

#[test]
fn budgets_check_passes_a_clean_session_report() {
    let (clean, _) = write_reports();
    let command = BudgetsCommand::Check {
        budgets: Some(root().join("budgets/default.ron")),
        reports: vec![clean],
    };
    match budgets_command(&command, &root()) {
        Ok(ExitCode::SUCCESS) => {}
        other => panic!("clean report must pass, got {other:?}"),
    }
}

#[test]
fn budgets_check_fails_a_breaching_summary() {
    let (_, summary) = write_reports();
    let command = BudgetsCommand::Check {
        budgets: Some(root().join("budgets/default.ron")),
        reports: vec![summary],
    };
    match budgets_command(&command, &root()) {
        Ok(ExitCode::FAILURE) => {}
        other => panic!("breached summary must fail, got {other:?}"),
    }
}

#[test]
fn the_shipped_input_script_parses_and_expands() {
    // The committed evidence input must stay a valid script.
    let text = std::fs::read_to_string(root().join("budgets/sim_bridge-soak.input.ron")).expect("script");
    let script: xtask::soak::input_script::InputScript = ron::from_str(&text).expect("parses");
    let schedule = xtask::soak::input_script::expand(&script, 120.0).expect("expands");
    assert!(
        schedule.len() > 20,
        "looped timeline has entries: {}",
        schedule.len()
    );
    assert!(
        schedule.iter().any(|s| s.at > 100.0),
        "the timeline loops past 100 s"
    );
}

#[test]
fn the_arena_input_script_parses_and_expands() {
    // The arena soak script (including raw `mouse_button` fire events) must stay valid.
    let text = std::fs::read_to_string(root().join("budgets/arena-soak.input.ron")).expect("script");
    let script: xtask::soak::input_script::InputScript = ron::from_str(&text).expect("parses");
    let schedule = xtask::soak::input_script::expand(&script, 60.0).expect("expands");
    assert!(!schedule.is_empty(), "arena timeline has entries");
    assert!(
        schedule
            .iter()
            .any(|s| s.events.iter().any(|e| e["type"] == "mouse_button")),
        "the arena script fires the rifle via raw mouse_button events"
    );
}
