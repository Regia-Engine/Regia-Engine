// SPDX-License-Identifier: MPL-2.0
//! A shared multi-user Steam mock for lobbies and peer-to-peer messages (spec 89 § 11): several
//! [`crate::MockSteam`] users on one [`MockSteamNet`] find each other's lobbies and exchange
//! datagrams without the SDK.
//!
//! Lobbies run on [`regia_net::LocalLobbyHub`] with each user's Steam id as its member id, so the
//! mock has exactly the local backend's semantics. A receiver's [`SteamAccept`] policy rejects
//! sessions from peers it does not allow, as Steam does when a session request is not accepted
//! (those messages are dropped and counted).
//!
//! **Links (spec 120 § 10.8, ADR-8351).** By default every message is delivered at once, in send
//! order, and nothing is lost. [`MockSteamNet::set_link`] (one user) and
//! [`MockSteamNet::set_default_link`] (every user) give users an impaired access link, a
//! [`MockLink`]: the soak harness's direction model and named profiles (`regia_net_soak`,
//! ADR-7899) on Steam peer-to-peer messages, so a Steam session runs on good-wifi, bad-wifi or
//! flappy without the SDK. A message crosses the sender's *up* direction when it is sent and the
//! receiver's *down* direction when it reaches the receiver's side; each direction applies, in
//! order, the outage windows, random loss, Gilbert-Elliott burst loss, a bandwidth bottleneck
//! behind a drop-tail queue, the one-way latency plus order-keeping jitter, reordering and
//! duplication. Time is virtual ([`MockSteamNet::advance_ms`]); every random draw comes from a
//! SplitMix64 stream per user and direction seeded from [`MockSteamNet::set_seed`], so the same
//! seed and the same traffic replay the same drops and arrival order. Nothing reads a wall clock.

use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_net::lobby::{LobbyBackend, LocalLobbyHub};

use crate::backend::{SteamError, SteamId};
use crate::sockets::{P2pMessages, SteamAccept};

/// Datagrams waiting for one receiver on one channel: `(sender, bytes)` in arrival order.
type Inbox = VecDeque<(SteamId, Vec<u8>)>;

/// The link profiles [`MockLink::profile`] knows, with the soak harness's numbers
/// (`book/agents/net-soak.md`).
pub const MOCK_LINK_PROFILES: &[&str] = &[
    "clean",
    "good-wifi",
    "bad-wifi",
    "mobile-4g",
    "transatlantic",
    "congested",
    "flappy",
];

/// Bytes of IPv4 + UDP header a bottleneck counts per message on top of its payload (the harness
/// counts the same; Steam's relay framing is not modeled).
pub const MOCK_HEADER_BYTES: usize = 28;

/// Two-state Gilbert-Elliott burst loss, stepped once per message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MockBurst {
    /// Probability of going from the good to the bad state.
    pub p_good_to_bad: f64,
    /// Probability of going from the bad to the good state (mean burst `1 / p` messages).
    pub p_bad_to_good: f64,
    /// Loss probability in the good state.
    pub loss_good: f64,
    /// Loss probability in the bad state.
    pub loss_bad: f64,
}

/// One direction of a [`MockLink`]; each number is one way.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MockDirection {
    /// One-way latency, milliseconds.
    pub latency_ms: u64,
    /// Extra latency, uniform in `0..=jitter_ms`; it keeps order (a message never arrives before
    /// one sent ahead of it on the same direction).
    pub jitter_ms: u64,
    /// Independent random loss probability.
    pub loss: f64,
    /// Bursty loss on top of `loss`.
    pub burst: Option<MockBurst>,
    /// Probability that a message gets an extra hold that later messages overtake.
    pub reorder: f64,
    /// Upper bound of the reorder hold, milliseconds (at least 1).
    pub reorder_ms: u64,
    /// Probability that a message is delivered twice.
    pub duplicate: f64,
    /// Bottleneck rate, bits per second; `0` is unlimited.
    pub bandwidth_bps: u64,
    /// Drop-tail queue in front of the bottleneck, bytes (ignored when unlimited).
    pub queue_bytes: usize,
    /// Path MTU, bytes on the wire (IPv4 + UDP header included); `0` is none. Steam fragments a
    /// message larger than the path takes, so a message over it travels as
    /// `ceil(len / (mtu_bytes - MOCK_HEADER_BYTES))` fragments: each one passes the random and
    /// burst loss (the burst chain steps once per fragment), the message is lost when any fragment
    /// is, and the bottleneck carries every fragment's header (KI-NT-051, ADR-10069).
    pub mtu_bytes: usize,
}

