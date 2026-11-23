// SPDX-License-Identifier: MPL-2.0
//! M3 acceptance: edit `hot_fixture`, reload, and check that reflected state survives (specs/20 § 3.9).
//!
//! The test builds `regia-hot-host` with `--features hot-dylib` and `-C prefer-dynamic` into
//! `target/hot-reload-m3`. The script edits the workspace fixture in place (a guard writes the
//! original source back, also after a killed run: [`FixtureGuard`], ADR-8787) so the game and the
//! host resolve the same `regia_dylib`. It adds a
//! field, changes a system body, retypes a field, then breaks the build. A second script covers the
//! rest of the specs/20 § 6 acceptance: a removed field, a renamed system and 20 reloads in one run.
//!
//! The script test (specs/20 § 3.5/3.6/3.9) reloads an unchanged library, changes a script body,
//! then removes a script type, and checks that the game's script slots run the new library's
//! code with their fields carried over.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// The tests edit the one workspace fixture in place, so they run one at a time.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

/// The wall-clock reload time (ms) is measured but never asserted by the
/// correctness test: the salted game rebuild runs as a `cargo rustc` subprocess,
/// so its duration is dominated by how loaded the machine is. A loaded 12-thread
/// gate machine pushed an incremental reload past the old 10 s wall-clock budget.
/// The budget lives in [`hot_fixture_reload_stays_fast`], which is `#[ignore]`d and
/// run in isolation.
#[test]
fn hot_fixture_edits_keep_reflected_state() {
    let (observations, stderr, hot) = run_hot_fixture("state", &script());
    let baseline = obs(&observations, "baseline");
    let add_field = obs(&observations, "add_field");
    let system = obs(&observations, "system");
    let retype = obs(&observations, "retype");
    let failed = obs(&observations, "build_failed");

    assert_eq!(baseline.value("accepted"), "true", "{baseline:?}\n{stderr}");
    assert_eq!(baseline.value("speed"), "4.5");
    assert_eq!(baseline.value("tag"), "12");
    assert_eq!(baseline.value("heading"), "absent");
    assert_eq!(baseline.value("marker"), "1");
    assert_eq!(baseline.value("hits"), "3");
    assert_eq!(baseline.value("movers"), "1");
    assert_eq!(baseline.value("sim"), "7");
    assert_eq!(baseline.value("phase"), "running");

    assert_field_edit(add_field);
    assert!(
        add_field.int("ticks") > 0,
        "thread ticks did not survive: {add_field:?}"
    );
    assert!(add_field.value("detail").contains("Reloaded in"), "{add_field:?}");
    assert!(stderr.contains("Reloaded in"), "{stderr}");

    assert_eq!(system.value("marker"), "2", "{system:?}");
    assert_eq!(system.value("speed"), "4.5");
    assert_eq!(system.value("tag"), "18");
    assert_eq!(system.value("hits"), "9");
    assert_eq!(system.value("heading"), "0");
    assert_eq!(system.value("sim"), "7");
    assert_eq!(system.value("accepted"), "true");
    assert!(
        system.int("ticks") >= add_field.int("ticks"),
        "{system:?} vs {add_field:?}"
    );

    assert_eq!(
        retype.value("speed"),
        "0",
        "retyped field must take Default: {retype:?}"
    );
    assert_eq!(retype.value("tag"), "21");
    assert_eq!(retype.value("hits"), "12");
    assert_eq!(retype.value("heading"), "0");
    assert_eq!(retype.value("marker"), "2");
    assert_eq!(retype.value("movers"), "1");
    assert_eq!(retype.value("sim"), "7");
    assert_eq!(retype.value("accepted"), "true");
    assert!(retype.int("ticks") >= system.int("ticks"));

    assert_eq!(failed.value("accepted"), "false", "{failed:?}\n{stderr}");
    assert_eq!(failed.value("old_running"), "true");
    assert_eq!(failed.value("phase"), "build_failed");
    assert_eq!(failed.value("generation"), retype.value("generation"));
    assert_eq!(failed.value("tag"), "24", "old system must still run: {failed:?}");
    assert_eq!(failed.value("marker"), "2");
    assert_eq!(failed.value("speed"), "0");
    assert_eq!(failed.value("hits"), "15");
    assert_eq!(failed.value("heading"), "0");
    assert_eq!(failed.value("sim"), "7");

    let ext = if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    // `game-<N>-<pid>-<serial>.<ext>` (ADR-2396): unique across processes sharing the hot dir.
    // The host is a child process, so only the generation part is known here.
    let salted = "game-4-";
    assert!(
        std::fs::read_dir(&hot)
            .expect("hot dir")
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(salted) && name.ends_with(&format!(".{ext}"))
            }),
        "salted copy missing"
    );
    assert!(hot.join("last_snapshot.bin").is_file(), "snapshot missing");
    assert!(hot.join("status.json").is_file(), "status missing");

    eprintln!(
        "hot reload save→running ms: add_field={} system={} retype={} build_failed={}",
        add_field.ms(),
        system.ms(),
        retype.ms(),
        failed.ms()
    );
}

