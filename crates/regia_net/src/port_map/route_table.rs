// SPDX-License-Identifier: MPL-2.0
//! Default-route parsers for the routing tables each operating system hands out (ADR-8893).
//!
//! Each parser reads the raw bytes one platform call returns and picks the IPv4 default route's
//! gateway. They are pure, so every parser is tested on every host against captured bytes
//! (`crates/regia_net/tests/gateway_readers.rs`); the calls that produce the bytes live in
//! `port_map/platform/<os>.rs`. Layouts follow the platforms' public documentation:
//!
//! - Linux and Android: an `RTM_GETROUTE` dump over `NETLINK_ROUTE` (`netlink(7)`, `rtnetlink(7)`):
//!   `nlmsghdr` (16 bytes), `rtmsg` (12 bytes), then `rtattr` records aligned to four bytes, all in
//!   host byte order except the addresses.
//! - Windows: `GetIpForwardTable` fills a `MIB_IPFORWARDTABLE`, a `DWORD` count and then
//!   `MIB_IPFORWARDROW`s of fourteen `DWORD`s (Win32 IP Helper reference); addresses are stored in
//!   network byte order.
//! - macOS: `sysctl {CTL_NET, PF_ROUTE, 0, AF_INET, NET_RT_FLAGS, RTF_GATEWAY}` returns
//!   `rt_msghdr`s (92 bytes at `RTM_VERSION` 5, `route(4)`) each followed by the `sockaddr`s its
//!   `rtm_addrs` bits name, every one padded to four bytes (a zero `sa_len` takes four).

use std::net::Ipv4Addr;

const AF_INET: u8 = 2;