/// A user's access link on a [`MockSteamNet`]: an up direction (what the user sends), a down
/// direction (what it receives) and outage windows in which both drop everything.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MockLink {
    /// What the user sends.
    pub up: MockDirection,
    /// What the user receives.
    pub down: MockDirection,
    /// Repeating outages `(first start ms, period ms, length ms)` in virtual time.
    pub flaps: Option<(u64, u64, u64)>,
    /// One-off outages `(start ms, length ms)` in virtual time.
    pub outages: Vec<(u64, u64)>,
    /// NAT rebindings of the user's access link, virtual milliseconds (KI-NT-051). Steam
    /// addresses peers by Steam id, so a rebinding moves nothing the game sees; Steam's relay
    /// network finds the new address and routes again. For [`Self::rebind_gap_ms`] after each
    /// rebinding both directions drop everything (counted as `rebind_drops`).
    pub rebinds_ms: Vec<u64>,
    /// How long Steam takes to route again after a rebinding, milliseconds;
    /// [`MOCK_REBIND_GAP_MS`] in [`Self::profile_for_client`]. A model value, not a measurement
    /// of Steam's relay network.
    pub rebind_gap_ms: u64,
}

/// The reroute gap after a NAT rebinding in [`MockLink::profile_for_client`], milliseconds (a model
/// value: one relay ping round and a route update, ADR-10069).
pub const MOCK_REBIND_GAP_MS: u64 = 500;
/// The path MTU of the `mobile-4g` profile, bytes (the soak harness's number).
pub const MOCK_MOBILE_MTU_BYTES: usize = 1_232;
/// How far `flappy`'s long outage on every fourth client runs past the engine's removal timeout,
/// milliseconds (the harness's `FLAP_LONG_PAST_TIMEOUT_MS`).
pub const MOCK_FLAP_LONG_PAST_TIMEOUT_MS: u64 = 2_000;

/// Length of each `flappy` outage, milliseconds (the harness's `FLAP_OUTAGE_MS`).
pub const MOCK_FLAP_OUTAGE_MS: u64 = 3_000;
/// Time between the starts of two `flappy` outages, milliseconds (the harness's `FLAP_PERIOD_MS`).
pub const MOCK_FLAP_PERIOD_MS: u64 = 20_000;
/// Start of the first `flappy` outage, milliseconds.
pub const MOCK_FLAP_FIRST_MS: u64 = 15_000;

impl MockLink {
    /// No impairment: delivery at once, in order.
    pub fn clean() -> Self {
        Self::default()
    }

