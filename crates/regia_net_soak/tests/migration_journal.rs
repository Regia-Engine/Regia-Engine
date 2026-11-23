// SPDX-License-Identifier: MPL-2.0
//! Warm successors on impaired links (spec 120 § 10.5, lane N302, ADR-8853): a listen-style
//! host journals a deterministic sample world to its two best connected clients through the real
//! transport (`NetServer` / `NetClient`, the bulk stream, the rate controller) on the harness's
//! seeded `good-wifi` and `bad-wifi` links, in virtual time.
//!
//! The acceptance rows: a successor's shadow world equals the host's world digest at every
//! full-state point on bad-wifi; the journal lag is at most 2 ticks p99 on good-wifi; the
//! journal's bandwidth stays within its share of the rate controller's allowance.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_net::connection::StreamId;
use regia_net::migration::{
    DIGEST_BYTES, JournalConfig, JournalHost, JournalSuccessor, ShadowWorld, WorldDigest, is_journal_payload,
};
use regia_net::{
    Channel, ClientConfig, ClientEvent, NetClient, NetGame, NetServer, PeerId, ServerConfig, ServerEvent,
    WireReader, WireWriter,
};
use regia_net_soak::impair::{ImpairedNet, ImpairedSocket, LinkSpec, SoakRng};
use regia_net_soak::profiles::Profile;

/// One server tick at 60 Hz, microseconds.
const TICK_US: u64 = 16_667;
const SERVER: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
/// First byte of the sample game's own unreliable state message (not a journal payload).
const TAG_STATE: u8 = 0x01;
/// Most bytes of the state message the host sends every client every tick, next to the journal.
/// Like a game that sizes its snapshots by the rate controller, the host spends half of each
/// client's send allowance on it.
const STATE_BYTES: usize = 300;

struct JournalGame;

impl NetGame for JournalGame {
    type Message = Vec<u8>;
    type Input = u8;
    type JoinData = u32;
    const GAME_ID: &'static str = "regia.test.migration_journal";
    const PROTOCOL_VERSION: u32 = 1;
}

/// The sample world: players that move by the direction they hold, cells the host's own events
/// stir, and a terrain blob that gives the full state its size. Everything the host does to it
/// is in the journal: the log's inputs are the directions that changed in the tick (a held
/// direction is not journaled again), its events are joins, leaves and the host's dice.
#[derive(Debug, Clone, Default)]
struct World {
    tick: u64,
    /// Per player: position, direction changes, the direction held.
    players: BTreeMap<u32, (i64, u32, u8)>,
    cells: Vec<u64>,
    terrain: Vec<u8>,
    /// The digest after each tick applied from the log (a shadow's own record).
    applied: BTreeMap<u64, WorldDigest>,
    /// The digest after each loaded full state.
    loaded: BTreeMap<u64, WorldDigest>,
}

impl World {
    fn new(terrain_bytes: usize) -> Self {
        Self {
            cells: vec![0; 64],
            terrain: (0..terrain_bytes).map(|i| (i * 131 % 251) as u8).collect(),
            ..Self::default()
        }
    }

