// SPDX-License-Identifier: MPL-2.0
//! `LoopbackHub`: N processes in one test (spec 70 § 10). Each process is a `World` with its own
//! gameplay schedule; the hub runs one fixed tick on every process per `LoopbackHub::step` and
//! moves frames between them, with optional loss, delay in ticks, duplication and reordering under
//! a seeded RNG. No sockets, no wall clock.
//!
//! Channel model: unreliable frames may be lost, duplicated or reordered; reliable frames are never
//! lost (a lost attempt is resent `resend_ticks` later) and reliable-ordered frames keep their order
//! per link, like the transport's reliable stream.

#[cfg(feature = "ecs")]
use std::collections::HashMap;

#[cfg(feature = "ecs")]
use regia_ecs::prelude::*;

#[cfg(feature = "ecs")]
use crate::message::Channel;
#[cfg(feature = "ecs")]
use crate::model::{LocalPeer, NetMode, NetTick, PeerId};
#[cfg(feature = "ecs")]
use crate::replication::{ConnWire, ServerReplication};
#[cfg(feature = "ecs")]
use crate::runtime::{
    Datagram, NetNode, ensure_resources, net_apply, net_receive, net_send, server_add_peer,
};
#[cfg(feature = "ecs")]
use crate::settings::{NetSettings, StateChannel};

/// Network conditions of one direction of a link.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkConditions {
    /// Probability that one send attempt is lost (unreliable: dropped; reliable: resent).
    pub loss: f64,
    /// Fixed delay in ticks.
    pub delay_ticks: u64,
    /// Probability that an unreliable frame arrives twice.
    pub duplicate: f64,
    /// Probability that an unreliable frame gets 1..=3 extra ticks of delay (reordering).
    pub reorder: f64,
    /// Ticks until a lost reliable attempt is resent.
    pub resend_ticks: u64,
    /// Extra delay of reliable frames only, in ticks (holds state back behind unreliable messages).
    pub reliable_extra_ticks: u64,
}

impl Default for LinkConditions {
    fn default() -> Self {
        Self {
            loss: 0.0,
            delay_ticks: 0,
            duplicate: 0.0,
            reorder: 0.0,
            resend_ticks: 3,
            reliable_extra_ticks: 0,
        }
    }
}

/// What a frame filter does with a frame (reliable frames are never dropped, like the transport).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFate {
    /// Normal delivery under the link conditions.
    Deliver,
    /// Delivered this many ticks later than normal.
    Delay(u64),
    /// Lost.
    Drop,
}

#[cfg(feature = "ecs")]
/// A deterministic per-frame decision: `(frame, hub step)`. Tests of loss and reorder name the exact
/// frame instead of finding a seed that happens to hit it.
pub type FrameFilter = Box<dyn FnMut(&Datagram, u64) -> FrameFate + Send>;

/// A small seeded generator (SplitMix64): the same seed gives the same conditions.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator from a seed.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A uniform value in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(feature = "ecs")]
/// One process in the hub.
pub struct NetProcess {
    /// Its world.
    pub world: World,
    /// Its gameplay schedule (runs between `NetSet::Apply` and `NetSet::Send`).
    pub gameplay: Schedule,
    /// Its peer id (`PeerId::SERVER` for the server).
    pub peer: PeerId,
    /// Whether it is still connected (clients).
    pub connected: bool,
}

#[cfg(feature = "ecs")]
impl std::fmt::Debug for NetProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetProcess")
            .field("peer", &self.peer)
            .field("connected", &self.connected)
            .finish()
    }
}

#[cfg(feature = "ecs")]
impl NetProcess {
    /// Runs one fixed tick: receive, apply, gameplay, send.
    pub fn tick(&mut self) {
        net_receive(&mut self.world);
        net_apply(&mut self.world);
        self.gameplay.run(&mut self.world);
        crate::predict::net_predict(&mut self.world);
        net_send(&mut self.world);
    }
}

#[cfg(feature = "ecs")]
#[derive(Debug, Clone)]
struct InFlight {
    at: u64,
    order: u64,
    to: PeerId,
    to_server: bool,
    datagram: Datagram,
}

#[cfg(feature = "ecs")]
/// Every frame a client received, in order, with the hub step: feed it to a fresh client with
/// [`replay_client`] to reproduce what it applied.
pub type PacketLog = Vec<(u64, Datagram)>;