    /// Whether the link changes nothing (no latency, loss, reorder, duplicate, cap or outage).
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }

    /// Both directions set to `d`.
    pub fn symmetric(d: MockDirection) -> Self {
        Self {
            up: d.clone(),
            down: d,
            ..Self::default()
        }
    }

    /// The link of a named profile ([`MOCK_LINK_PROFILES`]), with the soak harness's numbers,
    /// `mobile-4g`'s path MTU ([`MOCK_MOBILE_MTU_BYTES`]) included. The harness's per-client extras
    /// (the long `flappy` outage, the `mobile-4g` NAT rebinding, the per-client shift of the flaps)
    /// come with [`Self::profile_for_client`].
    pub fn profile(name: &str) -> Option<Self> {
        let wifi_good = || {
            let base = MockDirection {
                latency_ms: 4,
                jitter_ms: 4,
                loss: 0.002,
                reorder: 0.001,
                reorder_ms: 10,
                duplicate: 0.0005,
                queue_bytes: 64 * 1024,
                ..MockDirection::default()
            };
            Self {
                up: MockDirection {
                    bandwidth_bps: 20_000_000,
                    ..base.clone()
                },
                down: MockDirection {
                    bandwidth_bps: 50_000_000,
                    ..base
                },
                ..Self::default()
            }
        };
        let split = |d: MockDirection, up_bps: u64, down_bps: u64| Self {
            up: MockDirection {
                bandwidth_bps: up_bps,
                ..d.clone()
            },
            down: MockDirection {
                bandwidth_bps: down_bps,
                ..d
            },
            ..Self::default()
        };
        Some(match name {
            "clean" => Self::clean(),
            "good-wifi" => wifi_good(),
            "bad-wifi" => split(
                MockDirection {
                    latency_ms: 8,
                    jitter_ms: 30,
                    loss: 0.01,
                    burst: Some(MockBurst {
                        p_good_to_bad: 0.01,
                        p_bad_to_good: 0.3,
                        loss_good: 0.0,
                        loss_bad: 0.6,
                    }),
                    reorder: 0.01,
                    reorder_ms: 30,
                    duplicate: 0.005,
                    queue_bytes: 64 * 1024,
                    ..MockDirection::default()
                },
                2_000_000,
                6_000_000,
            ),
            "mobile-4g" => split(
                MockDirection {
                    latency_ms: 35,
                    jitter_ms: 25,
                    loss: 0.005,
                    burst: Some(MockBurst {
                        p_good_to_bad: 0.005,
                        p_bad_to_good: 0.2,
                        loss_good: 0.0,
                        loss_bad: 0.5,
                    }),
                    reorder: 0.005,
                    reorder_ms: 40,
                    duplicate: 0.001,
                    queue_bytes: 128 * 1024,
                    mtu_bytes: MOCK_MOBILE_MTU_BYTES,
                    ..MockDirection::default()
                },
                2_000_000,
                8_000_000,
            ),
            "transatlantic" => split(
                MockDirection {
                    latency_ms: 45,
                    jitter_ms: 6,
                    loss: 0.003,
                    reorder: 0.002,
                    reorder_ms: 15,
                    queue_bytes: 64 * 1024,
                    ..MockDirection::default()
                },
                20_000_000,
                50_000_000,
            ),
            "congested" => split(
                MockDirection {
                    latency_ms: 15,
                    jitter_ms: 10,
                    queue_bytes: 16 * 1024,
                    ..MockDirection::default()
                },
                1_000_000,
                256_000,
            ),
            "flappy" => Self {
                flaps: Some((MOCK_FLAP_FIRST_MS, MOCK_FLAP_PERIOD_MS, MOCK_FLAP_OUTAGE_MS)),
                ..wifi_good()
            },
            _ => return None,
        })
    }

    /// The link of client `index` of a run of `duration_ms` on profile `name`, as the soak
    /// harness builds it (KI-NT-051, ADR-10069): the [`Self::profile`] link, the `flappy` flaps
    /// shifted by `(index * 137) % 1000` ms, on every fourth client (`index % 4 == 3`) of `flappy`
    /// one outage of `regia_net::TimeoutPolicy::default().timeout_ms` plus
    /// [`MOCK_FLAP_LONG_PAST_TIMEOUT_MS`] from `duration_ms / 2` plus the shift, and on `mobile-4g` one NAT rebinding at `0.4 * duration_ms + index *
    /// 97` ms with a [`MOCK_REBIND_GAP_MS`] reroute gap.
    pub fn profile_for_client(name: &str, index: usize, duration_ms: u64) -> Option<Self> {
        let mut link = Self::profile(name)?;
        let shift = (index as u64 * 137) % 1_000;
        if name == "flappy" {
            link.flaps = Some((
                MOCK_FLAP_FIRST_MS + shift,
                MOCK_FLAP_PERIOD_MS,
                MOCK_FLAP_OUTAGE_MS,
            ));
            if index % 4 == 3 {
                let len = regia_net::TimeoutPolicy::default().timeout_ms + MOCK_FLAP_LONG_PAST_TIMEOUT_MS;
                link.outages.push((duration_ms / 2 + shift, len));
            }
        }
        if name == "mobile-4g" {
            link.rebinds_ms
                .push((duration_ms as f64 * 0.4) as u64 + index as u64 * 97);
            link.rebind_gap_ms = MOCK_REBIND_GAP_MS;
        }
        Some(link)
    }

    /// Whether virtual time `at_us` falls inside a reroute gap after a NAT rebinding.
    fn rerouting_at(&self, at_us: u64) -> bool {
        let ms = at_us / 1_000;
        self.rebinds_ms
            .iter()
            .any(|&t| ms >= t && ms - t < self.rebind_gap_ms)
    }

    /// Whether virtual time `at_us` falls inside an outage window.
    fn down_at(&self, at_us: u64) -> bool {
        let ms = at_us / 1_000;
        if let Some((first, period, len)) = self.flaps
            && ms >= first
        {
            // A period of 0 is one outage at `first`.
            let into = if period == 0 {
                ms - first
            } else {
                (ms - first) % period
            };
            if into < len {
                return true;
            }
        }
        self.outages
            .iter()
            .any(|&(start, len)| ms >= start && ms - start < len)
    }
}

