// SPDX-License-Identifier: MPL-2.0
//! Linux and Android: `/proc/net/route` first, then an `RTM_GETROUTE` dump over `NETLINK_ROUTE`
//! (`rtnetlink(7)`). `/proc/net/route` lists only the main table, and Android 10 and later deny it
//! to apps and keep each network's default route in a table of its own, so the netlink dump (all
//! tables, no root, no `bind`) is the reader there. Where the platform denies that too, the result
//! is `Unsupported` and the hosting flow reports `port_map_unavailable`.
#![allow(unsafe_code)]

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::port_map::PortMapError;
use crate::port_map::route_table::{NetlinkEnd, netlink_dump_end, parse_netlink_route_dump};

const RTM_GETROUTE: u16 = 26;
const NLM_F_REQUEST: u16 = 0x1;
/// `NLM_F_ROOT | NLM_F_MATCH`.
const NLM_F_DUMP: u16 = 0x300;
/// A dump larger than this is not a home machine's routing table: stop reading.
const MAX_DUMP_BYTES: usize = 4 << 20;

/// The default IPv4 gateway.
pub(in crate::port_map) fn default_gateway_v4() -> Result<Ipv4Addr, PortMapError> {
    match std::fs::read_to_string("/proc/net/route") {
        Ok(table) => {
            if let Some(gateway) = crate::port_map::parse_proc_route(&table) {
                return Ok(gateway);
            }
        }
        Err(error) => tracing::debug!(%error, "port map: /proc/net/route unreadable, asking netlink"),
    }
    netlink_default_gateway_v4()
}

/// The default IPv4 gateway from a netlink route dump.
fn netlink_default_gateway_v4() -> Result<Ipv4Addr, PortMapError> {
    let dump = netlink_route_dump().map_err(|error| match error.kind() {
        io::ErrorKind::PermissionDenied => {
            PortMapError::Unsupported("the routing table is not readable here (netlink denied)")
        }
        _ => PortMapError::Io(error),
    })?;
    parse_netlink_route_dump(&dump).ok_or(PortMapError::Unsupported("no IPv4 default route"))
}

/// Every message of an IPv4 `RTM_GETROUTE` dump, concatenated.
fn netlink_route_dump() -> io::Result<Vec<u8>> {
    // SAFETY: `socket` takes three integers and returns a new descriptor or -1.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a descriptor `socket` just returned and nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // A kernel that never answers must not hang the hosting flow's thread.
    let timeout = libc::timeval {
        tv_sec: 2,
        tv_usec: 0,
    };
    // SAFETY: `fd` is open; the pointer and length describe `timeout`, a live `timeval`.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let request = route_dump_request(1);
    // SAFETY: an all-zero `sockaddr_nl` is valid (it addresses the kernel); the family is set next.
    let mut kernel: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // SAFETY: `fd` is open; `request` is a live buffer of the length passed; `kernel` is a live
    // `sockaddr_nl` of the size passed.
    let sent = unsafe {
        libc::sendto(
            fd.as_raw_fd(),
            request.as_ptr().cast(),
            request.len(),
            0,
            (&raw const kernel).cast(),
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut dump = Vec::new();
    let mut chunk = vec![0_u8; 32 * 1024];
    loop {
        // SAFETY: `fd` is open; `chunk` is a live, writable buffer of the length passed.
        let got = unsafe { libc::recv(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len(), 0) };
        let got = match usize::try_from(got) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(got) => got,
            Err(_) => {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
        };
        let received = &chunk[..got];
        dump.extend_from_slice(received);
        match netlink_dump_end(received) {
            NetlinkEnd::Done => return Ok(dump),
            NetlinkEnd::Error(errno) => return Err(io::Error::from_raw_os_error(errno)),
            NetlinkEnd::More if dump.len() > MAX_DUMP_BYTES => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "routing table dump too large",
                ));
            }
            NetlinkEnd::More => {}
        }
    }
}

/// `nlmsghdr` + `rtmsg` asking for every IPv4 route (`rtm_family = AF_INET`, the rest zero).
fn route_dump_request(seq: u32) -> [u8; 28] {
    let mut request = [0_u8; 28];
    request[0..4].copy_from_slice(&28_u32.to_ne_bytes());
    request[4..6].copy_from_slice(&RTM_GETROUTE.to_ne_bytes());
    request[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    request[8..12].copy_from_slice(&seq.to_ne_bytes());
    // nlmsg_pid 0: the kernel fills in this socket's port.
    request[16] = libc::AF_INET as u8;
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_dump_request_has_the_rtnetlink_layout() {
        let request = route_dump_request(7);
        assert_eq!(u32::from_ne_bytes(request[0..4].try_into().expect("slice")), 28);
        assert_eq!(
            u16::from_ne_bytes(request[4..6].try_into().expect("slice")),
            libc::RTM_GETROUTE
        );
        assert_eq!(
            i32::from(u16::from_ne_bytes(request[6..8].try_into().expect("slice"))),
            libc::NLM_F_REQUEST | libc::NLM_F_DUMP
        );
        assert_eq!(u32::from_ne_bytes(request[8..12].try_into().expect("slice")), 7);
        assert_eq!(i32::from(request[16]), libc::AF_INET);
        assert!(request[17..].iter().all(|&b| b == 0));
    }

    /// The netlink reader alone (Android's path) agrees with `/proc/net/route` on a Linux host.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_netlink_reader_agrees_with_proc_net_route() {
        let Ok(table) = std::fs::read_to_string("/proc/net/route") else {
            eprintln!("no /proc/net/route here");
            return;
        };
        let netlink = netlink_default_gateway_v4();
        match crate::port_map::parse_proc_route(&table) {
            Some(expected) => {
                let got = netlink.expect("netlink route dump");
                eprintln!("netlink default gateway {got}, /proc/net/route {expected}");
                assert_eq!(got, expected);
            }
            None => eprintln!("no main-table default route; netlink says {netlink:?}"),
        }
    }
}
