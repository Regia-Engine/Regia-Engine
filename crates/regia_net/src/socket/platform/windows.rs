// SPDX-License-Identifier: MPL-2.0
//! Windows: Winsock `setsockopt` / `getsockopt` from `ws2_32`, which the standard library already
//! links. The option numbers are Winsock's published values. Windows applies `IP_TOS` only under
//! a QoS policy, so the DSCP mark is best effort there.
#![allow(unsafe_code)]

use std::io;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;

use super::Buffer;

const SOL_SOCKET: i32 = 0xffff;
const SO_SNDBUF: i32 = 0x1001;
const SO_RCVBUF: i32 = 0x1002;
const IPPROTO_IP: i32 = 0;
const IP_TOS: i32 = 3;
const IPPROTO_IPV6: i32 = 41;
const IPV6_TCLASS: i32 = 39;
/// `IP_DONTFRAGMENT` (`ws2ipdef.h`): set don't-fragment on IPv4 packets.
const IP_DONTFRAGMENT: i32 = 14;
/// `IPV6_DONTFRAG` (`ws2ipdef.h`): don't fragment IPv6 packets at the source.
const IPV6_DONTFRAG: i32 = 14;
/// `WSAEMSGSIZE`: the datagram is larger than the path allows.
const WSAEMSGSIZE: i32 = 10040;

// SAFETY: Winsock's `setsockopt` and `getsockopt` (`SOCKET` handle, `int` level and name, a value
// pointer and an `int` length; the `system` calling convention). Each call below passes the handle
// of a socket the caller owns and a pointer and length that describe one live `i32`.
#[link(name = "ws2_32")]
unsafe extern "system" {
    fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
    fn getsockopt(s: usize, level: i32, name: i32, value: *mut u8, len: *mut i32) -> i32;
}

fn set_int(socket: &UdpSocket, level: i32, name: i32, value: i32) -> io::Result<()> {
    let len = size_of::<i32>() as i32;
    // SAFETY: the handle is the open socket `socket` borrows for the whole call; the pointer and
    // length describe `value`, a live `i32` the call only reads.
    let r = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            level,
            name,
            (&raw const value).cast(),
            len,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn get_int(socket: &UdpSocket, level: i32, name: i32) -> io::Result<i32> {
    let mut value: i32 = 0;
    let mut len = size_of::<i32>() as i32;
    // SAFETY: the handle is the open socket `socket` borrows; the pointer and length describe
    // `value`, a live `i32` the call writes at most `len` bytes of, and `len` is a live `i32`.
    let r = unsafe {
        getsockopt(
            socket.as_raw_socket() as usize,
            level,
            name,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    if r == 0 {
        Ok(value)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn option(which: Buffer) -> i32 {
    match which {
        Buffer::Receive => SO_RCVBUF,
        Buffer::Send => SO_SNDBUF,
    }
}

/// Asks for a buffer of `bytes`.
pub(in crate::socket) fn set_buffer(socket: &UdpSocket, which: Buffer, bytes: usize) -> io::Result<()> {
    let value = i32::try_from(bytes).unwrap_or(i32::MAX);
    set_int(socket, SOL_SOCKET, option(which), value)
}

/// The buffer size Winsock reports now, bytes.
pub(in crate::socket) fn read_buffer(socket: &UdpSocket, which: Buffer) -> io::Result<usize> {
    get_int(socket, SOL_SOCKET, option(which)).map(|v| usize::try_from(v).unwrap_or(0))
}

/// Sets the traffic-class byte (DSCP in the upper six bits); Windows may ignore it.
pub(in crate::socket) fn set_dscp(socket: &UdpSocket, ipv6: bool, dscp: u8) -> io::Result<()> {
    let tos = i32::from(dscp) << 2;
    if ipv6 {
        set_int(socket, IPPROTO_IPV6, IPV6_TCLASS, tos)
    } else {
        set_int(socket, IPPROTO_IP, IP_TOS, tos)
    }
}

/// Sets don't-fragment on the socket's packets (RFC 9000 § 14, RFC 8899 § 3): `IP_DONTFRAGMENT`,
/// and `IPV6_DONTFRAG` on an IPv6 socket (plus the IPv4 option for v4-mapped traffic, best effort).
pub(in crate::socket) fn set_dont_fragment(socket: &UdpSocket, ipv6: bool) -> io::Result<()> {
    if ipv6 {
        set_int(socket, IPPROTO_IPV6, IPV6_DONTFRAG, 1)?;
        let _ = set_int(socket, IPPROTO_IP, IP_DONTFRAGMENT, 1);
        Ok(())
    } else {
        set_int(socket, IPPROTO_IP, IP_DONTFRAGMENT, 1)
    }
}

/// Whether the socket's packets carry don't-fragment now (the family's option read back).
pub(in crate::socket) fn read_dont_fragment(socket: &UdpSocket, ipv6: bool) -> io::Result<bool> {
    let value = if ipv6 {
        get_int(socket, IPPROTO_IPV6, IPV6_DONTFRAG)?
    } else {
        get_int(socket, IPPROTO_IP, IP_DONTFRAGMENT)?
    };
    Ok(value != 0)
}

/// The send failed because the datagram is larger than the path allows (`WSAEMSGSIZE`).
pub(in crate::socket) fn is_size_refusal(error: &io::Error) -> bool {
    error.raw_os_error() == Some(WSAEMSGSIZE)
}
