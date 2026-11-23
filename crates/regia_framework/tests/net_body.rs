// SPDX-License-Identifier: MPL-2.0
//! ADR-2902 acceptance: replicated rigid bodies (`regia_framework::net_body`) between a server app
//! and client apps, all with `NetPlugin`, `PhysicsPlugin` (rapier) and `NetBodyPlugin`, moved at
//! fixed steps over an in-memory transport. Unreliable frames (the acknowledged state channel) pass
//! through the session's `NetConditioner`; reliable frames get the same delay and are never lost
//! (the transport resends them).

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::net_body::{
    BODY_PROFILE, BODY_ROTATION_TOLERANCE_RAD, NetBodyPlugin, NetBodyShape, NetBodyState, NetBodyStats,
    ReplicatedBody, body_position_tolerance, body_profile, net_body_report, net_body_teleport,
};
use regia_net::conditioner::{ConditionerConfig, NetConditioner};
use regia_net::interp::{NetInterpolation, NetInterpolationPlugin, NetSnapshotBuffer};
use regia_net::message::Channel;
use regia_net::relevance::{NetInterest, NetViewpoint, install_net_interest};
use regia_net::replication::{ConnWire, ServerReplication};
use regia_net::runtime::{Datagram, NetEntities, NetNode, server_add_peer};
use regia_net::{
    LocalPeer, NetId, NetMode, NetPlayerId, NetPlugin, NetRole, PeerId, Replicated, TransformReplication,
};
use regia_physics::{BodyKind, Collider, ColliderShape, ExternalImpulse, PhysicsPlugin, RigidBody};
use regia_scene::Transform;
use regia_scene::floating_origin::Rebase;
use regia_scene::streaming::{OriginOffset, OriginShifted};

const HZ: f64 = 60.0;
const MS_PER_TICK: f64 = 1000.0 / HZ;
/// `replication.send_every_ticks` (the default: 30 state packets per second at 60 Hz).
const SEND_EVERY: u64 = 2;
const HALF: f32 = 0.25;

// ---- the transport ------------------------------------------------------------------------------

struct InFlight {
    at: u64,
    order: u64,
    to_server: bool,
    client: usize,
    datagram: Datagram,
}

/// A server app and client apps, stepped together.
struct Rig {
    server: App,
    clients: Vec<App>,
    peers: Vec<PeerId>,
    line: Vec<InFlight>,
    step: u64,
    order: u64,
    /// One-way delay of reliable frames, in ticks.
    delay_ticks: u64,
    conditioner: ConditionerConfig,
    down: Vec<NetConditioner>,
    up: Vec<NetConditioner>,
    /// Server → client bytes per client.
    bytes_down: Vec<u64>,
}

fn addr(channel: Channel, client: usize) -> SocketAddr {
    let c = match channel {
        Channel::Unreliable => 1,
        Channel::UnreliableSequenced => 2,
        Channel::ReliableOrdered => 3,
        Channel::ReliableUnordered => 4,
    };
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, c), client as u16 + 1))
}

fn from_addr(a: SocketAddr) -> (Channel, usize) {
    let SocketAddr::V4(v4) = a else {
        return (Channel::Unreliable, 0);
    };
    let ch = match v4.ip().octets()[3] {
        2 => Channel::UnreliableSequenced,
        3 => Channel::ReliableOrdered,
        4 => Channel::ReliableUnordered,
        _ => Channel::Unreliable,
    };
    (ch, usize::from(v4.port()).saturating_sub(1))
}

fn ground(app: &mut App) {
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(200.0, 0.5, 200.0),
            },
            ..Collider::default()
        },
    ));
}

fn net_app(mode: NetMode, peer: PeerId) -> App {
    let mut app = App::new();
    regia_app::enable_fixed_step(&mut app, HZ);
    app.insert_resource(LocalPeer(Some(peer)))
        .add_plugins(PhysicsPlugin)
        .add_plugins(NetPlugin { mode });
    if mode == NetMode::Client {
        app.add_plugins(NetInterpolationPlugin {
            tick_hz: HZ,
            send_interval_ticks: SEND_EVERY as f64,
        });
    }
    app.add_plugins(NetBodyPlugin);
    if mode.is_server() {
        install_net_interest(app.world_mut());
    }
    ground(&mut app);
    app
}

impl Rig {
    fn new(clients: usize, delay_ticks: u64, conditioner: ConditionerConfig) -> Self {
        let server = net_app(NetMode::DedicatedServer, PeerId::SERVER);
        let mut rig = Rig {
            server,
            clients: Vec::new(),
            peers: Vec::new(),
            line: Vec::new(),
            step: 0,
            order: 0,
            delay_ticks,
            conditioner,
            down: Vec::new(),
            up: Vec::new(),
            bytes_down: Vec::new(),
        };
        for _ in 0..clients {
            rig.join();
        }
        rig
    }

