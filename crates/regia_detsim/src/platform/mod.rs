// SPDX-License-Identifier: MPL-2.0
//! Per-thread CPU clock for the tick profiler.
//!
//! The value is a measurement of the calling thread. It is never written into hashed
//! simulation state and never read from a lockstep step function. When the operating
//! system has no thread clock, or the call fails, [`thread_cpu_now_ns`] returns `None`
//! and the profiler falls back to monotonic wall time.

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "android")]
#[path = "android.rs"]
mod os;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "windows",
    target_os = "macos"
)))]
mod os {
    /// This target has no thread CPU clock.
    pub fn thread_cpu_now_ns() -> Option<u64> {
        None
    }
}

/// Nanoseconds of CPU time the calling thread has consumed, or `None` if this target
/// has no thread clock or the call fails.
pub fn thread_cpu_now_ns() -> Option<u64> {
    os::thread_cpu_now_ns()
}
