// SPDX-License-Identifier: MPL-2.0
//! The impairment network: an in-process datagram network whose links add latency, jitter, random
//! and bursty (Gilbert-Elliott) loss, reordering, duplication, a bandwidth cap with a drop-tail
//! queue, an MTU limit, link outages and NAT rebinding (ADR-7899).
//!
//! It plugs in at `regia_net`'s transport seam: every endpoint gets an [`ImpairedSocket`], a
//! [`DatagramSocket`] like `UdpTransport`, so `NetServer` and `NetClient` run unchanged on top. Time
//! is virtual (microseconds, set by the driver with [`ImpairedNet::advance_to_ms`]) and every
//! random decision comes from a SplitMix64 stream per endpoint and direction, seeded from the run
//! seed, so the same seed and the same traffic give the same drops, holds and arrival order on
//! every machine. Nothing here reads a wall clock.
//!
//! **Path model.** Each endpoint has an access link with an *up* direction (what it sends) and a
//! *down* direction (what it receives). A datagram from A to B crosses A's up direction at send
//! time, then B's down direction when it reaches B's side; each direction applies, in order: the
//! outage schedule, the MTU, random loss, Gilbert-Elliott burst loss, the bandwidth bottleneck
//! (serialization at `bandwidth_bps` behind a drop-tail queue of `queue_bytes`), the one-way
//! latency plus jitter, reordering and duplication.
//!
//! - **Jitter** is uniform in `0..=jitter_ms` and keeps order on its own (a datagram never
//!   arrives before the one sent ahead of it on the same direction), the way one network path
//!   behaves; reordering is separate, so its rate is the configured one.
//! - **Reordering** gives a datagram an extra hold of `1..=reorder_ms` exempt from the order
//!   clamp, so later datagrams overtake it.
//! - **Gilbert-Elliott**: a two-state Markov chain stepped once per datagram (good -> bad with
//!   `p_good_to_bad`, bad -> good with `p_bad_to_good`), each state with its own loss probability
//!   (E. N. Gilbert, "Capacity of a burst-noise channel", BSTJ 1960; E. O. Elliott, "Estimates of
//!   error rates for codes on burst-noise channels", BSTJ 1963; parameterized as in G. Hasslinger
//!   and O. Hohlfeld, "The Gilbert-Elliott Model for Packet Loss in Real Time Services on the
//!   Internet", MMB 2008). Mean burst length is `1 / p_bad_to_good` datagrams.
//! - **Bandwidth**: a bottleneck that serializes `bytes + 28` (IPv4 + UDP header) at
//!   `bandwidth_bps`; a datagram that would find more than `queue_bytes` waiting is dropped
//!   (drop-tail, the default queue discipline of RFC 7567 § 1, which that RFC asks to replace with
//!   AQM; a drop-tail queue is the harsher and more common case for a home or cell uplink).
//! - **MTU**: a UDP payload longer than `mtu` is dropped (no IP fragmentation, as with DF set or a
//!   path that drops fragments; RFC 8085 § 3.2).
//! - **Outages** (`flaps`): every datagram that enters either direction inside an outage window is
//!   dropped.
//! - **NAT rebinding**: at each rebinding time the endpoint's public port changes. Datagrams it
//!   sends carry the new source; datagrams still addressed to the old one are dropped (the NAT
//!   mapping is gone; RFC 4787 § 4.3 mapping refresh and RFC 9000 § 9.3 on peers that see a new
//!   address). An *address move* (`ip_moves`) changes the public IP as well (a phone leaving Wi-Fi
//!   for cellular, or a client moving behind a carrier-grade NAT).
//! - **Phases** (ADR-8030): a link may switch both directions to new numbers at given times (an RTT
//!   step, a path MTU that shrinks, Wi-Fi to cellular). The bottleneck's backlog and the burst
//!   chain carry over a switch.
//! - **Cross traffic** (ADR-8030): a direction with `cross_bps` feeds a constant-rate filler of
//!   [`CROSS_PACKET_BYTES`] datagrams into its own bottleneck queue (another device's bulk download
//!   on the same home link). The queue starts full and the filler, offered faster than the
//!   bottleneck drains, keeps it full, so a game datagram that finds room waits about the whole
//!   queue; the filler's phase comes from the direction's seeded stream.
//! - **Real UDP wire** (ADR-8454, [`ImpairedNet::with_real_udp`]): every endpoint also owns a
//!   real non-blocking UDP socket on 127.0.0.1, and each delivered datagram crosses the kernel's
//!   loopback UDP path: the network `sendto`s it from the sender's socket to the receiver's, and
//!   the receiver's [`ImpairedSocket::recv_from`] reads it with a real `recvfrom`. The impairment
//!   decisions, the virtual addresses the engine sees (so encryption, rebinding and the address
//!   checks behave as in the virtual mode) and the seeded schedule are unchanged; the real path
//!   adds the system calls, the kernel copies and the receive buffer. At most [`WIRE_UNREAD`]
//!   datagrams and [`WIRE_UNREAD_BYTES`] wait in one receive buffer (under the smallest default
//!   buffer, Windows' 64 KiB); the rest wait in the endpoint's inbox and are pushed when its
//!   reader finds the socket empty, so the run does not lose datagrams to the buffer size. A
//!   datagram the kernel drops anyway is counted (`WireStats::kernel_drops`) when the reader
//!   next finds the socket empty, and the bookkeeping starts over.
//! - **Clock rate** (ADR-8030): each endpoint's local clock runs at `1 + clock_ppm / 10^6` of the
//!   virtual (server) clock ([`LinkSpec::local_ms`]); the driver hands each endpoint its local time.

use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_net::DatagramSocket;
use serde::{Deserialize, Serialize};

use super::workload::Histogram;

/// Bytes of IPv4 + UDP header counted against a bandwidth cap for every datagram.
pub const IP_UDP_OVERHEAD: usize = 28;
/// Largest datagram a [`ImpairedSocket`] hands to its reader; longer ones are truncated like UDP
/// truncates into a short buffer.
pub const MAX_WIRE_DATAGRAM: usize = 65_507;
/// Real UDP wire: most datagrams pushed into one endpoint's kernel receive buffer and not yet read.
pub const WIRE_UNREAD: usize = 96;
/// Real UDP wire: most bytes pushed into one endpoint's kernel receive buffer and not yet read,
/// each datagram charged its length plus [`WIRE_DATAGRAM_OVERHEAD`]. Below the smallest default
/// receive buffer of the build hosts (Windows 64 KiB; Linux 208 KiB).
pub const WIRE_UNREAD_BYTES: usize = 48 * 1024;
/// Real UDP wire: what one datagram costs a kernel receive buffer beyond its payload (the
/// kernel's per-packet bookkeeping), bytes.
pub const WIRE_DATAGRAM_OVERHEAD: usize = 768;

/// Most datagrams one endpoint's inbox holds; more are dropped and counted (a full socket buffer).
pub const INBOX_LIMIT: usize = 16_384;
/// Most outbound datagrams a tapped endpoint keeps for a replay attacker.
pub const TAP_LIMIT: usize = 256;
/// Wire size of one cross-traffic datagram (IP and UDP headers included), bytes.
pub const CROSS_PACKET_BYTES: usize = 1_500;
/// Buckets (one per millisecond) of the queueing-delay histograms; the last holds everything at or
/// above it.
pub const QUEUE_DELAY_BUCKETS: usize = 2_048;

/// A SplitMix64 stream (S. Vigna, public domain reference; G. Steele et al., "Fast splittable
/// pseudorandom number generators", OOPSLA 2014).
#[derive(Debug, Clone)]
pub struct SoakRng(u64);

