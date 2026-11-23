// SPDX-License-Identifier: MPL-2.0
//! Windows: the IPv4 routing table from IP Helper's `GetIpForwardTable` (`iphlpapi`), which every
//! supported Windows ships. The call sizes its buffer in a first pass (`ERROR_INSUFFICIENT_BUFFER`)
//! and fills it in the second; a table that grows in between is asked for again.
#![allow(unsafe_code)]

use std::io;
use std::net::Ipv4Addr;

use crate::port_map::PortMapError;
use crate::port_map::route_table::parse_ipv4_forward_table;

const NO_ERROR: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_NO_DATA: u32 = 232;

// SAFETY: IP Helper's `GetIpForwardTable(PMIB_IPFORWARDTABLE, PULONG, BOOL)` with the `system`
// calling convention. The call below passes a buffer of at least `*size` writable bytes (or null
// with `*size` 0 to ask for the size) and a live `u32`.
#[link(name = "iphlpapi")]
unsafe extern "system" {
    fn GetIpForwardTable(table: *mut u8, size: *mut u32, order: i32) -> u32;
}

/// The default IPv4 gateway.
pub(in crate::port_map) fn default_gateway_v4() -> Result<Ipv4Addr, PortMapError> {
    let table = forward_table()?;
    parse_ipv4_forward_table(&table).ok_or(PortMapError::Unsupported("no IPv4 default route"))
}

/// The bytes of a `MIB_IPFORWARDTABLE`.
fn forward_table() -> Result<Vec<u8>, PortMapError> {
    let mut size = 0_u32;
    // SAFETY: a null table with `size` 0 only asks for the size, which the call writes to `size`.
    let first = unsafe { GetIpForwardTable(std::ptr::null_mut(), &raw mut size, 0) };
    match first {
        ERROR_NO_DATA => return Err(PortMapError::Unsupported("no IPv4 default route")),
        ERROR_INSUFFICIENT_BUFFER | NO_ERROR => {}
        code => return Err(PortMapError::Io(win32_error(code))),
    }
    for _ in 0..4 {
        // `u32` storage keeps the table's `DWORD` alignment.
        let words = usize::try_from(size).unwrap_or(0).div_ceil(4).max(1);
        let mut buffer = vec![0_u32; words];
        let mut granted = u32::try_from(words * 4).unwrap_or(u32::MAX);
        // SAFETY: `buffer` is a live, writable allocation of `granted` bytes, aligned for `DWORD`.
        let code = unsafe { GetIpForwardTable(buffer.as_mut_ptr().cast(), &raw mut granted, 0) };
        match code {
            NO_ERROR => {
                let bytes: Vec<u8> = buffer.iter().flat_map(|w| w.to_ne_bytes()).collect();
                return Ok(bytes);
            }
            ERROR_INSUFFICIENT_BUFFER => size = granted,
            ERROR_NO_DATA => return Err(PortMapError::Unsupported("no IPv4 default route")),
            code => return Err(PortMapError::Io(win32_error(code))),
        }
    }
    Err(PortMapError::Unsupported("the routing table kept growing"))
}

fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}
