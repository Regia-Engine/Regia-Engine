// SPDX-License-Identifier: MPL-2.0
//! Reading memory that may be unmapped, from a signal handler.
//!
//! A load from a bad address inside the crash handler would fault again and lose the record. Here
//! the kernel does the load: `write`-ing the address range into a pipe copies it from this process,
//! and an unmapped page is an `EFAULT` return (no signal). The Android handler walks frame pointers
//! with it. Nothing in it is Android-specific, so it is also built and tested on the Linux host.

use std::sync::atomic::{AtomicI32, Ordering};

/// A non-blocking pipe for checked reads of this process's memory.
#[derive(Debug)]
pub(super) struct MemoryProbe {
    read: AtomicI32,
    write: AtomicI32,
}

impl MemoryProbe {
    /// A probe with no pipe yet: every read is `None`.
    pub(super) const fn new() -> Self {
        Self {
            read: AtomicI32::new(-1),
            write: AtomicI32::new(-1),
        }
    }

    /// Creates the pipe. Not for the signal handler: run it when the handler is installed.
    pub(super) fn open(&self) {
        let mut fds: [libc::c_int; 2] = [-1; 2];
        // SAFETY: `pipe2` fills the two-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == 0 {
            self.read.store(fds[0], Ordering::SeqCst);
            self.write.store(fds[1], Ordering::SeqCst);
        }
    }

    /// The two words at `addr`, or `None` when they cannot all be read. Async-signal-safe: atomics,
    /// `write` and `read` on stack values. One thread at a time: the handler runs once per process.
    pub(super) fn read_pair(&self, addr: u64) -> Option<[u64; 2]> {
        let (r, w) = (
            self.read.load(Ordering::SeqCst),
            self.write.load(Ordering::SeqCst),
        );
        if r < 0 || w < 0 {
            return None;
        }
        let mut buf = [0u8; 16];
        // Bytes an interrupted earlier drain left in the pipe would be read as this address's
        // words (audit v0.2.12): empty the pipe first. Non-blocking, so it stops at EAGAIN; the
        // bound keeps a broken pipe from spinning the handler.
        for _ in 0..64 {
            // SAFETY: reads at most 16 bytes into the stack buffer.
            let m = unsafe { libc::read(r, buf.as_mut_ptr().cast(), buf.len()) };
            let more = m > 0 || (m < 0 && interrupted());
            if !more {
                break;
            }
        }
        // SAFETY: the kernel checks `addr`: it copies 16 bytes into the pipe, or fewer up to the first
        // unreadable page, or fails with EFAULT, and this thread takes no signal either way. Rust
        // never loads through `addr`. The pipe is then drained into the stack buffer, at most `n`
        // (<= 16) bytes, so a short copy leaves nothing behind for the next read.
        unsafe {
            let n = libc::write(w, addr as usize as *const libc::c_void, 16);
            if n <= 0 {
                return None;
            }
            let n = n.cast_unsigned();
            let mut got = 0usize;
            let mut tries = 0;
            while got < n && tries < 64 {
                tries += 1;
                let m = libc::read(r, buf.as_mut_ptr().add(got).cast(), n - got);
                if m < 0 && interrupted() {
                    continue; // a nested signal: the bytes are still there
                }
                if m <= 0 {
                    break;
                }
                got += m.cast_unsigned();
            }
            if n != 16 || got != 16 {
                return None;
            }
        }
        let (lo, hi) = buf.split_at(8);
        let word = |b: &[u8]| {
            let mut v = [0u8; 8];
            v.copy_from_slice(b);
            u64::from_ne_bytes(v)
        };
        Some([word(lo), word(hi)])
    }
}

/// Whether the last libc call failed with `EINTR`. `last_os_error` only reads `errno` (no
/// allocation), so it is safe in the signal handler.
fn interrupted() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}

