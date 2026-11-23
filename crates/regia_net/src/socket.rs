// SPDX-License-Identifier: MPL-2.0
//! Datagram sockets: real UDP and an in-memory loopback network for tests.
//!
//! Endpoints talk to a [`DatagramSocket`]; nothing above this module knows whether the bytes go
//! through the operating system or through a [`LoopbackNet`]. Both are non-blocking: `recv_from`
//! returns `Ok(None)` when nothing is waiting.
//!
//! Portions copied from Direct Order 27 (ir-net/src/socket.rs, commit f7e8f4740), ADR-8012: the
//! socket-buffer and DSCP setup of `UdpTransport` (ADR-7998).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

mod platform;

/// Largest datagram any `regia_net` endpoint reads.
pub const MAX_DATAGRAM_BYTES: usize = 2048;

/// Default `SO_RCVBUF` request: 2 MiB. The kernel may grant less (`rmem_max`).
pub const DEFAULT_RECV_BUFFER_BYTES: usize = 2_097_152;

/// Default `SO_SNDBUF` request: 512 KiB. The kernel may grant less (`wmem_max`).
pub const DEFAULT_SEND_BUFFER_BYTES: usize = 524_288;

/// Largest buffer size [`crate`] agents may request for one socket: 64 MiB.
pub const MAX_SOCKET_BUFFER_BYTES: usize = 67_108_864;

/// IPv4 TOS byte for DSCP EF (codepoint 46 shifted left by two, RFC 3246 and RFC 2474).
/// IPv6 carries the same value in `IPV6_TCLASS`.
pub const DSCP_EF_TOS: u32 = 0xB8;

/// Requested UDP socket buffers and whether to mark packets DSCP EF.
///
/// The sizes are requests. [`SocketBufferReport`] is what the kernel granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketBufferSettings {
    /// `SO_RCVBUF` request, in bytes.
    pub recv_bytes: usize,
    /// `SO_SNDBUF` request, in bytes.
    pub send_bytes: usize,
    /// When true, set the DSCP EF mark (`0xB8`) on the bound family. Best-effort.
    pub dscp_ef: bool,
}

impl Default for SocketBufferSettings {
    fn default() -> Self {
        Self {
            recv_bytes: DEFAULT_RECV_BUFFER_BYTES,
            send_bytes: DEFAULT_SEND_BUFFER_BYTES,
            dscp_ef: true,
        }
    }
}

/// Kernel read-back of one socket's buffer sizes, plus whether DSCP EF was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketBufferReport {
    /// `SO_RCVBUF` as `getsockopt` reported it. `0` when the read-back failed.
    pub confirmed_recv: usize,
    /// `SO_SNDBUF` as `getsockopt` reported it. `0` when the read-back failed.
    pub confirmed_send: usize,
    /// The receive size that was requested.
    pub requested_recv: usize,
    /// The send size that was requested.
    pub requested_send: usize,
    /// The bound family accepted the DSCP EF sockopt and the read-back matches [`DSCP_EF_TOS`],
    /// or the set succeeded and the read-back itself failed.
    pub dscp_ef_applied: bool,
    /// Both buffer `getsockopt` calls succeeded.
    pub read_back: bool,
    /// The socket's packets carry don't-fragment (ADR-8893; RFC 9000 § 14, RFC 8899 § 3): the
    /// platform option was set and reads back on (`IP_MTU_DISCOVER` = `IP_PMTUDISC_PROBE` on Linux
    /// and Android, `IP_DONTFRAGMENT` on Windows, `IP_DONTFRAG` on macOS; the IPv6 options on an
    /// IPv6 socket). False where the platform refused it: IPv4 datagrams may then be fragmented.
    pub dont_fragment: bool,
}

impl SocketBufferReport {
    /// Both grants are at least the request. On Linux the kernel usually reports twice the
    /// granted size, which still passes.
    #[must_use]
    pub const fn meets_request(&self) -> bool {
        self.confirmed_recv >= self.requested_recv && self.confirmed_send >= self.requested_send
    }

    /// Either grant is strictly below half the request (integer division). A warning, not a
    /// failed bind.
    #[must_use]
    pub const fn below_half_request(&self) -> bool {
        self.confirmed_recv < self.requested_recv / 2 || self.confirmed_send < self.requested_send / 2
    }

    /// One line with the requested sizes, the kernel grant, and the warning flags.
    #[must_use]
    pub fn diagnostic_line(&self) -> String {
        let recv = self.confirmed_recv;
        let req_recv = self.requested_recv;
        let send = self.confirmed_send;
        let req_send = self.requested_send;
        let meets = self.meets_request();
        let below = self.below_half_request();
        let dscp = self.dscp_ef_applied;
        let read_back = self.read_back;
        let dont_fragment = self.dont_fragment;
        format!(
            "udp socket buffers: recv={recv} (requested {req_recv}) send={send} (requested {req_send}) meets_request={meets} below_half={below} dscp_ef={dscp} read_back={read_back} dont_fragment={dont_fragment}"
        )
    }
}