    /// Joins a client (with a viewpoint at the origin on the server); returns its index.
    fn join(&mut self) -> usize {
        let i = self.clients.len();
        let peer = PeerId(i as u32 + 1);
        self.clients.push(net_app(NetMode::Client, peer));
        self.peers.push(peer);
        server_add_peer(self.server.world_mut(), peer);
        self.server
            .world_mut()
            .resource_mut::<ServerReplication>()
            .set_wire(peer, ConnWire::Acked);
        self.server.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 1.7, 8.0)),
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
        ));
        let config = self.conditioner.clone();
        let seeded = |seed: u64| {
            NetConditioner::new(ConditionerConfig {
                seed,
                ..config.clone()
            })
        };
        self.down
            .push(seeded(config.seed.wrapping_add(2 * i as u64 + 11)));
        self.up.push(seeded(config.seed.wrapping_add(2 * i as u64 + 12)));
        self.bytes_down.push(0);
        i
    }

    fn now_ms(&self) -> u64 {
        (self.step as f64 * MS_PER_TICK).round() as u64
    }

    fn push(&mut self, at: u64, to_server: bool, client: usize, datagram: Datagram) {
        self.order += 1;
        self.line.push(InFlight {
            at,
            order: self.order,
            to_server,
            client,
            datagram,
        });
    }

    fn send(&mut self, to_server: bool, client: usize, d: Datagram) {
        if d.channel.is_reliable() {
            let at = self.step + 1 + self.delay_ticks;
            self.push(at, to_server, client, d);
            return;
        }
        let now = self.now_ms();
        let dest = addr(d.channel, client);
        let peer = d.peer;
        let cond = if to_server {
            &mut self.up[client]
        } else {
            &mut self.down[client]
        };
        let out = cond.process(d.bytes, dest, now);
        for (bytes, a) in out {
            let (channel, c) = from_addr(a);
            let at = self.step + 1;
            self.push(at, to_server, c, Datagram { peer, channel, bytes });
        }
    }

    fn release(&mut self) {
        let now = self.now_ms();
        for client in 0..self.clients.len() {
            for to_server in [false, true] {
                let cond = if to_server {
                    &mut self.up[client]
                } else {
                    &mut self.down[client]
                };
                let out = cond.release_due(now);
                let peer = if to_server {
                    self.peers[client]
                } else {
                    PeerId::SERVER
                };
                for (bytes, a) in out {
                    let (channel, c) = from_addr(a);
                    let at = self.step;
                    self.push(at, to_server, c, Datagram { peer, channel, bytes });
                }
            }
        }
    }

    fn step(&mut self) {
        self.step += 1;
        self.release();
        let now = self.step;
        let mut due: Vec<InFlight> = Vec::new();
        let mut wait = Vec::new();
        for f in self.line.drain(..) {
            if f.at <= now {
                due.push(f);
            } else {
                wait.push(f);
            }
        }
        self.line = wait;
        due.sort_by_key(|f| (f.at, f.order));
        for f in due {
            let app = if f.to_server {
                &mut self.server
            } else {
                match self.clients.get_mut(f.client) {
                    Some(c) => c,
                    None => continue,
                }
            };
            app.world_mut().resource_mut::<NetNode>().inbox.push(f.datagram);
        }
        self.server.update();
        let out = std::mem::take(&mut self.server.world_mut().resource_mut::<NetNode>().outbox);
        for d in out {
            let Some(c) = self.peers.iter().position(|p| *p == d.peer) else {
                continue;
            };
            self.bytes_down[c] += d.bytes.len() as u64;
            self.send(false, c, d);
        }
        for c in 0..self.clients.len() {
            self.clients[c].update();
            let out = std::mem::take(&mut self.clients[c].world_mut().resource_mut::<NetNode>().outbox);
            for mut d in out {
                d.peer = self.peers[c];
                self.send(true, c, d);
            }
        }
    }

    fn run(&mut self, n: usize) {
        for _ in 0..n {
            self.step();
        }
    }

    fn server_tick(&self) -> u32 {
        self.server.world().resource::<NetNode>().tick.0
    }
}

// ---- scene helpers ------------------------------------------------------------------------------

fn crate_shape() -> NetBodyShape {
    NetBodyShape::Cuboid {
        half_extents: Vec3::splat(HALF),
    }
}

fn body_bundle(at: Vec3) -> impl Bundle {
    (
        Replicated {
            transform: TransformReplication::Movement,
            profile: Some(BODY_PROFILE.into()),
            ..Replicated::default()
        },
        Transform::from_translation(at),
        ReplicatedBody::with_shape(crate_shape()),
    )
}

/// A stack of `n` crates in a `side` × `side` grid of columns, a small gap between layers so they
/// fall a little and settle.
fn spawn_stack(world: &mut World, n: usize, side: usize, origin: Vec3) -> Vec<Entity> {
    spawn_pile(world, n, side, origin, 0.03, 0.0)
}

/// Crates dropped in layers `gap` apart, each turned by up to `yaw` radians (a fixed pattern by
/// index), so they tumble and pile up before they rest.
fn spawn_pile(world: &mut World, n: usize, side: usize, origin: Vec3, gap: f32, yaw: f32) -> Vec<Entity> {
    let mut out = Vec::new();
    for i in 0..n {
        let layer = i / (side * side);
        let k = i % (side * side);
        let (x, z) = ((k % side) as f32, (k / side) as f32);
        let at = origin
            + Vec3::new(
                x * (2.0 * HALF + 0.05),
                HALF + 0.02 + layer as f32 * (2.0 * HALF + gap),
                z * (2.0 * HALF + 0.05),
            );
        let turn = yaw * (((i * 7919) % 13) as f32 / 6.0 - 1.0);
        let e = world.spawn(body_bundle(at)).id();
        if turn != 0.0
            && let Some(mut t) = world.get_mut::<Transform>(e)
        {
            t.rotation = Quat::from_rotation_y(turn) * Quat::from_rotation_x(0.5 * turn);
        }
        out.push(e);
    }
    out
}

fn net_id(world: &World, e: Entity) -> Option<NetId> {
    world.get::<NetId>(e).copied()
}

fn copy_of(client: &App, id: NetId) -> Option<Entity> {
    client.world().resource::<NetEntities>().entity(id)
}

fn pose(world: &World, e: Entity) -> Option<(Vec3, Quat)> {
    world.get::<Transform>(e).map(|t| (t.translation, t.rotation))
}

fn all_asleep(server: &App, bodies: &[Entity]) -> bool {
    bodies
        .iter()
        .all(|e| server.world().get::<NetBodyState>(*e).is_some_and(|s| s.asleep))
}

/// Steps until rapier has every body asleep (at most `max` steps); returns the steps taken.
fn settle(rig: &mut Rig, bodies: &[Entity], max: usize) -> usize {
    for n in 0..max {
        if n > 30 && all_asleep(&rig.server, bodies) {
            return n;
        }
        rig.step();
    }
    max
}

/// The worst position and rotation error of every client copy against its server body.
fn worst_error(rig: &Rig, client: usize, bodies: &[Entity]) -> (f32, f32, usize) {
    let (mut dp, mut dr, mut missing) = (0.0f32, 0.0f32, 0usize);
    for e in bodies {
        let Some(id) = net_id(rig.server.world(), *e) else {
            missing += 1;
            continue;
        };
        let (Some(c), Some((sp, sr))) = (copy_of(&rig.clients[client], id), pose(rig.server.world(), *e))
        else {
            missing += 1;
            continue;
        };
        let Some((cp, cr)) = pose(rig.clients[client].world(), c) else {
            missing += 1;
            continue;
        };
        dp = dp.max(sp.distance(cp));
        dr = dr.max(sr.angle_between(cr));
    }
    (dp, dr, missing)
}

/// State bytes sent for these bodies per second (5 s window), summed over connections.
fn body_bytes_per_second(rig: &mut Rig, bodies: &[Entity]) -> f64 {
    let ids: Vec<NetId> = bodies
        .iter()
        .filter_map(|e| net_id(rig.server.world(), *e))
        .collect();
    let second = (f64::from(rig.server_tick()) / HZ) as u64;
    let interest = rig.server.world().resource::<NetInterest>();
    let mut total = 0.0;
    for pi in interest.peers.values() {
        for id in &ids {
            if let Some(w) = pi.send.bandwidth.by_entity.get(id) {
                total += w.per_second(second);
            }
        }
    }
    total
}

