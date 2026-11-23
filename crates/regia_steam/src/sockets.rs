// SPDX-License-Identifier: MPL-2.0
//! The Steam sockets transport (spec 89 § 7, ADR-1877): a [`regia_net::DatagramSocket`] over
//! Steam's connectionless peer-to-peer messages, so the unchanged spec 77 session runs over
//! Steam's relay network (NAT traversal, IP hiding) addressed by Steam id.
//!
//! Every datagram goes out unreliable and without Nagle delay: `regia_net` already does
//! reliability, ordering, fragmentation and congestion control.
//!
//! **Addresses.** The session layer is keyed by `SocketAddr`, so a Steam id maps to a synthetic
//! IPv6 unique-local address (RFC 4193): `fd52:6567:6961:0:<steam id as four 16-bit groups>`
//! (`52 65 67 69 61` spells "Regia"), port = the messages channel. [`steam_peer_addr`] and
//! [`steam_id_of`] are exact inverses except for the invalid id 0, which has an address but no peer
//! (`steam_id_of` returns `None`, so a send to it is refused). A send goes out on the channel in the
//! destination address; a receive reports the socket's own channel. Synthetic addresses never reach the operating system.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex};

use regia_net::DatagramSocket;
use regia_net::socket::MAX_DATAGRAM_BYTES;

use crate::backend::{SteamError, SteamId};

/// The first three 16-bit groups of every synthetic Steam peer address.
pub const STEAM_ADDR_PREFIX: [u16; 3] = [0xfd52, 0x6567, 0x6961];
/// Messages taken from the platform per call. A [`SteamSocket`] asks again only once its queue is
/// empty, so it never holds more than one batch; the rest wait in Steam's own session queue.
pub const RECEIVE_BATCH: usize = 64;
/// Most senders [`SteamSocket::oversize_by_sender`] remembers; later new senders count only in
/// [`SteamSocketStats::oversize`].
pub const MAX_OVERSIZE_SENDERS: usize = 1024;

/// The synthetic address of Steam user `id` on `channel` (spec 89 § 7).
pub fn steam_peer_addr(id: SteamId, channel: u16) -> SocketAddr {
    let v = id.0;
    let ip = Ipv6Addr::new(
        STEAM_ADDR_PREFIX[0],
        STEAM_ADDR_PREFIX[1],
        STEAM_ADDR_PREFIX[2],
        0,
        (v >> 48) as u16,
        (v >> 32) as u16,
        (v >> 16) as u16,
        v as u16,
    );
    SocketAddr::V6(SocketAddrV6::new(ip, channel, 0, 0))
}

/// The Steam id and channel behind a synthetic address; `None` for any other address.
pub fn steam_id_of(addr: SocketAddr) -> Option<(SteamId, u16)> {
    let SocketAddr::V6(v6) = addr else {
        return None;
    };
    let g = v6.ip().segments();
    if g[..3] != STEAM_ADDR_PREFIX || g[3] != 0 {
        return None;
    }
    let id = (u64::from(g[4]) << 48) | (u64::from(g[5]) << 32) | (u64::from(g[6]) << 16) | u64::from(g[7]);
    (id != 0).then_some((SteamId(id), v6.port()))
}

/// Which peers may open a messages session with us.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SteamAccept {
    /// Anyone (the spec 77 handshake authenticates). The default.
    #[default]
    Anyone,
    /// Only these users (the lobby plugin keeps it equal to the current lobby's members).
    Listed(BTreeSet<SteamId>),
}

impl SteamAccept {
    /// Whether `peer` may open a session.
    pub fn allows(&self, peer: SteamId) -> bool {
        match self {
            Self::Anyone => true,
            Self::Listed(set) => set.contains(&peer),
        }
    }
}

