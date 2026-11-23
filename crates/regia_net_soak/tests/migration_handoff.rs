// SPDX-License-Identifier: MPL-2.0
//! The hand-off on impaired links (spec 120 § 10.5, lane N303, ADR-8901): a 16-player listen
//! match (a host and 15 clients on the real transport, `NetServer` / `NetClient`, sealed links)
//! loses its host and continues on a warm successor, on the harness's seeded `good-wifi` and
//! `bad-wifi` links, in virtual time.
//!
//! The acceptance rows: the match continues on the first successor within 5 s with the same
//! `PeerId`s and every entity present; with the first successor dead too the second takes over
//! within 8 s; a host that was only cut off comes back as a client on the seat kept for it; a
//! client in its own short outage loses nothing and no second match appears.
//!
//! After the hand-off every path is client to client: the promoted successor serves from its own
//! access link, so a datagram crosses two impaired links instead of one.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use regia_net::handoff::{
    HandoffAction, HandoffClient, HandoffConfig, HandoffEvent, HandoffHost, HostLink, PromotionPlan,
    is_handoff_payload,
};
use regia_net::migration::{
    DIGEST_BYTES, JournalConfig, JournalHost, JournalSuccessor, ShadowWorld, WorldDigest, is_journal_payload,
};
use regia_net::{
    Channel, ClientConfig, ClientEvent, NetClient, NetGame, NetServer, PeerId, ServerConfig, ServerEvent,
    WireReader, WireWriter,
};
use regia_net_soak::impair::{EndpointId, ImpairedNet, ImpairedSocket, LinkSpec, SoakRng};
use regia_net_soak::profiles::Profile;

/// One server tick at 60 Hz, microseconds.
const TICK_US: u64 = 16_667;
const HOST: ([u8; 4], u16) = ([10, 0, 0, 1], 7777);
/// First byte of the sample game's own unreliable state message.
const TAG_STATE: u8 = 0x01;
/// Most bytes of the state message a host sends every client every tick.
const STATE_BYTES: usize = 300;
/// Bytes of the state message before its padding.
const STATE_HEAD: usize = 1 + 8 + 8 + 4 + 2;
/// The host and 15 clients.
const PLAYERS: u32 = 16;

struct HandoffGame;

impl NetGame for HandoffGame {
    type Message = Vec<u8>;
    type Input = u8;
    type JoinData = u32;
    const GAME_ID: &'static str = "regia.test.migration_handoff";
    const PROTOCOL_VERSION: u32 = 1;
}

/// The sample world: one player entity per participant (keyed by `PeerId`, the host's own under
/// `PeerId::SERVER`), cells the host's own events stir, and a terrain blob that gives the full
/// state its size. A player moves by the direction it holds.
#[derive(Debug, Clone, Default)]
struct World {
    tick: u64,
    /// Per player: position, direction changes, the direction held.
    players: BTreeMap<u32, (i64, u32, u8)>,
    cells: Vec<u64>,
    terrain: Vec<u8>,
}

impl World {
    fn new(terrain_bytes: usize) -> Self {
        Self {
            cells: vec![0; 64],
            terrain: (0..terrain_bytes).map(|i| (i * 131 % 251) as u8).collect(),
            ..Self::default()
        }
    }

