// SPDX-License-Identifier: MPL-2.0
//! v0.5 net acceptance (ledger N01 and N03), on the loopback hub at fixed steps over a conditioned
//! link (loss, delay, duplicates, reordering), seeded; nothing reads a wall clock.
//!
//! - **N01** game-defined payloads, delta snapshots, relevance and the RPC channels in one session:
//!   a game component with a derived codec and one with a hand-written codec arrive exactly; a
//!   one-field change travels as a small delta, not the spawn's full state; spatial relevance gives
//!   each client only what is near it and follows a move both ways; reliable to-server calls arrive
//!   once each in order, a rejected call is counted and never runs, an entity broadcast reaches
//!   only the clients that hold the entity, a session broadcast reaches everyone once, and an
//!   unreliable sequenced to-owner stream reaches only the owner, never out of order. It runs on the
//!   reliable tick stream and on the negotiated state channel, and the same seed repeats.
//! - **N03** lag compensation through the real input stream: two clients on a lossy, reordering link
//!   fire at a strafing target's shown center; every shot the server judges hits `body`, posed at the
//!   view tick its command carried, clamped to the rewind window (exactly at that tick when the
//!   window is wide enough for the link), and the same shots with rewind off miss.

use std::collections::BTreeMap;

use glam::Vec3;
use regia_core::math::Ray;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_ecs::prelude::*;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::CodecContext;
use regia_net::interp::{
    NetInterpolation, install_interpolation, net_interpolate, view_tick_from_wire, view_tick_to_wire,
};
use regia_net::lagcomp::{install_lag_compensation, lagcomp_record, max_rewind_ticks};
use regia_net::message::{NetMessageRegistry, ValidateContext, Verdict, register_net_message};
use regia_net::predict::set_transport_rtt;
use regia_net::replication::{ConnWire, NetPredicted, ServerReplication, register_replicated};
use regia_net::runtime::{Datagram, NetEntities, NetNode};
use regia_net::testing::{LinkConditions, LoopbackHub};
use regia_net::*;
use regia_scene::transform::Transform;

// ---- N01: the game's own types ------------------------------------------------------------------

/// A game component with a derived codec.
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Loadout {
    ammo: u32,
    armor: u32,
    #[replicate(quantize(min = 0.0, max = 1.0, bits = 10))]
    stamina: f32,
    callsign: String,
}

/// A game component with a hand-written codec (large, so a full state is easy to tell from a delta).
#[derive(Component, Clone, Debug, PartialEq, Default)]
struct Inventory(Vec<u16>);

impl ReplicateCodec for Inventory {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(self.0.len() as u64);
        for item in &self.0 {
            w.write_bits(u64::from(*item), 16);
        }
    }
    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let n = r.read_len(4096)?;
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(r.read_bits(16)? as u16);
        }
        Ok(Self(items))
    }
}

/// A reliable call to the server, validated there.
#[derive(Clone, Debug, PartialEq, Replicate)]
struct UseItem {
    slot: u8,
    seq: u32,
}

impl NetMessage for UseItem {
    const DIRECTION: Direction = Direction::ToServer;
    fn validate(&self, _cx: &ValidateContext) -> Verdict {
        if self.slot == 255 {
            Verdict::Reject("no such slot")
        } else {
            Verdict::Accept
        }
    }
}
impl ToServerMessage for UseItem {}

/// An entity broadcast: routed to the clients that hold the entity.
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "acceptance.explosion")]
struct Explosion {
    radius: f32,
}

/// A session broadcast: every client.
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", id = "acceptance.chat")]
struct Chat {
    line: String,
}

/// An unreliable sequenced stream to the owner of an entity.
#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(
    direction = "to_owner",
    channel = "unreliable_sequenced",
    id = "acceptance.ping"
)]
struct Ping {
    n: u32,
}

/// What a process's game code saw, tick by tick.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct Log {
    used: Vec<(u32, u8, u32)>,
    explosions: Vec<u32>,
    chat: Vec<String>,
    pings: Vec<u32>,
}