impl SoakRng {
    /// A stream from a seed.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// A stream for one `(seed, stream id)` pair; different ids give unrelated streams.
    pub fn derive(seed: u64, stream: u64) -> Self {
        let mut s = Self(seed ^ stream.wrapping_mul(0xD1B5_4A32_D192_ED03));
        s.next_u64();
        s
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform float in `[0, 1)` (53 bits).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// True with probability `p` (`p <= 0` is never, `p >= 1` always).
    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && (p >= 1.0 || self.unit() < p)
    }

    /// A uniform integer in `0..=max`.
    pub fn up_to(&mut self, max: u64) -> u64 {
        if max == 0 { 0 } else { self.next_u64() % (max + 1) }
    }
}

/// Gilbert-Elliott burst loss parameters (per datagram).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GilbertElliott {
    /// Probability of moving from the good to the bad state at each datagram.
    pub p_good_to_bad: f64,
    /// Probability of moving from the bad to the good state at each datagram.
    pub p_bad_to_good: f64,
    /// Loss probability in the good state.
    pub loss_good: f64,
    /// Loss probability in the bad state.
    pub loss_bad: f64,
}

impl GilbertElliott {
    /// Long-run loss fraction of the chain: `pi_bad * loss_bad + pi_good * loss_good`.
    pub fn mean_loss(&self) -> f64 {
        let denom = self.p_good_to_bad + self.p_bad_to_good;
        if denom <= 0.0 {
            return self.loss_good;
        }
        let pi_bad = self.p_good_to_bad / denom;
        pi_bad * self.loss_bad + (1.0 - pi_bad) * self.loss_good
    }
}

/// One direction of an access link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Direction {
    /// One-way latency added in this direction, milliseconds.
    pub latency_ms: u64,
    /// Extra latency, uniform in `0..=jitter_ms`, order-preserving.
    pub jitter_ms: u64,
    /// Independent random loss probability.
    pub loss: f64,
    /// Bursty loss on top of `loss`.
    pub burst: Option<GilbertElliott>,
    /// Probability that a datagram is held back so later ones overtake it.
    pub reorder: f64,
    /// Upper bound of the reorder hold, milliseconds (at least 1).
    pub reorder_ms: u64,
    /// Probability that a datagram is delivered twice.
    pub duplicate: f64,
    /// Bottleneck rate in bits per second; `0` is unlimited.
    pub bandwidth_bps: u64,
    /// Drop-tail queue in front of the bottleneck, bytes (ignored when unlimited).
    pub queue_bytes: usize,
    /// Largest UDP payload; longer datagrams are dropped. `0` is unlimited.
    pub mtu: usize,
    /// Cross traffic offered into this direction's bottleneck queue, bits per second (`0`: none;
    /// ignored when the direction is unlimited).
    #[serde(default)]
    pub cross_bps: u64,
}

impl Default for Direction {
    fn default() -> Self {
        Self::clean()
    }
}

impl Direction {
    /// No impairment at all.
    pub const fn clean() -> Self {
        Self {
            latency_ms: 0,
            jitter_ms: 0,
            loss: 0.0,
            burst: None,
            reorder: 0.0,
            reorder_ms: 0,
            duplicate: 0.0,
            bandwidth_bps: 0,
            queue_bytes: 0,
            mtu: 0,
            cross_bps: 0,
        }
    }
}

/// A switch of both directions of a link to new numbers at a given time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkPhase {
    /// When the new numbers apply, milliseconds of virtual time.
    pub at_ms: u64,
    /// The new up direction.
    pub up: Direction,
    /// The new down direction.
    pub down: Direction,
}

/// An access link: both directions, the outage schedule and the NAT rebinding times.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LinkSpec {
    /// What the endpoint sends.
    pub up: Direction,
    /// What the endpoint receives.
    pub down: Direction,
    /// Outage windows `[start_ms, end_ms)`: both directions drop everything inside.
    pub outages: Vec<(u64, u64)>,
    /// Times (ms) at which the endpoint's public port changes.
    pub rebinds_ms: Vec<u64>,
    /// Times (ms) at which the endpoint's public address moves to another IP (and a new port).
    #[serde(default)]
    pub ip_moves: Vec<(u64, IpAddr)>,
    /// Switches to new numbers, in time order.
    #[serde(default)]
    pub phases: Vec<LinkPhase>,
    /// How fast the endpoint's clock runs against the virtual clock, parts per million (`5_000` is
    /// 0.5 % fast).
    #[serde(default)]
    pub clock_ppm: i64,
}

impl LinkSpec {
    /// The endpoint's local clock at virtual time `virtual_ms`: `virtual_ms * (1 + clock_ppm /
    /// 10^6)`, never below zero.
    pub fn local_ms(&self, virtual_ms: u64) -> u64 {
        let local = i128::from(virtual_ms) + i128::from(virtual_ms) * i128::from(self.clock_ppm) / 1_000_000;
        local.clamp(0, i128::from(u64::MAX)) as u64
    }

    /// The up and down directions in force at `now_ms` (the last phase that started by then).
    pub fn directions_at(&self, now_ms: u64) -> (&Direction, &Direction) {
        self.phases
            .iter()
            .rev()
            .find(|p| p.at_ms <= now_ms)
            .map_or((&self.up, &self.down), |p| (&p.up, &p.down))
    }

    /// True when `now_ms` is inside an outage window.
    pub fn in_outage(&self, now_ms: u64) -> bool {
        self.outages.iter().any(|&(s, e)| now_ms >= s && now_ms < e)
    }

    /// The end of the outage covering `now_ms`, if any.
    pub fn outage_end(&self, now_ms: u64) -> Option<u64> {
        self.outages
            .iter()
            .find(|&&(s, e)| now_ms >= s && now_ms < e)
            .map(|&(_, e)| e)
    }

    /// True when any outage window overlaps `[from_ms, to_ms)`.
    pub fn outage_overlaps(&self, from_ms: u64, to_ms: u64) -> bool {
        self.outages.iter().any(|&(s, e)| s < to_ms && e > from_ms)
    }
}

/// Why a datagram did not arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DropCause {
    /// Independent random loss.
    Random,
    /// Gilbert-Elliott burst loss.
    Burst,
    /// The bottleneck queue was full.
    Queue,
    /// Longer than the path MTU.
    Mtu,
    /// Inside an outage window.
    Outage,
    /// Addressed to a public address no endpoint holds (a stale NAT mapping or a closed port).
    NoRoute,
    /// The receiver's inbox was full.
    Inbox,
}

impl DropCause {
    /// Every cause, for reports.
    pub const ALL: [DropCause; 7] = [
        Self::Random,
        Self::Burst,
        Self::Queue,
        Self::Mtu,
        Self::Outage,
        Self::NoRoute,
        Self::Inbox,
    ];

    /// Lowercase code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Burst => "burst",
            Self::Queue => "queue",
            Self::Mtu => "mtu",
            Self::Outage => "outage",
            Self::NoRoute => "no_route",
            Self::Inbox => "inbox",
        }
    }
}

/// Most read traces one endpoint keeps between two [`ImpairedNet::take_read_traces`] calls.
const READ_TRACE_LIMIT: usize = 4_096;

/// Where one delivered datagram spent its time on the way, in microseconds of virtual time: the
/// network's part of the soak's staleness stages (ADR-10145). Recorded on the virtual wire only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatagramTrace {
    /// When the sender's socket took it.
    pub sent_us: u64,
    /// What it waited in the bottleneck queues of its route (the sender's up direction and the
    /// receiver's down direction) before its own transmission started.
    pub queue_us: u64,
    /// Its own transmission time at those bottlenecks.
    pub serialize_us: u64,
    /// When it reached the receiver's inbox. The rest of `arrive_us - sent_us` is the path:
    /// latency, jitter and any reordering hold.
    pub arrive_us: u64,
}