    /// One tick: `inputs` is `(player u32, new direction u8)*`, `events` is
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
                // A player's entity moves to another id: the lost host's own player gets the
                // seat the promoted successor kept for it.
                3 => {
                    if let Some(p) = self.players.remove(&a) {
                        self.players.insert(b, p);
                    }
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
            let player = u32::from_le_bytes([i[0], i[1], i[2], i[3]]);
            if let Some((_, changes, held)) = self.players.get_mut(&player) {
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
        for (player, (x, changes, held)) in &self.players {
            w.u32(*player);
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
            let player = r.u32()?;
            let x = r.u64()? as i64;
            let changes = r.u32()?;
            world.players.insert(player, (x, changes, r.u8()?));
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
        *self = world;
        Ok(())
    }

    fn apply(&mut self, tick: u64, inputs: &[u8], events: &[u8]) -> Result<(), String> {
        if tick != self.tick + 1 {
            return Err(format!("tick {tick} on a shadow at {}", self.tick));
        }
        self.step(tick, inputs, events)
    }

    fn digest(&self) -> WorldDigest {
        self.world_digest()
    }
}

/// The direction (0-4) participant `index` holds at `tick`: it changes about every 0.6 s.
fn direction(tick: u64, index: u32) -> u8 {
    ((tick / 37 + u64::from(index) * 3) % 5) as u8
}

fn server_config() -> ServerConfig {
    ServerConfig {
        max_peers: PLAYERS as usize + 4,
        tick_rate_hz: 60,
        ..ServerConfig::default()
    }
}

/// What a promoted successor started from.
#[derive(Debug, Clone)]
struct Promoted {
    at_ms: u64,
    rank: u8,
    /// The shadow's tick at the promotion.
    tick: u64,
    /// The shadow's digest at that tick.
    digest: WorldDigest,
    /// The player ids in the shadow at the promotion.
    players: BTreeSet<u32>,
    /// Each player's direction changes at the promotion.
    changes: BTreeMap<u32, u32>,
    host_seat: PeerId,
    own_peer: PeerId,
}

/// A machine that hosts: the first host, or a promoted successor.
struct Hosting {
    server: NetServer<HandoffGame, ImpairedSocket>,
    world: World,
    journal: JournalHost,
    handoff: HandoffHost,
    /// The id of this host's own player in the world.
    own_key: u32,
    pending_events: Vec<u8>,
    dice: SoakRng,
    next_tick_us: u64,
    /// The world's digest after every tick, and its player ids.
    digests: BTreeMap<u64, WorldDigest>,
    promoted: Option<Promoted>,
    /// Players that resumed here: id and when.
    resumed: BTreeMap<u32, u64>,
    /// Players that joined here as new peers after a promotion (none are expected).
    joined_after: Vec<u32>,
    left: Vec<(u64, u32, String)>,
    refusals: Vec<&'static str>,
}

struct Playing {
    net: NetClient<HandoffGame, ImpairedSocket>,
    shadow: JournalSuccessor<World>,
    ended: bool,
    /// A resume attempt on the machine's second socket, while the connection above is kept.
    attempt: Option<NetClient<HandoffGame, ImpairedSocket>>,
    /// This successor's armed server on the machine's third socket: it holds the players and
    /// does not tick until the hand-off commits.
    armed: Option<Box<Armed>>,
}

struct Armed {
    server: NetServer<HandoffGame, ImpairedSocket>,
    plan: PromotionPlan,
    /// The shadow's tick when the server was armed.
    base_tick: u64,
    next_tick_us: u64,
    /// Players that resumed while it was armed: id and when.
    resumed: BTreeMap<u32, u64>,
}

enum Role {
    Host(Box<Hosting>),
    Client(Box<Playing>),
    Dead,
}

/// One participant's machine: an access link with three sockets on it (the connection to the
/// host, a resume attempt, and the server a successor announced).
struct Machine {
    index: u32,
    eps: [EndpointId; 3],
    /// The socket not in use (a resume attempt takes it; the one it replaces comes back).
    spare: Option<ImpairedSocket>,
    role: Role,
    handoff: HandoffClient,
    /// The old host's hand-off, kept for its way back.
    old_host: Option<HandoffHost>,
    /// The `PeerId` on the first host.
    first_peer: Option<PeerId>,
    peer: Option<PeerId>,
    next_frame_us: u64,
    /// The newest state frame: (tick, own position, own direction changes, players).
    state: Option<(u64, i64, u32, u16)>,
    resumed_ms: Option<u64>,
    resumed_peer: Option<PeerId>,
    /// The first state frame after the resume: (when, its tick).
    first_state: Option<(u64, u64)>,
    gave_up: bool,
    events: Vec<(u64, HandoffEvent)>,
    ends: Vec<(u64, String)>,
    refusals: Vec<&'static str>,
}

struct Options {
    profile: &'static str,
    seed: u64,
    seconds: u64,
    terrain_bytes: usize,
    /// The host stops at this time (nothing more is received or sent by it).
    kill_host_ms: Option<u64>,
    /// The host says goodbye first (`NetServer::shutdown`).
    graceful: bool,
    /// The first successor stops at the same time.
    kill_first_successor: bool,
    /// The host's link is out in this window; the host keeps running.
    host_outage: Option<(u64, u64)>,
    /// Client `index`'s link is out in this window.
    client_outage: Option<(u32, u64, u64)>,
}

impl Options {
    fn new(profile: &'static str, seed: u64) -> Self {
        Self {
            profile,
            seed,
            seconds: 32,
            // bad-wifi keeps a successor's send allowance at the rate floor for much of a run
            // (lane N302's finding): a world of a few KiB is what its journal share carries.
            terrain_bytes: if profile == "bad-wifi" {
                5 * 1024
            } else {
                48 * 1024
            },
            kill_host_ms: Some(20_000),
            graceful: false,
            kill_first_successor: false,
            host_outage: None,
            client_outage: None,
        }
    }
}

/// What the lost host knew when it stopped.
#[derive(Debug, Clone, Default)]
struct Lost {
    at_ms: u64,
    tick: u64,
    players: BTreeSet<u32>,
    /// The roster's successors, first successor first, as machine indices.
    successors: Vec<u32>,
    digests: BTreeMap<u64, WorldDigest>,
}

struct Outcome {
    machines: Vec<Machine>,
    lost: Option<Lost>,
    /// When a host that was only cut off saw itself replaced and stopped hosting.
    deposed_ms: Option<u64>,
}

fn connect(
    socket: ImpairedSocket,
    to: SocketAddr,
    config: ClientConfig,
    index: u32,
) -> NetClient<HandoffGame, ImpairedSocket> {
    NetClient::<HandoffGame, _>::connect(socket, to, config, &index).expect("connect")
}

fn client_config(index: u32) -> ClientConfig {
    ClientConfig {
        name: format!("player{index}"),
        ..ClientConfig::default()
    }
}

fn host_tick(now: u64, index: u32, h: &mut Hosting, own: &mut HandoffClient) {
    h.next_tick_us += TICK_US;
    h.server.receive(now);
    while let Some(ev) = h.server.poll_event() {
        match ev {
            ServerEvent::PeerJoined { peer, .. } => {
                if h.promoted.is_some() {
                    h.joined_after.push(peer.0);
                }
                h.pending_events.push(0);
                h.pending_events.extend_from_slice(&peer.0.to_le_bytes());
                h.pending_events.extend_from_slice(&0u32.to_le_bytes());
            }
            ServerEvent::PeerResumed { peer, .. } => {
                h.resumed.insert(peer.0, now);
                own.peer_resumed(now, peer);
            }
            ServerEvent::PeerLeft { peer, reason } => {
                h.left.push((now, peer.0, format!("{reason:?}")));
                h.pending_events.push(1);
                h.pending_events.extend_from_slice(&peer.0.to_le_bytes());
                h.pending_events.extend_from_slice(&0u32.to_le_bytes());
            }
            ServerEvent::Message { peer, message, .. } => {
                if is_journal_payload(&message) {
                    if let Err(e) = h.journal.on_payload(peer, &message) {
                        h.refusals.push(e.code());
                    }
                } else if is_handoff_payload(&message)
                    && let Err(e) = h.handoff.on_payload(peer, &message)
                {
                    h.refusals.push(e.code());
                }
            }
            _ => {}
        }
    }
    let inputs = h.server.advance_tick(|_, _| {});
    let tick = h.server.driver().tick();
    // The log's inputs: the directions that changed this tick, the host's own player's too.
    let mut input_blob = Vec::new();
    for (peer, input) in &inputs {
        if h.world.players.get(&peer.0).is_some_and(|p| p.2 != *input) {
            input_blob.extend_from_slice(&peer.0.to_le_bytes());
            input_blob.push(*input);
        }
    }
    let mine = direction(tick, index);
    if h.world.players.get(&h.own_key).is_some_and(|p| p.2 != mine) {
        input_blob.extend_from_slice(&h.own_key.to_le_bytes());
        input_blob.push(mine);
    }
    // The host's own dice, about six events a second.
    let mut event_blob = std::mem::take(&mut h.pending_events);
    if h.dice.chance(0.1) {
        event_blob.push(2);
        event_blob.extend_from_slice(&(h.dice.next_u64() as u32).to_le_bytes());
        event_blob.extend_from_slice(&(h.dice.next_u64() as u32).to_le_bytes());
    }
    h.world.step(tick, &input_blob, &event_blob).expect("host step");
    let digest = h.world.world_digest();
    h.digests.insert(tick, digest);
    if h.journal.full_wanted(now) {
        h.journal
            .record_tick(tick, &input_blob, &event_blob, Some(digest))
            .expect("record_tick");
        h.journal
            .record_full(now, tick, h.world.encode(), digest)
            .expect("record_full");
    } else {
        h.journal
            .record_tick(tick, &input_blob, &event_blob, None)
            .expect("record_tick");
    }
    h.journal.pump_server(now, &mut h.server, |bytes| bytes);
    while h.journal.poll_event().is_some() {}
    h.handoff.pump_server(&mut h.server, &h.journal, |bytes| bytes);
    // The game's own state frames: each player's own entity, sized to half its allowance.
    let peers: Vec<PeerId> = h.server.session().peers().map(|p| p.id).collect();
    for peer in peers {
        let (x, changes, _) = h.world.players.get(&peer.0).copied().unwrap_or_default();
        let allowance = h.server.send_allowance(peer).unwrap_or(64 * 1024) as usize;
        let mut w = WireWriter::new();
        w.u8(TAG_STATE);
        w.u64(tick);
        w.u64(x as u64);
        w.u32(changes);
        w.u16(h.world.players.len() as u16);
        let mut frame = w.finish();
        frame.resize((allowance / 2 / 60).clamp(STATE_HEAD, STATE_BYTES), 0x5A);
        let _ = h.server.send(peer, Channel::Unreliable, &frame);
    }
    h.server.flush(now);
}

fn run(o: &Options) -> Outcome {
    let net = ImpairedNet::new(o.seed);
    let profile = Profile::named(o.profile).expect("known profile");
    let mut rng = SoakRng::derive(o.seed, 91);
    // Machine 0 hosts first; like N302's runs its own link is clean unless the test cuts it.
    let host_spec = LinkSpec {
        outages: o.host_outage.into_iter().collect(),
        ..LinkSpec::default()
    };
    let mut machines: Vec<Machine> = (0..PLAYERS)
        .map(|i| {
            let (addr, spec) = if i == 0 {
                (SocketAddr::from(HOST), host_spec.clone())
            } else {
                let mut spec = profile.client_link(i as usize - 1, o.seconds * 1_000);
                if let Some((index, from, to)) = o.client_outage
                    && index == i
                {
                    spec.outages.push((from, to));
                }
                (SocketAddr::from(([10, 1, 0, i as u8], 40_000)), spec)
            };
            let at = |port: u16| SocketAddr::new(addr.ip(), port);
            let eps = [
                net.add_endpoint(addr, spec.clone()).expect("endpoint"),
                net.add_endpoint(at(addr.port() + 1), spec.clone())
                    .expect("second endpoint"),
                net.add_endpoint(at(addr.port() + 2), spec)
                    .expect("third endpoint"),
            ];
            let role = if i == 0 {
                let mut world = World::new(o.terrain_bytes);
                world.players.insert(PeerId::SERVER.0, (0, 0, 0));
                Role::Host(Box::new(Hosting {
                    server: NetServer::<HandoffGame, _>::new(net.socket(eps[0]), server_config()),
                    world,
                    journal: JournalHost::new(JournalConfig::default()),
                    handoff: HandoffHost::new(HandoffConfig::default()),
                    own_key: PeerId::SERVER.0,
                    pending_events: Vec::new(),
                    dice: SoakRng::derive(o.seed, 92),
                    next_tick_us: 0,
                    digests: BTreeMap::new(),
                    promoted: None,
                    resumed: BTreeMap::new(),
                    joined_after: Vec::new(),
                    left: Vec::new(),
                    refusals: Vec::new(),
                }))
            } else {
                Role::Client(Box::new(Playing {
                    net: connect(net.socket(eps[0]), SocketAddr::from(HOST), client_config(i), i),
                    shadow: JournalSuccessor::new(World::default()),
                    ended: false,
                    attempt: None,
                    armed: None,
                }))
            };
            Machine {
                index: i,
                eps,
                spare: Some(net.socket(eps[1])),
                role,
                // A successor serves on the machine's third socket.
                handoff: HandoffClient::new(HandoffConfig::default()).with_port(addr.port() + 2),
                old_host: None,
                first_peer: None,
                peer: None,
                // Every machine frames at its own phase inside the tick.
                next_frame_us: rng.up_to(TICK_US),
                state: None,
                resumed_ms: None,
                resumed_peer: None,
                first_state: None,
                gave_up: false,
                events: Vec::new(),
                ends: Vec::new(),
                refusals: Vec::new(),
            }
        })
        .collect();
    let mut lost: Option<Lost> = None;
    let mut deposed_ms = None;

    for now in 0..=o.seconds * 1_000 {
        net.advance_to_ms(now);
        let now_us = now * 1_000;
        if o.kill_host_ms == Some(now) {
            // What the host knew, then it stops.
            let peer_to_index: BTreeMap<PeerId, u32> = machines
                .iter()
                .filter_map(|m| Some((m.first_peer?, m.index)))
                .collect();
            let Role::Host(mut h) = std::mem::replace(&mut machines[0].role, Role::Dead) else {
                panic!("machine 0 hosts until it is killed");
            };
            let successors: Vec<u32> = h
                .handoff
                .ready_successors()
                .iter()
                .map(|p| peer_to_index[p])
                .collect();
            if o.graceful {
                h.server.shutdown();
                h.server.flush(now);
            }
            if o.kill_first_successor {
                let first = successors.first().copied().expect("a successor to kill");
                machines[first as usize].role = Role::Dead;
            }
            lost = Some(Lost {
                at_ms: now,
                tick: h.world.tick,
                players: h.world.players.keys().copied().collect(),
                successors,
                digests: std::mem::take(&mut h.digests),
            });
        }
        for m in &mut machines {
            match m.role {
                Role::Dead => {}
                Role::Host(_) => host_frame(now, m, &net, &mut lost, &mut deposed_ms),
                Role::Client(_) => {
                    if now_us >= m.next_frame_us {
                        m.next_frame_us += TICK_US;
                        client_frame(now, m, o, &net);
                    }
                }
            }
        }
    }
    Outcome {
        machines,
        lost,
        deposed_ms,
    }
}

/// One tick of a hosting machine, and the check whether it has been replaced.
fn host_frame(
    now: u64,
    m: &mut Machine,
    net: &ImpairedNet,
    lost: &mut Option<Lost>,
    deposed_ms: &mut Option<u64>,
) {
    let Role::Host(h) = &mut m.role else {
        return;
    };
    if now * 1_000 < h.next_tick_us {
        return;
    }
    host_tick(now, m.index, h, &mut m.handoff);
    let _ = m.handoff.poll(now, HostLink::gone(), false);
    while let Some(e) = m.handoff.poll_event() {
        m.events.push((now, e));
    }
    // A host whose every client went silent after it announced successors has been replaced:
    // it stops hosting and goes back as a client.
    let all_silent = h.server.session().peers().all(|p| p.interrupted);
    if h.promoted.is_some() || !h.handoff.deposed(all_silent) {
        return;
    }
    let Some(mut back) = h.handoff.comeback(now) else {
        return;
    };
    let Role::Host(h) = std::mem::replace(&mut m.role, Role::Dead) else {
        unreachable!()
    };
    let Hosting {
        server,
        handoff,
        world,
        digests,
        ..
    } = *h;
    *deposed_ms = Some(now);
    *lost = Some(Lost {
        at_ms: now,
        tick: world.tick,
        players: world.players.keys().copied().collect(),
        successors: Vec::new(),
        digests,
    });
    drop(server);
    let Some(HandoffAction::Resume(target)) = back.poll(now, HostLink::gone(), false) else {
        panic!("a deposed host resumes to its first successor");
    };
    m.role = Role::Client(Box::new(Playing {
        net: connect(
            net.socket(m.eps[0]),
            target.addr,
            target.client_config(client_config(m.index)),
            m.index,
        ),
        shadow: JournalSuccessor::new(World::default()),
        ended: false,
        attempt: None,
        armed: None,
    }));
    m.handoff = back;
    m.old_host = Some(handoff);
    m.next_frame_us = now * 1_000;
}

/// What a client machine keeps about the game's messages.
struct Seen<'a> {
    refusals: &'a mut Vec<&'static str>,
    state: &'a mut Option<(u64, i64, u32, u16)>,
    first_state: &'a mut Option<(u64, u64)>,
    resumed: bool,
}

fn on_message(
    now: u64,
    message: &[u8],
    shadow: &mut JournalSuccessor<World>,
    handoff: &mut HandoffClient,
    seen: &mut Seen<'_>,
) {
    if is_journal_payload(message) {
        if let Err(e) = shadow.on_payload(message) {
            seen.refusals.push(e.code());
        }
    } else if is_handoff_payload(message) {
        if let Err(e) = handoff.on_payload(message) {
            seen.refusals.push(e.code());
        }
    } else if message.first() == Some(&TAG_STATE) && message.len() >= STATE_HEAD {
        let mut r = WireReader::new(&message[1..STATE_HEAD]);
        let (Ok(tick), Ok(x), Ok(changes), Ok(players)) = (r.u64(), r.u64(), r.u32(), r.u16()) else {
            return;
        };
        *seen.state = Some((tick, x as i64, changes, players));
        if seen.resumed && seen.first_state.is_none() {
            *seen.first_state = Some((now, tick));
        }
    }
}

/// One frame of a client machine.
fn client_frame(now: u64, m: &mut Machine, o: &Options, net: &ImpairedNet) {
    let Machine {
        index,
        eps,
        spare,
        role,
        handoff,
        old_host,
        first_peer,
        peer: my_peer,
        state,
        resumed_ms,
        resumed_peer,
        first_state,
        gave_up,
        events,
        ends,
        refusals,
        ..
    } = m;
    let index = *index;
    let Role::Client(p) = &mut *role else {
        return;
    };
    // An armed server takes resumes and keeps its tick clock running, so the clients that
    // arrive sync to a running server; it simulates nothing until the hand-off commits.
    if let Some(a) = p.armed.as_mut()
        && now * 1_000 >= a.next_tick_us
    {
        a.next_tick_us += TICK_US;
        a.server.receive(now);
        while let Some(ev) = a.server.poll_event() {
            if let ServerEvent::PeerResumed { peer, .. } = ev {
                a.resumed.insert(peer.0, now);
                handoff.peer_resumed(now, peer);
            }
        }
        a.server.advance_tick(|_, _| {});
        a.server.flush(now);
    }
    // The resume attempt first: when the successor accepts, it becomes the connection.
    let mut carried = Vec::new();
    if let Some(attempt) = p.attempt.as_mut() {
        attempt.receive(now);
        let mut accepted = None;
        let mut failed = false;
        while let Some(ev) = attempt.poll_event() {
            match ev {
                ClientEvent::Connected { peer, .. } => accepted = Some(peer),
                ClientEvent::Ended(end) => {
                    ends.push((now, format!("resume attempt: {end:?}")));
                    failed = true;
                }
                ClientEvent::Message { message, .. } => carried.push(message),
                _ => {}
            }
        }
        if let Some(peer) = accepted
            && let Some(attempt) = p.attempt.take()
        {
            let old = std::mem::replace(&mut p.net, attempt);
            *spare = Some(old.into_socket());
            p.shadow = JournalSuccessor::new(World::default());
            p.ended = false;
            handoff.resumed(now, peer);
            handoff.set_peer(peer);
            *my_peer = Some(peer);
            *resumed_ms = Some(now);
            *resumed_peer = Some(peer);
        } else if failed {
            if let Some(attempt) = p.attempt.take() {
                *spare = Some(attempt.into_socket());
            }
            handoff.resume_failed(now);
        }
    }
    p.net.receive(now);
    let mut messages = carried;
    while let Some(ev) = p.net.poll_event() {
        match ev {
            ClientEvent::Connected { peer, .. } => {
                if handoff.migrating() {
                    // A returning host: its one connection is the resume.
                    handoff.resumed(now, peer);
                    *resumed_ms = Some(now);
                    *resumed_peer = Some(peer);
                } else {
                    first_peer.get_or_insert(peer);
                }
                handoff.set_peer(peer);
                *my_peer = Some(peer);
            }
            ClientEvent::Ended(end) => {
                ends.push((now, format!("{end:?}")));
                if old_host.is_some() && handoff.migrating() {
                    handoff.resume_failed(now);
                }
                p.ended = true;
            }
            ClientEvent::Message { message, .. } => messages.push(message),
            _ => {}
        }
    }
    let mut seen = Seen {
        refusals,
        state,
        first_state,
        resumed: resumed_ms.is_some(),
    };
    for message in &messages {
        on_message(now, message, &mut p.shadow, handoff, &mut seen);
    }
    handoff.set_appointed(p.shadow.is_successor());
    if p.net.is_connected() {
        if let Some(announce) = handoff.poll_out() {
            let _ = p.net.send(Channel::GAME_RPC, &announce);
        }
        for tick in p.net.due_input_ticks(now) {
            let _ = p.net.send_input(tick, direction(tick, index));
        }
        if let Some(ack) = p.shadow.poll_ack() {
            let _ = p.net.send(Channel::Unreliable, &ack);
        }
    }
    // A returning host holds no connection to a host of its own: its link is gone.
    let link = if old_host.is_some() && handoff.migrating() {
        HostLink::gone()
    } else {
        HostLink {
            last_heard_ms: p.net.last_heard_ms(),
            ended: p.ended,
        }
    };
    let action = handoff.poll(now, link, p.shadow.promotable());
    while let Some(e) = handoff.poll_event() {
        events.push((now, e));
    }
    match action {
        Some(HandoffAction::Arm(plan)) => {
            // The server opens on the announced port and holds every player; the shadow and
            // the connection to the host stay.
            let mut server =
                NetServer::<HandoffGame, _>::new(net.socket(eps[2]), plan.server_config(server_config()));
            plan.apply(&mut server).expect("every player held");
            let base_tick = p.shadow.applied_tick().expect("a promotable shadow has a tick");
            assert!(server.start_at_tick(base_tick));
            p.armed = Some(Box::new(Armed {
                server,
                plan: *plan,
                base_tick,
                next_tick_us: now * 1_000,
                resumed: BTreeMap::new(),
            }));
        }
        Some(HandoffAction::Disarm) => {
            // A goodbye for whoever resumed to the armed server, then it is gone.
            if let Some(mut a) = p.armed.take() {
                a.server.shutdown();
                a.server.flush(now);
            }
        }
        Some(HandoffAction::Commit) => {
            let Role::Client(p) = std::mem::replace(role, Role::Dead) else {
                unreachable!()
            };
            let Playing {
                shadow,
                attempt,
                armed,
                ..
            } = *p;
            if let Some(attempt) = attempt {
                *spare = Some(attempt.into_socket());
            }
            let Armed {
                server,
                plan,
                base_tick,
                next_tick_us,
                resumed,
            } = *armed.expect("an armed server");
            let promotion = shadow.promote().expect("a promotable shadow");
            assert_eq!(promotion.tick, base_tick, "the shadow did not move while armed");
            // The world goes on at the server's tick: the ticks the server counted while it was
            // armed are not simulated.
            let world = promotion.world;
            // The lost host's own player moves to the seat kept for it.
            let mut pending_events = vec![3];
            pending_events.extend_from_slice(&PeerId::SERVER.0.to_le_bytes());
            pending_events.extend_from_slice(&plan.host_seat.0.to_le_bytes());
            let promoted = Promoted {
                at_ms: now,
                rank: plan.rank,
                tick: promotion.tick,
                digest: world.world_digest(),
                players: world.players.keys().copied().collect(),
                changes: world.players.iter().map(|(k, p)| (*k, p.1)).collect(),
                host_seat: plan.host_seat,
                own_peer: plan.own_peer,
            };
            *role = Role::Host(Box::new(Hosting {
                server,
                world,
                journal: JournalHost::new(JournalConfig::default()),
                handoff: HandoffHost::new(HandoffConfig::default()).with_epoch(plan.epoch),
                own_key: plan.own_peer.0,
                pending_events,
                dice: SoakRng::derive(o.seed, 300 + u64::from(index)),
                next_tick_us,
                digests: BTreeMap::new(),
                promoted: Some(promoted),
                resumed,
                joined_after: Vec::new(),
                left: Vec::new(),
                refusals: Vec::new(),
            }));
            return;
        }
        Some(HandoffAction::Promote(_)) => panic!("every machine announces a port and arms first"),
        Some(HandoffAction::Resume(target)) => {
            let config = target.client_config(client_config(index));
            if old_host.is_some() {
                // A returning host has one connection: the attempt replaces it.
                p.net = connect(net.socket(eps[0]), target.addr, config, index);
                p.ended = false;
            } else {
                // The connection to the lost host is kept; the attempt runs on the second socket.
                let socket = match p.attempt.take() {
                    Some(earlier) => earlier.into_socket(),
                    None => spare.take().expect("a spare socket"),
                };
                p.attempt = Some(connect(socket, target.addr, config, index));
            }
        }
        Some(HandoffAction::StayWithHost) => {
            if let Some(attempt) = p.attempt.take() {
                *spare = Some(attempt.into_socket());
            }
        }
        Some(HandoffAction::ReturnToHost) => {
            if let Some(attempt) = p.attempt.take() {
                *spare = Some(attempt.into_socket());
            }
            match old_host {
                // A returning host starts the next round.
                Some(old) => {
                    if let Some(back) = old.comeback(now) {
                        *handoff = back;
                    }
                }
                None => *gave_up = true,
            }
        }
        None => {}
    }
    if let Some(attempt) = p.attempt.as_mut() {
        attempt.flush(now);
    }
    p.net.flush(now);
}

impl Outcome {
    /// The one machine that hosts at the end.
    fn host(&self) -> (&Machine, &Hosting) {
        let hosts: Vec<(&Machine, &Hosting)> = self
            .machines
            .iter()
            .filter_map(|m| match &m.role {
                Role::Host(h) => Some((m, h.as_ref())),
                _ => None,
            })
            .collect();
        assert_eq!(hosts.len(), 1, "exactly one machine hosts");
        hosts[0]
    }