fn record(
    used: Res<NetMessages<UseItem>>,
    explosions: Res<NetMessages<Explosion>>,
    chat: Res<NetMessages<Chat>>,
    pings: Res<NetMessages<Ping>>,
    mut log: ResMut<Log>,
) {
    for m in used.iter() {
        log.used.push((m.from.0, m.msg.slot, m.msg.seq));
    }
    for m in explosions.iter() {
        log.explosions.push(m.msg.radius as u32);
    }
    for m in chat.iter() {
        log.chat.push(m.msg.line.clone());
    }
    for m in pings.iter() {
        log.pings.push(m.msg.n);
    }
}

fn registered(world: &mut World) {
    register_replicated::<Loadout>(world, ReplicateOptions::default());
    register_replicated::<Inventory>(world, ReplicateOptions::default());
    register_net_message::<UseItem>(world);
    register_net_message::<Explosion>(world);
    register_net_message::<Chat>(world);
    register_net_message::<Ping>(world);
    world.init_resource::<Log>();
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        let mut profiles = world.resource_mut::<NetProfiles>();
        profiles
            .insert(
                "near",
                NetProfile {
                    relevance: RelevanceShape::Spatial {
                        radius: 20.0,
                        hysteresis: 0.1,
                    },
                    linger_ticks: 0,
                    ..NetProfile::default()
                },
            )
            .expect("near profile");
        profiles
            .insert(
                "everywhere",
                NetProfile {
                    relevance: RelevanceShape::World,
                    linger_ticks: 0,
                    ..NetProfile::default()
                },
            )
            .expect("world profile");
        profiles.set_default("everywhere");
    }
}

fn reliable_setup(world: &mut World) {
    registered(world);
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Reliable;
}

fn state_setup(world: &mut World) {
    registered(world);
    world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
}

fn send<M: Send + 'static>(
    world: &mut World,
    f: impl FnOnce(&mut NetSender) -> M + Send + Sync + 'static,
) -> M {
    let mut f = Some(f);
    world
        .run_system_once(move |mut net: NetSender| (f.take().expect("runs once"))(&mut net))
        .expect("system runs")
}

fn at(x: f32) -> Transform {
    Transform::from_translation(Vec3::new(x, 0.0, 0.0))
}

fn loadout(ammo: u32) -> Loadout {
    Loadout {
        ammo,
        armor: 50,
        stamina: 0.5,
        callsign: "kestrel".into(),
    }
}

/// The client's copy of a loadout: exact, except the quantized field, which is within half of
/// one of its 1023 steps.
fn assert_loadout(got: Option<Loadout>, want: Loadout, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: no copy"));
    assert!(
        (got.stamina - want.stamina).abs() <= 0.5 / 1023.0 + f32::EPSILON,
        "{what}: {got:?} vs {want:?}"
    );
    assert_eq!(
        Loadout {
            stamina: want.stamina,
            ..got
        },
        want,
        "{what}"
    );
}

fn copy(hub: &LoopbackHub, client: usize, e: Entity) -> Option<Entity> {
    let id = hub.server.world.get::<NetId>(e).copied()?;
    hub.clients[client].world.resource::<NetEntities>().entity(id)
}

fn component<C: Component + Clone>(hub: &LoopbackHub, client: usize, e: Entity) -> Option<C> {
    copy(hub, client, e).and_then(|c| hub.clients[client].world.get::<C>(c).cloned())
}

/// Tick frames (1) and state frames (7) carry replication.
fn state_frame_sizes(log: &[(u64, Datagram)]) -> Vec<usize> {
    log.iter()
        .filter(|(_, d)| matches!(d.bytes.first(), Some(1 | 7)))
        .map(|(_, d)| d.bytes.len())
        .collect()
}

/// What one N01 session ended with, for the repeat check.
#[derive(Debug, Clone, PartialEq)]
struct N01Outcome {
    logs: Vec<Log>,
    server_log: Log,
    spawn_frame: usize,
    delta_frame: usize,
    rejected: u64,
}

