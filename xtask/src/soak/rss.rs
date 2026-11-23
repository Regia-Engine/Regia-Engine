// SPDX-License-Identifier: MPL-2.0
//! Memory sampling for soaks: resident set size on Linux (`/proc/<pid>/status`), working set via
//! `tasklist` on Windows. GPU memory is not exposed by the agent protocol yet; reports carry
//! `null` for it until then.
//!
//! Both parsers are pure functions over captured output, so they are unit-tested on fixtures and
//! the platform-specific part is only "run the command / read the file".

/// Resident memory of `pid` in MiB, or `None` when the platform cannot tell (or the process is
/// already gone).
pub fn rss_mb(pid: u32) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        parse_vm_rss_kib(&status).map(kib_to_mb)
    }
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        parse_tasklist_kib(&text).map(kib_to_mb)
    }
    #[cfg(target_os = "macos")]
    {
        // `ps -o rss=` prints the resident set in KiB.
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()
            .map(kib_to_mb)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

fn kib_to_mb(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

/// Reads `VmRSS:` out of a `/proc/<pid>/status` body. Values there are in kiB.
#[cfg(target_os = "linux")]
fn parse_vm_rss_kib(status: &str) -> Option<u64> {
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("VmRSS:") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let kib = fields.next()?;
        return kib.parse::<u64>().ok();
    }
    None
}

/// Reads the memory column of `tasklist /FO CSV /NH` output for one PID. The last field looks like
/// `"123,456 K"` (thousands separators and the unit vary by locale), so everything but digits and
/// the number itself is ignored. The field is quoted because the separator may be a comma: it is
/// taken between the last two quotes, not after the last comma.
#[cfg(target_os = "windows")]
fn parse_tasklist_kib(tasklist_output: &str) -> Option<u64> {
    let line = tasklist_output.lines().find(|l| !l.trim().is_empty())?.trim_end();
    let last = match line.strip_suffix('"') {
        Some(body) => &body[body.rfind('"')? + 1..],
        None => line.rsplit(',').next()?,
    };
    let digits: String = last.chars().filter(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok()
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn reads_vm_rss_line() {
        let status = "\
Name:\tsim_bridge\n\
Umask:\t0022\n\
State:\tS (sleeping)\n\
VmPeak:\t 2465408 kB\n\
VmRSS:\t  842132 kB\n\
VmData:\t 912344 kB\n";
        assert_eq!(parse_vm_rss_kib(status), Some(842_132));
    }

    #[test]
    fn missing_rss_line_is_none() {
        assert_eq!(parse_vm_rss_kib("Name:\tx\n"), None);
        assert_eq!(parse_vm_rss_kib(""), None);
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    use super::*;

    #[test]
    fn reads_csv_memory_column_with_thousands_separator() {
        let out = "\"sim_bridge.exe\",\"12345\",\"Console\",\"1\",\"1,234,567 K\"\r\n";
        assert_eq!(parse_tasklist_kib(out), Some(1_234_567));
    }

    #[test]
    fn reads_plain_number_without_separator() {
        let out = "\"sim_bridge.exe\",\"12345\",\"Console\",\"1\",\"842132 K\"\r\n";
        assert_eq!(parse_tasklist_kib(out), Some(842_132));
    }

    #[test]
    fn reads_a_dot_separated_memory_column() {
        let out = "\"sim_bridge.exe\",\"12345\",\"Console\",\"1\",\"1.234.567 K\"\r\n";
        assert_eq!(parse_tasklist_kib(out), Some(1_234_567));
    }

    #[test]
    fn no_matching_task_is_none() {
        assert_eq!(
            parse_tasklist_kib("INFO: No tasks are running which match the specified criteria.\r\n"),
            None
        );
        assert_eq!(parse_tasklist_kib(""), None);
    }
}