    /// The checks every hand-off shares. `loss_ms` is when the host was lost, `bar_ms` how long
    /// the match may take to continue, `rank` the successor that must have taken over. Returns
    /// the measured numbers as a line for the log.
    fn assert_continues(&self, label: &str, loss_ms: u64, bar_ms: u64, rank: usize, dead: &[u32]) -> String {
        let lost = self.lost.as_ref().expect("the host was lost");
        let (hm, h) = self.host();
        let promoted = h.promoted.as_ref().expect("a promoted successor hosts");
        assert_eq!(usize::from(promoted.rank), rank, "{label}: {promoted:?}");
        if let Some(expected) = lost.successors.get(rank) {
            assert_eq!(
                hm.index, *expected,
                "{label}: the roster's successor {rank} took over"
            );
        }
        assert_eq!(
            Some(promoted.own_peer),
            hm.first_peer,
            "{label}: the new host keeps its PeerId"
        );
        // Every entity present: the promoted shadow is the lost host's world of that tick, and
        // no entity went missing since.
        assert_eq!(
            Some(&promoted.digest),
            lost.digests.get(&promoted.tick),
            "{label}: the promoted world differs from the lost host's at tick {}",
            promoted.tick
        );
        assert_eq!(
            promoted.players, lost.players,
            "{label}: players at the promotion"
        );
        let mut expect = lost.players.clone();
        assert!(expect.remove(&PeerId::SERVER.0));
        assert!(expect.insert(promoted.host_seat.0));
        let now_players: BTreeSet<u32> = h.world.players.keys().copied().collect();
        assert_eq!(now_players, expect, "{label}: players at the end");
        assert_eq!(expect.len(), PLAYERS as usize, "{label}");
        assert_eq!(h.world.cells.len(), 64, "{label}");
        assert!(
            h.joined_after.is_empty(),
            "{label}: joined as new peers: {:?}",
            h.joined_after
        );
        assert!(h.left.is_empty(), "{label}: left: {:?}", h.left);
        assert_eq!(h.server.totals().resumes_refused, 0, "{label}");
        let mut worst_resume = 0;
        let mut worst_state = 0;
        let mut clients = 0;
        for m in &self.machines {
            if m.index == 0 || m.index == hm.index || dead.contains(&m.index) {
                continue;
            }
            clients += 1;
            let who = format!("{label}: client {}", m.index);
            assert!(!m.gave_up, "{who} gave up; {:?}", m.events);
            let at = m
                .resumed_ms
                .unwrap_or_else(|| panic!("{who} never resumed; {:?}", m.events));
            let took = at - loss_ms;
            assert!(took <= bar_ms, "{who} resumed after {took} ms; {:?}", m.events);
            assert_eq!(m.resumed_peer, m.first_peer, "{who} keeps its PeerId");
            assert_eq!(m.peer, m.first_peer, "{who}");
            let (state_at, state_tick) = m
                .first_state
                .unwrap_or_else(|| panic!("{who} got no state from the new host"));
            let state_took = state_at - loss_ms;
            assert!(state_took <= bar_ms, "{who}: first state after {state_took} ms");
            assert!(state_tick > promoted.tick, "{who}: state of tick {state_tick}");
            // The match goes on: the new host applies this client's inputs and the client sees
            // its own entity change.
            let id = m.first_peer.expect("joined").0;
            let before = promoted.changes[&id];
            assert!(
                h.world.players[&id].1 > before,
                "{who}: no input applied by the new host (changes {before} at the promotion, {} now; host tick {}, promoted at tick {}; client state {:?}, resumed at {at} ms, promoted at {} ms)",
                h.world.players[&id].1,
                h.world.tick,
                promoted.tick,
                m.state,
                promoted.at_ms
            );
            assert!(m.state.is_some_and(|s| s.2 > before), "{who}: {:?}", m.state);
            assert!(m.refusals.is_empty(), "{who} refused {:?}", m.refusals);
            assert!(h.resumed.contains_key(&id), "{who}: the host saw no resume");
            worst_resume = worst_resume.max(took);
            worst_state = worst_state.max(state_took);
        }
        let events: Vec<&HandoffEvent> = hm.events.iter().map(|(_, e)| e).collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, HandoffEvent::AllResumed { .. }))
                || !dead.is_empty(),
            "{label}: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, HandoffEvent::Quorum { .. })),
            "{label}: {events:?}"
        );
        let journal = h.journal.status();
        format!(
            "{label}: successor rank {rank} (machine {}) promoted {} ms after the loss at tick {} ({} ticks behind the lost host's {}); {clients} clients resumed, the last after {worst_resume} ms, the last first state after {worst_state} ms; new host journal: {} with {} successors; new host refused {:?}",
            hm.index,
            promoted.at_ms - loss_ms,
            promoted.tick,
            lost.tick.saturating_sub(promoted.tick),
            lost.tick,
            journal.state,
            journal.successors.len(),
            h.refusals,
        )
    }
}

