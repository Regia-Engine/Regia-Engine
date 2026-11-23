// SPDX-License-Identifier: MPL-2.0
//! LAN discovery announcements.
//!
//! A server that wants to be found on the local network broadcasts a small [`ServerAnnounce`]
//! every couple of seconds to [`DISCOVERY_PORT`]; a browser listens there and lists what it hears.
//! The layout has its own magic and format byte, independent of the game protocol, so an older
//! client still *sees* a newer server (the handshake then refuses the mismatch with a reason).
//! The game's own card (map name, mode, ...) rides along as opaque bytes.
//!
//! [`DiscoveryConfig`] chooses the magic and the port. The defaults stay [`ANNOUNCE_MAGIC`] and
//! [`DISCOVERY_PORT`]. Two announcers can share one port and still ignore each other's magic.
//! [`LanSocket`] is the UDP socket: it binds with `SO_REUSEADDR` and publishes a broadcast, and
//! [`ServerAnnounce::decode_with`] drops any datagram whose magic is not its own.
//! [`DiscoveryHub`] is the in-process bus the tests use for the same filter.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::wire::{WireError, WireReader, WireWriter};

/// Default discovery port.
pub const DISCOVERY_PORT: u16 = 7900;
/// Magic prefix of an announcement.
pub const ANNOUNCE_MAGIC: [u8; 4] = *b"RGNA";
/// Announcement layout version.
pub const ANNOUNCE_FORMAT: u8 = 1;
/// Longest server name.
pub const ANNOUNCE_NAME_BYTES: usize = 48;
/// Largest game card.
pub const ANNOUNCE_CARD_BYTES: usize = 256;
/// Datagrams one listener on an in-process [`DiscoveryHub`] will hold. A publish from a listener
/// that has not polled fails at this cap. A listener that polls drains its queue, so a steady
/// announce does not fill it. A listener that never polls stops receiving; the others still do.
const HUB_QUEUE_CAP: usize = 1024;

/// Magic and UDP port of a LAN announcer.
///
/// The defaults are [`ANNOUNCE_MAGIC`] (`RGNA`) and [`DISCOVERY_PORT`] (UDP 7900). A game that
/// needs its own announce stream sets a different magic, a different port, or both. The port is
/// where the announcer binds; it is not a field of the datagram (that datagram already carries
/// the game port).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryConfig {
    /// Four-byte prefix of every datagram this announcer sends and accepts.
    pub magic: [u8; 4],
    /// UDP port the announcer binds. Listeners on another port do not hear it.
    pub port: u16,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            magic: ANNOUNCE_MAGIC,
            port: DISCOVERY_PORT,
        }
    }
}

/// One server's announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAnnounce {
    /// The server's protocol id (`crate::payload::protocol_id`); browsers grey out mismatches.
    pub protocol_id: u64,
    /// The game port to connect to.
    pub game_port: u16,
    /// Connected peers.
    pub players: u16,
    /// Capacity.
    pub max_players: u16,
    /// True when a password is required.
    pub password: bool,
    /// Display name.
    pub name: String,
    /// The game's opaque card.
    pub card: Vec<u8>,
}