fn n01_session(setup: fn(&mut World), negotiate: bool, seed: u64) -> N01Outcome {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
    hub.negotiate = negotiate;
    let conditions = LinkConditions {
        loss: 0.1,
        delay_ticks: 2,
        duplicate: 0.02,
        reorder: 0.05,
        ..LinkConditions::default()
    };
    hub.down = conditions;
    hub.up = conditions;
    let (a, b) = (hub.add_client(), hub.add_client());
    let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
    hub.server.gameplay.add_systems(record);
    for c in [a, b] {
        hub.clients[c].gameplay.add_systems(record);
    }
    for (peer, x) in [(pa, 0.0), (pb, 200.0)] {
        hub.server.world.spawn((
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
            at(x),
        ));
    }
    let inventory = Inventory((0..240).map(|i| i * 7 + 3).collect());
    let hero = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            at(1.0),
            loadout(30),
            inventory.clone(),
            NetOwner(pa),
            NetProfileRef("near".into()),
        ))
        .id();
    let crate_ = hub
        .server
        .world
        .spawn((
            Replicated::default(),
            at(198.0),
            loadout(5),
            NetProfileRef("near".into()),
        ))
        .id();
    let flag = hub
        .server
        .world
        .spawn((Replicated::default(), at(100.0), loadout(1)))
        .id();
    hub.run(60);
    if negotiate {
        for peer in [pa, pb] {
            let wire = hub
                .server
                .world
                .resource::<ServerReplication>()
                .state_link(peer)
                .expect("connection")
                .wire;
            assert_eq!(wire, ConnWire::Acked, "the state channel was negotiated");
        }
    }

    // Payloads: each client holds exactly what is near it, with the game's values.
    assert_loadout(component::<Loadout>(&hub, a, hero), loadout(30), "hero on A");
    assert_eq!(component::<Inventory>(&hub, a, hero), Some(inventory.clone()));
    assert!(copy(&hub, b, hero).is_none(), "the hero is far from B");
    assert_loadout(component::<Loadout>(&hub, b, crate_), loadout(5), "crate on B");
    assert!(copy(&hub, a, crate_).is_none(), "the crate is far from A");
    for c in [a, b] {
        assert_loadout(component::<Loadout>(&hub, c, flag), loadout(1), "world relevance");
    }
    let spawn_frame = state_frame_sizes(&hub.client_logs[a])
        .into_iter()
        .max()
        .expect("frames");

    // Delta: one field changes, the frames after it are a small fraction of the spawn's.
    let from = hub.client_logs[a].len();
    if let Some(mut l) = hub.server.world.get_mut::<Loadout>(hero) {
        l.ammo = 29;
    }
    hub.run(40);
    assert_loadout(
        component::<Loadout>(&hub, a, hero),
        loadout(29),
        "hero after the change",
    );
    let delta_frame = state_frame_sizes(&hub.client_logs[a][from..])
        .into_iter()
        .max()
        .expect("frames after the change");
    assert!(
        spawn_frame > 480,
        "the spawn carried the inventory: {spawn_frame}"
    );
    assert!(
        delta_frame * 8 < spawn_frame,
        "a one-field change is a delta: {delta_frame} vs spawn {spawn_frame}"
    );

    // Relevance follows a move both ways.
    if let Some(mut t) = hub.server.world.get_mut::<Transform>(crate_) {
        t.translation.x = 4.0;
    }
    hub.run(60);
    assert_loadout(
        component::<Loadout>(&hub, a, crate_),
        loadout(5),
        "the crate came to A",
    );
    assert!(copy(&hub, b, crate_).is_none(), "and left B");

    // RPCs: twenty reliable calls in order, one rejected call, routed broadcasts, an owner stream.
    let hero_on_a = copy(&hub, a, hero).expect("A holds its hero");
    for seq in 0..20u32 {
        let slot = if seq == 10 { 255 } else { (seq % 4) as u8 };
        send(&mut hub.clients[a].world, move |n| {
            n.send_to_server(hero_on_a, UseItem { slot, seq })
        })
        .expect("sent");
        hub.step();
    }
    send(&mut hub.server.world, move |n| {
        n.broadcast(crate_, Explosion { radius: 12.0 })
    })
    .expect("entity broadcast");
    send(&mut hub.server.world, |n| {
        n.broadcast_session(Chat {
            line: "round two".into(),
        })
    })
    .expect("session broadcast");
    for n in 0..30u32 {
        send(&mut hub.server.world, move |s| s.send_to_owner(hero, Ping { n })).expect("ping");
        hub.step();
    }
    hub.run(60);

    let server_log = hub.server.world.resource::<Log>().clone();
    let expected: Vec<(u32, u8, u32)> = (0..20u32)
        .filter(|s| *s != 10)
        .map(|s| (pa.0, (s % 4) as u8, s))
        .collect();
    assert_eq!(server_log.used, expected, "every valid call once, in order");
    let registry = hub.server.world.resource::<NetMessageRegistry>();
    let index = registry.index_of::<UseItem>().expect("registered");
    let rejected = registry.counters[usize::from(index)].rejected;
    assert_eq!(rejected, 1, "the bad slot was rejected, not run");
    let logs: Vec<Log> = [a, b]
        .iter()
        .map(|c| hub.clients[*c].world.resource::<Log>().clone())
        .collect();
    assert_eq!(logs[0].explosions, vec![12], "A holds the crate");
    assert!(logs[1].explosions.is_empty(), "B does not hold it: {:?}", logs[1]);
    for log in &logs {
        assert_eq!(
            log.chat,
            vec!["round two".to_owned()],
            "a session broadcast, once"
        );
    }
    let pings = &logs[0].pings;
    assert!(pings.windows(2).all(|w| w[0] < w[1]), "sequenced: {pings:?}");
    assert!(
        pings.len() >= 15,
        "most of an unreliable stream arrives at 10 % loss: {pings:?}"
    );
    assert!(logs[1].pings.is_empty(), "only the owner gets it");
    N01Outcome {
        logs,
        server_log,
        spawn_frame,
        delta_frame,
        rejected,
    }
}

