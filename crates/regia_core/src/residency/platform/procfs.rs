// SPDX-License-Identifier: MPL-2.0
//! Linux and Android: `/proc/self/maps` lists the code mappings, `mincore` counts the pages the
//! kernel paged out, and `process_vm_readv` on this process reads one byte of every page.
//!
//! `process_vm_readv` rather than plain loads: a registered region can be freed, or a library
//! unloaded, between taking the list and reading it, and a load from an unmapped page would
//! crash the process, while the system call returns `EFAULT`. It reads through the kernel's
//! page-table walk, which faults a paged-out page back in and marks every page it reads
//! accessed, as a load would.

#![allow(unsafe_code)] // three libc calls on buffers this module owns; see each SAFETY note

use std::sync::atomic::{AtomicBool, Ordering};

use super::super::{Region, Sweep, code_regions, image_containing, registered_regions};

/// Remote iovecs per `process_vm_readv` (`IOV_MAX`).
const BATCH: usize = 1024;

/// Lists and reads this process's code mappings.
pub(in crate::residency) struct Sweeper {
    pid: libc::pid_t,
    page: usize,
    /// The object holding the engine's code (the executable, or the game library on Android).
    own_image: Option<String>,
    remote: Vec<libc::iovec>,
    local: Vec<u8>,
    resident: Vec<u8>,
}

// SAFETY: the iovecs are scratch space rebuilt by every sweep on whichever thread runs it. Their
// `iov_base` fields are plain addresses that Rust never loads through (only the kernel reads
// them, during `process_vm_readv`), and `read` clears them before it returns.
unsafe impl Send for Sweeper {}

impl Sweeper {
    /// `None` without a readable `/proc/self/maps` or a sane page size.
    pub(in crate::residency) fn new() -> Option<Self> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        let anchor = code_regions as fn(&str, Option<&str>) -> Vec<Region> as usize;
        // SAFETY: `sysconf` and `getpid` only read process state.
        let (page, pid) = unsafe { (libc::sysconf(libc::_SC_PAGESIZE), libc::getpid()) };
        let page = usize::try_from(page).ok().filter(|p| p.is_power_of_two())?;
        Some(Self {
            pid,
            page,
            own_image: image_containing(&maps, anchor),
            remote: Vec::with_capacity(BATCH),
            local: vec![0; BATCH],
            resident: Vec::new(),
        })
    }

    /// Reads every page of the registered regions (and of the code mappings with `code`)
    /// once. `None` when the kernel refuses the read (no `process_vm_readv`, or a sandbox
    /// forbids it). `stop` is polled between batches of pages: once set the rest of the sweep
    /// is abandoned and the partial numbers returned, so shutdown does not wait out a sweep
    /// that is faulting pages back in.
    pub(in crate::residency) fn sweep(&mut self, code: bool, stop: &AtomicBool) -> Option<Sweep> {
        let mut sweep = Sweep::default();
        for region in registered_regions() {
            let region = self.page_aligned(region);
            sweep.data_bytes += region.len as u64;
            sweep.restored += self.missing(&region);
            sweep.pages += self.read(&region, stop)?;
        }
        if code {
            let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
            for region in code_regions(&maps, self.own_image.as_deref()) {
                sweep.code_bytes += region.len as u64;
                sweep.restored += self.missing(&region);
                sweep.pages += self.read(&region, stop)?;
            }
        }
        Some(sweep)
    }

    /// `region` widened to whole pages (`mincore` wants a page-aligned start).
    fn page_aligned(&self, region: Region) -> Region {
        let start = region.start & !(self.page - 1);
        let end = (region.start + region.len).next_multiple_of(self.page);
        Region {
            start,
            len: end - start,
        }
    }

    /// Pages of `region` not in memory (0 when the region is gone).
    fn missing(&mut self, region: &Region) -> u64 {
        let pages = region.len.div_ceil(self.page);
        self.resident.clear();
        self.resident.resize(pages, 0);
        // SAFETY: `resident` holds one byte per page of the range, as `mincore` requires; an
        // unmapped range fails with ENOMEM and writes nothing.
        let rc = unsafe {
            libc::mincore(
                region.start as *mut libc::c_void,
                region.len,
                self.resident.as_mut_ptr().cast(),
            )
        };
        if rc != 0 {
            return 0;
        }
        self.resident.iter().filter(|&&r| r & 1 == 0).count() as u64
    }

    /// Reads one byte of every page of `region`; the pages read. Stops early when the region
    /// was unmapped meanwhile, or `stop` is set.
    fn read(&mut self, region: &Region, stop: &AtomicBool) -> Option<u64> {
        let mut read = 0u64;
        let mut addr = region.start;
        let end = region.start + region.len;
        while addr < end {
            if stop.load(Ordering::Relaxed) {
                return Some(read);
            }
            self.remote.clear();
            while addr < end && self.remote.len() < BATCH {
                self.remote.push(libc::iovec {
                    iov_base: addr as *mut libc::c_void,
                    iov_len: 1,
                });
                addr += self.page;
            }
            let n = self.remote.len();
            let local = libc::iovec {
                iov_base: self.local.as_mut_ptr().cast(),
                iov_len: n,
            };
            // SAFETY: `local` points at `n` writable bytes this sweeper owns; the remote
            // iovecs are addresses in this process, which the kernel validates (an unmapped
            // one fails the call with EFAULT instead of faulting this thread).
            let rc = unsafe {
                libc::process_vm_readv(self.pid, &raw const local, 1, self.remote.as_ptr(), n as _, 0)
            };
            if rc < 0 {
                match classify_failure(std::io::Error::last_os_error().raw_os_error()) {
                    Failure::Retry => {
                        // Interrupted before any byte moved: redo this batch.
                        addr -= n * self.page;
                        continue;
                    }
                    Failure::Refused => {
                        self.remote.clear();
                        return None;
                    }
                    // EFAULT: the region went away (a library unloaded); the rest is gone too.
                    Failure::Gone => {
                        self.remote.clear();
                        return Some(read);
                    }
                }
            }
            read += rc as u64;
            if (rc as usize) < n {
                // A partial read stops at the first page that is no longer mapped.
                self.remote.clear();
                return Some(read);
            }
        }
        self.remote.clear();
        Some(read)
    }
}

