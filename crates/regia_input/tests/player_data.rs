// SPDX-License-Identifier: MPL-2.0
//! `input_overrides.ron` never costs the player their bindings or blocks rebinding (ADR-3102,
//! `book/user-guide/player-data.md`).
//!
//! A table-driven sweep of the start-up read (`InputOverrides::load_or_recover`, what
//! `InputPlugin` calls): truncated, garbage, empty, not-UTF-8, older and newer `format_version`,
//! an interrupted write (a stray temp file) and a read-only directory. Each case asserts which
//! bindings loaded, which files exist afterwards and the warn line. Deterministic; temp
//! directories carry the process id and an atomic counter.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regia_app::prelude::*;
use regia_input::{
    Binding, InputOverrides, InputPlugin, InputRebinder, InputSource, MAX_BAD_OVERRIDE_COPIES,
    OVERRIDES_PATH, OVERRIDES_VERSION,
};
use regia_platform::{DirMount, MountPoint, Vfs, VfsPath};

/// A fresh directory unique to this process and call (the `unique_temp_dir` rule; this crate
/// does not depend on the test-utils crate).
fn unique_temp_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-input-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

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

/// Runs `f` with warn-and-above output captured on this thread.
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

fn vfs(dir: &Path) -> Vfs {
    let mut vfs = Vfs::new();
    vfs.mount(MountPoint::User, DirMount::writable(dir.to_path_buf()));
    vfs
}

/// A valid overrides file: Jump on `KeyJ`.
fn good() -> InputOverrides {
    let mut overrides = InputOverrides::default();
    overrides.set("Gameplay", "Jump", Binding::new("Jump", InputSource::key("KeyJ")));
    overrides
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The file's bindings load; nothing moves.
    Loads,
    /// The file is renamed to `.bad-1` and the project bindings load (an empty set).
    MovedAside,
}

