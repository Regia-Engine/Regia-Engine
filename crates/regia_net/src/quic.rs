// SPDX-License-Identifier: MPL-2.0
//! QUIC datagram socket (RFC 9221) under Regia's own reliability layer.
//!
//! One connection, bound to `127.0.0.1` only. The certificate is generated in this process and
//! pinned by its exact DER bytes: there is no file and no public CA store. Reliability, ordering
//! and the session cipher stay in `regia_net`. A dropped datagram is this socket's problem;
//! [`crate::DatagramSocket`] reports it the way a full UDP send buffer does (`Ok(())`), and the
//! reliable channel resends.
//!
//! The feature `net-quic` is off by default. Without it the status calls still answer, and no
//! QUIC crate is linked.
//!
//! Failures are [`std::io::Error`] values whose message starts with one of these codes (see
//! [`quic_error_code`]):
//!
//! - `quic_bind` — the address is not `127.0.0.1`, the socket could not bind, or the payload is
//!   larger than the peer/path payload limit or [`crate::socket::MAX_DATAGRAM_BYTES`].
//!   Oversized receives record the same code and increment `datagrams_lost`, then continue.
//! - `quic_handshake` — the QUIC handshake failed (including a certificate the peer did not pin).
//!   A handshake that gets no response uses the same code when the idle timer fires.
//! - `quic_closed` — the connection has stopped taking datagrams.
//!
//! Path MTU (spec 120 § 4.3, ADR-8893): QUIC starts at 1,200 bytes and runs its own discovery
//! (RFC 9000 § 14.3) inside the connection, so Regia's probing (`LinkConfig::mtu_probing`) is not
//! what grows a QUIC path: a Regia probe larger than QUIC's current datagram limit is refused as too
//! large. `QuicListener::set_mtu_probing` and `QuicDatagramSocket::connect_with_mtu_probing`
//! carry the `mtu_probing` choice to QUIC's discovery (off pins 1,200); `QuicListener::bind` and
//! `QuicDatagramSocket::connect` keep it on. The UDP socket under QUIC sets don't-fragment, so a
//! QUIC probe that arrives was carried whole.

use std::io;

/// Counters for the QUIC datagram sockets in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuicStatus {
    /// `true` when this build includes `net-quic`.
    pub enabled: bool,
    /// `true` when a socket is bound and its connection is still up.
    pub bound: bool,
    /// Datagrams the QUIC connection accepted (not ones this socket dropped on purpose).
    pub datagrams_sent: u64,
    /// Datagrams this socket dropped, or that QUIC refused.
    pub datagrams_lost: u64,
    /// The newest error code (`quic_bind`, `quic_handshake` or `quic_closed`), if one was recorded.
    pub last_error: Option<String>,
}

/// Whether this build includes the QUIC datagram socket.
pub fn quic_enabled() -> bool {
    cfg!(feature = "net-quic")
}

/// The stable code at the start of a QUIC socket error, or `None` when `err` is not one of them.
pub fn quic_error_code(err: &io::Error) -> Option<&'static str> {
    let msg = err.to_string();
    ["quic_bind", "quic_handshake", "quic_closed"]
        .into_iter()
        .find(|&code| msg.starts_with(code))
}

/// Status with the feature off: nothing is bound and every counter is zero.
#[cfg(not(feature = "net-quic"))]
pub fn quic_status() -> QuicStatus {
    QuicStatus {
        enabled: false,
        bound: false,
        datagrams_sent: 0,
        datagrams_lost: 0,
        last_error: None,
    }
}

#[cfg(feature = "net-quic")]
mod imp {
    use std::fmt;
    use std::future::{Future, IntoFuture};
    use std::io::{self, ErrorKind};
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Duration;