/// The peer-to-peer messages a [`SteamSocket`] needs from Steam (ISteamNetworkingMessages); the
/// real client and [`crate::mock_net::MockSteamNet`] implement it.
pub trait P2pMessages: Send + Sync {
    /// The local user's id.
    fn local_id(&self) -> Option<SteamId>;
    /// Sends one unreliable, no-delay message to `to` on `channel`.
    fn send(&self, to: SteamId, channel: u16, bytes: &[u8]) -> Result<(), SteamError>;
    /// Takes up to `max` messages waiting on `channel`, oldest first.
    fn receive(&self, channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)>;
    /// Takes up to `max` messages and appends their bytes to `scratch`.
    ///
    /// Each pair is `(sender, length)` of the slice just appended, in order. The default calls
    /// [`Self::receive`] and copies. A backend overrides this to copy once out of the platform
    /// buffer, so a [`SteamSocket`] does not keep one `Vec` per datagram.
    fn receive_into(&self, channel: u16, max: usize, scratch: &mut Vec<u8>) -> Vec<(SteamId, usize)> {
        self.receive(channel, max)
            .into_iter()
            .map(|(id, bytes)| {
                let len = bytes.len();
                scratch.extend_from_slice(&bytes);
                (id, len)
            })
            .collect()
    }
    /// Takes up to `max` messages, appending to `scratch` only those of at most `max_len` bytes
    /// (spec 120 § 10.8, ADR-7981).
    ///
    /// Each pair is `(sender, Some(length))` of the slice just appended, or `(sender, None)` for a
    /// message longer than `max_len`, which leaves nothing in `scratch`. The default calls
    /// [`Self::receive_into`] and removes the long ones again; a backend overrides this so a long
    /// message is never copied at all (the real Steam client does). [`SteamSocket`] reads through
    /// this with [`MAX_DATAGRAM_BYTES`].
    fn receive_bounded(
        &self,
        channel: u16,
        max: usize,
        max_len: usize,
        scratch: &mut Vec<u8>,
    ) -> Vec<(SteamId, Option<usize>)> {
        let start = scratch.len();
        let batch = self.receive_into(channel, max, scratch);
        let (mut read, mut write) = (start, start);
        let out = batch
            .into_iter()
            .map(|(id, len)| {
                let end = read.saturating_add(len).min(scratch.len());
                let kept = (len <= max_len).then(|| {
                    scratch.copy_within(read..end, write);
                    write += end - read;
                    end - read
                });
                read = end;
                (id, kept)
            })
            .collect();
        scratch.truncate(write);
        out
    }
    /// Sets which peers may open a session.
    fn set_accept(&self, accept: SteamAccept);
}

/// Counters of one [`SteamSocket`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SteamSocketStats {
    /// Datagrams handed to Steam.
    pub sent: u64,
    /// Datagrams received.
    pub received: u64,
    /// Platform receive calls made (one per batch of up to [`RECEIVE_BATCH`]).
    pub receive_calls: u64,
    /// Sends Steam refused (no route, peer gone, limits). Each is also an error from
    /// `send_to`.
    pub send_errors: u64,
    /// Messages dropped on receipt because they were longer than [`MAX_DATAGRAM_BYTES`] (never
    /// copied; per sender in [`SteamSocket::oversize_by_sender`]).
    pub oversize: u64,
}

/// One datagram parked in the socket's scratch buffer.
struct QueuedDatagram {
    from: SocketAddr,
    start: usize,
    len: usize,
}

/// A [`DatagramSocket`] over Steam peer-to-peer messages on one channel.
pub struct SteamSocket {
    messages: Arc<dyn P2pMessages>,
    channel: u16,
    local: SocketAddr,
    /// Byte ranges into `scratch`. `len()` is the number of queued datagrams.
    queue: VecDeque<QueuedDatagram>,
    /// Payloads of the current batch. Cleared when the queue is filled again.
    scratch: Vec<u8>,
    stats: Arc<Mutex<SteamSocketStats>>,
    /// Oversize messages per sender, at most [`MAX_OVERSIZE_SENDERS`] senders.
    oversize: BTreeMap<SteamId, u64>,
}