/// A non-blocking datagram socket.
pub trait DatagramSocket {
    /// Sends one datagram. `Ok(())` means the datagram left (the operating system or the network
    /// took it); a datagram that did not leave is an error, `WouldBlock` for a full send buffer,
    /// so the endpoint counts it in `socket_drops` and does not spend a packet number on it
    /// (spec 120 § 4.3). A transport may absorb transient errors the way [`UdpTransport`] does:
    /// retry a stale ICMP `ConnectionReset` once and retry `Interrupted`.
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()>;
    /// Receives one datagram into `buf`: `Ok(None)` when nothing is waiting.
    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>>;
    /// Payload budget for packets framed for `dest`, including Regia's endpoint header.
    /// The default is the receive-buffer ceiling. A transport with encapsulation or a peer
    /// limit overrides it; a conservative budget may be smaller than a discovered path MTU.
    /// Budgets below 256 bytes cannot carry this connection protocol. A size refusal must
    /// leave this value smaller than the refused payload so the endpoint can shrink and
    /// reschedule uncommitted work.
    fn max_datagram_size(&self, _dest: SocketAddr) -> usize {
        MAX_DATAGRAM_BYTES
    }
    /// The bound local address.
    fn local_addr(&self) -> io::Result<SocketAddr>;
    /// What the operating system gave the socket (buffer sizes read back, the DSCP mark; spec
    /// 120 § 4.4): `net.status` reports it. `None` for a transport without an OS socket of its
    /// own (the in-memory network, a relay or a vendor transport).
    fn socket_report(&self) -> Option<SocketReport> {
        None
    }
}

/// Receive buffer a [`UdpTransport`] asks for: 2 MiB (spec 120 § 4.4, DO27 rule A8), room for a
/// burst of about 1,800 full datagrams while a frame is busy.
pub const UDP_RECV_BUFFER_BYTES: usize = 2 * 1024 * 1024;
/// Send buffer a [`UdpTransport`] asks for: 512 KiB.
pub const UDP_SEND_BUFFER_BYTES: usize = 512 * 1024;
/// The DSCP a [`UdpTransport`] marks its packets with: Expedited Forwarding (46, the `0xB8`
/// traffic-class byte; RFC 3246), best effort.
pub const DSCP_EXPEDITED_FORWARDING: u8 = 46;

/// What the operating system gave a UDP socket (spec 120 § 4.4, ADR-8407): the buffer sizes the
/// transport asked for and what the kernel reports back, and whether the DSCP mark was taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocketReport {
    /// Receive buffer asked for, bytes.
    pub recv_buffer_requested: usize,
    /// Receive buffer the kernel reports, bytes (Linux reports twice what it was given, its
    /// bookkeeping share included); `0` when it could not be read.
    pub recv_buffer_bytes: usize,
    /// Send buffer asked for, bytes.
    pub send_buffer_requested: usize,
    /// Send buffer the kernel reports, bytes; `0` when it could not be read.
    pub send_buffer_bytes: usize,
    /// The DSCP the socket marks its packets with, `None` when the platform refused it.
    /// Some platforms (Windows without a QoS policy) take the option and ignore it.
    pub dscp: Option<u8>,
    /// Either buffer read back under half of what was asked: the host's limits are low
    /// (`net.core.rmem_max` on Linux) and bursts may drop. The transport warns once per process.
    pub undersized: bool,
}

impl SocketReport {
    /// The report of a socket set up by `UdpTransport` (ADR-7998's sockopts): the requests and
    /// the read-back of `buffers`, DSCP EF where it was accepted (ADR-8572).
    #[must_use]
    pub fn from_buffers(buffers: &SocketBufferReport) -> Self {
        Self {
            recv_buffer_requested: buffers.requested_recv,
            recv_buffer_bytes: buffers.confirmed_recv,
            send_buffer_requested: buffers.requested_send,
            send_buffer_bytes: buffers.confirmed_send,
            dscp: buffers.dscp_ef_applied.then_some(DSCP_EXPEDITED_FORWARDING),
            undersized: buffers.confirmed_recv < buffers.requested_recv / 2
                || buffers.confirmed_send < buffers.requested_send / 2,
        }
    }

    /// Asks the operating system for the buffers and the DSCP mark of `socket` and reads back what
    /// it gave. Failures are reported in the result, never returned: a socket without them still
    /// works.
    pub fn configure(socket: &UdpSocket) -> Self {
        use platform::Buffer;
        let mut report = Self {
            recv_buffer_requested: UDP_RECV_BUFFER_BYTES,
            send_buffer_requested: UDP_SEND_BUFFER_BYTES,
            ..Self::default()
        };
        for (which, bytes) in [
            (Buffer::Receive, UDP_RECV_BUFFER_BYTES),
            (Buffer::Send, UDP_SEND_BUFFER_BYTES),
        ] {
            if let Err(error) = platform::set_buffer(socket, which, bytes) {
                tracing::debug!(%error, ?which, "regia_net socket buffer not set");
            }
            let got = platform::read_buffer(socket, which).unwrap_or(0);
            match which {
                Buffer::Receive => report.recv_buffer_bytes = got,
                Buffer::Send => report.send_buffer_bytes = got,
            }
        }
        let ipv6 = socket.local_addr().is_ok_and(|a| a.is_ipv6());
        report.dscp = platform::set_dscp(socket, ipv6, DSCP_EXPEDITED_FORWARDING)
            .ok()
            .map(|()| DSCP_EXPEDITED_FORWARDING);
        report.undersized = report.recv_buffer_bytes < report.recv_buffer_requested / 2
            || report.send_buffer_bytes < report.send_buffer_requested / 2;
        report
    }
}

