// SPDX-License-Identifier: MPL-2.0
//! Background-work helpers: run a worker thread only on what the game leaves over.

/// Lowers the calling thread's priority for background work (a pak integrity check) so it
/// neither takes the frame thread's core nor competes much with asset streaming for the disk.
///
/// - Linux: nice 19 and the idle IO class (per thread). The IO class only matters under the BFQ
///   scheduler; NVMe's default (`none`) and mq-deadline ignore it, so callers still throttle.
/// - Android: nice 19.
/// - Windows: `THREAD_MODE_BACKGROUND_BEGIN` (low CPU, IO and memory priority).
///
/// Best effort: returns `false` when nothing could be lowered (other platforms).
#[must_use]
pub fn lower_current_thread_priority() -> bool {
    imp::lower()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    pub(super) fn lower() -> bool {
        // SAFETY: `setpriority` takes plain integers and touches no memory of ours; `who = 0`
        // with `PRIO_PROCESS` names the calling thread on Linux.
        let cpu = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) == 0 };
        cpu | lower_io()
    }

    #[cfg(target_os = "linux")]
    fn lower_io() -> bool {
        // `ioprio_set(IOPRIO_WHO_PROCESS, 0 = this thread, IOPRIO_CLASS_IDLE << 13)`. Honoured by
        // the BFQ scheduler; a no-op (but harmless) under mq-deadline / none.
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
        // SAFETY: a raw syscall with three integer arguments; it reads and writes no memory.
        unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS,
                0 as libc::c_long,
                IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT,
            ) == 0
        }
    }

    #[cfg(target_os = "android")]
    fn lower_io() -> bool {
        false
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };

    pub(super) fn lower() -> bool {
        // SAFETY: `GetCurrentThread` returns a pseudo-handle for the calling thread (never
        // closed); background mode lowers this thread's CPU, IO and memory priority.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) != 0 }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
mod imp {
    pub(super) fn lower() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lowering_a_scratch_thread_does_not_fail() {
        let lowered = std::thread::spawn(super::lower_current_thread_priority)
            .join()
            .expect("thread");
        #[cfg(target_os = "linux")]
        assert!(lowered, "nice 19 is always allowed for an unprivileged thread");
        let _ = lowered;
    }
}