/// Acceptance: a 16-player listen match whose host stops continues on the first successor
/// within 5 s: every client is connected to it under the `PeerId` it had, has a state frame of
/// a tick after the promotion, and its inputs are applied again; the promoted world is the lost
/// host's world of the promoted tick (every entity present) and no entity is missing at the
/// end. On good-wifi and bad-wifi, three seeds each, and once with a host that says goodbye.
#[test]
fn a_sixteen_player_match_continues_on_the_first_successor_within_five_seconds() {
    for profile in ["good-wifi", "bad-wifi"] {
        for seed in 1..=3 {
            let o = Options::new(profile, seed);
            let out = run(&o);
            let label = format!("{profile} seed {seed}");
            let lost = out.lost.as_ref().expect("the host was killed");
            assert_eq!(lost.successors.len(), 2, "{label}: two successors in the roster");
            println!("{}", out.assert_continues(&label, lost.at_ms, 5_000, 0, &[]));
            let (_, h) = out.host();
            assert_eq!(h.resumed.len(), PLAYERS as usize - 2, "{label}");
            assert_eq!(h.server.totals().resumes, PLAYERS as u64 - 2, "{label}");
            for m in &out.machines {
                assert!(m.ends.is_empty(), "{label}: client {}: {:?}", m.index, m.ends);
            }
        }
    }
    let mut o = Options::new("good-wifi", 4);
    o.graceful = true;
    let out = run(&o);
    let lost = out.lost.as_ref().expect("the host quit");
    println!(
        "{}",
        out.assert_continues("good-wifi seed 4, host quits", lost.at_ms, 5_000, 0, &[])
    );
}