/// What the links did since the network was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MockNetStats {
    /// Messages sent through an impaired link (either end).
    pub impaired_sent: u64,
    /// Messages dropped inside an outage window.
    pub outage_drops: u64,
    /// Messages dropped by random loss.
    pub random_drops: u64,
    /// Messages dropped by burst loss.
    pub burst_drops: u64,
    /// Messages dropped by a full bottleneck queue.
    pub queue_drops: u64,
    /// Messages dropped in the reroute gap after a NAT rebinding (KI-NT-051).
    pub rebind_drops: u64,
    /// Messages larger than the path MTU, sent as fragments (KI-NT-051).
    pub fragmented: u64,
    /// Messages held for reordering.
    pub reordered: u64,
    /// Extra copies delivered.
    pub duplicated: u64,
    /// Messages still in flight.
    pub in_flight: u64,
}

impl MockNetStats {
    /// Every drop, whatever the cause.
    pub fn drops(&self) -> u64 {
        self.outage_drops + self.random_drops + self.burst_drops + self.queue_drops + self.rebind_drops
    }
}

/// SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number generators", OOPSLA
/// 2014; the constants of Vigna's public-domain reference).
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// True with probability `p`.
    fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    /// Uniform in `0..=n`.
    fn up_to(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % (n + 1) }
    }
}

/// Why a direction dropped a message.
#[derive(Debug, Clone, Copy)]
enum DropCause {
    Outage,
    Random,
    Burst,
    Queue,
    Rebind,
}

/// One direction's running state.
#[derive(Debug)]
struct DirState {
    rng: Rng,
    bad: bool,
    busy_until_us: u64,
    last_arrival_us: u64,
}

impl DirState {
    fn new(seed: u64, id: SteamId, down: bool) -> Self {
        // One stream per user and direction: mix the id and the direction into the run seed.
        let mut mix = Rng(seed ^ id.0.rotate_left(17) ^ if down { 0xD0_57 } else { 0x0F_F5 });
        Self {
            rng: Rng(mix.next()),
            bad: false,
            busy_until_us: 0,
            last_arrival_us: 0,
        }
    }