/// Counters of one endpoint.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EndpointStats {
    /// Datagrams the endpoint sent.
    pub sent: u64,
    /// Bytes (UDP payload) the endpoint sent.
    pub sent_bytes: u64,
    /// Datagrams delivered to the endpoint.
    pub received: u64,
    /// Bytes delivered to the endpoint.
    pub received_bytes: u64,
    /// Bytes offered to this endpoint's down direction (arrived at its access link), per second of
    /// virtual time.
    pub down_offered_per_s: Vec<u64>,
    /// Bytes offered to this endpoint's up direction, per second of virtual time.
    pub up_offered_per_s: Vec<u64>,
    /// Drops on this endpoint's directions, by cause (index of [`DropCause::ALL`]).
    pub drops: [u64; 7],
    /// Duplicates made on this endpoint's directions.
    pub duplicated: u64,
    /// Datagrams held for reordering on this endpoint's directions.
    pub reordered: u64,
    /// Largest queueing delay at this endpoint's bottlenecks, microseconds.
    pub max_queue_delay_us: u64,
    /// Largest datagram the endpoint sent, bytes.
    pub largest_sent: usize,
    /// Datagrams offered to this endpoint's down direction, per second of virtual time.
    #[serde(default)]
    pub down_offered_datagrams_per_s: Vec<u64>,
    /// Datagrams offered to this endpoint's up direction, per second of virtual time.
    #[serde(default)]
    pub up_offered_datagrams_per_s: Vec<u64>,
    /// Queueing delay of every datagram that passed the up bottleneck, milliseconds (one bucket per
    /// millisecond, [`QUEUE_DELAY_BUCKETS`]).
    #[serde(default)]
    pub up_queue_delay_ms: Histogram,
    /// Queueing delay of every datagram that passed the down bottleneck, milliseconds.
    #[serde(default)]
    pub down_queue_delay_ms: Histogram,
    /// Cross-traffic datagrams admitted to this endpoint's bottleneck queues.
    #[serde(default)]
    pub cross_admitted: u64,
    /// Cross-traffic datagrams the full queue dropped.
    #[serde(default)]
    pub cross_dropped: u64,
}

impl EndpointStats {
    fn add_drop(&mut self, cause: DropCause) {
        self.drops[cause as usize] += 1;
    }

    /// Drops of one cause.
    pub fn drops_of(&self, cause: DropCause) -> u64 {
        self.drops[cause as usize]
    }
}

fn add_delay(h: &mut Histogram, delay_us: u64) {
    if h.buckets.len() < QUEUE_DELAY_BUCKETS {
        h.buckets.resize(QUEUE_DELAY_BUCKETS, 0);
    }
    h.add(delay_us / 1_000);
}

#[derive(Debug)]
struct DirState {
    spec: Direction,
    /// Later numbers `(at_us, spec)`, in time order; applied as time passes.
    phases: VecDeque<(u64, Direction)>,
    rng: SoakRng,
    bad: bool,
    busy_until_us: u64,
    last_arrival_us: u64,
    /// Next cross-traffic arrival; `None` until the first datagram (the queue starts full then).
    next_cross_us: Option<u64>,
    /// This is the down direction (for the stats it records into).
    down: bool,
    /// What the latest datagram [`DirState::pass`] let through waited in the queue, and its own
    /// transmission time, microseconds (0 without a bandwidth cap).
    last_wait_us: u64,
    last_tx_us: u64,
}

impl DirState {
    fn new(spec: Direction, rng: SoakRng) -> Self {
        Self {
            spec,
            phases: VecDeque::new(),
            rng,
            bad: false,
            busy_until_us: 0,
            last_arrival_us: 0,
            next_cross_us: None,
            down: false,
            last_wait_us: 0,
            last_tx_us: 0,
        }
    }

    /// Applies every phase that started by `now_us`.
    fn apply_phases(&mut self, now_us: u64) {
        while self.phases.front().is_some_and(|(at, _)| *at <= now_us) {
            if let Some((_, spec)) = self.phases.pop_front() {
                self.spec = spec;
            }
        }
    }

    /// Feeds the cross-traffic filler into the bottleneck queue up to `now_us`.
    fn fill_cross(&mut self, now_us: u64, stats: &mut EndpointStats) {
        let (bw, cross) = (self.spec.bandwidth_bps, self.spec.cross_bps);
        if bw == 0 || cross == 0 {
            return;
        }
        let bits = (CROSS_PACKET_BYTES * 8) as u64;
        let period_us = (bits * 1_000_000).div_ceil(cross).max(1);
        let tx_us = (bits * 1_000_000).div_ceil(bw);
        let next = match self.next_cross_us {
            Some(t) => t,
            None => {
                // A bulk download already running: the queue is full when the run starts.
                let full_us = (self.spec.queue_bytes as u64 * 8 * 1_000_000) / bw;
                self.busy_until_us = self.busy_until_us.max(now_us + full_us);
                now_us + self.rng.up_to(period_us - 1)
            }
        };
        let mut t = next;
        while t <= now_us {
            let backlog_bytes = self.busy_until_us.saturating_sub(t).saturating_mul(bw) / 8_000_000;
            if backlog_bytes as usize + CROSS_PACKET_BYTES > self.spec.queue_bytes.max(CROSS_PACKET_BYTES) {
                stats.cross_dropped += 1;
            } else {
                stats.cross_admitted += 1;
                self.busy_until_us = self.busy_until_us.max(t) + tx_us;
            }
            t += period_us;
        }
        self.next_cross_us = Some(t);
    }

    /// Runs one datagram through this direction at `now_us`: the arrival times at the far end
    /// (one, or two when duplicated), or the drop cause.
    fn pass(&mut self, len: usize, now_us: u64, stats: &mut EndpointStats) -> Result<Vec<u64>, DropCause> {
        self.apply_phases(now_us);
        self.fill_cross(now_us, stats);
        let spec = &self.spec;
        if spec.mtu > 0 && len > spec.mtu {
            return Err(DropCause::Mtu);
        }
        // Step the burst chain once per datagram, whether or not the random draw drops it, so the
        // chain's state sequence depends only on the datagram count.
        let burst_drop = match spec.burst {
            Some(ge) => {
                self.bad = if self.bad {
                    !self.rng.chance(ge.p_bad_to_good)
                } else {
                    self.rng.chance(ge.p_good_to_bad)
                };
                self.rng.chance(if self.bad { ge.loss_bad } else { ge.loss_good })
            }
            None => false,
        };
        if self.rng.chance(spec.loss) {
            return Err(DropCause::Random);
        }
        if burst_drop {
            return Err(DropCause::Burst);
        }
        let mut depart_us = now_us;
        (self.last_wait_us, self.last_tx_us) = (0, 0);
        if spec.bandwidth_bps > 0 {
            let wire_bits = ((len + IP_UDP_OVERHEAD) * 8) as u64;
            let backlog_us = self.busy_until_us.saturating_sub(now_us);
            let backlog_bytes = backlog_us.saturating_mul(spec.bandwidth_bps) / 8_000_000;
            if backlog_bytes as usize + len + IP_UDP_OVERHEAD > spec.queue_bytes.max(len + IP_UDP_OVERHEAD) {
                return Err(DropCause::Queue);
            }
            let tx_us = (wire_bits * 1_000_000).div_ceil(spec.bandwidth_bps);
            let start = self.busy_until_us.max(now_us);
            self.busy_until_us = start + tx_us;
            depart_us = self.busy_until_us;
            stats.max_queue_delay_us = stats.max_queue_delay_us.max(start - now_us);
            let h = if self.down {
                &mut stats.down_queue_delay_ms
            } else {
                &mut stats.up_queue_delay_ms
            };
            add_delay(h, start - now_us);
            (self.last_wait_us, self.last_tx_us) = (start - now_us, tx_us);
        }
        let jitter_us = self.rng.up_to(spec.jitter_ms) * 1000;
        let mut arrive_us = depart_us + spec.latency_ms * 1000 + jitter_us;
        if self.rng.chance(spec.reorder) {
            stats.reordered += 1;
            arrive_us += (1 + self.rng.up_to(spec.reorder_ms.saturating_sub(1))) * 1000;
        } else {
            arrive_us = arrive_us.max(self.last_arrival_us);
            self.last_arrival_us = arrive_us;
        }
        let mut out = vec![arrive_us];
        if self.rng.chance(spec.duplicate) {
            stats.duplicated += 1;
            out.push(arrive_us + 1 + self.rng.up_to(2_000));
        }
        Ok(out)
    }
}