    /// One tick: `inputs` is `(peer u32, new direction u8)*`, `events` is
    /// `(kind u8, a u32, b u32)*`.
    fn step(&mut self, tick: u64, inputs: &[u8], events: &[u8]) -> Result<(), String> {
        if !inputs.len().is_multiple_of(5) || !events.len().is_multiple_of(9) {
            return Err("journal record of the wrong shape".into());
        }
        self.tick = tick;
        for e in events.as_chunks::<9>().0 {
            let a = u32::from_le_bytes([e[1], e[2], e[3], e[4]]);
            let b = u32::from_le_bytes([e[5], e[6], e[7], e[8]]);
            match e[0] {
                // A player joined.
                0 => {
                    self.players.insert(a, (0, 0, 0));
                }
                // A player left.
                1 => {
                    self.players.remove(&a);
                }
                // The host's own dice: a cell changes and the terrain takes a dent.
                _ => {
                    let n = self.cells.len();
                    let c = &mut self.cells[a as usize % n];
                    *c = c.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(u64::from(b));
                    if !self.terrain.is_empty() {
                        let at = b as usize % self.terrain.len();
                        self.terrain[at] = self.terrain[at].wrapping_add(a as u8);
                    }
                }
            }
        }
        for i in inputs.as_chunks::<5>().0 {
            let peer = u32::from_le_bytes([i[0], i[1], i[2], i[3]]);
            if let Some((_, changes, held)) = self.players.get_mut(&peer) {
                *held = i[4];
                *changes += 1;
            }
        }
        for (x, _, held) in self.players.values_mut() {
            *x += i64::from(*held) - 2;
        }
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.u64(self.tick);
        w.u32(self.players.len() as u32);
        for (peer, (x, changes, held)) in &self.players {
            w.u32(*peer);
            w.u64(*x as u64);
            w.u32(*changes);
            w.u8(*held);
        }
        w.u32(self.cells.len() as u32);
        for c in &self.cells {
            w.u64(*c);
        }
        w.u32(self.terrain.len() as u32);
        w.raw(&self.terrain);
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, regia_net::WireError> {
        let mut r = WireReader::new(bytes);
        let mut world = World {
            tick: r.u64()?,
            ..World::default()
        };
        for _ in 0..r.u32()? {
            let peer = r.u32()?;
            let x = r.u64()? as i64;
            let changes = r.u32()?;
            world.players.insert(peer, (x, changes, r.u8()?));
        }
        for _ in 0..r.u32()? {
            world.cells.push(r.u64()?);
        }
        let len = r.u32()? as usize;
        world.terrain = r.take(len)?.to_vec();
        r.finish()?;
        Ok(world)
    }

    /// Four FNV-1a lanes over the canonical encoding: enough to tell two worlds apart in a test.
    fn world_digest(&self) -> WorldDigest {
        let bytes = self.encode();
        let mut out = [0u8; DIGEST_BYTES];
        for (lane, chunk) in out.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            let mut h = 0xcbf2_9ce4_8422_2325_u64 ^ (lane as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for b in &bytes {
                h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
            chunk.copy_from_slice(&h.to_le_bytes());
        }
        out
    }
}

impl ShadowWorld for World {
    fn load_full(&mut self, tick: u64, bytes: &[u8]) -> Result<(), String> {
        let world = World::decode(bytes).map_err(|e| e.to_string())?;
        if world.tick != tick {
            return Err(format!("full state of tick {} sent as {tick}", world.tick));
        }
        self.tick = world.tick;
        self.players = world.players;
        self.cells = world.cells;
        self.terrain = world.terrain;
        self.loaded.insert(tick, self.world_digest());
        Ok(())
    }

    fn apply(&mut self, tick: u64, inputs: &[u8], events: &[u8]) -> Result<(), String> {
        if tick != self.tick + 1 {
            return Err(format!("tick {tick} on a shadow at {}", self.tick));
        }
        self.step(tick, inputs, events)?;
        self.applied.insert(tick, self.world_digest());
        Ok(())
    }

    fn digest(&self) -> WorldDigest {
        self.world_digest()
    }
}

struct Client {
    index: u32,
    net: NetClient<JournalGame, ImpairedSocket>,
    shadow: JournalSuccessor<World>,
    peer: Option<PeerId>,
    next_frame_us: u64,
    refusals: Vec<&'static str>,
    left: bool,
}

/// A lag histogram of one successor, ticks.
#[derive(Debug, Default, Clone)]
struct Lag {
    counts: BTreeMap<u64, u64>,
    samples: u64,
}

impl Lag {
    fn add(&mut self, lag: u64) {
        *self.counts.entry(lag).or_default() += 1;
        self.samples += 1;
    }

    fn percentile(&self, p: u64) -> u64 {
        let need = (self.samples * p).div_ceil(100);
        let mut seen = 0;
        for (lag, n) in &self.counts {
            seen += n;
            if seen >= need {
                return *lag;
            }
        }
        0
    }
}

struct Options {
    profile: &'static str,
    seed: u64,
    clients: u32,
    seconds: u64,
    terrain_bytes: usize,
    config: JournalConfig,
    /// The first successor disconnects at this time, milliseconds.
    first_successor_leaves_ms: Option<u64>,
    /// The host journals (off: the same game with no journal, as a control).
    journal: bool,
}

impl Options {
    fn new(profile: &'static str, seed: u64) -> Self {
        Self {
            profile,
            seed,
            clients: 8,
            seconds: 60,
            terrain_bytes: 48 * 1024,
            config: JournalConfig::default(),
            first_successor_leaves_ms: None,
            journal: true,
        }
    }
}

struct Outcome {
    host: JournalHost,
    world: World,
    clients: Vec<Client>,
    /// The host's digest at every full-state tick.
    full_digests: BTreeMap<u64, WorldDigest>,
    /// The host's digest after every tick.
    host_digests: BTreeMap<u64, WorldDigest>,
    /// Ground-truth lag per successor (by client index): the host's newest journaled tick minus
    /// the shadow's applied tick, read every tick just before the host journals the next one,
    /// while the shadow is warm.
    lag: BTreeMap<u32, Lag>,
    /// Journal bytes the pump handed to the transport.
    pumped: u64,
    /// Per successor peer: (journal bytes the host counted, the budget the test integrated from
    /// the transport's allowance, the highest budget rate so far), at the end of each whole
    /// second.
    budget_checks: Vec<(u64, u32, u64, f64, f64)>,
    /// The transport's allowance for each successor, bytes per second: (lowest, highest, sum,
    /// samples).
    allowance: BTreeMap<u32, (f64, f64, f64, u64)>,
    /// Acknowledgments the host refused (an older appointment's, still in flight).
    host_refusals: Vec<&'static str>,
    /// When the first successor left, and who was the successor set just before and at the end.
    successors_before_leave: Vec<PeerId>,
    events: Vec<(u64, regia_net::migration::JournalEvent)>,
    /// Every way a client's connection ended, as the server and the client saw it:
    /// (time, who, what).
    ends: Vec<(u64, String, String)>,
    ticks: u64,
    /// Ticks from the first warm successor on, and how many of them had at least one warm
    /// successor whose shadow was promotable.
    ticks_since_warm: u64,
    ticks_covered: u64,
}

fn run(o: &Options) -> Outcome {
    let net = ImpairedNet::new(o.seed);
    let profile = Profile::named(o.profile).expect("known profile");
    let server_ep = net
        .add_endpoint(SocketAddr::from(SERVER), LinkSpec::default())
        .expect("server endpoint");
    let mut server = NetServer::<JournalGame, _>::new(
        net.socket(server_ep),
        ServerConfig {
            max_peers: o.clients as usize + 4,
            tick_rate_hz: 60,
            ..ServerConfig::default()
        },
    );
    let mut rng = SoakRng::derive(o.seed, 91);
    let mut clients: Vec<Client> = (0..o.clients)
        .map(|i| {
            let addr = SocketAddr::from(([10, 1, 0, i as u8 + 1], 40_000));
            let link = profile.client_link(i as usize, o.seconds * 1_000);
            let ep = net.add_endpoint(addr, link).expect("client endpoint");
            let client = NetClient::<JournalGame, _>::connect(
                net.socket(ep),
                SocketAddr::from(SERVER),
                ClientConfig {
                    name: format!("client{i}"),
                    ..ClientConfig::default()
                },
                &i,
            )
            .expect("connect");
            Client {
                index: i,
                net: client,
                shadow: JournalSuccessor::new(World::default()),
                peer: None,
                // Every client frames at its own phase inside the server's tick.
                next_frame_us: rng.up_to(TICK_US),
                refusals: Vec::new(),
                left: false,
            }
        })
        .collect();
    let mut host = JournalHost::new(o.config.clone());
    let mut world = World::new(o.terrain_bytes);
    let mut out_full_digests = BTreeMap::new();
    let mut host_digests = BTreeMap::new();
    let mut lag: BTreeMap<u32, Lag> = BTreeMap::new();
    let mut pumped = 0_u64;
    let mut budget: BTreeMap<u32, f64> = BTreeMap::new();
    let mut last_allowance: BTreeMap<u32, (u64, f64)> = BTreeMap::new();
    let mut budget_checks = Vec::new();
    let mut allowance_stats: BTreeMap<u32, (f64, f64, f64, u64)> = BTreeMap::new();
    let mut top_rate: BTreeMap<u32, f64> = BTreeMap::new();
    let mut host_refusals = Vec::new();
    let mut successors_before_leave = Vec::new();
    let mut events = Vec::new();
    let mut ends: Vec<(u64, String, String)> = Vec::new();
    let mut pending_events: Vec<u8> = Vec::new();
    let mut dice = SoakRng::derive(o.seed, 92);
    let mut next_tick_us = 0_u64;
    let mut ticks = 0_u64;
    let (mut ticks_since_warm, mut ticks_covered) = (0_u64, 0_u64);
    let mut peer_of_index: BTreeMap<u32, u32> = BTreeMap::new();
    let state = {
        let mut s = vec![TAG_STATE];
        s.resize(STATE_BYTES, 0x5A);
        s
    };
    let share = f64::from(o.config.clone().sanitized().budget_share);

    for now in 0..=o.seconds * 1_000 {
        net.advance_to_ms(now);
        let now_us = now * 1_000;
        if now_us >= next_tick_us {
            next_tick_us += TICK_US;
            server.receive(now);
            while let Some(ev) = server.poll_event() {
                match ev {
                    ServerEvent::PeerJoined { peer, join, .. } => {
                        peer_of_index.insert(join, peer.0);
                        pending_events.push(0);
                        pending_events.extend_from_slice(&peer.0.to_le_bytes());
                        pending_events.extend_from_slice(&0u32.to_le_bytes());
                    }
                    ServerEvent::Rejected { peer, kind, .. } => {
                        ends.push((
                            now,
                            format!("server: peer {}", peer.0),
                            format!("rejected {kind:?}"),
                        ));
                    }
                    ServerEvent::PeerLeft { peer, reason } => {
                        ends.push((
                            now,
                            format!("server: peer {}", peer.0),
                            format!("left {reason:?}"),
                        ));
                        pending_events.push(1);
                        pending_events.extend_from_slice(&peer.0.to_le_bytes());
                        pending_events.extend_from_slice(&0u32.to_le_bytes());
                    }
                    ServerEvent::Message { peer, message, .. } if is_journal_payload(&message) => {
                        if let Err(e) = host.on_payload(peer, &message) {
                            host_refusals.push(e.code());
                        }
                    }
                    _ => {}
                }
            }
            // Whether the match has a warm, promotable successor right now.
            let covered = host.successors().iter().any(|peer| {
                host.is_warm(*peer)
                    && clients
                        .iter()
                        .any(|c| c.peer == Some(*peer) && c.shadow.promotable())
            });
            if covered || ticks_since_warm > 0 {
                ticks_since_warm += 1;
                ticks_covered += u64::from(covered);
            }
            // The ground-truth lag, before this tick is journaled.
            if let Some(newest) = host.newest_tick() {
                for peer in host.successors() {
                    let Some(c) = clients.iter().find(|c| c.peer == Some(peer)) else {
                        continue;
                    };
                    if host.is_warm(peer)
                        && c.shadow.promotable()
                        && let Some(applied) = c.shadow.applied_tick()
                    {
                        lag.entry(c.index)
                            .or_default()
                            .add(newest.saturating_sub(applied));
                    }
                }
            }
            let inputs = server.advance_tick(|_, _| {});
            let tick = server.driver().tick();
            ticks += 1;
            // The log's inputs: the directions that changed this tick.
            let mut input_blob = Vec::new();
            for (peer, input) in &inputs {
                if world.players.get(&peer.0).is_some_and(|p| p.2 != *input) {
                    input_blob.extend_from_slice(&peer.0.to_le_bytes());
                    input_blob.push(*input);
                }
            }
            // The host's own dice, about six events a second.
            let mut event_blob = std::mem::take(&mut pending_events);
            if dice.chance(0.1) {
                event_blob.push(2);
                event_blob.extend_from_slice(&(dice.next_u64() as u32).to_le_bytes());
                event_blob.extend_from_slice(&(dice.next_u64() as u32).to_le_bytes());
            }
            world.step(tick, &input_blob, &event_blob).expect("host step");
            let digest = world.world_digest();
            host_digests.insert(tick, digest);
            if host.full_wanted(now) {
                out_full_digests.insert(tick, digest);
                host.record_tick(tick, &input_blob, &event_blob, Some(digest))
                    .expect("record_tick");
                host.record_full(now, tick, world.encode(), digest)
                    .expect("record_full");
            } else {
                host.record_tick(tick, &input_blob, &event_blob, None)
                    .expect("record_tick");
            }
            // The budget the journal may have spent on each successor up to now, integrated here
            // from the transport's allowance: the rate read at one pump pays for the time to the
            // next.
            for peer in host.successors() {
                let allowance = f64::from(
                    server
                        .send_allowance(peer)
                        .unwrap_or(o.config.fallback_bytes_per_s),
                );
                let stats = allowance_stats
                    .entry(peer.0)
                    .or_insert((allowance, allowance, 0.0, 0));
                *stats = (
                    stats.0.min(allowance),
                    stats.1.max(allowance),
                    stats.2 + allowance,
                    stats.3 + 1,
                );
                let top = top_rate.entry(peer.0).or_default();
                *top = top.max(allowance * share);
                if let Some((at, rate)) = last_allowance.insert(peer.0, (now, allowance * share)) {
                    *budget.entry(peer.0).or_default() += rate * (now - at) as f64 / 1_000.0;
                }
            }
            if o.journal {
                pumped += host.pump_server(now, &mut server, |bytes| bytes) as u64;
            }
            while let Some(e) = host.poll_event() {
                events.push((now, e));
            }
            last_allowance.retain(|peer, _| host.successors().contains(&PeerId(*peer)));
            if ticks.is_multiple_of(60) {
                for row in &host.status().successors {
                    budget_checks.push((
                        now,
                        row.peer,
                        row.bytes_sent,
                        budget.get(&row.peer).copied().unwrap_or(0.0),
                        top_rate.get(&row.peer).copied().unwrap_or(0.0),
                    ));
                }
            }
            // The game's own state frames: half of each client's allowance.
            for peer in peer_of_index.values() {
                let allowance = server.send_allowance(PeerId(*peer)).unwrap_or(64 * 1024) as usize;
                let bytes = (allowance / 2 / 60).clamp(16, STATE_BYTES);
                let _ = server.send(PeerId(*peer), Channel::Unreliable, &state[..bytes].to_vec());
            }
            server.flush(now);
        }
        if let Some(at) = o.first_successor_leaves_ms
            && now == at
        {
            successors_before_leave = host.successors();
            let first = successors_before_leave.first().copied().expect("a successor");
            let c = clients
                .iter_mut()
                .find(|c| c.peer == Some(first))
                .expect("the first successor's client");
            c.net.disconnect();
            c.net.flush(now);
            c.left = true;
        }
        for c in &mut clients {
            if c.left || now_us < c.next_frame_us {
                continue;
            }
            c.next_frame_us += TICK_US;
            c.net.receive(now);
            while let Some(ev) = c.net.poll_event() {
                match ev {
                    ClientEvent::Connected { peer, .. } => c.peer = Some(peer),
                    ClientEvent::Ended(end) => {
                        ends.push((now, format!("client {}", c.index), format!("{end:?}")));
                    }
                    ClientEvent::Message { channel, message } if is_journal_payload(&message) => {
                        assert_eq!(
                            channel,
                            Channel::Stream(StreamId::BULK),
                            "the journal rides the bulk stream"
                        );
                        if let Err(e) = c.shadow.on_payload(&message) {
                            c.refusals.push(e.code());
                        }
                    }
                    _ => {}
                }
            }
            if c.net.is_connected() {
                for tick in c.net.due_input_ticks(now) {
                    // A direction (0-4), held for about 0.6 s.
                    let direction = ((tick / 37 + u64::from(c.index) * 3) % 5) as u8;
                    let _ = c.net.send_input(tick, direction);
                }
                if let Some(ack) = c.shadow.poll_ack() {
                    let _ = c.net.send(Channel::Unreliable, &ack);
                }
            }
            c.net.flush(now);
        }
    }
    Outcome {
        host,
        world,
        clients,
        full_digests: out_full_digests,
        host_digests,
        lag,
        pumped,
        budget_checks,
        allowance: allowance_stats,
        host_refusals,
        successors_before_leave,
        events,
        ends,
        ticks,
        ticks_since_warm,
        ticks_covered,
    }
}

impl Outcome {
    fn client_of(&self, peer: PeerId) -> &Client {
        self.clients
            .iter()
            .find(|c| c.peer == Some(peer))
            .expect("the peer's client")
    }

    /// Checks every shadow against the host: the digest after every tick it applied from the log
    /// and after every full state it loaded is the host's digest of that tick, which covers
    /// every full-state point it passed. Returns per client index how many full-state points
    /// its replayed shadow was checked at.
    fn assert_shadows_match_the_host(&self, label: &str) -> BTreeMap<u32, usize> {
        let mut checked = BTreeMap::new();
        for c in &self.clients {
            let w = c.shadow.world();
            for (tick, d) in &w.applied {
                assert_eq!(
                    Some(d),
                    self.host_digests.get(tick),
                    "{label}: client {} replayed shadow differs from the host at tick {tick}",
                    c.index
                );
            }
            for (tick, d) in &w.loaded {
                assert_eq!(
                    Some(d),
                    self.full_digests.get(tick),
                    "{label}: client {} loaded shadow differs from the host at full-state tick {tick}",
                    c.index
                );
            }
            let by_replay = self
                .full_digests
                .keys()
                .filter(|tick| w.applied.contains_key(tick))
                .count();
            checked.insert(c.index, by_replay);
            let status = c.shadow.status();
            assert_eq!(
                status.replay_divergences, 0,
                "{label}: client {}: {status:?}",
                c.index
            );
            assert_eq!(
                status.digest_mismatches, 0,
                "{label}: client {}: {status:?}",
                c.index
            );
            assert!(
                c.refusals.is_empty(),
                "{label}: client {} refused {:?}",
                c.index,
                c.refusals
            );
        }
        checked
    }

    fn summary(&self, label: &str) -> String {
        let status = self.host.status();
        let rows: Vec<String> = status
            .successors
            .iter()
            .map(|r| {
                let c = self.client_of(PeerId(r.peer));
                let truth = self.lag.get(&c.index).cloned().unwrap_or_default();
                format!(
                    "peer {} rank {} {} (reloads {}): lag p50/p99/max {}/{}/{} ticks over {} samples {:?} (host reads p99 {} max {}), {} B sent, {} B/s of {} B/s budget, allowance min/mean/max {:.0}/{:.0}/{:.0} B/s, fell behind {}",
                    r.peer,
                    r.rank,
                    r.state,
                    c.shadow.status().reloads,
                    truth.percentile(50),
                    truth.percentile(99),
                    truth.counts.keys().last().copied().unwrap_or(0),
                    truth.samples,
                    truth.counts.iter().filter(|(lag, _)| **lag >= 3).take(12).collect::<Vec<_>>(),
                    r.lag_ticks_p99,
                    r.lag_ticks_max,
                    r.bytes_sent,
                    r.bytes_per_s,
                    r.budget_bytes_per_s,
                    self.allowance.get(&r.peer).map_or(0.0, |a| a.0),
                    self.allowance.get(&r.peer).map_or(0.0, |a| a.2 / a.3.max(1) as f64),
                    self.allowance.get(&r.peer).map_or(0.0, |a| a.1),
                    r.fell_behind,
                )
            })
            .collect();
        let mut reasons: BTreeMap<&'static str, usize> = BTreeMap::new();
        for (_, e) in &self.events {
            if let regia_net::migration::JournalEvent::Dismissed { reason, .. } = e {
                *reasons.entry(reason).or_default() += 1;
            }
        }
        format!(
            "{label}: {} ticks, dismissals {reasons:?}, a warm successor on {} of the {} ticks since the first, {} full states of {} B, {} successor changes, {} B pumped, host refused {:?}; {}",
            self.ticks,
            self.ticks_covered,
            self.ticks_since_warm,
            status.full_states,
            status.full_state_bytes,
            status.successor_changes,
            self.pumped,
            self.host_refusals.len(),
            rows.join("; ")
        )
    }
}

/// Acceptance: on bad-wifi (8 ms + 0-30 ms jitter, about 3 % loss in bursts, reorder,
/// duplicates) every shadow has the host's world digest at every full-state point it passed, by
/// replaying the log (and at every other tick it applied). Both successors pass every full state
/// after their seed, stay warm and promotable, and nobody loses the connection.
///
/// The rate controller holds bad-wifi links at or near its 4 KiB/s floor for much of a run, so a
/// quarter share is about 1 KiB/s: the sample world here is 5 KiB (the good-wifi tests use
/// 50 KiB), which that budget carries next to the log.
#[test]
fn shadows_equal_the_host_digest_at_every_full_state_on_bad_wifi() {
    for seed in [1, 2, 3] {
        let mut o = Options::new("bad-wifi", seed);
        o.terrain_bytes = 4 * 1024;
        o.seconds = 90;
        let out = run(&o);
        let label = format!("bad-wifi seed {seed}");
        println!("{}", out.summary(&label));
        let checked = out.assert_shadows_match_the_host(&label);
        let by_replay: usize = checked.values().sum();
        println!("{label}: full-state points checked by replay per client: {checked:?}");
        // 90 s at a full state every 5 s is 18 full states: the seed, and 17 each successor
        // passes by replay. Three are left for a change of successor.
        let full_states = out.full_digests.len();
        assert!(full_states >= 17, "{label}: {full_states} full states");
        assert!(
            by_replay >= 2 * (full_states - 3),
            "{label}: {by_replay} full-state points checked by replay, {full_states} full states"
        );
        assert!(out.ends.is_empty(), "{label}: a connection ended: {:?}", out.ends);
        let status = out.host.status();
        assert_eq!(status.successors.len(), 2, "{label}: {status:?}");
        for row in &status.successors {
            let shadow = out.client_of(PeerId(row.peer)).shadow.status();
            assert_eq!(row.state, "warm", "{label}: {row:?}");
            assert_eq!(row.fell_behind, 0, "{label}: {row:?}");
            assert!(shadow.promotable, "{label}: {shadow:?}");
            assert_eq!(
                shadow.reloads, 1,
                "{label}: every later full state was adopted: {shadow:?}"
            );
        }
        // From the first warm successor on, the match always has one.
        assert_eq!(out.ticks_covered, out.ticks_since_warm, "{label}");
    }
}

/// Acceptance: on good-wifi (4 ms + 0-4 ms jitter, 0.2 % loss) a warm shadow trails the host's
/// newest journaled tick by at most 2 ticks at the 99th percentile.
#[test]
fn journal_lag_is_at_most_two_ticks_p99_on_good_wifi() {
    for seed in [1, 2, 3] {
        let out = run(&Options::new("good-wifi", seed));
        let label = format!("good-wifi seed {seed}");
        println!("{}", out.summary(&label));
        out.assert_shadows_match_the_host(&label);
        let status = out.host.status();
        assert_eq!(status.successors.len(), 2, "{label}: {status:?}");
        assert_eq!(
            status.successor_changes, 1,
            "{label}: good links keep their successors"
        );
        assert!(out.ends.is_empty(), "{label}: a connection ended: {:?}", out.ends);
        for row in &status.successors {
            let c = out.client_of(PeerId(row.peer));
            let truth = &out.lag[&c.index];
            assert!(truth.samples > 3_000, "{label}: {} samples", truth.samples);
            assert!(
                truth.percentile(99) <= 2,
                "{label}: client {} journal lag p99 {} ticks ({:?})",
                c.index,
                truth.percentile(99),
                truth.counts
            );
            assert_eq!(row.state, "warm", "{label}: {row:?}");
            assert_eq!(row.fell_behind, 0, "{label}: {row:?}");
        }
    }
}

/// Acceptance: the journal spends no more than its share of the rate controller's allowance for
/// each successor: at every whole second the bytes the host counted are within the budget this
/// test integrates from `NetServer::send_allowance`: on good-wifi at the default quarter share
/// with a 50 KiB world (the journal uses nearly all of it) and at a 5 % share with a 9 KiB world
/// (the budget binds and the successors still get warm), and on bad-wifi at the quarter share.
#[test]
fn journal_bandwidth_stays_within_its_budget_share() {
    for (profile, share, terrain) in [
        ("good-wifi", 0.25, 48 * 1024),
        ("good-wifi", 0.05, 8 * 1024),
        ("clean", 0.05, 8 * 1024),
        ("bad-wifi", 0.25, 4 * 1024),
    ] {
        let mut o = Options::new(profile, 5);
        o.config.budget_share = share;
        o.terrain_bytes = terrain;
        let out = run(&o);
        let label = format!("{profile} share {share}");
        println!("{}", out.summary(&label));
        assert!(
            out.budget_checks.len() >= 100,
            "{label}: {} checks",
            out.budget_checks.len()
        );
        for (now, peer, sent, budget, top_rate) in &out.budget_checks {
            // The idle bucket: two ticks of the budget rate, at least one 900-byte payload.
            let burst = (top_rate * 2.0 / 60.0).max(900.0);
            assert!(
                (*sent as f64) <= budget + burst,
                "{label}: at {now} ms the journal to peer {peer} had sent {sent} B against a budget of {budget:.0} B"
            );
        }
        let status = out.host.status();
        for row in &status.successors {
            assert_eq!(row.seconds_over_budget, 0, "{label}: {row:?}");
            assert!(row.bytes_sent <= row.budget_bytes + 1, "{label}: {row:?}");
            if profile != "bad-wifi" {
                assert_eq!(row.state, "warm", "{label}: {row:?}");
                assert_eq!(row.fell_behind, 0, "{label}: {row:?}");
            }
        }
        assert!(out.pumped > 0, "{label}: nothing was sent");
        out.assert_shadows_match_the_host(&label);
    }
}

/// Successor change on real links: the first successor disconnects at 20 s. The second becomes
/// the first and stays warm without another full state loaded; the best connected of the rest is
/// appointed and is warm within 3 s.
#[test]
fn a_successor_that_leaves_is_replaced_and_the_second_keeps_its_shadow() {
    let mut o = Options::new("good-wifi", 7);
    o.seconds = 40;
    o.first_successor_leaves_ms = Some(20_000);
    let out = run(&o);
    println!("{}", out.summary("good-wifi, first successor leaves at 20 s"));
    let before = &out.successors_before_leave;
    assert_eq!(before.len(), 2);
    let after = out.host.successors();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0], before[1], "the second successor is first now");
    assert!(!before.contains(&after[1]), "and a new client is second");
    let second = out.client_of(after[0]);
    assert_eq!(second.shadow.rank(), Some(0));
    assert_eq!(
        second.shadow.status().reloads,
        1,
        "its shadow was never seeded again"
    );
    let new = out.client_of(after[1]);
    assert_eq!(new.shadow.status().state, "warm");
    let appointed_at = out
        .events
        .iter()
        .find(|(_, e)| matches!(e, regia_net::migration::JournalEvent::Appointed { peer, rank: 1 } if *peer == after[1]))
        .map(|(at, _)| *at)
        .expect("the new successor's appointment");
    let warm_at = out
        .events
        .iter()
        .find(|(_, e)| matches!(e, regia_net::migration::JournalEvent::Warm { peer } if *peer == after[1]))
        .map(|(at, _)| *at)
        .expect("the new successor is warm");
    assert!(
        (20_000..=21_500).contains(&appointed_at),
        "appointed at {appointed_at} ms"
    );
    assert!(
        warm_at - appointed_at <= 3_000,
        "warm {} ms after its appointment",
        warm_at - appointed_at
    );
    out.assert_shadows_match_the_host("successor change");
    for peer in &after {
        let c = out.client_of(*peer);
        assert!(c.shadow.promotable());
        assert!(c.refusals.is_empty(), "{:?}", c.refusals);
    }
    // The host's world and the first successor's shadow agree at the shadow's tick: replay the
    // promotion's journal on a fresh world.
    let applied = second.shadow.applied_tick().expect("applied");
    assert!(
        out.world.tick - applied <= 3,
        "{} ticks behind",
        out.world.tick - applied
    );
}

/// The journal must not cost a player its connection. On bad-wifi nobody's connection ends,
/// with the journal on as with it off (the same seeds, the same game). A journal that handed the
/// bulk stream a payload every tick, without waiting for the one before, lost seven of the eight
/// clients of seed 1 to the stream's reassembly stall deadline.
#[test]
fn the_journal_drops_no_player_on_bad_wifi() {
    for seed in [1, 2, 3] {
        let mut on = Options::new("bad-wifi", seed);
        on.terrain_bytes = 4 * 1024;
        on.seconds = 90;
        let mut off = Options::new("bad-wifi", seed);
        off.terrain_bytes = 4 * 1024;
        off.seconds = 90;
        off.journal = false;
        let with = run(&on);
        let without = run(&off);
        println!("bad-wifi seed {seed} journal on: ends {:?}", with.ends);
        println!("bad-wifi seed {seed} journal off: ends {:?}", without.ends);
        assert!(
            with.ends.is_empty() && without.ends.is_empty(),
            "bad-wifi seed {seed}: with the journal {:?}; without {:?}",
            with.ends,
            without.ends
        );
        assert_eq!(with.host.successors().len(), 2);
    }
}
