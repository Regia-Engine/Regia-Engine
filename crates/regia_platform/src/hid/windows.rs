// SPDX-License-Identifier: MPL-2.0
//! Windows raw HID: SetupAPI enumeration of the HID device-interface class, `hid.dll` for
//! attributes and top-level usage, synchronous `ReadFile` for input reports. All calls and
//! structures are the documented Win32 API (Microsoft Learn: "Finding and Opening a HID
//! Collection", "Obtaining HID Reports"), through `windows-sys`.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData, HidD_GetAttributes,
    HidD_GetHidGuid, HidD_GetPreparsedData, HidD_GetProductString, HidP_GetCaps,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_DEVICE_NOT_CONNECTED, ERROR_SHARING_VIOLATION, GENERIC_READ,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::core::GUID;

use super::{HidDeviceInfo, HidError};

/// An owned file handle, closed on drop.
#[derive(Debug)]
struct Handle(HANDLE);

// SAFETY: a Win32 file handle is a process-wide kernel object reference; it may be used and
// closed from any thread. `Device` is moved to its reader thread and only used there.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful CreateFileW and is closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_handle(path: &str, access: u32) -> Result<Handle, u32> {
    let path = wide(path);
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call; the other
    // arguments are plain flags or null as CreateFileW documents.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: no preconditions.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(handle))
    }
}

fn product_string(handle: &Handle) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: the buffer is valid for its full byte length, which is what we pass.
    let ok = unsafe { HidD_GetProductString(handle.0, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32) };
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn top_level_usage(handle: &Handle) -> Option<(u16, u16)> {
    let mut preparsed = 0;
    // SAFETY: `preparsed` is a valid out pointer; freed below when the call succeeds.
    if !unsafe { HidD_GetPreparsedData(handle.0, &mut preparsed) } {
        return None;
    }
    let mut caps = HIDP_CAPS::default();
    // SAFETY: `preparsed` is valid until HidD_FreePreparsedData; `caps` is a valid out pointer.
    let status = unsafe { HidP_GetCaps(preparsed, &mut caps) };
    // SAFETY: frees the data obtained above, once.
    unsafe {
        HidD_FreePreparsedData(preparsed);
    }
    (status == HIDP_STATUS_SUCCESS).then_some((caps.UsagePage, caps.Usage))
}

/// `buf` as a variable-length detail struct with its header set; `None` when the header does not fit.
fn detail_header(buf: &mut [u32]) -> Option<*mut SP_DEVICE_INTERFACE_DETAIL_DATA_W> {
    if size_of_val(buf) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
        return None;
    }
    let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    const { assert!(align_of::<u32>() >= align_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()) };
    // SAFETY: the buffer holds the whole struct (checked above) and is aligned for it (checked at
    // compile time); `cbSize` is its first field, and this writes nothing else.
    unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
    Some(detail)
}

/// The device interface paths of every present HID interface.
fn interface_paths() -> Vec<String> {
    let mut guid = GUID::from_u128(0);
    // SAFETY: `guid` is a valid out pointer.
    unsafe { HidD_GetHidGuid(&mut guid) };
    // SAFETY: valid GUID pointer, no enumerator or window, documented flags.
    let set =
        unsafe { SetupDiGetClassDevsW(&guid, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE) };
    if set == -1 {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for index in 0.. {
        let mut data = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: `set` is a live device info set; `data.cbSize` is initialised as required.
        if unsafe { SetupDiEnumDeviceInterfaces(set, null(), &guid, index, &mut data) } == 0 {
            break;
        }
        let mut required = 0u32;
        // SAFETY: the size query: null buffer and zero size are documented; only `required` is written.
        unsafe { SetupDiGetDeviceInterfaceDetailW(set, &data, null_mut(), 0, &mut required, null_mut()) };
        if (required as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
            continue;
        }
        // A u32-aligned buffer of `required` bytes for the variable-length detail struct.
        let mut buf = vec![0u32; (required as usize).div_ceil(4)];
        let Some(detail) = detail_header(&mut buf) else {
            continue;
        };
        // SAFETY: `detail` points to `required` writable bytes with `cbSize` set.
        if unsafe { SetupDiGetDeviceInterfaceDetailW(set, &data, detail, required, null_mut(), null_mut()) }
            == 0
        {
            continue;
        }
        paths.push(super::utf16_in_words(
            &buf,
            std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath),
        ));
    }
    // SAFETY: destroys the set created above, once.
    unsafe { SetupDiDestroyDeviceInfoList(set) };
    paths
}