#[cfg(feature = "ecs")]
/// N processes as N worlds, moved between at fixed steps.
pub struct LoopbackHub {
    /// The server (offline, dedicated or listen).
    pub server: NetProcess,
    /// The clients, in join order.
    pub clients: Vec<NetProcess>,
    /// Conditions server → client.
    pub down: LinkConditions,
    /// Conditions client → server.
    pub up: LinkConditions,
    /// Decides the fate of frames server → client, before the conditions.
    pub down_filter: Option<FrameFilter>,
    /// Decides the fate of frames client → server, before the conditions.
    pub up_filter: Option<FrameFilter>,
    /// Rewrites frames client → server (a client of another protocol).
    pub up_rewrite: Option<fn(&mut Datagram)>,
    /// Rewrites frames server → client (corrupt records).
    pub down_rewrite: Option<fn(&mut Datagram)>,
    /// New connections negotiate their wire even when `REGIA_NET_TEST_STATE_CHANNEL` skips that.
    pub negotiate: bool,
    rng: SplitMix64,
    step: u64,
    order: u64,
    in_flight: Vec<InFlight>,
    last_reliable: HashMap<(bool, PeerId), u64>,
    setup: fn(&mut World),
    /// Frames each client received (by client index), when recording.
    pub client_logs: Vec<PacketLog>,
    /// Test hook: client frames for which this returns true are lost on the way up (called with
    /// the hub step, the sending peer and the frame).
    pub up_drop: Option<UpDropFn>,
    /// Test hook: how many frames (fixed steps) client `index` runs in hub step `step` (called
    /// with the step and the index; `None` = one). A client whose clock runs fast runs two now and
    /// then, a slow one none (the clock-drift profile, spec 120 § 15.1).
    pub client_frames: Option<ClientFramesFn>,
}

#[cfg(feature = "ecs")]
/// Assembles every connection on the calling thread while on (the parallel-equals-serial test of
/// spec 72 § 13). Off by default; the bytes are the same either way, which is what that test shows.
pub fn set_serial_gather(on: bool) {
    crate::dense::set_serial(on);
}

#[cfg(feature = "ecs")]
/// See [`LoopbackHub::up_drop`].
pub type UpDropFn = Box<dyn FnMut(u64, PeerId, &Datagram) -> bool + Send>;

#[cfg(feature = "ecs")]
/// See [`LoopbackHub::client_frames`].
pub type ClientFramesFn = Box<dyn FnMut(u64, usize) -> u32 + Send>;

#[cfg(feature = "ecs")]
impl std::fmt::Debug for LoopbackHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackHub")
            .field("step", &self.step)
            .field("clients", &self.clients.len())
            .finish()
    }
}

#[cfg(feature = "ecs")]
/// Whether `REGIA_NET_TEST_STATE_CHANNEL=unreliable` runs the hub's connections on the state channel.
pub fn every_test_on_state_channel() -> bool {
    std::env::var("REGIA_NET_TEST_STATE_CHANNEL").as_deref() == Ok("unreliable")
}

#[cfg(feature = "ecs")]
fn new_world(mode: NetMode, peer: Option<PeerId>, setup: fn(&mut World)) -> World {
    let mut world = World::new();
    world.insert_resource(mode);
    world.insert_resource(LocalPeer(peer));
    // One frame per step: hub tests count in ticks. A test of the send cadence sets its own.
    // Tests of the state channel set theirs too; `REGIA_NET_TEST_STATE_CHANNEL=unreliable` runs
    // every hub test on it.
    let mut settings = NetSettings::default();
    settings.replication.send_every_ticks = 1;
    if !every_test_on_state_channel() {
        settings.replication.state_channel = StateChannel::Reliable;
    }
    world.insert_resource(settings);
    setup(&mut world);
    ensure_resources(&mut world);
    world
}

#[cfg(feature = "ecs")]
impl LoopbackHub {
    /// A hub with a server in `mode`. `setup` runs on every process (register messages and
    /// components there, in the same order everywhere).
    pub fn new(mode: NetMode, seed: u64, setup: fn(&mut World)) -> Self {
        Self {
            server: NetProcess {
                world: new_world(mode, Some(PeerId::SERVER), setup),
                gameplay: Schedule::default(),
                peer: PeerId::SERVER,
                connected: true,
            },
            clients: Vec::new(),
            down: LinkConditions::default(),
            up: LinkConditions::default(),
            down_filter: None,
            up_filter: None,
            up_rewrite: None,
            down_rewrite: None,
            negotiate: false,
            rng: SplitMix64::new(seed),
            step: 0,
            order: 0,
            in_flight: Vec::new(),
            last_reliable: HashMap::new(),
            setup,
            client_logs: Vec::new(),
            up_drop: None,
            client_frames: None,
        }
    }

