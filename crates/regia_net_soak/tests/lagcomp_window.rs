// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 9.6 (N210, ADR-8315): lag compensation through the real transport on the named
//! profiles, with `regia_net`'s default settings. Two shooters fire at a strafing target where
//! their interpolation showed it; the server judges each fired command with a rewound raycast.
//! Every judged shot must be posed exactly at the view tick its command carried (the N03 test of
//! `regia_net/tests/net_acceptance.rs`, on the time harness and with the default window this time),
//! and a 5 ms client that claims a 250 ms-old view must be clamped to its own expected age.
//!
//! Virtual time, seeded impairment, no wall clock.
// The time harness runs ECS worlds (feature `ecs`, on by default).
#![cfg(feature = "ecs")]

use std::collections::BTreeMap;

use glam::Vec3;
use regia_core::math::Ray;
use regia_ecs::prelude::*;
use regia_net::bridge::NetFrame;
use regia_net::interp::{
    NetInterpolation, install_interpolation, net_interpolate, view_tick_from_wire, view_tick_to_wire,
};
use regia_net::lagcomp::{install_lag_compensation, lagcomp_record, stats_report};
use regia_net::predict::ClientPrediction;
use regia_net::replication::{NetPredicted, register_replicated};
use regia_net::runtime::NetNode;
use regia_net::*;
use regia_net_soak::impair::{Direction, LinkSpec};
use regia_net_soak::profiles::Profile;
use regia_net_soak::timed::{TimedHub, TimedOptions};
use regia_scene::transform::Transform;

const HZ: f64 = 60.0;
const MSPT: f64 = 1000.0 / HZ;
const ORIGIN: Vec3 = Vec3::new(0.0, 1.0, -12.0);
const SHOTS: u32 = 100;
/// The profiles' runs are this long (the mobile-4g rebinding falls at 40 % of it).
const RUN_MS: u64 = 40_000;

struct Game;
impl NetGame for Game {
    type Message = NetFrame;
    type Input = u8;
    type JoinData = ();
    const GAME_ID: &'static str = "test.lagcomp_window";
    const PROTOCOL_VERSION: u32 = 1;
}

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

/// The strafing target (replicated so a client can find its proxy).
#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Strafer {
    lane: u8,
}

/// Client: fire from now on, and the shots fired so far.
#[derive(Resource, Default)]
struct Trigger {
    go: bool,
    fired: u32,
}

/// Server: per shooter peer, the shots judged.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct Tally(BTreeMap<u32, Judged>);

#[derive(Default, Debug, Clone, Copy, PartialEq)]
struct Judged {
    last_tick: u32,
    hits: u32,
    misses: u32,
}

/// Server: per `(shooter peer, command tick)`, the tick the shot was posed at and the present.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct PosedAt(BTreeMap<(u32, u32), (f64, u32)>);

/// Client: per fired command tick, the view tick the command carried.
#[derive(Resource, Default, Debug, Clone, PartialEq)]
struct CarriedView(BTreeMap<u32, f64>);

/// Triangle wave between -8 and 8 m at 6 m/s.
fn strafe_x(tick: u32) -> f32 {
    let leg = (16.0 / 6.0 * HZ) as u32;
    let phase = tick % (2 * leg);
    let s = if phase < leg { phase } else { 2 * leg - phase };
    -8.0 + 16.0 * s as f32 / leg as f32
}

fn server_strafe(world: &mut World) {
    let tick = world.resource::<NetNode>().tick.0;
    let mut q = world.query::<(&mut Transform, &Strafer)>();
    for (mut t, s) in q.iter_mut(world) {
        t.translation.x = strafe_x(tick);
        t.translation.z = f32::from(s.lane);
    }
}

/// Client gameplay: steps the interpolation clock, then fires every third command at the
/// target's shown center once triggered.
fn client_frame(world: &mut World) {
    if let Some(now) = world.get_resource::<NetInterpolation>().map(|n| n.now_ms) {
        net_interpolate(world, now);
    }
    let next = world
        .get_resource::<ClientPrediction>()
        .filter(|cp| cp.started)
        .map(|cp| cp.client_tick().0 + 1);
    let shown = {
        let mut q = world.query_filtered::<&Transform, With<Strafer>>();
        q.iter(world).next().map(|t| t.translation)
    };
    let mut pad = Pad::default();
    let mut trigger = world.resource_mut::<Trigger>();
    if let (Some(next), Some(at)) = (next, shown)
        && trigger.go
        && trigger.fired < SHOTS
        && next.is_multiple_of(3)
    {
        pad.fire = true;
        pad.aim = at + Vec3::Y;
        trigger.fired += 1;
    }
    let mut q = world.query::<&mut InputCommand<Pad>>();
    for mut c in q.iter_mut(world) {
        c.value = pad.clone();
    }
}

