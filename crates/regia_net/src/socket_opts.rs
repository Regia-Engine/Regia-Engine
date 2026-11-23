// SPDX-License-Identifier: MPL-2.0
//! Socket-buffer and DSCP sockopts for [`crate::socket::UdpTransport`] (ADR-7998).
//!
//! Linux doubles `SO_RCVBUF` and `SO_SNDBUF` (`socket(7)`): `getsockopt` returns twice the
//! granted size, and the grant is clamped by `rmem_max` and `wmem_max`. The DSCP EF codepoint
//! is 46 (RFC 3246). The TOS byte is `46 << 2` (`0xB8`, RFC 2474). IPv4 uses `IP_TOS` (`ip(7)`);
//! IPv6 uses `IPV6_TCLASS`. A refused sockopt is not a failed bind.
//!
//! Portions copied from Direct Order 27 (ir-net/src/host.rs, commit f7e8f4740), ADR-8012 (ADR-7998).
#![allow(unsafe_code)]

use std::io;
use std::net::UdpSocket;

use crate::socket::{SocketBufferReport, SocketBufferSettings};

/// Applies `settings` and reads the kernel grant back. Never fails the socket.
pub(crate) fn apply(socket: &UdpSocket, settings: &SocketBufferSettings) -> SocketBufferReport {
    let _ = imp::set_buffer(socket, true, settings.recv_bytes);
    let _ = imp::set_buffer(socket, false, settings.send_bytes);
    let confirmed_recv = imp::get_buffer(socket, true);
    let confirmed_send = imp::get_buffer(socket, false);
    let (confirmed_recv, confirmed_send, read_back) = match (confirmed_recv, confirmed_send) {
        (Some(recv), Some(send)) => (recv, send, true),
        _ => (0, 0, false),
    };
    let dscp_ef_applied = settings.dscp_ef && imp::mark_dscp(socket);
    SocketBufferReport {
        confirmed_recv,
        confirmed_send,
        requested_recv: settings.recv_bytes,
        requested_send: settings.send_bytes,
        dscp_ef_applied,
        read_back,
        // Set by the transport after the buffers (`UdpTransport`, ADR-8893).
        dont_fragment: false,
    }
}

/// A buffer size the platform sockopt can carry. Windows and Unix take a signed 32-bit length.
fn sock_int(bytes: usize) -> io::Result<i32> {
    i32::try_from(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket buffer size does not fit in a signed 32-bit sockopt",
        )
    })
}