#[test]
fn n01_game_payloads_deltas_relevance_and_rpcs_on_the_reliable_stream() {
    let first = n01_session(reliable_setup, false, 71);
    assert_eq!(
        first,
        n01_session(reliable_setup, false, 71),
        "the same seed repeats"
    );
    println!(
        "n01 reliable: spawn frame {} B, delta frame {} B, pings {}",
        first.spawn_frame,
        first.delta_frame,
        first.logs[0].pings.len()
    );
}

#[test]
fn n01_game_payloads_deltas_relevance_and_rpcs_on_the_state_channel() {
    let first = n01_session(state_setup, true, 72);
    assert_eq!(first, n01_session(state_setup, true, 72), "the same seed repeats");
    println!(
        "n01 state channel: spawn frame {} B, delta frame {} B, pings {}",
        first.spawn_frame,
        first.delta_frame,
        first.logs[0].pings.len()
    );
}

// ---- N03: lag compensation through the input stream on a conditioned link -----------------------

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;
const ORIGIN: Vec3 = Vec3::new(0.0, 1.0, -12.0);
const SHOTS: u32 = 100;

#[derive(Clone, Debug, PartialEq, Default, Replicate)]
struct Pad {
    fire: bool,
    aim: Vec3,
}

impl NetInput for Pad {
    fn on_missing(last: &Self) -> Self {
        Self {
            fire: false,
            aim: last.aim,
        }
    }
}

#[derive(Component)]
struct Target;

#[derive(Resource, Default)]
struct Aim(Pad);

/// Server: per shooter peer, the shots judged.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct Tally(BTreeMap<u32, Judged>);

/// Server: per `(shooter peer, command tick)`, the tick the judged shot was posed at, with the
/// present tick and the oldest recorded tick when it was judged.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct PosedAt(BTreeMap<(u32, u32), Posed>);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Posed {
    at: f64,
    current: u32,
    oldest: u32,
}

/// Client: per command tick that fired, the view tick the command carried (the client's
/// `interp_tick` when the tick was captured, quantized as on the wire).
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct CarriedView(BTreeMap<u32, f64>);

