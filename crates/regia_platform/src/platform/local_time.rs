// SPDX-License-Identifier: MPL-2.0
//! The host's UTC offset on Unix (Linux, macOS, Android): `localtime_r`'s `tm_gmtoff` (ADR-8586).
//! Windows reads the registry in `windows.rs`.

/// The local UTC offset in seconds (east positive) at `unix_secs`, from the C library's time zone
/// rules (`TZ`, `/etc/localtime`); `None` when `localtime_r` refuses the instant.
pub(crate) fn local_utc_offset_seconds(unix_secs: i64) -> Option<i32> {
    let time = unix_secs as libc::time_t;
    // SAFETY: `tm` is plain data (integers and, on glibc, a `*const c_char` zone name); all-zero is
    // a valid value for `localtime_r` to overwrite.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `time` and `tm` are live locals for the whole call; `localtime_r` reads one `time_t`,
    // writes one `tm` and keeps neither pointer.
    let out = unsafe { libc::localtime_r(&raw const time, &raw mut tm) };
    if out.is_null() {
        return None;
    }
    i32::try_from(tm.tm_gmtoff).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_offset_is_within_a_day_and_whole_minutes() {
        for secs in [0, 1_794_880_193, 1_800_000_000] {
            let offset = super::local_utc_offset_seconds(secs).expect("localtime_r knows these instants");
            assert!(offset.abs() <= 14 * 3600, "{offset}");
            assert_eq!(offset % 60, 0, "{offset}");
        }
    }
}