/// Per connection and body: the tick its state was last sent.
fn last_sent(rig: &Rig, bodies: &[Entity]) -> BTreeMap<(u32, u32), u32> {
    let rep = rig.server.world().resource::<ServerReplication>();
    let mut out = BTreeMap::new();
    for p in &rig.peers {
        for e in bodies {
            if let Some(id) = net_id(rig.server.world(), *e)
                && let Some((sent, _)) = rep.copy_ticks(*p, id)
            {
                out.insert((p.0, id.0), sent.0);
            }
        }
    }
    out
}

fn no_conditioner() -> ConditionerConfig {
    ConditionerConfig::default()
}

// ---- acceptance ---------------------------------------------------------------------------------

/// A stack of 200 crates settles with two clients watching. Every client copy is a kinematic proxy
/// and ends within the profile's tolerance of its server body; the bodies cost bytes while they
/// fall and none once asleep (per-entity byte windows and the last-sent tick of every copy).
#[test]
fn two_clients_watch_200_bodies_settle_and_asleep_bodies_cost_nothing() {
    let mut rig = Rig::new(2, 2, no_conditioner());
    let bodies = spawn_pile(
        rig.server.world_mut(),
        200,
        5,
        Vec3::new(-1.0, 0.0, -1.0),
        0.25,
        0.6,
    );
    rig.run(20);
    let falling = body_bytes_per_second(&mut rig, &bodies);
    assert!(falling > 0.0, "bodies cost bytes while they move");
    let steps = settle(&mut rig, &bodies, 3600);
    assert!(all_asleep(&rig.server, &bodies), "settled within {steps} steps");
    // Sleep waits for every connection's acknowledgement of the final state.
    rig.run(60);
    let tol = body_position_tolerance(&body_profile());
    for c in 0..2 {
        let (dp, dr, missing) = worst_error(&rig, c, &bodies);
        assert_eq!(missing, 0, "client {c} has every body");
        assert!(dp <= tol, "client {c}: position error {dp} > {tol}");
        assert!(
            dr <= BODY_ROTATION_TOLERANCE_RAD,
            "client {c}: rotation error {dr}"
        );
        let report = net_body_report(rig.clients[c].world_mut(), 0);
        assert_eq!((report.bodies, report.proxies), (200, 200), "{report:?}");
        let id = net_id(rig.server.world(), bodies[0]).expect("id");
        let copy = copy_of(&rig.clients[c], id).expect("copy");
        assert_eq!(
            rig.clients[c].world().get::<RigidBody>(copy).map(|b| b.kind),
            Some(BodyKind::Kinematic)
        );
    }
    let before = last_sent(&rig, &bodies);
    assert_eq!(before.len(), 400, "every body on both connections");
    let bytes_before = rig.bytes_down.clone();
    rig.run(360);
    assert_eq!(last_sent(&rig, &bodies), before, "no body record after sleep");
    assert_eq!(
        body_bytes_per_second(&mut rig, &bodies),
        0.0,
        "asleep bodies cost zero bytes"
    );
    let report = net_body_report(rig.server.world_mut(), 3);
    assert_eq!((report.bodies, report.asleep), (200, 200), "{report:?}");
    assert_eq!(report.bytes_per_second, Some(0.0));
    assert_eq!(report.rows.len(), 3);
    let idle: Vec<u64> = rig
        .bytes_down
        .iter()
        .zip(&bytes_before)
        .map(|(a, b)| a - b)
        .collect();
    println!(
        "settled in {steps} steps; falling {falling:.0} B/s for bodies; idle frames {idle:?} B over 360 ticks (no body records)"
    );
}

/// A settled stack is hit: the impulse wakes bodies, and the first moved state reaches a client
/// within the send interval plus the link delay.
#[test]
fn a_wake_on_impact_reaches_clients_within_the_send_interval() {
    let delay = 3;
    let mut rig = Rig::new(
        1,
        delay,
        ConditionerConfig {
            enabled: true,
            delay_ms: (delay as f64 * MS_PER_TICK) as u64,
            ..ConditionerConfig::default()
        },
    );
    let bodies = spawn_stack(rig.server.world_mut(), 18, 3, Vec3::ZERO);
    settle(&mut rig, &bodies, 2400);
    rig.run(60);
    assert!(all_asleep(&rig.server, &bodies));
    let woke_before = rig.server.world().resource::<NetBodyStats>().woke;
    let target = bodies[0];
    let id = net_id(rig.server.world(), target).expect("id");
    let copy = copy_of(&rig.clients[0], id).expect("copy");
    let newest = |rig: &Rig| {
        rig.clients[0]
            .world()
            .get::<NetSnapshotBuffer>(copy)
            .and_then(|b| b.0.newest().map(|s| s.tick))
            .unwrap_or(0)
    };
    let held = newest(&rig);
    rig.server.world_mut().entity_mut(target).insert(ExternalImpulse {
        impulse: Vec3::new(2.0, 0.5, 0.0),
        torque_impulse: Vec3::ZERO,
    });
    let mut woke_at = None;
    let mut arrived_at = None;
    for _ in 0..60 {
        rig.step();
        let t = rig.server_tick();
        if woke_at.is_none() && rig.server.world().resource::<NetBodyStats>().woke > woke_before {
            woke_at = Some(t);
        }
        if arrived_at.is_none() && newest(&rig) > held {
            arrived_at = Some(t);
        }
    }
    let (w, a) = (woke_at.expect("woke"), arrived_at.expect("arrived"));
    println!("woke at tick {w}, first moved state on the client at tick {a} (delay {delay})");
    assert!(
        u64::from(a - w) <= delay + SEND_EVERY + 1,
        "wake at {w}, arrival at {a}: more than the delay plus one send interval"
    );
}

/// The poses the server had at each tick, for comparing what a client shows at its interpolation
/// tick.
fn record(rig: &Rig, bodies: &[Entity], history: &mut BTreeMap<u32, Vec<Vec3>>) {
    let t = rig.server_tick();
    history.insert(
        t,
        bodies
            .iter()
            .map(|e| pose(rig.server.world(), *e).map_or(Vec3::NAN, |p| p.0))
            .collect(),
    );
}