/// Predicted schedule: on the server each fired command becomes one rewound raycast; on a client
/// the first run of a fired tick records the view tick its command carries.
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
        let (hit, at, current) = {
            let Ok(lc) = LagCompensation::new(world) else {
                return;
            };
            let Ok(at) = lc.rewind_tick(shooter) else {
                continue;
            };
            let hit = lc
                .raycast(
                    shooter,
                    Ray::new(ORIGIN, aim - ORIGIN),
                    100.0,
                    &HitFilter::default(),
                )
                .ok()
                .flatten();
            (hit, at.0, lc.current_tick().0)
        };
        world
            .resource_mut::<PosedAt>()
            .0
            .insert((shooter.peer.0, tick), (at, current));
        let mut tally = world.resource_mut::<Tally>();
        let j = tally.0.entry(shooter.peer.0).or_default();
        j.last_tick = tick;
        if hit.is_some() {
            j.hits += 1;
        } else {
            j.misses += 1;
        }
    }
}

/// The game, with `regia_net`'s default settings.
fn setup(world: &mut World) {
    register_replicated::<Strafer>(world, ReplicateOptions::default());
    add_net_input::<Pad>(world).expect("one input type");
    predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
    add_predicted_systems(world, judge);
    world.init_resource::<Tally>();
    world.init_resource::<PosedAt>();
    world.init_resource::<CarriedView>();
    world.init_resource::<Trigger>();
    if world.resource::<NetMode>().is_server() {
        install_lag_compensation(world);
    } else {
        let every = world.resource::<NetSettings>().replication.send_every_ticks;
        install_interpolation(world, HZ, f64::from(every.max(1)));
    }
}

/// What one run produced, per client.
struct Run {
    hub: TimedHub,
    peers: Vec<PeerId>,
}

/// Connects one client per link, spawns each a predicted pawn and the strafing target, lets the
/// links settle for `settle_ms`, then has every client fire `SHOTS` shots, and drains.
fn run(seed: u64, links: Vec<LinkSpec>, settle_ms: u64, overrides: &[Option<f64>]) -> Run {
    let options = TimedOptions {
        seed,
        ..TimedOptions::default()
    };
    let mut hub = TimedHub::new::<Game>(options, setup).expect("server");
    for link in links {
        let c = hub.add_client::<Game>(link).expect("client");
        hub.client(c).gameplay.add_systems(client_frame);
    }
    let n = hub.client_count();
    assert!(
        hub.run_until(10_000, |h| (0..n).all(|c| h.client_peer(c).is_some())),
        "every client connects"
    );
    let peers: Vec<PeerId> = (0..n).map(|c| hub.client_peer(c).expect("peer")).collect();
    for (c, peer) in peers.iter().enumerate() {
        hub.server().world.spawn((
            Replicated {
                transform: TransformReplication::PositionOnly,
                ..Replicated::default()
            },
            Transform::from_translation(Vec3::new(0.0, 0.0, -20.0 - c as f32)),
            NetPredicted,
            NetOwner(*peer),
        ));
    }
    hub.server()
        .gameplay
        .add_systems((server_strafe, lagcomp_record).chain());
    hub.server().world.spawn((
        Strafer { lane: 0 },
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
    ));
    for (c, o) in overrides.iter().enumerate() {
        if let Some(ms) = o
            && let Some(mut ni) = hub.client(c).world.get_resource_mut::<NetInterpolation>()
        {
            ni.clock.set_delay_override(Some(*ms));
        }
    }
    hub.run_ms(settle_ms);
    for c in 0..n {
        hub.client(c).world.resource_mut::<Trigger>().go = true;
    }
    let done = hub.run_until(30_000, |h| {
        (0..n).all(|c| h.client_world(c).resource::<Trigger>().fired >= SHOTS)
    });
    assert!(done, "every client fired its shots");
    hub.run_ms(3_000);
    Run { hub, peers }
}

/// Per client: (shots judged, shots posed exactly at the carried view tick, hits, the largest
/// carried view age in ms); asserts every judged shot was posed exactly where its command's view
/// was.
fn posed_where_seen(name: &str, r: &Run) -> Vec<(u32, u32, u32, f64)> {
    let posed = &r.hub.server_world().resource::<PosedAt>().0;
    let tally = &r.hub.server_world().resource::<Tally>().0;
    let mut out = Vec::new();
    for (c, peer) in r.peers.iter().enumerate() {
        let carried = &r.hub.client_world(c).resource::<CarriedView>().0;
        let mut judged = 0u32;
        let mut exact = 0u32;
        let mut max_age = 0.0f64;
        let mut wrong = Vec::new();
        for (&(p, tick), &(at, current)) in posed.range((peer.0, 0)..=(peer.0, u32::MAX)) {
            debug_assert_eq!(p, peer.0);
            judged += 1;
            let Some(view) = carried.get(&tick) else {
                wrong.push(format!("command tick {tick} carried no view"));
                continue;
            };
            max_age = max_age.max((f64::from(current) - view) * MSPT);
            if at == *view {
                exact += 1;
            } else if wrong.len() < 5 {
                wrong.push(format!(
                    "command tick {tick}: posed at {at}, its view was {view} ({:.0} ms old; posed {:.0} ms back)",
                    (f64::from(current) - view) * MSPT,
                    (f64::from(current) - at) * MSPT
                ));
            }
        }
        let (hits, misses) = tally.get(&peer.0).map_or((0, 0), |j| (j.hits, j.misses));
        println!(
            "{name} client {c}: fired {SHOTS}, judged {judged}, exact {exact}, hits {hits}, misses {misses}, oldest view {max_age:.0} ms"
        );
        assert!(
            judged * 100 >= SHOTS * 90,
            "{name} client {c}: only {judged} of {SHOTS} shots judged"
        );
        assert_eq!(
            exact, judged,
            "{name} client {c}: every shot is posed at its command's view tick: {wrong:?}"
        );
        out.push((judged, exact, hits, max_age));
    }
    println!("{name}: {:?}", stats_report(r.hub.server_world()));
    out
}