/// The once-per-process warning for undersized socket buffers.
static WARNED_UNDERSIZED: AtomicBool = AtomicBool::new(false);

fn warn_once_if_undersized(report: &SocketReport) {
    if report.undersized && !WARNED_UNDERSIZED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "regia_net",
            recv_asked = report.recv_buffer_requested,
            recv_got = report.recv_buffer_bytes,
            send_asked = report.send_buffer_requested,
            send_got = report.send_buffer_bytes,
            "UDP socket buffers are under half of what was asked: bursts may be dropped by the \
             host (raise net.core.rmem_max / wmem_max on Linux)"
        );
    }
}

/// A boxed socket is a socket, so a transport chosen at run time (`Box<dyn DatagramSocket + Send>`,
/// spec 89 § 2) plugs into the generic endpoints.
impl<T: DatagramSocket + ?Sized> DatagramSocket for Box<T> {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        (**self).send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        (**self).recv_from(buf)
    }

    fn max_datagram_size(&self, dest: SocketAddr) -> usize {
        (**self).max_datagram_size(dest)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        (**self).local_addr()
    }

    fn socket_report(&self) -> Option<SocketReport> {
        (**self).socket_report()
    }
}

/// Why a framed packet did not leave ([`transmit_packet`]): the connection keeps its work
/// uncommitted and its packet number unspent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxRefused {
    /// Larger than the transport carries to that peer: the largest it carries now.
    TooLarge(usize),
    /// The socket refused it (a full send buffer, `WouldBlock`, or a send error): counted in
    /// the peer's `socket_drops` (spec 120 § 4.3).
    SocketDrop,
}

/// Hand one framed packet to the conditioner/transport. The size is checked before any
/// simulated loss or delay. Immediate size refusals and socket drops leave connection work
/// uncommitted: with the conditioner off, a datagram the socket refused never left, so it
/// is [`TxRefused::SocketDrop`] rather than sent. A conditioned datagram (delayed, duplicated)
/// that the socket refuses later is counted in `socket_drops` only, like a network loss.
pub(crate) fn transmit_packet(
    socket: &mut impl DatagramSocket,
    conditioner: &mut crate::conditioner::NetConditioner,
    totals: &mut crate::status::EndpointTotals,
    bytes: Vec<u8>,
    dest: SocketAddr,
    now_ms: u64,
) -> Result<(), TxRefused> {
    let max = socket.max_datagram_size(dest);
    if bytes.len() > max {
        return Err(TxRefused::TooLarge(max));
    }
    let direct = conditioner.passes_through();
    let mut accepted = false;
    for (bytes, dest) in conditioner.process(bytes, dest, now_ms) {
        match socket.send_to(&bytes, dest) {
            Ok(()) => {
                totals.datagrams_out += 1;
                accepted = true;
            }
            Err(error) => {
                totals.socket_errors += 1;
                totals.socket_drops += 1;
                let max = socket.max_datagram_size(dest);
                if !accepted && bytes.len() > max {
                    return Err(TxRefused::TooLarge(max));
                }
                tracing::debug!(%error, %dest, "regia_net send dropped by the socket");
                if direct {
                    return Err(TxRefused::SocketDrop);
                }
            }
        }
    }
    Ok(())
}

/// A real UDP socket in non-blocking mode.
///
/// [`Self::bind`] and [`Self::from_socket`] request the default buffer sizes and the DSCP EF
/// mark, and set don't-fragment (ADR-8893), so a datagram larger than the path is refused or
/// dropped instead of fragmented: a path MTU probe that arrives was carried whole. The kernel grant
/// is [`Self::buffer_report`]. A send the operating system refuses as too large (`EMSGSIZE`,
/// `WSAEMSGSIZE`) lowers [`DatagramSocket::max_datagram_size`] toward that destination below the
/// refused size, so the endpoint reads it as a size refusal, not a socket drop.
#[derive(Debug)]
pub struct UdpTransport {
    socket: UdpSocket,
    buffers: SocketBufferReport,
    size_refusals: SizeRefusals,
}

/// The smallest datagram the operating system refused as too large, per destination.
#[derive(Debug, Default)]
struct SizeRefusals(HashMap<SocketAddr, usize>);

impl SizeRefusals {
    /// Destinations remembered at most; past it the memory starts over (a refusal is cheap to
    /// learn again: one more `EMSGSIZE`).
    const MAX_DESTINATIONS: usize = 1024;

    fn refused(&mut self, dest: SocketAddr, bytes: usize) {
        if self.0.len() >= Self::MAX_DESTINATIONS && !self.0.contains_key(&dest) {
            self.0.clear();
        }
        let smallest = self.0.entry(dest).or_insert(bytes);
        *smallest = (*smallest).min(bytes);
    }

