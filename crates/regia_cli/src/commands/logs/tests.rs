// SPDX-License-Identifier: MPL-2.0
use super::*;
use clap::Parser;
use std::fs;

#[derive(Parser)]
struct Parse {
    #[command(flatten)]
    args: TriageArgs,
}

#[test]
fn folder_triage_publishes_without_overwriting() {
    let root = regia_test_utils::unique_temp_dir("logs-triage-cli");
    let input = root.join("input");
    fs::create_dir(&input).expect("input");
    fs::write(
        input.join("sample.jsonl"),
        "{\"t_ms\":0,\"target\":\"regia::exit\",\"fields\":{\"data\":{\"clean\":true}}}\n",
    )
    .expect("log");
    let out = root.join("out");
    let args = Parse::parse_from([
        "test",
        input.to_str().expect("path"),
        "--out",
        out.to_str().expect("path"),
        "--json",
    ])
    .args;
    let report = execute(&args).expect("triage");
    assert_eq!(report["sessions"][0]["exit"], "clean");
    assert!(out.join("sample/sample.jsonl").is_file());
    let before = fs::read(out.join("report.json")).expect("report");
    assert_eq!(
        execute(&args).expect_err("no overwrite").code,
        "logs_triage_output_exists"
    );
    assert_eq!(fs::read(out.join("report.json")).expect("unchanged"), before);
    fs::remove_dir_all(root).expect("cleanup");
}

// Minimal stored ZIP fixtures, original test code using the public ZIP layout already read by apk_size.
fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let entries: Vec<_> = files
        .iter()
        .map(|&(name, bytes)| (name, bytes, 0, 0, false))
        .collect();
    zip_entries(&entries)
}