fn profile_links(name: &str, clients: usize) -> Vec<LinkSpec> {
    let p = Profile::named(name).unwrap_or_else(|| panic!("profile {name}"));
    (0..clients).map(|c| p.client_link(c, RUN_MS)).collect()
}

#[test]
fn shots_are_posed_at_the_carried_view_tick_on_transatlantic() {
    let r = run(9601, profile_links("transatlantic", 2), 8_000, &[]);
    posed_where_seen("transatlantic", &r);
}

#[test]
fn shots_are_posed_at_the_carried_view_tick_on_mobile_4g() {
    let r = run(9602, profile_links("mobile-4g", 2), 8_000, &[]);
    posed_where_seen("mobile-4g", &r);
}

/// high-rtt: client 0 at 450 ms RTT, client 1 at 650 ms (spec 120 § 1.1's envelope).
#[test]
fn shots_are_posed_at_the_carried_view_tick_on_high_rtt() {
    let r = run(9603, profile_links("high-rtt", 2), 8_000, &[]);
    let rows = posed_where_seen("high-rtt", &r);
    assert!(
        rows[0].3 > 450.0,
        "the 450 ms client's views are older than 450 ms: {rows:?}"
    );
}

/// A 5 ms client whose interpolation shows (and whose commands carry) a view about 250 ms old,
/// with the interpolation delay to match (a fixed 210 ms), is posed far less far back: no further
/// than its own expected age (round trip + lead + the interpolation delay the server allows + one
/// tick) plus the allowance, under 200 ms. An honest client on the same link is posed exactly at
/// its view. (The per-peer clamp counters: `regia_net/tests/lagcomp_window.rs`.)
#[test]
fn a_5ms_client_claiming_a_250ms_old_view_is_clamped_to_its_own_expected_age() {
    let five_ms = Direction {
        latency_ms: 1,
        jitter_ms: 1,
        ..Direction::clean()
    };
    let link = LinkSpec {
        up: five_ms.clone(),
        down: five_ms,
        ..LinkSpec::default()
    };
    // 15 s: the server's bound on the interpolation delay comes down from 250 ms first.
    let r = run(9604, vec![link.clone(), link], 15_000, &[None, Some(210.0)]);
    let posed = &r.hub.server_world().resource::<PosedAt>().0;
    // The honest client.
    let honest_peer = r.peers[0];
    let carried = &r.hub.client_world(0).resource::<CarriedView>().0;
    let mut honest = 0;
    for (&(_, tick), &(at, _)) in posed.range((honest_peer.0, 0)..=(honest_peer.0, u32::MAX)) {
        assert_eq!(
            Some(&at),
            carried.get(&tick),
            "the honest client is posed at its view"
        );
        honest += 1;
    }
    assert!(honest * 100 >= SHOTS * 90, "honest client judged {honest}");
    // The forging client.
    let peer = r.peers[1];
    let carried = &r.hub.client_world(1).resource::<CarriedView>().0;
    let mut judged = 0;
    let mut clamped = 0;
    let mut worst = 0.0f64;
    for (&(_, tick), &(at, current)) in posed.range((peer.0, 0)..=(peer.0, u32::MAX)) {
        judged += 1;
        let view = carried.get(&tick).copied().expect("carried view");
        let claimed_ms = (f64::from(current) - view) * MSPT;
        let posed_ms = (f64::from(current) - at) * MSPT;
        assert!(
            (240.0..300.0).contains(&claimed_ms),
            "tick {tick}: the client claims a view about 250 ms old, got {claimed_ms:.0} ms"
        );
        assert!(
            posed_ms < 200.0,
            "tick {tick}: a 5 ms client is posed {posed_ms:.0} ms back for a {claimed_ms:.0} ms claim"
        );
        worst = worst.max(posed_ms);
        clamped += u32::from(at > view);
    }
    println!(
        "forging client: judged {judged}, clamped {clamped}, posed at most {worst:.1} ms back; {:?}",
        stats_report(r.hub.server_world())
    );
    assert!(judged * 100 >= SHOTS * 90, "forging client judged {judged}");
    assert_eq!(clamped, judged, "every forged view is clamped");
}
