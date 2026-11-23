// SPDX-License-Identifier: MPL-2.0
//! `.session.ron` and the engine release manifest against the old-bytes corpus (ADR-2743): `session` and
//! `engine-release` in `crates/regia_agent/tests/fixtures/format-corpus/`. `regia_cli` sits above the registry's
//! crate (`COVERED_ELSEWHERE`), so the four checks run here.

#[path = "../../regia_agent/tests/format_corpus_support/mod.rs"]
mod format_corpus_support;

use regia_cli::distribution::EngineRelease;
use regia_cli::session::{SESSION_FILE_VERSION, Session};

const SESSION: &str = r#"(
    format_version: 2,
    build: Cargo(package: "arena", profile: "dev"),
    roles: [
        (name: "server", kind: Server, args: ["--port", "{port}"]),
        (name: "client", kind: Client, count: 2, args: ["--connect", "{server}"], render: true),
        (name: "phone", kind: Client, remote: Some("attached")),
    ],
    tests: (filter: ["net_*"], tags: ["smoke"]),
    script: [(role: "client[0]", call: "functest.run", params: Some({"name": "join"}))],
    limit_frames: 600,
    max_restarts: 1,
    allow_errors: ["audio device"],
    keep: (screenshots_on_failure: true, logs: false),
)"#;

const RELEASE: &str = r#"(
    format_version: 1,
    version: "0.2.13",
    commit: Some("f612a1ada"),
    rust_version: "1.99",
    crates: ["regia", "regia_core"],
    files: [
        (path: "Cargo.toml", size: 512, blake3: "0000000000000000000000000000000000000000000000000000000000000000"),
    ],
)"#;

fn session(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    // `parse` validates but keeps the file's own version number; the loader's output is this
    // build's canonical form of the file, so it is stamped at the current version (what
    // `upgrade-project` writes).
    let mut s = Session::parse(text).map_err(|e| e.to_string())?;
    s.format_version = SESSION_FILE_VERSION;
    ron::ser::to_string_pretty(&s, regia_core::ron_pretty())
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

fn release_chunks(bytes: &[u8]) -> Result<Vec<u8>, String> {
    regia_release_chunks::migrate_json(bytes).map_err(|err| format!("{} ({})", err.message, err.code))
}

fn release(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let r: EngineRelease = ron::from_str(text).map_err(|e| e.to_string())?;
    let r = r.migrate().map_err(|e| e.message)?;
    ron::ser::to_string_pretty(&r, regia_core::ron_pretty())
        .map(String::into_bytes)
        .map_err(|e| e.to_string())
}

#[test]
fn bless_missing_current_fixtures() {
    if !format_corpus_support::blessing() {
        return;
    }
    let src =
        "frozen copy of the fixture text of crates/regia_cli/tests/fuzz.rs (the example on the format page)";
    format_corpus_support::bless_current(
        "session",
        SESSION_FILE_VERSION,
        "session.ron",
        SESSION.as_bytes(),
        src,
    );
    format_corpus_support::bless_current("engine-release", 1, "ron", RELEASE.as_bytes(), src);
    format_corpus_support::bless_current(
        "key-track",
        regia_cli::key_track::KEY_TRACK_FORMAT_VERSION,
        "ron",
        KEY_TRACK.as_bytes(),
        src,
    );
}

#[test]
fn every_cli_fixture_loads_round_trips_and_refuses_the_future() {
    let mut problems = format_corpus_support::check_format("session", SESSION_FILE_VERSION, &session);
    problems.extend(format_corpus_support::check_format("engine-release", 1, &release));
    problems.extend(format_corpus_support::check_format(
        "key-track",
        regia_cli::key_track::KEY_TRACK_FORMAT_VERSION,
        &key_track,
    ));
    problems.extend(format_corpus_support::check_format(
        "release-chunks",
        regia_release_chunks::FORMAT_VERSION,
        &release_chunks,
    ));
    assert!(problems.is_empty(), "{problems:#?}");
}

const KEY_TRACK: &str = r#"(
    format_version: 1,
    steps: [
        Section(name: "boot"),
        Note(text: "scripted keys"),
        Tap(target: "input.confirm", millis: 200),
        Expect(pattern: "player ready", within_millis: 1000, optional: false),
        Press(target: "key:Escape"),
        Wait(millis: 50),
        Release(target: "key:Escape"),
        Hold(targets: ["key:Shift"], millis: 100),
        TapN(target: "input.confirm", count: 2, gap_millis: 300),
        Mouse(dx: 1, dy: -2, millis: 300),
        Wheel(notches: -1),
        ReleaseAll,
    ],
)"#;

fn key_track(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    let prepared = regia_cli::key_track::PreparedTrack::parse(text).map_err(|e| e.to_string())?;
    let mut track = prepared.track().clone();
    track.format_version = regia_cli::key_track::KEY_TRACK_FORMAT_VERSION;
    track.to_ron().map(String::into_bytes).map_err(|e| e.to_string())
}