    /// Runs one message of `len` bytes through `spec` at `now_us`: its arrival times at the far
    /// end (two when duplicated), or why it was dropped.
    fn pass(
        &mut self,
        spec: &MockDirection,
        link: &MockLink,
        len: usize,
        now_us: u64,
        stats: &mut MockNetStats,
    ) -> Result<Vec<u64>, DropCause> {
        if link.down_at(now_us) {
            return Err(DropCause::Outage);
        }
        if link.rerouting_at(now_us) {
            return Err(DropCause::Rebind);
        }
        // A message over the path MTU travels as fragments (Steam fragments, it does not drop).
        let fragments = if spec.mtu_bytes > MOCK_HEADER_BYTES && len + MOCK_HEADER_BYTES > spec.mtu_bytes {
            stats.fragmented += 1;
            len.div_ceil(spec.mtu_bytes - MOCK_HEADER_BYTES).max(1)
        } else {
            1
        };
        // The chain steps once per fragment, dropped or not, so its states depend only on the
        // fragment count; a message that fits draws exactly as before.
        for _ in 0..fragments {
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
        }
        let mut depart_us = now_us;
        if spec.bandwidth_bps > 0 {
            let wire = len + MOCK_HEADER_BYTES * fragments;
            let backlog_us = self.busy_until_us.saturating_sub(now_us);
            let backlog_bytes = backlog_us.saturating_mul(spec.bandwidth_bps) / 8_000_000;
            if backlog_bytes as usize + wire > spec.queue_bytes.max(wire) {
                return Err(DropCause::Queue);
            }
            let tx_us = (wire as u64 * 8 * 1_000_000).div_ceil(spec.bandwidth_bps);
            self.busy_until_us = self.busy_until_us.max(now_us) + tx_us;
            depart_us = self.busy_until_us;
        }
        let jitter_us = self.rng.up_to(spec.jitter_ms) * 1_000;
        let mut arrive_us = depart_us + spec.latency_ms * 1_000 + jitter_us;
        if self.rng.chance(spec.reorder) {
            stats.reordered += 1;
            arrive_us += (1 + self.rng.up_to(spec.reorder_ms.saturating_sub(1))) * 1_000;
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

/// A user's link and its two directions' state.
#[derive(Debug)]
struct LinkState {
    link: MockLink,
    up: DirState,
    down: DirState,
}

/// A message in flight between two impaired edges.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InFlight {
    at_us: u64,
    order: u64,
    /// `false`: crossed the sender's up direction, enters the receiver's down direction on
    /// arrival; `true`: crossed both, goes into the inbox on arrival.
    deliver: bool,
    from: SteamId,
    to: SteamId,
    channel: u16,
    bytes: Vec<u8>,
}

impl Ord for InFlight {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Min-heap on (time, send order).
        (other.at_us, other.order).cmp(&(self.at_us, self.order))
    }
}

impl PartialOrd for InFlight {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Default)]
struct NetState {
    /// Per `(receiver, channel)`.
    queues: BTreeMap<(SteamId, u16), Inbox>,
    accept: BTreeMap<SteamId, SteamAccept>,
    online: BTreeMap<SteamId, bool>,
    delivered: u64,
    rejected: u64,
    /// Virtual time, microseconds.
    now_us: u64,
    seed: u64,
    /// The link users get unless [`MockSteamNet::set_link`] gave them one.
    default_link: MockLink,
    links: BTreeMap<SteamId, LinkState>,
    in_flight: BinaryHeap<InFlight>,
    order: u64,
    stats: MockNetStats,
}

impl NetState {
    fn link_mut(&mut self, id: SteamId) -> &mut LinkState {
        let (seed, default) = (self.seed, &self.default_link);
        self.links.entry(id).or_insert_with(|| LinkState {
            link: default.clone(),
            up: DirState::new(seed, id, false),
            down: DirState::new(seed, id, true),
        })
    }

    fn impaired(&self, id: SteamId) -> bool {
        match self.links.get(&id) {
            Some(l) => !l.link.is_clean(),
            None => !self.default_link.is_clean(),
        }
    }

    fn count_drop(&mut self, why: DropCause) {
        match why {
            DropCause::Outage => self.stats.outage_drops += 1,
            DropCause::Random => self.stats.random_drops += 1,
            DropCause::Burst => self.stats.burst_drops += 1,
            DropCause::Queue => self.stats.queue_drops += 1,
            DropCause::Rebind => self.stats.rebind_drops += 1,
        }
    }