/// Acceptance: the host and the first successor stop together. The second successor takes over
/// 3 s after the loss is seen and the match continues on it within 8 s, same `PeerId`s, every
/// entity present (the dead successor's player too, held for its return).
#[test]
fn the_second_successor_takes_over_within_eight_seconds_when_the_first_is_dead_too() {
    for profile in ["good-wifi", "bad-wifi"] {
        for seed in 1..=3 {
            let mut o = Options::new(profile, seed);
            o.kill_first_successor = true;
            o.seconds = 36;
            let out = run(&o);
            let label = format!("{profile} seed {seed}, first successor dead");
            let lost = out.lost.as_ref().expect("the host was killed");
            assert_eq!(lost.successors.len(), 2, "{label}: two successors in the roster");
            let dead = [lost.successors[0]];
            println!("{}", out.assert_continues(&label, lost.at_ms, 8_000, 1, &dead));
            let (hm, h) = out.host();
            assert_eq!(h.resumed.len(), PLAYERS as usize - 3, "{label}");
            // Not before the first successor's 3 s were over.
            let promoted = h.promoted.as_ref().expect("promoted");
            assert!(promoted.at_ms - lost.at_ms >= 3_000, "{label}: {promoted:?}");
            assert!(
                hm.events
                    .iter()
                    .any(|(_, e)| matches!(e, HandoffEvent::SuccessorSilent { rank: 0 })),
                "{label}: {:?}",
                hm.events
            );
        }
    }
}

