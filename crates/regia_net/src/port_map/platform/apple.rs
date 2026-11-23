// SPDX-License-Identifier: MPL-2.0
//! macOS (and the other Apple platforms): the IPv4 routes with a gateway from the `PF_ROUTE`
//! sysctl (`sysctl {CTL_NET, PF_ROUTE, 0, AF_INET, NET_RT_FLAGS, RTF_GATEWAY}`, `route(4)`),
//! which needs no privilege. The first call sizes the buffer; a table that grows before the second
//! (`ENOMEM`) is asked for again.
#![allow(unsafe_code)]

use std::io;
use std::net::Ipv4Addr;

use crate::port_map::PortMapError;
use crate::port_map::route_table::parse_bsd_route_dump;

const CTL_NET: libc::c_int = 4;
const PF_ROUTE: libc::c_int = 17;
const AF_INET: libc::c_int = 2;
const NET_RT_FLAGS: libc::c_int = 2;
const RTF_GATEWAY: libc::c_int = 0x2;

/// The default IPv4 gateway.
pub(in crate::port_map) fn default_gateway_v4() -> Result<Ipv4Addr, PortMapError> {
    let dump = route_dump().map_err(PortMapError::Io)?;
    parse_bsd_route_dump(&dump).ok_or(PortMapError::Unsupported("no IPv4 default route"))
}

fn route_dump() -> io::Result<Vec<u8>> {
    let mut mib = [CTL_NET, PF_ROUTE, 0, AF_INET, NET_RT_FLAGS, RTF_GATEWAY];
    for _ in 0..4 {
        let mut size: libc::size_t = 0;
        // SAFETY: `mib` is a live array of the six names passed; a null old buffer only asks for
        // the size, which the call writes to `size`; no new value is set.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                std::ptr::null_mut(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // Headroom for routes added between the two calls.
        let mut buffer = vec![0_u8; size + size / 8 + 512];
        let mut granted: libc::size_t = buffer.len();
        // SAFETY: `buffer` is a live, writable allocation of `granted` bytes; the call writes at
        // most that many and stores the length it wrote in `granted`.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                buffer.as_mut_ptr().cast(),
                &raw mut granted,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            buffer.truncate(granted);
            return Ok(buffer);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::OutOfMemory,
        "the routing table kept growing",
    ))
}