#[derive(Default, Debug, Clone, Copy, PartialEq)]
struct Judged {
    last_tick: u32,
    hits: u32,
    misses: u32,
    body_hits: u32,
    rewound: u32,
}

/// Triangle wave between -8 and 8 m at 6 m/s.
fn strafe_x(tick: u32) -> f32 {
    let leg = (16.0 / 6.0 * HZ) as u32;
    let phase = tick % (2 * leg);
    let s = if phase < leg { phase } else { 2 * leg - phase };
    -8.0 + 16.0 * s as f32 / leg as f32
}

fn server_strafe(world: &mut World) {
    let tick = world.resource::<NetNode>().tick.0;
    let mut q = world.query_filtered::<&mut Transform, With<Target>>();
    for mut t in q.iter_mut(world) {
        t.translation.x = strafe_x(tick);
    }
}

/// Predicted schedule: on the server each fired command becomes one rewound raycast; on a client
/// the first (non-resimulated) run of a fired tick records the view tick its command carries.
fn judge(world: &mut World) {
    if !world.resource::<NetMode>().is_server() {
        let fired: Vec<u32> = world
            .query::<&InputCommand<Pad>>()
            .iter(world)
            .filter(|c| c.value.fire)
            .map(|c| c.tick.0)
            .collect();
        let view = world
            .get_resource::<NetInterpolation>()
            .and_then(|ni| ni.clock.interp_tick())
            .map(|t| view_tick_from_wire(view_tick_to_wire(t)));
        if let Some(view) = view {
            let mut carried = world.resource_mut::<CarriedView>();
            for tick in fired {
                // A resimulation of the tick runs later with a newer clock; the capture came first.
                carried.0.entry(tick).or_insert(view);
            }
        }
        return;
    }
    let mut q = world.query::<(&NetPlayer, &InputCommand<Pad>)>();
    let shots: Vec<(NetPlayerId, u32, Vec3)> = q
        .iter(world)
        .filter(|(_, c)| c.value.fire)
        .map(|(p, c)| (p.id, c.tick.0, c.value.aim))
        .collect();
    for (shooter, tick, aim) in shots {
        let last = world
            .resource::<Tally>()
            .0
            .get(&shooter.peer.0)
            .map_or(0, |j| j.last_tick);
        if tick <= last {
            continue;
        }
        let (hit, at, rewound) = {
            let lc = LagCompensation::new(world).expect("server");
            let at = lc.rewind_tick(shooter).expect("connected");
            let hit = lc
                .raycast(
                    shooter,
                    Ray::new(ORIGIN, aim - ORIGIN),
                    100.0,
                    &HitFilter::default(),
                )
                .expect("query");
            let posed = Posed {
                at: at.0,
                current: lc.current_tick().0,
                oldest: lc.oldest_tick().0,
            };
            (hit, posed, at.0 < f64::from(lc.current_tick().0))
        };
        world
            .resource_mut::<PosedAt>()
            .0
            .insert((shooter.peer.0, tick), at);
        let mut tally = world.resource_mut::<Tally>();
        let j = tally.0.entry(shooter.peer.0).or_default();
        j.last_tick = tick;
        j.rewound += u32::from(rewound);
        match hit {
            Some(h) => {
                j.hits += 1;
                j.body_hits += u32::from(h.tag.as_str() == "body");
            }
            None => j.misses += 1,
        }
    }
}

fn feed(aim: Res<Aim>, mut q: Query<&mut InputCommand<Pad>>) {
    for mut c in &mut q {
        c.value = aim.0.clone();
    }
}

fn shooter_setup(world: &mut World) {
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    add_predicted_systems(world, judge);
    world.init_resource::<Tally>();
    world.init_resource::<PosedAt>();
    world.init_resource::<CarriedView>();
}

/// Per `(shooter peer, command tick)`: the tick the server posed each judged shot at, and the view
/// tick each client's fired command carried.
#[derive(Debug, Default)]
struct Views {
    posed: BTreeMap<(u32, u32), Posed>,
    /// The server's rewind window, in ticks.
    max_rewind: u32,
    carried: BTreeMap<(u32, u32), f64>,
}