    /// One direction of `id` (`down` or up) on a message of `len` bytes entering it at `now`
    /// (microseconds).
    fn cross(&mut self, id: SteamId, down: bool, len: usize, now: u64) -> Result<Vec<u64>, DropCause> {
        let mut stats = self.stats;
        let state = self.link_mut(id);
        let (spec, dir) = if down {
            (&state.link.down, &mut state.down)
        } else {
            (&state.link.up, &mut state.up)
        };
        let r = dir.pass(spec, &state.link, len, now, &mut stats);
        self.stats = stats;
        r
    }

    /// Puts `msg` in flight once per arrival time in `at` (two when the direction duplicated it);
    /// `msg`'s own time and order are replaced.
    fn launch(&mut self, at: Vec<u64>, msg: InFlight) {
        let mut msg = Some(msg);
        let n = at.len();
        for (i, at_us) in at.into_iter().enumerate() {
            // The last copy takes the original.
            let Some(mut copy) = (if i + 1 == n { msg.take() } else { msg.clone() }) else {
                break;
            };
            self.order += 1;
            copy.at_us = at_us;
            copy.order = self.order;
            self.in_flight.push(copy);
        }
    }

    /// Moves every message due by now one stage on: into the receiver's down direction, or into
    /// its inbox.
    fn pump(&mut self) {
        while self.in_flight.peek().is_some_and(|m| m.at_us <= self.now_us) {
            let Some(m) = self.in_flight.pop() else { break };
            if m.deliver {
                self.queues
                    .entry((m.to, m.channel))
                    .or_default()
                    .push_back((m.from, m.bytes));
            } else {
                match self.cross(m.to, true, m.bytes.len(), m.at_us) {
                    Ok(at) => self.launch(at, InFlight { deliver: true, ..m }),
                    Err(why) => self.count_drop(why),
                }
            }
        }
    }
}

/// The shared mock network. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct MockSteamNet {
    hub: LocalLobbyHub,
    state: Arc<Mutex<NetState>>,
}

impl MockSteamNet {
    /// An empty network.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, NetState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lobby directory.
    pub fn hub(&self) -> &LocalLobbyHub {
        &self.hub
    }

    /// Registers user `id` and returns its lobby backend and messages endpoint.
    pub(crate) fn attach(&self, id: SteamId, name: &str) -> (Arc<dyn LobbyBackend>, Arc<dyn P2pMessages>) {
        self.lock().online.insert(id, true);
        let lobbies: Arc<dyn LobbyBackend> = Arc::new(self.hub.member(id.0, name));
        let messages: Arc<dyn P2pMessages> = Arc::new(MockP2p {
            net: self.clone(),
            me: id,
        });
        (lobbies, messages)
    }

    /// Takes a user off the network: sends to it fail, as for a peer that quit Steam.
    pub fn set_online(&self, id: SteamId, online: bool) {
        self.lock().online.insert(id, online);
    }

    /// `(delivered, rejected)` message counters.
    pub fn counters(&self) -> (u64, u64) {
        let s = self.lock();
        (s.delivered, s.rejected)
    }

    /// Seeds the links' random streams (default 0). Set it before the first impaired message: a
    /// user's streams are made from the seed when its link first carries one.
    pub fn set_seed(&self, seed: u64) {
        let mut s = self.lock();
        s.seed = seed;
        for (id, state) in s.links.iter_mut() {
            state.up = DirState::new(seed, *id, false);
            state.down = DirState::new(seed, *id, true);
        }
    }

    /// Gives user `id` the access link `link` (see the module docs). The user's random streams
    /// restart from the seed.
    pub fn set_link(&self, id: SteamId, link: MockLink) {
        let mut s = self.lock();
        let seed = s.seed;
        s.links.insert(
            id,
            LinkState {
                link,
                up: DirState::new(seed, id, false),
                down: DirState::new(seed, id, true),
            },
        );
    }

