// SPDX-License-Identifier: MPL-2.0
//! Linux and Android.

/// Drops the calling thread to the lowest normal priority (nice 19), so a background bake only
/// uses cycles the frame thread leaves: on a busy machine it no longer takes the render thread's
/// core (`specs/27` § 2.4). Linux nice values are per thread. Best effort: `false` on failure.
#[allow(unsafe_code)]
pub(crate) fn lower_current_thread_priority() -> bool {
    // SAFETY: `setpriority` takes plain integers and touches no memory of ours; `who = 0` with
    // `PRIO_PROCESS` names the calling thread on Linux.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) == 0 }
}

/// Time since the process was created (`/proc/self/stat` start time against `CLOCK_BOOTTIME`),
/// so the boot timeline counts the dynamic loader and everything before `main`. Resolution is
/// one clock tick (10 ms at the usual `CLK_TCK` of 100).
#[allow(unsafe_code)]
pub(crate) fn process_age() -> Option<std::time::Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised command name start at field 3 (state); start time is 22.
    let rest = stat.rsplit_once(')')?.1;
    let start_ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: `sysconf` reads a constant; `clock_gettime` writes only the timespec we own.
    let (hz, now) = unsafe {
        let hz = libc::sysconf(libc::_SC_CLK_TCK);
        let mut ts = std::mem::zeroed::<libc::timespec>();
        if libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) != 0 {
            return None;
        }
        (hz, ts)
    };
    if hz <= 0 {
        return None;
    }
    let now = std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
    let start = std::time::Duration::from_secs_f64(start_ticks as f64 / hz as f64);
    now.checked_sub(start)
}

/// Runs `hook` when the process exits (`atexit`), before the exit handlers registered earlier
/// (a loaded GPU driver's). `false` if it could not be registered.
#[allow(unsafe_code)]
pub(crate) fn at_exit(hook: extern "C" fn()) -> bool {
    // SAFETY: `atexit` stores a plain function pointer; `hook` must not unwind, which every
    // caller guarantees by catching panics inside it.
    unsafe { libc::atexit(hook) == 0 }
}

/// [`at_exit`] that also hands `hook` the status the process is exiting with (glibc's `on_exit`,
/// which the `libc` crate does not declare). `false` if it could not be registered.
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
pub(crate) fn at_exit_with_status(hook: extern "C" fn(std::ffi::c_int, *mut std::ffi::c_void)) -> bool {
    // SAFETY: the signature of glibc's `on_exit` (`on_exit(3)`); this item is built for glibc only.
    unsafe extern "C" {
        fn on_exit(
            function: extern "C" fn(std::ffi::c_int, *mut std::ffi::c_void),
            arg: *mut std::ffi::c_void,
        ) -> std::ffi::c_int;
    }
    // SAFETY: `on_exit` stores a plain function pointer and an argument it only passes back;
    // `hook` must not unwind, as for `atexit`.
    unsafe { on_exit(hook, std::ptr::null_mut()) == 0 }
}

/// Ends the process with `code` at once: every thread stops, and no exit handler or destructor
/// runs.
#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
#[allow(unsafe_code)]
pub(crate) fn exit_now(code: std::ffi::c_int) -> ! {
    // SAFETY: `_exit` takes a plain integer and never returns.
    unsafe { libc::_exit(code) }
}

/// Whether the renderer keeps its first Vulkan instance until the process ends, so the Vulkan
/// loader never unloads the GPU drivers (ADR-5794): `true` on Linux, `false` on Android.
///
/// The desktop loader loads every installed driver library for each new instance and unloads it
/// with the last one. glibc gives a library loaded after start-up its static thread-local
/// storage out of a fixed surplus, and each load and unload of NVIDIA's `libnvidia-tls.so` leaves
/// 16 bytes of it used (measured), so that driver can be loaded about a hundred times per
/// process. Android has its own loader and C library; nothing was measured there, so it is left
/// as it was.
pub(crate) fn keeps_vulkan_drivers_loaded() -> bool {
    cfg!(target_os = "linux")
}

/// No Metal device on this platform (ADR-5860): the allocator report is the only GPU figure.
pub(crate) fn device_allocated_bytes(_device: &wgpu::Device) -> Option<u64> {
    None
}
