// SPDX-License-Identifier: MPL-2.0
//! Linux raw HID: sysfs enumeration of `hidraw` nodes and blocking reads from `/dev/hidrawN`.
//! The sysfs layout (`uevent` with `HID_ID=bus:vendor:product` and `HID_NAME`, the binary
//! `report_descriptor`) is the kernel's documented hidraw/HID interface.

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;

use super::{HidDeviceInfo, HidError};

const SYSFS_HIDRAW: &str = "/sys/class/hidraw";

/// Parses `HID_ID=0003:0000256F:0000C635` and `HID_NAME=...` from a hidraw parent's `uevent`.
pub(super) fn parse_uevent(text: &str) -> Option<(u16, u16, String)> {
    let mut ids = None;
    let mut name = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("HID_ID=") {
            let mut parts = value.split(':');
            let _bus = parts.next()?;
            let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
            let product = u32::from_str_radix(parts.next()?, 16).ok()?;
            ids = Some((u16::try_from(vendor).ok()?, u16::try_from(product).ok()?));
        } else if let Some(value) = line.strip_prefix("HID_NAME=") {
            name = value.to_string();
        }
    }
    ids.map(|(vendor, product)| (vendor, product, name))
}

pub(super) fn enumerate(vendor_filter: &dyn Fn(u16) -> bool) -> Result<Vec<HidDeviceInfo>, HidError> {
    let entries = match std::fs::read_dir(SYSFS_HIDRAW) {
        Ok(entries) => entries,
        // No hidraw class (module not loaded, container): no devices, not an error.
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(HidError::Io(err.to_string())),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let node = entry.file_name().to_string_lossy().into_owned();
        let device = Path::new(SYSFS_HIDRAW).join(&node).join("device");
        let Ok(uevent) = std::fs::read_to_string(device.join("uevent")) else {
            continue;
        };
        let Some((vendor_id, product_id, product)) = parse_uevent(&uevent) else {
            continue;
        };
        if !vendor_filter(vendor_id) {
            continue;
        }
        out.push(HidDeviceInfo {
            path: format!("/dev/{node}"),
            vendor_id,
            product_id,
            product,
            usage: None,
            report_descriptor: std::fs::read(device.join("report_descriptor")).ok(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[derive(Debug)]
pub(super) struct Device {
    file: File,
    path: String,
}

fn map_error(err: &std::io::Error, path: &str) -> HidError {
    match (err.kind(), err.raw_os_error()) {
        (ErrorKind::PermissionDenied, _) => HidError::PermissionDenied(path.to_string()),
        // EBUSY
        (_, Some(16)) => HidError::Busy(path.to_string()),
        // ENODEV, ENXIO, EIO, ENOENT: the node went away with the device.
        (_, Some(19 | 6 | 5 | 2)) | (ErrorKind::NotFound, _) => HidError::Disconnected,
        _ => HidError::Io(format!("{path}: {err}")),
    }
}

impl Device {
    pub(super) fn open(path: &str) -> Result<Self, HidError> {
        File::open(path)
            .map(|file| Self {
                file,
                path: path.to_string(),
            })
            .map_err(|err| map_error(&err, path))
    }

    pub(super) fn read(&mut self, buf: &mut [u8]) -> Result<usize, HidError> {
        loop {
            match self.file.read(buf) {
                Ok(0) => return Err(HidError::Disconnected),
                Ok(n) => return Ok(n),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(map_error(&err, &self.path)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uevent_ids_and_name() {
        let text = "DRIVER=hid-generic\nHID_ID=0003:0000256F:0000C635\nHID_NAME=3Dconnexion SpaceMouse Compact\n\
                    HID_PHYS=usb-0000:00:14.0-2/input0\n";
        assert_eq!(
            parse_uevent(text),
            Some((0x256F, 0xC635, "3Dconnexion SpaceMouse Compact".to_string()))
        );
        assert_eq!(parse_uevent("HID_NAME=x\n"), None);
        assert_eq!(parse_uevent("HID_ID=0003:zz:0001\n"), None);
    }

    #[test]
    fn enumerate_never_fails_without_devices() {
        // On CI machines and containers this is usually empty; it must not error either way.
        assert!(enumerate(&|vendor| vendor == 0x256F).is_ok());
    }
}
