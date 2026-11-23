// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.3 (ADR-10052 (1) as amended by ADR-10124; transform record revision 4): the far
//! tier's reduced record. A far-tier copy on the acknowledged state channel travels as a far
//! update (state record kind 3): the coarse position inside the baseline's cell, 40 bits or a
//! shorter residual, instead of the transform fields. The client shows the position within half a
//! coarse step (1.6 cm on x and z, 0.512 m on y at the 1 mm step); the exact transform comes back
//! with the first record after the copy leaves the tier, however records were lost, repeated or
//! reordered in between.
//!
//! Loopback hub, fixed steps, seeded walkers; no wall clock.

use std::collections::HashMap;

use glam::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_net::replication::{ClientReplication, NetVelocity, ServerReplication};
use regia_net::runtime::{Datagram, NetEntities};
use regia_net::testing::{LinkConditions, LoopbackHub, SplitMix64};
use regia_net::*;
use regia_scene::transform::Transform;

const TAG_STATE: u8 = 7;
const TAG_STATE_ENTROPY: u8 = 8;

/// Half a coarse step at the default 1 mm position step, plus the 0.5 mm of the position grid
/// and `f32` rounding at a kilometre: what a far copy may be off by on x and z, and on y.
const FAR_XZ_M: f32 = 0.016 + 0.0005 + 0.0002;
const FAR_Y_M: f32 = 0.512 + 0.0005 + 0.0002;
/// What an exact copy may be off by: half a position step and `f32` rounding.
const EXACT_M: f32 = 0.0005 + 0.0002;

/// Where the crowd walks: 800 m from the origin on x (the far tier starts at 600 m), away from the
/// position cells' borders at 0 and 1,048.576 m.
const CROWD: Vec3 = Vec3::new(800.0, 0.0, 300.0);

fn setup(world: &mut World) {
    let mut settings = world.resource_mut::<NetSettings>();
    settings.replication.state_channel = StateChannel::Unreliable;
    // The default cadence: 60 Hz ticks, a send every second tick.
    settings.replication.send_every_ticks = 2;
    settings.replication.client_rate_bytes = 50_000_000;
    settings.replication.spawns_per_send = 100_000;
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        // The default tiers: near below 150 m, mid below 600 m (every third send), far at 2 Hz.
        let tiered = NetProfile {
            relevance: RelevanceShape::Spatial {
                radius: 4000.0,
                hysteresis: 0.15,
            },
            linger_ticks: 0,
            rate_tiers: Some(RateTiers::default()),
            ..NetProfile::default()
        };
        world
            .resource_mut::<NetProfiles>()
            .insert("tiered", tiered)
            .expect("profile");
        world.resource_mut::<NetProfiles>().set_default("tiered");
    }
}

struct Walker {
    entity: Entity,
    heading: f32,
    speed: f32,
    turn_in: u32,
}

/// How the crowd moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gait {
    /// Straight lines at a constant pace.
    Straight,
    /// A new heading and pace every one to three seconds.
    Turning,
    /// A new heading every 10 to 40 ticks, and a slow bob on y: nothing the velocity predicts.
    Milling,
}

struct Crowd {
    hub: LoopbackHub,
    viewers: Vec<Entity>,
    walkers: Vec<Walker>,
    rng: SplitMix64,
    dt: f32,
    gait: Gait,
    moving: bool,
    /// Server positions by server tick.
    history: HashMap<u32, Vec<Vec3>>,
}

impl Crowd {
    /// `clients` viewers at `viewer_at(i)` and `walkers` walkers in a 90 m square around `CROWD`.
    fn new(
        seed: u64,
        link: LinkConditions,
        clients: usize,
        walkers: usize,
        gait: Gait,
        viewer_at: impl Fn(usize) -> Vec3,
    ) -> Self {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
        hub.up = link;
        hub.down = link;
        let mut viewers = Vec::new();
        for i in 0..clients {
            let at = hub.add_client();
            let peer = hub.clients[at].peer;
            viewers.push(
                hub.server
                    .world
                    .spawn((
                        NetViewpoint {
                            player: NetPlayerId { peer, local: 0 },
                        },
                        Transform::from_translation(viewer_at(i)),
                    ))
                    .id(),
            );
        }
        let mut rng = SplitMix64::new(seed ^ 0x5eed);
        let mut list = Vec::with_capacity(walkers);
        for i in 0..walkers {
            let p = CROWD
                + Vec3::new(
                    -45.0 + (i % 16) as f32 * 6.0 + 0.4,
                    1.5 + (i % 3) as f32 * 0.25,
                    -45.0 + (i / 16) as f32 * 6.0 + 0.3,
                );
            let heading = (rng.next_f64() as f32) * std::f32::consts::TAU;
            let speed = 1.2 + (rng.next_f64() as f32) * 0.4;
            let entity = hub
                .server
                .world
                .spawn((
                    Replicated::default(),
                    Transform {
                        translation: p,
                        rotation: Quat::from_rotation_y(heading),
                        ..Transform::default()
                    },
                    NetVelocity(Vec3::new(heading.sin(), 0.0, heading.cos()) * speed),
                ))
                .id();
            let turn_in = 60 + (rng.next_u64() % 120) as u32;
            list.push(Walker {
                entity,
                heading,
                speed,
                turn_in,
            });
        }
        let dt = (1.0 / regia_net::runtime::tick_hz(&hub.server.world)) as f32;
        Self {
            hub,
            viewers,
            walkers: list,
            rng,
            dt,
            gait,
            moving: true,
            history: HashMap::new(),
        }
    }

    fn set_far_records(&mut self, on: bool) {
        self.hub
            .server
            .world
            .resource_mut::<ServerReplication>()
            .set_far_records(on);
    }