// (name, content, method 0 stored or 8 deflate, general-purpose flags, corrupt the stored CRC).
fn zip_entries(files: &[(&str, &[u8], u16, u16, bool)]) -> Vec<u8> {
    fn u16_(out: &mut Vec<u8>, value: u16) {
        out.extend(value.to_le_bytes());
    }
    fn u32_(out: &mut Vec<u8>, value: u32) {
        out.extend(value.to_le_bytes());
    }
    fn crc(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for &(name, content, method, flags, bad_crc) in files {
        let stored = if method == 8 {
            use std::io::Write as _;
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(content).expect("deflate");
            encoder.finish().expect("deflate")
        } else {
            content.to_vec()
        };
        let sum = crc(content) ^ u32::from(bad_crc);
        let offset = out.len() as u32;
        u32_(&mut out, 0x04034b50);
        u16_(&mut out, 20);
        u16_(&mut out, flags);
        u16_(&mut out, method);
        u16_(&mut out, 0);
        u16_(&mut out, 0);
        u32_(&mut out, sum);
        u32_(&mut out, stored.len() as u32);
        u32_(&mut out, content.len() as u32);
        u16_(&mut out, name.len() as u16);
        u16_(&mut out, 0);
        out.extend(name.as_bytes());
        out.extend(&stored);
        u32_(&mut central, 0x02014b50);
        u16_(&mut central, 20);
        u16_(&mut central, 20);
        u16_(&mut central, flags);
        u16_(&mut central, method);
        u16_(&mut central, 0);
        u16_(&mut central, 0);
        u32_(&mut central, sum);
        u32_(&mut central, stored.len() as u32);
        u32_(&mut central, content.len() as u32);
        u16_(&mut central, name.len() as u16);
        u16_(&mut central, 0);
        u16_(&mut central, 0);
        u16_(&mut central, 0);
        u16_(&mut central, 0);
        u32_(&mut central, 0);
        u32_(&mut central, offset);
        central.extend(name.as_bytes());
    }
    let offset = out.len() as u32;
    out.extend(&central);
    u32_(&mut out, 0x06054b50);
    u16_(&mut out, 0);
    u16_(&mut out, 0);
    u16_(&mut out, files.len() as u16);
    u16_(&mut out, files.len() as u16);
    u32_(&mut out, central.len() as u32);
    u32_(&mut out, offset);
    u16_(&mut out, 0);
    out
}

#[test]
fn split_upload_stitched_in_order() {
    let root = regia_test_utils::unique_temp_dir("logs-split-upload");
    let manifest = b"session: test-session\nseq: 1\nentry\tbytes\trange\nlogs/app.jsonl.from-0\t25\t0..50\nlogs/app.jsonl.from-50\t25\t50..100\n";
    let bytes = zip(&[
        ("manifest.txt", manifest),
        ("logs/app.jsonl.from-50", b"{\"t_ms\":1000,\"target\":\"last\"}\n"),
        ("logs/app.jsonl.from-0", b"{\"t_ms\":0,\"target\":\"first\"}\n"),
    ]);
    let first = root.join("upload.part01of02.zip");
    let last = root.join("upload.part02of02.zip");
    fs::write(&first, &bytes[..bytes.len() / 2]).expect("part1");
    fs::write(&last, &bytes[bytes.len() / 2..]).expect("part2");
    let args = Parse::parse_from([
        "test",
        last.to_str().expect("path"),
        first.to_str().expect("path"),
        "--json",
    ])
    .args;
    let report = execute(&args).expect("triage split zip");
    assert_eq!(report["sessions"][0]["session"], "test-session");
    assert_eq!(report["sessions"][0]["records"], 2);
    assert_eq!(report["sessions"][0]["source_gaps"], 0);
    assert_eq!(report["files"][0]["sources"][0]["start"], 0);
    assert_eq!(report["files"][0]["sources"][1]["start"], 50);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn missing_parts_traversal_archives_and_archive_budgets_are_refused() {
    let root = regia_test_utils::unique_temp_dir("logs-bad-archive");
    let part = root.join("upload.part01of02.zip");
    fs::write(&part, b"not-complete").expect("part");
    let args = Parse::parse_from(["test", part.to_str().expect("path")]).args;
    assert_eq!(
        execute(&args).expect_err("missing part").code,
        "logs_triage_parts"
    );
    let bad = root.join("bad.zip");
    fs::write(&bad, zip(&[("../escape.jsonl", b"{}\n")])).expect("zip");
    let args = Parse::parse_from(["test", bad.to_str().expect("path")]).args;
    assert_eq!(execute(&args).expect_err("traversal").code, "logs_triage_path");
    fs::write(&bad, zip(&[("a.jsonl", b"{}\n"), ("a.jsonl", b"{}\n")])).expect("zip");
    assert_eq!(
        execute(&args).expect_err("duplicate member").code,
        "logs_triage_path"
    );
    fs::write(&bad, zip(&[("a.jsonl", b"{}\n")])).expect("zip");
    let args = Parse::parse_from(["test", bad.to_str().expect("path"), "--max-bytes", "1"]).args;
    assert_eq!(execute(&args).expect_err("budget").code, "logs_triage_limit");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn discovers_extracted_bundles_once_and_preserves_latest_summary() {
    let root = regia_test_utils::unique_temp_dir("logs-drop-folder");
    for (name, seq, clean) in [("z-older", 1, false), ("a-newer", 2, true)] {
        let folder = root.join(name);
        fs::create_dir_all(folder.join("logs")).expect("folder");
        fs::write(
            folder.join("manifest.txt"),
            format!("session: session\nseq: {seq}\n"),
        )
        .expect("manifest");
        fs::write(
            folder.join("logs/app.summary.json"),
            format!("{{\"exit\":{{\"clean\":{clean}}}}}"),
        )
        .expect("summary");
    }
    let args = Parse::parse_from(["test", root.to_str().expect("path")]).args;
    let report = execute(&args).expect("drop");
    assert_eq!(report["sessions"].as_array().expect("sessions").len(), 1);
    assert_eq!(report["sessions"][0]["exit"], "clean");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn canonical_folder_summaries_stay_with_their_session() {
    let root = regia_test_utils::unique_temp_dir("logs-canonical-summary");
    let cases = [
        (
            "session-a",
            "summary.json",
            "{\"exit\":{\"clean\":true}}",
            "clean",
            0,
        ),
        ("session-b", "summary.partial.json", "{}", "unclean", 0),
        ("session-c", "summary.json", "broken", "unknown", 1),
    ];
    for (folder, summary, body, _, _) in cases {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            dir.join("events.jsonl"),
            "{\"t_ms\":1,\"target\":\"app::start\"}\n",
        )
        .expect("log");
        fs::write(dir.join(summary), body).expect("summary");
    }
    let stamped = root.join("session-a").join("kept-20261116-101010-6.jsonl");
    fs::write(&stamped, "{\"target\":\"app::panic\"}\n").expect("stamp");
    let args = Parse::parse_from(["test", root.to_str().expect("path")]).args;
    let report = execute(&args).expect("folders");
    let mut by_session = std::collections::BTreeMap::new();
    for session in report["sessions"].as_array().expect("sessions") {
        by_session.insert(
            session["session"].as_str().expect("name").to_owned(),
            session.clone(),
        );
    }
    assert_eq!(by_session["session-a"]["exit"], "clean");
    assert_eq!(by_session["session-a"]["panics"], 0);
    assert_eq!(by_session["session-b"]["exit"], "unclean");
    assert_eq!(by_session["session-c"]["exit"], "unknown");
    assert_eq!(by_session["session-c"]["summary_errors"], 1);
    assert_eq!(by_session["20261116-101010-6"]["panics"], 1);
    assert_eq!(by_session.len(), 4);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn manifest_folder_counts_every_visited_entry() {
    let root = regia_test_utils::unique_temp_dir("logs-entry-budget");
    fs::write(root.join("manifest.txt"), "session: bounded\n").expect("manifest");
    fs::write(
        root.join("events.jsonl"),
        "{\"t_ms\":0,\"target\":\"app::start\"}\n",
    )
    .expect("log");
    fs::write(root.join("summary.json"), "{\"exit\":{\"clean\":true}}").expect("summary");
    for i in 0..6 {
        fs::write(root.join(format!("note-{i}.txt")), "ignore").expect("junk");
    }
    // Useful files fit in the budget. Irrelevant entries must still exhaust it.
    let args = Parse::parse_from(["test", root.to_str().expect("path"), "--max-files", "4"]).args;
    assert_eq!(execute(&args).expect_err("budget").code, "logs_triage_limit");
    fs::remove_dir_all(root).expect("cleanup");
}

#[cfg(unix)]
#[test]
fn symlink_bundle_members_are_refused() {
    let root = regia_test_utils::unique_temp_dir("logs-symlink");
    fs::write(root.join("manifest.txt"), "session: session\n").expect("manifest");
    std::os::unix::fs::symlink("manifest.txt", root.join("linked.jsonl")).expect("link");
    let args = Parse::parse_from(["test", root.to_str().expect("path")]).args;
    assert_eq!(execute(&args).expect_err("symlink").code, "logs_triage_path");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn deflated_members_are_read_in_memory_and_integrity_is_checked() {
    let root = regia_test_utils::unique_temp_dir("logs-deflate");
    let log = b"{\"t_ms\":0,\"target\":\"app::start\"}\n{\"t_ms\":5,\"target\":\"app::start\"}\n";
    let good = root.join("good.zip");
    fs::write(&good, zip_entries(&[("logs/app.jsonl", log, 8, 0, false)])).expect("zip");
    let args = Parse::parse_from(["test", good.to_str().expect("path"), "--json"]).args;
    let report = execute(&args).expect("deflated archive");
    assert_eq!(report["sessions"][0]["records"], 2);
    for (name, method, flags, bad_crc) in [
        ("crc.zip", 8, 0, true),
        ("stored-crc.zip", 0, 0, true),
        ("encrypted.zip", 0, 1, false),
        ("method.zip", 12, 0, false),
    ] {
        let path = root.join(name);
        fs::write(&path, zip_entries(&[("app.jsonl", log, method, flags, bad_crc)])).expect("zip");
        let args = Parse::parse_from(["test", path.to_str().expect("path")]).args;
        assert_eq!(
            execute(&args).expect_err(name).code,
            "logs_triage_archive",
            "{name}"
        );
    }
    let truncated = root.join("truncated.zip");
    let bytes = zip_entries(&[("app.jsonl", log, 8, 0, false)]);
    fs::write(&truncated, &bytes[..bytes.len() - 3]).expect("zip");
    let args = Parse::parse_from(["test", truncated.to_str().expect("path")]).args;
    assert_eq!(execute(&args).expect_err("truncated").code, "logs_triage_archive");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn perf_gate_p99_exits_one() {
    let root = regia_test_utils::unique_temp_dir("logs-perf-gate");
    let mut frames = String::new();
    for index in 0..300 {
        if index > 0 {
            frames.push(',');
        }
        let ms = if index < 290 { 16.0 } else { 60.0 };
        frames.push_str(&format!(r#"{{"frame_ms":{ms}}}"#));
    }
    let log = root.join("session.jsonl");
    fs::write(
        &log,
        format!(
            r#"{{"target":"regia::summary","phase":"play","map":"map_a","res_class":"1440p","tier":"HIGH","frames":[{frames}]}}"#
        ),
    )
    .expect("log");
    let thresholds = root.join("perf-gate.toml");
    fs::write(
        &thresholds,
        "[classes.\"1440p\".HIGH.default]\nfps_min = 60.0\nframe_ms_p99_max = 33.0\n",
    )
    .expect("thresholds");
    let output = execute_perf(&PerfGateArgs {
        logs: vec![log],
        thresholds: Some(thresholds),
        baseline: None,
        res: None,
        tier: None,
        regression_pct: 10.0,
        hitch_ms: 25.0,
        json: false,
        run: None,
        update_baseline: false,
    });
    assert_eq!(output.code, 1, "{}", output.stdout);
    assert!(
        output.stdout.lines().any(|line| {
            line.contains("frame p99") && !line.contains("vs baseline") && line.contains("FAIL")
        }),
        "{}",
        output.stdout
    );
    let _ = fs::remove_dir_all(root);
}
