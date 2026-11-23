// SPDX-License-Identifier: MPL-2.0
//! Host power supply: a battery, AC, or an unknown source (ADR-3997).
//!
//! Linux reads a directory shaped like the kernel `power_supply` class
//! (`Documentation/power/power_supply_class.rst`, attribute names and strings from
//! `Documentation/ABI/testing/sysfs-class-power`). Android reads the sticky
//! `ACTION_BATTERY_CHANGED` extras. Tests pass a temporary directory and do not open
//! `/sys`. The host read is [`power_state`].

use std::io::Read;
use std::path::Path;

use super::ServiceError;

/// A battery percent below this value sets [`PowerState::low_power`].
///
/// The comparison is `percent < LOW_POWER_BELOW_PERCENT` (20). AC, an unknown source,
/// and a missing percent never set `low_power`, including when a capacity file is present
/// and below this value.
pub const LOW_POWER_BELOW_PERCENT: u8 = 20;

/// Linux sysfs class directory (`/sys/class/power_supply`).
///
/// [`power_state`] reads it on Linux. Other hosts do not. [`read_power_supply_dir`] reads
/// whatever directory the caller passes, on every host.
pub const LINUX_POWER_SUPPLY_CLASS: &str = "/sys/class/power_supply";

/// Where the machine is drawing power.
///
/// The set is closed: `Unknown`, `Battery`, and `Ac`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerSource {
    /// The platform did not report a supply, or the `type` text is not one this crate maps.
    #[default]
    Unknown,
    /// A battery or a UPS (`type` text `Battery` or `UPS`).
    Battery,
    /// Mains, USB, or wireless power (`type` text `Mains`, `USB`, or `Wireless`).
    Ac,
}

/// One reading of the host power supply.
///
/// `percent` is `None` when the platform does not report one. `Some` is only `0..=100`.
/// A value outside that range, a non-numeric value, or a non-UTF-8 file becomes `None`
/// and does not panic. `low_power` is `source == Battery` and `percent` is `Some` below
/// [`LOW_POWER_BELOW_PERCENT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerState {
    /// Battery, AC, or unknown.
    pub source: PowerSource,
    /// Charge in percent, when the platform reported one inside `0..=100`.
    pub percent: Option<u8>,
    /// `true` when a battery reports a percent below [`LOW_POWER_BELOW_PERCENT`].
    pub low_power: bool,
}

impl Default for PowerState {
    fn default() -> Self {
        Self::unknown()
    }
}

impl PowerState {
    /// No supply, no percent, not low power.
    pub fn unknown() -> Self {
        Self {
            source: PowerSource::Unknown,
            percent: None,
            low_power: false,
        }
    }
}

/// Reads one sysfs-shaped directory, or a class directory of supply subdirectories.
///
/// `dir` itself must be a directory. When it cannot be opened, or it is not a directory,
/// this returns [`ServiceError::Os`] with operation `power_state`. The agent maps that
/// error to `power_unavailable`. A directory that opens with missing attribute files
/// returns [`PowerState::unknown`] and is not an error.
///
/// A directory that contains a `type` file is one supply. Otherwise it is a class
/// directory: each child whose metadata says it is a directory (symlinks are followed,
/// as in `/sys/class/power_supply`) is read. A battery supply is kept over an AC supply.
/// Among supplies of the same kind, the lowest directory name is kept. A child that
/// cannot be read is skipped.
///
/// `type` is case-sensitive after trimming. `Battery` and `UPS` are [`PowerSource::Battery`].
/// `Mains`, `USB`, and `Wireless` are [`PowerSource::Ac`]. Any other text is
/// [`PowerSource::Unknown`]. `capacity` is a percent only when the trimmed text is ASCII
/// digits and the integer is `0..=100`. `status` (`Unknown`, `Charging`, `Discharging`,
/// `Not charging`, `Full`) is read and does not change the three fields.
///
/// Each attribute is at most 64 bytes. A longer file, a non-UTF-8 file, or a missing file
/// is ignored.
///
/// # Errors
///
/// [`ServiceError::Os`] when `dir` cannot be opened or is not a directory.
pub fn read_power_supply_dir(dir: &Path) -> Result<PowerState, ServiceError> {
    let meta = std::fs::metadata(dir).map_err(os_error)?;
    if !meta.is_dir() {
        return Err(ServiceError::Os {
            operation: "power_state",
            message: format!("{} is not a directory", dir.display()),
        });
    }
    let entries = std::fs::read_dir(dir).map_err(os_error)?;
    if dir.join("type").is_file() {
        return Ok(parse_supply(dir));
    }
    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(child) = std::fs::metadata(&path) else {
            continue;
        };
        if child.is_dir() {
            names.push(entry.file_name());
        }
    }
    names.sort_unstable();
    let mut chosen: Option<PowerState> = None;
    for name in names {
        let next = parse_supply(&dir.join(name));
        chosen = Some(match chosen {
            None => next,
            Some(current) if rank(next.source) > rank(current.source) => next,
            Some(current) => current,
        });
    }
    Ok(chosen.unwrap_or_else(PowerState::unknown))
}