pub(super) fn enumerate(vendor_filter: &dyn Fn(u16) -> bool) -> Result<Vec<HidDeviceInfo>, HidError> {
    let mut out = Vec::new();
    for path in interface_paths() {
        // Access 0: attributes only, works even for devices Windows opens exclusively.
        let Ok(handle) = open_handle(&path, 0) else {
            continue;
        };
        let mut attributes = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        // SAFETY: valid handle and out pointer with `Size` set.
        if !unsafe { HidD_GetAttributes(handle.0, &mut attributes) } || !vendor_filter(attributes.VendorID) {
            continue;
        }
        out.push(HidDeviceInfo {
            product: product_string(&handle),
            usage: top_level_usage(&handle),
            path,
            vendor_id: attributes.VendorID,
            product_id: attributes.ProductID,
            report_descriptor: None,
        });
    }
    Ok(out)
}

fn map_error(code: u32, path: &str) -> HidError {
    match code {
        ERROR_ACCESS_DENIED => HidError::PermissionDenied(path.to_string()),
        ERROR_SHARING_VIOLATION => HidError::Busy(path.to_string()),
        // ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND (2), ERROR_GEN_FAILURE (31),
        // ERROR_OPERATION_ABORTED (995): the device went away.
        ERROR_DEVICE_NOT_CONNECTED | 2 | 31 | 995 => HidError::Disconnected,
        other => HidError::Io(format!("{path}: Win32 error {other}")),
    }
}

#[derive(Debug)]
pub(super) struct Device {
    handle: Handle,
    path: String,
}

impl Device {
    pub(super) fn open(path: &str) -> Result<Self, HidError> {
        open_handle(path, GENERIC_READ)
            .map(|handle| Self {
                handle,
                path: path.to_string(),
            })
            .map_err(|code| map_error(code, path))
    }

    pub(super) fn read(&mut self, buf: &mut [u8]) -> Result<usize, HidError> {
        let mut read = 0u32;
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: synchronous read (no OVERLAPPED) into `buf`, valid for `len` bytes.
        let ok = unsafe { ReadFile(self.handle.0, buf.as_mut_ptr(), len, &mut read, null_mut()) };
        if ok == 0 {
            // SAFETY: no preconditions.
            return Err(map_error(unsafe { GetLastError() }, &self.path));
        }
        if read == 0 {
            return Err(HidError::Disconnected);
        }
        Ok(read as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header lands in the buffer's first words, and a path written after it reads back through
    /// the same decode `interface_paths` uses. Runs under Miri with the Windows target
    /// (`docs/06-testing-and-ci.md` § 14).
    #[test]
    fn the_detail_header_fits_its_buffer_and_the_path_follows_it() {
        let mut buf = vec![0u32; 8];
        let detail = detail_header(&mut buf).expect("eight words hold the header");
        // SAFETY: `detail` points into `buf`, which has room for these units after the header.
        unsafe {
            let path = std::ptr::addr_of_mut!((*detail).DevicePath).cast::<u16>();
            path.write(u16::from(b'h'));
            path.add(1).write(u16::from(b'i'));
        }
        assert_eq!(buf[0], size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32);
        let offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        assert_eq!(crate::hid::utf16_in_words(&buf, offset), "hi");
    }

    #[test]
    fn a_buffer_too_small_for_the_header_is_refused() {
        assert!(detail_header(&mut []).is_none());
    }
}