impl ServerAnnounce {
    /// The datagram with the default magic; fails when the name or card is too long.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        self.encode_with(&ANNOUNCE_MAGIC)
    }

    /// The datagram prefixed with `magic`; fails when the name or card is too long.
    pub fn encode_with(&self, magic: &[u8; 4]) -> Result<Vec<u8>, WireError> {
        let mut w = WireWriter::with_capacity(64 + self.card.len());
        w.raw(magic);
        w.u8(ANNOUNCE_FORMAT);
        w.u64(self.protocol_id);
        w.u16(self.game_port);
        w.u16(self.players);
        w.u16(self.max_players);
        w.u8(u8::from(self.password));
        w.str_u8(&self.name, ANNOUNCE_NAME_BYTES)?;
        w.bytes_u16(&self.card, ANNOUNCE_CARD_BYTES)?;
        Ok(w.finish())
    }

    /// Decodes a datagram with the default magic; `Ok(None)` for traffic that is not an
    /// announcement or has a newer format this build cannot read.
    pub fn decode(datagram: &[u8]) -> Result<Option<Self>, WireError> {
        Self::decode_with(datagram, &ANNOUNCE_MAGIC)
    }

    /// Decodes a datagram whose first four bytes are `magic`. `Ok(None)` when the magic or the
    /// format byte does not match, so two announcers on one port ignore each other.
    pub fn decode_with(datagram: &[u8], magic: &[u8; 4]) -> Result<Option<Self>, WireError> {
        if datagram.len() < 5 || datagram[..4] != *magic || datagram[4] != ANNOUNCE_FORMAT {
            return Ok(None);
        }
        let mut r = WireReader::new(&datagram[5..]);
        let out = Self {
            protocol_id: r.u64()?,
            game_port: r.u16()?,
            players: r.u16()?,
            max_players: r.u16()?,
            password: r.u8()? != 0,
            name: r.str_u8(ANNOUNCE_NAME_BYTES)?.to_owned(),
            card: r.bytes_u16(ANNOUNCE_CARD_BYTES)?.to_vec(),
        };
        r.finish()?;
        Ok(Some(out))
    }
}

/// In-process stand-in for one LAN: every announcer bound to a port sees every datagram
/// published to that port. Each [`LanAnnouncer`] then keeps only its own magic.
///
/// [`LanSocket`] is the UDP socket for the same filter. This hub is the in-process bus: a game
/// test can run two announcers without opening a port. The datagram filter is `decode_with`
/// either way.
#[derive(Clone, Default)]
pub struct DiscoveryHub {
    inner: Arc<Mutex<HubState>>,
}

#[derive(Default)]
struct HubState {
    ports: HashMap<u16, PortBus>,
}

#[derive(Default)]
struct PortBus {
    next: u64,
    listeners: HashMap<u64, VecDeque<Vec<u8>>>,
}

impl DiscoveryHub {
    /// An empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, port: u16) -> u64 {
        let mut state = self.lock();
        let bus = state.ports.entry(port).or_default();
        let id = bus.next;
        bus.next = bus.next.wrapping_add(1);
        bus.listeners.insert(id, VecDeque::new());
        id
    }

    fn unregister(&self, port: u16, id: u64) {
        let mut state = self.lock();
        let Some(bus) = state.ports.get_mut(&port) else {
            return;
        };
        bus.listeners.remove(&id);
        if bus.listeners.is_empty() {
            state.ports.remove(&port);
        }
    }

    fn publish_datagram(&self, port: u16, listener: u64, bytes: Vec<u8>) -> Result<(), WireError> {
        let mut state = self.lock();
        let Some(bus) = state.ports.get_mut(&port) else {
            return Err(WireError::Invalid("announcer is not bound"));
        };
        if bus
            .listeners
            .get(&listener)
            .is_some_and(|queue| queue.len() >= HUB_QUEUE_CAP)
        {
            return Err(WireError::TooLong {
                len: HUB_QUEUE_CAP + 1,
                max: HUB_QUEUE_CAP,
            });
        }
        for queue in bus.listeners.values_mut() {
            if queue.len() < HUB_QUEUE_CAP {
                queue.push_back(bytes.clone());
            }
        }
        Ok(())
    }

    fn drain(&self, port: u16, listener: u64) -> Vec<Vec<u8>> {
        let mut state = self.lock();
        let Some(queue) = state
            .ports
            .get_mut(&port)
            .and_then(|bus| bus.listeners.get_mut(&listener))
        else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }
}

/// Publishes and hears [`ServerAnnounce`] datagrams on a [`DiscoveryHub`] port.
pub struct LanAnnouncer {
    hub: DiscoveryHub,
    config: DiscoveryConfig,
    listener: u64,
}

impl Drop for LanAnnouncer {
    fn drop(&mut self) {
        self.hub.unregister(self.config.port, self.listener);
    }
}

