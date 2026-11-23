// SPDX-License-Identifier: MPL-2.0
//! Android thread CPU clock (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`).

/// Nanoseconds of CPU time this thread has consumed.
#[allow(
    unsafe_code,
    reason = "ADR-8498: clock_gettime reads this thread's CPU clock; the value is never hashed"
)]
pub fn thread_cpu_now_ns() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // det-scan: allow(wall_clock): profiling only, never hashed
    // SAFETY: `ts` is a writable timespec; CLOCK_THREAD_CPUTIME_ID needs no handle and
    // writes only that out-pointer.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return None;
    }
    let sec = u64::try_from(ts.tv_sec).ok()?;
    let nsec = u64::try_from(ts.tv_nsec).ok()?;
    Some(sec.saturating_mul(1_000_000_000).saturating_add(nsec))
}
