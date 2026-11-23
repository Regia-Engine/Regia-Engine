// SPDX-License-Identifier: MPL-2.0
//! Process-wide counters of the file work done through the `Vfs` (ADR-2987): files opened,
//! bytes read and written. `app.startup_work` reads them at the first presented frame, so a cold
//! start that reads more files shows up in the start-up ratchet (`crates/regia/tests/startup_idle_ratchet.rs`).
//!
//! The counters are relaxed atomic adds at the wrapper level (`Vfs::read_sync`,
//! `Vfs::open_stream` and its reader, `Vfs::write_atomic`); a mount's own reads
//! below that are not counted twice. Reads that bypass the VFS (the asset server's cooked cache and
//! paks) are counted by `regia_asset::load_stats`.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

static OPENS: AtomicU64 = AtomicU64::new(0);
static FAILED_OPENS: AtomicU64 = AtomicU64::new(0);
static BYTES_READ: AtomicU64 = AtomicU64::new(0);
static WRITES: AtomicU64 = AtomicU64::new(0);
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// File work done through the VFS since the process started (`regia_platform::vfs_stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VfsStats {
    /// Files opened: whole-file reads and streams that found their file.
    pub opens: u64,
    /// Reads and stream opens that failed (missing file, no mount, IO error).
    pub failed_opens: u64,
    /// Bytes handed to callers: whole files and what streams read.
    pub bytes_read: u64,
    /// Atomic writes that completed.
    pub writes: u64,
    /// Bytes those writes stored.
    pub bytes_written: u64,
}

impl VfsStats {
    /// `self - earlier`, per counter (saturating).
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            opens: self.opens.saturating_sub(earlier.opens),
            failed_opens: self.failed_opens.saturating_sub(earlier.failed_opens),
            bytes_read: self.bytes_read.saturating_sub(earlier.bytes_read),
            writes: self.writes.saturating_sub(earlier.writes),
            bytes_written: self.bytes_written.saturating_sub(earlier.bytes_written),
        }
    }
}

/// The VFS counters now (five relaxed loads).
#[must_use]
pub fn stats() -> VfsStats {
    VfsStats {
        opens: OPENS.load(Ordering::Relaxed),
        failed_opens: FAILED_OPENS.load(Ordering::Relaxed),
        bytes_read: BYTES_READ.load(Ordering::Relaxed),
        writes: WRITES.load(Ordering::Relaxed),
        bytes_written: BYTES_WRITTEN.load(Ordering::Relaxed),
    }
}

/// Counts one open; `bytes` is what a whole-file read returned (0 for a stream).
pub(super) fn note_open(bytes: usize) {
    OPENS.fetch_add(1, Ordering::Relaxed);
    note_read(bytes);
}

/// Counts a failed read or open.
pub(super) fn note_failed_open() {
    FAILED_OPENS.fetch_add(1, Ordering::Relaxed);
}

/// Counts bytes read.
fn note_read(bytes: usize) {
    if bytes > 0 {
        BYTES_READ.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Counts a completed write of `bytes`.
pub(super) fn note_write(bytes: usize) {
    WRITES.fetch_add(1, Ordering::Relaxed);
    BYTES_WRITTEN.fetch_add(bytes as u64, Ordering::Relaxed);
}

/// A stream from `Vfs::open_stream` whose reads count toward [`VfsStats::bytes_read`].
pub(super) struct CountingReader(pub(super) Box<dyn super::VfsReader>);

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        note_read(n);
        Ok(n)
    }
}

impl Seek for CountingReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

// `VfsReader` comes from the blanket impl for every `Read + Seek + Send` type.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{DirMount, MountPoint, Vfs, VfsPath};

    /// Reads, a stream, a miss and a write through a real directory mount. Other tests of this
    /// binary use the VFS on parallel threads, so each counter is checked as "grew by at least".
    #[test]
    fn reads_streams_misses_and_writes_are_counted() {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "regia-vfs-stats-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("a.bin"), [7u8; 100]).expect("fixture");
        let mut vfs = Vfs::new();
        vfs.mount(MountPoint::User, DirMount::writable(&dir));
        let path = |rel: &str| VfsPath::new(MountPoint::User, rel).expect("valid path");
        let before = stats();
        assert_eq!(vfs.read_sync(&path("a.bin")).map(|b| b.len()).ok(), Some(100));
        let mut stream = vfs.open_stream(&path("a.bin")).expect("stream");
        let mut buf = [0u8; 40];
        stream.read_exact(&mut buf).expect("40 bytes");
        assert!(vfs.read_sync(&path("missing.bin")).is_err());
        vfs.write_atomic(&path("b.bin"), &[1u8; 30]).expect("write");
        let grew = stats().since(before);
        assert!(grew.opens >= 2, "{grew:?}");
        assert!(grew.bytes_read >= 140, "{grew:?}");
        assert!(grew.failed_opens >= 1, "{grew:?}");
        assert!(grew.writes >= 1 && grew.bytes_written >= 30, "{grew:?}");
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn since_subtracts_per_counter_and_saturates() {
        let later = VfsStats {
            opens: 5,
            failed_opens: 1,
            bytes_read: 100,
            writes: 2,
            bytes_written: 30,
        };
        let earlier = VfsStats {
            opens: 2,
            failed_opens: 3,
            bytes_read: 40,
            writes: 2,
            bytes_written: 10,
        };
        assert_eq!(
            later.since(earlier),
            VfsStats {
                opens: 3,
                failed_opens: 0,
                bytes_read: 60,
                writes: 0,
                bytes_written: 20,
            }
        );
    }
}
