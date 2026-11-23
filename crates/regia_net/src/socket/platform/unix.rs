// SPDX-License-Identifier: MPL-2.0
//! Linux, Android and Apple platforms: `setsockopt` / `getsockopt` from the C library the
//! standard library already links. The option numbers are the platforms' published ABI values.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;

use super::Buffer;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod abi {
    pub const SOL_SOCKET: i32 = 1;
    pub const SO_SNDBUF: i32 = 7;
    pub const SO_RCVBUF: i32 = 8;
    pub const IP_TOS: i32 = 1;
    pub const IPV6_TCLASS: i32 = 67;
    /// `ip(7)`: the path MTU discovery mode of IPv4 packets.
    pub const IP_MTU_DISCOVER: i32 = 10;
    /// `ipv6(7)`: the same for IPv6 packets.
    pub const IPV6_MTU_DISCOVER: i32 = 23;
    /// `IP_PMTUDISC_PROBE` / `IPV6_PMTUDISC_PROBE`: set don't-fragment and ignore the kernel's
    /// cached path MTU, so the endpoint's own probing decides the size (a datagram over the
    /// interface MTU fails with `EMSGSIZE`).
    pub const PMTUDISC_PROBE: i32 = 3;
    pub const EMSGSIZE: i32 = 90;
}

#[cfg(target_vendor = "apple")]
mod abi {
    pub const SOL_SOCKET: i32 = 0xffff;
    pub const SO_SNDBUF: i32 = 0x1001;
    pub const SO_RCVBUF: i32 = 0x1002;
    pub const IP_TOS: i32 = 3;
    pub const IPV6_TCLASS: i32 = 36;
    /// `ip(4)`: set don't-fragment on IPv4 packets.
    pub const IP_DONTFRAG: i32 = 28;
    /// `ip6(4)` (RFC 3542 § 11.2): don't fragment IPv6 packets at the source.
    pub const IPV6_DONTFRAG: i32 = 62;
    pub const EMSGSIZE: i32 = 40;
}

const IPPROTO_IP: i32 = 0;
const IPPROTO_IPV6: i32 = 41;

// SAFETY: the C library's `setsockopt` and `getsockopt` with their POSIX signatures (`int`
// descriptor, level and name, a value pointer and a `socklen_t` length, which is 32 bits on every
// target this module builds for). Each call below passes a descriptor the caller's socket owns and
// a pointer and length that describe one live `i32`.
unsafe extern "C" {
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const c_void, len: u32) -> i32;
    fn getsockopt(fd: i32, level: i32, name: i32, value: *mut c_void, len: *mut u32) -> i32;
}

