// SPDX-License-Identifier: MPL-2.0
//! The scheduler's own account of the calling thread, for the pacing trace (ADR-5946;
//! `getrusage(2)`, `getcpu(2)`, `proc(5)` `schedstat`).

use std::cell::RefCell;
use std::fs::File;
use std::os::unix::fs::FileExt;

use crate::frame_pacing::{SchedSample, parse_schedstat_run_delay};

/// `RUSAGE_THREAD` (Linux 2.6.26): the usage of the calling thread only.
const RUSAGE_THREAD: libc::c_int = 1;

thread_local! {
    /// `/proc/thread-self/schedstat` of this thread, opened once (`Some(None)`: it cannot be opened).
    static SCHEDSTAT: RefCell<Option<Option<File>>> = const { RefCell::new(None) };
}

/// The calling thread's scheduler counters, or `None` where the kernel does not keep them
/// (`/proc/thread-self/schedstat` needs scheduler statistics; every Android common kernel has them).
/// Three system calls, no allocation after the first call on a thread.
pub(crate) fn thread_sched_sample() -> Option<SchedSample> {
    let run_delay_ns = SCHEDSTAT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let file = slot.get_or_insert_with(|| File::open("/proc/thread-self/schedstat").ok());
        let mut text = [0u8; 96];
        let read = file.as_ref()?.read_at(&mut text, 0).ok()?;
        parse_schedstat_run_delay(std::str::from_utf8(text.get(..read)?).ok()?)
    })?;
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` is a valid out pointer for one `rusage`.
    let counted = unsafe { libc::getrusage(RUSAGE_THREAD, usage.as_mut_ptr()) } == 0;
    // SAFETY: zero-initialised above, and filled in by the kernel when `counted`.
    let usage = unsafe { usage.assume_init() };
    let preemptions = if counted {
        u64::try_from(usage.ru_nivcsw).unwrap_or(0)
    } else {
        0
    };
    let mut cpu: libc::c_uint = 0;
    // SAFETY: `getcpu` writes one unsigned integer through its first pointer; the other two may
    // be null.
    let located = unsafe {
        libc::syscall(
            libc::SYS_getcpu,
            std::ptr::from_mut(&mut cpu),
            std::ptr::null_mut::<libc::c_uint>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    } == 0;
    Some(SchedSample {
        run_delay_ns,
        preemptions,
        cpu: if located { cpu } else { u32::MAX },
    })
}