/// Specs/20 § 6 (ADR-5627): the M11 acceptance edits the first test does not make. A field is
/// added and then **removed** (the removed field is dropped, the others keep their values), the
/// system is **renamed** with a new body (only the new system runs: `hits` grows by exactly one per
/// frame, so the old one was removed), and the run goes on to **20 reloads** in one host process,
/// each accepted, with the reflected state, the thread state and the engine-owned resource intact.
#[test]
fn hot_fixture_removes_a_field_renames_a_system_and_survives_twenty_reloads() {
    let (observations, stderr, hot) = run_hot_fixture("acceptance", &acceptance_script());
    let baseline = obs(&observations, "baseline");
    assert_eq!(baseline.value("accepted"), "true", "{baseline:?}\n{stderr}");
    assert_eq!(baseline.value("hits"), "3");
    assert_eq!(baseline.value("tag"), "12");

    let added = obs(&observations, "added");
    assert_eq!(added.value("accepted"), "true", "{added:?}\n{stderr}");
    assert_eq!(added.value("heading"), "0", "new field takes Default: {added:?}");

    let removed = obs(&observations, "removed");
    assert_eq!(removed.value("accepted"), "true", "{removed:?}\n{stderr}");
    assert_eq!(
        removed.value("heading"),
        "absent",
        "removed field is dropped: {removed:?}"
    );
    assert_eq!(removed.value("speed"), "4.5", "{removed:?}");
    assert_eq!(removed.value("tag"), "18", "{removed:?}");
    assert_eq!(removed.value("hits"), "9", "{removed:?}");
    assert_eq!(removed.value("marker"), "1", "{removed:?}");

    let renamed = obs(&observations, "renamed");
    assert_eq!(renamed.value("accepted"), "true", "{renamed:?}\n{stderr}");
    assert_eq!(
        renamed.value("marker"),
        "3",
        "the renamed system runs: {renamed:?}"
    );
    assert_eq!(
        renamed.value("hits"),
        "12",
        "exactly one system counts per frame (the old one is gone): {renamed:?}"
    );
    assert_eq!(renamed.value("tag"), "21", "{renamed:?}");
    assert_eq!(renamed.value("speed"), "4.5", "{renamed:?}");

    let last = obs(&observations, "twenty");
    assert_eq!(last.value("accepted"), "true", "{last:?}\n{stderr}");
    assert_eq!(last.value("generation"), "20", "{last:?}");
    assert_eq!(last.value("phase"), "running", "{last:?}");
    assert_eq!(last.value("hits"), "28", "{last:?}");
    assert_eq!(last.value("tag"), "37", "{last:?}");
    assert_eq!(last.value("marker"), "3", "{last:?}");
    assert_eq!(last.value("speed"), "4.5", "{last:?}");
    assert_eq!(
        last.value("movers"),
        "1",
        "no entity duplicated or lost: {last:?}"
    );
    assert_eq!(
        last.value("sim"),
        "7",
        "the engine-owned resource is untouched: {last:?}"
    );
    assert!(
        last.int("ticks") >= renamed.int("ticks"),
        "thread ticks survive every reload: {last:?} vs {renamed:?}"
    );
    let ext = if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let copies = std::fs::read_dir(&hot)
        .expect("hot dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("game-") && name.ends_with(&format!(".{ext}"))
        })
        .count();
    assert_eq!(copies, 20, "one uniquely named copy per reload, none replaced");
}