/// Extra evidence, not part of the default run (`--ignored`): the two acceptance rows over seeds
/// 11 to 30 on both profiles, with the same bars, and the slowest client of each row.
#[test]
#[ignore = "40 extra runs per row; run with --ignored --nocapture for the margins"]
fn hand_off_times_over_twenty_more_seeds() {
    for profile in ["good-wifi", "bad-wifi"] {
        for (dead_first, bar_ms, rank) in [(false, 5_000, 0), (true, 8_000, 1)] {
            let mut slowest = (0, 0);
            let mut behind = 0;
            for seed in 11..=30 {
                let mut o = Options::new(profile, seed);
                o.kill_first_successor = dead_first;
                o.seconds = 36;
                let out = run(&o);
                let label = format!("{profile} seed {seed}, first successor dead: {dead_first}");
                let lost = out.lost.as_ref().expect("the host was killed");
                assert_eq!(lost.successors.len(), 2, "{label}: two successors in the roster");
                let dead: Vec<u32> = lost.successors[..usize::from(dead_first)].to_vec();
                out.assert_continues(&label, lost.at_ms, bar_ms, rank, &dead);
                let (hm, h) = out.host();
                let promoted = h.promoted.as_ref().expect("promoted");
                behind = behind.max(lost.tick.saturating_sub(promoted.tick));
                for m in &out.machines {
                    if m.index != 0
                        && m.index != hm.index
                        && let Some(at) = m.resumed_ms
                    {
                        slowest = slowest.max((at - lost.at_ms, seed));
                    }
                }
            }
            println!(
                "{profile}, first successor dead: {dead_first}: 20 seeds, slowest client {} ms (seed {}), bar {bar_ms} ms; shadow at most {behind} ticks behind",
                slowest.0, slowest.1
            );
        }
    }
}

