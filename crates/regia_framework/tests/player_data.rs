// SPDX-License-Identifier: MPL-2.0
//! A player's own files never cost them their progress or crash the game (ADR-3102,
//! `book/user-guide/player-data.md`).
//!
//! Table-driven sweeps of the read side of the files the framework writes on the player's machine
//! (save slots and `settings.ron`): truncated, garbage, empty, not-UTF-8, wrong and future format
//! versions, an interrupted write (a stray temp file) and a read-only directory. Each case asserts
//! which data loaded, which files exist afterwards and the warn line. Deterministic: no sleeps,
//! fixed inputs, temp dirs from `unique_temp_dir`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_framework::save_flow::{self, SaveFlowPlugin};
use regia_framework::save_game::{
    SAVE_FORMAT_VERSION, SAVE_MAGIC, SaveCopy, SaveDirectory, SaveError, SaveGame, SavePlugin, SaveSpec,
    Saveable, previous_path,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A saved component whose value tells which copy loaded.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
struct Coins(u32);

/// A `std::io::Write` into a shared buffer: captures the warn lines of one case.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `f` with warn-and-above `tracing` output captured on this thread; returns its result and
/// the captured text.
fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer({
            let sink = sink.clone();
            move || sink.clone()
        })
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = sink
        .0
        .lock()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    (out, text)
}

/// IEEE CRC-32 (independent of the engine's copy), for hand-framed save files.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Whether the platform enforces a read-only directory for this user (not root, not Windows):
/// makes `dir` read-only and probes it. Leaves it read-only when it returns `true`.
#[cfg(unix)]
fn make_read_only(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).is_err() {
        return false;
    }
    let probe = dir.join(".probe");
    if std::fs::write(&probe, b"x").is_ok() {
        // Root (or a file system that ignores modes): the case cannot be staged here.
        let _ = std::fs::remove_file(&probe);
        make_writable(dir);
        return false;
    }
    true
}

#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
}

/// Sorted file names in `dir`.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Saves
// ---------------------------------------------------------------------------

/// A headless app with the save flow over `dir`.
fn save_app(dir: &Path) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SavePlugin::default(), SaveFlowPlugin))
        .register_type::<Coins>()
        .insert_resource(SaveDirectory(dir.to_path_buf()));
    app
}

