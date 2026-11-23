// SPDX-License-Identifier: MPL-2.0
//! Atomic file writes for editor saves (`.animgraph.ron`, `.animedit.ron` sidecars,
//! procgen capture PNGs, `.matgraph.ron`, level and bake description files): write a unique
//! temp file next to the target, then rename over it.
//!
//! Windows caveats (sprint audit g, finding 1; audit G editor4-platform L3): renaming over
//! a destination that another process holds open without delete sharing fails with
//! `Access is denied` (a viewer, an external editor, or a transient antivirus/search-indexer
//! lock), and renaming over a read-only destination (a Perforce-style sync) fails the same
//! way. On that error the rename clears the read-only attribute once and is retried briefly
//! with the same backoff as `regia_platform`'s `rename_replace`, then the save fails and the
//! temp file is removed — the previous document is never truncated, so a failed save never
//! tears the file either way. The temp file is always removed, on success and on failure, so
//! a failed save never leaves a stale `.tmp` next to the document.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Rename retries after a transient Windows sharing error (the destination is held open),
/// before the save fails. Indexer locks are transient; six tries over about 150 ms cover
/// them, matching `regia_platform::vfs::rename_replace`.
#[cfg(windows)]
const RENAME_ATTEMPTS: u32 = 6;

/// Per-process counter keeping every temp name unique (two editor processes, or two saves in
/// one process, never share one temp file).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` atomically: a temp file named `<file>.<pid>.<counter>.tmp` in the
/// same directory, then a rename over the target, see the module docs.
///
/// # Errors
///
/// Directory creation, temp write, or rename (after the transient-lock retries on Windows)
/// failure; the temp file is removed before the error is returned and the previous contents
/// of `path` are left untouched.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let tmp = temp_path(path);
    if let Err(e) = write_synced(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }
    match rename_over(&tmp, path) {
        Ok(()) => {
            // The rename consumes the temp.
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Writes `bytes` to a new file at `tmp` and flushes it to the device before the caller renames
/// it over the target: after a power loss the rename must not be durable while the data is not
/// (a zero-length "saved" level or autosave). Matches the scene crate's directory-form level
/// writer (ADR-7412).
fn write_synced(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The unique temp path for one `write_atomic` call, next to the target.
fn temp_path(path: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{}.tmp", std::process::id(), n));
    path.with_file_name(name)
}

/// Renames `tmp` over `dst`. Atomic on Linux and Android. On Windows a transient sharing
/// lock on the destination (access denied 5, sharing violation 32, lock violation 33) is
/// retried a few times with a short backoff before the error is returned, after clearing a
/// read-only attribute on the destination once; the write is never torn either way (mirrors
/// `regia_platform::vfs::rename_replace`).
fn rename_over(tmp: &Path, dst: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut attempt = 0;
        loop {
            match std::fs::rename(tmp, dst) {
                Err(err) if attempt + 1 < RENAME_ATTEMPTS && is_transient_sharing_error(&err) => {
                    if attempt == 0 {
                        clear_readonly(dst);
                    }
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(5 << attempt));
                }
                Ok(()) => return Ok(()),
                Err(err) => return Err(format!("cannot rename to {}: {err}", dst.display())),
            }
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, dst).map_err(|e| format!("cannot rename to {}: {e}", dst.display()))
    }
}

/// Whether `err` is a Windows access denied (5), sharing violation (32) or lock violation
/// (33), which clear once the other holder closes the file.
#[cfg(windows)]
fn is_transient_sharing_error(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(5 | 32 | 33))
}