/// 100 ms one way and 5 % loss on the unreliable frames: thrown crates stay within a stated bound
/// of the server's pose at the shown time, snaps never happen, resyncs stay rare, and once they
/// settle the copies match the server within the profile's tolerance.
#[test]
fn latency_and_loss_keep_shown_errors_bounded_without_a_storm() {
    /// The worst distance between a shown copy and the server's pose at the shown tick, while
    /// crates fly at up to 6 m/s and collide (measured worst 0.35 m; extrapolation past a lost
    /// packet is capped at 100 ms and decays back).
    const SHOWN_BOUND_M: f32 = 0.6;
    let mut rig = Rig::new(
        2,
        6,
        ConditionerConfig {
            enabled: true,
            seed: 5,
            loss: 0.05,
            delay_ms: 100,
            ..ConditionerConfig::default()
        },
    );
    let mut bodies = spawn_stack(rig.server.world_mut(), 27, 3, Vec3::ZERO);
    for k in 0..6 {
        let e = rig
            .server
            .world_mut()
            .spawn(body_bundle(Vec3::new(-6.0, 1.0 + k as f32 * 0.6, 0.4)))
            .id();
        rig.server
            .world_mut()
            .entity_mut(e)
            .insert(regia_physics::Velocity {
                linear: Vec3::new(6.0, 2.0, 0.0),
                angular: Vec3::ZERO,
            });
        bodies.push(e);
    }
    let mut history = BTreeMap::new();
    let mut worst = 0.0f32;
    let mut compared = 0u32;
    for _ in 0..600 {
        rig.step();
        record(&rig, &bodies, &mut history);
        let Some(it) = rig.clients[0]
            .world()
            .get_resource::<NetInterpolation>()
            .and_then(|n| n.clock.interp_tick())
        else {
            continue;
        };
        let (t0, f) = (it.floor() as u32, (it - it.floor()) as f32);
        let (Some(a), Some(b)) = (history.get(&t0), history.get(&(t0 + 1))) else {
            continue;
        };
        for (i, e) in bodies.iter().enumerate() {
            let Some(id) = net_id(rig.server.world(), *e) else {
                continue;
            };
            let Some(c) = copy_of(&rig.clients[0], id) else {
                continue;
            };
            if rig.clients[0]
                .world()
                .get::<regia_net::interp::NetHiddenUntilShown>(c)
                .is_some()
            {
                continue;
            }
            let Some((shown, _)) = pose(rig.clients[0].world(), c) else {
                continue;
            };
            let truth = a[i].lerp(b[i], f);
            if truth.is_finite() {
                worst = worst.max(shown.distance(truth));
                compared += 1;
            }
        }
    }
    let counters = rig.clients[0].world().resource::<NetInterpolation>().counters;
    let resyncs: u64 = rig
        .server
        .world()
        .resource::<NetInterest>()
        .peers
        .values()
        .map(|p| p.send.counters.resyncs)
        .sum();
    println!("worst shown error {worst:.3} m over {compared} samples; {counters:?}; resyncs {resyncs}");
    assert!(compared > 1000, "{compared}");
    assert!(worst <= SHOWN_BOUND_M, "worst shown error {worst} m");
    assert_eq!(counters.snaps, 0, "no snaps: {counters:?}");
    assert!(resyncs <= 4, "no resync storm: {resyncs}");
    settle(&mut rig, &bodies, 3600);
    rig.run(120);
    let tol = body_position_tolerance(&body_profile());
    for c in 0..2 {
        let (dp, dr, missing) = worst_error(&rig, c, &bodies);
        assert_eq!(missing, 0);
        assert!(
            dp <= tol && dr <= BODY_ROTATION_TOLERANCE_RAD,
            "client {c}: {dp} m, {dr} rad"
        );
    }
}

/// A client that joins after the stack settled receives every body at its settled pose; each body
/// costs it one spawn record and nothing afterwards.
#[test]
fn a_late_joiner_sees_the_settled_pose() {
    let mut rig = Rig::new(1, 2, no_conditioner());
    let bodies = spawn_stack(rig.server.world_mut(), 40, 4, Vec3::ZERO);
    settle(&mut rig, &bodies, 2400);
    rig.run(60);
    let late = rig.join();
    rig.run(90);
    let tol = body_position_tolerance(&body_profile());
    let (dp, dr, missing) = worst_error(&rig, late, &bodies);
    assert_eq!(missing, 0, "the late joiner has every body");
    assert!(dp <= tol && dr <= BODY_ROTATION_TOLERANCE_RAD, "{dp} m, {dr} rad");
    let before = last_sent(&rig, &bodies);
    rig.run(120);
    assert_eq!(last_sent(&rig, &bodies), before, "asleep after the spawn");
    assert!(all_asleep(&rig.server, &bodies));
}

/// Despawning an asleep body reaches every client.
#[test]
fn a_despawn_while_asleep_is_delivered() {
    let mut rig = Rig::new(2, 2, no_conditioner());
    let bodies = spawn_stack(rig.server.world_mut(), 9, 3, Vec3::ZERO);
    settle(&mut rig, &bodies, 2400);
    rig.run(60);
    // The top of a column: nothing rests on it, so its neighbors stay asleep.
    let gone = *bodies.last().expect("body");
    let id = net_id(rig.server.world(), gone).expect("id");
    assert_eq!(
        rig.server.world().resource::<ServerReplication>().is_asleep(id),
        Some(true)
    );
    for c in 0..2 {
        assert!(copy_of(&rig.clients[c], id).is_some());
    }
    rig.server.world_mut().despawn(gone);
    rig.run(30);
    for c in 0..2 {
        assert!(
            copy_of(&rig.clients[c], id).is_none(),
            "client {c} despawned the copy"
        );
        let report = net_body_report(rig.clients[c].world_mut(), 0);
        assert_eq!(report.bodies, 8);
    }
}