impl LanAnnouncer {
    /// Binds an announcer to `config.port` on `hub`. The port is a bus key, not a kernel bind,
    /// so two announcers may share it.
    pub fn bind(hub: DiscoveryHub, config: DiscoveryConfig) -> Result<Self, WireError> {
        let listener = hub.register(config.port);
        Ok(Self {
            hub,
            config,
            listener,
        })
    }

    /// The config this announcer was bound with.
    pub fn config(&self) -> DiscoveryConfig {
        self.config
    }

    /// Encodes `announce` with this announcer's magic and puts it on the port's bus.
    ///
    /// Fails when the announce does not fit, or when this announcer is already holding
    /// 1,024 datagrams it has not polled. Polling drains that queue. Another listener that
    /// never polls stops receiving; this publish still reaches the rest.
    pub fn publish(&self, announce: &ServerAnnounce) -> Result<(), WireError> {
        let bytes = announce.encode_with(&self.config.magic)?;
        self.hub.publish_datagram(self.config.port, self.listener, bytes)
    }

    /// Announces published to this port since the last poll whose magic is this announcer's.
    ///
    /// The announcer's own publishes are included. A datagram with another magic is skipped,
    /// not surfaced as an error. The datagrams returned here are removed from this announcer's
    /// queue.
    pub fn poll(&mut self) -> Vec<ServerAnnounce> {
        let mut out = Vec::new();
        for datagram in self.hub.drain(self.config.port, self.listener) {
            if let Ok(Some(row)) = ServerAnnounce::decode_with(&datagram, &self.config.magic) {
                out.push(row);
            }
        }
        out
    }
}

/// Why [`LanSocket::publish`] failed.
#[derive(Debug)]
pub enum LanSendError {
    /// The announce does not fit the datagram.
    Wire(WireError),
    /// The socket could not send.
    Io(io::Error),
}

impl LanSendError {
    /// Agent and log code.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wire(_) => "lan_announce_invalid",
            Self::Io(_) => "lan_send_failed",
        }
    }
}

impl std::fmt::Display for LanSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LanSendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Wire(err) => Some(err),
            Self::Io(err) => Some(err),
        }
    }
}

impl From<WireError> for LanSendError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}

/// Datagrams one [`LanSocket::poll`] reads at most (net-rev-b #14).
pub const LAN_POLL_MAX_DATAGRAMS: usize = 256;

/// A UDP announcer. Two sockets may bind one port (`SO_REUSEADDR`); each keeps only its magic.
///
/// [`Self::bind`] publishes to the IPv4 limited broadcast. [`Self::bind_sending_to`] publishes to
/// a chosen address. A loopback test uses `127.255.255.255`, which stays on the loopback interface
/// and is delivered to every socket bound to that port. A unicast to `127.0.0.1` is delivered to
/// only one of them.
pub struct LanSocket {
    socket: UdpSocket,
    config: DiscoveryConfig,
    dest: SocketAddr,
}

impl LanSocket {
    /// Binds `config.port` on every IPv4 interface and publishes to `255.255.255.255` on that port.
    pub fn bind(config: DiscoveryConfig) -> io::Result<Self> {
        let dest = SocketAddr::from(([255, 255, 255, 255], config.port));
        Self::bind_sending_to(config, dest)
    }

    /// Binds `config.port` on every IPv4 interface and publishes to `dest`.
    pub fn bind_sending_to(config: DiscoveryConfig, dest: SocketAddr) -> io::Result<Self> {
        let socket = bind_reuse_v4(config.port)?;
        Ok(Self { socket, config, dest })
    }

    /// The config this socket was bound with.
    pub fn config(&self) -> DiscoveryConfig {
        self.config
    }

    /// The address the kernel bound.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Encodes `announce` with this socket's magic and broadcasts it to the configured address.
    pub fn publish(&self, announce: &ServerAnnounce) -> Result<(), LanSendError> {
        let bytes = announce.encode_with(&self.config.magic)?;
        self.socket.send_to(&bytes, self.dest).map_err(LanSendError::Io)?;
        Ok(())
    }

