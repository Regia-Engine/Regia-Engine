// SPDX-License-Identifier: MPL-2.0
//! Windows thread CPU clock (`GetThreadTimes`).
//!
//! `QueryThreadCycleTime` counts cycles, not a duration, so the profiler does not use it.

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

/// Nanoseconds of CPU time this thread has consumed (user plus kernel).
#[allow(
    unsafe_code,
    reason = "ADR-8498: GetThreadTimes reads this thread's CPU time; the value is never hashed"
)]
pub fn thread_cpu_now_ns() -> Option<u64> {
    let mut creation = zeroed();
    let mut exit = zeroed();
    let mut kernel = zeroed();
    let mut user = zeroed();
    // det-scan: allow(wall_clock): profiling only, never hashed
    // SAFETY: `GetCurrentThread` returns a pseudohandle that must not be closed. The four
    // FILETIME outs are written only on success and are not read before that.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // FILETIME is a count of 100-nanosecond intervals.
    let hundred_ns = filetime_100ns(kernel).saturating_add(filetime_100ns(user));
    Some(hundred_ns.saturating_mul(100))
}

fn zeroed() -> FILETIME {
    FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }
}

fn filetime_100ns(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}