/// Two shooters fire `SHOTS` each at the target's shown center over a link that loses 5 % of its
/// frames, duplicates and reorders some, at 4 ticks each way. Returns the server's tally and the
/// shots each client fired, and the posed and carried view ticks. `max_rewind_ms` replaces the
/// default rewind window (1,000 ms) when set.
fn two_shooters(rewind: bool, max_rewind_ms: Option<f64>, seed: u64) -> (Tally, [u32; 2], Views) {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, shooter_setup);
    let conditions = LinkConditions {
        loss: 0.05,
        delay_ticks: 4,
        duplicate: 0.02,
        reorder: 0.05,
        ..LinkConditions::default()
    };
    hub.down = conditions;
    hub.up = conditions;
    let clients = [hub.add_client(), hub.add_client()];
    for c in clients {
        let peer = hub.clients[c].peer;
        let cw = &mut hub.clients[c].world;
        install_interpolation(cw, HZ, 1.0);
        set_transport_rtt(cw, 9.0 * MSPT, 1.0);
        cw.init_resource::<Aim>();
        hub.clients[c].gameplay.add_systems(feed);
        hub.server.world.spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(0.0, 0.0, -20.0 - c as f32)),
            NetPredicted,
            NetOwner(peer),
        ));
    }
    let sw = &mut hub.server.world;
    install_lag_compensation(sw);
    sw.resource_mut::<NetSettings>().lag_compensation.enabled = rewind;
    if let Some(ms) = max_rewind_ms {
        sw.resource_mut::<NetSettings>().lag_compensation.max_rewind_ms = ms;
    }
    hub.server
        .gameplay
        .add_systems((server_strafe, lagcomp_record).chain());
    let target = hub
        .server
        .world
        .spawn((
            Target,
            Replicated::default(),
            Transform::from_translation(Vec3::new(strafe_x(0), 0.0, 0.0)),
            HitShapes {
                shapes: vec![
                    HitShape::new(
                        HitShapeKind::Capsule {
                            radius: 0.4,
                            half_height: 0.5,
                        },
                        "body",
                    )
                    .at(Vec3::new(0.0, 1.0, 0.0)),
                ],
                pose_source: PoseSource::Root,
            },
        ))
        .id();
    let mut fired = [0u32; 2];
    for step in 1..=2400u64 {
        let now = step as f64 * MSPT;
        for (i, c) in clients.into_iter().enumerate() {
            let cw = &mut hub.clients[c].world;
            cw.resource_mut::<NetInterpolation>().now_ms = now;
            net_interpolate(cw, now);
            let mut pad = Pad::default();
            // The shooters alternate ticks; each fires every third tick after a settle.
            if step >= 300 && (step + i as u64).is_multiple_of(3) && fired[i] < SHOTS {
                let shown = hub
                    .server
                    .world
                    .get::<NetId>(target)
                    .copied()
                    .and_then(|id| hub.clients[c].world.resource::<NetEntities>().entity(id))
                    .and_then(|e| hub.clients[c].world.get::<Transform>(e));
                if let Some(t) = shown {
                    pad.fire = true;
                    pad.aim = t.translation + Vec3::Y;
                    fired[i] += 1;
                }
            }
            hub.clients[c].world.resource_mut::<Aim>().0 = pad;
        }
        hub.step();
    }
    let mut views = Views {
        posed: hub.server.world.resource::<PosedAt>().0.clone(),
        max_rewind: max_rewind_ticks(&hub.server.world.resource::<NetSettings>().lag_compensation, HZ),
        ..Views::default()
    };
    for c in clients {
        let peer = hub.clients[c].peer.0;
        for (tick, view) in &hub.clients[c].world.resource::<CarriedView>().0 {
            views.carried.insert((peer, *tick), *view);
        }
    }
    (hub.server.world.resource::<Tally>().clone(), fired, views)
}

