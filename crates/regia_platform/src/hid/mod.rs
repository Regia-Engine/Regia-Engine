// SPDX-License-Identifier: MPL-2.0
//! Minimal raw HID access for desktop input devices that the window system does not deliver
//! (the SpaceMouse, `docs/specs/29-spacemouse.md` § 3): enumerate HID interfaces, open one for
//! reading, and read input reports with a blocking call (meant for a reader thread).
//!
//! * **Linux**: `hidraw` through sysfs (`/sys/class/hidraw/*/device/uevent` and
//!   `report_descriptor`) and `/dev/hidrawN`; std only. Opening needs read permission on the
//!   node (a udev rule, see `book/user-guide/spacemouse.md`).
//! * **Windows**: SetupAPI enumeration of the HID interface class and `hid.dll`
//!   (`HidD_GetAttributes`, `HidP_GetCaps`), synchronous `ReadFile`, as documented on
//!   Microsoft Learn.
//! * Elsewhere: [`HidError::Unsupported`].
//!
//! Nothing here writes to a device (no LEDs, no feature reports): devices are only read.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// The UTF-16 string starting `byte_offset` bytes into `words` (an even offset), up to its NUL or
/// the end of the buffer. The Windows interface path sits in a `u32`-aligned buffer this way. Plain
/// code, so it also runs on the hosts that have no such buffer to read.
#[cfg(any(windows, test))]
fn utf16_in_words(words: &[u32], byte_offset: usize) -> String {
    let units: Vec<u16> = words
        .iter()
        .flat_map(|word| {
            let [a, b, c, d] = word.to_ne_bytes();
            [u16::from_ne_bytes([a, b]), u16::from_ne_bytes([c, d])]
        })
        .skip(byte_offset / 2)
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// One HID interface found by [`enumerate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidDeviceInfo {
    /// The OS path to pass to [`HidDevice::open`] (`/dev/hidrawN`, or a Windows interface path).
    pub path: String,
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// The product name the device reports (may be empty).
    pub product: String,
    /// The top-level collection's usage page and usage, where the OS reports them (Windows).
    pub usage: Option<(u16, u16)>,
    /// The device's HID report descriptor, where the OS exposes it (Linux).
    pub report_descriptor: Option<Vec<u8>>,
}

/// Why HID access failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HidError {
    /// The OS refused access (Linux: no udev rule for the hidraw node).
    #[error("permission denied opening {0}")]
    PermissionDenied(String),
    /// Another driver or process holds the device exclusively.
    #[error("{0} is in use by another driver or process")]
    Busy(String),
    /// The device was unplugged.
    #[error("device disconnected")]
    Disconnected,
    /// No HID backend on this platform.
    #[error("raw HID is not supported on this platform")]
    Unsupported,
    /// Any other OS error.
    #[error("HID I/O error: {0}")]
    Io(String),
}

/// Lists the present HID interfaces whose vendor id passes `vendor_filter` (cheap: product
/// strings, capabilities and descriptors are only read for those).
pub fn enumerate(vendor_filter: impl Fn(u16) -> bool) -> Result<Vec<HidDeviceInfo>, HidError> {
    #[cfg(target_os = "linux")]
    return linux::enumerate(&vendor_filter);
    #[cfg(windows)]
    return windows::enumerate(&vendor_filter);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = vendor_filter;
        Err(HidError::Unsupported)
    }
}

/// An open HID interface, read-only.
#[derive(Debug)]
pub struct HidDevice {
    #[cfg(target_os = "linux")]
    inner: linux::Device,
    #[cfg(windows)]
    inner: windows::Device,
}

impl HidDevice {
    /// Opens the interface at `path` (from [`HidDeviceInfo::path`]) for reading.
    pub fn open(path: &str) -> Result<Self, HidError> {
        #[cfg(target_os = "linux")]
        return Ok(Self {
            inner: linux::Device::open(path)?,
        });
        #[cfg(windows)]
        return Ok(Self {
            inner: windows::Device::open(path)?,
        });
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = path;
            Err(HidError::Unsupported)
        }
    }

    /// Blocks until the next input report and copies it into `buf` (report id first when the
    /// device numbers its reports); returns its length. [`HidError::Disconnected`] once the
    /// device is gone.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, HidError> {
        #[cfg(any(target_os = "linux", windows))]
        return self.inner.read(buf);
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = buf;
            Err(HidError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::utf16_in_words;

    /// `text` as UTF-16 in `u32` words after `pad_bytes` bytes of 0xAA, then 0xBB fill.
    fn words_with(pad_bytes: usize, text: &str, nul: bool) -> Vec<u32> {
        let mut bytes = vec![0xAAu8; pad_bytes];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_ne_bytes());
        }
        if nul {
            bytes.extend_from_slice(&[0, 0]);
        }
        bytes.resize(bytes.len().next_multiple_of(4), 0xBB);
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_ne_bytes(*word))
            .collect()
    }

    #[test]
    fn the_path_starts_at_the_offset_and_ends_at_its_nul() {
        // 4: `DevicePath` after `cbSize` in `SP_DEVICE_INTERFACE_DETAIL_DATA_W`.
        for path in [r"\\?\hid#vid_256f&pid_c635", "", "é∑𝄞", "a"] {
            assert_eq!(utf16_in_words(&words_with(4, path, true), 4), path);
        }
    }

    /// A buffer the driver filled without a terminator: the read stops at the buffer's end (the old
    /// code bounded its slice the same way, by hand, with raw pointers).
    #[test]
    fn a_path_with_no_nul_stops_at_the_buffer() {
        let words = words_with(4, "abcd", false);
        assert_eq!(utf16_in_words(&words, 4), "abcd");
    }

    #[test]
    fn an_offset_past_the_buffer_is_empty() {
        assert_eq!(utf16_in_words(&words_with(4, "abcd", true), 4096), "");
        assert_eq!(utf16_in_words(&[], 0), "");
    }
}