/// Spec 20 § 3.5/3.6/3.9: game script slots are snapshotted, torn down with the old library and
/// rebuilt from the new one.
///
/// - an unchanged library: the scripts keep ticking from their carried-over fields, nothing drops;
/// - a changed script body: the next tick runs the new body (`stamp` 3), `count` carries over;
/// - a removed script type: its slot is dropped with a warning and its registry entry is gone
///   (2 entries before, 1 after), while the remaining script keeps running.
#[test]
fn hot_fixture_reload_rebuilds_game_scripts() {
    let fixture = workspace_root().join("samples/hot_fixture/src/lib.rs");
    let source = std::fs::read_to_string(&fixture).expect("read fixture");
    let blocks = marked_blocks(&source, "// spinner-begin", "// spinner-end");
    assert_eq!(blocks.len(), 3, "the fixture marks Spinner in three places");
    let mut script = String::new();
    script.push_str("reload\nframes 3\nobserve baseline\n");
    script.push_str("reload\nframes 1\nobserve unchanged\n");
    push_replace(
        &mut script,
        "        self.stamp = 1;\n",
        "        self.stamp = 3;\n",
    );
    script.push_str("reload\nframes 1\nobserve body\n");
    for block in &blocks {
        push_replace(&mut script, block, "");
    }
    script.push_str("reload\nframes 1\nobserve removed\n");

    let (observations, stderr, _hot) = run_hot_fixture("scripts", &script);
    let baseline = obs(&observations, "baseline");
    let unchanged = obs(&observations, "unchanged");
    let body = obs(&observations, "body");
    let removed = obs(&observations, "removed");
    let row = |o: &Obs| {
        [
            "accepted", "blink", "stamp", "spin", "blinkers", "spinners", "registry",
        ]
        .map(|key| o.value(key).to_string())
    };
    let expect = |values: [&str; 7]| values.map(str::to_string);
    let keys = "(accepted, blink, stamp, spin, blinkers, spinners, registry)";
    assert_eq!(
        row(baseline),
        expect(["true", "3", "1", "3", "1", "1", "2"]),
        "baseline {keys}: {baseline:?}\n{stderr}"
    );
    assert_eq!(
        row(unchanged),
        expect(["true", "4", "1", "4", "1", "1", "2"]),
        "unchanged library {keys}: {unchanged:?}\n{stderr}"
    );
    assert_eq!(
        row(body),
        expect(["true", "5", "3", "5", "1", "1", "2"]),
        "changed script body {keys}: {body:?}\n{stderr}"
    );
    assert_eq!(
        row(removed),
        expect(["true", "6", "3", "5", "1", "0", "1"]),
        "removed script type {keys}: {removed:?}\n{stderr}"
    );
    // The reload report's counts (`scripts_rebuilt` / `scripts_dropped`, the agent surface).
    let counts = |o: &Obs| (o.int("rebuilt"), o.int("dropped"));
    assert_eq!(
        counts(baseline),
        (0, 0),
        "the first load restores no scripts: {baseline:?}"
    );
    assert_eq!(counts(unchanged), (2, 0), "{unchanged:?}");
    assert_eq!(counts(body), (2, 0), "{body:?}");
    assert_eq!(counts(removed), (1, 1), "{removed:?}");
    assert!(
        stderr.contains("dropped script slots whose type disappeared"),
        "the dropped slot is warned about\n{stderr}"
    );
    assert!(stderr.contains("hot_fixture::Spinner"), "{stderr}");
}

/// Every `begin`..`end` marked block of `source`, from the start of the `begin` line through the
/// end of the `end` line.
fn marked_blocks(source: &str, begin: &str, end: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut offset = 0;
    while let Some(found) = source[offset..].find(begin) {
        let at = offset + found;
        let line_start = source[..at].rfind('\n').map_or(0, |index| index + 1);
        let Some(close) = source[at..].find(end) else {
            break;
        };
        let mut stop = at + close + end.len();
        if source[stop..].starts_with('\n') {
            stop += 1;
        }
        blocks.push(source[line_start..stop].to_string());
        offset = stop;
    }
    blocks
}

/// Perf budget for the incremental reloads, split from the correctness test.
///
/// The `ms` values are wall-clock time of the salted game rebuild, which tracks
/// machine load. This budget is run in isolation (the bench), where wall time is
/// a load-independent signal: `cargo test -p regia_editor --test hot_reload
/// -- --ignored`.
#[test]
#[ignore = "perf budget: incremental hot-reload rebuilds under 10 s (run with -- --ignored)"]
fn hot_fixture_reload_stays_fast() {
    let (observations, stderr, _hot) = run_hot_fixture("fast", &script());
    let add_field = obs(&observations, "add_field");
    let system = obs(&observations, "system");
    let retype = obs(&observations, "retype");
    assert!(
        add_field.ms() < 10_000,
        "add_field took {} ms\n{stderr}",
        add_field.ms()
    );
    assert!(system.ms() < 10_000, "system took {} ms\n{stderr}", system.ms());
    assert!(retype.ms() < 10_000, "retype took {} ms\n{stderr}", retype.ms());
}

