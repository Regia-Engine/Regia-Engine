// SPDX-License-Identifier: MPL-2.0
//! Editor robustness (`docs/specs/19-editor.md` § 7): level autosave, crash recovery of the
//! open level, and the unsaved-changes decision on close.
//!
//! Autosaves live in `<project>/.regia/autosave/<stem>.level.ron` — one per level, written
//! atomically every 5 minutes while the level has unsaved edits. A successful manual save and
//! a clean exit remove them; a crashed editor leaves them behind, and the next launch offers
//! them back (a modal in the shell). Restoring opens the autosave but points the level at its
//! original path (found by stem under `assets/`), so the next save writes where the user
//! expects.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use regia_ecs::prelude::*;

use crate::level_io::{self, OpenLevel};

/// The default autosave interval (`specs/19` § 7: every 5 minutes).
pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The autosave directory of a project.
#[must_use]
pub fn autosave_dir(project_root: &Path) -> PathBuf {
    regia_core::brand::project_state_dir(project_root).join("autosave")
}

/// Separates the level stem from the path key in an autosave name.
const KEY_SEPARATOR: char = '~';

/// A stable 32-bit key (FNV-1a) of a level file's project-relative path, `/`-normalized.
fn path_key(project_root: &Path, level_path: &Path) -> u32 {
    let relative = level_path.strip_prefix(project_root).unwrap_or(level_path);
    let text = relative.to_string_lossy().replace('\\', "/");
    text.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// The autosave path for the open level: the level file's stem (`untitled` when it has no path
/// yet), keeping one autosave per level. When another level under `assets/` shares the stem,
/// a key of the project-relative path joins the name so two levels never share one autosave.
#[must_use]
pub fn autosave_path(project_root: &Path, open: &OpenLevel) -> PathBuf {
    let stem = open
        .path
        .as_deref()
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .map(|s| s.strip_suffix(".level").unwrap_or(s).to_string())
        .unwrap_or_else(|| "untitled".to_string());
    let name = match &open.path {
        Some(path)
            if level_files_with_stem(project_root, &stem)
                .iter()
                .any(|other| other != path) =>
        {
            format!("{stem}{KEY_SEPARATOR}{:08x}", path_key(project_root, path))
        }
        _ => stem,
    };
    autosave_dir(project_root).join(format!("{name}.level.ron"))
}

/// Writes the open level to its autosave path (atomic temp + rename).
///
/// # Errors
///
/// Snapshot or IO failure.
pub fn write_autosave(world: &mut World, open: &OpenLevel, project_root: &Path) -> Result<PathBuf, String> {
    let path = autosave_path(project_root, open);
    level_io::save_level_file(world, open, &path)?;
    tracing::info!(target: "regia_editor", "autosaved to {}", path.display());
    Ok(path)
}

/// Removes the open level's autosave (after a successful save of the level itself).
pub fn clear_autosave(project_root: &Path, open: &OpenLevel) {
    let _ = std::fs::remove_file(autosave_path(project_root, open));
}

/// Removes the autosaves a successful save supersedes: the level's previous path (or the
/// `untitled` file, when it had none) and the path just written.
pub fn clear_autosave_after_save(project_root: &Path, before: &OpenLevel, saved_path: &Path) {
    clear_autosave(project_root, before);
    let saved = OpenLevel {
        id: before.id,
        path: Some(saved_path.to_path_buf()),
    };
    clear_autosave(project_root, &saved);
}

/// Removes every autosave (nothing is left to recover). The editor's exit path uses
/// [`clear_autosaves_except`] instead, so crash-recovery autosaves still on offer survive.
pub fn clear_all_autosaves(project_root: &Path) {
    let _ = std::fs::remove_dir_all(autosave_dir(project_root));
}

/// Removes every autosave except the files in `keep`: the editor's clean exit, where `keep` holds
/// the crash-recovery autosaves the user has neither restored nor discarded yet. Those are the
/// only copy of a crashed session's edits, so exiting must not delete them (audit v033b: a clean
/// level's close removed the whole autosave directory while the recovery window was still up).
/// The directory itself goes when nothing is left in it.
pub fn clear_autosaves_except(project_root: &Path, keep: &[PathBuf]) {
    let dir = autosave_dir(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for path in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
        if path.is_file() && !keep.iter().any(|kept| kept == &path) {
            let _ = std::fs::remove_file(&path);
        }
    }
    // Only an empty directory is removed (`remove_dir` refuses a non-empty one).
    let _ = std::fs::remove_dir(&dir);
}

/// Writes the open level's autosave when the level stack has unsaved edits: the `quit.request`
/// path, which exits without a prompt and must leave the newest edits for the next launch to
/// recover (the 5-minute tick alone could be up to one interval behind). `None` when the level is
/// clean (nothing to keep).
pub fn autosave_unsaved_level(world: &mut World, project_root: &Path) -> Option<Result<PathBuf, String>> {
    let dirty = world
        .get_resource::<crate::command::UndoStack>()
        .is_some_and(crate::command::UndoStack::is_dirty);
    if !dirty {
        return None;
    }
    let open = world.get_resource::<OpenLevel>().cloned()?;
    Some(write_autosave(world, &open, project_root))
}

/// Marks an autosave written beside a pending recovery file: `<name>~quit.level.ron`, then
/// `~quit2`, `~quit3`, ... (see [`autosave_path_beside`]).
const BESIDE_MARK: &str = "~quit";

/// How many `~quitN` names [`autosave_path_beside`] tries before giving up.
const MAX_BESIDE_NAMES: u32 = 1_000;

/// The autosave path for the open level that is not one of `taken`: the level's own
/// [`autosave_path`] when that is free, else `<name>~quit.level.ron`, `<name>~quit2.level.ron`,
/// ... (the first that is neither taken nor on disk). The `quit.request` path passes the files
/// the recovery window still offers, so this session's edits are kept beside the crashed
/// session's copy instead of being dropped or written over it (rc2 review, editor-undo
/// finding 2). [`recovery_candidates`] maps such a file back to the same level. `None` when
/// every name is in use.
#[must_use]
pub fn autosave_path_beside(project_root: &Path, open: &OpenLevel, taken: &[PathBuf]) -> Option<PathBuf> {
    let own = autosave_path(project_root, open);
    if !taken.contains(&own) {
        return Some(own);
    }
    let name = own
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".level.ron"))
        .unwrap_or("untitled")
        .to_string();
    let dir = autosave_dir(project_root);
    (1..=MAX_BESIDE_NAMES)
        .map(|n| {
            let mark = if n == 1 {
                BESIDE_MARK.to_string()
            } else {
                format!("{BESIDE_MARK}{n}")
            };
            dir.join(format!("{name}{mark}.level.ron"))
        })
        .find(|path| !taken.contains(path) && !path.exists())
}

