// SPDX-License-Identifier: MPL-2.0
//! Summary-gate acceptance. The assertions follow the eight Python cases.

#![cfg(test)]

use super::{CommandOutput, SummaryRequest, summary_main};

const GATE_TOML: &str = r#"
[classes."1440p".HIGH.default]
fps_min = 60
frame_ms_p50_max = 16.7
frame_ms_p95_max = 25
frame_ms_p99_max = 33
update_ms_p50_max = 8
render_ms_p50_max = 8
outside_update_ms_p50_max = 2
gpu_ms_p50_max = 12
"#;

fn repeated(frame: &str, count: usize) -> String {
    std::iter::repeat_n(frame, count).collect::<Vec<_>>().join(",")
}

fn play(map: &str, frames: &str) -> String {
    format!(
        r#"{{"target":"regia::summary","phase":"play","map":"{map}","res_class":"1440p","tier":"HIGH","frames":[{frames}]}}"#
    )
}

fn run(logs: Vec<String>, baseline: Option<String>, json: bool) -> CommandOutput {
    summary_main(&SummaryRequest {
        logs,
        thresholds: GATE_TOML.to_owned(),
        baseline,
        res: None,
        tier: None,
        regression_pct: 10.0,
        hitch_ms: 25.0,
        json,
    })
}

fn frame_p99_line(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .find(|line| line.contains("frame p99") && !line.contains("vs baseline"))
}

#[test]
fn p99_over_threshold_fails_gate() {
    let frames = format!(
        "{},{}",
        repeated(r#"{"frame_ms":16.0}"#, 290),
        repeated(r#"{"frame_ms":60.0}"#, 10)
    );
    let output = run(vec![play("map_a", &frames)], None, false);
    assert_eq!(
        output.code, 1,
        "stdout:\n{}stderr:\n{}",
        output.stdout, output.stderr
    );
    let line = frame_p99_line(&output.stdout).expect("a frame p99 row");
    assert!(
        line.contains("FAIL"),
        "the frame p99 row must fail: {line}\n{}",
        output.stdout
    );
}

#[test]
fn a_missing_map_threshold_falls_back_to_the_default() {
    let frames = repeated(r#"{"frame_ms":16.0}"#, 300);
    let output = run(vec![play("map_never_seen", &frames)], None, false);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert!(output.stdout.contains("map_never_seen"), "{}", output.stdout);
    assert!(output.stdout.contains("overall: PASS"), "{}", output.stdout);
}

#[test]
fn baseline_regression_over_pct_fails() {
    let current = play("map_a", &repeated(r#"{"frame_ms":19.0}"#, 300));
    let baseline = play("map_a", &repeated(r#"{"frame_ms":16.0}"#, 300));
    let output = run(vec![current], Some(baseline), false);
    assert_eq!(output.code, 1, "{}", output.stdout);
    assert!(output.stdout.contains("vs baseline"), "{}", output.stdout);
    assert!(output.stdout.contains("FAIL"), "{}", output.stdout);
}

#[test]
fn hidden_frames_are_excluded() {
    let visible = repeated(r#"{"frame_ms":16.0}"#, 290);
    let hidden = repeated(r#"{"frame":100,"render":0,"outside":100}"#, 10);
    let loading = repeated(r#"{"frame_ms":200,"loading":true}"#, 3);
    let frames = format!("{visible},{hidden},{loading}");
    let output = run(vec![play("map_a", &frames)], None, true);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert!(
        output.stdout.contains("10 hidden frames skipped"),
        "{}",
        output.stdout
    );
    let value: serde_json::Value = serde_json::from_str(output.stdout.trim()).expect("json");
    assert_eq!(value["overall"], "PASS");
    assert_eq!(value["hidden_frames"], 10);
    assert_eq!(value["loading_frames"], 3);
    assert_eq!(value["stats"]["map_a"]["frame_ms"]["n"], 290);
}

#[test]
fn test_pass() {
    let frames = repeated(r#"{"frame_ms":16.6}"#, 600);
    let output = run(vec![play("map_a", &frames)], None, false);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert!(output.stdout.contains("overall: PASS"), "{}", output.stdout);
}

#[test]
fn test_non_play_phase_and_bad_lines_skipped() {
    let menu = format!(
        r#"{{"target":"regia::summary","phase":"menu","map":"map_a","res_class":"1440p","tier":"HIGH","frames":[{}]}}"#,
        repeated(r#"{"frame_ms":50.0}"#, 300)
    );
    let other = r#"{"target":"other","phase":"play","map":"map_a","frames":[{"frame_ms":50.0}]}"#;
    let play_line = play("map_a", &repeated(r#"{"frame_ms":16.0}"#, 300));
    let log = format!("not json\n{other}\n{menu}\n{play_line}\n");
    let output = run(vec![log], None, false);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert!(output.stdout.contains("overall: PASS"), "{}", output.stdout);
}

#[test]
fn test_json_output_parses() {
    let frames = repeated(r#"{"frame_ms":16.6}"#, 300);
    let output = run(vec![play("map_a", &frames)], None, true);
    assert_eq!(output.code, 0, "{}", output.stdout);
    let value: serde_json::Value = serde_json::from_str(output.stdout.trim()).expect("json");
    assert_eq!(value["overall"], "PASS");
    assert_eq!(value["stats"]["map_a"]["frame_ms"]["n"], 300);
}

#[test]
fn test_shipped_thresholds_have_pinned_start_values() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../regia_cli/perf-gate.toml");
    let text = std::fs::read_to_string(&path).expect("shipped thresholds");
    let doc: toml::Value = toml::from_str(&text).expect("toml");
    let high = doc
        .get("classes")
        .and_then(|classes| classes.get("1440p"))
        .and_then(|class| class.get("HIGH"))
        .and_then(|tier| tier.get("default"))
        .expect("1440p HIGH default");
    assert_eq!(
        high.get("frame_ms_p50_max").and_then(toml::Value::as_float),
        Some(16.7)
    );
    assert_eq!(
        high.get("frame_ms_p99_max").and_then(toml::Value::as_float),
        Some(33.0)
    );
    let ultra = doc
        .get("classes")
        .and_then(|classes| classes.get("ultrawide"))
        .and_then(|class| class.get("HIGH"))
        .and_then(|tier| tier.get("default"))
        .expect("ultrawide HIGH default");
    assert_eq!(
        ultra.get("frame_ms_p50_max").and_then(toml::Value::as_float),
        Some(25.0)
    );
}