fn set_int(socket: &UdpSocket, level: i32, name: i32, value: i32) -> io::Result<()> {
    let len = size_of::<i32>() as u32;
    // SAFETY: the descriptor is the open socket `socket` borrows for the whole call; the pointer
    // and length describe `value`, a live `i32` the call only reads.
    let r = unsafe { setsockopt(socket.as_raw_fd(), level, name, (&raw const value).cast(), len) };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn get_int(socket: &UdpSocket, level: i32, name: i32) -> io::Result<i32> {
    let mut value: i32 = 0;
    let mut len = size_of::<i32>() as u32;
    // SAFETY: the descriptor is the open socket `socket` borrows; the pointer and length describe
    // `value`, a live `i32` the call writes at most `len` bytes of, and `len` is a live `u32`.
    let r = unsafe {
        getsockopt(
            socket.as_raw_fd(),
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
        Buffer::Receive => abi::SO_RCVBUF,
        Buffer::Send => abi::SO_SNDBUF,
    }
}

/// Asks for a buffer of `bytes` (the kernel may cap it; Linux also doubles what it stores).
pub(in crate::socket) fn set_buffer(socket: &UdpSocket, which: Buffer, bytes: usize) -> io::Result<()> {
    let value = i32::try_from(bytes).unwrap_or(i32::MAX);
    set_int(socket, abi::SOL_SOCKET, option(which), value)
}

/// The buffer size the kernel reports now, bytes.
pub(in crate::socket) fn read_buffer(socket: &UdpSocket, which: Buffer) -> io::Result<usize> {
    get_int(socket, abi::SOL_SOCKET, option(which)).map(|v| usize::try_from(v).unwrap_or(0))
}

/// Sets the traffic-class byte (DSCP in the upper six bits) of the socket's packets.
pub(in crate::socket) fn set_dscp(socket: &UdpSocket, ipv6: bool, dscp: u8) -> io::Result<()> {
    let tos = i32::from(dscp) << 2;
    if ipv6 {
        set_int(socket, IPPROTO_IPV6, abi::IPV6_TCLASS, tos)
    } else {
        set_int(socket, IPPROTO_IP, abi::IP_TOS, tos)
    }
}

/// Sets don't-fragment on the socket's packets (RFC 9000 § 14, RFC 8899 § 3): Linux and Android
/// `IP_MTU_DISCOVER` = `IP_PMTUDISC_PROBE` (and `IPV6_MTU_DISCOVER` on IPv6 sockets), Apple
/// `IP_DONTFRAG` / `IPV6_DONTFRAG`. An IPv6 socket also sets the IPv4 option for its v4-mapped
/// traffic where the stack takes it; that one is best effort.
pub(in crate::socket) fn set_dont_fragment(socket: &UdpSocket, ipv6: bool) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let (v4, v6, on) = (
        (IPPROTO_IP, abi::IP_MTU_DISCOVER),
        (IPPROTO_IPV6, abi::IPV6_MTU_DISCOVER),
        abi::PMTUDISC_PROBE,
    );
    #[cfg(target_vendor = "apple")]
    let (v4, v6, on) = (
        (IPPROTO_IP, abi::IP_DONTFRAG),
        (IPPROTO_IPV6, abi::IPV6_DONTFRAG),
        1,
    );
    if ipv6 {
        set_int(socket, v6.0, v6.1, on)?;
        let _ = set_int(socket, v4.0, v4.1, on);
        Ok(())
    } else {
        set_int(socket, v4.0, v4.1, on)
    }
}

/// Whether the socket's packets carry don't-fragment now (the family's option read back).
pub(in crate::socket) fn read_dont_fragment(socket: &UdpSocket, ipv6: bool) -> io::Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let (option, on) = if ipv6 {
        ((IPPROTO_IPV6, abi::IPV6_MTU_DISCOVER), abi::PMTUDISC_PROBE)
    } else {
        ((IPPROTO_IP, abi::IP_MTU_DISCOVER), abi::PMTUDISC_PROBE)
    };
    #[cfg(target_vendor = "apple")]
    let (option, on) = if ipv6 {
        ((IPPROTO_IPV6, abi::IPV6_DONTFRAG), 1)
    } else {
        ((IPPROTO_IP, abi::IP_DONTFRAG), 1)
    };
    get_int(socket, option.0, option.1).map(|v| v == on)
}

/// The send failed because the datagram is larger than the path allows with don't-fragment set
/// (`EMSGSIZE`).
pub(in crate::socket) fn is_size_refusal(error: &io::Error) -> bool {
    error.raw_os_error() == Some(abi::EMSGSIZE)
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::abi;

    #[test]
    fn the_option_numbers_match_the_c_library() {
        assert_eq!(abi::IP_MTU_DISCOVER, libc::IP_MTU_DISCOVER);
        assert_eq!(abi::IPV6_MTU_DISCOVER, libc::IPV6_MTU_DISCOVER);
        assert_eq!(abi::PMTUDISC_PROBE, libc::IP_PMTUDISC_PROBE);
        assert_eq!(abi::PMTUDISC_PROBE, libc::IPV6_PMTUDISC_PROBE);
        assert_eq!(abi::EMSGSIZE, libc::EMSGSIZE);
    }
}