    /// The link every user without a [`Self::set_link`] link gets (default [`MockLink::clean`]).
    pub fn set_default_link(&self, link: MockLink) {
        self.lock().default_link = link;
    }

    /// The link user `id` sends and receives on.
    pub fn link(&self, id: SteamId) -> MockLink {
        let s = self.lock();
        s.links
            .get(&id)
            .map(|l| l.link.clone())
            .unwrap_or_else(|| s.default_link.clone())
    }

    /// Moves virtual time on by `ms` milliseconds and delivers what arrived by then. Only impaired
    /// links hold messages; with clean links time does not matter.
    pub fn advance_ms(&self, ms: u64) {
        let mut s = self.lock();
        s.now_us = s.now_us.saturating_add(ms.saturating_mul(1_000));
        s.pump();
    }

    /// Virtual time, milliseconds since the network was created.
    pub fn now_ms(&self) -> u64 {
        self.lock().now_us / 1_000
    }

    /// What the links did: drops by cause, reorders, duplicates, messages in flight.
    pub fn link_stats(&self) -> MockNetStats {
        let s = self.lock();
        MockNetStats {
            in_flight: s.in_flight.len() as u64,
            ..s.stats
        }
    }
}

/// One user's messages endpoint on a [`MockSteamNet`].
#[derive(Debug)]
struct MockP2p {
    net: MockSteamNet,
    me: SteamId,
}

impl P2pMessages for MockP2p {
    fn local_id(&self) -> Option<SteamId> {
        Some(self.me)
    }

    fn send(&self, to: SteamId, channel: u16, bytes: &[u8]) -> Result<(), SteamError> {
        let mut s = self.net.lock();
        if !s.online.get(&to).copied().unwrap_or(false) {
            return Err(SteamError::Rejected("no route to the peer".into()));
        }
        if !s.impaired(self.me) && !s.impaired(to) {
            s.queues
                .entry((to, channel))
                .or_default()
                .push_back((self.me, bytes.to_vec()));
            return Ok(());
        }
        // An impaired link loses quietly, like a datagram into the network: the send succeeds.
        s.stats.impaired_sent += 1;
        let now = s.now_us;
        match s.cross(self.me, false, bytes.len(), now) {
            Ok(at) => {
                let msg = InFlight {
                    at_us: now,
                    order: 0,
                    deliver: false,
                    from: self.me,
                    to,
                    channel,
                    bytes: bytes.to_vec(),
                };
                s.launch(at, msg);
                s.pump();
            }
            Err(why) => s.count_drop(why),
        }
        Ok(())
    }

    fn receive(&self, channel: u16, max: usize) -> Vec<(SteamId, Vec<u8>)> {
        let mut scratch = Vec::new();
        self.receive_into(channel, max, &mut scratch)
            .into_iter()
            .scan(0usize, |start, (id, len)| {
                let end = *start + len;
                let bytes = scratch[*start..end].to_vec();
                *start = end;
                Some((id, bytes))
            })
            .collect()
    }

    fn receive_into(&self, channel: u16, max: usize, scratch: &mut Vec<u8>) -> Vec<(SteamId, usize)> {
        let mut s = self.net.lock();
        s.pump();
        let accept = s.accept.get(&self.me).cloned().unwrap_or_default();
        let mut out = Vec::new();
        let mut rejected = 0;
        if let Some(q) = s.queues.get_mut(&(self.me, channel)) {
            while out.len() < max {
                let Some((from, bytes)) = q.pop_front() else {
                    break;
                };
                if accept.allows(from) {
                    scratch.extend_from_slice(&bytes);
                    out.push((from, bytes.len()));
                } else {
                    rejected += 1;
                }
            }
        }
        s.delivered += out.len() as u64;
        s.rejected += rejected;
        out
    }

    fn set_accept(&self, accept: SteamAccept) {
        self.net.lock().accept.insert(self.me, accept);
    }
}
