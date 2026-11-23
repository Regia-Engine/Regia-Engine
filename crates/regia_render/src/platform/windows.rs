// SPDX-License-Identifier: MPL-2.0
//! Windows: the exit hook through the C runtime's `atexit`; no thread-priority control yet.

/// No-op here (`false`): the sky worker keeps the default priority. Windows would use
/// `SetThreadPriority(THREAD_PRIORITY_BELOW_NORMAL)` through `windows-sys`.
pub(crate) fn lower_current_thread_priority() -> bool {
    false
}

/// Not available here: the boot timeline counts from the renderer plugin's build instead.
pub(crate) fn process_age() -> Option<std::time::Duration> {
    None
}

/// Runs `hook` when `main` returns or the C runtime's `exit` runs (`atexit`), before the exit
/// handlers registered earlier (a loaded GPU driver's, run in reverse order). `false` if it could
/// not be registered.
///
/// **Not reached by `std::process::exit` on Windows:** Rust's `process::exit` calls `ExitProcess`
/// directly, which skips the C runtime's `atexit` list (unlike Linux, where it goes through libc
/// `exit`). A host that ends with `process::exit` calls `PipelineWarmQueue::shutdown` first.
///
/// A process ended with `TerminateProcess` or by a crash never runs it; neither does it need to:
/// the OS reclaims the workers with the process.
#[allow(unsafe_code)]
pub(crate) fn at_exit(hook: extern "C" fn()) -> bool {
    // SAFETY: `atexit` stores a plain function pointer; `hook` must not unwind, which every
    // caller guarantees by catching panics inside it.
    unsafe { libc::atexit(hook) == 0 }
}

/// Whether the renderer keeps its first Vulkan instance until the process ends (ADR-5794):
/// `false` here: no driver reload failure has been measured on Windows (the limit found on Linux
/// is glibc's static TLS surplus).
pub(crate) fn keeps_vulkan_drivers_loaded() -> bool {
    false
}

/// No Metal device on this platform (ADR-5860): the allocator report is the only GPU figure.
pub(crate) fn device_allocated_bytes(_device: &wgpu::Device) -> Option<u64> {
    None
}