    use super::QuicStatus;
    use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};

    const ALPN: &[u8] = b"regia-quic";

    /// A loopback QUIC listener. [`Self::accept`] takes the one connection.
    pub struct QuicListener {
        udp: UdpSocket,
        local: SocketAddr,
        cert_der: Vec<u8>,
        server: quinn::ServerConfig,
    }

    #[cfg(test)]
    impl QuicListener {
        /// Test seam: the largest datagram frame this listener tells its peer it accepts.
        pub(crate) fn advertise_datagram_limit(&mut self, bytes: usize) {
            let mut transport = transport_config();
            transport.datagram_receive_buffer_size(Some(bytes));
            self.server.transport_config(Arc::new(transport));
        }
    }

    impl fmt::Debug for QuicListener {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("QuicListener")
                .field("local", &self.local)
                .field("cert_len", &self.cert_der.len())
                .finish()
        }
    }

    impl QuicListener {
        /// Binds `127.0.0.1:0` and builds the in-process certificate.
        ///
        /// Every other address returns `quic_bind` without opening a socket.
        pub fn bind(addr: SocketAddr) -> io::Result<Self> {
            if !is_localhost_v4(addr) || addr.port() != 0 {
                return Err(quic_err(
                    ErrorKind::InvalidInput,
                    "quic_bind",
                    "only 127.0.0.1:0 is accepted",
                ));
            }
            let (cert_der, pkcs8) = mint_certificate()?;
            let server = server_config(&cert_der, &pkcs8)?;
            let udp = UdpSocket::bind(addr)
                .map_err(|err| quic_err(ErrorKind::AddrNotAvailable, "quic_bind", err))?;
            note_dont_fragment(&udp);
            let local = udp
                .local_addr()
                .map_err(|err| quic_err(ErrorKind::AddrNotAvailable, "quic_bind", err))?;
            Ok(Self {
                udp,
                local,
                cert_der,
                server,
            })
        }

        /// Turns QUIC's own path MTU discovery on (the default of [`Self::bind`]) or off, which
        /// keeps the connection at 1,200 bytes: the `mtu_probing` choice of the link (ADR-8893).
        /// Call it before [`Self::accept`].
        pub fn set_mtu_probing(&mut self, on: bool) {
            self.server.transport_config(Arc::new(transport_config_for(on)));
        }

        /// The bound `127.0.0.1` address.
        pub fn local_addr(&self) -> SocketAddr {
            self.local
        }

        /// The certificate a client pins with [`QuicDatagramSocket::connect`].
        pub fn certificate_der(&self) -> &[u8] {
            &self.cert_der
        }

        /// Accepts the one connection. A handshake failure returns `quic_handshake`.
        pub fn accept(self) -> io::Result<QuicDatagramSocket> {
            let _span = tracing::info_span!("quic.accept").entered();
            let endpoint = open_endpoint(self.udp, Some(self.server))?;
            let incoming = wait_for(endpoint.accept())
                .ok_or_else(|| quic_err(ErrorKind::ConnectionRefused, "quic_handshake", "listener closed"))?;
            let connection = wait_for(incoming)
                .map_err(|err| quic_err(ErrorKind::ConnectionRefused, "quic_handshake", err))?;
            let peer = connection.remote_address();
            Ok(QuicDatagramSocket::established(
                endpoint, connection, self.local, peer,
            ))
        }
    }

    /// One QUIC connection implementing [`DatagramSocket`], with Regia's channels above it.
    pub struct QuicDatagramSocket {
        endpoint: quinn::Endpoint,
        live: Arc<Live>,
    }

    impl fmt::Debug for QuicDatagramSocket {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("QuicDatagramSocket")
                .field("local", &self.live.local)
                .field("bound", &self.live.is_bound())
                .finish()
        }
    }

    impl Drop for QuicDatagramSocket {
        fn drop(&mut self) {
            self.live.bound.store(false, Ordering::Release);
            self.live.connection.close(quinn::VarInt::from_u32(0), b"regia");
            self.endpoint.close(quinn::VarInt::from_u32(0), b"regia");
        }
    }

    impl QuicDatagramSocket {
        /// Dials a nonzero `127.0.0.1` port, pinning `cert_der` as the peer's certificate.
        ///
        /// Invalid addresses return `quic_bind` before opening a socket. Handshake and pin
        /// failures return `quic_handshake`. The local bind request is always `127.0.0.1:0`.
        pub fn connect(addr: SocketAddr, cert_der: &[u8]) -> io::Result<Self> {
            Self::connect_with_mtu_probing(addr, cert_der, true)
        }

        /// [`Self::connect`] with QUIC's own path MTU discovery on or off (off keeps the
        /// connection at 1,200 bytes): the `mtu_probing` choice of the link (ADR-8893).
        pub fn connect_with_mtu_probing(
            addr: SocketAddr,
            cert_der: &[u8],
            mtu_probing: bool,
        ) -> io::Result<Self> {
            let _span = tracing::info_span!("quic.connect").entered();
            if !is_localhost_v4(addr) || addr.port() == 0 {
                return Err(quic_err(
                    ErrorKind::InvalidInput,
                    "quic_bind",
                    "dial 127.0.0.1 with a nonzero port",
                ));
            }
            let client = client_config(cert_der, mtu_probing)?;
            let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .map_err(|err| quic_err(ErrorKind::AddrNotAvailable, "quic_bind", err))?;
            note_dont_fragment(&udp);
            let local = udp
                .local_addr()
                .map_err(|err| quic_err(ErrorKind::AddrNotAvailable, "quic_bind", err))?;
            let endpoint = open_endpoint(udp, None)?;
            let connecting = endpoint
                .connect_with(client, addr, "localhost")
                .map_err(|err| quic_err(ErrorKind::ConnectionRefused, "quic_handshake", err))?;
            let connection = wait_for(connecting)
                .map_err(|err| quic_err(ErrorKind::ConnectionRefused, "quic_handshake", err))?;
            Ok(Self::established(endpoint, connection, local, addr))
        }

        fn established(
            endpoint: quinn::Endpoint,
            connection: quinn::Connection,
            local: SocketAddr,
            peer: SocketAddr,
        ) -> Self {
            let live = Arc::new(Live::new(local, peer, connection));
            register(&live);
            Self { endpoint, live }
        }

        /// A handle that still sees this socket after it moves into a session.
        pub fn control(&self) -> QuicSocketControl {
            QuicSocketControl {
                live: Arc::clone(&self.live),
            }
        }
    }

    impl DatagramSocket for QuicDatagramSocket {
        fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
            let _span = tracing::trace_span!("quic.send").entered();
            if !self.live.is_bound() {
                return Err(self.live.error(
                    ErrorKind::ConnectionAborted,
                    "quic_closed",
                    "connection closed",
                ));
            }
            if dest != self.live.peer && dest != self.live.connection.remote_address() {
                return Err(self.live.error(
                    ErrorKind::InvalidInput,
                    "quic_bind",
                    "destination is not the connected peer",
                ));
            }
            if let Err(err) = check_payload_size(bytes.len(), self.live.connection.max_datagram_size()) {
                self.live.lost.fetch_add(1, Ordering::Relaxed);
                note_error(&self.live, "quic_bind");
                return Err(err);
            }
            if self.live.take_drop() {
                self.live.lost.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            match self.live.connection.send_datagram(bytes.to_vec().into()) {
                Ok(()) => {
                    self.live.sent.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Err(quinn::SendDatagramError::ConnectionLost(err)) => {
                    self.live.lost.fetch_add(1, Ordering::Relaxed);
                    Err(self.live.error(ErrorKind::ConnectionAborted, "quic_closed", err))
                }
                // Permanent refusals on a live connection: the payload is over the QUIC datagram
                // limit (path MTU or the peer's maximum), or the peer or this endpoint has
                // datagrams off. A retry cannot succeed, so this is an error, not a UDP-style drop;
                // the connection stays up.
                Err(err) => {
                    self.live.lost.fetch_add(1, Ordering::Relaxed);
                    Err(self.live.error(ErrorKind::InvalidInput, "quic_bind", err))
                }
            }
        }

        fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
            let _span = tracing::trace_span!("quic.receive").entered();
            receive_datagram(
                buf,
                self.live.peer,
                || {
                    // Poll only buffered datagrams; the protocol driver runs independently.
                    let mut receive = std::pin::pin!(self.live.connection.read_datagram());
                    let mut context = Context::from_waker(Waker::noop());
                    match receive.as_mut().poll(&mut context) {
                        Poll::Pending => Ok(None),
                        Poll::Ready(Ok(payload)) => Ok(Some(payload)),
                        Poll::Ready(Err(err)) => {
                            Err(self.live.error(ErrorKind::ConnectionAborted, "quic_closed", err))
                        }
                    }
                },
                || {
                    self.live.lost.fetch_add(1, Ordering::Relaxed);
                    self.live.error(
                        ErrorKind::InvalidInput,
                        "quic_bind",
                        "receive buffer is shorter than the datagram",
                    )
                },
            )
        }

        fn max_datagram_size(&self, _dest: SocketAddr) -> usize {
            // Keep reliable fragments valid if discovery later falls back to QUIC's minimum
            // path MTU. Short header: flags(1), max CID(20), packet number(4), tag(16),
            // DATAGRAM type + length(3). Discovery can still coalesce smaller frames.
            const MIN_PATH_PAYLOAD: usize = 1200 - (1 + 20 + 4 + 16 + 3);
            self.live
                .connection
                .max_datagram_size()
                .unwrap_or(0)
                .min(MIN_PATH_PAYLOAD)
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.live.local)
        }
    }

    // IO policy seams let deterministic tests inject peer limits and buffered datagrams.
    fn check_payload_size(len: usize, connection_max: Option<usize>) -> io::Result<()> {
        let Some(max) = connection_max else {
            return Err(quic_err(
                ErrorKind::InvalidInput,
                "quic_bind",
                "peer has datagrams disabled",
            ));
        };
        let max = max.min(MAX_DATAGRAM_BYTES);
        if len > max {
            return Err(quic_err(
                ErrorKind::InvalidInput,
                "quic_bind",
                format!("payload of {len} bytes exceeds {max}-byte datagram limit"),
            ));
        }
        Ok(())
    }

    fn receive_datagram<T: AsRef<[u8]>>(
        buf: &mut [u8],
        peer: SocketAddr,
        mut receive: impl FnMut() -> io::Result<Option<T>>,
        mut oversized: impl FnMut() -> io::Error,
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        // Bound work under a malicious oversized flood. Returning None after the budget
        // yields to the next endpoint poll, but one bad datagram never hides the next good one.
        for _ in 0..1024 {
            let Some(payload) = receive()? else {
                return Ok(None);
            };
            let payload = payload.as_ref();
            if payload.len() > buf.len() || payload.len() > MAX_DATAGRAM_BYTES {
                let _ = oversized();
                continue;
            }
            buf[..payload.len()].copy_from_slice(payload);
            return Ok(Some((payload.len(), peer)));
        }
        Ok(None)
    }

    #[cfg(test)]
    mod edge_tests {
        use super::*;

        /// A helper test (review row 23, ADR-7995): it exercises the size rule
        /// `check_payload_size` through a closure, not a live connection. The live case, a real
        /// QUIC pair whose peer advertises a smaller datagram limit, is
        /// `a_peer_with_a_smaller_datagram_limit_refuses_the_large_one_and_takes_the_retry`;
        /// the reschedule of connection work is covered by `connection::mtu_edge_tests`.
        #[test]
        fn quic_edges_small_peer_refuses_1300_and_accepts_small_retry() {
            let mut delivered = Vec::new();
            let mut send = |bytes: &[u8]| {
                check_payload_size(bytes.len(), Some(1200))?;
                delivered.push(bytes.to_vec());
                Ok::<_, io::Error>(())
            };
            assert!(
                send(&[7; 1300]).is_err(),
                "1300 exceeds the peer's 1200-byte limit"
            );
            send(&[8; 100]).expect("smaller retransmission");
            assert_eq!(delivered, vec![vec![8; 100]]);
        }

        #[test]
        fn quic_edges_oversized_receive_does_not_hide_next_datagram() {
            let peer = SocketAddr::from(([127, 0, 0, 1], 1234));
            let mut queue = std::collections::VecDeque::from([vec![7; 3000], vec![8; 100]]);
            let mut lost = 0;
            let mut buf = [0; MAX_DATAGRAM_BYTES];
            let got = receive_datagram(
                &mut buf,
                peer,
                || Ok(queue.pop_front()),
                || {
                    lost += 1;
                    quic_err(ErrorKind::InvalidInput, "quic_bind", "oversized")
                },
            );
            assert_eq!(
                got.expect("oversize must not abort the receive"),
                Some((100, peer))
            );
            assert_eq!(&buf[..100], &[8; 100]);
            assert_eq!(lost, 1);
        }

        #[test]
        fn quic_edges_oversize_flood_yields_after_a_bounded_drain() {
            let peer = SocketAddr::from(([127, 0, 0, 1], 1234));
            let mut queue: std::collections::VecDeque<_> = (0..1030).map(|_| vec![7; 3000]).collect();
            queue.push_back(vec![8; 100]);
            let mut lost = 0;
            let mut buf = [0; MAX_DATAGRAM_BYTES];
            let got = receive_datagram(
                &mut buf,
                peer,
                || Ok(queue.pop_front()),
                || {
                    lost += 1;
                    quic_err(ErrorKind::InvalidInput, "quic_bind", "oversized")
                },
            )
            .expect("drain");
            assert_eq!(got, None);
            assert_eq!(lost, 1024);
            let got = receive_datagram(
                &mut buf,
                peer,
                || Ok(queue.pop_front()),
                || {
                    lost += 1;
                    quic_err(ErrorKind::InvalidInput, "quic_bind", "oversized")
                },
            )
            .expect("next poll");
            assert_eq!(got, Some((100, peer)));
            assert_eq!(lost, 1030);
            assert_eq!(&buf[..100], &[8; 100]);
        }

        #[test]
        fn quic_edges_initial_path_is_1200_with_discovery() {
            // Quinn exposes its configuration through Debug, not public field getters.
            let config = format!("{:?}", transport_config());
            assert!(config.contains("initial_mtu: 1200"), "{config}");
            assert!(config.contains("mtu_discovery_config: Some"), "{config}");
        }

        /// `mtu_probing` off reaches QUIC: no discovery, the path stays at 1,200 (ADR-8893).
        #[test]
        fn mtu_probing_off_turns_quic_discovery_off() {
            let config = format!("{:?}", transport_config_for(false));
            assert!(config.contains("initial_mtu: 1200"), "{config}");
            assert!(config.contains("mtu_discovery_config: None"), "{config}");
        }

        /// The UDP socket QUIC owns carries don't-fragment (ADR-8893), read back through a clone
        /// of the descriptor after the endpoint took it.
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows))]
        #[test]
        fn the_quic_udp_socket_sets_dont_fragment() {
            let udp = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind");
            let clone = udp.try_clone().expect("clone");
            note_dont_fragment(&udp);
            let _endpoint = open_endpoint(udp, None).expect("endpoint");
            assert!(
                crate::socket::dont_fragment_is_set(&clone),
                "don't-fragment holds under QUIC"
            );
        }
    }

    /// Drops the next datagrams before they enter QUIC, and reads this socket's counters.
    ///
    /// Clone it before the socket moves into a [`crate::NetSession`]. [`Self::drop_next`] counts
    /// the datagram as lost and returns success from `send_to`, the same as a full UDP buffer, so
    /// the reliable channel resends it.
    #[derive(Clone)]
    pub struct QuicSocketControl {
        live: Arc<Live>,
    }

    impl fmt::Debug for QuicSocketControl {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("QuicSocketControl")
                .field("snapshot", &self.snapshot())
                .finish()
        }
    }

    impl QuicSocketControl {
        /// Drops the next `n` datagrams passed to `send_to`. They never reach QUIC, so QUIC does
        /// not retransmit them.
        pub fn drop_next(&self, n: u64) {
            self.live.drop_next.fetch_add(n, Ordering::Relaxed);
        }

        /// This socket's counters. Other sockets in the process are not included.
        pub fn snapshot(&self) -> QuicStatus {
            self.live.snapshot()
        }
    }

    struct Live {
        local: SocketAddr,
        peer: SocketAddr,
        connection: quinn::Connection,
        bound: AtomicBool,
        sent: AtomicU64,
        lost: AtomicU64,
        drop_next: AtomicU64,
        last_error: Mutex<Option<LastError>>,
    }

    struct LastError {
        seq: u64,
        code: String,
    }

    impl Live {
        fn new(local: SocketAddr, peer: SocketAddr, connection: quinn::Connection) -> Self {
            Self {
                local,
                peer,
                connection,
                bound: AtomicBool::new(true),
                sent: AtomicU64::new(0),
                lost: AtomicU64::new(0),
                drop_next: AtomicU64::new(0),
                last_error: Mutex::new(None),
            }
        }

        fn is_bound(&self) -> bool {
            if self.connection.close_reason().is_some() && self.bound.swap(false, Ordering::AcqRel) {
                note_error(self, "quic_closed");
            }
            self.bound.load(Ordering::Acquire)
        }

        fn error(&self, kind: ErrorKind, code: &str, detail: impl fmt::Display) -> io::Error {
            if code == "quic_closed" {
                self.bound.store(false, Ordering::Release);
            }
            note_error(self, code);
            quic_err(kind, code, detail)
        }

        fn take_drop(&self) -> bool {
            self.drop_next
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
        }

        fn snapshot(&self) -> QuicStatus {
            QuicStatus {
                enabled: true,
                bound: self.is_bound(),
                datagrams_sent: self.sent.load(Ordering::Relaxed),
                datagrams_lost: self.lost.load(Ordering::Relaxed),
                last_error: lock(&self.last_error).as_ref().map(|err| err.code.clone()),
            }
        }
    }

    fn note_error(live: &Live, code: &str) {
        let seq = ERR_SEQ.fetch_add(1, Ordering::Relaxed);
        *lock(&live.last_error) = Some(LastError {
            seq,
            code: code.to_owned(),
        });
    }

    static ERR_SEQ: AtomicU64 = AtomicU64::new(1);
    static REGISTRY: Mutex<Vec<Weak<Live>>> = Mutex::new(Vec::new());

    fn register(live: &Arc<Live>) {
        let mut guard = lock(&REGISTRY);
        guard.retain(|weak| weak.strong_count() > 0);
        guard.push(Arc::downgrade(live));
    }

    /// Aggregates every live QUIC socket in this process.
    pub fn quic_status() -> QuicStatus {
        let mut guard = lock(&REGISTRY);
        guard.retain(|weak| weak.strong_count() > 0);
        let mut sent = 0u64;
        let mut lost = 0u64;
        let mut bound = false;
        let mut best: Option<(u64, String)> = None;
        for weak in guard.iter() {
            let Some(live) = weak.upgrade() else { continue };
            sent = sent.saturating_add(live.sent.load(Ordering::Relaxed));
            lost = lost.saturating_add(live.lost.load(Ordering::Relaxed));
            bound |= live.is_bound();
            if let Some(err) = lock(&live.last_error).as_ref()
                && best.as_ref().is_none_or(|(seq, _)| err.seq > *seq)
            {
                best = Some((err.seq, err.code.clone()));
            }
        }
        QuicStatus {
            enabled: true,
            bound,
            datagrams_sent: sent,
            datagrams_lost: lost,
            last_error: best.map(|(_, code)| code),
        }
    }

    // A private notification per wait avoids nested executors consuming a shared unpark token.
    #[derive(Default)]
    struct HandshakeWake {
        notified: Mutex<bool>,
        ready: Condvar,
    }

    impl Wake for HandshakeWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            *lock(&self.notified) = true;
            self.ready.notify_one();
        }
    }

    fn wait_for<T>(future: impl IntoFuture<Output = T>) -> T {
        let signal = Arc::new(HandshakeWake::default());
        let waker = Waker::from(Arc::clone(&signal));
        let mut context = Context::from_waker(&waker);
        let mut pending = std::pin::pin!(future.into_future());
        loop {
            if let Poll::Ready(value) = pending.as_mut().poll(&mut context) {
                return value;
            }
            let mut notified = lock(&signal.notified);
            while !*notified {
                notified = signal
                    .ready
                    .wait(notified)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            *notified = false;
        }
    }

    #[cfg(test)]
    mod waiter_tests {
        use super::*;
        use std::pin::Pin;

        struct NotifyThenReady {
            polled: bool,
            nested: bool,
        }

        impl Future for NotifyThenReady {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.polled {
                    return Poll::Ready(());
                }
                self.polled = true;
                cx.waker().wake_by_ref();
                if self.nested {
                    wait_for(NotifyThenReady {
                        polled: false,
                        nested: false,
                    });
                }
                Poll::Pending
            }
        }

        #[test]
        fn notification_before_wait_is_retained() {
            wait_for(NotifyThenReady {
                polled: false,
                nested: false,
            });
        }

        #[test]
        fn nested_waiters_keep_separate_notifications() {
            wait_for(NotifyThenReady {
                polled: false,
                nested: true,
            });
        }
    }

    fn open_endpoint(udp: UdpSocket, server: Option<quinn::ServerConfig>) -> io::Result<quinn::Endpoint> {
        quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            server,
            udp,
            Arc::new(quinn::SmolRuntime),
        )
        .map_err(|err| quic_err(ErrorKind::AddrNotAvailable, "quic_bind", err))
    }

    /// Sets don't-fragment on the UDP socket QUIC will own (ADR-8893; RFC 9000 § 14 requires it
    /// where possible). quinn's socket layer sets it too where it can; this does not depend on that.
    fn note_dont_fragment(udp: &UdpSocket) {
        if !crate::socket::set_dont_fragment(udp) {
            tracing::debug!("quic: don't-fragment not set on the UDP socket");
        }
    }

    fn transport_config() -> quinn::TransportConfig {
        transport_config_for(true)
    }

    /// The transport settings; `mtu_discovery` off pins the path at QUIC's 1,200-byte minimum.
    fn transport_config_for(mtu_discovery: bool) -> quinn::TransportConfig {
        let mut transport = quinn::TransportConfig::default();
        transport.datagram_receive_buffer_size(Some(128 * 1024));
        transport.datagram_send_buffer_size(128 * 1024);
        // QUIC's minimum path MTU is 1200, including its headers. Regia asks this socket for
        // its payload budget before fragmenting; discovery is free to probe larger paths.
        transport.initial_mtu(1200);
        transport.mtu_discovery_config(mtu_discovery.then(quinn::MtuDiscoveryConfig::default));
        transport.enable_segmentation_offload(false);
        transport.max_concurrent_bidi_streams(quinn::VarInt::from_u32(0));
        transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(0));
        if let Ok(idle) = quinn::IdleTimeout::try_from(Duration::from_secs(60)) {
            transport.max_idle_timeout(Some(idle));
        }
        transport
    }

    fn server_config(cert_der: &[u8], pkcs8: &[u8]) -> io::Result<quinn::ServerConfig> {
        let provider = Arc::new(quinn::rustls::crypto::ring::default_provider());
        let mut tls = quinn::rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", err))?
            .with_no_client_auth()
            .with_single_cert(
                vec![quinn::rustls::pki_types::CertificateDer::from(cert_der.to_vec())],
                quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(
                    quinn::rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8.to_vec()),
                ),
            )
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", err))?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", format!("crypto: {err:?}")))?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        server.transport_config(Arc::new(transport_config()));
        Ok(server)
    }

    fn client_config(cert_der: &[u8], mtu_discovery: bool) -> io::Result<quinn::ClientConfig> {
        let provider = Arc::new(quinn::rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedCert {
            der: cert_der.to_vec(),
            provider: Arc::clone(&provider),
        });
        let mut tls = quinn::rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", err))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", format!("crypto: {err:?}")))?;
        let mut client = quinn::ClientConfig::new(Arc::new(crypto));
        client.transport_config(Arc::new(transport_config_for(mtu_discovery)));
        Ok(client)
    }

    #[derive(Debug)]
    struct PinnedCert {
        der: Vec<u8>,
        provider: Arc<quinn::rustls::crypto::CryptoProvider>,
    }

    impl quinn::rustls::client::danger::ServerCertVerifier for PinnedCert {
        fn verify_server_cert(
            &self,
            end_entity: &quinn::rustls::pki_types::CertificateDer<'_>,
            intermediates: &[quinn::rustls::pki_types::CertificateDer<'_>],
            _server_name: &quinn::rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: quinn::rustls::pki_types::UnixTime,
        ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error> {
            if intermediates.is_empty() && end_entity.as_ref() == self.der.as_slice() {
                Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(quinn::rustls::Error::InvalidCertificate(
                    quinn::rustls::CertificateError::BadEncoding,
                ))
            }
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
            _dss: &quinn::rustls::DigitallySignedStruct,
        ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
            Err(quinn::rustls::Error::PeerIncompatible(
                quinn::rustls::PeerIncompatible::Tls12NotOffered,
            ))
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &quinn::rustls::pki_types::CertificateDer<'_>,
            dss: &quinn::rustls::DigitallySignedStruct,
        ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error> {
            quinn::rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    // Ed25519 AlgorithmIdentifier, with absent parameters (RFC 8410 § 3).
    const ED25519: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

    fn mint_certificate() -> io::Result<(Vec<u8>, Vec<u8>)> {
        let provider = quinn::rustls::crypto::ring::default_provider();
        let mut seed = [0u8; 32];
        provider
            .secure_random
            .fill(&mut seed)
            .map_err(|_| quic_err(ErrorKind::InvalidInput, "quic_bind", "key generation failed"))?;
        // PKCS#8 version 0, Ed25519 identifier and nested OCTET STRING seed (RFC 8410 § 7).
        let mut pkcs8 = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
        ];
        pkcs8.extend_from_slice(&seed);
        let loaded = provider
            .key_provider
            .load_private_key(quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(
                quinn::rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8.clone()),
            ))
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", err))?;
        let spki = loaded
            .public_key()
            .ok_or_else(|| quic_err(ErrorKind::InvalidInput, "quic_bind", "key has no public key"))?;
        let tbs = tbs_certificate(spki.as_ref())?;
        let signer = loaded
            .choose_scheme(&[quinn::rustls::SignatureScheme::ED25519])
            .ok_or_else(|| quic_err(ErrorKind::InvalidInput, "quic_bind", "key cannot sign Ed25519"))?;
        let signature = signer
            .sign(&tbs)
            .map_err(|err| quic_err(ErrorKind::InvalidInput, "quic_bind", err))?;
        Ok((certificate(&tbs, &signature)?, pkcs8))
    }

    fn tbs_certificate(spki: &[u8]) -> io::Result<Vec<u8>> {
        let name = distinguished_name()?;
        let validity = der_seq(&concat(&[
            &utc_time("200101000000Z")?,
            &utc_time("491231235959Z")?,
        ]))?;
        let extensions = extensions()?;
        let body = concat(&[
            &[0xA0, 0x03, 0x02, 0x01, 0x02],
            &[0x02, 0x01, 0x01],
            ED25519,
            &name,
            &validity,
            &name,
            spki,
            &extensions,
        ]);
        der_seq(&body)
    }

    fn certificate(tbs: &[u8], signature: &[u8]) -> io::Result<Vec<u8>> {
        let bit = der_bit_string(signature)?;
        der_seq(&concat(&[tbs, ED25519, &bit]))
    }

    fn distinguished_name() -> io::Result<Vec<u8>> {
        let oid = der_tlv(0x06, &[0x55, 0x04, 0x03])?;
        let value = der_tlv(0x13, b"regia-quic")?;
        let atav = der_seq(&concat(&[&oid, &value]))?;
        let set = der_tlv(0x31, &atav)?;
        der_seq(&set)
    }

    fn extensions() -> io::Result<Vec<u8>> {
        let key_usage = extension(&[0x55, 0x1D, 0x0F], &[0x03, 0x02, 0x07, 0x80])?;
        let basic = extension(&[0x55, 0x1D, 0x13], &[0x30, 0x00])?;
        let dns = der_tlv(0x82, b"localhost")?;
        let san_seq = der_seq(&dns)?;
        let san = extension(&[0x55, 0x1D, 0x11], &san_seq)?;
        let seq = der_seq(&concat(&[&key_usage, &basic, &san]))?;
        der_tlv(0xA3, &seq)
    }

    fn extension(oid: &[u8], value: &[u8]) -> io::Result<Vec<u8>> {
        let oid = der_tlv(0x06, oid)?;
        let octets = der_tlv(0x04, value)?;
        der_seq(&concat(&[&oid, &octets]))
    }

    fn utc_time(text: &str) -> io::Result<Vec<u8>> {
        der_tlv(0x17, text.as_bytes())
    }

    fn der_bit_string(data: &[u8]) -> io::Result<Vec<u8>> {
        let mut body = Vec::with_capacity(1 + data.len());
        body.push(0x00);
        body.extend_from_slice(data);
        der_tlv(0x03, &body)
    }

    fn der_seq(body: &[u8]) -> io::Result<Vec<u8>> {
        der_tlv(0x30, body)
    }

    fn der_tlv(tag: u8, body: &[u8]) -> io::Result<Vec<u8>> {
        if body.len() > 0xFFFF {
            return Err(quic_err(
                ErrorKind::InvalidInput,
                "quic_bind",
                "certificate field is too long",
            ));
        }
        let mut out = Vec::with_capacity(4 + body.len());
        out.push(tag);
        let len = body.len();
        if len < 0x80 {
            out.push(len as u8);
        } else if len <= 0xFF {
            out.push(0x81);
            out.push(len as u8);
        } else {
            out.push(0x82);
            out.push((len >> 8) as u8);
            out.push((len & 0xFF) as u8);
        }
        out.extend_from_slice(body);
        Ok(out)
    }

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for part in parts {
            out.extend_from_slice(part);
        }
        out
    }

    fn is_localhost_v4(addr: SocketAddr) -> bool {
        matches!(addr, SocketAddr::V4(v4) if *v4.ip() == Ipv4Addr::LOCALHOST)
    }

    fn quic_err(kind: ErrorKind, code: &str, detail: impl fmt::Display) -> io::Error {
        io::Error::new(kind, format!("{code}: {detail}"))
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(feature = "net-quic")]
pub use imp::{QuicDatagramSocket, QuicListener, QuicSocketControl};

#[cfg(feature = "net-quic")]
pub use imp::quic_status;

#[cfg(test)]
mod tests {
    use super::quic_error_code;
    #[cfg(not(feature = "net-quic"))]
    use super::{quic_enabled, quic_status};

    #[test]
    fn error_code_reads_the_prefix() {
        let bind = std::io::Error::new(std::io::ErrorKind::InvalidInput, "quic_bind: refused");
        assert_eq!(quic_error_code(&bind), Some("quic_bind"));
        let handshake = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "quic_handshake: alert");
        assert_eq!(quic_error_code(&handshake), Some("quic_handshake"));
        let closed = std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "quic_closed: gone");
        assert_eq!(quic_error_code(&closed), Some("quic_closed"));
        let other = std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a quic error");
        assert_eq!(quic_error_code(&other), None);
    }

    #[cfg(not(feature = "net-quic"))]
    #[test]
    fn feature_off_status_is_disabled() {
        assert!(!quic_enabled());
        let status = quic_status();
        assert!(!status.enabled);
        assert!(!status.bound);
        assert_eq!(status.datagrams_sent, 0);
        assert_eq!(status.datagrams_lost, 0);
        assert!(status.last_error.is_none());
    }
}