/// Clears the read-only attribute on `path` when it is set. Windows refuses to replace a
/// read-only destination (`MoveFileExW` fails with access denied, e.g. Perforce-style syncs
/// mark saved files read-only); POSIX rename needs only directory write permission, so it
/// exists on Windows only.
#[cfg(windows)]
fn clear_readonly(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().readonly()
    {
        let mut perms = meta.permissions();
        // On Windows `set_readonly(false)` clears exactly FILE_ATTRIBUTE_READONLY, the bit
        // `MoveFileExW` refuses on; the lint's concern (a world-writable Unix mode) cannot
        // arise in this Windows-only function (ADR-5959).
        #[expect(
            clippy::permissions_set_readonly_false,
            reason = "Windows only: clears the read-only attribute, no Unix mode bits (ADR-5959)"
        )]
        perms.set_readonly(false);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            tracing::debug!(
                "could not clear the read-only attribute on {}: {e}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Temp-file residue (`*.tmp`, `*.ron.new`) left anywhere in `dir`.
    fn residue(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp") || n.ends_with(".new"))
            .collect()
    }

    #[test]
    fn failed_write_removes_the_temp_file() {
        let dir = regia_test_utils::unique_temp_dir("editor-atomic-write-fail");
        let target = dir.join("hero.animgraph.ron");
        // A directory at the target path makes the rename fail with an error that is not a
        // transient sharing lock, exercising the "report the error, but clean up the temp"
        // arm.
        std::fs::create_dir(&target).expect("blocker dir");
        let err = write_atomic(&target, b"graph").expect_err("rename onto a directory must fail");
        assert!(err.contains("cannot rename"), "unexpected error: {err}");
        assert!(
            residue(&dir).is_empty(),
            "temp file left behind after the failed save: {:?}",
            residue(&dir)
        );
    }

    #[test]
    fn consecutive_writes_leave_no_temp_residue_and_use_unique_names() {
        let dir = regia_test_utils::unique_temp_dir("editor-atomic-write-ok");
        let target = dir.join("hero.animgraph.ron");
        write_atomic(&target, b"one").expect("first write");
        write_atomic(&target, b"two").expect("second write");
        assert_eq!(std::fs::read(&target).expect("read target"), b"two");
        assert!(
            residue(&dir).is_empty(),
            "temp residue after successful saves: {:?}",
            residue(&dir)
        );
        assert_ne!(
            temp_path(&target),
            temp_path(&target),
            "temp names must be unique per call so two processes never share one temp file"
        );
    }

    /// Sprint audit g, finding 1 (Windows clause): a destination held open without delete
    /// sharing (a viewer or external editor) makes the rename fail with access denied; the
    /// retries must give up, the save must fail and the temp file must be removed, and the
    /// bytes the holder still sees must be the old document (the save never tears the file).
    /// Run on a Windows host: `cargo test -p regia_editor --lib atomic_write`.
    #[cfg(windows)]
    #[test]
    fn locked_destination_fails_without_tearing_or_residue() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = regia_test_utils::unique_temp_dir("editor-atomic-write-locked");
        let target = dir.join("shot.png");
        std::fs::write(&target, b"old").expect("seed");
        // Share read+write but not delete: the holder can keep reading/writing,
        // while rename fails. The safe standard-library API preserves that setup.
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0x1 | 0x2)
            .open(&target)
            .expect("open without delete sharing");

        let result = write_atomic(&target, b"new-bytes");

        drop(holder);

        result.expect_err("a save over a held-open destination must fail, not tear the file");
        assert_eq!(
            std::fs::read(&target).expect("read target"),
            b"old",
            "the previous document must survive the failed save"
        );
        assert!(residue(&dir).is_empty(), "temp residue: {:?}", residue(&dir));
        // Once the lock is gone the same save succeeds.
        write_atomic(&target, b"new-bytes").expect("save after the lock is released");
        assert_eq!(std::fs::read(&target).expect("read target"), b"new-bytes");
        assert!(residue(&dir).is_empty(), "temp residue: {:?}", residue(&dir));
    }

    /// Audit G editor4-platform L3 (sprint-fixrev-08-k): a read-only destination
    /// (Perforce-style sync) refuses the replacing rename; the save succeeds after the
    /// helper clears the attribute once. Mirrors the bake-description regression test.
    /// Runs on Windows: `cargo test -p regia_editor --lib atomic_write`.
    #[cfg(windows)]
    #[test]
    fn replaces_read_only_destination() {
        let dir = regia_test_utils::unique_temp_dir("editor-atomic-write-readonly");
        let target = dir.join("arena.level.ron");
        write_atomic(&target, b"first").expect("first write");
        let mut perms = std::fs::metadata(&target).expect("meta").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&target, perms).expect("mark read-only");
        write_atomic(&target, b"second").expect("rewrite over read-only");
        assert_eq!(std::fs::read(&target).expect("read"), b"second");
        assert!(residue(&dir).is_empty(), "temp residue: {:?}", residue(&dir));
    }

    /// Audit G editor2-platform L6 (sprint-fixrev-08-k): a write that fails AFTER the temp
    /// file was created (disk full, quota) must remove the temp. `RLIMIT_FSIZE=0` makes the
    /// very first write fail with `EFBIG`; a resource limit can only apply to a fresh
    /// process, so this test re-executes itself under `sh -c 'ulimit -f 0; trap "" XFSZ;
    /// exec ...'` (SIGXFSZ ignored so the write returns the error instead of killing the
    /// child) and asserts the child sees the error and no residue. Removing the helper's
    /// write-error cleanup makes the child exit 1. Unix-only (no `ulimit` on Windows).
    #[cfg(unix)]
    #[test]
    fn write_failure_after_create_removes_the_temp() {
        const CHILD: &str = "REGIA_ATOMIC_WRITE_EFBIG_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let dir = regia_test_utils::unique_temp_dir("editor-atomic-write-efbig-child");
            let target = dir.join("graph.animgraph.ron");
            let result = write_atomic(&target, b"some bytes");
            let violated = result.is_ok() || !residue(&dir).is_empty();
            let _ = std::fs::remove_dir_all(&dir);
            std::process::exit(if violated { 1 } else { 0 });
        }
        let exe = std::env::current_exe().expect("current test binary");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg("ulimit -f 0; trap '' XFSZ; exec \"$@\"")
            .arg("sh")
            .arg(&exe)
            .arg("--exact")
            .arg("atomic_write::tests::write_failure_after_create_removes_the_temp")
            .env(CHILD, "1")
            // Piped, not inherited: under `ulimit -f 0` the child's own test-harness output would
            // fail with EFBIG when the parent's stdout is a regular file (a log), panicking the
            // child before it reports.
            .output()
            .expect("spawn the file-size-limited child");
        let status = output.status;
        assert!(
            status.success(),
            "child exited with {status}: a failed temp write left residue, or the save succeeded under RLIMIT_FSIZE=0; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
