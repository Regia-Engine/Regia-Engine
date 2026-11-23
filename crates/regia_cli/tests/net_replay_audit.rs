// SPDX-License-Identifier: MPL-2.0
//! `regia net replay-audit` (ADR-8513): the real binary audits a sample `.lsreplay` bit-exact
//! (exit 0), names a flipped input's tick and subsystem (exit 1), and refuses a newer version, a
//! truncated file, another game and a missing file with their codes (exit 1).

use std::path::Path;
use std::process::{Command, Output};

use regia_net::lockstep::replay::{LSREPLAY_MAGIC, sample_replay};
use regia_test_utils::temp::TempDir;

fn regia(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_regia"))
        .args(args)
        .output()
        .expect("run regia")
}

fn json(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("one JSON object: {e}\n{stdout}"))
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write");
    path.to_str().expect("utf-8 temp path").to_owned()
}

#[test]
fn replay_audit_passes_a_clean_file_and_names_a_flipped_input() {
    let dir = TempDir::new("replay-audit-cli");
    let clean = write(&dir, "clean.lsreplay", &sample_replay(4, 120, 30, 1, None));
    let out = regia(&["net", "replay-audit", &clean, "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(
        (v["ok"].clone(), v["bit_exact"].clone()),
        (true.into(), true.into())
    );
    assert_eq!(v["frames"], 120);
    assert_eq!(v["hashes_compared"], 120);

    let flipped = write(
        &dir,
        "flipped.lsreplay",
        &sample_replay(4, 120, 30, 1, Some((61, 3))),
    );
    let out = regia(&["net", "replay-audit", &flipped, "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["bit_exact"], false);
    assert_eq!(v["first_diverging_tick"], 61);
    assert_eq!(v["last_agreed_tick"], 60);
    assert_eq!(v["subsystem"], "units");

    let out = regia(&["net", "replay-audit", &flipped]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("first diverging tick 61: subsystem units"),
        "{text}"
    );
    assert!(text.contains("NOT bit-exact"), "{text}");
}

#[test]
fn replay_audit_refuses_with_codes() {
    let dir = TempDir::new("replay-audit-cli-refused");
    let good = sample_replay(2, 30, 10, 5, None);
    let mut newer = good.clone();
    newer[LSREPLAY_MAGIC.len()..LSREPLAY_MAGIC.len() + 4].copy_from_slice(&3u32.to_le_bytes());
    let cases = [
        (write(&dir, "newer.lsreplay", &newer), "replay_version_newer"),
        (
            write(&dir, "short.lsreplay", &good[..good.len() - 2]),
            "replay_truncated",
        ),
        (
            dir.join("missing.lsreplay").to_str().expect("utf-8").to_owned(),
            "replay_not_found",
        ),
    ];
    for (path, code) in &cases {
        let out = regia(&["net", "replay-audit", path, "--json"]);
        assert_eq!(out.status.code(), Some(1), "{path}");
        let v = json(&out);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], *code, "{path}: {v}");
    }
    let out = regia(&["net", "replay-audit", &cases[0].0]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("error[replay_version_newer]") && err.contains("newer build"),
        "{err}"
    );
}
