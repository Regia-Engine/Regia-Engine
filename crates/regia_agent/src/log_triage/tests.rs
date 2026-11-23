// SPDX-License-Identifier: MPL-2.0
//! Offline log triage acceptance tests (C-13, ADR-8035): stitching, summaries, rates and storms.
#![cfg(test)]

use super::*;
use serde_json::json;

#[test]
fn split_upload_stitched_in_order() {
    let first = "{\"t_ms\":0,\"target\":\"app::net\",\"msg\":\"begin\"}\n";
    let last = "{\"t_ms\":2000,\"target\":\"regia::hitch\",\"fields\":{\"data\":{\"frame_ms\":125}}}\n";
    // Scrubbing shortened the stored bytes: only the SOURCE ranges can establish continuity.
    let params = json!({"include_stitched": true, "bundles": [
        {"name":"upload_s0002", "files": {
            "manifest.txt":"session: session-a\nseq: 2\nentry\tbytes\trange\nlogs/app.jsonl.from-100\t77\t100..200\n",
            "logs/app.jsonl.from-100": last,
            "logs/app.summary.json":"{\"exit\":{\"clean\":true}}"
        }},
        {"name":"upload_s0001", "files": {
            "manifest.txt":"session: session-a\nseq: 1\nentry\tbytes\trange\nlogs/app.jsonl.from-0\t49\t0..100\n",
            "logs/app.jsonl.from-0":first,
            "logs/app.summary.json":"{\"exit\":{\"clean\":false}}"
        }}
    ]});
    let output = triage(&params).expect("triage");
    assert_eq!(output["files"][0]["text"], format!("{first}{last}"));
    assert_eq!(output["sessions"][0]["session"], "session-a");
    assert_eq!(output["sessions"][0]["records"], 2);
    assert_eq!(output["sessions"][0]["source_gaps"], 0);
    assert_eq!(output["sessions"][0]["exit"], "clean");
    assert_eq!(output["sessions"][0]["hitch_max_ms"], 125.0);
    let mut reverse = params.clone();
    reverse["bundles"].as_array_mut().expect("bundles").reverse();
    assert_eq!(triage(&reverse).expect("reverse"), output);
}

fn run_lines(lines: Vec<serde_json::Value>, settings: serde_json::Value) -> serde_json::Value {
    let text = lines.iter().map(|v| format!("{v}\n")).collect::<String>();
    triage(&json!({"bundles":[{"name":"sample","files":{"sample.jsonl":text}}],"settings":settings}))
        .expect("triage")
}

fn code(params: serde_json::Value) -> String {
    triage(&params).expect_err("rejected").data.expect("error data")["code"]
        .as_str()
        .expect("code")
        .to_owned()
}

#[test]
fn repeated_pieces_are_not_counted_twice_and_missing_source_is_named() {
    let bundle = json!({"name":"tail", "files": {
        "manifest.txt":"session: a\nseq: 2\nentry\tbytes\trange\nlogs/app.jsonl.from-50\t1\t50..90\n",
        "logs/app.jsonl.from-50":"{\"t_ms\":0}\n"
    }});
    let output = triage(&json!({"bundles":[bundle.clone(),bundle],"include_stitched":true})).expect("triage");
    assert_eq!(output["sessions"][0]["records"], 1);
    assert_eq!(output["sessions"][0]["duplicates"], 1);
    assert_eq!(
        output["files"][0]["gaps"][0],
        json!({"start":0,"end":50,"output_offset":0})
    );
}

#[test]
fn overlapping_scrubbed_ranges_are_refused_without_guessing_a_trim() {
    let value = json!({"bundles":[{"name":"parts","files":{
        "manifest.txt":"entry\tbytes\trange\napp.jsonl.from-0\t2\t0..10\napp.jsonl.from-5\t2\t5..15\n",
        "app.jsonl.from-0":"{}","app.jsonl.from-5":"{}"
    }}]});
    assert_eq!(code(value), "logs_triage_overlap");
}

