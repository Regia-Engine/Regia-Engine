// SPDX-License-Identifier: MPL-2.0
//! macOS (testing only, ADR-264): the exit hook through libc's `atexit` and the Metal device's
//! allocated size (ADR-5860); no thread-priority control or process start time yet.

/// No-op here (`false`): the sky worker keeps the default priority.
pub(crate) fn lower_current_thread_priority() -> bool {
    false
}

/// Not available here: the boot timeline counts from the renderer plugin's build instead.
pub(crate) fn process_age() -> Option<std::time::Duration> {
    None
}

/// Runs `hook` when `main` returns or libc's `exit` runs (`atexit`), before the exit handlers
/// registered earlier (a loaded GPU driver's, run in reverse order). `false` if it could not be
/// registered. Without it a warm worker still compiling when `main` returns is not joined (the
/// `warm_quit_abort` test failed on macOS because of that, ADR-1625).
#[allow(unsafe_code)]
pub(crate) fn at_exit(hook: extern "C" fn()) -> bool {
    // SAFETY: `atexit` stores a plain function pointer; `hook` must not unwind, which every
    // caller guarantees by catching panics inside it.
    unsafe { libc::atexit(hook) == 0 }
}

/// Whether the renderer keeps its first Vulkan instance until the process ends (ADR-5794):
/// `false` here: the renderer runs on Metal here, which has no loader that unloads a driver.
pub(crate) fn keeps_vulkan_drivers_loaded() -> bool {
    false
}

/// `MTLDevice.currentAllocatedSize` of a Metal device: the bytes of every resource and heap the
/// device has allocated, driver-internal ones included (ADR-5860). `None` on another backend.
#[allow(unsafe_code)]
pub(crate) fn device_allocated_bytes(device: &wgpu::Device) -> Option<u64> {
    use objc2_metal::MTLDevice as _;
    // SAFETY: the hal device guard is only read: one `currentAllocatedSize` property query on the
    // `MTLDevice` wgpu keeps alive (no object is created or released), and the guard is dropped
    // before this function returns.
    unsafe {
        let hal = device.as_hal::<wgpu::hal::api::Metal>()?;
        u64::try_from(hal.raw_device().currentAllocatedSize()).ok()
    }
}