    fn move_viewer(&mut self, i: usize, to: Vec3) {
        if let Some(mut t) = self.hub.server.world.get_mut::<Transform>(self.viewers[i]) {
            t.translation = to;
        }
    }

    fn step(&mut self) {
        let tick = self.hub.server_tick().0;
        for w in &mut self.walkers {
            if !self.moving {
                break;
            }
            let (lo, span): (u32, u32) = match self.gait {
                Gait::Straight => (u32::MAX, 1),
                Gait::Turning => (60, 120),
                Gait::Milling => (10, 30),
            };
            w.turn_in = w.turn_in.saturating_sub(1);
            if w.turn_in == 0 && self.gait != Gait::Straight {
                w.heading += (self.rng.next_f64() as f32 - 0.5) * 2.0;
                w.speed = 1.2 + (self.rng.next_f64() as f32) * 0.4;
                w.turn_in = lo + (self.rng.next_u64() % u64::from(span)) as u32;
            }
            let mut v = Vec3::new(w.heading.sin(), 0.0, w.heading.cos()) * w.speed;
            if self.gait == Gait::Milling {
                v.y = 0.3 * ((tick as f32) * 0.05 + w.heading).sin();
            }
            let world = &mut self.hub.server.world;
            if let Some(mut t) = world.get_mut::<Transform>(w.entity) {
                t.translation += v * self.dt;
                t.rotation = Quat::from_rotation_y(w.heading);
            }
            if let Some(mut nv) = world.get_mut::<NetVelocity>(w.entity) {
                nv.0 = v;
            }
        }
        let now = self.server_positions();
        self.hub.run(1);
        self.history.insert(tick, now);
    }

    fn run(&mut self, ticks: usize) {
        for _ in 0..ticks {
            self.step();
        }
    }

    /// Every walker stands still from now on (its velocity is zero).
    fn stop(&mut self) {
        self.moving = false;
        for w in &self.walkers {
            if let Some(mut nv) = self.hub.server.world.get_mut::<NetVelocity>(w.entity) {
                nv.0 = Vec3::ZERO;
            }
        }
    }

    fn server_positions(&self) -> Vec<Vec3> {
        self.walkers
            .iter()
            .map(|w| {
                self.hub
                    .server
                    .world
                    .get::<Transform>(w.entity)
                    .map_or(Vec3::ZERO, |t| t.translation)
            })
            .collect()
    }

    fn copy(&self, client: usize, e: Entity) -> Option<Entity> {
        let id = *self.hub.server.world.get::<NetId>(e)?;
        self.hub.clients[client]
            .world
            .resource::<NetEntities>()
            .entity(id)
    }

    fn shown(&self, client: usize, e: Entity) -> Transform {
        let copy = self.copy(client, e).expect("the walker has a copy");
        *self.hub.clients[client]
            .world
            .get::<Transform>(copy)
            .expect("the copy has a transform")
    }

    fn state_counters(&self, client: usize) -> regia_net::replication::StateCounters {
        self.hub
            .server
            .world
            .resource::<ServerReplication>()
            .state_link(self.hub.clients[client].peer)
            .expect("a state link")
            .counters
    }

    fn client_counters(&self, client: usize) -> regia_net::replication::ClientStateCounters {
        self.hub.clients[client]
            .world
            .resource::<ClientReplication>()
            .state
            .counters
    }

    /// State-frame bytes and records every client received from log position `from` on.
    fn received(&self, from: &[usize]) -> (usize, u64) {
        let (mut bytes, mut records) = (0usize, 0u64);
        for (log, start) in self.hub.client_logs.iter().zip(from) {
            for (_, d) in &log[*start..] {
                if matches!(d.bytes.first().copied(), Some(TAG_STATE | TAG_STATE_ENTROPY)) {
                    bytes += d.bytes.len();
                    records += record_count(d);
                }
            }
        }
        (bytes, records)
    }

    fn log_marks(&self) -> Vec<usize> {
        self.hub.client_logs.iter().map(Vec::len).collect()
    }
}

/// The record count of a `STATE` frame: `tag u8 | tick varint | seq varint | count varint`.
fn record_count(d: &Datagram) -> u64 {
    let mut r = regia_net::bits::BitReader::new(&d.bytes);
    r.read_bits(8).expect("tag");
    r.read_varint().expect("tick");
    r.read_varint().expect("seq");
    r.read_varint().expect("count")
}

fn rtt_100ms() -> LinkConditions {
    LinkConditions {
        delay_ticks: 3,
        ..LinkConditions::default()
    }
}

fn within(a: Vec3, b: Vec3, xz: f32, y: f32) -> bool {
    (a.x - b.x).abs() <= xz && (a.z - b.z).abs() <= xz && (a.y - b.y).abs() <= y
}