#[test]
fn invalid_ranges_paths_and_budgets_have_stable_codes() {
    for path in [
        "../app.jsonl",
        "/app.jsonl",
        "C:/app.jsonl",
        "logs\\app.jsonl",
        "a//app.jsonl",
    ] {
        assert_eq!(
            code(json!({"bundles":[{"name":"bad","files":{path:"{}"}}]})),
            "logs_triage_path"
        );
    }
    assert_eq!(
        code(json!({"bundles":[{"name":"bad","files":{
        "manifest.txt":"session: ../escape\n", "app.jsonl":"{}"}}]})),
        "logs_triage_path"
    );
    assert_eq!(
        code(json!({"bundles":[{"name":"bad","files":{
        "manifest.txt":"entry\tbytes\trange\napp.jsonl\t2\t20..10\n", "app.jsonl":"{}"}}]})),
        "logs_triage_range"
    );
    for settings in [json!({"max_bytes":1}), json!({"max_files":1})] {
        assert_eq!(
            code(
                json!({"bundles":[{"name":"a","files":{"a.jsonl":"{}","b.jsonl":"{}"}}],"settings":settings})
            ),
            "logs_triage_limit"
        );
    }
    for settings in [
        json!({"bucket_seconds":0}),
        json!({"gap_seconds":-1}),
        json!({"storm_factor":0.5}),
        json!({"storm_min_count":0}),
        json!({"loss_percent":101}),
        json!({"max_bytes":0}),
    ] {
        assert_eq!(
            code(json!({"bundles":[],"settings":settings})),
            "logs_triage_settings"
        );
    }
}

#[test]
fn malformed_tail_untimed_records_and_nested_legacy_data_remain_distinct() {
    let output = triage(&json!({"bundles":[{"name":"mixed","files":{"s.jsonl":concat!(
        "\n{\"t_ms\":0,\"target\":\"regia::hitch\",\"fields\":{\"data\":{\"frame_ms\":50}}}\n",
        "{\"t_ms\":1000,\"target\":\"app::hitch\",\"fields\":{\"data_json\":\"{\\\"ms\\\":125}\"}}\n",
        "{\"level\":\"WARN\",\"msg\":\"untimed\"}\nnull\n{truncated"
    )}}]}))
    .expect("triage");
    let s = &output["sessions"][0];
    assert_eq!(s["records"], 3);
    assert_eq!(s["malformed_lines"], 2);
    assert_eq!(s["warnings"], 1);
    assert_eq!(s["hitches"], 2);
    assert_eq!(s["hitch_max_ms"], 125.0);
    assert_eq!(s["rates"]["timed_records"], 2);
    assert_eq!(s["rates"]["untimed_records"], 1);
}

#[test]
fn empty_buckets_count_in_storm_medians_without_dense_time_allocation() {
    let mut lines = vec![json!({"t_ms":0,"target":"quiet"})];
    for i in 0..5 {
        lines.push(json!({"t_ms":60000+i,"target":"burst"}));
    }
    lines.push(json!({"t_ms":60000000000000_u64,"target":"quiet"}));
    let output = run_lines(lines, json!({}));
    let rates = &output["sessions"][0]["rates"];
    assert_eq!(rates["bucket_span"], 1000000001_u64);
    assert_eq!(rates["buckets"].as_array().expect("buckets").len(), 3);
    assert_eq!(rates["storms"].as_array().expect("storms").len(), 1);
    assert_eq!(rates["storms"][0]["median_count"], 0.0);
    assert_eq!(rates["storms"][0]["ratio"], serde_json::Value::Null);
    assert_eq!(rates["storms"][0]["count"], 5);
}