    fn budget(&self, dest: SocketAddr) -> usize {
        self.0.get(&dest).map_or(MAX_DATAGRAM_BYTES, |&refused| {
            refused.saturating_sub(1).min(MAX_DATAGRAM_BYTES)
        })
    }
}

impl UdpTransport {
    /// Binds `addr` (use port 0 for an OS-chosen port), switches to non-blocking mode, and
    /// applies [`SocketBufferSettings::default`]. A refused sockopt does not fail the bind.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::bind_with(addr, SocketBufferSettings::default())
    }

    /// [`Self::bind`] with caller-chosen buffer requests and DSCP marking.
    pub fn bind_with(addr: SocketAddr, settings: SocketBufferSettings) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(Self::from_nonblocking(socket, settings))
    }

    /// Wraps an already bound socket, switches it to non-blocking mode, and applies
    /// [`SocketBufferSettings::default`]. Options already set on `socket` are overwritten.
    pub fn from_socket(socket: UdpSocket) -> io::Result<Self> {
        Self::from_socket_with(socket, SocketBufferSettings::default())
    }

    /// [`Self::from_socket`] with caller-chosen buffer requests and DSCP marking.
    pub fn from_socket_with(socket: UdpSocket, settings: SocketBufferSettings) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self::from_nonblocking(socket, settings))
    }

    fn from_nonblocking(socket: UdpSocket, settings: SocketBufferSettings) -> Self {
        let mut buffers = crate::socket_opts::apply(&socket, &settings);
        buffers.dont_fragment = set_dont_fragment(&socket);
        note_buffer_report(&buffers);
        warn_once_if_undersized(&SocketReport::from_buffers(&buffers));
        Self {
            socket,
            buffers,
            size_refusals: SizeRefusals::default(),
        }
    }

    /// The kernel grant as a [`SocketReport`] (spec 120 § 4.4, ADR-8407): the read-back of
    /// [`Self::buffer_report`] with the DSCP codepoint and the undersized flag.
    #[must_use]
    pub fn report(&self) -> SocketReport {
        SocketReport::from_buffers(&self.buffers)
    }

    /// The kernel grant recorded when this socket was wrapped.
    #[must_use]
    pub const fn buffer_report(&self) -> SocketBufferReport {
        self.buffers
    }

    /// The inner socket, for socket options this crate does not set.
    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }
}

/// Sets don't-fragment on `socket` (ADR-8893) and reports whether it reads back on. A refusal
/// leaves the socket up, as before: the datagrams may then be fragmented on IPv4.
pub(crate) fn set_dont_fragment(socket: &UdpSocket) -> bool {
    let ipv6 = socket.local_addr().is_ok_and(|a| a.is_ipv6());
    if let Err(error) = platform::set_dont_fragment(socket, ipv6) {
        tracing::debug!(%error, "regia_net don't-fragment not set");
        return false;
    }
    dont_fragment_is_set(socket)
}

/// Whether `socket`'s packets carry don't-fragment now.
pub(crate) fn dont_fragment_is_set(socket: &UdpSocket) -> bool {
    let ipv6 = socket.local_addr().is_ok_and(|a| a.is_ipv6());
    platform::read_dont_fragment(socket, ipv6).unwrap_or(false)
}

fn note_buffer_report(report: &SocketBufferReport) {
    let line = report.diagnostic_line();
    if report.below_half_request() {
        tracing::warn!("{line}");
    } else {
        tracing::debug!("{line}");
    }
}

/// Sends one datagram, absorbing a stale Windows ICMP port-unreachable. Windows reports an ICMP
/// unreachable drawn by an *earlier* send as `ConnectionReset` on the socket's next call, so a
/// `send_to` that returns it was never transmitted; `recv_from` skips the same error. Retry that
/// send once; a second `ConnectionReset` is returned, like a full send buffer (`WouldBlock`):
/// the datagram did not leave, and the endpoint counts it in `socket_drops` (spec 120 § 4.3).
/// `Interrupted` is retried like `recv_from` retries it.
fn send_datagram_with_icmp_retry(
    mut send: impl FnMut(&[u8]) -> io::Result<usize>,
    bytes: &[u8],
) -> io::Result<()> {
    let mut stale_reset = false;
    loop {
        match send(bytes) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset && !stale_reset => stale_reset = true,
            // A full buffer, a second stale reset or any other failure: the datagram did not leave.
            Err(e) => return Err(e),
        }
    }
}

