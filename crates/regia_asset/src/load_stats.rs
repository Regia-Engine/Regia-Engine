// SPDX-License-Identifier: MPL-2.0
//! Process-wide counters of the asset server's load work (ADR-2987): loads asked for, loads that
//! failed, and the cooked bytes read for them from paks and the dev cache. `app.startup_work` reads
//! them at the first presented frame, so a cold start that loads more shows up in the start-up
//! ratchet (`crates/regia/tests/startup_idle_ratchet.rs`).
//!
//! The asset server reads its cooked files with `std::fs` and its paks through mapped readers, not
//! through the platform VFS, so these are counted here and not in `regia_platform::vfs_stats`. Each
//! counter is a relaxed atomic add per load or file, never per byte.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static LOADS: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static FILES_READ: AtomicU64 = AtomicU64::new(0);
static BYTES_READ: AtomicU64 = AtomicU64::new(0);

/// The asset server's load work since the process started ([`load_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssetLoadStats {
    /// Typed loads asked for (`load_mesh`, `load_texture`, `load_image`, `load_material`,
    /// `load_model`) and raw `read_cooked` reads.
    pub loads: u64,
    /// Those that returned an error.
    pub failed: u64,
    /// Cooked payloads read: pak entries and dev-cache files.
    pub files_read: u64,
    /// Bytes of those payloads.
    pub bytes_read: u64,
}

impl AssetLoadStats {
    /// `self - earlier`, per counter (saturating).
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            loads: self.loads.saturating_sub(earlier.loads),
            failed: self.failed.saturating_sub(earlier.failed),
            files_read: self.files_read.saturating_sub(earlier.files_read),
            bytes_read: self.bytes_read.saturating_sub(earlier.bytes_read),
        }
    }
}

/// The asset load counters now (four relaxed loads).
#[must_use]
pub fn load_stats() -> AssetLoadStats {
    AssetLoadStats {
        loads: LOADS.load(Ordering::Relaxed),
        failed: FAILED.load(Ordering::Relaxed),
        files_read: FILES_READ.load(Ordering::Relaxed),
        bytes_read: BYTES_READ.load(Ordering::Relaxed),
    }
}

/// Counts one load and whether it succeeded.
pub(crate) fn note_load(ok: bool) {
    LOADS.fetch_add(1, Ordering::Relaxed);
    if !ok {
        FAILED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts one cooked payload of `bytes` read.
pub(crate) fn note_payload(bytes: usize) {
    FILES_READ.fetch_add(1, Ordering::Relaxed);
    BYTES_READ.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// `std::fs::read` that counts the file it read ([`note_payload`]).
pub(crate) fn read_file(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let result = std::fs::read(path.as_ref());
    crate::access_trace::note_io(path.as_ref(), &result);
    let bytes = result?;
    note_payload(bytes.len());
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_and_a_failed_load_are_counted() {
        let dir = regia_test_utils::unique_temp_dir("regia-load-stats");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("payload.bin");
        std::fs::write(&file, [3u8; 64]).expect("fixture");
        let before = load_stats();
        assert_eq!(read_file(&file).map(|b| b.len()).ok(), Some(64));
        assert!(read_file(dir.join("missing.bin")).is_err());
        note_load(true);
        note_load(false);
        let grew = load_stats().since(before);
        // Other tests of this binary load on parallel threads: "grew by at least".
        assert!(grew.files_read >= 1 && grew.bytes_read >= 64, "{grew:?}");
        assert!(grew.loads >= 2 && grew.failed >= 1, "{grew:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