#[test]
fn nonzero_median_storm_threshold_is_strict_and_configurable() {
    let mut lines = Vec::new();
    for (bucket, count) in [(0, 2), (1, 2), (2, 20)] {
        for i in 0..count {
            lines.push(json!({"t_ms":bucket*1000+i,"target":"net"}));
        }
    }
    let at_boundary = run_lines(lines.clone(), json!({"bucket_seconds":1,"storm_factor":10}));
    assert_eq!(at_boundary["sessions"][0]["rates"]["storms"], json!([]));
    let beyond = run_lines(lines, json!({"bucket_seconds":1,"storm_factor":9}));
    assert_eq!(beyond["sessions"][0]["rates"]["storms"][0]["ratio"], 10.0);
}

#[test]
fn numeric_and_iso_timestamps_are_equivalent_and_invalid_dates_are_untimed() {
    use super::time::timestamp;
    for v in [
        json!({"t_ms":"1000"}),
        json!({"t":1}),
        json!({"unix_ms":1000}),
        json!({"wall":1}),
        json!({"wall":"1970-01-01T00:00:01Z"}),
        json!({"wall":"1970-01-01T01:30:01+01:30"}),
        json!({"timestamp":"1969-12-31T19:00:01-05:00"}),
    ] {
        assert_eq!(timestamp(&v), Some(1000.0), "{v}");
    }
    assert_eq!(
        timestamp(&json!({"wall":"2000-02-29T00:00:00.125Z"})),
        Some(951782400125.0)
    );
    for wall in [
        "1900-02-29T00:00:00Z",
        "2024-02-30T00:00:00Z",
        "2024-13-01T00:00:00Z",
        "2024-01-01T24:00:00Z",
        "NaN",
        "inf",
    ] {
        assert_eq!(timestamp(&json!({"wall":wall})), None, "{wall}");
    }
    assert_eq!(timestamp(&json!({"t_ms":true})), None);
}

#[test]
fn time_gaps_and_bucket_boundaries_are_reported_after_time_sorting() {
    let output = run_lines(
        vec![
            json!({"t_ms":1000,"target":"a"}),
            json!({"t_ms":0,"target":"a"}),
            json!({"t_ms":3001,"target":"a"}),
        ],
        json!({"bucket_seconds":1,"gap_seconds":1}),
    );
    let rates = &output["sessions"][0]["rates"];
    assert_eq!(rates["buckets"][1]["bucket"], 1);
    assert_eq!(
        rates["gaps"],
        json!([{"start_ms":1000.0,"end_ms":3001.0,"duration_ms":2001.0}])
    );
}

#[test]
fn partial_and_corrupt_final_summaries_do_not_prove_clean_exit() {
    for (summary, expected, errors) in [
        ("{\"exit\":{\"clean\":false}}", "unclean", 0),
        ("broken", "unknown", 1),
    ] {
        let output = triage(&json!({"bundles":[{"name":"b","files":{
            "s.jsonl":"{\"target\":\"regia::exit\",\"data\":{\"clean\":true}}\n",
            "s.summary.json":summary
        }}]}))
        .expect("triage");
        assert_eq!(output["sessions"][0]["exit"], expected);
        assert_eq!(output["sessions"][0]["summary_errors"], errors);
    }
    let output =
        triage(&json!({"bundles":[{"name":"b","files":{"s.summary.partial.json":"{}"}}]})).expect("triage");
    assert_eq!(output["sessions"][0]["exit"], "unclean");
}

#[test]
fn unrelated_sessions_and_prefixes_do_not_mix() {
    let output = triage(&json!({"bundles":[{"name":"b","files":{
        "manifest.txt":"session: active\n",
        "logs/demo-20261116-101010-7.jsonl":"{\"target\":\"app::panic\"}\n",
        "logs/other-20261116-101010-8.jsonl":"{\"t_ms\":1}\n"
    }}]}))
    .expect("triage");
    assert_eq!(output["sessions"].as_array().expect("sessions").len(), 2);
    assert_eq!(output["sessions"][0]["panics"], 1);
    assert_eq!(output["sessions"][1]["panics"], 0);
}

