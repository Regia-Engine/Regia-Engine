// SPDX-License-Identifier: MPL-2.0
//! Platforms without a socket-option backend: the transport keeps the operating system's defaults
//! and reports that it could not read them.

use std::io;
use std::net::UdpSocket;

use super::Buffer;

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "socket options are not wired on this platform",
    )
}

/// Not available here.
pub(in crate::socket) fn set_buffer(_socket: &UdpSocket, _which: Buffer, _bytes: usize) -> io::Result<()> {
    Err(unsupported())
}

/// Not available here.
pub(in crate::socket) fn read_buffer(_socket: &UdpSocket, _which: Buffer) -> io::Result<usize> {
    Err(unsupported())
}

/// Not available here.
pub(in crate::socket) fn set_dscp(_socket: &UdpSocket, _ipv6: bool, _dscp: u8) -> io::Result<()> {
    Err(unsupported())
}

/// Not available here.
pub(in crate::socket) fn set_dont_fragment(_socket: &UdpSocket, _ipv6: bool) -> io::Result<()> {
    Err(unsupported())
}

/// Not available here.
pub(in crate::socket) fn read_dont_fragment(_socket: &UdpSocket, _ipv6: bool) -> io::Result<bool> {
    Err(unsupported())
}

/// No size refusal is recognized here.
pub(in crate::socket) fn is_size_refusal(_error: &io::Error) -> bool {
    false
}
