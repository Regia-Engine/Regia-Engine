// SPDX-License-Identifier: MPL-2.0
//! Keeps the data (and optionally the code) a frame may need at any moment paged in
//! (`specs/27` § 2.4: no frame over 50 ms).
//!
//! On a machine short of memory the OS pages out the least recently used memory of every
//! process. For a game that is what it touches rarely: the terrain heights far from the camera,
//! read when an effect, a decal or a body is placed on the ground; the code of a spawn, a cut, a
//! menu. The next frame that touches such a page sleeps in a major page fault while it is read
//! back from swap or disk: 100–600 ms on a loaded box (the `[spawn]` frames of the 2026-10-08
//! hour soaks, `docs/evidence/soak/2026-10-08-1551/`: a bilinear height sample ahead of the
//! vehicle, two rows, two faults), with the CPU idle and no engine span covering it.
//!
//! [`ResidencyKeeper`] moves that wait off the frame. A background thread reads one byte of
//! every page of every [`ResidentRegion`] each [`ResidencySettings::period`] (and, with
//! [`ResidencySettings::code`], of the process's code mappings). The reads mark the pages
//! recently used, so the kernel's reclaim takes other pages first; a page that was paged out
//! anyway is read back by the keeper, not by the frame. Only registered data is kept: the heap
//! as a whole stays the OS's to page, so memory the game really does not use can still go.
//!
//! The reads go through `process_vm_readv` on the process itself, so a region freed or moved
//! while a sweep runs is never a crash (the call fails with `EFAULT`). The cost is one page-table
//! walk per 4 KiB page per period: a 64 MiB height map is 16 384 walks, about a millisecond of
//! the keeper's thread every half second.
//!
//! Linux and Android (`/proc/self/maps`, `process_vm_readv`, `mincore`). Elsewhere
//! [`ResidencyKeeper::start`] returns `None` and regions register for nothing.

mod platform;
mod squeeze;

pub use squeeze::{MemorySqueeze, SqueezeSettings};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use std::time::{Duration, Instant};

/// How the keeper runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidencySettings {
    /// Time between sweeps. The kernel ages pages over seconds under pressure, so a sweep
    /// every half second keeps the regions young; a shorter period only costs CPU.
    pub period: Duration,
    /// Also sweep the process's code: every executable file mapping (the game image, the
    /// graphics driver, libc) and the game image's read-only data. Off by default: on the
    /// 2026-10-08 farm soaks the spawn stalls were data, and sweeping code did not reduce them.
    pub code: bool,
}

impl Default for ResidencySettings {
    fn default() -> Self {
        Self {
            period: Duration::from_millis(500),
            code: false,
        }
    }
}

/// What the keeper has done so far ([`ResidencyKeeper::stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResidencyStats {
    /// Bytes of registered data the last sweep covered.
    pub data_bytes: u64,
    /// Bytes of code the last sweep covered (0 without [`ResidencySettings::code`]).
    pub code_bytes: u64,
    /// Pages the last sweep read.
    pub pages: u64,
    /// Sweeps finished.
    pub sweeps: u64,
    /// Pages a sweep found paged out and read back, summed over all sweeps: each one a major
    /// fault the keeper took instead of a frame.
    pub restored_pages: u64,
    /// The last sweep's wall time, µs (long when it read pages back).
    pub last_sweep_us: u64,
    /// The longest sweep so far, µs.
    pub max_sweep_us: u64,
}

#[derive(Default)]
struct Counters {
    data_bytes: AtomicU64,
    code_bytes: AtomicU64,
    pages: AtomicU64,
    sweeps: AtomicU64,
    restored_pages: AtomicU64,
    last_sweep_us: AtomicU64,
    max_sweep_us: AtomicU64,
    /// Sequence lock over the fields above: odd while `record` is storing them.
    seq: AtomicU64,
}

/// A range of memory the keeper keeps paged in while this handle lives (registration is
/// process-wide; it costs nothing where no keeper runs).
///
/// Register data that a frame may read at any point but that nothing reads every frame: a
/// height map queried where things spawn, a navigation grid, a lookup table for a rare event.
/// The handle stores the range, not a borrow: drop it with (or before) the data. A range that
/// outlives its data is harmless, only wasted reads (the keeper never dereferences it itself).
#[derive(Debug)]
pub struct ResidentRegion {
    id: u64,
}

struct Registry {
    next: u64,
    regions: Vec<(u64, Region)>,
}