    /// Joins a client; returns its index. Peers get ids 1, 2, ... in join order.
    pub fn add_client(&mut self) -> usize {
        let peer = PeerId(self.clients.len() as u32 + 1);
        self.clients.push(NetProcess {
            world: new_world(NetMode::Client, Some(peer), self.setup),
            gameplay: Schedule::default(),
            peer,
            connected: true,
        });
        self.client_logs.push(Vec::new());
        server_add_peer(&mut self.server.world, peer);
        // Tests count ticks from the join: the negotiating round trip is left out of them here
        // (`tests/state_channel.rs` tests it).
        if every_test_on_state_channel() && !self.negotiate {
            self.server
                .world
                .resource_mut::<ServerReplication>()
                .set_wire(peer, ConnWire::Acked);
        }
        self.clients.len() - 1
    }

    /// The hub's step count.
    pub fn step_count(&self) -> u64 {
        self.step
    }

    /// The server's current tick.
    pub fn server_tick(&self) -> NetTick {
        self.server.world.resource::<NetNode>().tick
    }

    /// One step: deliver due frames, run one tick on the server and on every client, then move
    /// their outboxes into flight.
    pub fn step(&mut self) {
        self.step += 1;
        let now = self.step;
        let mut due: Vec<InFlight> = Vec::new();
        self.in_flight.retain(|f| {
            if f.at <= now {
                due.push(f.clone());
                false
            } else {
                true
            }
        });
        due.sort_by_key(|f| (f.at, f.order));
        for f in due {
            if f.to_server {
                if let Some(mut node) = self.server.world.get_resource_mut::<NetNode>() {
                    node.inbox.push(f.datagram);
                }
            } else if let Some(i) = self.clients.iter().position(|c| c.peer == f.to)
                && self.clients[i].connected
            {
                self.client_logs[i].push((now, f.datagram.clone()));
                if let Some(mut node) = self.clients[i].world.get_resource_mut::<NetNode>() {
                    node.inbox.push(f.datagram);
                }
            }
        }
        self.server.tick();
        // Disconnects decided by the server.
        let disconnects = std::mem::take(&mut self.server.world.resource_mut::<NetNode>().disconnects);
        for d in disconnects {
            if let Some(c) = self.clients.iter_mut().find(|c| c.peer == d.peer) {
                c.connected = false;
                c.world.resource_mut::<NetNode>().closed = Some(d.reason);
            }
            self.in_flight
                .retain(|f| f.to != d.peer && !(f.to_server && f.datagram.peer == d.peer));
        }
        let out = std::mem::take(&mut self.server.world.resource_mut::<NetNode>().outbox);
        for d in out {
            let to = d.peer;
            self.send(false, to, d);
        }
        let mut sent: Vec<(PeerId, Vec<Datagram>)> = Vec::new();
        let step = self.step;
        for (i, c) in self.clients.iter_mut().enumerate().filter(|(_, c)| c.connected) {
            let frames = self.client_frames.as_mut().map_or(1, |f| f(step, i));
            for _ in 0..frames {
                c.tick();
            }
            let out = std::mem::take(&mut c.world.resource_mut::<NetNode>().outbox);
            sent.push((c.peer, out));
        }
        for (peer, out) in sent {
            for mut d in out {
                d.peer = peer;
                if let Some(drop) = self.up_drop.as_mut()
                    && drop(self.step, peer, &d)
                {
                    continue;
                }
                self.send(true, peer, d);
            }
        }
    }