    /// Announces waiting on the socket whose magic is this socket's, each with the address it
    /// came from.
    ///
    /// The announce carries only `game_port`; the host to join is the sender's IP with that port
    /// (`SocketAddr::new(from.ip(), announce.game_port)`). A datagram with another magic is
    /// skipped, not surfaced as an error. One call reads at most [`LAN_POLL_MAX_DATAGRAMS`]
    /// datagrams, kept or skipped, so a flood on the public port cannot hold the caller's thread or
    /// grow the result without bound; the rest wait for the next call. `Ok` with an empty vec means
    /// nothing was waiting.
    pub fn poll(&self) -> io::Result<Vec<(SocketAddr, ServerAnnounce)>> {
        let mut buf = [0u8; crate::socket::MAX_DATAGRAM_BYTES];
        let mut out = Vec::new();
        let mut read = 0usize;
        while read < LAN_POLL_MAX_DATAGRAMS {
            match self.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    read += 1;
                    if let Ok(Some(row)) = ServerAnnounce::decode_with(&buf[..n], &self.config.magic) {
                        out.push((from, row));
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                // Windows reports a stale ICMP unreachable as a receive error. It is not this
                // datagram, but it counts toward the cap so an error loop also ends.
                Err(err) if err.kind() == io::ErrorKind::ConnectionReset => read += 1,
                Err(err) => return Err(err),
            }
        }
        Ok(out)
    }
}

/// Binds an IPv4 UDP socket to `0.0.0.0:port` with `SO_REUSEADDR`, so a second socket can bind
/// the same port and a broadcast is delivered to each of them.
///
/// `SO_REUSEPORT` is not set. On Linux that option load-balances one datagram onto one socket
/// instead of copying it to every socket bound to the port.
fn bind_reuse_v4(port: u16) -> io::Result<UdpSocket> {
    #[cfg(unix)]
    {
        bind_reuse_unix(port)
    }
    #[cfg(windows)]
    {
        bind_reuse_windows(port)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = port;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "LanSocket binds an IPv4 UDP socket on unix and windows",
        ))
    }
}

// `std::net::UdpSocket::bind` does not set `SO_REUSEADDR` before `bind`, so two sockets cannot
// share the port. The option has to be set on the raw fd first (ADR-7989).
#[cfg(unix)]
#[allow(unsafe_code)]
fn bind_reuse_unix(port: u16) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    // SAFETY: `socket` allocates a new datagram fd, or returns -1 and allocates nothing.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let yes: libc::c_int = 1;
    // SAFETY: `fd` is the socket just created. `yes` is a live `c_int` and the length is its size.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &yes as *const libc::c_int as *const libc::c_void,
            std::mem::size_of_val(&yes) as libc::socklen_t,
        )
    };
    if rc != 0 {
        // SAFETY: `fd` is still owned here and is not used after `close`.
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a zeroed `sockaddr_in` is a valid all-zero bit pattern. Family, port and address
    // are written before the pointer is passed to `bind`.
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos"
    ))]
    {
        addr.sin_len = u8::try_from(std::mem::size_of::<libc::sockaddr_in>()).unwrap_or(u8::MAX);
    }
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr = libc::in_addr { s_addr: 0 };
    // SAFETY: `fd` is the socket just created. `addr` is an `AF_INET` address and lives for the call.
    let rc = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        // SAFETY: `fd` is still owned here and is not used after `close`.
        unsafe { libc::close(fd) };
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a bound socket this function owns. `from_raw_fd` takes that ownership.
    // A later error drops the `UdpSocket`, which closes `fd`.
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    socket.set_nonblocking(true)?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