/// The handler's probe is a `static`, whose pipe lives as long as the process; only tests make others.
#[cfg(test)]
impl Drop for MemoryProbe {
    fn drop(&mut self) {
        for fd in [
            self.read.load(Ordering::SeqCst),
            self.write.load(Ordering::SeqCst),
        ] {
            if fd >= 0 {
                // SAFETY: this probe opened `fd` and nothing else closes it.
                unsafe { libc::close(fd) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryProbe;

    fn opened() -> MemoryProbe {
        let probe = MemoryProbe::new();
        probe.open();
        probe
    }

    /// Two anonymous pages, the second one inaccessible. `PROT_NONE` rather than `munmap`: no other
    /// thread's mapping can land on it during the test.
    struct GuardedPage {
        start: *mut u8,
        page: usize,
    }

    impl GuardedPage {
        fn new() -> Self {
            // SAFETY: `sysconf` only reads process state.
            let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).expect("page size");
            // SAFETY: a fresh anonymous mapping; the kernel picks the address.
            let start = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    page * 2,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(start, libc::MAP_FAILED, "mmap");
            // SAFETY: `start + page` is the second page of the mapping made above.
            let rc = unsafe { libc::mprotect(start.cast::<u8>().add(page).cast(), page, libc::PROT_NONE) };
            assert_eq!(rc, 0, "mprotect");
            Self {
                start: start.cast(),
                page,
            }
        }

        /// The address `bytes` before the guard.
        fn before_guard(&self, bytes: usize) -> *mut u8 {
            // SAFETY: `bytes <= page`, so the result stays inside the first page.
            unsafe { self.start.add(self.page - bytes) }
        }
    }

    impl Drop for GuardedPage {
        fn drop(&mut self) {
            // SAFETY: the mapping made in `new`, unmapped once.
            unsafe { libc::munmap(self.start.cast(), self.page * 2) };
        }
    }

    /// Audit v0.2.12 (Zai L4): bytes an interrupted drain left in the pipe were read back as the
    /// next address's words. Stale bytes are now emptied before each read.
    #[test]
    fn stale_bytes_in_the_pipe_do_not_shift_the_next_read() {
        let probe = opened();
        let w = probe.write.load(std::sync::atomic::Ordering::SeqCst);
        let junk = [0xEEu8; 8];
        // SAFETY: writes 8 bytes from a stack array into this probe's own pipe.
        assert_eq!(unsafe { libc::write(w, junk.as_ptr().cast(), junk.len()) }, 8);
        let value = [0x1111_2222_3333_4444u64, 0x5555_6666_7777_8888];
        assert_eq!(probe.read_pair(value.as_ptr() as u64), Some(value));
        assert_eq!(
            probe.read_pair(value.as_ptr() as u64),
            Some(value),
            "and the next one"
        );
    }

    #[test]
    fn a_probe_with_no_pipe_reads_nothing() {
        let value = [1u64, 2];
        assert_eq!(MemoryProbe::new().read_pair(value.as_ptr() as u64), None);
    }

    #[test]
    fn readable_memory_reads_back_at_any_alignment() {
        let probe = opened();
        let bytes: [u8; 40] = std::array::from_fn(|i| i as u8);
        for offset in 0..8 {
            let at = &bytes[offset..];
            let expected = [
                u64::from_ne_bytes(at[..8].try_into().expect("8 bytes")),
                u64::from_ne_bytes(at[8..16].try_into().expect("8 bytes")),
            ];
            assert_eq!(
                probe.read_pair(at.as_ptr() as u64),
                Some(expected),
                "offset {offset}"
            );
        }
    }

    /// The bad addresses of a corrupt frame chain: this process stays alive (it would die of SIGSEGV
    /// on a plain load).
    #[test]
    fn unreadable_addresses_are_none_not_a_fault() {
        let probe = opened();
        let guard = GuardedPage::new();
        for addr in [0, 0x10, guard.before_guard(0) as u64, u64::MAX - 7, 1 << 62] {
            assert_eq!(probe.read_pair(addr), None, "{addr:#x}");
        }
    }

    /// A pair that crosses into a bad page: the kernel copies only the readable half. It is refused,
    /// and the half copied does not show up in the next read.
    #[test]
    fn a_short_copy_is_refused_and_leaves_the_pipe_empty() {
        let probe = opened();
        let guard = GuardedPage::new();
        let end = guard.before_guard(16);
        for i in 0..16u8 {
            // SAFETY: the 16 bytes before the guard are in the writable first page.
            unsafe { end.add(usize::from(i)).write(0xA0 + i) };
        }
        assert_eq!(
            probe.read_pair(guard.before_guard(8) as u64),
            None,
            "crosses the guard"
        );
        assert_eq!(
            probe.read_pair(guard.before_guard(1) as u64),
            None,
            "crosses the guard"
        );
        let first = u64::from_ne_bytes([0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7]);
        let second = u64::from_ne_bytes([0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF]);
        assert_eq!(probe.read_pair(end as u64), Some([first, second]));
    }
}