    /// Runs `n` steps.
    pub fn run(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    fn send(&mut self, to_server: bool, peer: PeerId, mut d: Datagram) {
        let c = if to_server { self.up } else { self.down };
        if let Some(rewrite) = if to_server {
            self.up_rewrite
        } else {
            self.down_rewrite
        } {
            rewrite(&mut d);
        }
        let step = self.step;
        let filter = if to_server {
            &mut self.up_filter
        } else {
            &mut self.down_filter
        };
        let fate = filter.as_mut().map_or(FrameFate::Deliver, |f| f(&d, step));
        let held = match fate {
            FrameFate::Drop if !d.channel.is_reliable() => return,
            FrameFate::Delay(ticks) => ticks,
            _ => 0,
        };
        let base = self.step + 1 + c.delay_ticks + held;
        if d.channel.is_reliable() {
            let mut at = base + c.reliable_extra_ticks;
            // Lost attempts are resent; at most 64 attempts are modeled.
            for _ in 0..64 {
                if self.rng.next_f64() >= c.loss {
                    break;
                }
                at += c.resend_ticks.max(1);
            }
            if d.channel == Channel::ReliableOrdered {
                let key = (to_server, peer);
                let last = self.last_reliable.get(&key).copied().unwrap_or(0);
                at = at.max(last);
                self.last_reliable.insert(key, at);
            }
            self.push(at, to_server, peer, d);
            return;
        }
        if self.rng.next_f64() < c.loss {
            return;
        }
        let mut at = base;
        if self.rng.next_f64() < c.reorder {
            at += 1 + self.rng.next_u64() % 3;
        }
        if self.rng.next_f64() < c.duplicate {
            self.push(at + 1, to_server, peer, d.clone());
        }
        self.push(at, to_server, peer, d);
    }

    fn push(&mut self, at: u64, to_server: bool, to: PeerId, datagram: Datagram) {
        self.order += 1;
        self.in_flight.push(InFlight {
            at,
            order: self.order,
            to,
            to_server,
            datagram,
        });
    }
}

#[cfg(feature = "ecs")]
/// Feeds a recorded packet log to a fresh client (set up with `setup`) at the recorded steps and
/// returns the messages it delivered, in order (spec 70 § 10 test 13).
pub fn replay_client(
    setup: fn(&mut World),
    peer: PeerId,
    log: &PacketLog,
    steps: u64,
) -> Vec<(NetTick, u16, PeerId, Option<crate::model::NetId>)> {
    let mut p = NetProcess {
        world: new_world(NetMode::Client, Some(peer), setup),
        gameplay: Schedule::default(),
        peer,
        connected: true,
    };
    p.world.resource_mut::<NetNode>().record = true;
    let mut i = 0;
    for step in 1..=steps {
        while let Some((at, d)) = log.get(i) {
            if *at != step {
                break;
            }
            p.world.resource_mut::<NetNode>().inbox.push(d.clone());
            i += 1;
        }
        p.tick();
        p.world.resource_mut::<NetNode>().outbox.clear();
    }
    std::mem::take(&mut p.world.resource_mut::<NetNode>().delivered_log)
}

#[cfg(feature = "ecs")]
/// Deterministic fuzzing of the client's state-frame apply path (`STATE` frames on the state
/// channel, spec 72 § 4, ADR-8761): frames recorded from a loopback session are mutated with the
/// seed-stable mutators of `regia_core::fuzz` and applied, after the recorded frames before them,
/// to a scratch client world with a small replication registry. A state frame has no checksum of
/// its own (the transport below seals it), so a mutation reaches the decoder as is. Used by
/// `tests/state_frame_fuzz.rs` and by the `net.state_frame_apply` target of `regia_agent`'s fuzz
/// registry; not a stable API.
#[doc(hidden)]
pub mod state_frame_fuzz {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::OnceLock;

    use glam::Vec3;
    use regia_core::fuzz::{FuzzRng, mutate_bytes};
    use regia_ecs::prelude::*;
    use regia_scene::transform::Transform;

    use super::LoopbackHub;
    use crate::model::{NetMode, PeerId};
    use crate::replication::{ClientReplication, NetVec, Replicated, register_netvec, register_replicated};
    use crate::runtime::TAG_STATE;
    use crate::settings::{NetSettings, StateChannel};
    use crate::{Audience, ReplicateOptions};

    /// A plain value component.
    #[derive(Component, Clone, Debug, PartialEq, Default, crate::Replicate)]
    pub struct FuzzHp {
        /// A value.
        pub hp: u32,
        /// A second field, so deltas carry change bits.
        pub armor: u16,
    }

    /// A quantized component (the clamp path).
    #[derive(Component, Clone, Debug, PartialEq, Default, crate::Replicate)]
    pub struct FuzzHeat {
        /// Quantized to 8 bits over 0..100.
        #[replicate(quantize(min = 0.0, max = 100.0, bits = 8))]
        pub temp: f32,
    }

    /// An entity reference (the pending-reference path).
    #[derive(Component, Clone, Debug, PartialEq, crate::Replicate)]
    pub struct FuzzLink {
        /// Another replicated entity.
        pub target: Entity,
    }

    /// A `NetVec` element.
    #[derive(Clone, Debug, PartialEq, crate::Replicate)]
    pub struct FuzzItem {
        /// A count.
        pub count: u32,
    }

    /// The registry: the same on the recording server and on every scratch client.
    pub fn setup(world: &mut World) {
        register_replicated::<FuzzHp>(world, ReplicateOptions::default());
        register_replicated::<FuzzHeat>(world, ReplicateOptions::default());
        register_replicated::<FuzzLink>(world, ReplicateOptions::default());
        register_netvec::<FuzzItem>(world, Audience::All);
        world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
    }

    fn items(n: u32) -> NetVec<FuzzItem> {
        let mut v = NetVec::default();
        for i in 0..n {
            let _ = v.push(FuzzItem { count: i * 3 + 1 });
        }
        v
    }