/// An autosave stem without its `~quit` / `~quitN` mark (see [`autosave_path_beside`]).
fn strip_beside_mark(stem: &str) -> &str {
    match stem.rfind(BESIDE_MARK) {
        Some(at) if stem[at + BESIDE_MARK.len()..].bytes().all(|b| b.is_ascii_digit()) => &stem[..at],
        _ => stem,
    }
}

/// [`autosave_unsaved_level`] that never writes to one of `taken` (the files the recovery window
/// still offers): the autosave goes to [`autosave_path_beside`] instead, so both copies survive.
/// `None` when the level is clean.
pub fn autosave_unsaved_level_beside(
    world: &mut World,
    project_root: &Path,
    taken: &[PathBuf],
) -> Option<Result<PathBuf, String>> {
    let dirty = world
        .get_resource::<crate::command::UndoStack>()
        .is_some_and(crate::command::UndoStack::is_dirty);
    if !dirty {
        return None;
    }
    let open = world.get_resource::<OpenLevel>().cloned()?;
    let Some(path) = autosave_path_beside(project_root, &open, taken) else {
        return Some(Err(format!(
            "every autosave name for this level is in use in {}",
            autosave_dir(project_root).display()
        )));
    };
    Some(level_io::save_level_file(world, &open, &path).map(|()| {
        tracing::info!(target: "regia_editor", "autosaved to {}", path.display());
        path
    }))
}

/// One recoverable autosave found at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCandidate {
    /// The autosave file.
    pub autosave: PathBuf,
    /// The level file it belongs to (`None` when none matches by stem under `assets/`).
    pub original: Option<PathBuf>,
    /// The autosave's modification time.
    pub modified: SystemTime,
}