/// A floating-origin rebase on one client keeps every body at its world position there and leaves
/// the other client alone; after a rebase on the server every client still shows every body at its
/// world position once the bodies rest again.
#[test]
fn an_origin_rebase_on_one_peer_keeps_world_positions() {
    let mut rig = Rig::new(2, 2, no_conditioner());
    let bodies = spawn_stack(rig.server.world_mut(), 18, 3, Vec3::ZERO);
    settle(&mut rig, &bodies, 2400);
    rig.run(60);
    let shift = [1024.0f32, 0.0, -512.0];
    rebase(&mut rig.clients[0], shift);
    rig.run(30);
    let tol = body_position_tolerance(&body_profile());
    for e in &bodies {
        let id = net_id(rig.server.world(), *e).expect("id");
        let (sp, _) = pose(rig.server.world(), *e).expect("server");
        let c0 = copy_of(&rig.clients[0], id).expect("copy 0");
        let (p0, _) = pose(rig.clients[0].world(), c0).expect("pose 0");
        let world0 = p0 + Vec3::from_array(shift);
        assert!(world0.distance(sp) <= tol + 1e-3, "{world0} vs {sp}");
    }
    let (dp, _, _) = worst_error(&rig, 1, &bodies);
    assert!(dp <= tol, "the other client did not move: {dp}");
    // A rebase on the server moves the static ground in rapier, which wakes the bodies resting on
    // it: they resend once (a known cost, ADR-2902) and fall asleep again at the same world place.
    rebase(&mut rig.server, [256.0, 0.0, 0.0]);
    let quiet = settle(&mut rig, &bodies, 2400);
    rig.run(60);
    assert!(
        all_asleep(&rig.server, &bodies),
        "asleep again within {quiet} steps"
    );
    let before = last_sent(&rig, &bodies);
    rig.run(120);
    assert_eq!(
        last_sent(&rig, &bodies),
        before,
        "quiet again after the server rebase"
    );
    for c in 0..2 {
        let offset = if c == 0 {
            Vec3::from_array(shift)
        } else {
            Vec3::ZERO
        };
        let (dp, _, _) = worst_error(&rig, c, &bodies);
        for e in &bodies {
            let id = net_id(rig.server.world(), *e).expect("id");
            let (sp, _) = pose(rig.server.world(), *e).expect("server");
            let cc = copy_of(&rig.clients[c], id).expect("copy");
            let (cp, _) = pose(rig.clients[c].world(), cc).expect("pose");
            let world_s = sp + Vec3::new(256.0, 0.0, 0.0);
            assert!(
                (cp + offset).distance(world_s) <= tol + 1e-3,
                "client {c}: {} vs {world_s} (local error {dp})",
                cp + offset
            );
        }
    }
}

/// What the streaming runtime does on a rebase: the offset grows, every root moves back by the
/// shift and `OriginShifted` tells the stores.
fn rebase(app: &mut App, shift: [f32; 3]) {
    let world = app.world_mut();
    let old = world.get_resource::<OriginOffset>().map_or([0.0; 3], |o| o.0);
    world.insert_resource(OriginOffset([
        old[0] + f64::from(shift[0]),
        old[1] + f64::from(shift[1]),
        old[2] + f64::from(shift[2]),
    ]));
    let mut q = world.query_filtered::<&mut Transform, Without<ChildOf>>();
    for mut t in q.iter_mut(world) {
        t.translation -= Vec3::from_array(shift);
    }
    world.write_message(OriginShifted {
        rebase: Rebase {
            cell_delta: [0; 3],
            shift,
        },
        origin_cell: [0; 3],
    });
}

/// A teleported body snaps on the client: no shown frame lies between the old and the new pose.
#[test]
fn a_teleport_snaps_without_inbetween_frames() {
    let mut rig = Rig::new(1, 2, no_conditioner());
    let bodies = spawn_stack(rig.server.world_mut(), 1, 1, Vec3::ZERO);
    settle(&mut rig, &bodies, 2400);
    rig.run(30);
    let e = bodies[0];
    let id = net_id(rig.server.world(), e).expect("id");
    let copy = copy_of(&rig.clients[0], id).expect("copy");
    let from = pose(rig.clients[0].world(), copy).expect("pose").0;
    let to = Vec3::new(3.0, HALF, 2.0);
    assert!(net_body_teleport(
        rig.server.world_mut(),
        e,
        Transform::from_translation(to)
    ));
    let mut seen_new = false;
    for _ in 0..40 {
        rig.step();
        let shown = pose(rig.clients[0].world(), copy).expect("pose").0;
        let near_old = shown.distance(from) < 0.05;
        let near_new = shown.distance(to) < 0.05;
        assert!(near_old || near_new, "an in-between frame at {shown}");
        seen_new |= near_new;
    }
    assert!(seen_new, "the teleport arrived");
    assert_eq!(rig.server.world().resource::<NetBodyStats>().teleports, 1);
}

/// A body that wakes and slides shows no jump: the shown position moves at most a little more than
/// the server body moves per frame.
#[test]
fn a_waking_body_shows_no_jump() {
    /// The largest shown step beyond the server's own per-tick motion (m): a recovery offset that
    /// decays with a 60 ms half-life after a late state.
    const JUMP_BOUND_M: f32 = 0.05;
    let mut rig = Rig::new(
        1,
        3,
        ConditionerConfig {
            enabled: true,
            delay_ms: 50,
            jitter_ms: 10,
            ..ConditionerConfig::default()
        },
    );
    let bodies = spawn_stack(rig.server.world_mut(), 1, 1, Vec3::ZERO);
    settle(&mut rig, &bodies, 2400);
    rig.run(30);
    let e = bodies[0];
    let id = net_id(rig.server.world(), e).expect("id");
    let copy = copy_of(&rig.clients[0], id).expect("copy");
    rig.server.world_mut().entity_mut(e).insert(ExternalImpulse {
        impulse: Vec3::new(0.0, 0.6, 0.4),
        torque_impulse: Vec3::ZERO,
    });
    let mut last_shown = pose(rig.clients[0].world(), copy).expect("pose").0;
    let mut last_server = pose(rig.server.world(), e).expect("server").0;
    let mut max_server_step = 0.0f32;
    let mut worst_excess = 0.0f32;
    for _ in 0..240 {
        rig.step();
        let s = pose(rig.server.world(), e).expect("server").0;
        max_server_step = max_server_step.max(s.distance(last_server));
        last_server = s;
        let shown = pose(rig.clients[0].world(), copy).expect("pose").0;
        worst_excess = worst_excess.max(shown.distance(last_shown) - max_server_step);
        last_shown = shown;
    }
    println!("server step at most {max_server_step:.4} m, shown excess at most {worst_excess:.4} m");
    assert!(max_server_step > 0.0, "it moved");
    assert!(
        worst_excess <= JUMP_BOUND_M,
        "a jump of {worst_excess} m beyond the server's motion"
    );
}

