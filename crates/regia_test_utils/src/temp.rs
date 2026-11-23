// SPDX-License-Identifier: MPL-2.0
//! Per-process, per-call scratch paths for tests.
//!
//! The merge gate runs several test passes (software and real GPU) concurrently on one host, and
//! the threads of one test binary run in parallel too, so a fixed name under the temp directory
//! is shared state. Names built here carry the process id and an atomic counter.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory under the system temp directory, unique to this process and call.
/// `tag` only makes the name readable. Remove it with `std::fs::remove_dir_all` when done.
///
/// # Panics
/// Panics when the directory cannot be created (a test environment failure).
pub fn unique_temp_dir(tag: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create unique temp dir");
    dir
}

/// A [`unique_temp_dir`] that removes itself on drop, so a failing assertion cannot leave the
/// folder behind on the shared merge-gate hosts. Dereferences to the directory's [`Path`].
///
/// # Panics
/// Panics when the directory cannot be created (a test environment failure), like
/// [`unique_temp_dir`].
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// A fresh, empty tagged directory (see [`unique_temp_dir`]).
    #[must_use]
    pub fn new(tag: &str) -> Self {
        Self(unique_temp_dir(tag))
    }

    /// The directory path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes `bytes` to `path` through a sibling temp file and a rename, so a concurrent reader
/// sees either nothing or the whole file, never a truncated one.
///
/// # Errors
/// Returns the IO error of the write or the rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_are_distinct_and_exist() {
        let a = unique_temp_dir("tmp-helper");
        let b = unique_temp_dir("tmp-helper");
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        let f = a.join("x.bin");
        write_atomic(&f, b"abc").expect("write");
        assert_eq!(std::fs::read(&f).expect("read"), b"abc");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}
