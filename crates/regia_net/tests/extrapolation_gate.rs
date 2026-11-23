// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.2 (ADR-8522): the extrapolation gate, the steady bit and the velocity-predicted
//! position residual. With `extrapolation_gate_m` set, the acknowledged state channel sends a
//! moving entity's transform only when the client's extrapolation of the record it may be showing
//! errs past the gate; the client keeps extrapolating steady records instead of holding them, so
//! the shown pose stays near the server's. With the gate off (the default) nothing changes.
//!
//! Loopback hub, fixed steps, seeded walkers; no wall clock. "Shown error" is the distance between
//! a copy's `Transform` and the server's position at the tick the client estimates it is showing
//! (`ClientStateChannel::server_tick_estimate`: the newest state frame's tick plus the local ticks
//! since): what the player sees against where the entity is.

use std::collections::HashMap;

use glam::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_net::codec::LayoutHasher;
use regia_net::replication::{ClientReplication, NetSteady, NetVelocity};
use regia_net::runtime::{Datagram, NetEntities, NetNode};
use regia_net::testing::{LinkConditions, LoopbackHub, SplitMix64};
use regia_net::*;
use regia_scene::transform::Transform;

const TAG_STATE: u8 = 7;
const TAG_STATE_ENTROPY: u8 = 8;

/// The gate the owner may make the default (spec 120 § 8.2: DO27's 2 cm).
const GATE_M: f32 = 0.02;

fn setup_with(world: &mut World, gate: Option<f32>) {
    let mut settings = world.resource_mut::<NetSettings>();
    settings.replication.state_channel = StateChannel::Unreliable;
    // The default cadence: 60 Hz ticks, a send every second tick.
    settings.replication.send_every_ticks = 2;
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        // World relevance, so every walker is relevant; the default precision.
        let everywhere = NetProfile {
            relevance: RelevanceShape::World,
            linger_ticks: 0,
            extrapolation_gate_m: gate,
            ..NetProfile::default()
        };
        world
            .resource_mut::<NetProfiles>()
            .insert("everywhere", everywhere)
            .expect("profile");
        world.resource_mut::<NetProfiles>().set_default("everywhere");
    }
}

fn setup_off(world: &mut World) {
    setup_with(world, None);
}

fn setup_gated(world: &mut World) {
    setup_with(world, Some(GATE_M));
}

struct Walker {
    entity: Entity,
    heading: f32,
    speed: f32,
    turn_in: u32,
}

/// What a run measured.
#[derive(Debug, Default, Clone, Copy)]
struct Measure {
    /// State-frame bytes in the window.
    bytes: usize,
    /// Moving-entity update records in the window.
    records: u64,
    /// The largest shown error, metres.
    worst_shown: f32,
    /// The mean shown error, metres.
    mean_shown: f32,
    /// Copies sampled.
    samples: u64,
}

struct Crowd {
    hub: LoopbackHub,
    walkers: Vec<Walker>,
    rng: SplitMix64,
    dt: f32,
    turns: bool,
    /// Server positions by server tick.
    history: HashMap<u32, Vec<Vec3>>,
}

impl Crowd {
    fn new(seed: u64, gate: Option<f32>, link: LinkConditions, walkers: usize, turns: bool) -> Self {
        let setup: fn(&mut World) = match gate {
            Some(g) => {
                assert_eq!(g, GATE_M, "the hub's setup is a plain fn: one gated profile");
                setup_gated
            }
            None => setup_off,
        };
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, setup);
        hub.up = link;
        hub.down = link;
        hub.add_client();
        let mut rng = SplitMix64::new(seed ^ 0x5eed);
        let mut list = Vec::with_capacity(walkers);
        for i in 0..walkers {
            let p = Vec3::new(
                -45.0 + (i % 16) as f32 * 6.0 + 123.4,
                1.5 + (i % 3) as f32 * 0.25,
                -45.0 + (i / 16) as f32 * 6.0 - 321.7,
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
            walkers: list,
            rng,
            dt,
            turns,
            history: HashMap::new(),
        }
    }

