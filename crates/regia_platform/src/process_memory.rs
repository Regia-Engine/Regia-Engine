// SPDX-License-Identifier: MPL-2.0
//! The process's resident memory, current and peak, from the OS: `/proc/self/status` (`VmRSS`,
//! `VmHWM`) on Linux and Android, `K32GetProcessMemoryInfo` (working set, peak working set) on
//! Windows, `proc_pid_rusage` (resident size) and `getrusage` (`ru_maxrss`) on macOS. Telemetry, the
//! agent's `memory.report` and the crash bundles read it through `regia_render::process_rss_bytes`;
//! before v0.2.15 Windows reported `null` there, so a Windows min-spec run could not check a RAM
//! budget from the engine's own numbers, and macOS reported `null` until ADR-5860.

/// Resident memory of this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessMemory {
    /// Bytes resident now (Linux `VmRSS`, Windows working set, macOS `ri_resident_size`).
    pub resident_bytes: u64,
    /// The most bytes resident at once since the process started (Linux `VmHWM`, Windows peak
    /// working set, macOS `ru_maxrss`).
    pub peak_resident_bytes: u64,
}

/// This process's resident memory; `None` where the OS gives no number (targets other than Linux,
/// Android, Windows and macOS) or the query failed.
#[must_use]
pub fn process_memory() -> Option<ProcessMemory> {
    imp::process_memory()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use super::ProcessMemory;

    pub(super) fn process_memory() -> Option<ProcessMemory> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let resident = status_kib(&status, "VmRSS:")?;
        // A kernel without `VmHWM` still has a current value; the peak is at least that.
        let peak = status_kib(&status, "VmHWM:").unwrap_or(resident).max(resident);
        Some(ProcessMemory {
            resident_bytes: resident * 1024,
            peak_resident_bytes: peak * 1024,
        })
    }

    /// The kibibyte value of the `/proc/<pid>/status` line starting with `key`.
    pub(super) fn status_kib(status: &str, key: &str) -> Option<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
    }
}

#[cfg(windows)]
mod imp {
    use super::ProcessMemory;
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn process_memory() -> Option<ProcessMemory> {
        // SAFETY: PROCESS_MEMORY_COUNTERS is plain data; all-zero is a valid value before `cb` is set.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        counters.cb = size;
        // SAFETY: the current-process pseudo handle is always valid, and `counters` is a writable
        // PROCESS_MEMORY_COUNTERS whose size is passed as `cb`, as the API requires.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
        (ok != 0).then(|| ProcessMemory {
            resident_bytes: counters.WorkingSetSize as u64,
            peak_resident_bytes: (counters.PeakWorkingSetSize as u64).max(counters.WorkingSetSize as u64),
        })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::ProcessMemory;

    pub(super) fn process_memory() -> Option<ProcessMemory> {
        // SAFETY: `rusage_info_v4` is plain data; all-zero is a valid value for the kernel to fill.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a writable `rusage_info_v4`, the struct the `RUSAGE_INFO_V4` flavor
        // writes; libc declares the buffer as `*mut rusage_info_t`, a pointer to the struct.
        let rc = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V4,
                (&raw mut info).cast::<libc::rusage_info_t>(),
            )
        };
        if rc != 0 || info.ri_resident_size == 0 {
            return None;
        }
        // SAFETY: `rusage` is plain data; all-zero is a valid value for `getrusage` to fill.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `usage` is a writable `rusage`; `RUSAGE_SELF` is always a valid target.
        let peak = if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } == 0 {
            // macOS reports `ru_maxrss` in bytes (Linux uses KiB).
            u64::try_from(usage.ru_maxrss).unwrap_or(0)
        } else {
            0
        };
        Some(ProcessMemory {
            resident_bytes: info.ri_resident_size,
            peak_resident_bytes: peak.max(info.ri_resident_size),
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
mod imp {
    use super::ProcessMemory;

    pub(super) fn process_memory() -> Option<ProcessMemory> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", windows))]
    #[test]
    fn the_os_reports_resident_memory_and_the_peak_is_never_below_it() {
        // Touch a few MiB so the working set is clearly above zero on every OS.
        let block = vec![1u8; 8 << 20];
        let mem = process_memory().expect("Linux, Android, Windows and macOS report resident memory");
        assert!(mem.resident_bytes > 0, "{mem:?}");
        assert!(mem.peak_resident_bytes >= mem.resident_bytes, "{mem:?}");
        assert!(
            mem.peak_resident_bytes >= 8 << 20,
            "an 8 MiB block was resident: {mem:?}"
        );
        assert_eq!(block.iter().map(|&b| u64::from(b)).sum::<u64>(), 8 << 20);
    }

    /// Other targets have no reader yet: they report no figure rather than a made-up one.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
    #[test]
    fn targets_without_a_reader_report_nothing() {
        assert_eq!(process_memory(), None);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn proc_status_lines_parse_in_kib() {
        let status = "Name:\tx\nVmHWM:\t  2048 kB\nVmRSS:\t  1024 kB\n";
        assert_eq!(imp::status_kib(status, "VmRSS:"), Some(1024));
        assert_eq!(imp::status_kib(status, "VmHWM:"), Some(2048));
        assert_eq!(imp::status_kib(status, "VmSwap:"), None);
    }
}