impl DatagramSocket for UdpTransport {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let sent = send_datagram_with_icmp_retry(|b| self.socket.send_to(b, dest), bytes);
        if let Err(error) = &sent
            && platform::is_size_refusal(error)
        {
            self.size_refusals.refused(dest, bytes.len());
        }
        sent
    }

    fn max_datagram_size(&self, dest: SocketAddr) -> usize {
        self.size_refusals.budget(dest)
    }

    fn socket_report(&self) -> Option<SocketReport> {
        Some(self.report())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            match self.socket.recv_from(buf) {
                Ok((n, from)) => return Ok(Some((n, from))),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                // Windows reports an ICMP port-unreachable for an earlier send as a receive
                // error on UDP sockets; it says nothing about this datagram, so skip it.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

#[derive(Debug, Default)]
struct NetState {
    queues: HashMap<SocketAddr, VecDeque<(Vec<u8>, SocketAddr)>>,
    next_port: u16,
    delivered: u64,
    undeliverable: u64,
}

/// An in-memory network of [`LoopbackSocket`]s. Cloning shares the network. Delivery is
/// immediate and in order; put a `crate::conditioner::NetConditioner` on an endpoint for loss and
/// delay.
#[derive(Debug, Clone, Default)]
pub struct LoopbackNet {
    state: Arc<Mutex<NetState>>,
}

impl LoopbackNet {
    /// An empty network.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, NetState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Binds `addr`; port 0 picks a free port. Fails when the address is taken.
    pub fn bind(&self, addr: SocketAddr) -> io::Result<LoopbackSocket> {
        let mut state = self.lock();
        let mut addr = addr;
        if addr.port() == 0 {
            loop {
                state.next_port = state.next_port.wrapping_add(1).max(20_000);
                let candidate = SocketAddr::new(addr.ip(), state.next_port);
                if !state.queues.contains_key(&candidate) {
                    addr = candidate;
                    break;
                }
            }
        }
        if state.queues.contains_key(&addr) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "loopback address in use",
            ));
        }
        state.queues.insert(addr, VecDeque::new());
        Ok(LoopbackSocket {
            net: self.clone(),
            addr,
        })
    }

    /// Datagrams delivered so far, and datagrams sent to an unbound address.
    pub fn counters(&self) -> (u64, u64) {
        let state = self.lock();
        (state.delivered, state.undeliverable)
    }
}

/// One address on a [`LoopbackNet`]. Dropping it unbinds the address.
#[derive(Debug)]
pub struct LoopbackSocket {
    net: LoopbackNet,
    addr: SocketAddr,
}

impl DatagramSocket for LoopbackSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let mut state = self.net.lock();
        match state.queues.get_mut(&dest) {
            Some(queue) => {
                queue.push_back((bytes.to_vec(), self.addr));
                state.delivered += 1;
            }
            None => state.undeliverable += 1,
        }
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut state = self.net.lock();
        let Some((bytes, from)) = state.queues.get_mut(&self.addr).and_then(VecDeque::pop_front) else {
            return Ok(None);
        };
        // Like UDP, a datagram longer than the buffer is truncated.
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(Some((n, from)))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.addr)
    }
}

impl Drop for LoopbackSocket {
    fn drop(&mut self) {
        self.net.lock().queues.remove(&self.addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_delivers_in_order_and_unbinds_on_drop() {
        let net = LoopbackNet::new();
        let mut a = net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let b_addr = SocketAddr::from(([127, 0, 0, 1], 5000));
        let mut b = net.bind(b_addr).expect("bind");
        assert!(net.bind(b_addr).is_err(), "address in use");
        a.send_to(b"one", b_addr).expect("send");
        a.send_to(b"two", b_addr).expect("send");
        let mut buf = [0u8; 16];
        let (n, from) = b.recv_from(&mut buf).expect("io").expect("datagram");
        assert_eq!(&buf[..n], b"one");
        assert_eq!(from, a.local_addr().expect("addr"));
        let (n, _) = b.recv_from(&mut buf).expect("io").expect("datagram");
        assert_eq!(&buf[..n], b"two");
        assert!(b.recv_from(&mut buf).expect("io").is_none());
        drop(b);
        a.send_to(b"lost", b_addr).expect("send");
        assert_eq!(net.counters(), (2, 1));
    }

    #[test]
    fn send_to_does_not_fail_the_datagram_on_connection_reset() {
        // A loopback socket cannot raise WSAECONNRESET (Windows raises it for a stale ICMP
        // port-unreachable from an earlier send), so drive the helper with scripted attempts.
        // First attempt: the stale reset; second attempt: the datagram goes out.
        let attempts = std::cell::Cell::new(0_u32);
        let delivered = std::cell::Cell::new(0_usize);
        let result = send_datagram_with_icmp_retry(
            |bytes| {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "stale icmp"));
                }
                delivered.set(delivered.get() + bytes.len());
                Ok(bytes.len())
            },
            b"ping",
        );
        assert!(result.is_ok(), "a stale reset must not fail the datagram");
        assert_eq!(attempts.get(), 2, "the send is retried once");
        assert_eq!(delivered.get(), 4, "the bytes are handed to the socket once more");

        // Two resets in a row: the datagram did not leave, so it is reported like `WouldBlock`
        // and the endpoint counts a socket drop instead of a send (spec 120 § 4.3, ADR-8407).
        let attempts = std::cell::Cell::new(0_u32);
        let result = send_datagram_with_icmp_retry(
            |_| {
                attempts.set(attempts.get() + 1);
                Err(io::Error::new(io::ErrorKind::ConnectionReset, "stale icmp"))
            },
            b"ping",
        );
        assert_eq!(
            result.expect_err("a persistent stale reset did not send").kind(),
            io::ErrorKind::ConnectionReset
        );
        assert_eq!(attempts.get(), 2, "reset is retried exactly once");