/// Bodies that enter relevance show no jump: a resting crate appears at its rest pose, and a crate
/// sliding in from outside the radius appears where the server had it at the shown tick and then
/// moves at most its own speed per frame (plus the stated bound).
#[test]
fn bodies_entering_relevance_show_no_jump() {
    /// The largest shown step beyond the sliding crate's own motion per tick (m).
    const ENTER_BOUND_M: f32 = 0.05;
    let mut rig = Rig::new(1, 2, no_conditioner());
    // On the ground (±200 m) but outside the body profile's reach (150 m × 1.15) from the viewpoint.
    let resting = rig
        .server
        .world_mut()
        .spawn(body_bundle(Vec3::new(185.0, HALF + 0.02, 0.0)))
        .id();
    let sliding = rig
        .server
        .world_mut()
        .spawn(body_bundle(Vec3::new(195.0, 3.0, 20.0)))
        .id();
    rig.server.world_mut().entity_mut(sliding).insert((
        RigidBody {
            gravity_scale: 0.0,
            can_sleep: false,
            ..RigidBody::dynamic()
        },
        regia_physics::Velocity {
            linear: Vec3::new(-3.0, 0.0, 0.0),
            angular: Vec3::ZERO,
        },
    ));
    settle(&mut rig, &[resting], 2400);
    rig.run(30);
    let rest_id = net_id(rig.server.world(), resting).expect("id");
    let slide_id = net_id(rig.server.world(), sliding).expect("id");
    assert!(copy_of(&rig.clients[0], rest_id).is_none(), "out of relevance");
    // The player walks over.
    {
        let w = rig.server.world_mut();
        let mut q = w.query_filtered::<&mut Transform, With<NetViewpoint>>();
        for mut t in q.iter_mut(w) {
            t.translation = Vec3::new(150.0, 1.7, 8.0);
        }
    }
    let tol = body_position_tolerance(&body_profile());
    let mut slide_last: Option<Vec3> = None;
    let mut server_last = pose(rig.server.world(), sliding).expect("server").0;
    let (mut server_step, mut worst_excess) = (0.0f32, 0.0f32);
    let mut rest_seen = 0u32;
    for _ in 0..180 {
        rig.step();
        let s = pose(rig.server.world(), sliding).expect("server").0;
        server_step = server_step.max(s.distance(server_last));
        server_last = s;
        let shown = |id: NetId| -> Option<Vec3> {
            let c = copy_of(&rig.clients[0], id)?;
            let w = rig.clients[0].world();
            if w.get::<regia_net::interp::NetHiddenUntilShown>(c).is_some() {
                return None;
            }
            pose(w, c).map(|p| p.0)
        };
        if let Some(p) = shown(rest_id) {
            let (sp, _) = pose(rig.server.world(), resting).expect("server");
            assert!(
                p.distance(sp) <= tol,
                "the resting crate appears at rest: {p} vs {sp}"
            );
            rest_seen += 1;
        }
        if let Some(p) = shown(slide_id) {
            if let Some(last) = slide_last {
                worst_excess = worst_excess.max(p.distance(last) - server_step);
            }
            slide_last = Some(p);
        }
    }
    println!(
        "entering: resting shown {rest_seen} frames; sliding excess {worst_excess:.4} m (server step {server_step:.4} m)"
    );
    assert!(rest_seen > 0, "the resting crate became relevant");
    assert!(slide_last.is_some(), "the sliding crate became relevant");
    assert!(
        worst_excess <= ENTER_BOUND_M,
        "a jump of {worst_excess} m on entering"
    );
}

/// Under a tight budget, fast bodies are sent more often than slow ones at the same distance
/// (`NetPriorityScale` from speed), and near bodies more often than far ones at the same speed
/// (the profile's distance factor).
///
/// The rate is 3,000 B/s, about half of what the scene sends with no limit (5.8 kB/s down). It
/// was 15,000 B/s; the quantized transform record and its deltas (ADR-8467) and the
/// velocity-predicted residual (ADR-8522) shrank these records so much that 15,000 B/s no longer
/// bound: every body went every send tick (near fast 672, near slow 785, far fast 672 B/s). The
/// test checks that the budget defers deltas, so the scene cannot go slack again unnoticed
/// (ADR-8717).
#[test]
fn speed_and_distance_set_the_priority_under_a_tight_budget() {
    let mut rig = Rig::new(1, 1, no_conditioner());
    rig.server
        .world_mut()
        .resource_mut::<regia_net::NetSettings>()
        .replication
        .client_rate_bytes = 3_000;
    let spawn_group = |rig: &mut Rig, at: Vec3, speed: f32| -> Vec<Entity> {
        (0..12)
            .map(|k| {
                let e = rig
                    .server
                    .world_mut()
                    .spawn(body_bundle(at + Vec3::new(0.0, 0.0, k as f32 * 0.8)))
                    .id();
                rig.server.world_mut().entity_mut(e).insert((
                    RigidBody {
                        gravity_scale: 0.0,
                        can_sleep: false,
                        ..RigidBody::dynamic()
                    },
                    regia_physics::Velocity {
                        linear: Vec3::new(speed, 0.0, 0.0),
                        angular: Vec3::ZERO,
                    },
                ));
                e
            })
            .collect()
    };
    let near_fast = spawn_group(&mut rig, Vec3::new(-20.0, 3.0, 0.0), 6.0);
    let near_slow = spawn_group(&mut rig, Vec3::new(-20.0, 6.0, 0.0), 0.5);
    let far_fast = spawn_group(&mut rig, Vec3::new(-20.0, 3.0, 110.0), 6.0);
    rig.run(300);
    // Deltas the budget left for a later tick, summed over the last 100 ticks.
    let mut deferred = 0u64;
    for _ in 0..100 {
        rig.run(1);
        deferred += rig
            .server
            .world()
            .resource::<NetInterest>()
            .peers
            .values()
            .map(|pi| u64::from(pi.send.counters.deferred))
            .sum::<u64>();
    }
    let nf = body_bytes_per_second(&mut rig, &near_fast);
    let ns = body_bytes_per_second(&mut rig, &near_slow);
    let ff = body_bytes_per_second(&mut rig, &far_fast);
    println!("near fast {nf:.0} B/s, near slow {ns:.0} B/s, far fast {ff:.0} B/s, deferred {deferred}");
    assert!(deferred > 0, "the budget is tight: it deferred no delta");
    assert!(nf > 1.5 * ns, "speed raises priority: {nf} vs {ns}");
    assert!(nf > 1.5 * ff, "distance lowers priority: {nf} vs {ff}");
}

// ---- a predicted character on and against proxies (ADR-2902 (3)) -------------------------------

mod characters {
    use super::*;
    use regia_core::math::Vec2;
    use regia_framework::character::{CharacterPlugin, CharacterState, use_rapier_character_controller};
    use regia_framework::net_character::{CharacterNetInput, NetCharacter, NetCharacterPlugin};
    use regia_net::NetOwner;
    use regia_net::predict::{ClientPrediction, InputCommand, NetVisualOffset, set_transport_rtt};
    use regia_net::replication::NetPredicted;

    const STAND_OFF: f32 = 1.2;

    #[derive(Resource, Clone, Copy)]
    struct Script(fn(u32) -> CharacterNetInput);