/// Builds `regia-hot-host`, runs `script` once, and returns the parsed observations, the
/// host's stderr, and the hot directory. `label` names the temp work dir so the tests never
/// collide; the fixture source is held for the whole run ([`FIXTURE_LOCK`]).
fn run_hot_fixture(label: &str, script: &str) -> (Vec<Obs>, String, PathBuf) {
    // A test that panicked while holding the lock has already restored the source.
    let _turn = FIXTURE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let workspace = workspace_root();
    let target = workspace.join("target/hot-reload-m3");
    let fixture_src = workspace.join("samples/hot_fixture/src/lib.rs");
    let restore = FixtureGuard::acquire(&fixture_src, &target);
    assert_committed_fixture(&fixture_src, &restore.original);
    let work = std::env::temp_dir().join(format!("regia-hot-m3-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("work dir");
    let script_path = work.join("script.txt");
    std::fs::write(&script_path, script).expect("script");

    let hot = work.join(".regia").join("hot");
    build_host(&workspace, &target);
    #[cfg(target_os = "macos")]
    warm_game_build(&workspace, &target);

    let output = run_host(&workspace, &target, &hot, &script_path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "host failed {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    let observations = parse_observations(&stdout);
    (observations, stderr, hot)
}

fn assert_field_edit(add_field: &Obs) {
    assert_eq!(add_field.value("accepted"), "true", "{add_field:?}");
    assert_eq!(add_field.value("heading"), "0", "new field must take Default");
    assert_eq!(add_field.value("speed"), "4.5");
    assert_eq!(add_field.value("tag"), "15");
    assert_eq!(add_field.value("hits"), "6");
    assert_eq!(add_field.value("marker"), "1");
    assert_eq!(add_field.value("movers"), "1");
    assert_eq!(add_field.value("sim"), "7");
    assert_eq!(add_field.value("phase"), "running");
}

fn script() -> String {
    let mut out = String::new();
    out.push_str("reload\nframes 3\nobserve baseline\n");
    push_replace(
        &mut out,
        "    pub tag: u32,\n",
        "    pub tag: u32,\n    pub heading: f32,\n",
    );
    out.push_str("reload\nframes 3\nobserve add_field\n");
    push_replace(&mut out, "    score.marker = 1;\n", "    score.marker = 2;\n");
    out.push_str("reload\nframes 3\nobserve system\n");
    push_replace(&mut out, "    pub speed: f32,\n", "    pub speed: f64,\n");
    out.push_str("reload\nframes 3\nobserve retype\n");
    out.push_str("append\nfn hot_reload_syntax_error() { break }\n===\n");
    out.push_str("reload\nframes 3\nobserve build_failed\n");
    out
}

/// The script of [`hot_fixture_removes_a_field_renames_a_system_and_survives_twenty_reloads`]:
/// add `heading`, remove it, rename `move_bodies` with a new body, then reload until the host
/// has mapped 20 libraries (one frame after each).
fn acceptance_script() -> String {
    let mut out = String::new();
    out.push_str("reload\nframes 3\nobserve baseline\n");
    push_replace(
        &mut out,
        "    pub tag: u32,\n",
        "    pub tag: u32,\n    pub heading: f32,\n",
    );
    out.push_str("reload\nframes 3\nobserve added\n");
    push_replace(&mut out, "    pub heading: f32,\n", "");
    out.push_str("reload\nframes 3\nobserve removed\n");
    push_replace(
        &mut out,
        "    app.add_systems(Update, move_bodies);\n",
        "    app.add_systems(Update, move_bodies_renamed);\n",
    );
    push_replace(
        &mut out,
        "fn move_bodies(mut score: ResMut<Score>, mut bodies: Query<&mut Mover>) {\n",
        "fn move_bodies_renamed(mut score: ResMut<Score>, mut bodies: Query<&mut Mover>) {\n",
    );
    push_replace(&mut out, "    score.marker = 1;\n", "    score.marker = 3;\n");
    out.push_str("reload\nframes 3\nobserve renamed\n");
    // Reloads 5..=20 with no further edit: each is a new salted build and a full swap.
    for _ in 5..=20 {
        out.push_str("reload\nframes 1\n");
    }
    out.push_str("observe twenty\n");
    out
}

fn push_replace(out: &mut String, from: &str, to: &str) {
    out.push_str("replace\n");
    out.push_str(from);
    out.push_str("---\n");
    out.push_str(to);
    out.push_str("===\n");
}

/// Holds the workspace fixture for one run and writes the original source back, including after a
/// run that was killed (ADR-8787).
///
/// `Drop` restores the source after a normal end or a panic, but a SIGKILL (the merge gate's
/// per-binary timeout, a Ctrl-C) skips it, and the killed test's `regia-hot-host` child kept
/// editing the fixture. The v0.6.0 gate on a03439ea2 hit exactly that: the run beside load timed
/// out in the acceptance script, the fixture stayed renamed (`move_bodies_renamed`,
/// `marker = 3`), and the rerun alone failed with "pattern not found" before any assertion.
///
/// So the guard also keeps a journal: before the first edit it writes the original source to
/// `<target>/hot-fixture-lib.rs.journal` (temp file + rename, so it is never half written), and
/// it removes the journal only after the source is back. A run that finds a journal knows the
/// previous run died while editing: it copies the edited source to
/// `<target>/hot-fixture-lib.rs.interrupted` (nothing is lost) and restores the journal first.
/// An exclusive file lock on `<target>/hot-fixture.lock` serializes runs across processes (the
/// in-process [`FIXTURE_LOCK`] only covers threads); the OS releases it when a process dies.
///
/// ADR-10281: the journal lives in `target/`, which can outlive the source tree: the v0.6.1 gate
/// rebuilt its git-less source tree over a warm target that still held a journal from a run killed
/// under the pre-SPDX fixture, and the guard "restored" that stale text over the new committed
/// fixture (and `Drop` wrote it back), so the SPDX check failed after the hot reload tests. The
/// journal therefore records the fixture file's identity ([`file_identity`]). The host and this
/// guard edit the file in place, which keeps its identity; a checkout or an archive extraction
/// replaces the file, which changes it. A journal whose identity differs is stale: it is set aside
/// as `<target>/hot-fixture-lib.rs.stale` and the current source is the original.
struct FixtureGuard {
    path: PathBuf,
    journal: PathBuf,
    original: String,
    /// Held for the whole run; `None` only in [`FixtureGuard::abandon`].
    lock: Option<std::fs::File>,
}

/// The journal file in the hot reload target dir: [`JOURNAL_HEADER`], the fixture's identity,
/// a newline, then the original source.
const JOURNAL: &str = "hot-fixture-lib.rs.journal";
/// The first word of the journal's first line (format 1).
const JOURNAL_HEADER: &str = "regia-hot-fixture-journal-1";
/// The journal before ADR-10281: the original source alone, no identity.
const LEGACY_JOURNAL: &str = "hot-fixture-lib.rs.pristine";
/// Where a stale journal is set aside (nothing is lost).
const STALE_JOURNAL: &str = "hot-fixture-lib.rs.stale";

/// The identity of the file at `path`: device, inode and birth time on Unix, birth time elsewhere.
/// An in-place write (`std::fs::write` truncates) keeps it; replacing the file changes it. Where
/// the platform reports none of them the identity is constant, and the guard restores as before.
fn file_identity(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else {
        return "missing".to_string();
    };
    let born = meta
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(|| "none".to_string(), |since| since.as_nanos().to_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("dev={} ino={} born={born}", meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        format!("born={born}")
    }
}

impl FixtureGuard {
    fn acquire(path: &Path, target: &Path) -> Self {
        std::fs::create_dir_all(target).expect("hot reload target dir");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(target.join("hot-fixture.lock"))
            .expect("open fixture lock");
        lock.lock().expect("lock fixture");
        let journal = target.join(JOURNAL);
        let mut current = std::fs::read_to_string(path).expect("read fixture");
        let legacy = target.join(LEGACY_JOURNAL);
        let legacy_text = match std::fs::read_to_string(&legacy) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("read fixture journal {}: {error}", legacy.display()),
        };
        if let Some(pristine) = legacy_text.as_deref()
            && current != pristine
        {
            // No identity to tell a killed run from a replaced source: restore only a source that
            // is visibly not the committed fixture, keep any other as the original.
            if fixture_looks_committed(&current) {
                set_aside(
                    &legacy,
                    target,
                    path,
                    "it predates the identity check and the source differs",
                );
            } else {
                keep_interrupted_and_restore(path, target, &legacy, &current, pristine);
                current = pristine.to_string();
            }
        }
        let original = match std::fs::read_to_string(&journal) {
            Ok(text) => {
                let (header, pristine) = text.split_once('\n').unwrap_or((text.as_str(), ""));
                let recorded = header.strip_prefix(JOURNAL_HEADER).map(str::trim);
                if recorded == Some(file_identity(path).as_str()) {
                    if current != pristine {
                        keep_interrupted_and_restore(path, target, &journal, &current, pristine);
                    }
                    pristine.to_string()
                } else {
                    set_aside(
                        &journal,
                        target,
                        path,
                        "the source file was replaced after it was written",
                    );
                    write_journal(path, target, &current);
                    current
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_journal(path, target, &current);
                current
            }
            Err(error) => panic!("read fixture journal {}: {error}", journal.display()),
        };
        if legacy_text.is_some() {
            // The new journal (written above or kept) now protects the source.
            std::fs::remove_file(&legacy).ok();
        }
        Self {
            path: path.to_path_buf(),
            journal,
            original,
            lock: Some(lock),
        }
    }

    /// What a SIGKILL does to a run: the OS releases the lock, nothing is restored and the
    /// journal stays.
    fn abandon(mut self) {
        drop(self.lock.take());
        std::mem::forget(self);
    }
}

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        match std::fs::write(&self.path, &self.original) {
            // Only a restored source retires the journal; otherwise the next run restores it.
            Ok(()) => {
                if let Err(error) = std::fs::remove_file(&self.journal) {
                    eprintln!("failed to remove {}: {error}", self.journal.display());
                }
            }
            Err(error) => eprintln!("failed to restore {}: {error}", self.path.display()),
        }
    }
}

/// Publishes the journal for `path` (temp file + rename, so it is never half written).
fn write_journal(path: &Path, target: &Path, original: &str) {
    let temp = target.join(format!("hot-fixture-lib.rs.{}.tmp", std::process::id()));
    let text = format!("{JOURNAL_HEADER} {}\n{original}", file_identity(path));
    std::fs::write(&temp, text).expect("write fixture journal");
    std::fs::rename(&temp, target.join(JOURNAL)).expect("publish fixture journal");
}

/// A killed run left `path` edited: keep the edited copy, then write `pristine` back in place.
fn keep_interrupted_and_restore(path: &Path, target: &Path, journal: &Path, current: &str, pristine: &str) {
    let kept = target.join("hot-fixture-lib.rs.interrupted");
    std::fs::write(&kept, current).expect("keep interrupted fixture");
    std::fs::write(path, pristine).expect("restore fixture from journal");
    eprintln!(
        "restored {} from {}: an earlier run was killed while editing it (its edited copy is {})",
        path.display(),
        journal.display(),
        kept.display()
    );
}

/// Moves a stale `journal` to [`STALE_JOURNAL`] so it is never restored over `path`.
fn set_aside(journal: &Path, target: &Path, path: &Path, why: &str) {
    let stale = target.join(STALE_JOURNAL);
    std::fs::rename(journal, &stale).expect("set aside stale fixture journal");
    eprintln!(
        "kept {} as it is and set aside the journal {} as {}: {why}",
        path.display(),
        journal.display(),
        stale.display()
    );
}

/// Whether `source` has every line the scripts replace (the script test's too: `stamp = 1` and the
/// three marked `Spinner` blocks) and none of the lines they add.
fn fixture_looks_committed(source: &str) -> bool {
    FIXTURE_ANCHORS.iter().all(|anchor| source.contains(anchor))
        && source.contains("        self.stamp = 1;\n")
        && source.matches("// spinner-begin").count() == 3
        && !source.contains("pub heading")
        && !source.contains("hot_reload_syntax_error")
}

/// Lines the edit scripts replace, as committed.
const FIXTURE_ANCHORS: [&str; 5] = [
    "    pub speed: f32,\n",
    "    pub tag: u32,\n",
    "    score.marker = 1;\n",
    "    app.add_systems(Update, move_bodies);\n",
    "fn move_bodies(mut score: ResMut<Score>, mut bodies: Query<&mut Mover>) {\n",
];

/// Fails with the cause, before any build, when the fixture the run starts from is not the
/// committed one: an edited copy left by a run killed before [`FixtureGuard`] kept a journal (the
/// v0.6.0 gate tree) or a hand edit. Without this the host only reported "pattern not found".
fn assert_committed_fixture(path: &Path, source: &str) {
    let missing: Vec<&str> = FIXTURE_ANCHORS
        .iter()
        .copied()
        .filter(|anchor| !source.contains(anchor))
        .collect();
    assert!(
        missing.is_empty() && !source.contains("pub heading"),
        "{} is not the committed fixture (left edited by a killed run, or edited by hand): \
         restore it (`git checkout -- samples/hot_fixture`) and rerun; missing lines: {missing:?}",
        path.display()
    );
}

/// ADR-8787: the committed fixture has every line the scripts replace, and the state the v0.6.0
/// gate tree was left in (the acceptance script's rename, `marker = 3`) is refused by name.
#[test]
fn an_edited_fixture_is_refused_before_the_build() {
    let source = {
        let _turn = FIXTURE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let workspace = workspace_root();
        let guard = FixtureGuard::acquire(
            &workspace.join("samples/hot_fixture/src/lib.rs"),
            &workspace.join("target/hot-reload-m3"),
        );
        guard.original.clone()
    };
    let path = Path::new("samples/hot_fixture/src/lib.rs");
    assert_committed_fixture(path, &source);
    let renamed = source
        .replacen("    score.marker = 1;\n", "    score.marker = 3;\n", 1)
        .replace("move_bodies", "move_bodies_renamed");
    let refused = std::panic::catch_unwind(|| assert_committed_fixture(path, &renamed));
    assert!(
        refused.is_err(),
        "the gate tree's renamed fixture must be refused"
    );
    let added = source.replacen(
        "    pub tag: u32,\n",
        "    pub tag: u32,\n    pub heading: f32,\n",
        1,
    );
    assert!(std::panic::catch_unwind(|| assert_committed_fixture(path, &added)).is_err());
}

/// ADR-8787: a run killed mid-edit (no `Drop`) leaves the journal and an edited source; the next
/// guard restores the original before it hands the file out, keeps the edited copy, and retires
/// the journal when it is done. Uses a scratch "fixture", never the workspace one.
#[test]
fn fixture_guard_restores_a_source_left_edited_by_a_killed_run() {
    let dir = regia_test_utils::TempDir::new("hot-fixture-guard");
    let target = dir.join("target");
    let source = dir.join("lib.rs");
    let pristine = "pub fn move_bodies() {\n    score.marker = 1;\n}\n";
    std::fs::write(&source, pristine).unwrap();

    // A run that ends normally leaves the source and no journal.
    {
        let _guard = FixtureGuard::acquire(&source, &target);
        assert!(target.join(JOURNAL).is_file());
        std::fs::write(&source, "edited\n").unwrap();
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), pristine);
    assert!(!target.join(JOURNAL).exists());

    // A killed run: the guard is never dropped, the edit stays.
    let killed = FixtureGuard::acquire(&source, &target);
    let renamed = "pub fn move_bodies_renamed() {\n    score.marker = 3;\n}\n";
    std::fs::write(&source, renamed).unwrap();
    killed.abandon();

    {
        let _guard = FixtureGuard::acquire(&source, &target);
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            pristine,
            "the next run starts from the original source"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("hot-fixture-lib.rs.interrupted")).unwrap(),
            renamed,
            "the edited copy is kept"
        );
        std::fs::write(&source, "edited again\n").unwrap();
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), pristine);
    assert!(!target.join(JOURNAL).exists());
}

