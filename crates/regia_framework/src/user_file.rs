// SPDX-License-Identifier: MPL-2.0
//! Damaged player files are moved aside, never overwritten (ADR-3102, `book/user-guide/player-data.md`).
//!
//! A file the runtime reads back from the player's machine (`settings.ron` here; `regia_input` and
//! `regia_net` keep their own copy of this rule for `input_overrides.ron` and `known_hosts.ron`)
//! that does not parse, or comes from a newer build, is renamed to `<name>.bad-<n>` before
//! defaults load, so the next write never replaces the player's data. At most
//! [`MAX_BAD_COPIES`] copies are kept per file: when all exist, the oldest (`.bad-1`) is dropped
//! and the rest shift down by one.

use std::path::{Path, PathBuf};

/// Damaged copies kept per file (`<name>.bad-1` .. `<name>.bad-5`).
pub const MAX_BAD_COPIES: u32 = 5;

/// The path of damaged copy `n` of `path`: `<path>.bad-<n>`.
pub fn bad_copy_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".bad-{n}"));
    PathBuf::from(name)
}

/// Whether something occupies `path`. Only "not found" is free: any other stat error (permission,
/// a broken mount) counts as taken, so a rename never replaces what could not be checked.
fn occupied(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// The free copy slot for `path`, after making room. Existing copies are first closed up in
/// order (a deleted `.bad-2` leaves no gap the newest copy would fill below older ones), then
/// the slot after the last is used; when all [`MAX_BAD_COPIES`] exist, `.bad-1` (the oldest) is
/// dropped and the rest shift down.
fn free_slot(path: &Path) -> std::io::Result<PathBuf> {
    let existing: Vec<u32> = (1..=MAX_BAD_COPIES)
        .filter(|n| occupied(&bad_copy_path(path, *n)))
        .collect();
    for (want, have) in (1..).zip(&existing) {
        if want != *have {
            std::fs::rename(bad_copy_path(path, *have), bad_copy_path(path, want))?;
        }
    }
    let count = existing.len() as u32;
    if count < MAX_BAD_COPIES {
        return Ok(bad_copy_path(path, count + 1));
    }
    std::fs::remove_file(bad_copy_path(path, 1))?;
    for n in 2..=MAX_BAD_COPIES {
        std::fs::rename(bad_copy_path(path, n), bad_copy_path(path, n - 1))?;
    }
    Ok(bad_copy_path(path, MAX_BAD_COPIES))
}

/// Renames `path` to its next free `<name>.bad-<n>` and returns that path.
///
/// # Errors
/// The IO error of a rename or of dropping the oldest copy (a read-only directory, for one);
/// then `path` is still where it was and the caller must not write over it.
pub fn move_aside(path: &Path) -> std::io::Result<PathBuf> {
    let target = free_slot(path)?;
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Copies `bytes` (the content of `path`) to the next free `<name>.bad-<n>`, unless a copy with
/// the same bytes already exists; returns the copy's path. Used for a file that still loads but
/// holds data this build would drop on its next write (a newer build's fields).
///
/// # Errors
/// The IO error of the write or of making room.
pub fn copy_aside(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    for n in 1..=MAX_BAD_COPIES {
        let candidate = bad_copy_path(path, n);
        if std::fs::read(&candidate).is_ok_and(|existing| existing == bytes) {
            return Ok(candidate);
        }
    }
    let target = free_slot(path)?;
    std::fs::write(&target, bytes)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_are_numbered_and_bounded_oldest_first_out() {
        let dir = regia_test_utils::unique_temp_dir("user-file-bounded");
        let path = dir.join("settings.ron");
        for round in 1..=(MAX_BAD_COPIES + 2) {
            std::fs::write(&path, format!("round {round}")).expect("write");
            let moved = move_aside(&path).expect("move aside");
            assert!(!path.exists(), "round {round}: the original is gone");
            let expected = bad_copy_path(&path, round.min(MAX_BAD_COPIES));
            assert_eq!(moved, expected, "round {round}");
        }
        // Rounds 1 and 2 were dropped; .bad-1 holds round 3 and .bad-5 the newest.
        for n in 1..=MAX_BAD_COPIES {
            let text = std::fs::read_to_string(bad_copy_path(&path, n)).expect("copy");
            assert_eq!(text, format!("round {}", n + 2));
        }
        assert!(!bad_copy_path(&path, MAX_BAD_COPIES + 1).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_gap_in_the_copies_never_puts_the_newest_below_older_ones() {
        // .bad-1 was deleted by the player; .bad-2 .. .bad-5 hold rounds 2 .. 5.
        let dir = regia_test_utils::unique_temp_dir("user-file-gap");
        let path = dir.join("settings.ron");
        for n in 2..=MAX_BAD_COPIES {
            std::fs::write(bad_copy_path(&path, n), format!("round {n}")).expect("copy");
        }
        for round in [6, 7] {
            std::fs::write(&path, format!("round {round}")).expect("write");
            move_aside(&path).expect("move aside");
        }
        // Oldest first out: round 2 went, and the two newest rounds both survive.
        let kept: Vec<String> = (1..=MAX_BAD_COPIES)
            .map(|n| std::fs::read_to_string(bad_copy_path(&path, n)).expect("copy"))
            .collect();
        assert_eq!(kept, ["round 3", "round 4", "round 5", "round 6", "round 7"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_identical_copy_is_not_made_twice() {
        let dir = regia_test_utils::unique_temp_dir("user-file-copy");
        let path = dir.join("settings.ron");
        let first = copy_aside(&path, b"newer").expect("copy");
        let again = copy_aside(&path, b"newer").expect("copy again");
        assert_eq!(first, again);
        let other = copy_aside(&path, b"different").expect("copy other");
        assert_eq!(other, bad_copy_path(&path, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