    fn feed(
        script: Res<Script>,
        cp: Res<ClientPrediction>,
        mut q: Query<&mut InputCommand<CharacterNetInput>>,
    ) {
        let next = cp.client_tick().0 + 1;
        for mut c in &mut q {
            c.value = (script.0)(next);
        }
    }

    fn character_app(mode: NetMode, peer: PeerId) -> App {
        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, HZ);
        app.insert_resource(LocalPeer(Some(peer)))
            .add_plugins(PhysicsPlugin)
            .add_plugins(NetPlugin { mode })
            .add_plugins(CharacterPlugin)
            .add_plugins(NetCharacterPlugin);
        use_rapier_character_controller(&mut app);
        if mode == NetMode::Client {
            app.add_plugins(NetInterpolationPlugin {
                tick_hz: HZ,
                send_interval_ticks: SEND_EVERY as f64,
            });
        }
        app.add_plugins(NetBodyPlugin);
        if mode.is_server() {
            install_net_interest(app.world_mut());
        }
        ground(&mut app);
        app
    }

    fn hero_bundle(at: Vec3) -> impl Bundle {
        (
            Replicated {
                transform: TransformReplication::Movement,
                ..Replicated::default()
            },
            Transform::from_translation(at),
            NetCharacter::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            NetPredicted,
            NetOwner(PeerId(1)),
        )
    }

    /// Stands, then walks +X off the plank into a crate and keeps pushing, then stands.
    fn stand_walk_push(t: u32) -> CharacterNetInput {
        CharacterNetInput {
            move_dir: if (240..600).contains(&t) {
                Vec2::new(0.6, 0.0)
            } else {
                Vec2::ZERO
            },
            ..CharacterNetInput::default()
        }
    }

    fn rendered(world: &World, e: Entity) -> Vec3 {
        let offset = world.get::<NetVisualOffset>(e).copied().unwrap_or_default();
        world.get::<Transform>(e).map_or(Vec3::NAN, |t| t.translation) + offset.translation
    }

    /// What one stand-then-push run measured on the client.
    #[derive(Debug, Clone, Copy)]
    struct PushRun {
        standing: u64,
        pushing: u64,
        worst_step: f32,
        crate_moved: f32,
        final_error: f32,
        /// Corrections in the 240 ticks after the push ended.
        after: u64,
        /// The largest per-tick move of the server's character.
        server_step: f32,
    }

    /// A predicted character stands on a body that is a kinematic proxy on its client for 100
    /// ticks, then walks into a crate and pushes it for 360 ticks at 4 ticks of latency each way.
    fn push_run() -> PushRun {
        let delay = 4;
        let mut rig = Rig::new(0, delay, no_conditioner());
        rig.server = character_app(NetMode::DedicatedServer, PeerId::SERVER);
        let peer = PeerId(1);
        rig.clients.push(character_app(NetMode::Client, peer));
        rig.peers.push(peer);
        rig.down.push(NetConditioner::new(no_conditioner()));
        rig.up.push(NetConditioner::new(no_conditioner()));
        rig.bytes_down.push(0);
        server_add_peer(rig.server.world_mut(), peer);
        rig.server
            .world_mut()
            .resource_mut::<ServerReplication>()
            .set_wire(peer, ConnWire::Acked);
        set_transport_rtt(
            rig.clients[0].world_mut(),
            (2 * delay + 1) as f64 * MS_PER_TICK,
            0.0,
        );
        rig.clients[0]
            .world_mut()
            .insert_resource(Script(stand_walk_push));
        rig.clients[0].add_systems(FixedUpdate, feed.before(regia_net::NetSet::Predict));

        let plank = rig
            .server
            .world_mut()
            .spawn((
                Replicated {
                    transform: TransformReplication::Movement,
                    profile: Some(BODY_PROFILE.into()),
                    ..Replicated::default()
                },
                Transform::from_translation(Vec3::new(0.0, 0.25, 0.0)),
                ReplicatedBody::with_shape(NetBodyShape::Cuboid {
                    half_extents: Vec3::new(1.0, 0.25, 1.0),
                }),
            ))
            .id();
        let pushed = rig
            .server
            .world_mut()
            .spawn(body_bundle(Vec3::new(2.4, HALF, 0.0)))
            .id();
        let hero = rig
            .server
            .world_mut()
            .spawn(hero_bundle(Vec3::new(0.0, 0.5 + STAND_OFF + 0.02, 0.0)))
            .id();
        rig.run(120);
        let corrections = |rig: &Rig| {
            rig.clients[0]
                .world()
                .resource::<ClientPrediction>()
                .stats
                .corrections
        };
        let ch = {
            let w = rig.clients[0].world_mut();
            let mut q = w.query_filtered::<(Entity, &NetRole), With<NetCharacter>>();
            q.iter(w)
                .find(|(_, r)| **r == NetRole::Predicted)
                .map(|(e, _)| e)
                .expect("predicted copy")
        };
        let plank_id = net_id(rig.server.world(), plank).expect("plank id");
        let plank_copy = copy_of(&rig.clients[0], plank_id).expect("plank copy");
        assert_eq!(
            rig.clients[0]
                .world()
                .get::<RigidBody>(plank_copy)
                .map(|b| b.kind),
            Some(BodyKind::Kinematic),
            "the plank is a kinematic proxy on the client"
        );
        let at_rest = corrections(&rig);
        rig.run(100);
        let standing = corrections(&rig) - at_rest;
        let (sp, cp) = (
            pose(rig.server.world(), hero).expect("hero").0,
            pose(rig.clients[0].world(), ch).expect("copy").0,
        );
        assert!(
            sp.y > 0.5 + STAND_OFF - 0.05,
            "the server hero stands on the plank: {sp}"
        );
        assert!(sp.distance(cp) < 0.02, "both stand at the same place: {sp} {cp}");

        let pushed_from = pose(rig.server.world(), pushed).expect("crate").0;
        let before_push = corrections(&rig);
        let mut last = rendered(rig.clients[0].world(), ch);
        let mut last_server = pose(rig.server.world(), hero).expect("hero").0;
        let (mut worst_step, mut server_step) = (0.0f32, 0.0f32);
        for _ in 0..420 {
            rig.step();
            let drawn = rendered(rig.clients[0].world(), ch);
            assert!(drawn.is_finite(), "the drawn character stays finite");
            worst_step = worst_step.max(drawn.distance(last));
            last = drawn;
            let s = pose(rig.server.world(), hero).expect("hero").0;
            server_step = server_step.max(s.distance(last_server));
            last_server = s;
        }
        let pushing = corrections(&rig) - before_push;
        let crate_moved = pose(rig.server.world(), pushed)
            .expect("crate")
            .0
            .distance(pushed_from);
        let before_rest = corrections(&rig);
        rig.run(240);
        let after = corrections(&rig) - before_rest;
        let (sp, cp) = (
            pose(rig.server.world(), hero).expect("hero").0,
            pose(rig.clients[0].world(), ch).expect("copy").0,
        );
        PushRun {
            standing,
            pushing,
            worst_step,
            crate_moved,
            final_error: sp.distance(cp),
            after,
            server_step,
        }
    }

    /// Standing on a proxy body makes no correction. Pushing a crate the client only sees late
    /// (pushed bodies are not predicted, ADR-2902 (4)) corrects while the push lasts and stops
    /// when it ends: the drawn character never jumps further in a frame than the server's
    /// character moves in a tick plus the correction smoothing, it stays finite, and both sides
    /// agree at rest.
    #[test]
    fn a_predicted_character_stands_on_and_pushes_proxies_stably() {
        /// Corrections allowed over 360 ticks of pushing at 4 ticks of latency: measured 167 (one
        /// about every two ticks, each re-running the ticks since the server's state). The bound
        /// catches a run-away, not the rate the deferral of spec 73 § 14 accepts.
        const PUSH_CORRECTIONS_MAX: u64 = 240;
        /// The largest drawn step beyond the server character's own largest step per tick (m).
        const POP_EXCESS_M: f32 = 0.1;
        let r = push_run();
        println!("{r:?}");
        assert_eq!(r.standing, 0, "standing on a proxy makes no correction: {r:?}");
        assert!(r.crate_moved > 0.3, "the server crate was pushed: {r:?}");
        assert!(
            r.pushing <= PUSH_CORRECTIONS_MAX,
            "no run-away while pushing: {r:?}"
        );
        assert_eq!(r.after, 0, "corrections stop when the push ends: {r:?}");
        assert!(r.worst_step - r.server_step <= POP_EXCESS_M, "no pop: {r:?}");
        assert!(r.final_error < 0.05, "both sides agree at rest: {r:?}");
    }
}