/// ADR-10281: a journal left by a killed run is stale once the source file is replaced (a new
/// checkout or archive extraction under a warm target, the v0.6.1 gate tree): the guard keeps the
/// new source, sets the journal aside, and the run ends with the new source. Scratch files only.
#[test]
fn fixture_guard_never_restores_a_journal_over_a_replaced_source() {
    let dir = regia_test_utils::TempDir::new("hot-fixture-stale");
    let target = dir.join("target");
    let source = dir.join("lib.rs");
    let old = "pub fn move_bodies() {\n    score.marker = 1;\n}\n";
    std::fs::write(&source, old).unwrap();
    let killed = FixtureGuard::acquire(&source, &target);
    std::fs::write(&source, "edited\n").unwrap();
    killed.abandon();

    // The new tree's committed source replaces the file (written beside it, renamed over it).
    let new = "// SPDX-License-Identifier: MIT-0\npub fn move_bodies() {\n    score.marker = 1;\n}\n";
    let checkout = dir.join("lib.rs.checkout");
    std::fs::write(&checkout, new).unwrap();
    std::fs::rename(&checkout, &source).unwrap();
    {
        let guard = FixtureGuard::acquire(&source, &target);
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            new,
            "the replaced source is kept"
        );
        assert_eq!(guard.original, new, "and is what the run restores");
        let stale = std::fs::read_to_string(target.join(STALE_JOURNAL)).unwrap();
        assert!(
            stale.starts_with(JOURNAL_HEADER) && stale.ends_with(old),
            "{stale:?}"
        );
        assert!(
            target.join(JOURNAL).is_file(),
            "a fresh journal protects the new source"
        );
        std::fs::write(&source, "edited again\n").unwrap();
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), new);
    assert!(!target.join(JOURNAL).exists());
}