#[test]
fn overrides_reads_sweep_damage_and_versions() {
    let text = good().to_ron();
    let truncated = text.as_bytes()[..text.len() / 2].to_vec();
    let newer = text.replace(
        &format!("format_version: {OVERRIDES_VERSION}"),
        &format!("format_version: {}", OVERRIDES_VERSION + 1),
    );
    assert_ne!(newer, text, "the file names its format_version");
    // Version 1 had no `format_version` field: it still loads (migrated in memory).
    let older = text.replace(&format!("format_version: {OVERRIDES_VERSION},"), "");
    assert_ne!(older, text);
    // Name, file bytes, an optional stray temp file (name, prefix), the expected outcome and a
    // phrase the warning must contain ("" when none is logged).
    type Case<'a> = (&'a str, Vec<u8>, Option<(&'a str, &'a [u8])>, Outcome, &'a str);
    let cases: Vec<Case<'_>> = vec![
        ("good", text.clone().into_bytes(), None, Outcome::Loads, ""),
        (
            "older format_version (v1)",
            older.into_bytes(),
            None,
            Outcome::Loads,
            "",
        ),
        ("truncated", truncated, None, Outcome::MovedAside, "is unusable"),
        (
            "garbage",
            b"}}} not ron {{{".to_vec(),
            None,
            Outcome::MovedAside,
            "is unusable",
        ),
        ("empty", Vec::new(), None, Outcome::MovedAside, "is unusable"),
        (
            "not UTF-8",
            vec![b'(', 0xFF, 0xFE],
            None,
            Outcome::MovedAside,
            "invalid UTF-8",
        ),
        (
            "newer format_version",
            newer.into_bytes(),
            None,
            Outcome::MovedAside,
            "newer than this build",
        ),
        (
            "interrupted write (stray temp file)",
            text.clone().into_bytes(),
            Some(("input_overrides.ron.tmp.4242.0", b"(format_ver")),
            Outcome::Loads,
            "",
        ),
    ];
    let path = VfsPath::parse(OVERRIDES_PATH).expect("path");
    for (name, bytes, stray, outcome, log_text) in cases {
        let dir = unique_temp_dir("overrides-sweep");
        let file = dir.join("input_overrides.ron");
        std::fs::write(&file, &bytes).expect("stage");
        if let Some((stray_name, stray_bytes)) = stray {
            std::fs::write(dir.join(stray_name), stray_bytes).expect("stray");
        }
        let staged = files(&dir);
        let vfs = vfs(&dir);
        let (loaded, log) = capture(|| InputOverrides::load_or_recover(&vfs, &path));
        let loaded = loaded.unwrap_or_else(|e| panic!("{name}: {e}"));
        let bad = dir.join("input_overrides.ron.bad-1");
        match outcome {
            Outcome::Loads => {
                assert_eq!(loaded.contexts, good().contexts, "{name}");
                assert_eq!(files(&dir), staged, "{name}: nothing moved");
                assert!(!log.contains("WARN"), "{name}: {log}");
            }
            Outcome::MovedAside => {
                assert_eq!(loaded, InputOverrides::default(), "{name}: project bindings");
                assert!(!file.exists(), "{name}");
                assert_eq!(std::fs::read(&bad).expect("bad copy"), bytes, "{name}");
                assert_eq!(log.matches(" WARN ").count(), 1, "{name}: one warn line: {log}");
                assert!(
                    log.contains("input_overrides.ron")
                        && log.contains("input_overrides.ron.bad-1")
                        && log.contains(log_text),
                    "{name}: {log}"
                );
                // The next rebind saves normally; the copy is never touched.
                good().save(&vfs, &path).expect("save after recovery");
                assert_eq!(std::fs::read(&bad).expect("bad copy"), bytes, "{name}");
            }
        }
        // The strict reader (named profiles use it) still reports the damage.
        if outcome == Outcome::MovedAside {
            std::fs::write(&file, &bytes).expect("restage");
            assert!(InputOverrides::load(&vfs, &path).is_err(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn damaged_copies_are_bounded() {
    let dir = unique_temp_dir("overrides-bounded");
    let path = VfsPath::parse(OVERRIDES_PATH).expect("path");
    let vfs = vfs(&dir);
    for round in 1..=(MAX_BAD_OVERRIDE_COPIES + 2) {
        std::fs::write(dir.join("input_overrides.ron"), format!("broken {round}")).expect("stage");
        let (loaded, _) = capture(|| InputOverrides::load_or_recover(&vfs, &path));
        assert!(loaded.is_ok(), "round {round}");
    }
    let names = files(&dir);
    assert_eq!(names.len(), MAX_BAD_OVERRIDE_COPIES as usize, "{names:?}");
    // The two oldest were dropped: `.bad-1` holds round 3, the last copy the newest round.
    let first = std::fs::read_to_string(dir.join("input_overrides.ron.bad-1")).expect("first");
    assert_eq!(first, "broken 3");
    let last =
        std::fs::read_to_string(dir.join(format!("input_overrides.ron.bad-{MAX_BAD_OVERRIDE_COPIES}")))
            .expect("last");
    assert_eq!(last, format!("broken {}", MAX_BAD_OVERRIDE_COPIES + 2));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The plugin's start-up read recovers: rebinding is not blocked after a damaged file.
#[test]
fn the_plugin_recovers_and_keeps_rebinding_on() {
    let dir = unique_temp_dir("overrides-plugin");
    std::fs::write(dir.join("input_overrides.ron"), b"}}} damaged").expect("stage");
    let mut app = App::new();
    app.insert_resource(vfs(&dir));
    let (_, log) = capture(|| {
        app.add_plugins((MinimalPlugins, InputPlugin::default()));
    });
    assert!(log.contains("moved it to"), "{log}");
    assert!(!app.world().resource::<InputRebinder>().persist_blocked());
    assert_eq!(
        files(&dir),
        vec!["input_overrides.ron.bad-1"],
        "the damaged file is kept aside"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A damaged file in a read-only directory cannot be moved: the read fails, the plugin keeps
/// rebinds from saving over it (`persist_blocked`), and the file is byte for byte what it was.
#[cfg(unix)]
#[test]
fn a_damaged_file_that_cannot_be_moved_blocks_saving_over_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = unique_temp_dir("overrides-readonly");
    let file = dir.join("input_overrides.ron");
    std::fs::write(&file, b"}}} damaged").expect("stage");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("read-only");
    if std::fs::write(dir.join(".probe"), b"x").is_ok() {
        eprintln!("read-only directories are not enforced here (root?); case skipped");
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let path = VfsPath::parse(OVERRIDES_PATH).expect("path");
    let err = InputOverrides::load_or_recover(&vfs(&dir), &path).expect_err("cannot move");
    assert!(err.contains("cannot be moved aside"), "{err}");
    let mut app = App::new();
    app.insert_resource(vfs(&dir));
    app.add_plugins((MinimalPlugins, InputPlugin::default()));
    assert!(app.world().resource::<InputRebinder>().persist_blocked());
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    assert_eq!(std::fs::read(&file).expect("kept"), b"}}} damaged");
    assert_eq!(files(&dir), vec!["input_overrides.ron"]);
    let _ = std::fs::remove_dir_all(&dir);
}