// ---- informational bench (ADR-2902): 500 bodies, 4 clients --------------------------------------

/// Bandwidth and server tick cost of 500 crates watched by 4 clients, by phase (falling, settling,
/// asleep). Wall-clock numbers, printed only: run with
/// `cargo test -p regia_framework --features net --test net_body -- --ignored --nocapture bench_`.
#[test]
#[ignore = "informational bench: prints a table, asserts nothing about time"]
fn bench_500_bodies_4_clients() {
    let mut rig = Rig::new(4, 2, no_conditioner());
    let mut bodies = Vec::new();
    for k in 0..5 {
        let origin = Vec3::new(-6.0 + k as f32 * 3.0, 0.0, -1.0);
        bodies.extend(spawn_pile(rig.server.world_mut(), 100, 4, origin, 0.25, 0.6));
    }
    println!(
        "| phase | ticks | server tick ms (mean) | physics step ms | net gather ms | client tick ms (mean) | bytes/s per client | awake |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let phase = |rig: &mut Rig, name: &str, ticks: usize| {
        let bytes0 = rig.bytes_down.clone();
        let (mut server_ms, mut physics_ms, mut gather_ms, mut client_ms) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for _ in 0..ticks {
            rig.step_timed(&mut server_ms, &mut client_ms);
            physics_ms += rig
                .server
                .world()
                .resource::<regia_physics::PhysicsWorld>()
                .stats()
                .last_step_ms;
            let t = rig.server.world().resource::<ServerReplication>().timing;
            gather_ms += t.changes_ms + t.relevance_ms + t.send_ms;
        }
        let n = ticks as f64;
        let secs = n / HZ;
        let per_client: Vec<f64> = rig
            .bytes_down
            .iter()
            .zip(&bytes0)
            .map(|(a, b)| (a - b) as f64 / secs)
            .collect();
        let mean_bytes = per_client.iter().sum::<f64>() / per_client.len().max(1) as f64;
        let awake = bodies.len()
            - bodies
                .iter()
                .filter(|e| {
                    rig.server
                        .world()
                        .get::<NetBodyState>(**e)
                        .is_some_and(|s| s.asleep)
                })
                .count();
        println!(
            "| {name} | {ticks} | {:.3} | {:.3} | {:.3} | {:.3} | {:.0} | {awake} |",
            server_ms / n,
            physics_ms / n,
            gather_ms / n,
            client_ms / n / rig.clients.len() as f64,
            mean_bytes
        );
    };
    phase(&mut rig, "falling", 120);
    phase(&mut rig, "settling", 600);
    let left = settle(&mut rig, &bodies, 3600);
    println!("(settled {left} ticks after the settling phase)");
    rig.run(120);
    phase(&mut rig, "asleep", 300);
}

impl Rig {
    /// One step, adding the server's and the clients' update wall time in milliseconds.
    fn step_timed(&mut self, server_ms: &mut f64, client_ms: &mut f64) {
        self.step += 1;
        self.release();
        let now = self.step;
        let mut due: Vec<InFlight> = Vec::new();
        let mut wait = Vec::new();
        for f in self.line.drain(..) {
            if f.at <= now {
                due.push(f);
            } else {
                wait.push(f);
            }
        }
        self.line = wait;
        due.sort_by_key(|f| (f.at, f.order));
        for f in due {
            let app = if f.to_server {
                &mut self.server
            } else {
                match self.clients.get_mut(f.client) {
                    Some(c) => c,
                    None => continue,
                }
            };
            app.world_mut().resource_mut::<NetNode>().inbox.push(f.datagram);
        }
        let t = std::time::Instant::now();
        self.server.update();
        *server_ms += t.elapsed().as_secs_f64() * 1000.0;
        let out = std::mem::take(&mut self.server.world_mut().resource_mut::<NetNode>().outbox);
        for d in out {
            let Some(c) = self.peers.iter().position(|p| *p == d.peer) else {
                continue;
            };
            self.bytes_down[c] += d.bytes.len() as u64;
            self.send(false, c, d);
        }
        for c in 0..self.clients.len() {
            let t = std::time::Instant::now();
            self.clients[c].update();
            *client_ms += t.elapsed().as_secs_f64() * 1000.0;
            let out = std::mem::take(&mut self.clients[c].world_mut().resource_mut::<NetNode>().outbox);
            for mut d in out {
                d.peer = self.peers[c];
                self.send(true, c, d);
            }
        }
    }
}