/// ADR-10281: a journal from before the identity check (`hot-fixture-lib.rs.pristine`, no
/// identity) is restored only over a source that is visibly edited; a committed-looking source
/// (the v0.6.1 gate tree: the pre-SPDX journal beside the SPDX fixture) is kept. Scratch files only.
#[test]
fn fixture_guard_restores_a_legacy_journal_only_over_an_edited_source() {
    let dir = regia_test_utils::TempDir::new("hot-fixture-legacy");
    let target = dir.join("target");
    let source = dir.join("lib.rs");
    std::fs::create_dir_all(&target).unwrap();
    let body = format!(
        "{}        self.stamp = 1;\n{}",
        FIXTURE_ANCHORS.concat(),
        "// spinner-begin\n// spinner-end\n".repeat(3)
    );
    let committed = format!("// SPDX-License-Identifier: MIT-0\n{body}");
    assert!(fixture_looks_committed(&committed) && fixture_looks_committed(&body));

    std::fs::write(&source, &committed).unwrap();
    std::fs::write(target.join(LEGACY_JOURNAL), &body).unwrap();
    {
        let guard = FixtureGuard::acquire(&source, &target);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), committed);
        assert_eq!(guard.original, committed);
        assert_eq!(std::fs::read_to_string(target.join(STALE_JOURNAL)).unwrap(), body);
        assert!(!target.join(LEGACY_JOURNAL).exists());
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), committed);

    // The old guard's killed acceptance run: renamed system, `marker = 3`.
    let edited = committed
        .replacen("    score.marker = 1;\n", "    score.marker = 3;\n", 1)
        .replace("move_bodies", "move_bodies_renamed");
    assert!(!fixture_looks_committed(&edited));
    std::fs::write(&source, &edited).unwrap();
    std::fs::write(target.join(LEGACY_JOURNAL), &committed).unwrap();
    {
        let guard = FixtureGuard::acquire(&source, &target);
        assert_eq!(std::fs::read_to_string(&source).unwrap(), committed);
        assert_eq!(guard.original, committed);
        assert_eq!(
            std::fs::read_to_string(target.join("hot-fixture-lib.rs.interrupted")).unwrap(),
            edited
        );
        assert!(!target.join(LEGACY_JOURNAL).exists());
    }
    assert_eq!(std::fs::read_to_string(&source).unwrap(), committed);
    assert!(!target.join(JOURNAL).exists());
}

fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir.pop();
    dir
}

/// Builds `regia-hot-host` with the host's `REGIA_ABI_HASH` pin (ADR-348, ADR-7774 (2)).
///
/// `regia_framework` reads the pin with `option_env!`, so cargo rebuilds it when the pin changes,
/// and the host passes it to every game build. A host built without it had its first game build
/// recompile the framework and relink `regia_dylib` under the running host, which refuses that load
/// ("engine changed — restart the editor"). Under `cargo test` the pin happened to be in the test's
/// environment (cargo exports the editor build script's `rustc-env` to the test process); a test
/// binary run directly had none, so the build sets it itself.
fn build_host(workspace: &Path, target: &Path) {
    let status = Command::new("cargo")
        .current_dir(workspace)
        .env("RUSTFLAGS", "-C prefer-dynamic")
        .env("CARGO_TARGET_DIR", target)
        .env("REGIA_ABI_HASH", regia_editor::host_abi_hash().to_string())
        .args(["build", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .args([
            "-p",
            "regia_editor",
            "--bin",
            "regia-hot-host",
            "--features",
            "hot-dylib",
        ])
        .status()
        .expect("spawn cargo build");
    assert!(status.success(), "regia-hot-host failed to build: {status}");
}

/// macOS only: runs the host's salted game build once before the script starts.
///
/// On the Apple M5 run the first reload was refused ("engine changed") because that first game
/// build relinked `regia_dylib` under the running host (ADR-278). A warm-up build with the same
/// flags makes the engine dylib final, so the fingerprint is stable when the script reloads.
///
/// The warm-up must also carry the host's `REGIA_ABI_HASH` pin (ADR-348). `regia_framework`
/// reads it with `option_env!`, so cargo records the variable as a dependency of that crate; a
/// warm-up without the pin left the first host build to recompile the framework and relink
/// `regia_dylib` (the 5-6 s baseline reload on the M5 versus 0.8 s for the later ones).
/// `build_host` carries the same pin; Linux resolves the same features there and needs no warm-up.
#[cfg(target_os = "macos")]
fn warm_game_build(workspace: &Path, target: &Path) {
    let status = Command::new("cargo")
        .current_dir(workspace)
        .env("RUSTFLAGS", "-C prefer-dynamic")
        .env("CARGO_TARGET_DIR", target)
        .env("REGIA_ABI_HASH", regia_editor::host_abi_hash().to_string())
        .env("CARGO_TERM_COLOR", "never")
        .args(["rustc", "--manifest-path"])
        .arg(workspace.join("Cargo.toml"))
        .args([
            "-p",
            "hot_fixture",
            "--lib",
            "--crate-type",
            "dylib",
            "--features",
            "hot-reload",
            "--target-dir",
        ])
        .arg(target)
        .args(["--", "-C", "prefer-dynamic", "-C", "metadata=hotgenwarm"])
        .status()
        .expect("spawn cargo rustc");
    assert!(status.success(), "hot_fixture warm-up build failed: {status}");
}

fn run_host(workspace: &Path, target: &Path, hot: &Path, script: &Path) -> std::process::Output {
    let bin_name = if cfg!(windows) {
        "regia-hot-host.exe"
    } else {
        "regia-hot-host"
    };
    let bin = target.join("debug").join(bin_name);
    let mut command = Command::new(&bin);
    command
        .current_dir(workspace)
        .env("RUSTFLAGS", "-C prefer-dynamic")
        .env("CARGO_TARGET_DIR", target)
        .env("RUST_BACKTRACE", "1")
        .arg("--manifest")
        .arg(workspace.join("Cargo.toml"))
        .arg("--package")
        .arg("hot_fixture")
        .arg("--crate-dir")
        .arg(workspace.join("samples/hot_fixture"))
        .arg("--target-dir")
        .arg(target)
        .arg("--hot-dir")
        .arg(hot)
        .arg("--script")
        .arg(script);
    let lib_dir = toolchain_lib_dir();
    let profile = target.join("debug");
    let deps = profile.join("deps");
    #[cfg(windows)]
    {
        let mut path = lib_dir.into_os_string();
        path.push(";");
        path.push(profile.as_os_str());
        path.push(";");
        path.push(deps.as_os_str());
        path.push(";");
        if let Some(existing) = regia_core::brand::env_var_os("PATH") {
            path.push(existing);
        }
        command.env("PATH", path);
    }
    #[cfg(not(windows))]
    {
        let mut path = lib_dir.into_os_string();
        path.push(":");
        path.push(profile.as_os_str());
        path.push(":");
        path.push(deps.as_os_str());
        path.push(":");
        if let Some(existing) = regia_core::brand::env_var_os("LD_LIBRARY_PATH") {
            path.push(existing);
        }
        command.env("LD_LIBRARY_PATH", path);
    }
    command.output().expect("spawn regia-hot-host")
}

fn toolchain_lib_dir() -> PathBuf {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc sysroot");
    let sysroot = String::from_utf8_lossy(&sysroot.stdout).trim().to_string();
    let verbose = Command::new("rustc").arg("-vV").output().expect("rustc -vV");
    let verbose = String::from_utf8_lossy(&verbose.stdout);
    let host = verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("host triple")
        .trim();
    PathBuf::from(sysroot).join("lib/rustlib").join(host).join("lib")
}

#[derive(Debug)]
struct Obs {
    fields: Vec<(String, String)>,
    detail: String,
}

impl Obs {
    fn value(&self, key: &str) -> &str {
        if key == "detail" {
            return &self.detail;
        }
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }

    fn ms(&self) -> u128 {
        self.value("ms").parse().unwrap_or(Duration::MAX.as_millis())
    }

    fn int(&self, key: &str) -> i64 {
        self.value(key).parse().unwrap_or(-1)
    }
}

fn parse_observations(stdout: &str) -> Vec<Obs> {
    stdout
        .lines()
        .filter(|line| line.starts_with("OBS "))
        .map(|line| {
            let mut fields = Vec::new();
            let mut detail = String::new();
            let rest = line.trim_start_matches("OBS ");
            if let Some(index) = rest.find(" detail=") {
                detail = rest[index + " detail=".len()..].to_string();
                for token in rest[..index].split_whitespace() {
                    if let Some((key, value)) = token.split_once('=') {
                        fields.push((key.to_string(), value.to_string()));
                    }
                }
            }
            Obs { fields, detail }
        })
        .collect()
}

fn obs<'a>(observations: &'a [Obs], name: &str) -> &'a Obs {
    observations
        .iter()
        .find(|item| item.value("name") == name)
        .unwrap_or_else(|| panic!("missing observation {name}: {observations:?}"))
}