fn nonneg_usize(value: i32) -> Option<usize> {
    usize::try_from(value).ok()
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::net::{SocketAddr, UdpSocket};
    use std::os::fd::AsRawFd;

    use super::{nonneg_usize, sock_int};
    use crate::socket::DSCP_EF_TOS;

    const OPT_LEN: libc::socklen_t = 4;

    pub(super) fn set_buffer(socket: &UdpSocket, receive: bool, bytes: usize) -> io::Result<()> {
        let value = sock_int(bytes)?;
        let name = if receive { libc::SO_RCVBUF } else { libc::SO_SNDBUF };
        set_int(socket, libc::SOL_SOCKET, name, value)
    }

    pub(super) fn get_buffer(socket: &UdpSocket, receive: bool) -> Option<usize> {
        let name = if receive { libc::SO_RCVBUF } else { libc::SO_SNDBUF };
        get_int(socket, libc::SOL_SOCKET, name).and_then(nonneg_usize)
    }

    pub(super) fn mark_dscp(socket: &UdpSocket) -> bool {
        let Ok(value) = i32::try_from(DSCP_EF_TOS) else {
            return false;
        };
        let Some((level, name)) = dscp_opt(socket) else {
            return false;
        };
        if set_int(socket, level, name, value).is_err() {
            return false;
        }
        match get_int(socket, level, name) {
            Some(got) => u32::try_from(got).ok() == Some(DSCP_EF_TOS),
            // The set was accepted. A failed read-back does not undo that.
            None => true,
        }
    }

    fn dscp_opt(socket: &UdpSocket) -> Option<(libc::c_int, libc::c_int)> {
        match socket.local_addr().ok()? {
            SocketAddr::V4(_) => Some((libc::IPPROTO_IP, libc::IP_TOS)),
            SocketAddr::V6(_) => Some((libc::IPPROTO_IPV6, libc::IPV6_TCLASS)),
        }
    }

    fn set_int(socket: &UdpSocket, level: libc::c_int, name: libc::c_int, value: i32) -> io::Result<()> {
        // SAFETY: `socket` owns this descriptor for the call. `setsockopt` reads exactly
        // `OPT_LEN` (4) bytes from `value`, which is a live `i32`.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                name,
                std::ptr::from_ref(&value).cast::<libc::c_void>(),
                OPT_LEN,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn get_int(socket: &UdpSocket, level: libc::c_int, name: libc::c_int) -> Option<i32> {
        let mut value = 0_i32;
        let mut len = OPT_LEN;
        // SAFETY: `socket` owns this descriptor for the call. `getsockopt` writes at most
        // `len` bytes into `value` (a live `i32`) and updates `len`.
        let rc = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                level,
                name,
                std::ptr::from_mut(&mut value).cast::<libc::c_void>(),
                &mut len,
            )
        };
        (rc == 0 && len == OPT_LEN).then_some(value)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::net::{SocketAddr, UdpSocket};
    use std::os::windows::io::AsRawSocket;

    use super::{nonneg_usize, sock_int};
    use crate::socket::DSCP_EF_TOS;
    use windows_sys::Win32::Networking::WinSock::{
        IP_TOS, IPPROTO_IP, IPPROTO_IPV6, IPV6_TCLASS, SO_RCVBUF, SO_SNDBUF, SOCKET, SOL_SOCKET,
        WSAGetLastError, getsockopt, setsockopt,
    };

    const OPT_LEN: i32 = 4;

    pub(super) fn set_buffer(socket: &UdpSocket, receive: bool, bytes: usize) -> io::Result<()> {
        let value = sock_int(bytes)?;
        let name = if receive { SO_RCVBUF } else { SO_SNDBUF };
        set_int(socket, SOL_SOCKET, name, value)
    }

    pub(super) fn get_buffer(socket: &UdpSocket, receive: bool) -> Option<usize> {
        let name = if receive { SO_RCVBUF } else { SO_SNDBUF };
        get_int(socket, SOL_SOCKET, name).and_then(nonneg_usize)
    }

    pub(super) fn mark_dscp(socket: &UdpSocket) -> bool {
        let Ok(value) = i32::try_from(DSCP_EF_TOS) else {
            return false;
        };
        let Some((level, name)) = dscp_opt(socket) else {
            return false;
        };
        if set_int(socket, level, name, value).is_err() {
            return false;
        }
        match get_int(socket, level, name) {
            Some(got) => u32::try_from(got).ok() == Some(DSCP_EF_TOS),
            None => true,
        }
    }

    fn dscp_opt(socket: &UdpSocket) -> Option<(i32, i32)> {
        match socket.local_addr().ok()? {
            SocketAddr::V4(_) => Some((IPPROTO_IP, IP_TOS)),
            SocketAddr::V6(_) => Some((IPPROTO_IPV6, IPV6_TCLASS)),
        }
    }

    fn raw_socket(socket: &UdpSocket) -> SOCKET {
        // `RawSocket` is `u64`. `windows-sys`'s `SOCKET` is `usize`. A handle that does not
        // fit is not a socket we can configure; the caller treats the error as best-effort.
        usize::try_from(socket.as_raw_socket()).unwrap_or(0)
    }

    fn set_int(socket: &UdpSocket, level: i32, name: i32, value: i32) -> io::Result<()> {
        let raw = raw_socket(socket);
        // SAFETY: `raw` is the `SOCKET` of a live `UdpSocket` this process owns, or 0 when the
        // handle did not fit (the call then fails and we report the error). `setsockopt` reads
        // exactly `OPT_LEN` (4) bytes from `value`, a live `i32`. `WSAGetLastError` reads the
        // calling thread's last Winsock error and takes no pointer.
        let failed = unsafe {
            let rc = setsockopt(raw, level, name, std::ptr::from_ref(&value).cast::<u8>(), OPT_LEN);
            if rc == 0 { None } else { Some(WSAGetLastError()) }
        };
        match failed {
            None => Ok(()),
            Some(code) => Err(io::Error::from_raw_os_error(code)),
        }
    }

    fn get_int(socket: &UdpSocket, level: i32, name: i32) -> Option<i32> {
        let raw = raw_socket(socket);
        let mut value = 0_i32;
        let mut len = OPT_LEN;
        // SAFETY: `raw` is the `SOCKET` of a live `UdpSocket` this process owns, or 0 when the
        // handle did not fit. `getsockopt` writes at most `len` bytes into `value` (a live `i32`)
        // and updates `len`.
        let rc = unsafe {
            getsockopt(
                raw,
                level,
                name,
                std::ptr::from_mut(&mut value).cast::<u8>(),
                &mut len,
            )
        };
        (rc == 0 && len == OPT_LEN).then_some(value)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    use std::net::UdpSocket;

    pub(super) fn set_buffer(_socket: &UdpSocket, _receive: bool, bytes: usize) -> io::Result<()> {
        let _ = super::sock_int(bytes)?;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "socket buffers are not set on this target",
        ))
    }

    pub(super) fn get_buffer(_socket: &UdpSocket, _receive: bool) -> Option<usize> {
        None
    }

    pub(super) fn mark_dscp(_socket: &UdpSocket) -> bool {
        false
    }
}
