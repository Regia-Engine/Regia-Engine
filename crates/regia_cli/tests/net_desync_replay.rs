// SPDX-License-Identifier: MPL-2.0
//! `regia net desync-replay` (spec 120 § 7.6, ADR-8455): the real binary replays a sample bundle
//! with its built-in hook and names the planted tick and subsystem; a bundle of another game
//! version, a bundle of an unregistered game and a missing file are refused with their codes.

use std::path::Path;
use std::process::{Command, Output};

use regia_net::desync::DesyncInject;
use regia_net::desync::sample::sample_bundle;
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

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 temp path")
}

#[test]
fn desync_replay_names_the_planted_tick_and_subsystem() {
    let dir = TempDir::new("desync-replay-cli");
    let bundle = sample_bundle(4, 90, 3, Some(DesyncInject::new("units", 61)));
    let path = bundle.write_to_dir(&dir).expect("write the bundle");
    assert!(path_str(&path).ends_with(".desync.ron"));

    let out = regia(&["net", "desync-replay", path_str(&path), "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out);
    assert_eq!(v["ok"], true);
    assert_eq!(v["first_diverging_tick"], 61);
    assert_eq!(v["subsystem"], "units");
    assert_eq!(v["voter"], serde_json::json!({ "seat": 3 }));
    assert_eq!(v["last_agreed_tick"], 60);
    assert_eq!(v["matches_live"], true);

    let out = regia(&["net", "desync-replay", path_str(&path)]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("first diverging tick 61: seat3, subsystem units"),
        "{text}"
    );
    assert!(text.contains("the replay agrees"), "{text}");
}

#[test]
fn desync_replay_refuses_another_game_version_and_unknown_games() {
    let dir = TempDir::new("desync-replay-cli-refused");
    let mut newer = sample_bundle(3, 40, 1, None);
    newer.game.protocol_version += 1;
    newer.game.protocol_id ^= 0xff;
    let path = newer.write_to_dir(&dir).expect("write");
    let out = regia(&["net", "desync-replay", path_str(&path), "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v = json(&out);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "desync_bundle_game_version");
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("protocol version 2"), "{message}");
    assert!(
        message.contains("replay it with the build that wrote it"),
        "{message}"
    );
    // The person's form says the same on stderr.
    let out = regia(&["net", "desync-replay", path_str(&path)]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("error[desync_bundle_game_version]"));

    let mut other = sample_bundle(3, 40, 1, None);
    other.game.game_id = "studio.tactics".into();
    let path = other.write_to_dir(&dir).expect("write");
    let v = json(&regia(&["net", "desync-replay", path_str(&path), "--json"]));
    assert_eq!(v["error"]["code"], "desync_replay_no_hook");

    let missing = dir.join("missing.desync.ron");
    let v = json(&regia(&["net", "desync-replay", path_str(&missing), "--json"]));
    assert_eq!(v["error"]["code"], "desync_bundle_io");
}