/// The autosaves present in a project at startup (`specs/19` § 7 "crash recovery prompt on
/// next launch"), newest first, with their original level paths when one matches by stem.
#[must_use]
pub fn recovery_candidates(project_root: &Path) -> Vec<RecoveryCandidate> {
    let dir = autosave_dir(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<RecoveryCandidate> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ron"))
        .filter_map(|autosave| {
            let modified = std::fs::metadata(&autosave).ok()?.modified().ok()?;
            let stem = autosave.file_stem()?.to_str()?.to_string();
            let stem = stem.strip_suffix(".level").unwrap_or(&stem);
            let stem = strip_beside_mark(stem).to_string();
            let original = match stem.split_once(KEY_SEPARATOR) {
                Some((base, key)) => level_files_with_stem(project_root, base)
                    .into_iter()
                    .find(|path| format!("{:08x}", path_key(project_root, path)) == key),
                None => find_level_by_stem(project_root, &stem),
            };
            Some(RecoveryCandidate {
                original,
                autosave,
                modified,
            })
        })
        .collect();
    out.sort_by_key(|candidate| candidate.modified);
    out.reverse();
    out
}

/// Whether restoring `candidate` is worth offering: it is newer than its original level file,
/// or the original is gone (an untitled level, or the file moved).
#[must_use]
pub fn should_offer_recovery(candidate: &RecoveryCandidate) -> bool {
    let Some(original) = &candidate.original else {
        return true; // untitled level with edits — nothing to compare against
    };
    match std::fs::metadata(original).and_then(|m| m.modified()) {
        Ok(original_modified) => candidate.modified > original_modified,
        Err(_) => true, // original vanished: the autosave is the only copy
    }
}

/// Every level named `<stem>` under `assets/`, in either storage form: a `<stem>.level.ron` file
/// or a `<stem>.level.d` directory (ADR-7412), sorted for a deterministic order. A level
/// directory is never searched for further levels.
fn level_files_with_stem(project_root: &Path, stem: &str) -> Vec<PathBuf> {
    let wanted = format!("{stem}.level.ron");
    let wanted_dir = format!("{stem}.level.d");
    let mut found = Vec::new();
    let mut stack = vec![project_root.join("assets")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                if name == wanted_dir {
                    found.push(path);
                } else if !name.ends_with(".level.d") {
                    stack.push(path);
                }
            } else if name == wanted {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Finds `assets/**/<stem>.level.ron` (or `<stem>.level.d`) in the project (the level an
/// autosave's stem belongs to).
/// `None` when no level matches or when more than one does (ambiguous, so never guessed:
/// autosaves of levels that share a stem carry a path key instead, see `autosave_path`).
#[must_use]
pub fn find_level_by_stem(project_root: &Path, stem: &str) -> Option<PathBuf> {
    let mut found = level_files_with_stem(project_root, stem);
    if found.len() == 1 { found.pop() } else { None }
}

/// Restores an autosave: the level opens from the autosave file, but the open level's path
/// points at the original level file, so the next save lands where the user expects. The
/// autosave stays until the level is saved (it is the only copy until then).
///
/// # Errors
///
/// When the autosave cannot be opened.
pub fn restore_autosave(world: &mut World, candidate: &RecoveryCandidate) -> Result<OpenLevel, String> {
    let mut open = level_io::open_level_file(world, &candidate.autosave)?;
    open.path = candidate.original.clone();
    tracing::info!(target: "regia_editor", "restored autosave {}", candidate.autosave.display());
    Ok(open)
}

/// Why [`apply_restore`] refuses while a prefab is open (the command palette's
/// `recovery.restore` says the same; `editor.recovery_restore` answers `prefab_open`).
pub const RESTORE_IN_PREFAB_MODE: &str =
    "Close the prefab editor first (editor.close_prefab) to restore an autosave";

/// The recovery window's Restore: opens the autosave ([`restore_autosave`]), makes it the open
/// level, and starts a fresh edit session on it. The old undo history addresses the level the
/// restore replaced, so it is dropped (like File → Open), and the new stack is marked dirty: the
/// restored edits exist only in the autosave until the user saves.
///
/// # Errors
///
/// When the autosave cannot be opened, or a prefab edit session is open ([`RESTORE_IN_PREFAB_MODE`]:
/// the restored level would load beside the prefab under the session's stash). The open level,
/// the stash and the undo history are then unchanged.
pub fn apply_restore(world: &mut World, candidate: &RecoveryCandidate) -> Result<OpenLevel, String> {
    if crate::prefab_edit::in_prefab_mode(world) {
        return Err(RESTORE_IN_PREFAB_MODE.to_string());
    }
    let open = restore_autosave(world, candidate)?;
    world.insert_resource(open.clone());
    level_io::reset_edit_state(world);
    world
        .get_resource_or_insert_with::<crate::command::UndoStack>(crate::command::UndoStack::new)
        .mark_dirty();
    Ok(open)
}

/// The unsaved-changes decision the close modal collects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Save the level, then exit.
    SaveAndExit,
    /// Exit without saving.
    DiscardAndExit,
    /// Keep editing.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::App;
    use regia_scene::prelude::LevelId;

    fn temp_project(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-editor-{label}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets/maps")).expect("dir");
        dir
    }

    fn app_with_level() -> (App, OpenLevel) {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        let open = level_io::new_level(app.world_mut());
        app.world_mut().insert_resource(open.clone());
        (app, open)
    }

    /// `specs/19` § 7: autosave runs every 5 minutes by default, and the preference agrees.
    #[test]
    fn the_default_autosave_interval_is_the_spec_5_minutes() {
        assert_eq!(AUTOSAVE_INTERVAL, Duration::from_secs(300));
        let prefs = crate::editor_prefs::EditorPreferences::default();
        assert_eq!(prefs.int("autosave.interval_minutes"), 5);
    }

    #[test]
    fn autosave_writes_clears_and_recovers() {
        let dir = temp_project("autosave");
        let (mut app, mut open) = app_with_level();

        // An untitled level autosaves as untitled.
        let path = write_autosave(app.world_mut(), &open, &dir).expect("autosave");
        assert!(path.ends_with("untitled.level.ron"), "{}", path.display());
        assert!(path.is_file());

        // A saved level: name the level file, autosave again (stem follows).
        let level_path = dir.join("assets/maps/main.level.ron");
        level_io::save_level_file(app.world_mut(), &open, &level_path).expect("save level");
        // A real save drops the untitled autosave (the level now has a path).
        clear_autosave_after_save(&dir, &open, &level_path);
        open.path = Some(level_path.clone());
        let path = write_autosave(app.world_mut(), &open, &dir).expect("autosave 2");
        assert!(path.ends_with("main.level.ron"), "{}", path.display());

        // Editing content changes the autosave's bytes (the recovery carries edits).
        // The editor's spawn path tags the entity as a member of the open level; a bare
        // spawn is invisible to the level snapshot.
        use crate::command::EditorCommand as _;
        let mut command = crate::command::SpawnEntities::new_entity(
            app.world(),
            regia_core::id::Guid::new_random(),
            "AutosavedRock",
            regia_scene::prelude::Transform::default(),
        )
        .expect("spawn");
        command.apply(app.world_mut());
        let _ = write_autosave(app.world_mut(), &open, &dir).expect("autosave 3");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("AutosavedRock"),
            "the autosave has the edit: {text}"
        );

        // The level was saved before the autosave. Pin its time 2 s earlier: two writes inside one
        // file-time tick (about 15 ms on Windows) would otherwise compare equal.
        let autosaved = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .expect("autosave time");
        std::fs::File::options()
            .write(true)
            .open(&level_path)
            .and_then(|f| f.set_modified(autosaved - std::time::Duration::from_secs(2)))
            .expect("level time");

        // Startup finds it, maps it to the original, and offers it (autosave newer than level).
        let candidates = recovery_candidates(&dir);
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        let candidate = &candidates[0];
        assert_eq!(candidate.original, Some(level_path.clone()));
        assert!(should_offer_recovery(candidate), "the autosave is newer");

        // Restore: contents come back, the path points at the original.
        let (mut fresh, _open) = app_with_level();
        let restored = restore_autosave(fresh.world_mut(), candidate).expect("restore");
        assert_eq!(restored.path, Some(level_path.clone()));
        let names: Vec<String> = {
            let world = fresh.world_mut();
            let mut query = world.query::<&regia_scene::prelude::Name>();
            query.iter(world).map(|n| n.as_str().to_string()).collect()
        };
        assert!(names.contains(&"AutosavedRock".to_string()), "{names:?}");

        // Clearing the level's autosave removes it; clearing all empties the dir.
        clear_autosave(&dir, &open);
        assert!(!path.is_file());
        let _ = write_autosave(app.world_mut(), &open, &dir).expect("autosave 4");
        clear_all_autosaves(&dir);
        assert!(recovery_candidates(&dir).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovery_prefers_the_newest_and_skips_stale_autosaves() {
        let dir = temp_project("recovery-stale");
        let level_path = dir.join("assets/maps/main.level.ron");
        std::fs::write(&level_path, "( format_version: 1, entities: [] )").expect("level");
        let autosave = autosave_path(
            &dir,
            &OpenLevel {
                id: LevelId(regia_core::id::Guid::new_random()),
                path: Some(level_path.clone()),
            },
        );
        std::fs::create_dir_all(autosave.parent().expect("dir")).expect("dir");
        std::fs::write(&autosave, "( format_version: 1, entities: [] )").expect("autosave");

        // Explicit mtimes: no sleeping on filesystem timestamp granularity.
        let set_mtime = |path: &std::path::Path, secs: u64| {
            let file = std::fs::OpenOptions::new().write(true).open(path).expect("open");
            file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(secs))
                .expect("set mtime");
        };
        // The autosave was written AFTER the level file.
        set_mtime(&level_path, 1_000);
        set_mtime(&autosave, 2_000);

        let candidates = recovery_candidates(&dir);
        assert_eq!(candidates.len(), 1);
        assert!(should_offer_recovery(&candidates[0]));

        // An autosave OLDER than its level (the level was saved after the last autosave) is
        // not offered.
        set_mtime(&level_path, 3_000);
        let candidates = recovery_candidates(&dir);
        assert!(
            !should_offer_recovery(&candidates[0]),
            "the level file is newer than the autosave"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn levels_sharing_a_stem_get_separate_autosaves() {
        let dir = temp_project("same-stem");
        std::fs::create_dir_all(dir.join("assets/tests")).expect("dir");
        let first = dir.join("assets/maps/arena.level.ron");
        let second = dir.join("assets/tests/arena.level.ron");
        std::fs::write(&first, "()").expect("level");
        std::fs::write(&second, "()").expect("level");
        let open = |path: &PathBuf| OpenLevel {
            id: LevelId(regia_core::id::Guid::new_random()),
            path: Some(path.clone()),
        };
        let a = autosave_path(&dir, &open(&first));
        let b = autosave_path(&dir, &open(&second));
        assert_ne!(a, b, "one autosave per level file");
        std::fs::create_dir_all(a.parent().expect("dir")).expect("dir");
        std::fs::write(&a, "()").expect("autosave");
        std::fs::write(&b, "()").expect("autosave");
        let originals: std::collections::BTreeSet<Option<PathBuf>> = recovery_candidates(&dir)
            .into_iter()
            .map(|c| c.original)
            .collect();
        assert_eq!(originals, [Some(first), Some(second)].into_iter().collect());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rc2 review (editor-undo finding 2): an autosave written beside a pending recovery file
    /// (`~quit`, `~quitN`) maps back to the same level, for a plain stem and a keyed one, and a
    /// free own path is used as is.
    #[test]
    fn autosaves_beside_a_pending_file_map_back_to_their_level() {
        let dir = temp_project("beside");
        std::fs::create_dir_all(dir.join("assets/tests")).expect("dir");
        let solo = dir.join("assets/maps/keep.level.ron");
        let first = dir.join("assets/maps/arena.level.ron");
        let second = dir.join("assets/tests/arena.level.ron");
        for level in [&solo, &first, &second] {
            std::fs::write(level, "()").expect("level");
        }
        let open = |path: &PathBuf| OpenLevel {
            id: LevelId(regia_core::id::Guid::new_random()),
            path: Some(path.clone()),
        };
        std::fs::create_dir_all(autosave_dir(&dir)).expect("dir");
        for level in [&solo, &second] {
            let own = autosave_path(&dir, &open(level));
            assert_eq!(autosave_path_beside(&dir, &open(level), &[]), Some(own.clone()));
            std::fs::write(&own, "()").expect("pending");
            let beside =
                autosave_path_beside(&dir, &open(level), std::slice::from_ref(&own)).expect("a name");
            assert_ne!(beside, own);
            std::fs::write(&beside, "()").expect("beside");
            let again =
                autosave_path_beside(&dir, &open(level), std::slice::from_ref(&own)).expect("a third name");
            assert_ne!(again, beside, "an existing file is never written over");
            std::fs::write(&again, "()").expect("beside 2");
        }
        let mut originals: Vec<(String, Option<PathBuf>)> = recovery_candidates(&dir)
            .into_iter()
            .map(|c| {
                let name = c
                    .autosave
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                (name, c.original)
            })
            .collect();
        originals.sort();
        let key = format!("{:08x}", path_key(&dir, &second));
        assert_eq!(
            originals,
            vec![
                (format!("arena~{key}.level.ron"), Some(second.clone())),
                (format!("arena~{key}~quit.level.ron"), Some(second.clone())),
                (format!("arena~{key}~quit2.level.ron"), Some(second.clone())),
                ("keep.level.ron".to_string(), Some(solo.clone())),
                ("keep~quit.level.ron".to_string(), Some(solo.clone())),
                ("keep~quit2.level.ron".to_string(), Some(solo.clone())),
            ]
        );
        assert_eq!(
            strip_beside_mark("keep~quitter"),
            "keep~quitter",
            "only a digit tail is a mark"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stem_lookup_finds_levels_by_stem() {
        let dir = temp_project("stem");
        std::fs::create_dir_all(dir.join("assets/deep")).expect("dir");
        std::fs::write(dir.join("assets/deep/arena.level.ron"), "()").expect("level");
        assert_eq!(
            find_level_by_stem(&dir, "arena"),
            Some(dir.join("assets/deep/arena.level.ron"))
        );
        assert_eq!(find_level_by_stem(&dir, "missing"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stem_lookup_refuses_an_ambiguous_stem() {
        let dir = temp_project("stem-dup");
        std::fs::create_dir_all(dir.join("assets/a")).expect("dir");
        std::fs::create_dir_all(dir.join("assets/b")).expect("dir");
        std::fs::write(dir.join("assets/a/arena.level.ron"), "()").expect("level");
        std::fs::write(dir.join("assets/b/arena.level.ron"), "()").expect("level");
        assert_eq!(
            find_level_by_stem(&dir, "arena"),
            None,
            "two matches are never guessed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rc1 editor-undo finding 6 (ADR-7412): the stem lookup matched only `<stem>.level.ron`
    /// files, so the autosave of a directory-form level (`.level.d`) never found its original
    /// (offered forever, restored without a path) and two same-stem `.level.d` levels shared one
    /// autosave.
    #[test]
    fn directory_form_levels_map_back_to_their_autosaves() {
        let dir = temp_project("stem-dir-form");
        std::fs::create_dir_all(dir.join("assets/tests")).expect("dir");
        let first = dir.join("assets/maps/arena.level.d");
        std::fs::create_dir_all(first.join("entities")).expect("level dir");
        std::fs::write(first.join("level.ron"), "()").expect("header");
        assert_eq!(find_level_by_stem(&dir, "arena"), Some(first.clone()));

        let open = |path: &PathBuf| OpenLevel {
            id: LevelId(regia_core::id::Guid::new_random()),
            path: Some(path.clone()),
        };
        let alone = autosave_path(&dir, &open(&first));
        assert_eq!(alone, autosave_dir(&dir).join("arena.level.ron"));
        std::fs::create_dir_all(alone.parent().expect("dir")).expect("dir");
        std::fs::write(&alone, "()").expect("autosave");
        let candidates = recovery_candidates(&dir);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].original,
            Some(first.clone()),
            "the autosave finds its level"
        );
        std::fs::remove_file(&alone).expect("remove");

        // A second `arena.level.d` elsewhere: each level gets its own autosave.
        let second = dir.join("assets/tests/arena.level.d");
        std::fs::create_dir_all(&second).expect("level dir");
        let a = autosave_path(&dir, &open(&first));
        let b = autosave_path(&dir, &open(&second));
        assert_ne!(a, b, "one autosave per level directory");
        std::fs::write(&a, "()").expect("autosave");
        std::fs::write(&b, "()").expect("autosave");
        let originals: std::collections::BTreeSet<Option<PathBuf>> = recovery_candidates(&dir)
            .into_iter()
            .map(|c| c.original)
            .collect();
        assert_eq!(originals, [Some(first), Some(second)].into_iter().collect());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