    /// Records the `STATE` frames a client receives in a fixed loopback session: spawns with
    /// values, references, `NetVec`s, parents and transforms, then deltas, a teleport, a
    /// component removal, a despawn and a late spawn. Deterministic: no wall clock, a fixed seed.
    pub fn record() -> Vec<Vec<u8>> {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 0x57a7e, setup);
        let client = hub.add_client();
        hub.server
            .world
            .resource_mut::<crate::replication::ServerReplication>()
            .set_wire(PeerId(1), crate::replication::ConnWire::Acked);
        let w = &mut hub.server.world;
        let a = w
            .spawn((
                Replicated::default(),
                Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
                FuzzHp { hp: 7, armor: 2 },
                FuzzHeat { temp: 40.0 },
                items(3),
            ))
            .id();
        let b = w
            .spawn((
                Replicated::default(),
                Transform::from_translation(Vec3::new(-4.0, 0.5, 9.0)),
                FuzzHp { hp: 70_000, armor: 9 },
                FuzzLink { target: a },
            ))
            .id();
        let c = w
            .spawn((
                Replicated::default(),
                Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
                FuzzHeat { temp: 250.0 },
                ChildOf(a),
            ))
            .id();
        hub.run(6);
        for step in 0..24u32 {
            let w = &mut hub.server.world;
            if let Some(mut hp) = w.get_mut::<FuzzHp>(a) {
                hp.hp += 1;
            }
            if let Some(mut t) = w.get_mut::<Transform>(b) {
                t.translation.x += 0.25;
            }
            if let Some(mut heat) = w.get_mut::<FuzzHeat>(c) {
                heat.temp = (step * 7 % 100) as f32;
            }
            if step == 4
                && let Some(mut v) = w.get_mut::<NetVec<FuzzItem>>(a)
            {
                let _ = v.push(FuzzItem { count: 99 });
            }
            if step == 8
                && let Some(mut t) = w.get_mut::<Transform>(a)
            {
                t.translation = Vec3::new(500.0, -20.0, 64.0);
            }
            if step == 10
                && let Ok(mut e) = w.get_entity_mut(b)
            {
                e.remove::<FuzzLink>();
            }
            if step == 14 {
                let _ = w.despawn(c);
            }
            if step == 16 {
                w.spawn((
                    Replicated::default(),
                    FuzzHp { hp: 1, armor: 1 },
                    FuzzLink { target: b },
                    items(2),
                ));
            }
            hub.run(1);
        }
        let mut frames: Vec<Vec<u8>> = hub.client_logs[client]
            .iter()
            .filter(|(_, d)| d.bytes.first() == Some(&TAG_STATE))
            .map(|(_, d)| d.bytes.clone())
            .collect();
        frames.extend(far_seeds(&hub, client, b, &frames));
        frames
    }

    /// The hand-built frames at the end of [`record`]'s frames (state record kind 3, spec 120
    /// § 8.3, ADR-10124): an update that puts an entity on the position grid, then two far
    /// updates of it. The session has no profiles (positions travel raw) and no rate tiers, so it
    /// records neither.
    pub const FAR_SEEDS: usize = 3;

    /// Three frames with one record of `e` each, after the recorded ones: an exact update with an
    /// absolute grid position (the baseline a far record needs), the absolute far form against
    /// it, then the residual far form against that far version. Empty when the session did not
    /// leave what they need.
    fn far_seeds(hub: &LoopbackHub, client: usize, e: Entity, frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
        use crate::bits::BitWriter;
        let seeds = || {
            let id = *hub.server.world.get::<crate::model::NetId>(e)?;
            let base = hub.clients[client]
                .world
                .resource::<ClientReplication>()
                .state
                .newest_version_tick(id)?;
            let (tick, seq, _) = split_records(frames.last()?)?;
            let frame = |tick: u64, seq: u64, kind: u64, base_ago: u64, fields: &dyn Fn(&mut BitWriter)| {
                let mut body = BitWriter::new();
                body.write_bits(kind, 2);
                body.write_bool(false); // version_ago 0: the frame's tick
                // base_ago, the 1..=32 code of the state channel's tick distances.
                body.write_bool(true);
                body.write_bool(false);
                body.write_bits(base_ago - 1, 5);
                fields(&mut body);
                body.write_bool(false); // no component entries
                let mut w = BitWriter::new();
                w.write_bits(u64::from(TAG_STATE), 8);
                w.write_varint(tick);
                w.write_varint(seq);
                w.write_varint(1);
                w.write_varint(u64::from(id.0));
                w.write_varint(body.bit_len() as u64);
                w.append(&body);
                w.finish()
            };
            let first = tick + 2;
            let base_ago = first
                .checked_sub(u64::from(base))
                .filter(|a| (1..=32).contains(a))?;
            Some(vec![
                // kind 2: an update with an absolute position-only record on the 1 mm grid.
                frame(first, seq + 1, 2, base_ago, &|w| {
                    w.write_bool(false); // parent unchanged
                    w.write_bool(true); // transform
                    w.write_bool(false); // no teleport
                    w.write_bool(false); // not steady
                    w.write_bool(false); // absolute, not a delta
                    w.write_bits(1, 2); // mode: position only
                    w.write_bool(true); // grid
                    w.write_bool(true); // the default step
                    for offset in [2_000u64, 500, 9_000] {
                        w.write_zigzag(0); // cell 0
                        w.write_bits(offset, 20);
                    }
                }),
                // kind 3: the absolute far form, x u15 | z u15 | y u10.
                frame(first + 2, seq + 2, 3, 2, &|w| {
                    w.write_bool(true);
                    w.write_bits(100, 15);
                    w.write_bits(300, 15);
                    w.write_bits(2, 10);
                }),
                // kind 3: residuals against the far version before it.
                frame(first + 4, seq + 3, 3, 2, &|w| {
                    w.write_bool(false);
                    w.write_bool(true); // x changed: class 0, zigzag(+3) - 1
                    w.write_bits(0, 2);
                    w.write_bits(5, 4);
                    w.write_bool(false); // z
                    w.write_bool(false); // y
                }),
            ])
        };
        let out = seeds().unwrap_or_default();
        debug_assert_eq!(out.len(), FAR_SEEDS, "the recorded session carries the far seeds");
        out
    }