/// The rewind half of N03 for one run: both shooters judged (at least 95 % of their shots), every
/// judged shot hits `body` in the past, and each is posed at the view tick its own command carried,
/// clamped to the rewind window and the history (spec 75 § 5 step 1). Returns, per peer, how many
/// shots the clamp moved.
fn judged_where_seen(on: &Tally, views: &Views) -> BTreeMap<u32, u32> {
    assert_eq!(on.0.len(), 2, "both shooters were judged: {on:?}");
    let mut clamped_by_peer = BTreeMap::new();
    for (peer, j) in &on.0 {
        let judged = j.hits + j.misses;
        // A fire command lost with all its redundant copies is never judged; it is not a miss.
        assert!(
            judged * 100 >= SHOTS * 95,
            "peer {peer}: only {judged} of {SHOTS} judged"
        );
        assert_eq!(j.misses, 0, "peer {peer}: every judged shot hits: {j:?}");
        assert_eq!(j.body_hits, j.hits, "peer {peer}: on body");
        assert_eq!(
            j.rewound, judged,
            "peer {peer}: judged at the view tick from the wire"
        );
        let posed: Vec<(u32, Posed)> = views
            .posed
            .range((*peer, 0)..=(*peer, u32::MAX))
            .map(|((_, tick), p)| (*tick, *p))
            .collect();
        assert_eq!(
            posed.len() as u32,
            judged,
            "peer {peer}: one posed tick per judged shot"
        );
        let mut clamped = 0u32;
        for (tick, p) in posed {
            // The command's own view tick: what the client's interpolation showed when it
            // captured the fired tick, quantized as on the wire.
            let carried = views
                .carried
                .get(&(*peer, tick))
                .copied()
                .unwrap_or_else(|| panic!("peer {peer}: command tick {tick} carried no view"));
            // Spec 75 § 5 step 1: the view tick, clamped to the rewind window and the history.
            let floor = f64::from(p.current.saturating_sub(views.max_rewind).max(p.oldest));
            let want = carried.clamp(floor, f64::from(p.current));
            assert_eq!(
                p.at, want,
                "peer {peer}: the shot of command tick {tick} is posed at its command's view tick \
                 {carried}, clamped to [{floor}, {}]",
                p.current
            );
            clamped += u32::from(want != carried);
        }
        clamped_by_peer.insert(*peer, clamped);
    }
    clamped_by_peer
}

#[test]
fn n03_two_shooters_on_a_lossy_link_hit_where_they_saw_the_target() {
    let (on, fired, views) = two_shooters(true, None, 303);
    assert_eq!(fired, [SHOTS, SHOTS]);
    let clamped = judged_where_seen(&on, &views);
    // At 4 ticks each way plus the interpolation delay the carried view is older than the old
    // 250 ms default window, which clamped these shots to its edge. Spec 120 § 9.6 (ADR-8315): with
    // the default settings every shot is posed exactly at its carried view tick.
    println!("n03: default window, shots clamped per peer {clamped:?}");
    assert!(
        clamped.values().all(|c| *c == 0),
        "with the default window no shot is clamped: {clamped:?}"
    );
    // With a window wide enough for this link, every shot is posed exactly at its carried view tick.
    let (wide, wide_fired, wide_views) = two_shooters(true, Some(400.0), 303);
    assert_eq!(wide_fired, [SHOTS, SHOTS]);
    let wide_clamped = judged_where_seen(&wide, &wide_views);
    assert!(
        wide_clamped.values().all(|c| *c == 0),
        "with a 400 ms window no shot is clamped: {wide_clamped:?}"
    );
    assert_eq!(on, two_shooters(true, None, 303).0, "the same seed repeats");
    let (off, _, _) = two_shooters(false, None, 303);
    assert_eq!(
        off.0.len(),
        2,
        "both shooters were judged without rewind: {off:?}"
    );
    for (peer, j) in &off.0 {
        let judged = j.hits + j.misses;
        assert!(
            judged * 100 >= SHOTS * 95,
            "peer {peer}: without rewind only {judged} of {SHOTS} judged"
        );
        assert_eq!(j.rewound, 0, "peer {peer}: without rewind judged at the present");
        assert!(
            j.misses * 10 >= judged * 9,
            "peer {peer}: without rewind only {} of {judged} missed",
            j.misses
        );
    }
    println!("n03: with rewind {on:?}; without {off:?}");
}