/// The save-file bytes of a world holding one `Coins(value)`.
fn save_bytes(value: u32) -> Vec<u8> {
    let dir = regia_test_utils::unique_temp_dir("player-data-encode");
    let mut app = save_app(&dir);
    app.world_mut().spawn((Saveable, Coins(value)));
    let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode");
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// The `Coins` values in the world, sorted.
fn coins(app: &mut App) -> Vec<u32> {
    let world = app.world_mut();
    let mut values: Vec<u32> = world.query::<&Coins>().iter(world).map(|c| c.0).collect();
    values.sort_unstable();
    values
}

/// Frames a hand-built payload: magic, `header_version`, payload, CRC of the payload.
fn frame(header_version: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = SAVE_MAGIC.to_vec();
    bytes.extend_from_slice(&header_version.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&crc32(payload).to_le_bytes());
    bytes
}

/// `bytes` with the payload's leading version varint (one byte for small versions) and the
/// header version both set to `version`, and the CRC recomputed: a well-formed file of another
/// format version.
fn restamp(bytes: &[u8], version: u32) -> Vec<u8> {
    let mut payload = bytes[8..bytes.len() - 4].to_vec();
    payload[0] = u8::try_from(version).expect("small version");
    frame(version, &payload)
}

/// What a case expects of a load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// The current copy loads (`Coins(2)`).
    Current,
    /// The previous copy loads (`Coins(1)`) with the fallback warn line.
    Previous,
    /// The load fails with `SaveError::Corrupt`; the world keeps its `Coins(9)`.
    Corrupt,
    /// The load fails with `SaveError::Deserialize` naming the text; no fallback.
    Refused(&'static str),
}

/// One save-load case: what `<slot>.save` holds (`None`: missing), whether a good `.prev` holds
/// `Coins(1)`, extra files staged beside them, and the expected outcome.
struct SaveCase {
    name: &'static str,
    current: Option<Vec<u8>>,
    previous: Option<Vec<u8>>,
    stray: Vec<(&'static str, Vec<u8>)>,
    expect: Expect,
}

fn save_cases() -> Vec<SaveCase> {
    let good = save_bytes(2);
    let prev = save_bytes(1);
    let half = good[..good.len() / 2].to_vec();
    let mut flipped = good.clone();
    let mid = flipped.len() / 2;
    flipped[mid] ^= 0x10;
    let mut header_flip = good.clone();
    header_flip[4..8].copy_from_slice(&7u32.to_le_bytes());
    // Byte 4 is the header version, which the CRC does not cover. Clearing it leaves a current
    // payload (version 1) under a version-0 header. That is header damage, not a version-0 file.
    let mut header_cleared = good.clone();
    header_cleared[4] = 0;
    let future = restamp(&good, SAVE_FORMAT_VERSION + 1);
    // A version no step leads up from. `SAVE_FORMAT_VERSION - 1` (1) has the engine's own step to
    // format 2 (entity references, ADR-3239) and loads, so it cannot stand for "no migration";
    // nothing migrates from version 0, and this test registers no `SaveMigrations` (ADR-7386).
    let older = restamp(&good, 0);
    let case = |name, current: Option<Vec<u8>>, previous: Option<Vec<u8>>, expect| SaveCase {
        name,
        current,
        previous,
        stray: Vec::new(),
        expect,
    };
    vec![
        case("good", Some(good.clone()), Some(prev.clone()), Expect::Current),
        case(
            "truncated",
            Some(half.clone()),
            Some(prev.clone()),
            Expect::Previous,
        ),
        case(
            "garbage",
            Some(b"this is not a save file at all, only text".to_vec()),
            Some(prev.clone()),
            Expect::Previous,
        ),
        case("empty", Some(Vec::new()), Some(prev.clone()), Expect::Previous),
        case(
            "bit flip",
            Some(flipped.clone()),
            Some(prev.clone()),
            Expect::Previous,
        ),
        case(
            "header version flip",
            Some(header_flip),
            Some(prev.clone()),
            Expect::Previous,
        ),
        case(
            "header version cleared over an intact payload",
            Some(header_cleared),
            Some(prev.clone()),
            Expect::Current,
        ),
        case("missing current", None, Some(prev.clone()), Expect::Previous),
        case(
            "future format_version",
            Some(future),
            Some(prev.clone()),
            Expect::Refused("newer than this engine"),
        ),
        case(
            "older format_version without a migration",
            Some(older),
            Some(prev.clone()),
            Expect::Refused("needs a migration"),
        ),
        case(
            "truncated, no previous",
            Some(half.clone()),
            None,
            Expect::Corrupt,
        ),
        case(
            "truncated, previous damaged too",
            Some(half),
            Some(flipped),
            Expect::Corrupt,
        ),
        SaveCase {
            name: "interrupted write (stray temp file)",
            current: Some(good.clone()),
            previous: Some(prev.clone()),
            stray: vec![("Slot.save.tmp.4242.0", good[..10].to_vec())],
            expect: Expect::Current,
        },
        SaveCase {
            name: "interrupted write, no current",
            current: None,
            previous: Some(prev),
            stray: vec![("Slot.save.tmp.4242.1", good)],
            expect: Expect::Previous,
        },
    ]
}

/// Stages one case in a fresh directory; returns the directory and the files staged.
fn stage(case: &SaveCase) -> (PathBuf, Vec<String>) {
    let dir = regia_test_utils::unique_temp_dir("player-data-save");
    if let Some(bytes) = &case.current {
        std::fs::write(dir.join("Slot.save"), bytes).expect("stage current");
    }
    if let Some(bytes) = &case.previous {
        std::fs::write(dir.join("Slot.prev"), bytes).expect("stage previous");
    }
    for (name, bytes) in &case.stray {
        std::fs::write(dir.join(name), bytes).expect("stage stray");
    }
    let staged = files(&dir);
    (dir, staged)
}

/// Checks one load result against the case.
fn check_load(
    case: &SaveCase,
    via: &str,
    result: Result<regia_framework::save_game::SaveResult, SaveError>,
    log: &str,
    app: &mut App,
) {
    let what = format!("{} via {via}", case.name);
    match case.expect {
        Expect::Current => {
            let result = result.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(result.copy, SaveCopy::Current, "{what}");
            assert_eq!(coins(app), vec![2], "{what}");
            assert!(!log.contains("previous copy"), "{what}: no fallback line: {log}");
        }
        Expect::Previous => {
            let result = result.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert_eq!(result.copy, SaveCopy::Previous, "{what}");
            assert_eq!(coins(app), vec![1], "{what}: the previous copy's data");
            assert_eq!(log.matches(" WARN ").count(), 1, "{what}: one warn line: {log}");
            assert!(
                log.contains("save slot Slot: the current copy") && log.contains("loaded the previous copy"),
                "{what}: {log}"
            );
            let reason = if case.current.is_some() {
                "is damaged"
            } else {
                "is missing"
            };
            assert!(log.contains(reason), "{what}: {log}");
        }
        Expect::Corrupt => {
            let err = result.expect_err(&what);
            assert!(matches!(err, SaveError::Corrupt(_)), "{what}: {err}");
            assert_eq!(coins(app), vec![9], "{what}: the world is untouched");
        }
        Expect::Refused(text) => {
            let err = result.expect_err(&what);
            assert!(
                matches!(&err, SaveError::Deserialize(why) if why.contains(text)),
                "{what}: {err}"
            );
            assert_eq!(coins(app), vec![9], "{what}: the world is untouched");
            assert!(!log.contains("previous copy"), "{what}: no fallback: {log}");
        }
    }
    if matches!(case.expect, Expect::Current | Expect::Previous) {
        let last = app
            .world()
            .get_resource::<regia_framework::save_game::LastSaveLoad>()
            .cloned()
            .unwrap_or_else(|| panic!("{what}: LastSaveLoad recorded"));
        assert_eq!(last.slot, "Slot", "{what}");
        assert_eq!(
            last.copy,
            if case.expect == Expect::Current {
                SaveCopy::Current
            } else {
                SaveCopy::Previous
            },
            "{what}"
        );
    }
}

#[test]
fn save_loads_sweep_damage_versions_and_interrupted_writes() {
    for case in save_cases() {
        for via in ["SaveGame::load", "save_flow::load_from_store"] {
            let (dir, staged) = stage(&case);
            let mut app = save_app(&dir);
            app.world_mut().spawn((Saveable, Coins(9)));
            let (result, log) = capture(|| {
                if via == "SaveGame::load" {
                    let slot = SaveGame::slot(&SaveDirectory(dir.clone()), "Slot");
                    SaveGame::load(&slot, app.world_mut(), &SaveSpec::default())
                } else {
                    save_flow::load_from_store(app.world_mut(), "Slot")
                }
            });
            check_load(&case, via, result, &log, &mut app);
            // A load never writes, renames or deletes a file.
            assert_eq!(files(&dir), staged, "{} via {via}: files unchanged", case.name);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[test]
fn a_save_keeps_one_previous_good_generation() {
    let dir = regia_test_utils::unique_temp_dir("player-data-rotate");
    let mut app = save_app(&dir);
    let current = dir.join("Slot.save");
    let previous = previous_path(&current);
    assert_eq!(previous, dir.join("Slot.prev"));

    let mut written = Vec::new();
    for round in 1..=3_u32 {
        app.world_mut().spawn((Saveable, Coins(round)));
        let result = save_flow::save_to_store(app.world_mut(), "Slot").expect("save");
        assert_eq!(result.copy, SaveCopy::Current);
        written.push(std::fs::read(&current).expect("current"));
        let world = app.world_mut();
        let all: Vec<Entity> = world
            .query_filtered::<Entity, With<Coins>>()
            .iter(world)
            .collect();
        for entity in all {
            world.despawn(entity);
        }
    }
    // The newest is current, the one before is `.prev`; the first is gone.
    assert_eq!(std::fs::read(&previous).expect("prev"), written[1]);
    assert_eq!(files(&dir), vec!["Slot.prev", "Slot.save"]);

    // A damaged current file is not rotated over the good `.prev`.
    std::fs::write(&current, b"torn").expect("damage");
    app.world_mut().spawn((Saveable, Coins(4)));
    SaveGame::save(
        &SaveGame::slot(&SaveDirectory(dir.clone()), "Slot"),
        app.world_mut(),
        &SaveSpec::default(),
    )
    .expect("save over damage");
    assert_eq!(
        std::fs::read(&previous).expect("prev"),
        written[1],
        "the good previous copy survives a save over a damaged current file"
    );

    // Listing reports the previous copy; a deliberate delete removes both.
    let listed = save_flow::list_store(app.world()).expect("list");
    assert_eq!(listed.len(), 1);
    assert!(listed[0].has_previous);
    assert!(save_flow::delete_from_store(app.world(), "Slot").expect("delete"));
    assert_eq!(files(&dir), Vec::<String>::new(), "both copies deleted");
    assert!(!save_flow::delete_from_store(app.world(), "Slot").expect("delete twice"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_slot_with_only_a_previous_copy_is_listed_and_deletable() {
    let dir = regia_test_utils::unique_temp_dir("player-data-orphan");
    std::fs::write(dir.join("Lone.prev"), save_bytes(5)).expect("stage");
    let app = save_app(&dir);
    let listed = save_flow::list_store(app.world()).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Lone");
    assert!(listed[0].has_previous);
    assert!(listed[0].header.is_some(), "the previous copy's header");
    let slots = SaveGame::list_slots(&SaveDirectory(dir.clone()));
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].name, "Lone");
    assert!(slots[0].has_previous());
    let deleted = SaveGame::delete(&slots[0]).expect("delete");
    assert!(deleted);
    assert_eq!(files(&dir), Vec::<String>::new());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A save into a read-only directory (disk full and permission denied take the same path) fails
/// with `SaveError::Io`, leaves both copies byte for byte, and leaves no temp file.
#[cfg(unix)]
#[test]
fn a_failed_save_keeps_both_copies_and_reports_an_io_error() {
    let dir = regia_test_utils::unique_temp_dir("player-data-readonly-save");
    let current = save_bytes(2);
    let prev = save_bytes(1);
    std::fs::write(dir.join("Slot.save"), &current).expect("stage");
    std::fs::write(dir.join("Slot.prev"), &prev).expect("stage");
    if !make_read_only(&dir) {
        eprintln!("read-only directories are not enforced here (root?); case skipped");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let mut app = save_app(&dir);
    app.world_mut().spawn((Saveable, Coins(3)));
    let err = save_flow::save_to_store(app.world_mut(), "Slot").expect_err("read-only");
    assert!(matches!(err, SaveError::Io(..)), "{err}");
    let err = SaveGame::save(
        &SaveGame::slot(&SaveDirectory(dir.clone()), "Slot"),
        app.world_mut(),
        &SaveSpec::default(),
    )
    .expect_err("read-only");
    assert!(matches!(err, SaveError::Io(..)), "{err}");
    // A request answers with an error outcome once; nothing retries on later frames.
    app.world_mut()
        .write_message(save_flow::SaveRequest::Save { slot: "Slot".into() });
    app.update();
    app.update();
    let outcomes: Vec<_> = app
        .world_mut()
        .resource_mut::<Messages<save_flow::SaveOutcome>>()
        .drain()
        .collect();
    assert_eq!(outcomes.len(), 1, "one answer, no retry: {outcomes:?}");
    assert!(outcomes[0].result.is_err());
    make_writable(&dir);
    assert_eq!(files(&dir), vec!["Slot.prev", "Slot.save"], "no temp file left");
    assert_eq!(std::fs::read(dir.join("Slot.save")).expect("current"), current);
    assert_eq!(std::fs::read(dir.join("Slot.prev")).expect("prev"), prev);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Windows: a read-only current save file (the attribute set by the player or a sync tool). The
/// save either succeeds with the old bytes kept as `.prev`, or fails with the old file intact;
/// never a lost slot. Run on the Windows rig (report: `cargo test -p regia_framework --test
/// player_data windows_`).
#[cfg(windows)]
#[test]
fn windows_read_only_save_file_never_loses_the_slot() {
    let dir = regia_test_utils::unique_temp_dir("player-data-win-ro-save");
    let current_path = dir.join("Slot.save");
    let current = save_bytes(2);
    std::fs::write(&current_path, &current).expect("stage");
    let mut perms = std::fs::metadata(&current_path).expect("meta").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&current_path, perms).expect("read-only");
    let mut app = save_app(&dir);
    app.world_mut().spawn((Saveable, Coins(3)));
    match save_flow::save_to_store(app.world_mut(), "Slot") {
        Ok(_) => assert_eq!(std::fs::read(dir.join("Slot.prev")).expect("prev"), current),
        Err(err) => {
            assert!(matches!(err, SaveError::Io(..)), "{err}");
            assert_eq!(std::fs::read(&current_path).expect("current"), current);
        }
    }
    for name in files(&dir) {
        let path = dir.join(name);
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)] // Windows test cleanup only
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[cfg(feature = "client")]
mod settings {
    use super::*;
    use regia_framework::settings::{
        GameSettings, SettingsPath, SettingsPersistence, SettingsPlugin, SettingsWriteFailed,
    };
    use regia_framework::user_file::bad_copy_path;

    fn settings_app(path: &Path) -> App {
        let mut app = App::new();
        app.insert_resource(SettingsPath(Some(path.to_path_buf())))
            .add_plugins((MinimalPlugins, SettingsPlugin));
        app.world_mut().resource_mut::<SettingsPersistence>().min_interval = std::time::Duration::ZERO;
        app
    }

    /// A valid settings file with sensitivity 2.5.
    fn good_text() -> String {
        let mut settings = GameSettings::default();
        settings.input.sensitivity = 2.5;
        settings.to_ron().expect("serializes")
    }

    /// What a case expects.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        /// The file loads as it is (sensitivity 2.5), nothing is copied.
        Loads,
        /// The file is moved to `.bad-1`, defaults load (sensitivity 1.0).
        MovedAside,
        /// The known fields load (2.5) and the whole file is copied to `.bad-1`.
        CopiedAside,
    }

    struct Case {
        name: &'static str,
        bytes: Vec<u8>,
        stray: Option<(&'static str, &'static [u8])>,
        outcome: Outcome,
        log: &'static str,
    }

    fn cases() -> Vec<Case> {
        let good = good_text();
        let truncated = good.as_bytes()[..good.len() / 2].to_vec();
        // `settings.ron` has no format_version: a newer game's file shows up as top-level fields
        // this build does not know, a `format_version` among them.
        let future = good.replacen('(', "(format_version: 9, future_section: (x: 1), ", 1);
        let case = |name, bytes: Vec<u8>, outcome, log| Case {
            name,
            bytes,
            stray: None,
            outcome,
            log,
        };
        vec![
            case("good", good.as_bytes().to_vec(), Outcome::Loads, ""),
            case("truncated", truncated, Outcome::MovedAside, "is damaged"),
            case(
                "garbage",
                b"}}}not ron{{{".to_vec(),
                Outcome::MovedAside,
                "is damaged",
            ),
            case("empty", Vec::new(), Outcome::MovedAside, "is damaged"),
            case(
                "not UTF-8",
                vec![b'(', 0xFF, 0xFE],
                Outcome::MovedAside,
                "not UTF-8",
            ),
            case(
                "wrong shape (a field of the wrong type)",
                b"(input: (sensitivity: \"fast\"))".to_vec(),
                Outcome::MovedAside,
                "is damaged",
            ),
            case(
                "future format_version and fields",
                future.into_bytes(),
                Outcome::CopiedAside,
                "does not know",
            ),
            Case {
                name: "interrupted write (stray temp file)",
                bytes: good.as_bytes().to_vec(),
                stray: Some(("settings.ron.tmp.4242.0", b"(input: (sens")),
                outcome: Outcome::Loads,
                log: "",
            },
        ]
    }

    #[test]
    fn settings_reads_sweep_damage_and_never_overwrite_the_players_file() {
        for case in cases() {
            let dir = regia_test_utils::unique_temp_dir("player-data-settings");
            let path = dir.join("settings.ron");
            std::fs::write(&path, &case.bytes).expect("stage");
            if let Some((name, bytes)) = case.stray {
                std::fs::write(dir.join(name), bytes).expect("stray");
            }
            let (mut app, log) = capture(|| settings_app(&path));
            let what = case.name;
            let sensitivity = app.world().resource::<GameSettings>().input.sensitivity;
            let bad = bad_copy_path(&path, 1);
            match case.outcome {
                Outcome::Loads => {
                    assert_eq!(sensitivity, 2.5, "{what}");
                    assert!(!bad.exists(), "{what}");
                    assert!(!log.contains("WARN"), "{what}: {log}");
                }
                Outcome::MovedAside => {
                    assert_eq!(sensitivity, 1.0, "{what}: defaults");
                    assert!(!path.exists(), "{what}: moved, not copied");
                    assert_eq!(std::fs::read(&bad).expect("bad copy"), case.bytes, "{what}");
                }
                Outcome::CopiedAside => {
                    assert_eq!(sensitivity, 2.5, "{what}: the known fields load");
                    assert_eq!(std::fs::read(&path).expect("kept"), case.bytes, "{what}");
                    assert_eq!(std::fs::read(&bad).expect("copy"), case.bytes, "{what}");
                }
            }
            if case.outcome != Outcome::Loads {
                assert_eq!(log.matches(" WARN ").count(), 1, "{what}: one warn line: {log}");
                assert!(
                    log.contains("settings.ron") && log.contains(case.log),
                    "{what}: {log}"
                );
                assert!(
                    log.contains("settings.ron.bad-1"),
                    "{what}: names the copy: {log}"
                );
            }
            assert!(
                app.world().resource::<SettingsPersistence>().blocked().is_none(),
                "{what}: writes stay on"
            );

            // A change after the recovery writes a fresh file; the copy is never touched.
            app.update();
            app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 3.0;
            app.update();
            let text = std::fs::read_to_string(&path).expect("written");
            assert_eq!(
                GameSettings::from_ron(&text).expect("parses").input.sensitivity,
                3.0,
                "{what}"
            );
            if case.outcome != Outcome::Loads {
                assert_eq!(
                    std::fs::read(&bad).expect("copy"),
                    case.bytes,
                    "{what}: copy kept"
                );
            }
            drop(app);

            // The next launch: the newer-fields copy is not made twice.
            let (_again, log) = capture(|| settings_app(&path));
            assert!(!bad_copy_path(&path, 2).exists(), "{what}: {log}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A damaged file in a directory where it cannot be moved aside: defaults load, writes are
    /// off for the session, and the file is byte for byte what it was.
    #[cfg(unix)]
    #[test]
    fn a_damaged_file_that_cannot_be_moved_is_never_written_over() {
        let dir = regia_test_utils::unique_temp_dir("player-data-settings-ro");
        let path = dir.join("settings.ron");
        std::fs::write(&path, b"}}}damaged").expect("stage");
        if !make_read_only(&dir) {
            eprintln!("read-only directories are not enforced here (root?); case skipped");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let (mut app, log) = capture(|| settings_app(&path));
        assert_eq!(app.world().resource::<GameSettings>().input.sensitivity, 1.0);
        assert!(
            log.contains("cannot be moved aside") && log.contains("not saved"),
            "{log}"
        );
        assert_eq!(log.matches(" WARN ").count(), 1, "{log}");
        assert!(app.world().resource::<SettingsPersistence>().blocked().is_some());
        app.update();
        app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 3.0;
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<SettingsPersistence>().writes(),
            0,
            "no write attempt"
        );
        make_writable(&dir);
        assert_eq!(std::fs::read(&path).expect("kept"), b"}}}damaged");
        assert_eq!(files(&dir), vec!["settings.ron"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A good file in a read-only directory: a change fails to write, the game gets one
    /// `SettingsWriteFailed` per attempt, and frames without a change do not retry.
    #[cfg(unix)]
    #[test]
    fn a_failed_settings_write_is_reported_once_and_not_retried() {
        let dir = regia_test_utils::unique_temp_dir("player-data-settings-wf");
        let path = dir.join("settings.ron");
        let good = good_text();
        std::fs::write(&path, &good).expect("stage");
        if !make_read_only(&dir) {
            eprintln!("read-only directories are not enforced here (root?); case skipped");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut app = settings_app(&path);
        app.update();
        let before = app.world().resource::<SettingsPersistence>().writes();
        app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 3.0;
        app.update();
        let failed: Vec<SettingsWriteFailed> = app
            .world_mut()
            .resource_mut::<Messages<SettingsWriteFailed>>()
            .drain()
            .collect();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(failed[0].path, path);
        for _ in 0..5 {
            app.update();
        }
        let persistence = app.world().resource::<SettingsPersistence>();
        assert_eq!(
            persistence.writes(),
            before + 1,
            "one attempt for one change, no retries"
        );
        assert_eq!(persistence.failures(), before + 1);
        let again = app
            .world_mut()
            .resource_mut::<Messages<SettingsWriteFailed>>()
            .drain()
            .count();
        assert_eq!(again, 0, "frames without a change send nothing");
        make_writable(&dir);
        assert_eq!(
            std::fs::read_to_string(&path).expect("kept"),
            good,
            "old value kept"
        );
        assert_eq!(files(&dir), vec!["settings.ron"], "no temp file left");
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows: a read-only `settings.ron` (the attribute) cannot be replaced; the change is
    /// reported and the file keeps its value. Run on the Windows rig (report: `cargo test -p
    /// regia_framework --test player_data windows_`).
    #[cfg(windows)]
    #[test]
    fn windows_read_only_settings_file_keeps_its_value_and_reports() {
        let dir = regia_test_utils::unique_temp_dir("player-data-win-ro-settings");
        let path = dir.join("settings.ron");
        let good = good_text();
        std::fs::write(&path, &good).expect("stage");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).expect("read-only");
        let mut app = settings_app(&path);
        app.update();
        app.world_mut().resource_mut::<GameSettings>().input.sensitivity = 3.0;
        app.update();
        let failed: Vec<SettingsWriteFailed> = app
            .world_mut()
            .resource_mut::<Messages<SettingsWriteFailed>>()
            .drain()
            .collect();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(std::fs::read_to_string(&path).expect("kept"), good);
        drop(app);
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        #[allow(clippy::permissions_set_readonly_false)] // Windows test cleanup only
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(&path, perms);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