        // A full send buffer: the datagram did not leave (spec 120 § 4.3; it used to be `Ok`).
        let attempts = std::cell::Cell::new(0_u32);
        let result = send_datagram_with_icmp_retry(
            |_| {
                attempts.set(attempts.get() + 1);
                Err(io::Error::new(io::ErrorKind::WouldBlock, "full"))
            },
            b"ping",
        );
        assert_eq!(
            result.expect_err("WouldBlock is not a send").kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(attempts.get(), 1, "a full buffer is not retried");

        // Interrupted retries like recv_from does, without failing the datagram.
        let attempts = std::cell::Cell::new(0_u32);
        let delivered = std::cell::Cell::new(0_usize);
        let result = send_datagram_with_icmp_retry(
            |bytes| {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "retry me"));
                }
                delivered.set(delivered.get() + bytes.len());
                Ok(bytes.len())
            },
            b"ping",
        );
        assert!(result.is_ok(), "Interrupted must not fail the datagram");
        assert_eq!(attempts.get(), 2, "Interrupted is retried");
        assert_eq!(delivered.get(), 4, "the bytes are handed to the socket once more");

        // Other errors still surface.
        let result = send_datagram_with_icmp_retry(
            |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            b"ping",
        );
        assert_eq!(
            result.expect_err("other errors still surface").kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    /// Spec 120 § 4.4 (DO27 rule A8): the transport asks for 2 MiB / 512 KiB, reads both back
    /// and reports them; the DSCP mark is taken where the platform allows it.
    #[test]
    fn udp_transport_sets_and_reads_back_its_buffers() {
        let Ok(t) = UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            // Sandboxes without a loopback interface cannot run this test.
            return;
        };
        let r = t.socket_report().expect("a UDP socket reports");
        assert_eq!(r.recv_buffer_requested, UDP_RECV_BUFFER_BYTES);
        assert_eq!(r.send_buffer_requested, UDP_SEND_BUFFER_BYTES);
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows))]
        {
            assert!(r.recv_buffer_bytes > 0 && r.send_buffer_bytes > 0, "{r:?}");
            assert_eq!(
                r.undersized,
                r.recv_buffer_bytes < UDP_RECV_BUFFER_BYTES / 2
                    || r.send_buffer_bytes < UDP_SEND_BUFFER_BYTES / 2
            );
        }
        #[cfg(target_os = "linux")]
        assert_eq!(
            r.dscp,
            Some(DSCP_EXPEDITED_FORWARDING),
            "Linux takes IP_TOS from a user"
        );
        // What the socket really holds matches the report.
        let read = platform::read_buffer(t.socket(), platform::Buffer::Receive).unwrap_or(0);
        assert_eq!(read, r.recv_buffer_bytes);
    }

    /// Don't-fragment (ADR-8893, KI-NT-028): the transport's IPv4 and IPv6 sockets hold the
    /// platform option, read back with a raw `getsockopt` that does not go through the code under
    /// test, and the report says so.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows))]
    #[test]
    fn udp_transport_sets_dont_fragment() {
        let mut checked = 0;
        for addr in [
            SocketAddr::from(([127, 0, 0, 1], 0)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
        ] {
            let Ok(t) = UdpTransport::bind(addr) else {
                // Sandboxes without that loopback cannot run this half.
                continue;
            };
            let raw = kernel_probe::dont_fragment(t.socket()).expect("read the don't-fragment option");
            assert_eq!(raw, kernel_probe::DONT_FRAGMENT_ON, "{addr}: the option holds");
            assert!(t.buffer_report().dont_fragment, "{addr}: the report says so");
            assert!(t.buffer_report().diagnostic_line().contains("dont_fragment=true"));
            checked += 1;
        }
        assert!(checked > 0, "no loopback socket could bind");
    }

    #[test]
    fn a_size_refusal_lowers_the_budget_per_destination_and_stays_bounded() {
        let a = SocketAddr::from(([192, 0, 2, 1], 7777));
        let b = SocketAddr::from(([192, 0, 2, 2], 7777));
        let mut refusals = SizeRefusals::default();
        assert_eq!(refusals.budget(a), MAX_DATAGRAM_BYTES);
        refusals.refused(a, 1452);
        assert_eq!(refusals.budget(a), 1451);
        refusals.refused(a, 1500);
        assert_eq!(refusals.budget(a), 1451, "the smallest refusal holds");
        refusals.refused(a, 1400);
        assert_eq!(refusals.budget(a), 1399);
        assert_eq!(refusals.budget(b), MAX_DATAGRAM_BYTES, "per destination");
        for port in 0..SizeRefusals::MAX_DESTINATIONS as u16 {
            refusals.refused(SocketAddr::from(([192, 0, 2, 3], port)), 1300);
        }
        assert!(refusals.0.len() <= SizeRefusals::MAX_DESTINATIONS);
    }

    #[test]
    fn udp_loopback_round_trip() {
        let Ok(mut a) = UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))) else {
            // Sandboxes without a loopback interface cannot run this test.
            return;
        };
        let mut b = UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let b_addr = b.local_addr().expect("addr");
        a.send_to(b"ping", b_addr).expect("send");
        let mut buf = [0u8; 16];
        // Bounded poll: loopback delivery is immediate on every supported OS, but give the
        // kernel a few yields rather than assume it.
        let mut got = None;
        for _ in 0..10_000 {
            if let Some(r) = b.recv_from(&mut buf).expect("io") {
                got = Some(r);
                break;
            }
            std::thread::yield_now();
        }
        let (n, _) = got.expect("datagram arrives on loopback");
        assert_eq!(&buf[..n], b"ping");
    }

    #[test]
    fn below_half_request_is_a_warning_in_the_diagnostic() {
        let low = SocketBufferReport {
            confirmed_recv: 100,
            confirmed_send: 1_000,
            requested_recv: 1_000,
            requested_send: 1_000,
            dscp_ef_applied: false,
            read_back: true,
            dont_fragment: true,
        };
        assert!(low.below_half_request());
        assert!(!low.meets_request());
        let line = low.diagnostic_line();
        assert!(line.contains("recv=100"));
        assert!(line.contains("requested 1000"));
        assert!(line.contains("below_half=true"));

        let half = SocketBufferReport {
            confirmed_recv: 500,
            confirmed_send: 500,
            ..low
        };
        assert!(
            !half.below_half_request(),
            "half the request is not strictly below"
        );
        assert!(!half.meets_request());

        let granted = SocketBufferReport {
            confirmed_recv: 2_000,
            confirmed_send: 2_000,
            dscp_ef_applied: true,
            ..low
        };
        assert!(granted.meets_request());
        assert!(!granted.below_half_request());
        assert!(granted.diagnostic_line().contains("meets_request=true"));
    }

    #[test]
    fn buffers_read_back_reported() {
        let settings = SocketBufferSettings {
            recv_bytes: 65_536,
            send_bytes: 65_536,
            dscp_ef: true,
        };
        let transport =
            UdpTransport::bind_with(SocketAddr::from(([127, 0, 0, 1], 0)), settings).expect("loopback bind");
        let report = transport.buffer_report();
        assert_eq!(report.requested_recv, 65_536);
        assert_eq!(report.requested_send, 65_536);
        assert!(report.read_back, "getsockopt of both buffers");
        let probe = kernel_probe::read(transport.socket()).expect("independent getsockopt");
        assert_eq!(report.confirmed_recv, probe.recv);
        assert_eq!(report.confirmed_send, probe.send);
        if probe.recv == 65_536 * 2 {
            assert_eq!(report.confirmed_recv, 65_536 * 2);
            assert_ne!(report.confirmed_recv, settings.recv_bytes);
        }
        if probe.send == 65_536 * 2 {
            assert_eq!(report.confirmed_send, 65_536 * 2);
            assert_ne!(report.confirmed_send, settings.send_bytes);
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(probe.recv, 65_536 * 2, "Linux doubles SO_RCVBUF");
            assert_eq!(probe.send, 65_536 * 2, "Linux doubles SO_SNDBUF");
            assert!(report.dscp_ef_applied, "Linux loopback accepted IP_TOS 0xB8");
            assert_eq!(probe.dscp, DSCP_EF_TOS);
        }
    }

    #[test]
    fn dscp_off_is_not_applied_and_defaults_are_recorded() {
        let transport = UdpTransport::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let defaults = transport.buffer_report();
        assert_eq!(defaults.requested_recv, DEFAULT_RECV_BUFFER_BYTES);
        assert_eq!(defaults.requested_send, DEFAULT_SEND_BUFFER_BYTES);
        assert!(defaults.read_back);

        let raw = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind raw");
        let wrapped = UdpTransport::from_socket_with(
            raw,
            SocketBufferSettings {
                recv_bytes: 65_536,
                send_bytes: 65_536,
                dscp_ef: false,
            },
        )
        .expect("from_socket");
        let report = wrapped.buffer_report();
        assert!(!report.dscp_ef_applied);
        assert_eq!(report.requested_recv, 65_536);
        let probe = kernel_probe::read(wrapped.socket()).expect("independent getsockopt");
        assert_eq!(report.confirmed_recv, probe.recv);
        assert_eq!(report.confirmed_send, probe.send);
    }

    /// Direct `getsockopt`. Not the production helper in `socket_opts`.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    mod kernel_probe {
        use std::io;
        #[cfg(target_os = "linux")]
        use std::net::SocketAddr;
        use std::net::UdpSocket;
        use std::os::fd::AsRawFd;

        const OPT_LEN: libc::socklen_t = 4;

        /// What the don't-fragment option reads when it is on: `IP_PMTUDISC_PROBE` on Linux and
        /// Android, 1 for Apple's `IP_DONTFRAG`.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        pub(super) const DONT_FRAGMENT_ON: usize = libc::IP_PMTUDISC_PROBE as usize;
        #[cfg(target_vendor = "apple")]
        pub(super) const DONT_FRAGMENT_ON: usize = 1;

        /// The family's don't-fragment option as the kernel holds it.
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        pub(super) fn dont_fragment(socket: &UdpSocket) -> io::Result<usize> {
            let v6 = socket.local_addr()?.is_ipv6();
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let (level, name) = if v6 {
                (libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER)
            } else {
                (libc::IPPROTO_IP, libc::IP_MTU_DISCOVER)
            };
            // `netinet/in.h` IP_DONTFRAG 28, `netinet6/in6.h` IPV6_DONTFRAG 62.
            #[cfg(target_vendor = "apple")]
            let (level, name) = if v6 {
                (libc::IPPROTO_IPV6, 62)
            } else {
                (libc::IPPROTO_IP, 28)
            };
            get(socket, level, name)
        }

        pub(super) struct Probe {
            pub recv: usize,
            pub send: usize,
            /// The TOS / traffic-class byte; only Linux's loopback test asserts it.
            #[cfg(target_os = "linux")]
            pub dscp: u32,
        }

        pub(super) fn read(socket: &UdpSocket) -> io::Result<Probe> {
            let recv = get(socket, libc::SOL_SOCKET, libc::SO_RCVBUF)?;
            let send = get(socket, libc::SOL_SOCKET, libc::SO_SNDBUF)?;
            #[cfg(target_os = "linux")]
            {
                let (level, name) = match socket.local_addr()? {
                    SocketAddr::V4(_) => (libc::IPPROTO_IP, libc::IP_TOS),
                    SocketAddr::V6(_) => (libc::IPPROTO_IPV6, libc::IPV6_TCLASS),
                };
                let dscp = u32::try_from(get(socket, level, name)?).unwrap_or(0);
                Ok(Probe { recv, send, dscp })
            }
            #[cfg(not(target_os = "linux"))]
            Ok(Probe { recv, send })
        }

        fn get(socket: &UdpSocket, level: libc::c_int, name: libc::c_int) -> io::Result<usize> {
            let mut value = 0_i32;
            let mut len = OPT_LEN;
            // SAFETY: `socket` owns this descriptor. `getsockopt` writes at most 4 bytes into
            // `value` and updates `len`. This probe does not call `socket_opts`.
            let rc = unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    level,
                    name,
                    std::ptr::from_mut(&mut value).cast::<libc::c_void>(),
                    &mut len,
                )
            };
            if rc != 0 || len != OPT_LEN {
                return Err(io::Error::last_os_error());
            }
            usize::try_from(value).map_err(|_| io::Error::other("negative sockopt"))
        }
    }

    #[cfg(windows)]
    #[allow(unsafe_code)]
    mod kernel_probe {
        use std::io;
        use std::net::UdpSocket;
        use std::os::windows::io::AsRawSocket;

        use windows_sys::Win32::Networking::WinSock::{SO_RCVBUF, SO_SNDBUF, SOCKET, SOL_SOCKET, getsockopt};

        const OPT_LEN: i32 = 4;

        /// `IP_DONTFRAGMENT` / `IPV6_DONTFRAG` read 1 when on.
        pub(super) const DONT_FRAGMENT_ON: usize = 1;

        /// The family's don't-fragment option as Winsock holds it.
        pub(super) fn dont_fragment(socket: &UdpSocket) -> io::Result<usize> {
            use windows_sys::Win32::Networking::WinSock::{
                IP_DONTFRAGMENT, IPPROTO_IP, IPPROTO_IPV6, IPV6_DONTFRAG,
            };
            if socket.local_addr()?.is_ipv6() {
                get(socket, IPPROTO_IPV6, IPV6_DONTFRAG)
            } else {
                get(socket, IPPROTO_IP, IP_DONTFRAGMENT)
            }
        }

        /// The buffer grants. The DSCP byte is asserted on Linux only, so this probe does not read it.
        pub(super) struct Probe {
            pub recv: usize,
            pub send: usize,
        }

        pub(super) fn read(socket: &UdpSocket) -> io::Result<Probe> {
            let recv = get(socket, SOL_SOCKET, SO_RCVBUF)?;
            let send = get(socket, SOL_SOCKET, SO_SNDBUF)?;
            Ok(Probe { recv, send })
        }

        fn raw_socket(socket: &UdpSocket) -> SOCKET {
            usize::try_from(socket.as_raw_socket()).unwrap_or(0)
        }

        fn get(socket: &UdpSocket, level: i32, name: i32) -> io::Result<usize> {
            let mut value = 0_i32;
            let mut len = OPT_LEN;
            // SAFETY: `raw_socket` is the `SOCKET` of a live `UdpSocket`, or 0 when it does not
            // fit. `getsockopt` writes at most `len` bytes into `value` and updates `len`.
            // This probe does not call `socket_opts`.
            let rc = unsafe {
                getsockopt(
                    raw_socket(socket),
                    level,
                    name,
                    std::ptr::from_mut(&mut value).cast::<u8>(),
                    &mut len,
                )
            };
            if rc != 0 || len != OPT_LEN {
                return Err(io::Error::last_os_error());
            }
            usize::try_from(value).map_err(|_| io::Error::other("negative sockopt"))
        }
    }
}