/// What a failed `process_vm_readv` means for the sweep.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// `EINTR`: a signal arrived first; try the same batch again.
    Retry,
    /// The kernel will not let this process read itself (no syscall, or a sandbox policy);
    /// the keeper cannot work.
    Refused,
    /// The region is no longer mapped (or another per-region error): skip the rest of it.
    Gone,
}

fn classify_failure(errno: Option<i32>) -> Failure {
    match errno {
        Some(libc::EINTR) => Failure::Retry,
        Some(libc::ENOSYS | libc::EPERM | libc::EACCES) => Failure::Refused,
        _ => Failure::Gone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_classified() {
        assert_eq!(classify_failure(Some(libc::EINTR)), Failure::Retry);
        for e in [libc::ENOSYS, libc::EPERM, libc::EACCES] {
            assert_eq!(classify_failure(Some(e)), Failure::Refused);
        }
        assert_eq!(classify_failure(Some(libc::EFAULT)), Failure::Gone);
        assert_eq!(classify_failure(None), Failure::Gone);
    }

    /// An address range that was never mapped: the kernel refuses it, the sweeper counts nothing, and
    /// this process takes no signal. Below `mmap_min_addr`, so no other thread can map it either.
    #[test]
    fn a_region_that_is_not_mapped_reads_nothing_and_does_not_fault() {
        let mut sweeper = Sweeper::new().expect("procfs sweeper");
        let region = Region {
            start: sweeper.page,
            len: sweeper.page * 4,
        };
        assert_eq!(
            sweeper.missing(&region),
            0,
            "mincore's ENOMEM counts no page as missing"
        );
        assert_eq!(sweeper.read(&region, &AtomicBool::new(false)), Some(0));
        assert!(
            sweeper.remote.is_empty(),
            "the iovecs are scratch: none survives a sweep"
        );
    }

    /// A region whose tail became unreadable: one page reads, the next stops the region. `PROT_NONE`
    /// rather than `munmap`, so another thread's mapping cannot land on it during the test.
    #[test]
    fn a_partial_read_stops_at_the_first_unreadable_page() {
        let mut sweeper = Sweeper::new().expect("procfs sweeper");
        let page = sweeper.page;
        // SAFETY: a fresh anonymous mapping; the kernel picks the address.
        let start = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page * 3,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(start, libc::MAP_FAILED, "mmap");
        // SAFETY: the last two pages of the mapping made above.
        let rc = unsafe { libc::mprotect(start.cast::<u8>().add(page).cast(), page * 2, libc::PROT_NONE) };
        assert_eq!(rc, 0, "mprotect");
        let region = Region {
            start: start.addr(),
            len: page * 3,
        };
        assert_eq!(sweeper.read(&region, &AtomicBool::new(false)), Some(1));
        assert!(sweeper.remote.is_empty());
        // SAFETY: the mapping made above, unmapped once.
        unsafe { libc::munmap(start, page * 3) };
    }

    /// More pages than one `process_vm_readv` takes: the batches add up.
    #[test]
    fn a_region_longer_than_a_batch_reads_every_page() {
        let mut sweeper = Sweeper::new().expect("procfs sweeper");
        let pages = BATCH + 3;
        let data = vec![1u8; (pages + 1) * sweeper.page];
        let region = sweeper.page_aligned(Region {
            start: data.as_ptr().addr(),
            len: pages * sweeper.page,
        });
        let read = sweeper.read(&region, &AtomicBool::new(false)).expect("read");
        assert!(read >= pages as u64, "{read} pages of {pages}");
    }

    /// A sweep started with `stop` already set reads no page, and one that is not stopped
    /// reads the registered region.
    #[test]
    fn sweep_stops_between_batches() {
        let data = vec![1u8; 4 << 20];
        let _region = crate::residency::ResidentRegion::of_slice(&data);
        let mut sweeper = Sweeper::new().expect("procfs sweeper");
        let stopped = sweeper.sweep(false, &AtomicBool::new(true)).expect("sweep");
        assert_eq!(stopped.pages, 0);
        let full = sweeper.sweep(false, &AtomicBool::new(false)).expect("sweep");
        assert!(full.pages >= 64, "{full:?}");
    }
}