/// A far crowd over a 100 ms round trip: its updates travel as far records; every copy shows a
/// position within half a coarse step of where the server had the walker at one of the last
/// ticks; a crowd that stops goes silent; and when the viewer walks up to it, every copy gets its
/// exact position and rotation back with one record each, and the channel goes silent again.
#[test]
fn a_far_crowd_travels_as_far_records_and_comes_back_exact() {
    let mut c = Crowd::new(0x1_0124, rtt_100ms(), 1, 64, Gait::Turning, |_| Vec3::ZERO);
    // Every walker has turned at least once after 3 s: from its first turn on, its far record is
    // more than a byte shorter than its exact one.
    c.run(240);
    assert!(
        c.walkers.iter().all(|w| c.copy(0, w.entity).is_some()),
        "every walker spawned on the client"
    );
    let before = c.state_counters(0);
    let marks = c.log_marks();
    // The far interval is 30 ticks and a record takes the delay to land: a copy shows the
    // position of a tick at most this far back.
    const REACH: u32 = 30 + 2 + 3 + 6;
    let mut checked = 0u64;
    for _ in 0..600 {
        c.step();
        let tick = c.hub.server_tick().0;
        for (i, w) in c.walkers.iter().enumerate() {
            let shown = c.shown(0, w.entity).translation;
            let hit = (0..=REACH).any(|ago| {
                tick.checked_sub(ago)
                    .and_then(|t| c.history.get(&t))
                    .and_then(|h| h.get(i))
                    .is_some_and(|at| within(shown, *at, FAR_XZ_M, FAR_Y_M))
            });
            assert!(
                hit,
                "walker {i} at tick {tick}: shown {shown} is no far position of the last {REACH} ticks"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 600 * 64);
    let after = c.state_counters(0);
    let (_, records) = c.received(&marks);
    let far = after.far_records - before.far_records;
    eprintln!("far crowd: {records} records in 600 ticks, {far} far records");
    // 64 walkers at 2 Hz for 10 s.
    assert!(
        (1100..=1400).contains(&records),
        "the far tier sends at 2 Hz: {records} records"
    );
    assert!(
        far * 10 >= records * 9,
        "far updates travel as far records: {far} of {records}"
    );
    assert_eq!(c.client_counters(0).record_errors, 0);
    assert_eq!(c.client_counters(0).baseline_missing, 0);

    // The crowd stops: the last far records land, then nothing is sent while the copies wait out
    // the settle time in their far form (two seconds from the stop, ADR-10199; the test
    // `a_far_crowd_at_rest_settles_exact_and_leaves_the_walk` follows a crowd past it).
    c.stop();
    c.run(60);
    let marks = c.log_marks();
    let quiet = c.state_counters(0);
    c.run(30);
    let (_, records) = c.received(&marks);
    assert_eq!(records, 0, "a far crowd that just stopped sends nothing");
    assert_eq!(c.state_counters(0).far_records, quiet.far_records);
    let truth = c.server_positions();
    let mut off_grid = 0;
    for (w, at) in c.walkers.iter().zip(&truth) {
        let shown = c.shown(0, w.entity).translation;
        assert!(within(shown, *at, FAR_XZ_M, FAR_Y_M), "{shown} vs {at}");
        off_grid += u32::from(!within(shown, *at, EXACT_M, EXACT_M));
    }
    assert!(
        off_grid > 32,
        "the copies hold coarse positions: {off_grid} of 64"
    );

    // The viewer walks up to the crowd before it settles: nothing changed on the server, and
    // still every copy gets its exact state, once.
    c.move_viewer(0, CROWD);
    let marks = c.log_marks();
    c.run(60);
    let (_, records) = c.received(&marks);
    assert_eq!(records, 64, "one exact record per copy");
    for (w, at) in c.walkers.iter().zip(&truth) {
        let shown = c.shown(0, w.entity);
        assert!(
            within(shown.translation, *at, EXACT_M, EXACT_M),
            "exact again: {} vs {at}",
            shown.translation
        );
        let server = c.hub.server.world.get::<Transform>(w.entity).expect("walker");
        assert!(
            shown.rotation.angle_between(server.rotation).to_degrees() < 0.2,
            "the rotation came back"
        );
    }
    let marks = c.log_marks();
    c.run(120);
    assert_eq!(c.received(&marks).1, 0, "and nothing after it");
    assert_eq!(c.client_counters(0).record_errors, 0);
    assert_eq!(c.client_counters(0).baseline_missing, 0);
}

/// ADR-10199: a far crowd that comes to rest settles. While a copy is held in its far form the
/// gather walks it and its priority pass reads it on every send tick; two seconds after the
/// entity's last change the copy gets its exact transform, once, still in the far tier, and from
/// then on no gather walks a copy of the crowd and nothing is sent, also when the viewer walks
/// up. With far records off the crowd at rest is walked by no gather at all: that is the bar the
/// settled crowd meets.
#[test]
fn a_far_crowd_at_rest_settles_exact_and_leaves_the_walk() {
    /// The most copies a gather walked, and the most packed slots its priority pass read, over
    /// four ticks (two sends).
    fn walk(c: &mut Crowd) -> (u64, u64) {
        let mut most = (0, 0);
        for _ in 0..4 {
            c.step();
            let rep = c.hub.server.world.resource::<ServerReplication>();
            most = (most.0.max(rep.copies_visited()), most.1.max(rep.priority_slots()));
        }
        most
    }
    for far_on in [false, true] {
        let mut c = Crowd::new(0x1_0199, rtt_100ms(), 1, 64, Gait::Turning, |_| Vec3::ZERO);
        c.set_far_records(far_on);
        c.run(480);
        let far_walking = c.state_counters(0).far_records;
        assert_eq!(
            far_walking > 400,
            far_on,
            "far records of the walk: {far_walking}"
        );
        c.stop();
        // A second on, the last records of the walk have landed and are acknowledged.
        c.run(60);
        let truth = c.server_positions();
        let off_grid = c
            .walkers
            .iter()
            .zip(&truth)
            .filter(|(w, at)| !within(c.shown(0, w.entity).translation, **at, EXACT_M, EXACT_M))
            .count();
        let (visited, slots) = walk(&mut c);
        eprintln!(
            "far records {far_on}, a second after the stop: {off_grid} coarse copies, a gather walks {visited} copies, \
             priority reads {slots} slots"
        );
        if far_on {
            assert!(
                off_grid > 32,
                "the copies hold coarse positions: {off_grid} of 64"
            );
            assert!(
                visited > 32 && slots > 32,
                "a copy held in its far form is walked: {visited} copies, {slots} slots"
            );
        } else {
            assert_eq!(off_grid, 0, "exact records: every copy is exact");
            assert_eq!((visited, slots), (0, 0), "an exact crowd at rest is not walked");
        }
        // Past the settle time: two seconds from the stop, a send and the round trip.
        let marks = c.log_marks();
        let before = c.state_counters(0);
        c.run(120);
        let (_, records) = c.received(&marks);
        assert_eq!(
            records,
            if far_on { 64 } else { 0 },
            "far records {far_on}: one exact record per copy that was held in its far form"
        );
        assert_eq!(c.state_counters(0).far_records, before.far_records);
        for (i, (w, at)) in c.walkers.iter().zip(&truth).enumerate() {
            let shown = c.shown(0, w.entity);
            assert!(
                within(shown.translation, *at, EXACT_M, EXACT_M),
                "far records {far_on} walker {i}: settled exact, {} vs {at}",
                shown.translation
            );
            let server = c.hub.server.world.get::<Transform>(w.entity).expect("walker");
            assert!(
                shown.rotation.angle_between(server.rotation).to_degrees() < 0.2,
                "far records {far_on} walker {i}: the rotation settled"
            );
        }
        assert_eq!(
            walk(&mut c),
            (0, 0),
            "far records {far_on}: a settled far crowd is not walked"
        );
        let marks = c.log_marks();
        c.run(120);
        assert_eq!(c.received(&marks).1, 0, "far records {far_on}: silent at rest");
        // Nothing is owed to a settled copy that comes near.
        c.move_viewer(0, CROWD);
        let marks = c.log_marks();
        c.run(60);
        assert_eq!(
            c.received(&marks).1,
            0,
            "far records {far_on}: nothing is sent when the viewer walks up"
        );
        let counters = c.client_counters(0);
        assert_eq!((counters.record_errors, counters.baseline_missing), (0, 0));
    }
}

/// Soundness under a bad link: 20 % loss, reordering and duplicates both ways, while the viewer
/// jumps between the crowd and 800 m away every 2.5 s so copies enter and leave the far tier with
/// records of both forms on their way. No record ever fails to decode (the server never codes a
/// transform delta against a far version), and once the link is clean and the viewer near, every
/// copy holds the server's exact transform.
#[test]
fn far_and_exact_records_mix_soundly_under_loss_reorder_and_duplication() {
    let bad = LinkConditions {
        loss: 0.2,
        delay_ticks: 2,
        duplicate: 0.1,
        reorder: 0.25,
        ..LinkConditions::default()
    };
    let mut far_total = 0u64;
    for seed in [0x51, 0x52, 0x53] {
        far_total += mix_run(seed, bad, true, |_| 150);
    }
    // And a crowd that stops on the bad link and settles without the viewer coming near.
    far_total += mix_run(0x54, bad, false, |_| 150);
    assert!(far_total > 500, "far records were part of the mix: {far_total}");
}

/// One run of the mix: 48 milling walkers and one viewer that flaps between the far tier and the
/// crowd for eight phases of `phase_ticks(phase)` ticks on the link `bad`; then far once more
/// and the crowd stops. With `walk_up` the link is clean from the stop on and the viewer walks up
/// (the tier exit restores the exact transforms); without it the viewer stays far and the crowd
/// settles (ADR-10199), its first 150 ticks at rest still on the bad link. Asserts that no record
/// failed to decode, that every copy then shows the server's exact transform, and that nothing is
/// sent after that. Returns the far records sent during the flapping.
fn mix_run(seed: u64, bad: LinkConditions, walk_up: bool, phase_ticks: impl Fn(usize) -> usize) -> u64 {
    let mut c = Crowd::new(seed, bad, 1, 48, Gait::Milling, |_| Vec3::ZERO);
    for phase in 0..8 {
        c.move_viewer(0, if phase % 2 == 0 { Vec3::ZERO } else { CROWD });
        c.run(phase_ticks(phase));
    }
    let far = c.state_counters(0).far_records;
    // Far once more, then stop; clean the link and walk up, or stay far while the crowd settles.
    c.move_viewer(0, Vec3::ZERO);
    c.run(150);
    c.stop();
    if !walk_up {
        c.run(150);
    }
    c.hub.up = LinkConditions::default();
    c.hub.down = LinkConditions::default();
    if walk_up {
        c.move_viewer(0, CROWD);
    }
    c.run(if walk_up { 240 } else { 360 });
    let counters = c.client_counters(0);
    assert_eq!(
        counters.record_errors, 0,
        "seed {seed:#x} (walk up: {walk_up}) on {bad:?}: a record failed to decode: {counters:?}"
    );
    let truth = c.server_positions();
    for (i, (w, at)) in c.walkers.iter().zip(&truth).enumerate() {
        let shown = c.shown(0, w.entity);
        assert!(
            within(shown.translation, *at, EXACT_M, EXACT_M),
            "seed {seed:#x} (walk up: {walk_up}) on {bad:?} walker {i}: {} vs {at}",
            shown.translation
        );
        let server = c.hub.server.world.get::<Transform>(w.entity).expect("walker");
        assert!(
            shown.rotation.angle_between(server.rotation).to_degrees() < 0.2,
            "seed {seed:#x} (walk up: {walk_up}) on {bad:?} walker {i}: rotation"
        );
    }
    // And silent: no copy is still owed anything.
    let marks = c.log_marks();
    c.run(120);
    assert_eq!(
        c.received(&marks).1,
        0,
        "seed {seed:#x} (walk up: {walk_up}) on {bad:?}: records after the crowd settled"
    );
    far
}

/// The same mix over a sweep of seeds (64, or `REGIA_FAR_MIX_SEEDS`), each with its own link (5 to
/// 35 % loss, 0 to 6 ticks of delay, up to 30 % duplicates, up to 50 % reordering) and its own
/// flap lengths (12 to 200 ticks a phase, so a tier change lands inside a record's flight as often
/// as after its acknowledgment); every other seed ends with the crowd settling far instead of the
/// viewer walking up. The bars are `mix_run`'s. 3,000 seeds ran clean when ADR-10199 was built.
#[test]
fn far_and_exact_records_mix_soundly_over_a_seed_sweep() {
    let seeds = std::env::var("REGIA_FAR_MIX_SEEDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(64);
    let (mut far_total, mut far_runs) = (0u64, 0u64);
    for n in 0..seeds {
        let seed = 0x1000 + n;
        let mut rng = SplitMix64::new(seed ^ 0xfa2_71e2);
        let bad = LinkConditions {
            loss: 0.05 + rng.next_f64() * 0.30,
            delay_ticks: rng.next_u64() % 7,
            duplicate: rng.next_f64() * 0.30,
            reorder: rng.next_f64() * 0.50,
            ..LinkConditions::default()
        };
        let lengths: [usize; 8] = std::array::from_fn(|_| 12 + (rng.next_u64() % 189) as usize);
        // Every other seed settles far instead of walking up.
        let far = mix_run(seed, bad, n % 2 == 0, |phase| lengths[phase]);
        far_total += far;
        far_runs += u64::from(far > 0);
    }
    println!("far-mix sweep: {seeds} seeds, {far_total} far records, {far_runs} runs with far records");
    assert!(
        far_runs * 2 >= seeds,
        "far records were part of at least half the runs: {far_runs} of {seeds}"
    );
}

/// A walker that crosses a position cell's border (1,048.576 m) while far: the record that
/// crosses is the exact one (a far record names no cell), far records follow in the new cell, and
/// the shown position never leaves the far bound.
#[test]
fn a_cell_crossing_takes_the_exact_record() {
    let mut c = Crowd::new(0x77, rtt_100ms(), 1, 1, Gait::Straight, |_| Vec3::ZERO);
    let e = c.walkers[0].entity;
    // The test moves the walker itself: +x at 1.5 m/s from 3 m short of the border, weaving on z
    // so its heading and velocity change every tick (its exact record is long).
    c.moving = false;
    let mut at = Vec3::new(1045.5, 1.5, 300.0);
    let mut drive = |c: &mut Crowd, t: u32| {
        let v = Vec3::new(1.5, 0.0, 0.8 * (t as f32 * 0.21).sin());
        at += v / 60.0;
        let world = &mut c.hub.server.world;
        if let Some(mut tr) = world.get_mut::<Transform>(e) {
            tr.translation = at;
            tr.rotation = Quat::from_rotation_y(v.x.atan2(v.z));
        }
        if let Some(mut nv) = world.get_mut::<NetVelocity>(e) {
            nv.0 = v;
        }
        c.step();
    };
    for t in 0..60 {
        drive(&mut c, t);
    }
    let start = c.state_counters(0);
    let marks = c.log_marks();
    for t in 60..420 {
        drive(&mut c, t);
        let tick = c.hub.server_tick().0;
        let shown = c.shown(0, e).translation;
        let hit = (0..=41u32).any(|ago| {
            tick.checked_sub(ago)
                .and_then(|t| c.history.get(&t))
                .is_some_and(|h| within(shown, h[0], FAR_XZ_M, FAR_Y_M))
        });
        assert!(hit, "tick {tick}: shown {shown}");
    }
    let at = c.server_positions()[0];
    assert!(at.x > 1050.0, "the walker crossed the border: {at}");
    let end = c.state_counters(0);
    let (_, records) = c.received(&marks);
    let far = end.far_records - start.far_records;
    assert!((11..=13).contains(&records), "2 Hz for 6 s: {records}");
    assert!(
        far >= records - 2,
        "{far} far records of {records}: only the crossing is exact"
    );
    assert!(far < records, "the crossing itself is not a far record");
    assert_eq!(c.client_counters(0).record_errors, 0);
}

/// With the far record switched off (`ServerReplication::set_far_records(false)`) the far tier
/// sends its exact record at the far rate, as before revision 4: no far record, and a stopped far
/// copy is exact.
#[test]
fn the_switch_keeps_exact_records_in_the_far_tier() {
    let mut c = Crowd::new(0x1_0124, rtt_100ms(), 1, 32, Gait::Turning, |_| Vec3::ZERO);
    c.set_far_records(false);
    c.run(400);
    c.stop();
    c.run(120);
    assert_eq!(c.state_counters(0).far_records, 0);
    for (w, at) in c.walkers.iter().zip(c.server_positions()) {
        let shown = c.shown(0, w.entity).translation;
        assert!(within(shown, at, EXACT_M, EXACT_M), "{shown} vs {at}");
    }
    // Switched on mid-session and off again: the copies end exact.
    c.moving = true;
    c.set_far_records(true);
    c.run(300);
    assert!(c.state_counters(0).far_records > 0);
    c.set_far_records(false);
    c.stop();
    c.run(180);
    for (w, at) in c.walkers.iter().zip(c.server_positions()) {
        let shown = c.shown(0, w.entity).translation;
        assert!(
            within(shown, at, EXACT_M, EXACT_M),
            "after the switch: {shown} vs {at}"
        );
    }
    assert_eq!(c.client_counters(0).record_errors, 0);
}

/// The most a turning or milling all-far crowd's record bytes may be with the far record, in
/// percent of the bytes without it (`docs/perf/net-far-record.md` has the measured rows).
const FAR_SAVING_BAR_PERCENT: usize = 90;

/// One row of the bandwidth table.
#[derive(Debug, Clone, Copy)]
struct Row {
    bytes: usize,
    records: u64,
    far: u64,
}

/// 40 clients and 128 walkers for 10 s after a 3 s warm-up; `far_on` is the switch.
fn bandwidth(gait: Gait, far_on: bool, viewer_at: impl Fn(usize) -> Vec3) -> Row {
    let mut c = Crowd::new(0xb17e, rtt_100ms(), 40, 128, gait, viewer_at);
    c.set_far_records(far_on);
    c.run(180);
    let marks = c.log_marks();
    let before: u64 = (0..40).map(|i| c.state_counters(i).far_records).sum();
    c.run(600);
    let (bytes, records) = c.received(&marks);
    let far = (0..40).map(|i| c.state_counters(i).far_records).sum::<u64>() - before;
    for i in 0..40 {
        assert_eq!(c.client_counters(i).record_errors, 0, "client {i}");
    }
    Row { bytes, records, far }
}

/// The bandwidth of the far tier with and without the far record, 40 clients on a 128-walker
/// crowd (`docs/perf/net-far-record.md`). All viewers far: every update is a far-tier update.
/// Mixed: viewers on a line through the crowd, so each sees near, mid and far walkers.
///
/// Bars: the far record never costs bytes on any row, and on the crowds whose motion the velocity
/// does not predict (turning, milling) the far tier's record bytes fall to
/// `FAR_SAVING_BAR_PERCENT` percent or less.
#[test]
fn bandwidth_of_the_far_tier_with_and_without_the_far_record() {
    let all_far = |i: usize| Vec3::new(-20.0 + i as f32, 0.0, (i % 5) as f32 * 8.0);
    let mixed = |i: usize| CROWD + Vec3::new(-780.0 + i as f32 * 40.0, 0.0, 0.0);
    // The empty frames a client gets anyway: one per send tick.
    let floor = {
        let mut c = Crowd::new(0xb17e, rtt_100ms(), 40, 0, Gait::Straight, all_far);
        c.run(180);
        let marks = c.log_marks();
        c.run(600);
        c.received(&marks).0
    };
    eprintln!(
        "| crowd | viewers | far record | state bytes/s per client | of which records | records/s per client | far records | bytes per record |"
    );
    eprintln!("|---|---|---|---|---|---|---|---|");
    let mut rows = Vec::new();
    for (gait, name) in [
        (Gait::Straight, "straight"),
        (Gait::Turning, "turning"),
        (Gait::Milling, "milling"),
    ] {
        for (place, viewers) in [("all far", 0), ("mixed", 1)] {
            let run = |on: bool| {
                if viewers == 0 {
                    bandwidth(gait, on, all_far)
                } else {
                    bandwidth(gait, on, mixed)
                }
            };
            let (off, on) = (run(false), run(true));
            for (label, r) in [("off", off), ("on", on)] {
                let per_s = r.bytes as f64 / 40.0 / 10.0;
                let payload = (r.bytes - floor) as f64;
                eprintln!(
                    "| {name} | {place} | {label} | {per_s:.0} | {:.0} | {:.1} | {} | {:.2} |",
                    payload / 40.0 / 10.0,
                    r.records as f64 / 40.0 / 10.0,
                    r.far,
                    payload / r.records.max(1) as f64,
                );
            }
            rows.push((name, place, gait, off, on));
        }
    }
    eprintln!(
        "empty-frame floor: {:.0} bytes/s per client",
        floor as f64 / 40.0 / 10.0
    );
    for (name, place, gait, off, on) in rows {
        assert_eq!(off.far, 0, "{name}, {place}: the switch is off");
        // A walker on a straight line stays exact: its velocity-predicted delta is as short as
        // the far record, which must be a byte shorter to be taken.
        if gait != Gait::Straight {
            assert!(on.far > 0, "{name}, {place}: far records were sent");
        }
        assert!(
            on.bytes <= off.bytes,
            "{name}, {place}: the far record costs bytes: {} vs {}",
            on.bytes,
            off.bytes
        );
        if gait != Gait::Straight && place == "all far" {
            let (off_p, on_p) = (off.bytes - floor, on.bytes - floor);
            assert!(
                on_p * 100 <= off_p * FAR_SAVING_BAR_PERCENT,
                "{name}, {place}: record bytes with the far record {on_p}, without {off_p}"
            );
        }
    }
}

/// The handshake row (spec 120 § 12.1): the far record is transform record revision 4, which the
/// protocol hash carries, so a build without it is refused with `protocol_mismatch` (naming
/// `regia_net::replication::transform_record`) instead of reading kind 3 as a bad record.
#[test]
fn the_far_record_is_transform_record_revision_4_in_the_protocol_hash() {
    use regia_net::codec::LayoutHasher;
    use regia_net::replication::{
        FAR_RECORD_BITS, FAR_XZ_SHIFT, FAR_Y_SHIFT, POSITION_CELL_BITS, TRANSFORM_RECORD_REVISION,
        TRANSFORM_RECORD_TYPE,
    };
    assert_eq!(TRANSFORM_RECORD_REVISION, 4);
    assert_eq!(FAR_RECORD_BITS, 40, "the 5-byte record");
    assert_eq!(
        2 * (POSITION_CELL_BITS - FAR_XZ_SHIFT) + (POSITION_CELL_BITS - FAR_Y_SHIFT),
        40,
        "x u15 | z u15 | y u10"
    );
    let hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    let world = &hub.server.world;
    let types = regia_net::runtime::protocol_types(world);
    assert!(
        types.contains(&(TRANSFORM_RECORD_TYPE.to_owned(), 4)),
        "the protocol types name the record's revision: {types:?}"
    );
    // The hash of a build at revision 3 (ADR-8522), computed the way `protocol_hash` does for a
    // world with nothing registered, differs: such a peer is refused at the handshake.
    let hash_at = |revision: u64| {
        let mut h = LayoutHasher::new();
        h.write_u64(regia_net::session::proto::SESSION_WIRE_REVISION);
        h.write_str(TRANSFORM_RECORD_TYPE);
        h.write_u64(revision);
        h.finish()
    };
    assert_ne!(hash_at(3), hash_at(4));
    assert_eq!(
        regia_net::REGIA_NET_WIRE_VERSION,
        6,
        "the far record rides wire 6"
    );
}

/// The state-frame fuzz corpus (`net.state_frame`) ends with three hand-built frames: an update
/// that puts a copy on the position grid, then a far update in the absolute and one in the
/// residual form. All apply to the fuzz client after the recorded session, and the copy ends at
/// the coarse position they name.
#[test]
fn the_state_frame_fuzz_corpus_seeds_both_far_forms() {
    use regia_net::testing::state_frame_fuzz as fuzz;
    let frames = fuzz::recorded();
    assert!(frames.len() > fuzz::FAR_SEEDS);
    let (session, far) = frames.split_at(frames.len() - fuzz::FAR_SEEDS);
    let mut world = fuzz::scratch_world();
    for f in session {
        fuzz::apply(&mut world, f).expect("a recorded frame");
    }
    let before = world.resource::<ClientReplication>().state.counters;
    for f in far {
        assert_eq!(f[0], TAG_STATE);
        fuzz::apply(&mut world, f).expect("a far seed");
    }
    let after = world.resource::<ClientReplication>().state.counters;
    assert_eq!(
        after.applied - before.applied,
        3,
        "the grid update and both far updates applied: {after:?}"
    );
    assert_eq!(after.record_errors, before.record_errors);
    assert_eq!(after.baseline_missing, before.baseline_missing);
    // x: coarse 100 + 3 = 103 x 32 mm; z: 300 x 32 mm; y: 2 x 1,024 mm.
    let want = Vec3::new(3.296, 2.048, 9.6);
    let mut q = world.query::<&Transform>();
    assert!(
        q.iter(&world).any(|t| t.translation.distance(want) < 1e-4),
        "a copy sits at the far position {want}"
    );
    // The mutation loop runs over them like over any recorded frame.
    let report = fuzz::run(0x1_0124, 400).unwrap_or_else(|f| panic!("{}", f.message));
    assert!(report.applied + report.refused == 400);
}

/// Review proof: entering the far form drops the previously replicated velocity, including
/// the component visible to gameplay/animation, rather than retaining the spawn's velocity.
#[test]
fn review_far_record_clears_the_previously_replicated_velocity() {
    let mut c = Crowd::new(0x1_0124, rtt_100ms(), 1, 1, Gait::Turning, |_| Vec3::ZERO);
    c.run(400);
    assert!(
        c.state_counters(0).far_records > 0,
        "the copy entered the far form"
    );
    c.stop();
    c.run(120);
    assert_eq!(c.client_counters(0).record_errors, 0);
    let copy = c.copy(0, c.walkers[0].entity).expect("copy");
    assert!(
        c.hub.clients[0].world.get::<NetVelocity>(copy).is_none(),
        "a far copy must not expose the stale exact-record velocity: {:?}",
        c.hub.clients[0].world.get::<NetVelocity>(copy)
    );
}

/// The same review finding at the presentation seam: sampling velocity-less far snapshots
/// must remove the exact snapshot's old velocity component.
#[test]
fn review_interpolated_far_record_clears_the_previously_replicated_velocity() {
    use regia_net::interp::{NetInterpolation, NetSnapshotBuffer, install_interpolation, net_interpolate};

    let mut c = Crowd::new(0x1_0124, rtt_100ms(), 1, 1, Gait::Turning, |_| Vec3::ZERO);
    install_interpolation(&mut c.hub.clients[0].world, 60.0, 2.0);
    for step in 1..=520 {
        if step == 401 {
            c.stop();
        }
        let now = f64::from(step) * 1000.0 / 60.0;
        c.hub.clients[0].world.resource_mut::<NetInterpolation>().now_ms = now;
        c.step();
        net_interpolate(&mut c.hub.clients[0].world, now);
    }
    assert!(c.state_counters(0).far_records > 0);
    assert_eq!(c.client_counters(0).record_errors, 0);
    let copy = c.copy(0, c.walkers[0].entity).expect("copy");
    let world = &c.hub.clients[0].world;
    assert!(
        world
            .get::<NetSnapshotBuffer>(copy)
            .and_then(|b| b.0.newest())
            .is_some_and(|s| s.velocity.is_none()),
        "the far snapshot itself has no velocity"
    );
    assert!(
        world.get::<NetVelocity>(copy).is_none(),
        "sampling far snapshots must not expose a stale exact-record velocity: {:?}",
        world.get::<NetVelocity>(copy)
    );
}

/// ADR-10215: whether a client's `NetVelocity` is one the server walker had in the last
/// `recent` ticks, within the velocity grid's half step and `f32` rounding.
fn recent_velocity(shown: Vec3, recent: &[Vec3]) -> bool {
    recent
        .iter()
        .any(|v| (shown - *v).abs().max_element() <= 1.0 / 64.0 + 1e-4)
}

/// The box around `recent` (an interpolated velocity is a blend of two of them), grown by the
/// velocity grid's half step.
fn in_recent_box(shown: Vec3, recent: &[Vec3]) -> bool {
    let lo = recent.iter().fold(Vec3::splat(f32::MAX), |a, v| a.min(*v));
    let hi = recent.iter().fold(Vec3::splat(f32::MIN), |a, v| a.max(*v));
    let pad = Vec3::splat(1.0 / 64.0 + 1e-4);
    shown.cmpge(lo - pad).all() && shown.cmple(hi + pad).all()
}

/// The last `n` server velocities of walker 0, newest last.
fn push_velocity(c: &Crowd, recent: &mut Vec<Vec3>, n: usize) {
    let v = c
        .hub
        .server
        .world
        .get::<NetVelocity>(c.walkers[0].entity)
        .map_or(Vec3::ZERO, |v| v.0);
    recent.push(v);
    if recent.len() > n {
        recent.remove(0);
    }
}

/// ADR-10215 (review R1): a turning walker in the far tier. While its copy is shown from far
/// records it has no replicated velocity; whenever it has one (an exact record that was shorter
/// than the far form), it is one the server had during the last records' reach, never the
/// velocity from before the turns; and after the viewer walks up, the exact records bring the
/// current velocity back.
#[test]
fn a_far_copy_never_shows_a_stale_velocity_and_gets_the_current_one_back() {
    let mut c = Crowd::new(0x1_0124, rtt_100ms(), 1, 1, Gait::Turning, |_| Vec3::ZERO);
    // The spawn lands within a far interval and the delay.
    let copy = (0..120)
        .find_map(|_| {
            c.step();
            c.copy(0, c.walkers[0].entity)
        })
        .expect("copy");
    let first = c.hub.clients[0]
        .world
        .get::<NetVelocity>(copy)
        .expect("the spawn has a velocity")
        .0;
    // As far back as a far-tier record may show (the far interval, a send and the delay), as in
    // `a_far_crowd_travels_as_far_records_and_comes_back_exact`.
    const REACH: usize = 30 + 2 + 3 + 6;
    let mut recent = Vec::new();
    let (mut none, mut some) = (0u32, 0u32);
    for _ in 0..600 {
        c.step();
        push_velocity(&c, &mut recent, REACH);
        match c.hub.clients[0].world.get::<NetVelocity>(copy) {
            None => none += 1,
            Some(v) => {
                some += 1;
                assert!(
                    recent_velocity(v.0, &recent),
                    "a far-tier copy shows a velocity the server had in the last {REACH} ticks: {:?}",
                    v.0
                );
            }
        }
    }
    eprintln!("far tier: {none} ticks without a velocity, {some} with a recent one");
    assert!(c.state_counters(0).far_records > 0, "the copy took the far form");
    assert!(
        none > 300,
        "far records leave the copy without a velocity: {none} of 600 ticks"
    );
    let current = *recent.last().expect("velocities");
    assert!(
        !recent_velocity(first, &recent),
        "the walker turned since its spawn: {first} vs {current}"
    );
    // The viewer walks up while the walker still moves: from the first exact record on, the copy
    // has the velocity of one of the last few ticks (two sends and the delay).
    c.move_viewer(0, CROWD);
    const NEAR_REACH: usize = 2 + 2 + 3 + 1;
    for _ in 0..60 {
        c.step();
        push_velocity(&c, &mut recent, NEAR_REACH);
    }
    for _ in 0..120 {
        c.step();
        push_velocity(&c, &mut recent, NEAR_REACH);
        let shown = c.hub.clients[0].world.get::<NetVelocity>(copy).map(|v| v.0);
        assert!(
            shown.is_some_and(|v| recent_velocity(v, &recent)),
            "an exact copy has the current velocity: {shown:?} vs {recent:?}"
        );
    }
    assert_eq!(c.client_counters(0).record_errors, 0);
    assert_eq!(c.client_counters(0).baseline_missing, 0);
}

/// ADR-10215 at the presentation seam: an interpolated copy of a turning far-tier walker never
/// shows a velocity outside what the server had over the shown snapshots' reach (the
/// interpolation delay included), has none while it blends far snapshots, and has a current
/// one again once it shows exact snapshots after the viewer walks up.
#[test]
fn an_interpolated_far_copy_never_shows_a_stale_velocity_and_gets_the_current_one_back() {
    use regia_net::interp::{NetInterpolation, install_interpolation, net_interpolate};

    let mut c = Crowd::new(0x1_0124, rtt_100ms(), 1, 1, Gait::Turning, |_| Vec3::ZERO);
    install_interpolation(&mut c.hub.clients[0].world, 60.0, 2.0);
    let mut frame = 0u32;
    let mut step = |c: &mut Crowd| {
        frame += 1;
        let now = f64::from(frame) * 1000.0 / 60.0;
        c.hub.clients[0].world.resource_mut::<NetInterpolation>().now_ms = now;
        c.step();
        net_interpolate(&mut c.hub.clients[0].world, now);
    };
    let copy = (0..120)
        .find_map(|_| {
            step(&mut c);
            c.copy(0, c.walkers[0].entity)
        })
        .expect("copy");
    // The far reach plus the interpolation delay and the blended pair (a generous 1 s bound).
    const REACH: usize = 41 + 60;
    let mut recent = Vec::new();
    let mut none = 0u32;
    for _ in 0..600 {
        step(&mut c);
        push_velocity(&c, &mut recent, REACH);
        match c.hub.clients[0].world.get::<NetVelocity>(copy) {
            None => none += 1,
            Some(v) => assert!(
                in_recent_box(v.0, &recent),
                "an interpolated far-tier copy shows a velocity of the last {REACH} ticks: {:?}",
                v.0
            ),
        }
    }
    assert!(c.state_counters(0).far_records > 0, "the copy took the far form");
    assert!(
        none > 300,
        "far snapshots leave the copy without a velocity: {none} of 600 frames"
    );
    c.move_viewer(0, CROWD);
    for _ in 0..120 {
        step(&mut c);
        push_velocity(&c, &mut recent, 60);
    }
    for _ in 0..120 {
        step(&mut c);
        push_velocity(&c, &mut recent, 60);
        let shown = c.hub.clients[0].world.get::<NetVelocity>(copy).map(|v| v.0);
        assert!(
            shown.is_some_and(|v| in_recent_box(v, &recent)),
            "exact snapshots bring a current velocity back: {shown:?}"
        );
    }
    assert_eq!(c.client_counters(0).record_errors, 0);
}