    fn walk(&mut self) {
        for w in &mut self.walkers {
            if self.turns {
                w.turn_in = w.turn_in.saturating_sub(1);
                if w.turn_in == 0 {
                    w.heading += (self.rng.next_f64() as f32 - 0.5) * 2.0;
                    w.speed = 1.2 + (self.rng.next_f64() as f32) * 0.4;
                    w.turn_in = 60 + (self.rng.next_u64() % 120) as u32;
                }
            }
            let v = Vec3::new(w.heading.sin(), 0.0, w.heading.cos()) * w.speed;
            let world = &mut self.hub.server.world;
            if let Some(mut t) = world.get_mut::<Transform>(w.entity) {
                t.translation += v * self.dt;
                t.rotation = Quat::from_rotation_y(w.heading);
            }
            if let Some(mut nv) = world.get_mut::<NetVelocity>(w.entity) {
                nv.0 = v;
            }
        }
        let now: Vec<Vec3> = self
            .walkers
            .iter()
            .map(|w| {
                self.hub
                    .server
                    .world
                    .get::<Transform>(w.entity)
                    .map_or(Vec3::ZERO, |t| t.translation)
            })
            .collect();
        // The server sends this step's state at the tick it is on now (`NetNode::tick` moves on
        // at the end of the send).
        let tick = self.hub.server_tick().0;
        self.hub.run(1);
        self.history.insert(tick, now);
    }

    fn stop(&mut self) {
        for w in &self.walkers {
            if let Some(mut nv) = self.hub.server.world.get_mut::<NetVelocity>(w.entity) {
                nv.0 = Vec3::ZERO;
            }
        }
    }

    fn copy(&self, e: Entity) -> Option<Entity> {
        let id = *self.hub.server.world.get::<NetId>(e)?;
        self.hub.clients[0].world.resource::<NetEntities>().entity(id)
    }

    /// The shown error of every copy this tick: `(worst, sum, count)`.
    fn shown_errors(&self) -> (f32, f64, u64) {
        let cw = &self.hub.clients[0].world;
        let local = cw.resource::<NetNode>().local_ticks;
        let Some(est) = cw
            .resource::<ClientReplication>()
            .state
            .server_tick_estimate(local)
        else {
            return (0.0, 0.0, 0);
        };
        let Some(truth) = self.history.get(&(est as u32)) else {
            return (0.0, 0.0, 0);
        };
        let (mut worst, mut sum, mut n) = (0.0f32, 0.0f64, 0u64);
        for (i, w) in self.walkers.iter().enumerate() {
            let Some(copy) = self.copy(w.entity) else {
                continue;
            };
            let Some(shown) = cw.get::<Transform>(copy) else {
                continue;
            };
            let Some(at) = truth.get(i) else {
                continue;
            };
            let e = shown.translation.distance(*at);
            worst = worst.max(e);
            sum += f64::from(e);
            n += 1;
        }
        (worst, sum, n)
    }

