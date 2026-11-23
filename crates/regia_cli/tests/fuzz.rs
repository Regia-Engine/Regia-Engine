// SPDX-License-Identifier: MPL-2.0
//! The CLI-owned text formats under seeded mutation (ADR-1917, ADR-2316, ADR-8403, ADR-8404):
//! `.session.ron`, `regia-engine.release.ron`, `.track.ron` and `release-chunks.json`. Each reader
//! returns a value or an error, never a panic or a hang. `regia_cli` sits above `regia_agent`, so the
//! registry test (`regia_agent/tests/fuzz_targets.rs`) names these tests for
//! `docs/formats/session-ron.md`, `engine-release-ron.md`, `key-track-ron.md` and
//! `release-chunks.md`. The fixtures are the examples on those pages.

use regia_cli::distribution::EngineRelease;
use regia_cli::session::Session;
use regia_test_utils::fuzz::fuzz_text_loader;

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

fn session_load(s: &str) -> Result<String, String> {
    let session = Session::parse(s).map_err(|e| e.to_string())?;
    ron::to_string(&session).map_err(|e| e.to_string())
}

fn release_load(s: &str) -> Result<String, String> {
    let release: EngineRelease = ron::from_str(s).map_err(|e| e.to_string())?;
    let release = release.migrate().map_err(|e| e.message)?;
    ron::to_string(&release).map_err(|e| e.to_string())
}

#[test]
fn session_ron_never_panics() {
    let parsed = Session::parse(SESSION).expect("the documented example parses");
    assert_eq!(parsed.roles.len(), 3);
    // The re-encoding reads back to the same session.
    let again = session_load(&session_load(SESSION).expect("loads")).expect("re-loads");
    assert_eq!(Session::parse(&again).expect("parses"), parsed);
    fuzz_text_loader(SESSION, 0x5345_5353, 2000, session_load);
}

#[test]
fn engine_release_ron_never_panics() {
    assert!(release_load(RELEASE).is_ok(), "the documented example parses");
    let newer = RELEASE.replace("format_version: 1", "format_version: 2");
    assert!(release_load(&newer).is_err(), "a newer manifest is refused");
    fuzz_text_loader(RELEASE, 0x5245_4c45, 2000, release_load);
}

const KEY_TRACK: &str = r#"(
    format_version: 1,
    steps: [
        Section(name: "boot"),
        Tap(target: "input.confirm", millis: 200),
        Expect(pattern: "player ready", within_millis: 1000, optional: false),
        Press(target: "key:Escape"),
    ],
)"#;

fn key_track_load(text: &str) -> Result<String, String> {
    let prepared = regia_cli::key_track::PreparedTrack::parse(text).map_err(|e| e.to_string())?;
    let _replay = prepared.replay_text(text);
    prepared.track().to_ron().map_err(|e| e.to_string())
}

#[test]
fn key_track_ron_never_panics() {
    assert!(key_track_load(KEY_TRACK).is_ok(), "the documented example parses");
    let newer = KEY_TRACK.replace("format_version: 1", "format_version: 2");
    let err = key_track_load(&newer).expect_err("a newer track is refused");
    assert!(err.contains("version"), "{err}");
    // A newer track whose body is no longer v1 still names the version (ADR-8553).
    for body in [
        r#"steps: "no""#,
        r#"steps: [Tap(target: 3)], extra: ()"#,
        r#"lanes: {"a": 1}"#,
    ] {
        let foreign = format!("(\n    format_version: 2,\n    {body},\n)\n");
        let err = key_track_load(&foreign).expect_err("a newer foreign track is refused");
        assert!(
            err.starts_with("track_version") && err.contains("format_version 2"),
            "{err}"
        );
    }
    fuzz_text_loader(KEY_TRACK, 0x4b54_524b, 2000, key_track_load);
}

#[test]
fn cli_format_fingerprints_list_every_reached_type() {
    let session = regia_cli::session::session_schema_fingerprint();
    assert!(!session.split(';').any(str::is_empty), "{session}");
    assert!(
        session.contains("0:{package,profile}"),
        "BuildSpec::Cargo's fields: {session}"
    );
    assert!(session.contains("remote"), "{session}");
    let release = regia_cli::distribution::release::release_schema_fingerprint();
    assert_eq!(
        release,
        "format_version,version,commit,rust_version,crates,files;path,size,blake3"
    );
    assert_eq!(regia_cli::session::session_migration_sources(), vec![1]);
}

const RELEASE_CHUNKS: &str =
    include_str!("../../regia_agent/tests/fixtures/format-corpus/release-chunks/v1.json");

fn release_chunks_load(text: &str) -> Result<String, String> {
    let bytes = regia_release_chunks::migrate_json(text.as_bytes())
        .map_err(|err| format!("{} ({})", err.message, err.code))?;
    String::from_utf8(bytes).map_err(|err| err.to_string())
}

#[test]
fn release_chunks_json_never_panics() {
    let loaded = release_chunks_load(RELEASE_CHUNKS).expect("the corpus fixture parses");
    assert!(loaded.contains("d6ed37677"), "{loaded}");
    let newer = RELEASE_CHUNKS.replacen("\"format_version\": 1", "\"format_version\": 2", 1);
    let err = release_chunks_load(&newer).expect_err("a newer manifest is refused");
    assert!(err.contains('2'), "{err}");
    fuzz_text_loader(RELEASE_CHUNKS, 0x5243_484b, 2000, release_chunks_load);
}

#[test]
fn release_chunks_schema_hash_matches_format_hashes() {
    let hash = blake3::hash(regia_release_chunks::schema_fingerprint().as_bytes())
        .to_hex()
        .to_string();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = std::fs::read_to_string(root.join("docs/formats/format-hashes.ron")).expect("hashes");
    assert!(
        text.contains(&format!("hash: \"{hash}\"")),
        "format-hashes.ron is missing {hash}"
    );
}