impl std::fmt::Debug for SteamSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamSocket")
            .field("channel", &self.channel)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl SteamSocket {
    /// A socket for the local user on `channel`. Fails when Steam has no signed-in user.
    pub fn open(messages: Arc<dyn P2pMessages>, channel: u16) -> Result<Self, SteamError> {
        let me = messages.local_id().ok_or(SteamError::Unavailable)?;
        Ok(Self {
            messages,
            channel,
            local: steam_peer_addr(me, channel),
            queue: VecDeque::new(),
            scratch: Vec::new(),
            stats: Arc::new(Mutex::new(SteamSocketStats::default())),
            oversize: BTreeMap::new(),
        })
    }

    /// The channel.
    pub fn channel(&self) -> u16 {
        self.channel
    }

    /// The counters (shared: readable after the socket moved into a session).
    pub fn stats_handle(&self) -> Arc<Mutex<SteamSocketStats>> {
        self.stats.clone()
    }

    /// A copy of the counters.
    pub fn stats(&self) -> SteamSocketStats {
        *self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Messages dropped for being longer than [`MAX_DATAGRAM_BYTES`], per sender (spec 120
    /// § 10.8).
    pub fn oversize_by_sender(&self) -> &BTreeMap<SteamId, u64> {
        &self.oversize
    }

    fn count(&self, f: impl FnOnce(&mut SteamSocketStats)) {
        f(&mut self.stats.lock().unwrap_or_else(|e| e.into_inner()));
    }

    fn fill(&mut self) {
        if !self.queue.is_empty() {
            return;
        }
        self.scratch.clear();
        let batch =
            self.messages
                .receive_bounded(self.channel, RECEIVE_BATCH, MAX_DATAGRAM_BYTES, &mut self.scratch);
        let channel = self.channel;
        let mut start = 0usize;
        let mut n = 0u64;
        let mut oversize = 0u64;
        for (from, len) in batch {
            let Some(len) = len else {
                // Longer than any endpoint reads: never copied, counted against the sender.
                oversize += 1;
                if let Some(count) = self.oversize.get_mut(&from) {
                    *count += 1;
                } else if self.oversize.len() < MAX_OVERSIZE_SENDERS {
                    self.oversize.insert(from, 1);
                }
                continue;
            };
            n += 1;
            self.queue.push_back(QueuedDatagram {
                from: steam_peer_addr(from, channel),
                start,
                len,
            });
            start = start.saturating_add(len);
        }
        self.count(|s| {
            s.received += n;
            s.receive_calls += 1;
            s.oversize += oversize;
        });
    }
}

impl DatagramSocket for SteamSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let Some((peer, channel)) = steam_id_of(dest) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a Steam peer address (spec 89 § 7)",
            ));
        };
        if bytes.len() > MAX_DATAGRAM_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram longer than MAX_DATAGRAM_BYTES",
            ));
        }
        match self.messages.send(peer, channel, bytes) {
            Ok(()) => {
                self.count(|s| s.sent += 1);
                Ok(())
            }
            // The datagram did not leave: surfaced, so the endpoint counts it as a socket error
            // instead of thinking it went out (spec 120 § 10.8, adversarial row 15). The session's
            // timeouts still decide what a dead route means.
            Err(e) => {
                self.count(|s| s.send_errors += 1);
                Err(io::Error::other(format!("Steam refused the send: {e}")))
            }
        }
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.fill();
        let Some(queued) = self.queue.pop_front() else {
            return Ok(None);
        };
        let end = queued.start.saturating_add(queued.len).min(self.scratch.len());
        let bytes = &self.scratch[queued.start.min(end)..end];
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(Some((n, queued.from)))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
}

/// The [`regia_net::lobby::LobbyTransport`] over Steam sockets: the owner hosts on a
/// [`SteamSocket`] and publishes its Steam id; members connect to the owner's synthetic address.
pub struct SteamLobbyTransport {
    messages: Arc<dyn P2pMessages>,
    channel: u16,
}