    /// Walks `ticks` ticks after a warm-up and measures bytes per moving entity update and the
    /// shown error.
    fn run(&mut self, warm: usize, ticks: usize) -> Measure {
        for _ in 0..warm {
            self.walk();
        }
        assert!(
            self.walkers.iter().all(|w| self.copy(w.entity).is_some()),
            "every walker spawned on the client"
        );
        let from = self.hub.client_logs[0].len();
        let mut m = Measure::default();
        let mut sum = 0.0f64;
        for _ in 0..ticks {
            self.walk();
            let (worst, s, n) = self.shown_errors();
            m.worst_shown = m.worst_shown.max(worst);
            sum += s;
            m.samples += n;
        }
        m.mean_shown = (sum / m.samples.max(1) as f64) as f32;
        let window: Vec<&Datagram> = self.hub.client_logs[0][from..]
            .iter()
            .map(|(_, d)| d)
            .filter(|d| matches!(d.bytes.first().copied(), Some(TAG_STATE | TAG_STATE_ENTROPY)))
            .collect();
        m.bytes = window.iter().map(|d| d.bytes.len()).sum();
        m.records = window.iter().map(|d| record_count(d)).sum();
        m
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

/// Walkers on straight lines: with a 2 cm gate the server sends a walker only when the 1/32 m/s
/// velocity grid's rounding has carried the client's extrapolation 2 cm off (about once a second),
/// not on every change; the shown error stays within the gate plus what accrues while the next
/// record travels. With the gate off every send tick carries updates and a held copy shows its
/// last record until the next arrives.
#[test]
fn straight_walkers_cost_a_fraction_with_the_gate_and_stay_within_it() {
    let mut off = Crowd::new(0x8522, None, rtt_100ms(), 64, false);
    let off_m = off.run(60, 240);
    let mut on = Crowd::new(0x8522, Some(GATE_M), rtt_100ms(), 64, false);
    let on_m = on.run(60, 240);
    eprintln!("straight, gate off: {off_m:?}");
    eprintln!("straight, gate on:  {on_m:?}");
    let gate_held = |c: &Crowd| {
        c.hub
            .server
            .world
            .resource::<regia_net::replication::ServerReplication>()
            .state_link(c.hub.clients[0].peer)
            .map_or(0, |l| l.counters.gate_held)
    };
    assert!(gate_held(&on) > 0, "the gate held records back");
    assert_eq!(gate_held(&off), 0, "no gate, nothing held back");
    assert!(
        on_m.bytes * 5 <= off_m.bytes,
        "the gate cuts straight walkers' bytes at least five times: {} vs {}",
        on_m.bytes,
        off_m.bytes
    );
    // The velocity grid's rounding error grows at most sqrt(2)/64 m/s; a record takes the send
    // interval plus the delay plus one tick to land.
    let lag_s = (2 + 3 + 2) as f32 / 60.0;
    let bound = GATE_M + 2.0f32.sqrt() / 64.0 * lag_s + 0.002;
    assert!(
        on_m.worst_shown <= bound,
        "worst shown error {} m over the bound {bound} m",
        on_m.worst_shown
    );
    // Every steady copy without interpolation carries `NetSteady`.
    let cw = &on.hub.clients[0].world;
    for w in &on.walkers {
        let copy = on.copy(w.entity).expect("copy");
        assert!(
            cw.get::<NetSteady>(copy).is_some(),
            "a steady copy moves along its record"
        );
    }
    // With the gate off, records are not steady: no copy carries `NetSteady`.
    let cw = &off.hub.clients[0].world;
    for w in &off.walkers {
        let copy = off.copy(w.entity).expect("copy");
        assert!(cw.get::<NetSteady>(copy).is_none(), "the gate is off by default");
    }
}

/// A turning crowd (the quantized-transform workload): the gate sends far fewer bytes, and the
/// shown error with the gate is no worse than the default's, where a copy holds its last record.
/// Once everyone stops, every copy lands within the gate of the server.
#[test]
fn a_turning_crowd_with_the_gate_sends_less_and_lands_within_it() {
    let mut off = Crowd::new(0x0208, None, rtt_100ms(), 128, true);
    let off_m = off.run(120, 240);
    let mut on = Crowd::new(0x0208, Some(GATE_M), rtt_100ms(), 128, true);
    let on_m = on.run(120, 240);
    let per = |m: &Measure| m.bytes as f64 / m.records.max(1) as f64;
    eprintln!(
        "turning, gate off: {off_m:?} ({:.2} B per moving entity update)",
        per(&off_m)
    );
    eprintln!(
        "turning, gate on:  {on_m:?} ({:.2} B per moving entity update)",
        per(&on_m)
    );
    assert!(
        on_m.bytes * 2 <= off_m.bytes,
        "the gate at least halves a turning crowd's bytes: {} vs {}",
        on_m.bytes,
        off_m.bytes
    );
    assert!(
        on_m.mean_shown <= off_m.mean_shown,
        "the mean shown error with the gate ({}) is no worse than without ({})",
        on_m.mean_shown,
        off_m.mean_shown
    );
    on.stop();
    on.hub.run(180);
    let cw = &on.hub.clients[0].world;
    for w in &on.walkers {
        let server = *on.hub.server.world.get::<Transform>(w.entity).expect("server");
        let copy = on.copy(w.entity).expect("copy");
        let client = *cw.get::<Transform>(copy).expect("client transform");
        let d = server.translation.distance(client.translation);
        assert!(d <= GATE_M + 0.001, "stopped walker {d} m off (gate {GATE_M})");
        assert!(
            server.rotation.angle_between(client.rotation).to_degrees()
                <= regia_net::profile::EXTRAPOLATION_GATE_DEG + 0.2
        );
        assert!(cw.get::<NetSteady>(copy).is_none(), "a stopped copy holds still");
    }
}

/// Loss, reordering and duplicates: the gate still converges (a lost record leaves the in-flight
/// list when the link resolves it, and the acknowledged baseline is checked again).
#[test]
fn the_gate_converges_through_loss_reorder_and_duplicates() {
    for seed in [3u64, 17, 0x8522] {
        let link = LinkConditions {
            delay_ticks: 4,
            loss: 0.15,
            reorder: 0.2,
            duplicate: 0.1,
            ..LinkConditions::default()
        };
        let mut crowd = Crowd::new(seed, Some(GATE_M), link, 64, true);
        let m = crowd.run(120, 240);
        eprintln!("seed {seed}, gate on, impaired: {m:?}");
        crowd.stop();
        crowd.hub.run(240);
        let cw = &crowd.hub.clients[0].world;
        for w in &crowd.walkers {
            let server = *crowd.hub.server.world.get::<Transform>(w.entity).expect("server");
            let copy = crowd.copy(w.entity).expect("copy");
            let client = *cw.get::<Transform>(copy).expect("client transform");
            let d = server.translation.distance(client.translation);
            assert!(d <= GATE_M + 0.001, "seed {seed}: stopped walker {d} m off");
        }
    }
}

/// The default profile has no gate, so the walking crowd's records are exactly the quantized
/// transform lane's: no copy is held back, no record is steady.
#[test]
fn the_gate_is_off_by_default() {
    assert_eq!(NetProfile::default().extrapolation_gate_m, None);
    let mut crowd = Crowd::new(0x1024, None, rtt_100ms(), 32, true);
    crowd.run(60, 60);
    let cw = &crowd.hub.clients[0].world;
    for w in &crowd.walkers {
        let copy = crowd.copy(w.entity).expect("copy");
        assert!(cw.get::<NetSteady>(copy).is_none());
    }
}

/// The protocol hash a revision 2 build (ADR-8467, no steady bit, no predicted residual) computed
/// for the same registrations.
fn revision_2_protocol_hash(world: &World) -> u64 {
    let mut h = LayoutHasher::new();
    h.write_u64(regia_net::session::proto::SESSION_WIRE_REVISION);
    if let Some(m) = world.get_resource::<regia_net::message::NetMessageRegistry>() {
        m.hash_into(&mut h);
    }
    if let Some(r) = world.get_resource::<regia_net::replication::ReplicationRegistry>() {
        r.hash_into(&mut h);
    }
    if let Some(p) = world.get_resource::<regia_net::predict::PredictionRegistry>() {
        p.hash_into(&mut h);
    }
    h.write_str(regia_net::replication::TRANSFORM_RECORD_TYPE);
    h.write_u64(2);
    h.finish()
}

/// A revision 2 peer reads records without the steady bit and predicted residuals: it must be
/// refused at the handshake, so the revision is in the protocol hash.
#[test]
fn a_revision_2_peer_has_another_protocol_hash() {
    // Revision 4 since ADR-10124 (the far-tier record); the revision 2 peer is refused as before.
    assert_eq!(regia_net::replication::TRANSFORM_RECORD_REVISION, 4);
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    world.insert_resource(NetSettings::default());
    regia_net::runtime::ensure_resources(&mut world);
    assert_ne!(
        regia_net::runtime::protocol_hash(&world),
        revision_2_protocol_hash(&world)
    );
    let types = regia_net::runtime::protocol_types(&world);
    assert!(
        types.contains(&(regia_net::replication::TRANSFORM_RECORD_TYPE.to_owned(), 4)),
        "{types:?}"
    );
}

/// One impaired link of the soak row, as the loopback hub models it at 60 Hz (one tick is
/// 16.7 ms; the hub adds one tick to every delivery, and its reorder holds a frame 1-3 ticks).
fn impaired(name: &str) -> LinkConditions {
    let ticks = |ms: u64| (ms as f64 / (1000.0 / 60.0)).round() as u64;
    match name {
        // `regia_net_soak` `good-wifi`: 4 ms + 0-4 ms jitter, 0.2 % loss, 0.1 % reorder, 0.05 %
        // duplicates (its bandwidth caps are far above this crowd's rate).
        "good-wifi" => LinkConditions {
            delay_ticks: ticks(4 + 2),
            loss: 0.002,
            reorder: 0.001,
            duplicate: 0.0005,
            ..LinkConditions::default()
        },
        // `regia_net_soak` `bad-wifi`: 8 ms + 0-30 ms jitter (as reordering), 1 % random loss plus
        // Gilbert-Elliott bursts (about 3 % in all), 1 % reorder, 0.5 % duplicates.
        "bad-wifi" => LinkConditions {
            delay_ticks: ticks(8 + 15),
            loss: 0.03,
            reorder: 0.02,
            duplicate: 0.005,
            ..LinkConditions::default()
        },
        // The conditioner's `wan_stress` (spec 76), read from its own profile.
        "wan_stress" => {
            let c = regia_net::conditioner::ConditionerConfig::from_profile("wan_stress").expect("profile");
            LinkConditions {
                delay_ticks: ticks(c.delay_ms + c.jitter_ms / 2),
                loss: c.loss,
                reorder: c.reorder,
                duplicate: c.duplicate,
                ..LinkConditions::default()
            }
        }
        other => panic!("no soak link {other}"),
    }
}

/// The soak row (docs/perf/net-extrapolation-gate.md): bytes per moving entity update, bytes per
/// second and the shown error of the turning crowd, gate off and on (2 cm), on the impaired links
/// good-wifi, bad-wifi and wan_stress. Bars: the gate cuts the bytes on every link, and once the
/// crowd stops every copy lands within the gate. Run with `--nocapture` for the table.
#[test]
fn soak_row_bytes_and_shown_error_on_impaired_links() {
    eprintln!(
        "| link | gate | B per moving entity update | kB/s | updates/s | worst shown m | mean shown m |"
    );
    eprintln!("|---|---|---|---|---|---|---|");
    for link in ["good-wifi", "bad-wifi", "wan_stress"] {
        let mut rows = Vec::new();
        for gate in [None, Some(GATE_M)] {
            let mut crowd = Crowd::new(0x8522, gate, impaired(link), 128, true);
            let ticks = 600;
            let m = crowd.run(120, ticks);
            let seconds = ticks as f64 / 60.0;
            eprintln!(
                "| {link} | {} | {:.2} | {:.1} | {:.0} | {:.3} | {:.4} |",
                gate.map_or("off".to_owned(), |g| format!("{} cm", g * 100.0)),
                m.bytes as f64 / m.records.max(1) as f64,
                m.bytes as f64 / seconds / 1000.0,
                m.records as f64 / seconds,
                m.worst_shown,
                m.mean_shown
            );
            if gate.is_some() {
                crowd.stop();
                crowd.hub.run(300);
                let cw = &crowd.hub.clients[0].world;
                for w in &crowd.walkers {
                    let server = *crowd.hub.server.world.get::<Transform>(w.entity).expect("server");
                    let copy = crowd.copy(w.entity).expect("copy");
                    let client = *cw.get::<Transform>(copy).expect("client transform");
                    let d = server.translation.distance(client.translation);
                    assert!(d <= GATE_M + 0.001, "{link}: stopped walker {d} m off");
                }
            }
            rows.push(m);
        }
        assert!(
            rows[1].bytes < rows[0].bytes,
            "{link}: the gate sends fewer bytes ({} vs {})",
            rows[1].bytes,
            rows[0].bytes
        );
    }
}