#[cfg(all(test, feature = "net-quic"))]
mod quic_tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::thread;

    use super::{QuicDatagramSocket, QuicListener, quic_enabled, quic_error_code};
    use crate::crypto::CryptoMode;
    use crate::session::{ConnectOptions, ContentManifest, HostOptions, NetSession, SessionSettings};
    use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};

    fn localhost(port: u16) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    }

    fn pair() -> (QuicDatagramSocket, QuicDatagramSocket, SocketAddr) {
        let listener = QuicListener::bind(localhost(0)).expect("bind");
        let addr = listener.local_addr();
        let cert = listener.certificate_der().to_vec();
        let server = thread::spawn(move || listener.accept());
        match QuicDatagramSocket::connect(addr, &cert) {
            Ok(client) => {
                let server = server.join().expect("server thread").expect("accept");
                (server, client, addr)
            }
            Err(err) => {
                let server_err = server.join().expect("server thread");
                panic!("connect failed: {err}; accept: {server_err:?}");
            }
        }
    }

    fn spin() {
        for _ in 0..512 {
            thread::yield_now();
        }
    }

    #[test]
    fn feature_on_is_enabled() {
        assert!(quic_enabled());
    }

    #[test]
    fn refused_addresses_are_quic_bind() {
        for addr in [
            localhost(12345),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            SocketAddr::from((Ipv4Addr::new(127, 0, 0, 2), 0)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 0)),
        ] {
            let err = QuicListener::bind(addr).expect_err("refused");
            assert_eq!(quic_error_code(&err), Some("quic_bind"), "{addr}: {err}");
        }
        let err = QuicDatagramSocket::connect(SocketAddr::from(([8, 8, 8, 8], 443)), b"x").expect_err("wan");
        assert_eq!(quic_error_code(&err), Some("quic_bind"));
        let err = QuicDatagramSocket::connect(localhost(0), b"x").expect_err("port 0");
        assert_eq!(quic_error_code(&err), Some("quic_bind"));
    }

    #[test]
    fn endpoints_exchange_a_payload() {
        let (mut server, mut client, addr) = pair();
        client.send_to(b"ping-quic", addr).expect("ping");
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        let mut from = None;
        for _ in 0..200_000 {
            if let Some((n, peer)) = server.recv_from(&mut buf).expect("recv") {
                assert_eq!(&buf[..n], b"ping-quic");
                from = Some(peer);
                break;
            }
            thread::yield_now();
        }
        let from = from.expect("ping arrived");
        server.send_to(b"pong-quic", from).expect("pong");
        let mut got = false;
        for _ in 0..200_000 {
            if let Some((n, _)) = client.recv_from(&mut buf).expect("recv") {
                assert_eq!(&buf[..n], b"pong-quic");
                got = true;
                break;
            }
            thread::yield_now();
        }
        assert!(got, "pong arrived");
        assert!(client.control().snapshot().datagrams_sent >= 1);
        assert!(server.control().snapshot().datagrams_sent >= 1);
    }

    #[test]
    fn wrong_peer_and_oversize_are_quic_bind() {
        let (server, mut client, addr) = pair();
        let err = client.send_to(b"nope", localhost(9)).expect_err("peer");
        assert_eq!(quic_error_code(&err), Some("quic_bind"));
        let big = vec![0u8; MAX_DATAGRAM_BYTES + 1];
        let err = client.send_to(&big, addr).expect_err("size");
        assert_eq!(quic_error_code(&err), Some("quic_bind"));
        let _ = server.local_addr().expect("local");
    }

    /// A payload under the local 2048-byte cap but over the QUIC datagram limit (the socket's
    /// path MTU) is refused with `quic_bind`, is not sent, and never reaches the peer; the
    /// connection stays up for the next datagram.
    #[test]
    fn a_payload_over_the_quic_datagram_limit_is_quic_bind_and_not_delivered() {
        let (mut server, mut client, addr) = pair();
        let big = vec![0x5a_u8; MAX_DATAGRAM_BYTES];
        let err = client
            .send_to(&big, addr)
            .expect_err("over the QUIC datagram limit");
        assert_eq!(quic_error_code(&err), Some("quic_bind"), "{err}");
        let status = client.control().snapshot();
        assert_eq!(status.datagrams_sent, 0);
        assert_eq!(status.datagrams_lost, 1);
        assert!(status.bound, "the connection stays up");
        assert_eq!(status.last_error.as_deref(), Some("quic_bind"));
        client
            .send_to(b"after", addr)
            .expect("a small datagram still goes");
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        let mut first = None;
        for _ in 0..200_000 {
            if let Some((n, _)) = server.recv_from(&mut buf).expect("recv") {
                first = Some(buf[..n].to_vec());
                break;
            }
            thread::yield_now();
        }
        assert_eq!(
            first.as_deref(),
            Some(&b"after"[..]),
            "the refused payload was not delivered"
        );
        assert_eq!(client.control().snapshot().datagrams_sent, 1);
    }

    /// Review row 23 (ADR-7995): a live pair whose peer advertises a smaller datagram limit
    /// than the path allows. The connection itself refuses the larger datagram, stays up, and
    /// delivers the smaller retry.
    #[test]
    fn a_peer_with_a_smaller_datagram_limit_refuses_the_large_one_and_takes_the_retry() {
        let mut listener = QuicListener::bind(localhost(0)).expect("bind");
        listener.advertise_datagram_limit(600);
        let addr = listener.local_addr();
        let cert = listener.certificate_der().to_vec();
        let server = thread::spawn(move || listener.accept());
        let mut client = QuicDatagramSocket::connect(addr, &cert).expect("connect");
        let mut server = server.join().expect("server thread").expect("accept");
        let limit = client.max_datagram_size(addr);
        assert!(
            (256..600).contains(&limit),
            "the peer's limit, not the path, bounds the datagram: {limit}"
        );
        let err = client
            .send_to(&[7u8; 700], addr)
            .expect_err("over the peer's limit");
        assert_eq!(quic_error_code(&err), Some("quic_bind"), "{err}");
        let status = client.control().snapshot();
        assert_eq!(status.datagrams_sent, 0);
        assert_eq!(status.datagrams_lost, 1);
        assert!(status.bound, "the connection stays up");
        client.send_to(&[8u8; 500], addr).expect("the smaller retry goes");
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        let mut first = None;
        for _ in 0..200_000 {
            if let Some((n, _)) = server.recv_from(&mut buf).expect("recv") {
                first = Some(buf[..n].to_vec());
                break;
            }
            thread::yield_now();
        }
        assert_eq!(first, Some(vec![8u8; 500]), "only the retry arrived");
        assert_eq!(client.control().snapshot().datagrams_sent, 1);
    }

    #[test]
    fn garbage_certificate_is_quic_handshake() {
        let listener = QuicListener::bind(localhost(0)).expect("bind");
        let addr = listener.local_addr();
        let server = thread::spawn(move || listener.accept());
        let client = QuicDatagramSocket::connect(addr, b"not-a-certificate");
        let server = server.join().expect("server thread");
        assert_eq!(
            quic_error_code(&client.expect_err("client")),
            Some("quic_handshake")
        );
        assert_eq!(
            quic_error_code(&server.expect_err("server")),
            Some("quic_handshake")
        );
    }

    #[test]
    fn dropped_peer_reports_quic_closed() {
        let (server, mut client, addr) = pair();
        drop(server);
        let mut buf = [0u8; 64];
        let mut closed = false;
        for _ in 0..200_000 {
            if client
                .recv_from(&mut buf)
                .err()
                .and_then(|err| quic_error_code(&err))
                == Some("quic_closed")
            {
                closed = true;
                break;
            }
            if client
                .send_to(b"x", addr)
                .err()
                .and_then(|err| quic_error_code(&err))
                == Some("quic_closed")
            {
                closed = true;
                break;
            }
            thread::yield_now();
        }
        assert!(closed, "peer close is quic_closed");
        let status = client.control().snapshot();
        assert!(!status.bound, "closed connections are not reported as bound");
        assert_eq!(status.last_error.as_deref(), Some("quic_closed"));
    }

    #[test]
    fn reliable_session_delivers_a_payload_after_a_dropped_datagram() {
        let listener = QuicListener::bind(localhost(0)).expect("bind");
        let addr = listener.local_addr();
        let cert = listener.certificate_der().to_vec();
        let server_sock = thread::spawn(move || listener.accept());
        let client_sock = QuicDatagramSocket::connect(addr, &cert).expect("connect");
        let server_sock = server_sock.join().expect("server thread").expect("accept");
        let client_ctl = client_sock.control();
        let server_ctl = server_sock.control();
        client_ctl.drop_next(1);
        let manifest = ContentManifest::build("regia", "test", 1, 0, &[]);
        let settings = SessionSettings {
            encryption: CryptoMode::Off,
            ..SessionSettings::default()
        };
        let mut server = NetSession::new(settings.clone(), manifest.clone());
        let mut client = NetSession::new(settings, manifest);
        server.set_manual_clock(0);
        client.set_manual_clock(0);
        server
            .host_on(server_sock, HostOptions::default(), false)
            .expect("host");
        client
            .connect_on(client_sock, addr, ConnectOptions::default())
            .expect("connect");
        for _ in 0..160 {
            if client.state().as_str() == "in_game" {
                break;
            }
            client.advance_manual_clock(50);
            server.advance_manual_clock(50);
            client.update();
            spin();
            server.update();
            spin();
            client.update();
            spin();
        }
        assert_eq!(client.state().as_str(), "in_game", "state {:?}", client.state());
        assert!(client_ctl.snapshot().datagrams_lost >= 1);
        // Drop the application packet after the session is fully joined. QUIC never receives
        // this datagram; only Regia's reliable channel can recover the payload.
        let lost_before = server_ctl.snapshot().datagrams_lost;
        server_ctl.drop_next(1);
        let payload = b"reliable game payload over quic".to_vec();
        let peer = client
            .client()
            .expect("client core")
            .peer_id()
            .expect("joined peer");
        server
            .server_mut()
            .expect("server core")
            .send_game(peer, payload.clone());
        let mut received = Vec::new();
        for _ in 0..160 {
            client.advance_manual_clock(50);
            server.advance_manual_clock(50);
            client.update();
            spin();
            server.update();
            spin();
            for event in client.take_client_events() {
                if let crate::session::ClientSessionEvent::Game(bytes) = event {
                    received.push(bytes);
                }
            }
        }
        assert_eq!(received, vec![payload]);
        assert_eq!(server_ctl.snapshot().datagrams_lost, lost_before + 1);
    }
}