// Same reason as `bind_reuse_unix` (ADR-7989). libc 0.2.190 does not export the WinSock constants
// or `closesocket` on Windows, so those values are the public WinSock2 numbers.
#[cfg(windows)]
#[allow(unsafe_code)]
fn bind_reuse_windows(port: u16) -> io::Result<UdpSocket> {
    use std::os::windows::io::FromRawSocket;

    // The standard library starts Winsock on the first socket. `socket` before that fails.
    drop(UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?);

    const AF_INET: libc::c_int = 2;
    const SOCK_DGRAM: libc::c_int = 2;
    const SOL_SOCKET: libc::c_int = 0xffff;
    const SO_REUSEADDR: libc::c_int = 4;
    const INVALID_SOCKET: libc::SOCKET = !0;

    // SAFETY: `socket` allocates a new datagram socket, or returns `INVALID_SOCKET`.
    let sock = unsafe { libc::socket(AF_INET, SOCK_DGRAM, 0) };
    if sock == INVALID_SOCKET {
        return Err(wsa_error());
    }
    let yes: libc::c_int = 1;
    // SAFETY: `sock` is the socket just created. `yes` is a live `c_int` and the length is its size.
    let rc = unsafe {
        libc::setsockopt(
            sock,
            SOL_SOCKET,
            SO_REUSEADDR,
            &yes as *const libc::c_int as *const libc::c_char,
            std::mem::size_of_val(&yes) as libc::c_int,
        )
    };
    if rc != 0 {
        // SAFETY: `sock` is still owned here and is not used after `closesocket`.
        unsafe { closesocket(sock) };
        return Err(wsa_error());
    }
    let port_be = port.to_be_bytes();
    let mut addr = libc::sockaddr {
        sa_family: AF_INET as libc::c_ushort,
        sa_data: [0; 14],
    };
    addr.sa_data[0] = libc::c_char::from_ne_bytes([port_be[0]]);
    addr.sa_data[1] = libc::c_char::from_ne_bytes([port_be[1]]);
    // SAFETY: `sock` is the socket just created. `addr` is an `AF_INET` address and lives for the call.
    let rc = unsafe { libc::bind(sock, &addr, std::mem::size_of::<libc::sockaddr>() as libc::c_int) };
    if rc != 0 {
        // SAFETY: `sock` is still owned here and is not used after `closesocket`.
        unsafe { closesocket(sock) };
        return Err(wsa_error());
    }
    // SAFETY: `sock` is a bound socket this function owns. `from_raw_socket` takes that ownership.
    // `libc::SOCKET` is `usize` on Windows and `RawSocket` is `u64` (net-rev-b #1).
    let socket = unsafe { UdpSocket::from_raw_socket(sock as std::os::windows::io::RawSocket) };
    socket.set_nonblocking(true)?;
    socket.set_broadcast(true)?;
    Ok(socket)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn wsa_error() -> io::Error {
    // SAFETY: `WSAGetLastError` reads this thread's last Winsock error and takes no pointers.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

#[cfg(windows)]
#[allow(unsafe_code)]
#[link(name = "ws2_32")]
// SAFETY: `closesocket` and `WSAGetLastError` are the WinSock2 functions of those names.
// `closesocket` is called only with a socket this module opened and still owns.
unsafe extern "system" {
    fn closesocket(s: libc::SOCKET) -> i32;
    fn WSAGetLastError() -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_round_trip_and_rejects() {
        let a = ServerAnnounce {
            protocol_id: 42,
            game_port: 7777,
            players: 3,
            max_players: 8,
            password: true,
            name: "Friday match".into(),
            card: b"map=dunes".to_vec(),
        };
        let bytes = a.encode().expect("fits");
        assert_eq!(ServerAnnounce::decode(&bytes), Ok(Some(a.clone())));
        assert_eq!(ServerAnnounce::decode(b"hello"), Ok(None));
        let mut newer = bytes.clone();
        newer[4] = 2;
        assert_eq!(
            ServerAnnounce::decode(&newer),
            Ok(None),
            "newer formats are skipped, not misread"
        );
        assert!(ServerAnnounce::decode(&bytes[..bytes.len() - 1]).is_err());
        let long = ServerAnnounce {
            card: vec![0; ANNOUNCE_CARD_BYTES + 1],
            ..a
        };
        assert!(long.encode().is_err());
    }

    #[test]
    fn a_foreign_magic_is_invisible_to_the_default_decoder() {
        let row = ServerAnnounce {
            protocol_id: 1,
            game_port: 9,
            players: 0,
            max_players: 1,
            password: false,
            name: "other".into(),
            card: Vec::new(),
        };
        let foreign = row.encode_with(b"IRNR").expect("fits");
        assert_eq!(ServerAnnounce::decode(&foreign), Ok(None));
        assert_eq!(ServerAnnounce::decode_with(&foreign, b"IRNR"), Ok(Some(row)));
        assert_eq!(DiscoveryConfig::default().magic, ANNOUNCE_MAGIC);
        assert_eq!(DiscoveryConfig::default().port, DISCOVERY_PORT);
    }

    #[test]
    fn lan_send_error_codes() {
        let wire = LanSendError::Wire(WireError::TooLong { len: 2, max: 1 });
        assert_eq!(wire.as_str(), "lan_announce_invalid");
        let io_err = LanSendError::Io(io::Error::other("down"));
        assert_eq!(io_err.as_str(), "lan_send_failed");
    }

    /// Two sockets, one ephemeral port, broadcast to `127.255.255.255` (loopback only).
    fn loopback_pair(magic_a: [u8; 4], magic_b: [u8; 4]) -> (LanSocket, LanSocket) {
        let mut alpha = LanSocket::bind_sending_to(
            DiscoveryConfig {
                magic: magic_a,
                port: 0,
            },
            SocketAddr::from(([127, 255, 255, 255], 9)),
        )
        .expect("bind alpha");
        let port = alpha.local_addr().expect("alpha port").port();
        let dest = SocketAddr::from(([127, 255, 255, 255], port));
        alpha.dest = dest;
        let beta =
            LanSocket::bind_sending_to(DiscoveryConfig { magic: magic_b, port }, dest).expect("bind beta");
        (alpha, beta)
    }

    fn heard_names(rows: &[(SocketAddr, ServerAnnounce)]) -> Vec<&str> {
        rows.iter().map(|(_, row)| row.name.as_str()).collect()
    }

    /// Review net-rev-b #14: the announce carries only `game_port`, so a browser needs the sender
    /// to know which host to join. Each polled announce comes with the address it came from.
    #[test]
    fn poll_names_the_sender_of_each_announce() {
        let receiver = LanSocket::bind_sending_to(
            DiscoveryConfig {
                magic: ANNOUNCE_MAGIC,
                port: 0,
            },
            SocketAddr::from(([127, 0, 0, 1], 9)),
        )
        .expect("bind receiver");
        let port = receiver.local_addr().expect("receiver port").port();
        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
        let from = sender.local_addr().expect("sender address");
        let mut announce = row("host");
        announce.game_port = 7777;
        sender
            .send_to(
                &announce.encode().expect("fits"),
                SocketAddr::from(([127, 0, 0, 1], port)),
            )
            .expect("send");
        let mut heard = Vec::new();
        // A loopback datagram is queued before `send_to` returns; a few polls cover a slow host.
        for _ in 0..100 {
            heard = receiver.poll().expect("poll");
            if !heard.is_empty() {
                break;
            }
        }
        assert_eq!(
            heard,
            vec![(from, announce.clone())],
            "the sender comes with the announce"
        );
        let (sender_addr, row) = &heard[0];
        assert_eq!(
            SocketAddr::new(sender_addr.ip(), row.game_port),
            SocketAddr::from(([127, 0, 0, 1], 7777)),
            "the host to join is the sender's IP and the announced game port"
        );
    }

    #[test]
    fn two_sockets_on_one_port_keep_only_their_own_magic() {
        let (alpha, beta) = loopback_pair(*b"RGNA", *b"IRNR");
        alpha.publish(&row("alpha")).expect("publish alpha");
        beta.publish(&row("beta")).expect("publish beta");
        let polled_alpha = alpha.poll().expect("poll alpha");
        let polled_beta = beta.poll().expect("poll beta");
        let heard_alpha = heard_names(&polled_alpha);
        let heard_beta = heard_names(&polled_beta);
        assert!(
            heard_alpha.contains(&"alpha"),
            "alpha hears itself, got {heard_alpha:?}"
        );
        assert!(
            !heard_alpha.contains(&"beta"),
            "alpha drops a foreign magic, got {heard_alpha:?}"
        );
        assert!(
            heard_beta.contains(&"beta"),
            "beta hears itself, got {heard_beta:?}"
        );
        assert!(
            !heard_beta.contains(&"alpha"),
            "beta drops a foreign magic, got {heard_beta:?}"
        );
    }

    #[test]
    fn two_sockets_on_one_port_with_the_same_magic_both_hear_both() {
        let (alpha, beta) = loopback_pair(*b"RGNA", *b"RGNA");
        alpha.publish(&row("alpha")).expect("publish alpha");
        beta.publish(&row("beta")).expect("publish beta");
        let polled_alpha = alpha.poll().expect("poll alpha");
        let polled_beta = beta.poll().expect("poll beta");
        let heard_alpha = heard_names(&polled_alpha);
        let heard_beta = heard_names(&polled_beta);
        assert!(
            heard_alpha.contains(&"alpha") && heard_alpha.contains(&"beta"),
            "same magic delivers a copy to each socket, alpha got {heard_alpha:?}"
        );
        assert!(
            heard_beta.contains(&"alpha") && heard_beta.contains(&"beta"),
            "same magic delivers a copy to each socket, beta got {heard_beta:?}"
        );
    }

    /// Review net-rev-b #14: the magic is public and the port is fixed, so a flood of valid
    /// announces must not be read in one unbounded loop. One poll reads at most 256 datagrams; the
    /// rest wait for the next poll. Linux only: the receive buffer is raised so the flood is queued
    /// whole, and loopback broadcast and buffer sizes differ on other hosts.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn a_flood_of_valid_announces_is_read_at_most_256_per_poll() {
        use std::os::fd::AsRawFd;

        let receiver = LanSocket::bind_sending_to(
            DiscoveryConfig {
                magic: ANNOUNCE_MAGIC,
                port: 0,
            },
            SocketAddr::from(([127, 0, 0, 1], 9)),
        )
        .expect("bind receiver");
        let big: libc::c_int = 4 << 20;
        // SAFETY: the fd is the receiver's live socket; `big` is a live `c_int` and the length is
        // its size.
        let rc = unsafe {
            libc::setsockopt(
                receiver.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                &big as *const libc::c_int as *const libc::c_void,
                std::mem::size_of_val(&big) as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "raise the receive buffer");
        let port = receiver.local_addr().expect("receiver port").port();
        let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
        let bytes = row("flood").encode().expect("fits");
        const SENT: usize = 300;
        for _ in 0..SENT {
            sender
                .send_to(&bytes, SocketAddr::from(([127, 0, 0, 1], port)))
                .expect("send");
        }
        let first = receiver.poll().expect("poll").len();
        let mut total = first;
        for _ in 0..4 {
            total += receiver.poll().expect("poll").len();
        }
        assert!(
            total > LAN_POLL_MAX_DATAGRAMS,
            "the flood was queued past the cap, total {total}"
        );
        assert_eq!(
            first, LAN_POLL_MAX_DATAGRAMS,
            "one poll reads at most the cap, the rest wait (total {total})"
        );
        assert_eq!(total, SENT, "nothing is lost between polls");
    }

    fn row(name: &str) -> ServerAnnounce {
        ServerAnnounce {
            protocol_id: 1,
            game_port: 9,
            players: 0,
            max_players: 1,
            password: false,
            name: name.into(),
            card: Vec::new(),
        }
    }
}