/// A host that was only cut off (its link is out for 8 s, the process lives): its clients move
/// to the first successor as above; the host sees every client silent, stops hosting and goes
/// back as a client. When its link returns it is accepted on the seat the successor kept for its
/// player, and its inputs move that player again.
#[test]
fn a_host_that_was_only_cut_off_comes_back_as_a_client_on_the_seat_kept_for_it() {
    for seed in [1, 2] {
        let mut o = Options::new("good-wifi", seed);
        o.kill_host_ms = None;
        o.host_outage = Some((20_000, 28_000));
        o.seconds = 40;
        let out = run(&o);
        let label = format!("good-wifi seed {seed}, host cut off 8 s");
        println!("{}", out.assert_continues(&label, 20_000, 5_000, 0, &[]));
        let deposed = out.deposed_ms.expect("the host saw itself replaced");
        assert!(
            (21_400..=22_500).contains(&deposed),
            "{label}: deposed at {deposed}"
        );
        let (_, h) = out.host();
        let promoted = h.promoted.as_ref().expect("promoted");
        let old = &out.machines[0];
        assert!(
            matches!(old.role, Role::Client(_)),
            "{label}: the old host is a client now"
        );
        assert_eq!(
            old.resumed_peer,
            Some(promoted.host_seat),
            "{label}: {:?}",
            old.events
        );
        let back = old.resumed_ms.expect("the old host came back");
        assert!(
            (28_000..=33_000).contains(&back),
            "{label}: back at {back} ms (the link returned at 28,000)"
        );
        // Its player was `PeerId::SERVER`'s; it is the seat's now and moves by its inputs again.
        let seat = promoted.host_seat.0;
        let before = promoted.changes[&PeerId::SERVER.0];
        assert!(
            h.world.players[&seat].1 > before,
            "{label}: the old host's inputs are applied"
        );
        assert!(
            old.state.is_some_and(|s| s.2 > before && s.3 == PLAYERS as u16),
            "{label}: {:?}",
            old.state
        );
        assert_eq!(
            h.resumed.len(),
            PLAYERS as usize - 1,
            "{label}: 14 clients and the old host"
        );
        println!("{label}: deposed at {deposed} ms, back as peer {seat} at {back} ms");
    }
}

