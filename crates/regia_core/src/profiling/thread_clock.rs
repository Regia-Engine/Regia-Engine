// SPDX-License-Identifier: MPL-2.0
//! The calling thread's CPU time ([`super::thread_cpu_time`]).
//!
//! `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` brings the running thread's accounting up to date
//! before it answers. `/proc/thread-self/schedstat` does not: the kernel updates it only when the
//! thread is switched out or ticked, so a reading between two short stretches of running work
//! often shows none of it (`regia_vfx/tests/frame_creep.rs`, ADR-214).

#![allow(unsafe_code)] // one libc call on a value this module owns; see the SAFETY note

use std::time::Duration;

/// CPU time the calling thread has run, `None` if the clock is unavailable.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn thread_cpu_time() -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec` owned by this frame; the call writes only it.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (rc == 0).then(|| {
        Duration::new(
            u64::try_from(ts.tv_sec).unwrap_or(0),
            u32::try_from(ts.tv_nsec).unwrap_or(0),
        )
    })
}

/// CPU time the calling thread has run: not measured on this platform.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) fn thread_cpu_time() -> Option<Duration> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use std::time::{Duration, Instant};

    /// A 5 ms busy loop reads as CPU time (the `schedstat` file read 0 for such stretches when no
    /// context switch fell inside them), and never as more than the wall time it took.
    #[test]
    fn counts_running_work_without_a_context_switch() {
        let start = super::thread_cpu_time().expect("thread clock");
        let wall = Instant::now();
        let mut x = 1u64;
        while wall.elapsed() < Duration::from_millis(5) {
            for _ in 0..1000 {
                x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
            }
        }
        let cpu = super::thread_cpu_time().expect("thread clock") - start;
        assert!(cpu > Duration::ZERO, "no CPU time counted for a 5 ms busy loop");
        assert!(cpu <= wall.elapsed() + Duration::from_millis(1), "{cpu:?} > wall");
    }
}