    /// The recorded frames, recorded once per process.
    pub fn recorded() -> &'static [Vec<u8>] {
        static FRAMES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
        FRAMES.get_or_init(record)
    }

    /// A fresh client world with [`setup`]'s registry. Its cap is the smallest the recording
    /// fits — four ids get spawn records (three live ones plus the copy whose despawn notice
    /// travels on the reliable tick frame, never in a `STATE` frame, so its ring stays) — so
    /// the cap refusal path of ADR-8760 and the "maps <= cap" bound of [`apply_after_in`] sit
    /// at the boundary instead of being vacuous at the default 65536 (ADR-8850; rev-claude-55
    /// finding 6).
    pub fn scratch_world() -> World {
        let mut world = super::new_world(NetMode::Client, Some(PeerId(1)), setup);
        world.resource_mut::<ClientReplication>().state.entity_cap = Some(4);
        world
    }

    /// [`scratch_world`] with the interpolation clock installed, so copies are interpolated proxies
    /// (the step buffers, `hide_until_shown` and the clock's frame hook run).
    pub fn scratch_world_interpolated() -> World {
        let mut world = scratch_world();
        crate::interp::install_interpolation(&mut world, 60.0, 1.0);
        world
    }

    /// Applies one state frame as the client runtime does (`Err` closes a live session).
    ///
    /// # Errors
    /// The frame's framing is broken, or it spawns past the client's entity cap.
    pub fn apply(world: &mut World, bytes: &[u8]) -> Result<(), crate::bits::DecodeError> {
        crate::replication::client_apply_state_frame(world, bytes)
    }

    /// Applies `prefix` then `frame` to a fresh scratch world, then `after` while no frame failed,
    /// and checks the bounds a hostile frame must not break (ADR-8760). `Ok(true)` when `frame`
    /// applied, `Ok(false)` when it was refused.
    ///
    /// # Errors
    /// A broken invariant, as text.
    pub fn apply_after(prefix: &[Vec<u8>], frame: &[u8], after: &[Vec<u8>]) -> Result<bool, String> {
        apply_after_counted(prefix, frame, after).map(|(applied, _)| applied)
    }

    /// [`apply_after`], with the client's state counters after the mutated frame and the frames
    /// after it (the depth a run reached).
    ///
    /// # Errors
    /// A broken invariant, as text.
    pub fn apply_after_counted(
        prefix: &[Vec<u8>],
        frame: &[u8],
        after: &[Vec<u8>],
    ) -> Result<(bool, crate::replication::ClientStateCounters), String> {
        apply_after_in(scratch_world(), prefix, frame, after)
    }

    /// [`apply_after_counted`] on a given scratch world.
    ///
    /// # Errors
    /// A broken invariant, as text.
    pub fn apply_after_in(
        mut world: World,
        prefix: &[Vec<u8>],
        frame: &[u8],
        after: &[Vec<u8>],
    ) -> Result<(bool, crate::replication::ClientStateCounters), String> {
        for p in prefix {
            apply(&mut world, p).map_err(|e| format!("a recorded frame failed: {e}"))?;
        }
        let before = world.resource::<ClientReplication>().state.counters;
        let applied = apply(&mut world, frame).is_ok();
        if applied {
            for p in after {
                if apply(&mut world, p).is_err() {
                    break;
                }
            }
        }
        let state = &world.resource::<ClientReplication>().state;
        let (rings, tombs, own) = state.map_lens();
        let cap = state.cap();
        if rings > cap || tombs > cap || own > cap {
            return Err(format!("maps ({rings}, {tombs}, {own}) past the cap {cap}"));
        }
        let mut delta = state.counters;
        delta.applied -= before.applied;
        delta.record_errors -= before.record_errors;
        delta.baseline_missing -= before.baseline_missing;
        Ok((applied, delta))
    }

    /// Counts of a run.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Report {
        /// Mutated frames that applied.
        pub applied: usize,
        /// Mutated frames the client refused (a broken frame).
        pub refused: usize,
        /// Records applied to the scratch worlds from the mutated frame on.
        pub records_applied: u64,
        /// Records that failed to decode and were resynced alone.
        pub record_errors: u64,
        /// Deltas whose baseline the scratch world did not hold.
        pub baseline_missing: u64,
    }

    /// A panic or a broken invariant: the iteration and the frame that caused it.
    #[derive(Debug, Clone)]
    pub struct Failure {
        /// The iteration (with the seed, it reproduces the input).
        pub iteration: usize,
        /// The index of the recorded frame that was mutated.
        pub frame: usize,
        /// The mutated frame.
        pub input: Vec<u8>,
        /// The panic message or the broken invariant.
        pub message: String,
    }

    /// A `STATE` frame split into its header and its records: `(tick, seq, [(id, body bits)])`,
    /// each body as `(bits, words)` read 64 bits at a time.
    type Records = (u64, u64, Vec<(u64, usize, Vec<u64>)>);

    fn split_records(bytes: &[u8]) -> Option<Records> {
        use crate::bits::BitReader;
        let mut r = BitReader::new(bytes);
        if r.read_bits(8).ok()? != u64::from(TAG_STATE) {
            return None;
        }
        let tick = r.read_varint().ok()?;
        let seq = r.read_varint().ok()?;
        let n = r.read_varint().ok()?;
        let mut out = Vec::new();
        for _ in 0..n.min(4096) {
            let id = r.read_varint().ok()?;
            let len = usize::try_from(r.read_varint().ok()?).ok()?;
            if len > r.remaining_bits() {
                return None;
            }
            let mut words = Vec::with_capacity(len.div_ceil(64));
            let mut left = len;
            while left > 0 {
                let take = left.min(64);
                words.push(r.read_bits(take as u32).ok()?);
                left -= take;
            }
            out.push((id, len, words));
        }
        Some((tick, seq, out))
    }

    fn join_records(tick: u64, seq: u64, records: &[(u64, usize, Vec<u64>)]) -> Vec<u8> {
        use crate::bits::BitWriter;
        let mut w = BitWriter::new();
        w.write_bits(u64::from(TAG_STATE), 8);
        w.write_varint(tick);
        w.write_varint(seq);
        w.write_varint(records.len() as u64);
        for (id, len, words) in records {
            w.write_varint(*id);
            w.write_varint(*len as u64);
            let mut left = *len;
            for word in words {
                let take = left.min(64);
                w.write_bits(*word, take as u32);
                left -= take;
            }
        }
        w.finish()
    }

    /// A record-level mutation: well-framed records that are wrong for this client (a record
    /// given another entity's id or a fresh one, duplicated, dropped, reordered, or moved from
    /// another recorded frame), so the ring, baseline and tombstone paths see them.
    fn mutate_records(frames: &[Vec<u8>], at: usize, rng: &mut FuzzRng) -> Option<Vec<u8>> {
        let (tick, seq, mut records) = split_records(frames.get(at)?)?;
        let (_, _, others) = split_records(&frames[rng.below(frames.len())])?;
        let ids: Vec<u64> = records.iter().chain(others.iter()).map(|r| r.0).collect();
        for _ in 0..=rng.below(3) {
            let n = records.len();
            match rng.below(6) {
                0 if n > 0 => {
                    let i = rng.below(n);
                    records[i].0 = ids[rng.below(ids.len())];
                }
                1 if n > 0 => {
                    let i = rng.below(n);
                    records[i].0 = rng.next_u64() % 4096;
                }
                2 if n > 0 => {
                    let i = rng.below(n);
                    records.push(records[i].clone());
                }
                3 if n > 1 => {
                    let (i, j) = (rng.below(n), rng.below(n));
                    records.swap(i, j);
                }
                4 if n > 0 => {
                    records.remove(rng.below(n));
                }
                _ if !others.is_empty() => records.push(others[rng.below(others.len())].clone()),
                _ => {}
            }
        }
        // Sometimes an older or newer tick for the same records.
        let tick = match rng.below(4) {
            0 => tick.saturating_sub(rng.below(80) as u64),
            1 => tick + rng.below(80) as u64,
            _ => tick,
        };
        Some(join_records(tick, seq, &records))
    }

    /// One mutation of `frames[at]`: a byte-level mutation of `regia_core::fuzz`, a splice with
    /// another recorded frame, a body-only mutation that keeps the header (so the frame is not
    /// dropped as a duplicate before its records are read), or a record-level mutation.
    pub fn mutate(frames: &[Vec<u8>], at: usize, rng: &mut FuzzRng) -> Vec<u8> {
        let Some(base) = frames.get(at) else {
            return Vec::new();
        };
        match rng.below(5) {
            4 => mutate_records(frames, at, rng).unwrap_or_else(|| mutate_bytes(base, rng)),
            0 => {
                let other = &frames[rng.below(frames.len())];
                let cut = rng.below(base.len() + 1);
                let from = rng.below(other.len() + 1);
                let mut out = base[..cut].to_vec();
                out.extend_from_slice(&other[from..]);
                out
            }
            1 if base.len() > 4 => {
                // Keep tag, tick and seq (a few bytes of varints) and mutate the rest.
                let keep = 4.min(base.len());
                let mut out = base[..keep].to_vec();
                out.extend(mutate_bytes(&base[keep..], rng));
                out
            }
            _ => {
                let mut out = mutate_bytes(base, rng);
                for _ in 0..rng.below(3) {
                    out = mutate_bytes(&out, rng);
                }
                out
            }
        }
    }

    /// Runs `iterations` mutations from `seed`: each applies the recorded frames before a picked
    /// frame, the mutated frame, then up to three recorded frames after it, to a fresh scratch
    /// world (interpolated on odd iterations). A panic or a broken bound is a [`Failure`].
    ///
    /// # Errors
    /// The first failure.
    pub fn run(seed: u64, iterations: usize) -> Result<Report, Box<Failure>> {
        let frames = recorded();
        let mut report = Report::default();
        if frames.is_empty() {
            return Ok(report);
        }
        let mut rng = FuzzRng::new(seed);
        for iteration in 0..iterations {
            let at = rng.below(frames.len());
            let input = mutate(frames, at, &mut rng);
            let after_end = (at + 1 + rng.below(4)).min(frames.len());
            let (prefix, after) = (&frames[..at], &frames[at + 1..after_end]);
            // Every other iteration runs on an interpolated client.
            let world = if iteration % 2 == 1 {
                scratch_world_interpolated()
            } else {
                scratch_world()
            };
            let outcome = catch_unwind(AssertUnwindSafe(|| apply_after_in(world, prefix, &input, after)));
            let message = match outcome {
                Ok(Ok((applied, c))) => {
                    if applied {
                        report.applied += 1;
                    } else {
                        report.refused += 1;
                    }
                    report.records_applied += c.applied;
                    report.record_errors += c.record_errors;
                    report.baseline_missing += c.baseline_missing;
                    continue;
                }
                Ok(Err(broken)) => broken,
                Err(panic) => panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "panic".into()),
            };
            return Err(Box::new(Failure {
                iteration,
                frame: at,
                input,
                message,
            }));
        }
        Ok(report)
    }

    /// The `regia_agent` registry's fixture: the recorded frame with the most bytes.
    pub fn fixture() -> Vec<u8> {
        recorded()
            .iter()
            .max_by_key(|f| f.len())
            .cloned()
            .unwrap_or_default()
    }

    /// Re-encodes `frame` with one extra record appended: a copy of `spawn_source`'s first
    /// record renamed to `fresh_id`, a spawn body for an id the client never saw. Test support
    /// for the scratch cap (ADR-8850): applied when the client already holds a full map, the
    /// extra spawn must refuse the frame instead of growing it. Returns `None` when either frame
    /// does not split into records.
    pub fn with_extra_record(frame: &[u8], spawn_source: &[u8], fresh_id: u64) -> Option<Vec<u8>> {
        let (tick, seq, mut records) = split_records(frame)?;
        let (_, _, source) = split_records(spawn_source)?;
        let mut record = source.first()?.clone();
        record.0 = fresh_id;
        records.push(record);
        Some(join_records(tick, seq, &records))
    }

    /// The `regia_agent` registry's loader: applies `bytes` after every recorded frame before
    /// the fixture, in a fresh scratch world. A refused frame is a rejection (`Err`); a panic
    /// propagates to the registry, which counts it. Returns no re-encoding.
    ///
    /// # Errors
    /// The client refused the frame, or a bound broke.
    pub fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
        let frames = recorded();
        let at = frames
            .iter()
            .enumerate()
            .max_by_key(|(_, f)| f.len())
            .map_or(0, |(i, _)| i);
        match apply_after(&frames[..at], bytes, &[])? {
            true => Ok(Vec::new()),
            false => Err("the client refused the frame".into()),
        }
    }
}