static REGISTRY: parking_lot::Mutex<Registry> = parking_lot::Mutex::new(Registry {
    next: 1,
    regions: Vec::new(),
});

impl ResidentRegion {
    /// Keeps `slice`'s memory paged in. An empty slice registers nothing.
    #[must_use]
    pub fn of_slice<T>(slice: &[T]) -> Self {
        let len = std::mem::size_of_val(slice);
        if len == 0 {
            return Self { id: 0 };
        }
        let mut registry = REGISTRY.lock();
        let id = registry.next;
        registry.next += 1;
        registry.regions.push((
            id,
            Region {
                start: slice.as_ptr() as usize,
                len,
            },
        ));
        Self { id }
    }

    /// Bytes registered by every live handle in the process.
    #[must_use]
    pub fn registered_bytes() -> u64 {
        REGISTRY.lock().regions.iter().map(|(_, r)| r.len as u64).sum()
    }
}

impl Drop for ResidentRegion {
    fn drop(&mut self) {
        if self.id != 0 {
            REGISTRY.lock().regions.retain(|(id, _)| *id != self.id);
        }
    }
}

/// The registered regions now (a copy: the sweep runs without the lock).
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn registered_regions() -> Vec<Region> {
    REGISTRY.lock().regions.iter().map(|(_, r)| *r).collect()
}

/// The running keeper; stops (and joins its thread) when dropped.
pub struct ResidencyKeeper {
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ResidencyKeeper {
    /// Starts the sweeping thread. `None` where the platform cannot read its own memory this
    /// way (not Linux or Android, or a sandbox that refuses the call), or the thread did not
    /// start.
    #[must_use]
    pub fn start(settings: ResidencySettings) -> Option<Self> {
        let mut sweeper = platform::Sweeper::new()?;
        // One sweep up front: proves the platform calls work, and pages everything in now.
        let stop = Arc::new(AtomicBool::new(false));
        let first = sweeper.sweep(settings.code, &stop)?;
        let counters = Arc::new(Counters::default());
        counters.record(first, 0);
        let (stop_t, counters_t) = (Arc::clone(&stop), Arc::clone(&counters));
        let period = settings.period.max(Duration::from_millis(10));
        let thread = std::thread::Builder::new()
            .name("regia-residency".into())
            .spawn(move || {
                while !stop_t.load(Ordering::Relaxed) {
                    std::thread::park_timeout(period);
                    if stop_t.load(Ordering::Relaxed) {
                        break;
                    }
                    let started = Instant::now();
                    let Some(sweep) = sweeper.sweep(settings.code, &stop_t) else {
                        tracing::warn!(target: "regia_core", "residency keeper: sweep failed; stopping");
                        break;
                    };
                    let us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    counters_t.record(sweep, us);
                }
            })
            .ok()?;
        tracing::info!(
            target: "regia_core",
            "residency keeper: {} MiB of data, {} MiB of code, every {} ms",
            first.data_bytes >> 20,
            first.code_bytes >> 20,
            period.as_millis()
        );
        Some(Self {
            stop,
            counters,
            thread: Some(thread),
        })
    }

    /// What the keeper has done so far.
    #[must_use]
    pub fn stats(&self) -> ResidencyStats {
        self.counters.snapshot()
    }
}

impl Counters {
    /// Publishes one sweep. One writer at a time (the keeper thread, after `start`'s first call).
    fn record(&self, sweep: Sweep, us: u64) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.data_bytes.store(sweep.data_bytes, Ordering::Relaxed);
        self.code_bytes.store(sweep.code_bytes, Ordering::Relaxed);
        self.pages.store(sweep.pages, Ordering::Relaxed);
        self.restored_pages.fetch_add(sweep.restored, Ordering::Relaxed);
        self.last_sweep_us.store(us, Ordering::Relaxed);
        self.max_sweep_us.fetch_max(us, Ordering::Relaxed);
        self.sweeps.fetch_add(1, Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// All fields of one sweep, never a mix of two (a HUD would show this sweep's fault time
    /// next to the previous sweep's count). Retries while a `record` is in progress; that
    /// takes a handful of stores.
    fn snapshot(&self) -> ResidencyStats {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let stats = ResidencyStats {
                data_bytes: self.data_bytes.load(Ordering::Relaxed),
                code_bytes: self.code_bytes.load(Ordering::Relaxed),
                pages: self.pages.load(Ordering::Relaxed),
                sweeps: self.sweeps.load(Ordering::Relaxed),
                restored_pages: self.restored_pages.load(Ordering::Relaxed),
                last_sweep_us: self.last_sweep_us.load(Ordering::Relaxed),
                max_sweep_us: self.max_sweep_us.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return stats;
            }
        }
    }
}

impl Drop for ResidencyKeeper {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// One sweep's numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Sweep {
    data_bytes: u64,
    code_bytes: u64,
    pages: u64,
    restored: u64,
}

/// A mapping the keeper covers: `[start, start + len)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
struct Region {
    start: usize,
    len: usize,
}

/// The mappings of a `/proc/<pid>/maps` listing the keeper covers: file-backed, readable, and
/// either executable (code of any loaded object) or part of `own_image` (the object holding
/// the engine's code: its read-only data too). Devices, memfds and anonymous mappings are
/// skipped.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn code_regions(maps: &str, own_image: Option<&str>) -> Vec<Region> {
    let mut out = Vec::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(perms), Some(_offset), Some(_dev), Some(inode)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        // The path may hold spaces (and ends in " (deleted)" for a replaced file).
        let path = fields.collect::<Vec<_>>().join(" ");
        let perms = perms.as_bytes();
        if perms.first() != Some(&b'r') || inode == "0" || !path.starts_with('/') {
            continue;
        }
        if path.starts_with("/dev/") || path.starts_with("/memfd:") {
            continue;
        }
        let executable = perms.get(2) == Some(&b'x');
        let writable = perms.get(1) == Some(&b'w');
        if !(executable || (!writable && own_image == Some(path.as_str()))) {
            continue;
        }
        let Some((a, b)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (usize::from_str_radix(a, 16), usize::from_str_radix(b, 16)) else {
            continue;
        };
        if end > start {
            out.push(Region {
                start,
                len: end - start,
            });
        }
    }
    out
}