fn u16_ne(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_ne(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn i32_ne(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn ipv4_at(bytes: &[u8], at: usize) -> Option<Ipv4Addr> {
    let octets: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(Ipv4Addr::from(octets))
}

/// Rounds a length up to the four-byte alignment netlink and the BSD routing messages use.
const fn align4(len: usize) -> usize {
    (len + 3) & !3
}

// --- Linux and Android: rtnetlink ----------------------------------------------------------

const NLMSG_HDR_BYTES: usize = 16;
// The dump's end markers matter only to the reader, which runs on Linux and Android.
#[cfg(any(target_os = "linux", target_os = "android", test))]
const NLMSG_ERROR: u16 = 2;
#[cfg(any(target_os = "linux", target_os = "android", test))]
const NLMSG_DONE: u16 = 3;
const RTM_NEWROUTE: u16 = 24;
const RTMSG_BYTES: usize = 12;
const RTN_UNICAST: u8 = 1;
const RT_TABLE_MAIN: u32 = 254;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_TABLE: u16 = 15;

/// Where a chunk of a netlink dump leaves the dump.
#[cfg(any(target_os = "linux", target_os = "android", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetlinkEnd {
    /// More messages follow.
    More,
    /// `NLMSG_DONE` was seen.
    Done,
    /// The kernel answered `NLMSG_ERROR` with this (positive) errno.
    Error(i32),
}

/// The messages of a netlink byte stream: `(type, whole message)`. Stops at the first message
/// whose length does not fit, so truncated input yields only the complete messages before it.
fn netlink_messages(bytes: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut at = 0_usize;
    std::iter::from_fn(move || {
        let len = usize::try_from(u32_ne(bytes, at)?).ok()?;
        let kind = u16_ne(bytes, at + 4)?;
        if len < NLMSG_HDR_BYTES {
            return None;
        }
        let message = bytes.get(at..at.checked_add(len)?)?;
        at = at.saturating_add(align4(len));
        Some((kind, message))
    })
}

/// Whether one received chunk of a dump ends it.
#[cfg(any(target_os = "linux", target_os = "android", test))]
pub(crate) fn netlink_dump_end(chunk: &[u8]) -> NetlinkEnd {
    for (kind, message) in netlink_messages(chunk) {
        match kind {
            NLMSG_DONE => return NetlinkEnd::Done,
            NLMSG_ERROR => {
                // `nlmsgerr`: a negative errno, 0 for an acknowledgment.
                let code = i32_ne(message, NLMSG_HDR_BYTES).unwrap_or(0);
                return if code == 0 {
                    NetlinkEnd::Done
                } else {
                    NetlinkEnd::Error(code.saturating_neg())
                };
            }
            _ => {}
        }
    }
    NetlinkEnd::More
}

/// The gateway of the IPv4 default route in an `RTM_GETROUTE` dump (the bytes of every
/// `RTM_NEWROUTE` message the kernel sent, in order).
///
/// Only unicast routes with a zero-length destination and an `RTA_GATEWAY` count. The main table
/// wins over the others; inside a tier the lowest `RTA_PRIORITY` (metric) wins, then the first
/// seen. Routes in other tables still count, because Android keeps each network's default route
/// in a table of its own and leaves the main table without one. Integers are read in host byte
/// order, as the kernel writes them.
#[must_use]
pub fn parse_netlink_route_dump(bytes: &[u8]) -> Option<Ipv4Addr> {
    let mut best: Option<((bool, u32), Ipv4Addr)> = None;
    for (kind, message) in netlink_messages(bytes) {
        if kind != RTM_NEWROUTE {
            continue;
        }
        let Some(rtm) = message.get(NLMSG_HDR_BYTES..NLMSG_HDR_BYTES + RTMSG_BYTES) else {
            continue;
        };
        let (family, dst_len, table, route_type) = (rtm[0], rtm[1], rtm[4], rtm[7]);
        if family != AF_INET || dst_len != 0 || route_type != RTN_UNICAST {
            continue;
        }
        let mut table = u32::from(table);
        let mut priority = 0_u32;
        let mut gateway = None;
        let mut at = NLMSG_HDR_BYTES + RTMSG_BYTES;
        while let (Some(len), Some(kind)) = (u16_ne(message, at), u16_ne(message, at + 2)) {
            let len = usize::from(len);
            if len < 4 || at + len > message.len() {
                break;
            }
            match kind {
                RTA_GATEWAY if len == 8 => gateway = ipv4_at(message, at + 4),
                RTA_PRIORITY if len == 8 => priority = u32_ne(message, at + 4).unwrap_or(0),
                RTA_TABLE if len == 8 => table = u32_ne(message, at + 4).unwrap_or(table),
                _ => {}
            }
            at += align4(len);
        }
        let Some(gateway) = gateway.filter(|g| !g.is_unspecified()) else {
            continue;
        };
        let key = (table != RT_TABLE_MAIN, priority);
        if best.is_none_or(|(held, _)| key < held) {
            best = Some((key, gateway));
        }
    }
    best.map(|(_, gateway)| gateway)
}

// --- Windows: IP Helper's MIB_IPFORWARDTABLE -----------------------------------------------

/// `MIB_IPFORWARDROW`: fourteen `DWORD`s.
const FORWARD_ROW_BYTES: usize = 14 * 4;

/// The gateway of the IPv4 default route in a `MIB_IPFORWARDTABLE` as `GetIpForwardTable`
/// fills it: destination and mask `0.0.0.0` and a non-zero next hop; the lowest
/// `dwForwardMetric1` wins, then the first row. Counts and metrics are little-endian (Windows);
/// addresses are in network byte order. A count larger than the bytes is cut to the rows present.
#[must_use]
pub fn parse_ipv4_forward_table(bytes: &[u8]) -> Option<Ipv4Addr> {
    let count = usize::try_from(u32_le(bytes, 0)?).ok()?;
    let rows = bytes.get(4..)?;
    let mut best: Option<(u32, Ipv4Addr)> = None;
    for row in rows.as_chunks::<FORWARD_ROW_BYTES>().0.iter().take(count) {
        let dest = ipv4_at(row, 0)?;
        let mask = ipv4_at(row, 4)?;
        let next_hop = ipv4_at(row, 12)?;
        let metric = u32_le(row, 36)?;
        if !dest.is_unspecified() || !mask.is_unspecified() || next_hop.is_unspecified() {
            continue;
        }
        if best.is_none_or(|(held, _)| metric < held) {
            best = Some((metric, next_hop));
        }
    }
    best.map(|(_, gateway)| gateway)
}

// --- macOS: the PF_ROUTE sysctl dump --------------------------------------------------------

/// `sizeof(struct rt_msghdr)` at `RTM_VERSION` 5 on Apple platforms.
const RT_MSGHDR_BYTES: usize = 92;
const RTM_VERSION: u8 = 5;
const RTF_UP: i32 = 0x1;
const RTF_GATEWAY: i32 = 0x2;
const RTF_IFSCOPE: i32 = 0x0100_0000;
const RTA_DST: u32 = 0x1;
const RTA_GATEWAY_BIT: u32 = 0x2;
const RTA_NETMASK: u32 = 0x4;
/// `RTAX_MAX`: the number of `sockaddr` slots `rtm_addrs` can name.
const RTAX_MAX: u32 = 8;

/// The gateway of the IPv4 default route in a `PF_ROUTE` sysctl dump (`NET_RT_FLAGS` or
/// `NET_RT_DUMP`): an up route with `RTF_GATEWAY`, destination `0.0.0.0`, a zero (or absent)
/// netmask and an `AF_INET` gateway. An unscoped default wins over an interface-scoped one
/// (`RTF_IFSCOPE`), then the first seen. Messages of another `rtm_version` are skipped.
#[must_use]
pub fn parse_bsd_route_dump(bytes: &[u8]) -> Option<Ipv4Addr> {
    let mut scoped = None;
    let mut at = 0_usize;
    while let Some(len) = u16_ne(bytes, at) {
        let len = usize::from(len);
        if len == 0 {
            break;
        }
        let Some(message) = bytes.get(at..at + len) else {
            break;
        };
        at += len;
        if message.len() < RT_MSGHDR_BYTES || message[2] != RTM_VERSION {
            continue;
        }
        let (Some(flags), Some(addrs)) = (i32_ne(message, 8), u32_ne(message, 12)) else {
            continue;
        };
        if flags & (RTF_UP | RTF_GATEWAY) != RTF_UP | RTF_GATEWAY {
            continue;
        }
        let Some(gateway) = bsd_default_gateway(message, addrs) else {
            continue;
        };
        if flags & RTF_IFSCOPE == 0 {
            return Some(gateway);
        }
        scoped.get_or_insert(gateway);
    }
    scoped
}

/// The `AF_INET` gateway of one routing message when its destination is the IPv4 default.
fn bsd_default_gateway(message: &[u8], addrs: u32) -> Option<Ipv4Addr> {
    let mut at = RT_MSGHDR_BYTES;
    let (mut dst, mut gateway, mut mask_zero) = (None, None, true);
    for slot in 0..RTAX_MAX {
        let bit = 1_u32 << slot;
        if addrs & bit == 0 {
            continue;
        }
        let sa_len = usize::from(*message.get(at)?);
        let family = *message.get(at + 1).unwrap_or(&0);
        let sockaddr = message.get(at..at + sa_len.max(4))?;
        match bit {
            RTA_DST if family == AF_INET && sa_len >= 8 => dst = ipv4_at(sockaddr, 4),
            RTA_GATEWAY_BIT if family == AF_INET && sa_len >= 8 => gateway = ipv4_at(sockaddr, 4),
            // A netmask carries only the bytes up to its last non-zero one; `sa_len` 0 is /0.
            RTA_NETMASK => mask_zero = sockaddr.iter().take(sa_len.min(8)).skip(4).all(|&b| b == 0),
            _ => {}
        }
        at += if sa_len == 0 { 4 } else { align4(sa_len) };
    }
    (dst?.is_unspecified() && mask_zero).then_some(gateway?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_and_empty_input_find_nothing() {
        assert_eq!(parse_netlink_route_dump(&[]), None);
        assert_eq!(parse_ipv4_forward_table(&[]), None);
        assert_eq!(parse_bsd_route_dump(&[]), None);
        assert_eq!(netlink_dump_end(&[0; 3]), NetlinkEnd::More);
        // A netlink length shorter than its header ends the walk instead of looping.
        let mut bogus = vec![0_u8; 32];
        bogus[..4].copy_from_slice(&4_u32.to_ne_bytes());
        assert_eq!(parse_netlink_route_dump(&bogus), None);
    }

    #[test]
    fn a_netlink_error_message_reports_its_errno() {
        let mut chunk = vec![0_u8; 36];
        chunk[..4].copy_from_slice(&36_u32.to_ne_bytes());
        chunk[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        chunk[16..20].copy_from_slice(&(-13_i32).to_ne_bytes());
        assert_eq!(netlink_dump_end(&chunk), NetlinkEnd::Error(13));
        chunk[16..20].copy_from_slice(&0_i32.to_ne_bytes());
        assert_eq!(netlink_dump_end(&chunk), NetlinkEnd::Done);
    }
}