#[derive(Debug)]
struct Endpoint {
    public: SocketAddr,
    next_port: u16,
    spec: LinkSpec,
    up: DirState,
    down: DirState,
    /// Address changes `(at_ms, new IP or None for a new port only)`, in time order.
    rebinds: VecDeque<(u64, Option<IpAddr>)>,
    /// Every public address the endpoint has held, oldest first.
    held: Vec<SocketAddr>,
    /// Delivered datagrams waiting to be read: `(bytes, virtual source, sending endpoint)`.
    inbox: VecDeque<(Vec<u8>, SocketAddr, usize)>,
    /// The trace of every datagram in `inbox`, in the same order (virtual wire only; empty on
    /// the real UDP wire, where the kernel reorders the reads).
    inbox_traces: VecDeque<DatagramTrace>,
    /// The traces of the datagrams read since [`ImpairedNet::take_read_traces`], when
    /// [`ImpairedNet::trace_reads`] asked for them.
    read_traces: Option<Vec<DatagramTrace>>,
    stats: EndpointStats,
    tap: Option<VecDeque<Vec<u8>>>,
}

/// Where a datagram in flight is.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    /// Crossed the sender's up direction; enters the receiver's down direction on arrival.
    AtReceiverEdge,
    /// Crossed both; goes into the inbox on arrival.
    Deliver(usize),
}

#[derive(Debug, PartialEq, Eq)]
struct InFlight {
    at_us: u64,
    order: u64,
    stage: Stage,
    dest: SocketAddr,
    from: SocketAddr,
    /// The sending endpoint.
    src: usize,
    bytes: Vec<u8>,
    trace: DatagramTrace,
}

impl Ord for InFlight {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap on (time, order).
        (other.at_us, other.order).cmp(&(self.at_us, self.order))
    }
}

impl PartialOrd for InFlight {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Default)]
struct Core {
    seed: u64,
    now_us: u64,
    endpoints: Vec<Endpoint>,
    by_addr: HashMap<SocketAddr, usize>,
    flight: BinaryHeap<InFlight>,
    order: u64,
    /// The real UDP wire, when the network was made with [`ImpairedNet::with_real_udp`].
    wire: Option<Wire>,
}

/// Counters of the real UDP wire ([`ImpairedNet::wire_stats`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStats {
    /// Datagrams sent through a real socket.
    pub sent: u64,
    /// Datagrams read back from a real socket.
    pub read: u64,
    /// Reads whose length did not match the datagram the network expected from that sender
    /// (the kernel reordered or dropped one); the read is delivered anyway.
    pub mismatched: u64,
    /// Reads from a port that is not an endpoint's (dropped).
    pub unknown_source: u64,
    /// `sendto` failures other than a full buffer (the datagram is dropped).
    pub send_errors: u64,
    /// Datagrams pushed that the receiver's socket never returned (the kernel dropped them),
    /// counted when the socket reads empty.
    #[serde(default)]
    pub kernel_drops: u64,
}

/// One real socket per endpoint and what is in flight on the kernel path.
#[derive(Debug)]
struct Wire {
    sockets: Vec<UdpSocket>,
    addrs: Vec<SocketAddr>,
    by_port: HashMap<u16, usize>,
    unread: Vec<usize>,
    unread_bytes: Vec<usize>,
    /// Per receiving endpoint, per sending endpoint: `(length, virtual source)` of each datagram
    /// pushed and not yet read, in push order.
    pending: Vec<HashMap<usize, VecDeque<(usize, SocketAddr)>>>,
    stats: WireStats,
}

impl Wire {
    fn add(&mut self) -> io::Result<()> {
        let sock = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        sock.set_nonblocking(true)?;
        let addr = sock.local_addr()?;
        self.by_port.insert(addr.port(), self.sockets.len());
        self.sockets.push(sock);
        self.addrs.push(addr);
        self.unread.push(0);
        self.unread_bytes.push(0);
        self.pending.push(HashMap::new());
        Ok(())
    }
}

impl Core {
    fn second(&self) -> usize {
        (self.now_us / 1_000_000) as usize
    }

    fn bump(per_s: &mut Vec<u64>, second: usize, bytes: usize) {
        if per_s.len() <= second {
            per_s.resize(second + 1, 0);
        }
        per_s[second] += bytes as u64;
    }

    fn rebind_due(&mut self) {
        let now_ms = self.now_us / 1000;
        for i in 0..self.endpoints.len() {
            while self.endpoints[i]
                .rebinds
                .front()
                .is_some_and(|&(t, _)| t <= now_ms)
            {
                let ip = self.endpoints[i].rebinds.pop_front().and_then(|(_, ip)| ip);
                self.rebind(i, ip);
            }
        }
    }

    fn rebind(&mut self, ep: usize, ip: Option<IpAddr>) {
        let e = &mut self.endpoints[ep];
        let old = e.public;
        let ip = ip.unwrap_or(old.ip());
        let mut port = e.next_port;
        let new = loop {
            port = port.wrapping_add(1).max(20_000);
            let candidate = SocketAddr::new(ip, port);
            if !self.by_addr.contains_key(&candidate) {
                break candidate;
            }
        };
        e.next_port = port;
        e.public = new;
        e.held.push(new);
        self.by_addr.remove(&old);
        self.by_addr.insert(new, ep);
    }

    fn send(&mut self, ep: usize, bytes: &[u8], dest: SocketAddr, forged_from: Option<SocketAddr>) {
        let now_us = self.now_us;
        let second = self.second();
        let e = &mut self.endpoints[ep];
        let from = forged_from.unwrap_or(e.public);
        e.stats.sent += 1;
        e.stats.sent_bytes += bytes.len() as u64;
        e.stats.largest_sent = e.stats.largest_sent.max(bytes.len());
        Self::bump(&mut e.stats.up_offered_per_s, second, bytes.len());
        Self::bump(&mut e.stats.up_offered_datagrams_per_s, second, 1);
        if let Some(tap) = &mut e.tap {
            if tap.len() >= TAP_LIMIT {
                tap.pop_front();
            }
            tap.push_back(bytes.to_vec());
        }
        if e.spec.in_outage(now_us / 1000) {
            e.stats.add_drop(DropCause::Outage);
            return;
        }
        let arrivals = match e.up.pass(bytes.len(), now_us, &mut e.stats) {
            Ok(a) => a,
            Err(cause) => {
                e.stats.add_drop(cause);
                return;
            }
        };
        let trace = DatagramTrace {
            sent_us: now_us,
            queue_us: e.up.last_wait_us,
            serialize_us: e.up.last_tx_us,
            arrive_us: 0,
        };
        for at_us in arrivals {
            self.order += 1;
            self.flight.push(InFlight {
                at_us,
                order: self.order,
                stage: Stage::AtReceiverEdge,
                dest,
                from,
                src: ep,
                bytes: bytes.to_vec(),
                trace,
            });
        }
    }

