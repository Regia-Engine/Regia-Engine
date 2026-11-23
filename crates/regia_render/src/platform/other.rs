// SPDX-License-Identifier: MPL-2.0
//! Windows and the rest: no thread-priority control yet.

/// No-op here (`false`): the sky worker keeps the default priority. Windows would use
/// `SetThreadPriority(THREAD_PRIORITY_BELOW_NORMAL)` through `windows-sys`.
pub(crate) fn lower_current_thread_priority() -> bool {
    false
}

/// Not available here: the boot timeline counts from the renderer plugin's build instead.
pub(crate) fn process_age() -> Option<std::time::Duration> {
    None
}

/// Not registered here (`false`): no exit hook on this platform yet.
pub(crate) fn at_exit(_hook: extern "C" fn()) -> bool {
    false
}

/// Whether the renderer keeps its first Vulkan instance until the process ends (ADR-5794):
/// `false` here: no driver reload failure has been measured on this platform.
pub(crate) fn keeps_vulkan_drivers_loaded() -> bool {
    false
}

/// No Metal device on this platform (ADR-5860): the allocator report is the only GPU figure.
pub(crate) fn device_allocated_bytes(_device: &wgpu::Device) -> Option<u64> {
    None
}