/// The machines the host's roster names as successors after 20 s on this profile and seed,
/// first successor first. A run with an outage that starts at 20 s is the same run until then.
fn successors_at_20_s(profile: &'static str, seed: u64) -> Vec<u32> {
    let mut o = Options::new(profile, seed);
    o.kill_host_ms = None;
    o.seconds = 20;
    let out = run(&o);
    let (_, h) = out.host();
    h.handoff
        .ready_successors()
        .iter()
        .map(|peer| {
            out.machines
                .iter()
                .find(|m| m.first_peer == Some(*peer))
                .expect("the successor's machine")
                .index
        })
        .collect()
}

/// What an outage of one client's own link must leave untouched: the first host still hosts and
/// nobody was promoted, nobody left, every client is the same peer on the connection it had,
/// and only the client in the outage saw anything. Returns that client's events.
fn assert_one_match(out: &Outcome, victim: u32) -> Vec<&HandoffEvent> {
    let (hm, h) = out.host();
    assert_eq!(hm.index, 0, "the first host still hosts");
    assert!(h.promoted.is_none());
    assert!(h.left.is_empty(), "{:?}", h.left);
    assert_eq!(h.world.players.len(), PLAYERS as usize);
    assert_eq!(h.handoff.ready_successors().len(), 2);
    for m in &out.machines[1..] {
        let Role::Client(p) = &m.role else {
            panic!("client {} is still a client", m.index);
        };
        assert!(p.armed.is_none() && p.attempt.is_none(), "client {}", m.index);
        assert!(p.net.is_connected(), "client {}", m.index);
        assert!(
            m.resumed_ms.is_none() && !m.gave_up,
            "client {}: {:?}",
            m.index,
            m.events
        );
        assert_eq!(m.peer, m.first_peer);
        assert!(m.ends.is_empty(), "client {}: {:?}", m.index, m.ends);
        if m.index != victim {
            assert!(m.events.is_empty(), "client {}: {:?}", m.index, m.events);
        }
    }
    let v = &out.machines[victim as usize];
    // Its own entity is still the host's and still moves.
    let id = v.first_peer.expect("joined").0;
    let (tick, _, changes, players) = v.state.expect("state frames");
    assert!(
        h.world.tick - tick < 30,
        "state of tick {tick}, host at {}",
        h.world.tick
    );
    assert_eq!(players, PLAYERS as u16);
    assert!(h.world.players[&id].1 - changes <= 1);
    v.events.iter().map(|(_, e)| e).collect()
}

/// A client in its own outage (2.5 s, longer than the 1.5 s after which a host counts as lost)
/// while the host is fine: it starts a resume to the first successor, which never armed, hears
/// the host again when its link returns and stays. Nobody is promoted, nobody leaves, there is
/// one match, and the client is the same peer on the same connection.
#[test]
fn a_client_in_its_own_outage_stays_with_its_host_and_nobody_is_promoted() {
    let successors = successors_at_20_s("good-wifi", 5);
    let victim = (1..PLAYERS)
        .find(|i| !successors.contains(i))
        .expect("a client that is not a successor");
    let mut o = Options::new("good-wifi", 5);
    o.kill_host_ms = None;
    o.client_outage = Some((victim, 20_000, 22_500));
    o.seconds = 30;
    let out = run(&o);
    let kinds = assert_one_match(&out, victim);
    assert!(
        matches!(kinds.first(), Some(HandoffEvent::HostLost { .. })),
        "{kinds:?}"
    );
    assert!(
        matches!(kinds.get(1), Some(HandoffEvent::Resuming { rank: 0, .. })),
        "{kinds:?}"
    );
    assert!(
        matches!(kinds.last(), Some(HandoffEvent::HostRestored { .. })),
        "{kinds:?}"
    );
    let v = &out.machines[victim as usize];
    let restored = v.events.last().expect("events").0;
    assert!((22_500..=23_000).contains(&restored), "restored at {restored} ms");
    println!(
        "good-wifi seed 5, client {victim} (not a successor) out 2.5 s: host lost seen at {} ms, restored at {restored} ms, events {kinds:?}",
        v.events.first().expect("events").0
    );
}

/// The first successor in its own outage (2.5 s) while the host is fine: it arms (opens its
/// server and waits), no player comes because nobody else lost the host, and when its link
/// returns it hears the host and disarms. It never took its shadow out of the journal or gave
/// its connection up: nobody is promoted, nobody leaves, there is one match.
#[test]
fn a_successor_in_its_own_outage_arms_and_disarms_and_nobody_is_promoted() {
    let successors = successors_at_20_s("good-wifi", 5);
    let victim = successors[0];
    let mut o = Options::new("good-wifi", 5);
    o.kill_host_ms = None;
    o.client_outage = Some((victim, 20_000, 22_500));
    o.seconds = 30;
    let out = run(&o);
    let kinds = assert_one_match(&out, victim);
    assert!(
        matches!(kinds.first(), Some(HandoffEvent::HostLost { .. })),
        "{kinds:?}"
    );
    assert!(
        kinds.iter().any(|e| matches!(e, HandoffEvent::Armed { .. })),
        "{kinds:?}"
    );
    assert!(
        !kinds.iter().any(|e| matches!(e, HandoffEvent::Promoted { .. })),
        "{kinds:?}"
    );
    assert!(
        matches!(kinds.last(), Some(HandoffEvent::HostRestored { .. })),
        "{kinds:?}"
    );
    let v = &out.machines[victim as usize];
    let restored = v.events.last().expect("events").0;
    assert!((22_500..=23_000).contains(&restored), "restored at {restored} ms");
    println!(
        "good-wifi seed 5, first successor (client {victim}) out 2.5 s: host lost seen at {} ms, restored at {restored} ms, events {kinds:?}",
        v.events.first().expect("events").0
    );
}