    /// Real UDP wire: moves datagrams from `dest`'s inbox into its kernel receive buffer, from
    /// each sender's real socket, while fewer than [`WIRE_UNREAD`] wait there unread.
    fn push_wire(&mut self, dest: usize) {
        let Some(w) = self.wire.as_mut() else { return };
        let e = &mut self.endpoints[dest];
        while w.unread[dest] < WIRE_UNREAD {
            let Some(next) = e.inbox.front() else { break };
            let charge = next.0.len() + WIRE_DATAGRAM_OVERHEAD;
            // One datagram always fits an empty buffer, whatever its size.
            if w.unread[dest] > 0 && w.unread_bytes[dest] + charge > WIRE_UNREAD_BYTES {
                break;
            }
            let Some((bytes, from, src)) = e.inbox.pop_front() else {
                break;
            };
            match w.sockets[src].send_to(&bytes, w.addrs[dest]) {
                Ok(_) => {
                    w.stats.sent += 1;
                    w.unread[dest] += 1;
                    w.unread_bytes[dest] += charge;
                    w.pending[dest]
                        .entry(src)
                        .or_default()
                        .push_back((bytes.len(), from));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    e.inbox.push_front((bytes, from, src));
                    break;
                }
                Err(_) => w.stats.send_errors += 1,
            }
        }
    }