/// The host supply.
///
/// Linux reads [`LINUX_POWER_SUPPLY_CLASS`]. Android reads the sticky battery intent and,
/// when that JNI call fails, logs the error and returns [`PowerState::unknown`] (that is
/// not `power_unavailable`). Windows, macOS, and every other host return
/// [`PowerState::unknown`] without reading a device.
///
/// # Errors
///
/// On Linux, [`ServiceError::Os`] when the class directory cannot be opened. Other hosts
/// do not return an error from this function.
pub fn power_state() -> Result<PowerState, ServiceError> {
    #[cfg(target_os = "linux")]
    {
        read_power_supply_dir(Path::new(LINUX_POWER_SUPPLY_CLASS))
    }
    #[cfg(target_os = "android")]
    {
        match crate::platform::battery_power() {
            Ok(state) => Ok(state),
            Err(err) => {
                tracing::warn!(target: "regia_platform", "power_state: {err}");
                Ok(PowerState::unknown())
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Ok(PowerState::unknown())
    }
}

/// `level * 100 / scale` when that integer is `0..=100`.
///
/// A negative level, a scale of zero or less, or a result outside `0..=100` is `None`.
/// The division is truncating and does not panic, including on the largest `i32` inputs.
#[cfg(any(test, target_os = "android"))]
pub(crate) fn percent_from_level_scale(level: i32, scale: i32) -> Option<u8> {
    if level < 0 || scale <= 0 {
        return None;
    }
    let percent = (i64::from(level) * 100) / i64::from(scale);
    u8::try_from(percent).ok().filter(|p| *p <= 100)
}

/// Maps Android `BatteryManager` extras to a [`PowerState`].
///
/// A present battery stays [`PowerSource::Battery`] even when `plugged` is non-zero.
/// Percent comes from [`percent_from_level_scale`]. Absent and plugged (`plugged != 0`,
/// which covers AC, USB, wireless, and dock) is [`PowerSource::Ac`] with no percent.
/// Absent and not plugged is [`PowerState::unknown`]. `low_power` uses the same percent
/// rule as sysfs. The status extra is not an argument: it does not change the fields.
#[cfg(any(test, target_os = "android"))]
pub(crate) fn from_android_battery(present: bool, plugged: i32, level: i32, scale: i32) -> PowerState {
    if present {
        return assemble(PowerSource::Battery, percent_from_level_scale(level, scale));
    }
    if plugged != 0 {
        return assemble(PowerSource::Ac, None);
    }
    PowerState::unknown()
}

fn os_error(err: std::io::Error) -> ServiceError {
    ServiceError::Os {
        operation: "power_state",
        message: err.to_string(),
    }
}

fn rank(source: PowerSource) -> u8 {
    match source {
        PowerSource::Unknown => 0,
        PowerSource::Ac => 1,
        PowerSource::Battery => 2,
    }
}

fn assemble(source: PowerSource, percent: Option<u8>) -> PowerState {
    let low_power = source == PowerSource::Battery && percent.is_some_and(|p| p < LOW_POWER_BELOW_PERCENT);
    PowerState {
        source,
        percent,
        low_power,
    }
}

const MAX_ATTR: u16 = 64;

/// Reads one attribute file. Anything but a regular file (symlinks followed, as sysfs links) reads
/// as missing: opening a FIFO blocks until a writer comes, and a device may never end.
fn read_attribute(path: &Path) -> Option<String> {
    read_attribute_with(path, || {})
}

/// [`read_attribute`] with a hook run between the path check and the open (tests swap the file
/// there). The path check skips a FIFO or device before it is opened at all; the open does not
/// block (`O_NONBLOCK` where the host has it) and the opened handle is checked again, so a FIFO
/// swapped in after the path check reads as missing too (ADR-5869, w7-runtime-smalls-z item 2).
fn read_attribute_with(path: &Path, before_open: impl FnOnce()) -> Option<String> {
    // Regular files only: opening a FIFO planted in a test directory would block the caller
    // (ADR-5869). sysfs attributes are regular files.
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    before_open();
    let file = open_attribute(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut limited = file.take(u64::from(MAX_ATTR) + 1);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).ok()?;
    if buf.len() > usize::from(MAX_ATTR) {
        return None;
    }
    let text = std::str::from_utf8(&buf).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Opens an attribute for reading without waiting for a FIFO writer. On a regular file
/// `O_NONBLOCK` changes nothing.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn open_attribute(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// Opens an attribute for reading (no FIFOs to wait on here; the handle is still checked).
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn open_attribute(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn parse_percent(text: &str) -> Option<u8> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: u32 = text.parse().ok()?;
    u8::try_from(value).ok().filter(|p| *p <= 100)
}

fn source_from_type(text: &str) -> PowerSource {
    match text {
        "Battery" | "UPS" => PowerSource::Battery,
        "Mains" | "USB" | "Wireless" => PowerSource::Ac,
        _ => PowerSource::Unknown,
    }
}

fn parse_supply(dir: &Path) -> PowerState {
    // A peripheral's battery (a wireless mouse, a gamepad: `scope` is `Device` in the sysfs
    // power-supply ABI) and an empty battery slot (`present` is `0`) are not the host's supply.
    // Without this a desktop with a mouse at 15 % read as a battery machine in low-power mode.
    if read_attribute(&dir.join("scope")).is_some_and(|scope| scope == "Device")
        || read_attribute(&dir.join("present")).is_some_and(|present| present == "0")
    {
        return PowerState::unknown();
    }
    let source =
        read_attribute(&dir.join("type")).map_or(PowerSource::Unknown, |text| source_from_type(&text));
    let percent = read_attribute(&dir.join("capacity")).and_then(|text| parse_percent(&text));
    // `status` is part of the sysfs shape. It does not change the three fields.
    let _status = read_attribute(&dir.join("status"));
    assemble(source, percent)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use regia_test_utils::{unique_temp_dir, write_atomic};

    use super::*;

    fn scratch() -> PathBuf {
        unique_temp_dir("power-state")
    }

    fn finish(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn write_supply(dir: &std::path::Path, kind: &str, capacity: Option<&[u8]>, status: Option<&[u8]>) {
        std::fs::create_dir_all(dir).unwrap_or_else(|err| panic!("create {}: {err}", dir.display()));
        write_atomic(&dir.join("type"), kind.as_bytes()).unwrap_or_else(|err| panic!("type: {err}"));
        if let Some(bytes) = capacity {
            write_atomic(&dir.join("capacity"), bytes).unwrap_or_else(|err| panic!("capacity: {err}"));
        }
        if let Some(bytes) = status {
            write_atomic(&dir.join("status"), bytes).unwrap_or_else(|err| panic!("status: {err}"));
        }
    }

    fn read_ok(dir: &std::path::Path) -> PowerState {
        read_power_supply_dir(dir).unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
    }

    /// A wireless mouse's battery (`scope` `Device`) is not the host supply: a desktop with only
    /// that must read unknown, not a battery at 10 % in low-power mode, and a laptop keeps its
    /// own battery even when the peripheral's directory name sorts first. An empty battery slot
    /// (`present` `0`) does not win over the real battery either.
    #[test]
    fn peripheral_and_absent_batteries_are_not_the_host_supply() {
        let root = scratch();
        let mouse = root.join("hidpp_battery_0");
        write_supply(&mouse, "Battery", Some(b"10\n"), Some(b"Discharging\n"));
        write_atomic(&mouse.join("scope"), b"Device\n").unwrap_or_else(|err| panic!("scope: {err}"));
        assert_eq!(read_ok(&root), PowerState::unknown(), "a mouse battery alone");

        let host = root.join("macsmc-battery");
        write_supply(&host, "Battery", Some(b"80\n"), Some(b"Discharging\n"));
        write_atomic(&host.join("scope"), b"System\n").unwrap_or_else(|err| panic!("scope: {err}"));
        let state = read_ok(&root);
        assert_eq!(state.source, PowerSource::Battery);
        assert_eq!(state.percent, Some(80), "the host battery, not the mouse");
        assert!(!state.low_power);

        let empty = root.join("BAT0");
        write_supply(&empty, "Battery", None, None);
        write_atomic(&empty.join("present"), b"0\n").unwrap_or_else(|err| panic!("present: {err}"));
        assert_eq!(
            read_ok(&root).percent,
            Some(80),
            "an empty slot sorts first and must not win"
        );
        finish(&root);
    }

    #[test]
    fn battery_at_15_discharging_is_low_power() {
        let root = scratch();
        let supply = root.join("BAT0");
        write_supply(&supply, "Battery", Some(b"15\n"), Some(b"Discharging\n"));
        let state = read_ok(&supply);
        assert_eq!(state.source, PowerSource::Battery);
        assert_eq!(state.percent, Some(15));
        assert!(state.low_power);
        finish(&root);
    }

    #[test]
    fn mains_is_ac_and_not_low_power() {
        let root = scratch();
        let supply = root.join("AC");
        write_supply(&supply, "Mains", None, Some(b"Charging\n"));
        let state = read_ok(&supply);
        assert_eq!(state.source, PowerSource::Ac);
        assert_eq!(state.percent, None);
        assert!(!state.low_power);
        finish(&root);
    }

    #[test]
    fn mains_with_a_low_capacity_stays_not_low_power() {
        let root = scratch();
        let supply = root.join("AC");
        write_supply(&supply, "Mains", Some(b"10\n"), Some(b"Charging\n"));
        let state = read_ok(&supply);
        assert_eq!(
            state,
            PowerState {
                source: PowerSource::Ac,
                percent: Some(10),
                low_power: false,
            }
        );
        finish(&root);
    }

    #[test]
    fn capacity_150_is_not_a_percent() {
        let root = scratch();
        let supply = root.join("BAT0");
        write_supply(&supply, "Battery", Some(b"150\n"), Some(b"Discharging\n"));
        let state = read_ok(&supply);
        assert_eq!(state.source, PowerSource::Battery);
        assert_eq!(state.percent, None);
        assert!(!state.low_power);
        finish(&root);
    }

    #[test]
    fn missing_directory_is_an_error() {
        let root = scratch();
        let missing = root.join("never-created");
        let err = read_power_supply_dir(&missing);
        assert!(
            err.is_err(),
            "a missing directory is power_unavailable at the agent"
        );
        finish(&root);
    }

    #[test]
    fn a_file_is_not_a_supply_directory() {
        let root = scratch();
        let file = root.join("not-a-dir");
        write_atomic(&file, b"Battery").unwrap_or_else(|err| panic!("write: {err}"));
        assert!(read_power_supply_dir(&file).is_err());
        finish(&root);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_supply_directory_is_unavailable_even_with_readable_attributes() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch();
        let supply = root.join("BAT0");
        write_supply(&supply, "Battery", Some(b"15\n"), Some(b"Discharging\n"));
        std::fs::set_permissions(&supply, std::fs::Permissions::from_mode(0o111))
            .unwrap_or_else(|err| panic!("restrict directory: {err}"));
        let directory_unavailable = std::fs::read_dir(&supply).is_err();
        let result = read_power_supply_dir(&supply);
        std::fs::set_permissions(&supply, std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|err| panic!("restore directory: {err}"));
        finish(&root);
        // Privileged test runners may bypass directory permissions.
        if directory_unavailable {
            assert!(result.is_err(), "an unopenable directory is power_unavailable");
        }
    }

    #[test]
    fn empty_directory_is_unknown() {
        let root = scratch();
        assert_eq!(read_ok(&root), PowerState::unknown());
        finish(&root);
    }

    #[test]
    fn low_power_threshold_is_below_20() {
        let root = scratch();
        for (capacity, low) in [("0\n", true), ("19\n", true), ("20\n", false), ("100\n", false)] {
            let supply = root.join(capacity.trim());
            write_supply(&supply, "Battery", Some(capacity.as_bytes()), Some(b"Full\n"));
            let state = read_ok(&supply);
            assert_eq!(state.percent, Some(capacity.trim().parse().unwrap_or(255)));
            assert_eq!(state.low_power, low, "capacity {capacity}");
        }
        finish(&root);
    }

    #[test]
    fn bad_capacity_is_none_and_does_not_panic() {
        let root = scratch();
        let cases: &[(&str, &[u8])] = &[
            ("text", b"nope\n"),
            ("neg", b"-1\n"),
            ("utf8", &[0xff, 0xfe]),
            ("blank", b"\n"),
            ("plus", b"+15\n"),
        ];
        for (name, bytes) in cases {
            let supply = root.join(name);
            write_supply(&supply, "Battery", Some(bytes), Some(b"Discharging\n"));
            let state = read_ok(&supply);
            assert_eq!(state.source, PowerSource::Battery);
            assert_eq!(state.percent, None, "{name}");
            assert!(!state.low_power, "{name}");
        }
        finish(&root);
    }

    #[test]
    fn charging_does_not_clear_low_power() {
        let root = scratch();
        let supply = root.join("BAT0");
        write_supply(&supply, "Battery", Some(b"15\n"), Some(b"Charging\n"));
        let state = read_ok(&supply);
        assert!(state.low_power);
        assert_eq!(state.percent, Some(15));
        finish(&root);
    }

    #[test]
    fn type_strings_follow_the_sysfs_abi() {
        let root = scratch();
        let cases = [
            ("Battery", PowerSource::Battery),
            ("UPS", PowerSource::Battery),
            ("Mains", PowerSource::Ac),
            ("USB", PowerSource::Ac),
            ("Wireless", PowerSource::Ac),
            ("USB_DCP", PowerSource::Unknown),
            ("battery", PowerSource::Unknown),
        ];
        for (kind, source) in cases {
            let supply = root.join(kind);
            write_supply(&supply, kind, None, None);
            assert_eq!(read_ok(&supply).source, source, "{kind}");
        }
        finish(&root);
    }

    #[test]
    fn a_class_directory_prefers_a_battery() {
        let root = scratch();
        let class = root.join("class");
        write_supply(&class.join("AC"), "Mains", Some(b"10\n"), Some(b"Charging\n"));
        write_supply(
            &class.join("BAT0"),
            "Battery",
            Some(b"15\n"),
            Some(b"Discharging\n"),
        );
        let state = read_ok(&class);
        assert_eq!(state.source, PowerSource::Battery);
        assert_eq!(state.percent, Some(15));
        assert!(state.low_power);
        finish(&root);
    }

    #[test]
    fn the_lowest_battery_name_wins() {
        let root = scratch();
        let class = root.join("class");
        write_supply(&class.join("BAT1"), "Battery", Some(b"80\n"), None);
        write_supply(&class.join("BAT0"), "Battery", Some(b"15\n"), None);
        let state = read_ok(&class);
        assert_eq!(state.percent, Some(15));
        finish(&root);
    }

    #[cfg(unix)]
    #[test]
    fn class_symlinks_are_followed() {
        let root = scratch();
        let cells = root.join("cells");
        let class = root.join("class");
        write_supply(
            &cells.join("BAT0"),
            "Battery",
            Some(b"15\n"),
            Some(b"Discharging\n"),
        );
        std::fs::create_dir_all(&class).unwrap_or_else(|err| panic!("class: {err}"));
        std::os::unix::fs::symlink(cells.join("BAT0"), class.join("BAT0"))
            .unwrap_or_else(|err| panic!("symlink: {err}"));
        write_supply(&class.join("AC"), "Mains", None, None);
        let state = read_ok(&class);
        assert_eq!(state.source, PowerSource::Battery);
        assert_eq!(state.percent, Some(15));
        assert!(state.low_power);
        finish(&root);
    }

    #[test]
    fn android_extras_map_without_jni() {
        let low = from_android_battery(true, 1, 15, 100);
        assert_eq!(low.source, PowerSource::Battery);
        assert_eq!(low.percent, Some(15));
        assert!(low.low_power);

        let charging = from_android_battery(true, 1, 50, 100);
        assert_eq!(charging.source, PowerSource::Battery);
        assert_eq!(charging.percent, Some(50));
        assert!(!charging.low_power);

        let ac = from_android_battery(false, 1, 15, 100);
        assert_eq!(ac, assemble(PowerSource::Ac, None));

        assert_eq!(from_android_battery(false, 0, 0, 100), PowerState::unknown());
        assert_eq!(percent_from_level_scale(15, 0), None);
        assert_eq!(percent_from_level_scale(-1, 100), None);
        assert_eq!(percent_from_level_scale(150, 100), None);
        assert_eq!(percent_from_level_scale(1, 2), Some(50));
        assert_eq!(percent_from_level_scale(i32::MAX, 1), None);
        assert!(from_android_battery(true, 0, 0, 100).low_power);
    }

    /// Audit corr-agent finding 16 (ADR-5895): an attribute that is a FIFO used to be opened, and
    /// a blocking open of a FIFO waits for a writer forever (the agent's world thread with it). It
    /// now reads as a missing attribute. No timeout decides the result: a non-blocking writer open
    /// succeeds only while a reader has the FIFO open, so either the read finishes or the test sees
    /// it holding the FIFO, writes `50` to release it and fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_attribute_reads_as_missing_without_blocking() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let root = scratch();
        let supply = root.join("BAT0");
        write_supply(&supply, "Battery", None, Some(b"Discharging\n"));
        let fifo = supply.join("capacity");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success());
        assert_eq!(made.ok(), Some(true), "mkfifo");
        let (sender, receiver) = std::sync::mpsc::channel();
        let dir = supply.clone();
        let reader = std::thread::spawn(move || {
            let _ = sender.send(read_power_supply_dir(&dir));
        });
        let finished = loop {
            if let Ok(result) = receiver.try_recv() {
                break Some(result);
            }
            if let Ok(mut writer) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                let _ = writer.write_all(b"50\n");
                drop(writer);
                break None;
            }
            std::thread::yield_now();
        };
        let _ = reader.join();
        finish(&root);
        let state = finished
            .unwrap_or_else(|| panic!("the read opened the FIFO `capacity` and blocked until a writer came"))
            .unwrap_or_else(|err| panic!("read: {err}"));
        assert_eq!(state.source, PowerSource::Battery);
        assert_eq!(state.percent, None, "a FIFO is not a capacity");
    }

    /// w7-runtime-smalls-z item 2: the path is a regular file when it is checked and a FIFO when
    /// it is opened (swapped in by the hook between the two). The open must not block and the
    /// handle must be checked, so the read is `None`. No timeout decides the result: if a
    /// non-blocking writer open succeeds, some reader has the FIFO open, so the test writes `50`
    /// (which releases a blocked open) and lets the result decide; a blocking open then reads
    /// `50`, which fails the assertion.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_swapped_in_after_the_check_reads_as_missing_without_blocking() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let root = scratch();
        std::fs::create_dir_all(&root).unwrap_or_else(|err| panic!("create: {err}"));
        let path = root.join("capacity");
        write_atomic(&path, b"80\n").unwrap_or_else(|err| panic!("capacity: {err}"));
        let (swapped_tx, swapped_rx) = std::sync::mpsc::channel();
        let (sender, receiver) = std::sync::mpsc::channel();
        let target = path.clone();
        let reader = std::thread::spawn(move || {
            let swap = target.clone();
            let result = read_attribute_with(&target, move || {
                let _ = std::fs::remove_file(&swap);
                let made = std::process::Command::new("mkfifo")
                    .arg(&swap)
                    .status()
                    .map(|s| s.success());
                let _ = swapped_tx.send(made.ok() == Some(true));
            });
            let _ = sender.send(result);
        });
        let swapped = swapped_rx.recv().unwrap_or(false);
        let result = loop {
            if let Ok(result) = receiver.try_recv() {
                break result;
            }
            if let Ok(mut writer) = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                let _ = writer.write_all(b"50\n");
                drop(writer);
                break receiver.recv().unwrap_or(None);
            }
            std::thread::yield_now();
        };
        let _ = reader.join();
        finish(&root);
        assert!(swapped, "mkfifo");
        assert_eq!(
            result, None,
            "the read opened the FIFO swapped in after the check and read what a writer sent"
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    #[test]
    fn other_hosts_report_unknown() {
        assert_eq!(
            power_state().unwrap_or_else(|err| panic!("{err}")),
            PowerState::unknown()
        );
    }
}