impl SteamLobbyTransport {
    /// A transport on `channel`.
    pub fn new(messages: Arc<dyn P2pMessages>, channel: u16) -> Self {
        Self { messages, channel }
    }
}

impl regia_net::lobby::LobbyTransport for SteamLobbyTransport {
    fn name(&self) -> &'static str {
        "steam"
    }

    fn open(&self) -> io::Result<Box<dyn DatagramSocket + Send>> {
        SteamSocket::open(self.messages.clone(), self.channel)
            .map(|s| Box::new(s) as Box<dyn DatagramSocket + Send>)
            .map_err(|e| io::Error::new(io::ErrorKind::NotConnected, e.to_string()))
    }

    fn server_for_host(&self, local: SocketAddr) -> regia_net::lobby::LobbyServer {
        match steam_id_of(local) {
            Some((id, _)) => regia_net::lobby::LobbyServer::Steam(id.0),
            None => regia_net::lobby::LobbyServer::Address(local),
        }
    }

    fn peer_addr(&self, server: &regia_net::lobby::LobbyServer) -> Option<SocketAddr> {
        match server {
            regia_net::lobby::LobbyServer::Steam(id) => Some(steam_peer_addr(SteamId(*id), self.channel)),
            regia_net::lobby::LobbyServer::Address(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_round_trip_and_reject_others() {
        for id in [1u64, 76_561_197_960_287_930, u64::MAX] {
            for channel in [0u16, 7, u16::MAX] {
                let addr = steam_peer_addr(SteamId(id), channel);
                assert_eq!(steam_id_of(addr), Some((SteamId(id), channel)));
            }
        }
        assert_eq!(
            steam_peer_addr(SteamId(0x0110_0001_0000_0042), 3).to_string(),
            "[fd52:6567:6961:0:110:1:0:42]:3"
        );
        assert_eq!(steam_id_of("127.0.0.1:7777".parse().expect("addr")), None);
        assert_eq!(steam_id_of("[::1]:7777".parse().expect("addr")), None);
        assert_eq!(
            steam_id_of("[fd52:6567:6961:1::5]:0".parse().expect("addr")),
            None
        );
        assert_eq!(
            steam_id_of(steam_peer_addr(SteamId(0), 0)),
            None,
            "the invalid id"
        );
    }

    #[test]
    fn accept_policy() {
        assert!(SteamAccept::Anyone.allows(SteamId(5)));
        let listed = SteamAccept::Listed([SteamId(1)].into_iter().collect());
        assert!(listed.allows(SteamId(1)));
        assert!(!listed.allows(SteamId(2)));
    }

    /// A platform queue of messages from one peer that records how often it is asked.
    struct Inbox {
        waiting: Mutex<VecDeque<(SteamId, Vec<u8>)>>,
        asked_max: Mutex<Vec<usize>>,
    }

    impl P2pMessages for Inbox {
        fn local_id(&self) -> Option<SteamId> {
            Some(SteamId(1))
        }
        fn send(&self, _to: SteamId, _channel: u16, _bytes: &[u8]) -> Result<(), SteamError> {
            Ok(())
        }
        fn receive(&self, _channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)> {
            self.asked_max.lock().expect("lock").push(max);
            let mut q = self.waiting.lock().expect("lock");
            let n = max.min(q.len());
            q.drain(..n).collect()
        }
        fn set_accept(&self, _accept: SteamAccept) {}
    }

    #[test]
    fn bursts_arrive_in_order_one_platform_call_per_batch_and_long_ones_truncate() {
        let burst = 150usize;
        let mut waiting: VecDeque<(SteamId, Vec<u8>)> =
            (0..burst).map(|i| (SteamId(7), vec![i as u8; 4])).collect();
        waiting.push_back((SteamId(9), (0..100u8).collect()));
        let inbox = Arc::new(Inbox {
            waiting: Mutex::new(waiting),
            asked_max: Mutex::new(Vec::new()),
        });
        let mut socket = SteamSocket::open(inbox.clone(), 3).expect("open");
        assert_eq!(
            socket.local_addr().expect("local"),
            steam_peer_addr(SteamId(1), 3)
        );

        let mut buf = [0u8; 16];
        for i in 0..burst {
            let (n, from) = socket.recv_from(&mut buf).expect("recv").expect("a datagram");
            assert_eq!((n, from), (4, steam_peer_addr(SteamId(7), 3)));
            assert_eq!(buf[..4], [i as u8; 4], "datagram {i} out of order");
        }
        // A datagram longer than the buffer is truncated, as with UDP.
        let (n, from) = socket.recv_from(&mut buf).expect("recv").expect("the long one");
        assert_eq!((n, from), (16, steam_peer_addr(SteamId(9), 3)));
        assert_eq!(buf, std::array::from_fn::<u8, 16, _>(|i| i as u8));
        assert_eq!(socket.recv_from(&mut buf).expect("recv"), None);

        // 151 datagrams: batches of 64, 64, 23, then one empty call.
        let asked = inbox.asked_max.lock().expect("lock").clone();
        assert_eq!(asked, vec![RECEIVE_BATCH; 4]);
        let stats = socket.stats();
        assert_eq!((stats.received, stats.receive_calls), (burst as u64 + 1, 4));
        assert_eq!(socket.queue.len(), 0);
    }

    /// Three datagrams in one batch stay byte-identical, and the socket does not keep a
    /// fresh owned `Vec` per datagram.
    #[test]
    fn a_batch_is_not_stored_as_one_vec_per_datagram() {
        struct Count {
            waiting: Mutex<VecDeque<(SteamId, Vec<u8>)>>,
            owned_vecs: Mutex<usize>,
        }
        impl P2pMessages for Count {
            fn local_id(&self) -> Option<SteamId> {
                Some(SteamId(1))
            }
            fn send(&self, _to: SteamId, _channel: u16, _bytes: &[u8]) -> Result<(), SteamError> {
                Ok(())
            }
            fn receive(&self, _channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)> {
                let mut q = self.waiting.lock().expect("lock");
                let n = max.min(q.len());
                let batch: Vec<_> = q.drain(..n).collect();
                *self.owned_vecs.lock().expect("lock") += batch.len();
                batch
            }
            fn receive_into(
                &self,
                _channel: u16,
                max: usize,
                scratch: &mut Vec<u8>,
            ) -> Vec<(SteamId, usize)> {
                let mut q = self.waiting.lock().expect("lock");
                let n = max.min(q.len());
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    let Some((id, bytes)) = q.pop_front() else {
                        break;
                    };
                    scratch.extend_from_slice(&bytes);
                    out.push((id, bytes.len()));
                }
                out
            }
            fn set_accept(&self, _accept: SteamAccept) {}
        }
        let inbox = Arc::new(Count {
            waiting: Mutex::new(VecDeque::from([
                (SteamId(7), b"aaa".to_vec()),
                (SteamId(7), b"bbb".to_vec()),
                (SteamId(8), b"ccc".to_vec()),
            ])),
            owned_vecs: Mutex::new(0),
        });
        let mut socket = SteamSocket::open(inbox.clone(), 1).expect("open");
        let mut buf = [0u8; 8];
        let mut got = Vec::new();
        for _ in 0..3 {
            let (n, from) = socket.recv_from(&mut buf).expect("io").expect("datagram");
            got.push((from, buf[..n].to_vec()));
        }
        assert_eq!(
            got,
            vec![
                (steam_peer_addr(SteamId(7), 1), b"aaa".to_vec()),
                (steam_peer_addr(SteamId(7), 1), b"bbb".to_vec()),
                (steam_peer_addr(SteamId(8), 1), b"ccc".to_vec()),
            ]
        );
        // The three payloads came from one platform poll. The empty follow-up is a second poll,
        // the same as the burst test's trailing empty call, so it is not part of this count.
        assert_eq!(socket.stats().receive_calls, 1);
        assert_eq!(
            *inbox.owned_vecs.lock().expect("lock"),
            0,
            "the payload must not be stored as a second owned Vec per datagram"
        );
        assert_eq!(socket.recv_from(&mut buf).expect("io"), None);
        assert_eq!(socket.stats().receive_calls, 2);
        assert_eq!(*inbox.owned_vecs.lock().expect("lock"), 0);
    }

    #[test]
    fn the_queue_never_holds_more_than_one_batch() {
        let inbox = Arc::new(Inbox {
            waiting: Mutex::new((0..1000).map(|i| (SteamId(7), vec![i as u8])).collect()),
            asked_max: Mutex::new(Vec::new()),
        });
        let mut socket = SteamSocket::open(inbox, 0).expect("open");
        let mut buf = [0u8; 4];
        let mut got = 0;
        while socket.recv_from(&mut buf).expect("recv").is_some() {
            got += 1;
            assert!(socket.queue.len() < RECEIVE_BATCH);
        }
        assert_eq!(got, 1000);
    }

    /// Spec 120 § 10.8 / adversarial row 15 (ADR-7981): a message longer than any endpoint reads
    /// is dropped from the batch, never queued, and counted against its sender; the rest of the
    /// batch arrives intact.
    #[test]
    fn oversize_messages_are_dropped_and_counted_per_sender() {
        let waiting: VecDeque<(SteamId, Vec<u8>)> = [
            (SteamId(7), vec![1; 8]),
            (SteamId(9), vec![2; MAX_DATAGRAM_BYTES + 1]),
            (SteamId(7), vec![3; MAX_DATAGRAM_BYTES]),
            (SteamId(9), vec![4; 512 * 1024]),
            (SteamId(8), vec![5; 600_000]),
            (SteamId(7), vec![6; 3]),
        ]
        .into_iter()
        .collect();
        let inbox = Arc::new(Inbox {
            waiting: Mutex::new(waiting),
            asked_max: Mutex::new(Vec::new()),
        });
        let mut socket = SteamSocket::open(inbox, 3).expect("open");
        let mut buf = vec![0u8; MAX_DATAGRAM_BYTES];
        let mut got = Vec::new();
        while let Some((n, from)) = socket.recv_from(&mut buf).expect("recv") {
            got.push((from, buf[..n].to_vec()));
        }
        let from7 = steam_peer_addr(SteamId(7), 3);
        assert_eq!(
            got,
            vec![
                (from7, vec![1; 8]),
                (from7, vec![3; MAX_DATAGRAM_BYTES]),
                (from7, vec![6; 3])
            ]
        );
        assert!(
            socket.scratch.len() <= 3 * MAX_DATAGRAM_BYTES,
            "nothing oversize kept"
        );
        let stats = socket.stats();
        assert_eq!((stats.received, stats.oversize), (3, 3));
        assert_eq!(
            socket
                .oversize_by_sender()
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect::<Vec<_>>(),
            vec![(SteamId(8), 1), (SteamId(9), 2)]
        );
    }

    /// Adversarial row 15: a send Steam refuses is an error from `send_to`, not `Ok`.
    #[test]
    fn a_refused_send_is_an_error() {
        struct Refuses;
        impl P2pMessages for Refuses {
            fn local_id(&self) -> Option<SteamId> {
                Some(SteamId(1))
            }
            fn send(&self, _to: SteamId, _channel: u16, _bytes: &[u8]) -> Result<(), SteamError> {
                Err(SteamError::Rejected("no session".into()))
            }
            fn receive(&self, _channel: u16, _max: usize) -> Vec<(SteamId, Vec<u8>)> {
                Vec::new()
            }
            fn set_accept(&self, _accept: SteamAccept) {}
        }
        let mut socket = SteamSocket::open(Arc::new(Refuses), 0).expect("open");
        assert!(socket.send_to(b"x", steam_peer_addr(SteamId(2), 0)).is_err());
        assert_eq!((socket.stats().sent, socket.stats().send_errors), (0, 1));
    }
}