#[test]
fn match_verdict_excludes_menu_and_tick_zero_frame_summaries() {
    let output = run_lines(
        vec![
            json!({"t_ms":0,"target":"regia::frame_summary","data":{"p50_ms":1000}}),
            json!({"t_ms":1,"target":"app::match","data":{"event":"start"}}),
            json!({"t_ms":2,"target":"regia::frame_summary","data":{"tick":0,"p50_ms":1000}}),
            json!({"t_ms":3,"target":"regia::frame_summary","data":{"tick":10,"p50_ms":12}}),
            json!({"t_ms":4,"target":"app::match","data":{"event":"end"}}),
            json!({"t_ms":5,"target":"regia::frame_summary","data":{"p50_ms":1000}}),
        ],
        json!({}),
    );
    assert_eq!(output["sessions"][0]["match_frame_summary_p50_median_ms"], 12.0);
    assert_eq!(output["sessions"][0]["verdict"], "no_issue_detected");
}

#[test]
fn network_and_phase_diagnostics_are_engine_general() {
    let output = run_lines(
        vec![
            json!({"t_ms":0,"target":"app::network","data":{"event":"late_input","count":3,"rtt_ms":250,"loss_pct":1}}),
            json!({"t_ms":10,"target":"regia::phase","data":{"phase":"scene_load_end","ms":123}}),
            json!({"t_ms":11,"target":"regia::stall","data":{"silent_ms":3000,"where":"load"}}),
            json!({"t_ms":12,"target":"regia::exit","data":{"clean":true,"steps":[{"step":"flush","ms":12},{"step":"join","ms":50}]}}),
        ],
        json!({}),
    );
    let s = &output["sessions"][0];
    assert_eq!(s["late_inputs"], 3);
    assert_eq!(s["verdict"], "network");
    assert_eq!(s["phases"][0]["max_ms"], 123.0);
    assert_eq!(s["exit_max_ms"], 50.0);
    assert_eq!(s["stalls"][0]["ms"], 3000.0);
}

#[test]
fn templates_normalize_numbers_hex_uuid_and_whitespace_and_scrub_secrets() {
    let output = run_lines(
        vec![
            json!({"t_ms":0,"msg":"item 12 0x1a 123e4567-e89b-12d3-a456-426614174000 speed -3.5e+2"}),
            json!({"t_ms":1,"msg":"item 99 0xff 123e4567-e89b-12d3-a456-426614174111  speed -2.5e+2"}),
            json!({"t_ms":2,"msg":"password=private-value"}),
        ],
        json!({"top":2}),
    );
    assert_eq!(
        output["sessions"][0]["templates"][0],
        json!({"template":"item # # # speed #","count":2})
    );
    assert!(!output.to_string().contains("private-value"));
}

#[test]
fn utf8_split_inside_a_character_is_decoded_after_stitching() {
    let line = "{\"t_ms\":0,\"msg\":\"café\"}\n";
    let split = line.find('é').expect("character") + 1;
    let a = &line.as_bytes()[..split];
    let b = &line.as_bytes()[split..];
    let tail = format!("s.jsonl.from-{split}");
    let output = triage(
        &json!({"include_stitched":true,"bundles":[{"name":"parts","files":{
            "s.jsonl.from-0":{"bytes":a},tail:{"bytes":b}
        }}]}),
    )
    .expect("stitch bytes");
    assert_eq!(output["files"][0]["text"], line);
    assert_eq!(output["sessions"][0]["records"], 1);
    assert_eq!(output["sessions"][0]["templates"][0]["template"], "café");
}