    /// Real UDP wire: reads one datagram from `ep`'s real socket; `None` when nothing is waiting
    /// in the kernel or the inbox.
    fn recv_wire(&mut self, ep: usize, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut pushed = false;
        loop {
            let Some(w) = self.wire.as_mut() else {
                return Ok(None);
            };
            match w.sockets[ep].recv_from(buf) {
                Ok((n, real_src)) => {
                    w.stats.read += 1;
                    w.unread[ep] = w.unread[ep].saturating_sub(1);
                    let Some(&src) = w.by_port.get(&real_src.port()) else {
                        w.stats.unknown_source += 1;
                        continue;
                    };
                    let Some((len, from)) = w.pending[ep].get_mut(&src).and_then(VecDeque::pop_front) else {
                        w.stats.unknown_source += 1;
                        continue;
                    };
                    w.unread_bytes[ep] = w.unread_bytes[ep].saturating_sub(len + WIRE_DATAGRAM_OVERHEAD);
                    if len.min(buf.len()) != n {
                        w.stats.mismatched += 1;
                    }
                    return Ok(Some((n, from)));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    // The buffer is empty: whatever was pushed and not read, the kernel dropped.
                    let lost: usize = w.pending[ep].values().map(VecDeque::len).sum();
                    if lost > 0 {
                        w.stats.kernel_drops += lost as u64;
                        w.pending[ep].clear();
                    }
                    w.unread[ep] = 0;
                    w.unread_bytes[ep] = 0;
                    if pushed || self.endpoints[ep].inbox.is_empty() {
                        return Ok(None);
                    }
                    pushed = true;
                    self.push_wire(ep);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
    }

    fn advance_to(&mut self, now_us: u64) {
        while self.flight.peek().is_some_and(|f| f.at_us <= now_us) {
            let Some(f) = self.flight.pop() else { break };
            self.now_us = self.now_us.max(f.at_us);
            self.rebind_due();
            match f.stage {
                Stage::AtReceiverEdge => {
                    let Some(&dest) = self.by_addr.get(&f.dest) else {
                        // Charged to the sender: nobody owns the address any more.
                        if let Some(&src) = self.by_addr.get(&f.from) {
                            self.endpoints[src].stats.add_drop(DropCause::NoRoute);
                        }
                        continue;
                    };
                    let second = self.second();
                    let t = f.at_us;
                    let e = &mut self.endpoints[dest];
                    Self::bump(&mut e.stats.down_offered_per_s, second, f.bytes.len());
                    Self::bump(&mut e.stats.down_offered_datagrams_per_s, second, 1);
                    if e.spec.in_outage(t / 1000) {
                        e.stats.add_drop(DropCause::Outage);
                        continue;
                    }
                    let arrivals = match e.down.pass(f.bytes.len(), t, &mut e.stats) {
                        Ok(arrivals) => arrivals,
                        Err(cause) => {
                            e.stats.add_drop(cause);
                            continue;
                        }
                    };
                    let trace = DatagramTrace {
                        queue_us: f.trace.queue_us + e.down.last_wait_us,
                        serialize_us: f.trace.serialize_us + e.down.last_tx_us,
                        ..f.trace
                    };
                    for at_us in arrivals {
                        self.order += 1;
                        self.flight.push(InFlight {
                            at_us,
                            order: self.order,
                            stage: Stage::Deliver(dest),
                            dest: f.dest,
                            from: f.from,
                            src: f.src,
                            bytes: f.bytes.clone(),
                            trace,
                        });
                    }
                }
                Stage::Deliver(dest) => {
                    let virtual_wire = self.wire.is_none();
                    let e = &mut self.endpoints[dest];
                    // A rebinding between the edge and the inbox orphans the datagram too.
                    if e.public != f.dest {
                        e.stats.add_drop(DropCause::NoRoute);
                        continue;
                    }
                    if e.inbox.len() >= INBOX_LIMIT {
                        e.stats.add_drop(DropCause::Inbox);
                        continue;
                    }
                    e.stats.received += 1;
                    e.stats.received_bytes += f.bytes.len() as u64;
                    e.inbox.push_back((f.bytes, f.from, f.src));
                    if virtual_wire {
                        e.inbox_traces.push_back(DatagramTrace {
                            arrive_us: f.at_us,
                            ..f.trace
                        });
                    }
                    self.push_wire(dest);
                }
            }
        }
        self.now_us = self.now_us.max(now_us);
        self.rebind_due();
    }
}

/// The impairment network. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct ImpairedNet {
    core: Arc<Mutex<Core>>,
}

/// Handle of one endpoint on an [`ImpairedNet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EndpointId(pub usize);

impl ImpairedNet {
    /// An empty network whose random streams derive from `seed`.
    pub fn new(seed: u64) -> Self {
        let net = Self::default();
        net.lock().seed = seed;
        net
    }

    /// An empty network like [`Self::new`] whose delivered datagrams cross real UDP sockets on
    /// 127.0.0.1 (see the module docs, "Real UDP wire"). Every later [`Self::add_endpoint`] binds
    /// one socket.
    pub fn with_real_udp(seed: u64) -> Self {
        let net = Self::new(seed);
        net.lock().wire = Some(Wire {
            sockets: Vec::new(),
            addrs: Vec::new(),
            by_port: HashMap::new(),
            unread: Vec::new(),
            unread_bytes: Vec::new(),
            pending: Vec::new(),
            stats: WireStats::default(),
        });
        net
    }

    /// Whether datagrams cross real UDP sockets ([`Self::with_real_udp`]).
    pub fn is_real_udp(&self) -> bool {
        self.lock().wire.is_some()
    }

    /// The real UDP wire's counters; `None` on a virtual network.
    pub fn wire_stats(&self) -> Option<WireStats> {
        self.lock().wire.as_ref().map(|w| w.stats.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds an endpoint at public address `addr` with access link `spec`. Fails when the address
    /// is taken.
    pub fn add_endpoint(&self, addr: SocketAddr, spec: LinkSpec) -> io::Result<EndpointId> {
        let mut core = self.lock();
        if core.by_addr.contains_key(&addr) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "impaired address in use",
            ));
        }
        if let Some(w) = core.wire.as_mut() {
            w.add()?;
        }
        let id = core.endpoints.len();
        let seed = core.seed;
        let mut rebinds: Vec<(u64, Option<IpAddr>)> = spec.rebinds_ms.iter().map(|&t| (t, None)).collect();
        rebinds.extend(spec.ip_moves.iter().map(|&(t, ip)| (t, Some(ip))));
        rebinds.sort_by_key(|&(t, ip)| (t, ip.is_some()));
        let mut phases: Vec<&LinkPhase> = spec.phases.iter().collect();
        phases.sort_by_key(|p| p.at_ms);
        let mut up = DirState::new(spec.up.clone(), SoakRng::derive(seed, (id as u64) << 1));
        let mut down = DirState::new(spec.down.clone(), SoakRng::derive(seed, ((id as u64) << 1) | 1));
        down.down = true;
        for p in phases {
            up.phases.push_back((p.at_ms * 1000, p.up.clone()));
            down.phases.push_back((p.at_ms * 1000, p.down.clone()));
        }
        core.endpoints.push(Endpoint {
            public: addr,
            next_port: addr.port(),
            up,
            down,
            spec,
            rebinds: rebinds.into(),
            held: vec![addr],
            inbox: VecDeque::new(),
            inbox_traces: VecDeque::new(),
            read_traces: None,
            stats: EndpointStats::default(),
            tap: None,
        });
        core.by_addr.insert(addr, id);
        Ok(EndpointId(id))
    }

    /// A socket for `ep`. Several sockets of one endpoint share its address and inbox.
    pub fn socket(&self, ep: EndpointId) -> ImpairedSocket {
        ImpairedSocket {
            net: self.clone(),
            ep: ep.0,
        }
    }

    /// Advances virtual time to `now_ms`, delivering every datagram due by then.
    pub fn advance_to_ms(&self, now_ms: u64) {
        self.lock().advance_to(now_ms * 1000);
    }

    /// Current virtual time, milliseconds.
    pub fn now_ms(&self) -> u64 {
        self.lock().now_us / 1000
    }

    /// Advances virtual time to `now_us` microseconds, delivering every datagram due by then (the
    /// time harness's sub-tick delivery, ADR-8185).
    pub fn advance_to_us(&self, now_us: u64) {
        self.lock().advance_to(now_us);
    }

    /// Current virtual time, microseconds.
    pub fn now_us(&self) -> u64 {
        self.lock().now_us
    }

    /// Moves `ep` to a new public port now (a NAT rebinding, or a new socket on reconnect).
    pub fn rebind_now(&self, ep: EndpointId) -> SocketAddr {
        let mut core = self.lock();
        core.rebind(ep.0, None);
        core.endpoints[ep.0].public
    }

    /// Every public address `ep` has held, oldest first (the harness's ground truth for which
    /// addresses a peer may legitimately move to).
    pub fn addresses_held(&self, ep: EndpointId) -> Vec<SocketAddr> {
        self.lock().endpoints[ep.0].held.clone()
    }

    /// Sends `bytes` from `ep`'s link but with the forged source address `from` (a spoofing
    /// attacker: the reply goes to `from`, wherever that is).
    pub fn send_forged(&self, ep: EndpointId, from: SocketAddr, bytes: &[u8], dest: SocketAddr) {
        self.lock().send(ep.0, bytes, dest, Some(from));
    }

    /// The public address of `ep`.
    pub fn public_addr(&self, ep: EndpointId) -> SocketAddr {
        self.lock().endpoints[ep.0].public
    }

    /// The counters of `ep`.
    pub fn stats(&self, ep: EndpointId) -> EndpointStats {
        self.lock().endpoints[ep.0].stats.clone()
    }

    /// The link spec of `ep`.
    pub fn spec(&self, ep: EndpointId) -> LinkSpec {
        self.lock().endpoints[ep.0].spec.clone()
    }

    /// Starts keeping the last [`TAP_LIMIT`] datagrams `ep` sends (for a replay attacker).
    pub fn tap(&self, ep: EndpointId) {
        self.lock().endpoints[ep.0].tap = Some(VecDeque::new());
    }

    /// The datagrams kept by [`Self::tap`], oldest first.
    pub fn tapped(&self, ep: EndpointId) -> Vec<Vec<u8>> {
        self.lock().endpoints[ep.0]
            .tap
            .as_ref()
            .map(|t| t.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Starts recording the [`DatagramTrace`] of every datagram `ep` reads (virtual wire only):
    /// [`Self::take_read_traces`] hands them over.
    pub fn trace_reads(&self, ep: EndpointId) {
        let mut core = self.lock();
        if core.wire.is_none() {
            core.endpoints[ep.0].read_traces.get_or_insert_with(Vec::new);
        }
    }

    /// Moves the traces of the datagrams `ep` read since the last call into `out` (cleared
    /// first), in read order. Nothing without [`Self::trace_reads`].
    pub fn take_read_traces(&self, ep: EndpointId, out: &mut Vec<DatagramTrace>) {
        out.clear();
        if let Some(read) = self.lock().endpoints[ep.0].read_traces.as_mut() {
            std::mem::swap(read, out);
        }
    }

    /// Datagrams in flight.
    pub fn in_flight(&self) -> usize {
        self.lock().flight.len()
    }

    /// Bytes waiting in every inbox and in flight (the network's own memory, for reports).
    pub fn buffered_bytes(&self) -> usize {
        let core = self.lock();
        core.flight.iter().map(|f| f.bytes.len()).sum::<usize>()
            + core
                .endpoints
                .iter()
                .flat_map(|e| e.inbox.iter())
                .map(|(b, _, _)| b.len())
                .sum::<usize>()
    }
}

/// One endpoint's socket on an [`ImpairedNet`]. Non-blocking like `UdpTransport`.
#[derive(Debug, Clone)]
pub struct ImpairedSocket {
    net: ImpairedNet,
    ep: usize,
}

impl ImpairedSocket {
    /// The endpoint this socket belongs to.
    pub fn endpoint(&self) -> EndpointId {
        EndpointId(self.ep)
    }
}

impl DatagramSocket for ImpairedSocket {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        if bytes.len() > MAX_WIRE_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram longer than UDP allows",
            ));
        }
        super::probe::harness(|| self.net.lock().send(self.ep, bytes, dest, None));
        Ok(())
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut core = self.net.lock();
        if core.wire.is_some() {
            return core.recv_wire(self.ep, buf);
        }
        let e = &mut core.endpoints[self.ep];
        let Some((bytes, from, _)) = e.inbox.pop_front() else {
            return Ok(None);
        };
        if let (Some(trace), Some(read)) = (e.inbox_traces.pop_front(), e.read_traces.as_mut())
            && read.len() < READ_TRACE_LIMIT
        {
            read.push(trace);
        }
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(Some((n, from)))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.net.public_addr(EndpointId(self.ep)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], port))
    }

    fn pump(net: &ImpairedNet, ms: u64) {
        let start = net.now_ms();
        for t in start..=start + ms {
            net.advance_to_ms(t);
        }
    }

    fn drain(s: &mut ImpairedSocket) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while let Some((n, _)) = s.recv_from(&mut buf).expect("io") {
            out.push(buf[..n].to_vec());
        }
        out
    }

    #[test]
    fn clean_link_delivers_in_order_after_latency() {
        let net = ImpairedNet::new(1);
        let spec = LinkSpec {
            up: Direction {
                latency_ms: 20,
                ..Direction::clean()
            },
            ..LinkSpec::default()
        };
        let a = net.add_endpoint(addr(1, 1), spec).expect("a");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let mut sa = net.socket(a);
        let mut sb = net.socket(b);
        for i in 0..10u8 {
            sa.send_to(&[i], addr(2, 1)).expect("send");
        }
        net.advance_to_ms(19);
        assert!(drain(&mut sb).is_empty(), "nothing before the latency");
        net.advance_to_ms(20);
        let got = drain(&mut sb);
        assert_eq!(got, (0..10u8).map(|i| vec![i]).collect::<Vec<_>>());
    }

    /// The real UDP wire (ADR-8454) delivers what the virtual wire does: more datagrams than one
    /// receive buffer may hold unread all arrive, in order, from the sender's virtual address, and
    /// a rebound sender's datagrams carry its new address.
    #[test]
    fn real_udp_wire_delivers_in_order_with_virtual_sources() {
        let net = ImpairedNet::with_real_udp(3);
        assert!(net.is_real_udp());
        let a = net.add_endpoint(addr(1, 1), LinkSpec::default()).expect("a");
        let c = net.add_endpoint(addr(3, 1), LinkSpec::default()).expect("c");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let mut sa = net.socket(a);
        let mut sc = net.socket(c);
        let mut sb = net.socket(b);
        let n = WIRE_UNREAD * 3 + 7;
        for i in 0..n {
            let bytes = (i as u32).to_le_bytes();
            sa.send_to(&bytes, addr(2, 1)).expect("send a");
            sc.send_to(&bytes[..3], addr(2, 1)).expect("send c");
        }
        net.advance_to_ms(1);
        let mut from_a = Vec::new();
        let mut from_c = 0;
        let mut buf = [0u8; 64];
        while let Some((len, from)) = sb.recv_from(&mut buf).expect("recv") {
            if from == addr(1, 1) {
                assert_eq!(len, 4);
                from_a.push(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]));
            } else {
                assert_eq!(from, addr(3, 1));
                assert_eq!(len, 3);
                from_c += 1;
            }
        }
        assert_eq!(from_a, (0..n as u32).collect::<Vec<_>>());
        assert_eq!(from_c, n);
        let w = net.wire_stats().expect("wire");
        assert_eq!((w.sent, w.read), (2 * n as u64, 2 * n as u64));
        assert_eq!(
            (w.mismatched, w.unknown_source, w.send_errors, w.kernel_drops),
            (0, 0, 0, 0)
        );
        // Large datagrams are paced by bytes: 200 of 8 KiB (1.6 MiB, far over any default
        // buffer) all arrive.
        let big = vec![7u8; 8 * 1024];
        for _ in 0..200 {
            sa.send_to(&big, addr(2, 1)).expect("send big");
        }
        net.advance_to_ms(2);
        let mut large = [0u8; 16 * 1024];
        let mut got = 0;
        while let Some((len, from)) = sb.recv_from(&mut large).expect("recv big") {
            assert_eq!((len, from), (big.len(), addr(1, 1)));
            got += 1;
        }
        assert_eq!(got, 200);
        let w = net.wire_stats().expect("wire");
        assert_eq!((w.mismatched, w.kernel_drops), (0, 0), "{w:?}");
        // A NAT rebinding moves the virtual source; the real socket stays.
        let moved = net.rebind_now(a);
        sa.send_to(b"x", addr(2, 1)).expect("send");
        net.advance_to_ms(3);
        let got = sb.recv_from(&mut buf).expect("recv").map(|(_, f)| f);
        assert_eq!(got, Some(moved));
        assert!(ImpairedNet::new(3).wire_stats().is_none());
    }

    #[test]
    fn same_seed_same_fate_and_different_seed_differs() {
        let run = |seed: u64| {
            let net = ImpairedNet::new(seed);
            let lossy = Direction {
                latency_ms: 30,
                jitter_ms: 20,
                loss: 0.1,
                burst: Some(GilbertElliott {
                    p_good_to_bad: 0.05,
                    p_bad_to_good: 0.3,
                    loss_good: 0.0,
                    loss_bad: 0.7,
                }),
                reorder: 0.05,
                reorder_ms: 40,
                duplicate: 0.02,
                ..Direction::clean()
            };
            let a = net
                .add_endpoint(
                    addr(1, 1),
                    LinkSpec {
                        up: lossy,
                        ..LinkSpec::default()
                    },
                )
                .expect("a");
            let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
            let mut sa = net.socket(a);
            let mut sb = net.socket(b);
            let mut got = Vec::new();
            for i in 0..2000u16 {
                sa.send_to(&i.to_le_bytes(), addr(2, 1)).expect("send");
                net.advance_to_ms(u64::from(i));
                got.extend(drain(&mut sb));
            }
            pump(&net, 500);
            got.extend(drain(&mut sb));
            (got, net.stats(a))
        };
        let (a1, s1) = run(7);
        let (a2, s2) = run(7);
        assert_eq!(a1, a2, "same seed, same arrivals");
        assert_eq!(s1, s2);
        let (b1, _) = run(8);
        assert_ne!(a1, b1, "another seed, other arrivals");
        assert!(s1.drops_of(DropCause::Random) > 100, "{s1:?}");
        assert!(s1.drops_of(DropCause::Burst) > 20, "{s1:?}");
        assert!(s1.reordered > 40 && s1.duplicated > 10, "{s1:?}");
    }

    #[test]
    fn gilbert_elliott_losses_come_in_runs() {
        let mut d = DirState::new(
            Direction {
                burst: Some(GilbertElliott {
                    p_good_to_bad: 0.01,
                    p_bad_to_good: 0.25,
                    loss_good: 0.0,
                    loss_bad: 1.0,
                }),
                ..Direction::clean()
            },
            SoakRng::new(3),
        );
        let mut stats = EndpointStats::default();
        let mut runs = Vec::new();
        let mut run = 0u32;
        let mut lost = 0u32;
        let n = 200_000u32;
        for _ in 0..n {
            if d.pass(10, 0, &mut stats).is_err() {
                run += 1;
                lost += 1;
            } else if run > 0 {
                runs.push(run);
                run = 0;
            }
        }
        let mean_run = f64::from(runs.iter().sum::<u32>()) / runs.len() as f64;
        // Mean burst length 1 / p_bad_to_good = 4; mean loss pi_bad = 0.01 / 0.26.
        assert!((mean_run - 4.0).abs() < 0.3, "mean run {mean_run}");
        let frac = f64::from(lost) / f64::from(n);
        assert!((frac - 0.01 / 0.26).abs() < 0.005, "loss {frac}");
    }

    #[test]
    fn bandwidth_cap_serializes_and_drop_tail_queue_overflows() {
        let net = ImpairedNet::new(1);
        // 80 kbit/s = 10 kB/s; 972 + 28 = 1000 wire bytes is 100 ms each; queue holds 3.
        let a = net
            .add_endpoint(
                addr(1, 1),
                LinkSpec {
                    up: Direction {
                        bandwidth_bps: 80_000,
                        queue_bytes: 3_000,
                        ..Direction::clean()
                    },
                    ..LinkSpec::default()
                },
            )
            .expect("a");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let mut sa = net.socket(a);
        let mut sb = net.socket(b);
        for _ in 0..10 {
            sa.send_to(&[0u8; 972], addr(2, 1)).expect("send");
        }
        let stats = net.stats(a);
        assert_eq!(stats.drops_of(DropCause::Queue), 7, "{stats:?}");
        net.advance_to_ms(99);
        assert!(drain(&mut sb).is_empty());
        net.advance_to_ms(100);
        assert_eq!(drain(&mut sb).len(), 1);
        net.advance_to_ms(300);
        assert_eq!(drain(&mut sb).len(), 2);
        assert_eq!(net.stats(a).max_queue_delay_us, 200_000);
    }

    #[test]
    fn mtu_outage_and_rebinding() {
        let net = ImpairedNet::new(1);
        let a = net
            .add_endpoint(
                addr(1, 1000),
                LinkSpec {
                    up: Direction {
                        mtu: 1232,
                        ..Direction::clean()
                    },
                    outages: vec![(100, 200)],
                    rebinds_ms: vec![300],
                    ..LinkSpec::default()
                },
            )
            .expect("a");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let mut sa = net.socket(a);
        let mut sb = net.socket(b);
        sa.send_to(&[0u8; 1233], addr(2, 1)).expect("send");
        sa.send_to(&[0u8; 1232], addr(2, 1)).expect("send");
        net.advance_to_ms(1);
        assert_eq!(drain(&mut sb).len(), 1);
        assert_eq!(net.stats(a).drops_of(DropCause::Mtu), 1);
        net.advance_to_ms(150);
        sa.send_to(b"x", addr(2, 1)).expect("send");
        sb.send_to(b"y", addr(1, 1000)).expect("send");
        net.advance_to_ms(151);
        assert!(drain(&mut sb).is_empty() && drain(&mut sa).is_empty());
        assert_eq!(net.stats(a).drops_of(DropCause::Outage), 2);
        net.advance_to_ms(300);
        let moved = sa.local_addr().expect("addr");
        assert_ne!(moved, addr(1, 1000), "rebound");
        sb.send_to(b"old", addr(1, 1000)).expect("send");
        sa.send_to(b"new", addr(2, 1)).expect("send");
        net.advance_to_ms(301);
        assert!(drain(&mut sa).is_empty(), "the old mapping is gone");
        let mut buf = [0u8; 8];
        let (_, from) = sb.recv_from(&mut buf).expect("io").expect("datagram");
        assert_eq!(from, moved, "the new source is seen");
        assert_eq!(net.stats(b).drops_of(DropCause::NoRoute), 1);
    }

    #[test]
    fn jitter_keeps_order_and_reorder_breaks_it() {
        let net = ImpairedNet::new(5);
        let a = net
            .add_endpoint(
                addr(1, 1),
                LinkSpec {
                    up: Direction {
                        latency_ms: 10,
                        jitter_ms: 50,
                        ..Direction::clean()
                    },
                    ..LinkSpec::default()
                },
            )
            .expect("a");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let mut sa = net.socket(a);
        let mut sb = net.socket(b);
        for i in 0..200u16 {
            sa.send_to(&i.to_le_bytes(), addr(2, 1)).expect("send");
            net.advance_to_ms(u64::from(i));
        }
        pump(&net, 200);
        let got: Vec<u16> = drain(&mut sb)
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(got, (0..200).collect::<Vec<_>>(), "jitter alone keeps order");
    }

    #[test]
    fn clock_rate_runs_an_endpoint_fast_or_slow() {
        let fast = LinkSpec {
            clock_ppm: 5_000,
            ..LinkSpec::default()
        };
        let slow = LinkSpec {
            clock_ppm: -5_000,
            ..LinkSpec::default()
        };
        // +0.5 % over 10 virtual minutes is 3,000 extra local milliseconds.
        assert_eq!(fast.local_ms(600_000), 603_000);
        assert_eq!(slow.local_ms(600_000), 597_000);
        assert_eq!(LinkSpec::default().local_ms(600_000), 600_000);
    }

    #[test]
    fn phases_switch_numbers_at_their_time() {
        let net = ImpairedNet::new(1);
        let d = |latency_ms| Direction {
            latency_ms,
            ..Direction::clean()
        };
        let a = net
            .add_endpoint(
                addr(1, 1),
                LinkSpec {
                    up: d(20),
                    phases: vec![LinkPhase {
                        at_ms: 100,
                        up: d(125),
                        down: Direction::clean(),
                    }],
                    ..LinkSpec::default()
                },
            )
            .expect("a");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let (mut sa, mut sb) = (net.socket(a), net.socket(b));
        sa.send_to(b"early", addr(2, 1)).expect("send");
        net.advance_to_ms(20);
        assert_eq!(drain(&mut sb).len(), 1, "20 ms before the switch");
        net.advance_to_ms(100);
        sa.send_to(b"late", addr(2, 1)).expect("send");
        net.advance_to_ms(224);
        assert!(drain(&mut sb).is_empty());
        net.advance_to_ms(225);
        assert_eq!(drain(&mut sb).len(), 1, "125 ms after the switch");
        assert_eq!(net.spec(a).directions_at(150).0.latency_ms, 125);
    }

    #[test]
    fn cross_traffic_keeps_a_one_second_queue_full() {
        // asym-bufferbloat's downlink: 4 Mbit/s, 500,000-byte (1 s) queue, 4.4 Mbit/s cross traffic.
        let spec = Direction {
            bandwidth_bps: 4_000_000,
            queue_bytes: 500_000,
            cross_bps: 4_400_000,
            ..Direction::clean()
        };
        let fate = |seed: u64| {
            let mut d = DirState::new(spec.clone(), SoakRng::new(seed));
            d.down = true;
            let mut stats = EndpointStats::default();
            let mut admitted = 0u32;
            let mut dropped = 0u32;
            for i in 0..600u64 {
                // A 1,000-byte game datagram every 33 ms for 20 s.
                match d.pass(1_000, i * 33_000, &mut stats) {
                    Ok(_) => admitted += 1,
                    Err(DropCause::Queue) => dropped += 1,
                    Err(e) => panic!("{e:?}"),
                }
            }
            (admitted, dropped, stats)
        };
        let (admitted, dropped, stats) = fate(4);
        let h = &stats.down_queue_delay_ms;
        assert!(
            admitted > 100 && dropped > 100,
            "admitted {admitted}, dropped {dropped}"
        );
        assert!(
            (900..=1_000).contains(&h.quantile(0.05)) && h.max <= 1_000,
            "a game datagram that gets in waits 0.9-1 s: p5 {} ms, max {} ms",
            h.quantile(0.05),
            h.max
        );
        assert!(stats.cross_admitted > 0 && stats.cross_dropped > 0, "{stats:?}");
        assert_eq!(fate(4).2, stats, "seeded: same fate");
    }

    #[test]
    fn address_moves_and_forged_sources() {
        let net = ImpairedNet::new(1);
        let new_ip: IpAddr = [100, 64, 0, 1].into();
        let a = net
            .add_endpoint(
                addr(1, 1000),
                LinkSpec {
                    ip_moves: vec![(50, new_ip)],
                    ..LinkSpec::default()
                },
            )
            .expect("a");
        let b = net.add_endpoint(addr(2, 1), LinkSpec::default()).expect("b");
        let (mut sa, mut sb) = (net.socket(a), net.socket(b));
        net.advance_to_ms(50);
        let moved = sa.local_addr().expect("addr");
        assert_eq!(moved.ip(), new_ip);
        assert_eq!(net.addresses_held(a), vec![addr(1, 1000), moved]);
        sb.send_to(b"old", addr(1, 1000)).expect("send");
        net.advance_to_ms(51);
        assert!(drain(&mut sa).is_empty(), "the old address is gone");
        let forged = SocketAddr::from(([20, 0, 7, 9], 5555));
        net.send_forged(a, forged, b"spoof", addr(2, 1));
        net.advance_to_ms(52);
        let mut buf = [0u8; 16];
        let (_, from) = sb.recv_from(&mut buf).expect("io").expect("datagram");
        assert_eq!(from, forged, "the receiver sees the forged source");
        let st = net.stats(a);
        assert_eq!(st.up_offered_datagrams_per_s.iter().sum::<u64>(), 1);
    }
}