/// The path of the mapping that contains `addr` in a `/proc/<pid>/maps` listing.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn image_containing(maps: &str, addr: usize) -> Option<String> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (a, b) = fields.next()?.split_once('-')?;
        let (start, end) = (
            usize::from_str_radix(a, 16).ok()?,
            usize::from_str_radix(b, 16).ok()?,
        );
        if !(start..end).contains(&addr) {
            return None;
        }
        let path = fields.skip(4).collect::<Vec<_>>().join(" ");
        path.starts_with('/').then_some(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
5555aaaa0000-5555aaab0000 r--p 00000000 103:02 4242 /opt/game/bin/game
5555aaab0000-5555aac00000 r-xp 00010000 103:02 4242 /opt/game/bin/game
5555aac00000-5555aac10000 r--p 00160000 103:02 4242 /opt/game/bin/game
5555aac10000-5555aac11000 rw-p 00170000 103:02 4242 /opt/game/bin/game
5555aac11000-5555aac40000 rw-p 00000000 00:00 0 [heap]
7f0000000000-7f0000100000 r--p 00000000 103:02 77 /usr/lib/libnvidia-glcore.so.615
7f0000100000-7f0002000000 r-xp 00100000 103:02 77 /usr/lib/libnvidia-glcore.so.615
7f0003000000-7f0003001000 rw-s 00000000 00:05 12 /dev/nvidia0
7f0004000000-7f0004100000 r-xp 00000000 00:01 9 /memfd:jit (deleted)
7f0005000000-7f0005100000 r--p 00000000 103:02 88 /srv/content/level.pak
7f0006000000-7f0006010000 r-xp 00000000 103:02 99 /home/dev/My Game/libgame.so
7ffd00000000-7ffd00002000 r-xp 00000000 00:00 0 [vdso]
";

    #[test]
    fn code_regions_take_code_of_every_object_and_the_images_read_only_data() {
        let regions = code_regions(MAPS, Some("/opt/game/bin/game"));
        let starts: Vec<usize> = regions.iter().map(|r| r.start).collect();
        assert_eq!(
            starts,
            vec![
                0x5555_aaaa_0000, // the image's headers / read-only data
                0x5555_aaab_0000, // the image's code
                0x5555_aac0_0000, // the image's read-only data (after relocation)
                0x7f00_0010_0000, // the driver's code (not its read-only data)
                0x7f00_0600_0000, // a path with a space
            ]
        );
        assert_eq!(regions[1].len, 0x15_0000);
    }

    #[test]
    fn without_an_own_image_only_code_is_taken() {
        let regions = code_regions(MAPS, None);
        assert_eq!(regions.len(), 3);
        assert!(regions.iter().all(|r| r.len > 0));
    }

    #[test]
    fn the_image_is_the_mapping_holding_the_address() {
        assert_eq!(
            image_containing(MAPS, 0x5555_aab0_1234).as_deref(),
            Some("/opt/game/bin/game")
        );
        assert_eq!(
            image_containing(MAPS, 0x7f00_0600_0010).as_deref(),
            Some("/home/dev/My Game/libgame.so")
        );
        assert_eq!(
            image_containing(MAPS, 0x5555_aac2_0000),
            None,
            "[heap] is not an image"
        );
        assert_eq!(image_containing(MAPS, 0x10), None);
    }

    fn wait_for_sweeps(keeper: &ResidencyKeeper, n: u64) -> ResidencyStats {
        let deadline = Instant::now() + Duration::from_secs(10);
        while keeper.stats().sweeps < n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        keeper.stats()
    }

    #[test]
    fn regions_register_while_their_handle_lives() {
        let data = vec![1u32; 64 * 1024];
        let before = ResidentRegion::registered_bytes();
        let region = ResidentRegion::of_slice(&data);
        assert!(ResidentRegion::registered_bytes() >= before + 256 * 1024);
        assert!(
            registered_regions()
                .iter()
                .any(|r| r.start == data.as_ptr() as usize && r.len == 256 * 1024)
        );
        drop(region);
        assert!(
            !registered_regions()
                .iter()
                .any(|r| r.start == data.as_ptr() as usize)
        );
        let empty: [u8; 0] = [];
        assert_eq!(ResidentRegion::of_slice(&empty).id, 0);
    }

    /// `stats` never mixes two sweeps: a writer storing equal values in every field is never
    /// seen half-way by a reader.
    #[test]
    fn stats_snapshot_is_never_torn() {
        let counters = Arc::new(Counters::default());
        let writer = {
            let counters = Arc::clone(&counters);
            std::thread::spawn(move || {
                for n in 1..=20_000u64 {
                    counters.record(
                        Sweep {
                            data_bytes: n,
                            code_bytes: n,
                            pages: n,
                            restored: 0,
                        },
                        n,
                    );
                }
            })
        };
        for _ in 0..20_000 {
            let s = counters.snapshot();
            assert!(
                s.data_bytes == s.code_bytes
                    && s.code_bytes == s.pages
                    && s.pages == s.last_sweep_us
                    && s.sweeps == s.pages,
                "torn: {s:?}"
            );
        }
        writer.join().expect("writer");
    }

    /// On Linux the keeper sweeps registered data (and only that by default), and stops when
    /// dropped.
    #[test]
    fn keeper_sweeps_registered_data_where_supported() {
        let data = vec![7u8; 1 << 20];
        let _region = ResidentRegion::of_slice(&data);
        let keeper = ResidencyKeeper::start(ResidencySettings {
            period: Duration::from_millis(10),
            code: false,
        });
        if !cfg!(any(target_os = "linux", target_os = "android")) {
            assert!(keeper.is_none());
            return;
        }
        let Some(keeper) = keeper else {
            panic!("procfs platforms sweep their own memory");
        };
        let stats = wait_for_sweeps(&keeper, 3);
        assert!(stats.sweeps >= 3, "{stats:?}");
        assert!(stats.data_bytes >= 1 << 20, "{stats:?}");
        assert_eq!(stats.code_bytes, 0, "code is opt-in: {stats:?}");
        assert!(stats.pages >= 256, "{stats:?}");
        drop(keeper);
    }

    /// With `code`, the keeper also covers this test binary's code.
    #[test]
    fn keeper_sweeps_the_process_code_when_asked() {
        let keeper = ResidencyKeeper::start(ResidencySettings {
            period: Duration::from_millis(10),
            code: true,
        });
        if !cfg!(any(target_os = "linux", target_os = "android")) {
            assert!(keeper.is_none());
            return;
        }
        let Some(keeper) = keeper else {
            panic!("procfs platforms sweep their own code");
        };
        let stats = wait_for_sweeps(&keeper, 2);
        assert!(
            stats.code_bytes > 256 * 1024,
            "the test binary's code is covered: {stats:?}"
        );
        assert!(stats.pages >= stats.code_bytes / 65_536, "{stats:?}");
    }
}