#[test]
fn real_frame_summary_schema_sets_frame_rate_verdict() {
    // Producer shape: timings live in data.frame_ms, and `data` is often a JSON string.
    // Outer tracing fields are only a fallback when that object has no timings.
    let slow = r#"{"frame_ms":{"p50":80,"p99":140},"frames":10}"#;
    let menu = r#"{"frame_ms":{"p50":1000,"p99":1000}}"#;
    let output = run_lines(
        vec![
            json!({"t_ms":0,"target":"regia::frame_summary","fields":{"data": menu}}),
            json!({"t_ms":1,"target":"regia::phase","fields":{"data":"{\"phase\":\"match_start\"}"}}),
            json!({"t_ms":2,"target":"regia::frame_summary","data":{"tick":0,"frame_ms":{"p50":1000,"p99":1000}}}),
            json!({"t_ms":3,"target":"regia::frame_summary","fields":{"p50_ms":1.0,"p99_ms":1.0,"data": slow}}),
            json!({"t_ms":4,"target":"regia::frame_summary","fields":{"p50_ms":60.0,"p99_ms":90.0,"data":"{\"frames\":4}"}}),
            json!({"t_ms":5,"target":"regia::phase","data":{"phase":"match_end"}}),
        ],
        json!({}),
    );
    let s = &output["sessions"][0];
    assert_eq!(s["match_frame_summary_p50_median_ms"], 70.0);
    assert_eq!(s["frame_summary_p99_max_ms"], 1000.0);
    assert_eq!(s["verdict"], "frame_rate");
}

#[test]
fn canonical_summary_names_follow_the_session_folder() {
    let output = triage(&json!({"bundles":[{"name":"drop","files":{
        "session-a/events.jsonl": "{\"t_ms\":1,\"target\":\"app::start\"}\n",
        "session-a/summary.json": "{\"exit\":{\"clean\":true}}",
        "session-a/match/detail.jsonl": "{\"target\":\"app::panic\"}\n",
        "session-b/events.jsonl": "{\"t_ms\":1,\"target\":\"app::panic\"}\n",
        "session-b/summary.partial.json": "{}",
        "session-c/events.jsonl": "{\"t_ms\":1}\n",
        "session-c/summary.json": "broken",
        "logs/kept-20261116-101010-4.jsonl": "{\"t_ms\":1}\n",
        "logs/other-20261116-101010-5.jsonl": "{\"target\":\"app::panic\"}\n"
    }}]}))
    .expect("triage");
    let mut by_session = std::collections::BTreeMap::new();
    for session in output["sessions"].as_array().expect("sessions") {
        by_session.insert(
            session["session"].as_str().expect("name").to_owned(),
            session.clone(),
        );
    }
    assert_eq!(by_session.len(), 5, "{by_session:?}");
    assert_eq!(by_session["session-a"]["exit"], "clean");
    assert_eq!(by_session["session-a"]["records"], 1);
    assert_eq!(by_session["session-a"]["panics"], 0);
    assert_eq!(by_session["session-b"]["exit"], "unclean");
    assert_eq!(by_session["session-b"]["panics"], 1);
    assert_eq!(by_session["session-c"]["exit"], "unknown");
    assert_eq!(by_session["session-c"]["summary_errors"], 1);
    assert_eq!(by_session["20261116-101010-5"]["panics"], 1);
    assert_eq!(by_session["20261116-101010-4"]["panics"], 0);
}

#[test]
fn saturated_bucket_span_stays_sparse() {
    // 2e19 ms is finite, below the 1e20 cap, and above 2^64. Dividing by a 1 ms bucket
    // makes the float-to-u64 cast saturate. The span must not allocate one slot per bucket.
    let text = concat!(
        "{\"t_ms\":0,\"target\":\"quiet\"}\n",
        "{\"t_ms\":2e19,\"target\":\"quiet\"}\n",
    );
    let output = triage(&json!({"bundles":[{"name":"span","files":{"span.jsonl": text}}],
        "settings":{"bucket_seconds":0.001}}))
    .expect("triage");
    let rates = &output["sessions"][0]["rates"];
    assert_eq!(rates["bucket_span"], u64::MAX);
    assert_eq!(rates["buckets"].as_array().expect("buckets").len(), 2);
    assert_eq!(rates["storms"].as_array().expect("storms").len(), 0);
}
