// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.2 (N213, ADR-8467): quantized transform records and deltas against the acked
//! baseline on the state channel. A walking crowd costs at most 10 bytes per moving entity update
//! at the default precision, positions stay within the profile's precision, and a build with the
//! old (raw `f32`) transform record is refused at the handshake instead of misreading the records.
//!
//! Loopback hub, fixed steps, seeded walkers; no wall clock.

use glam::{Quat, Vec3};
use regia_ecs::prelude::*;
use regia_net::bits::BitReader;
use regia_net::codec::LayoutHasher;
use regia_net::replication::{NetVelocity, ReplicationRegistry};
use regia_net::runtime::{Datagram, NetEntities};
use regia_net::session::hub::SessionHub;
use regia_net::session::lifecycle::{HostOptions, SessionSettings, explain_protocol_mismatch};
use regia_net::session::manifest::ContentManifest;
use regia_net::session::proto::{DisconnectReason, Hello};
use regia_net::testing::{LinkConditions, LoopbackHub, SplitMix64};
use regia_net::*;
use regia_scene::transform::Transform;

const TAG_STATE: u8 = 7;
const TAG_STATE_ENTROPY: u8 = 8;

/// Walkers in the crowd: ids above 127 take two varint bytes, as in a real crowd.
const WALKERS: usize = 256;

fn crowd_setup(world: &mut World) {
    let mut settings = world.resource_mut::<NetSettings>();
    settings.replication.state_channel = StateChannel::Unreliable;
    // The default cadence: 60 Hz ticks, a send every second tick (30 send ticks per second).
    settings.replication.send_every_ticks = 2;
    if world.resource::<NetMode>().is_server() {
        install_net_interest(world);
        // The default profile with world relevance, so every walker is relevant; the position
        // precision is the default profile's.
        let everywhere = NetProfile {
            relevance: RelevanceShape::World,
            linger_ticks: 0,
            ..NetProfile::default()
        };
        world
            .resource_mut::<NetProfiles>()
            .insert("everywhere", everywhere)
            .expect("profile");
        world.resource_mut::<NetProfiles>().set_default("everywhere");
    }
}

struct Walker {
    entity: Entity,
    heading: f32,
    speed: f32,
    turn_in: u32,
}

struct Crowd {
    hub: LoopbackHub,
    walkers: Vec<Walker>,
    rng: SplitMix64,
    dt: f32,
}

impl Crowd {
    fn new(seed: u64) -> Self {
        // About 100 ms round trip at 60 Hz.
        Self::on(
            seed,
            LinkConditions {
                delay_ticks: 3,
                ..LinkConditions::default()
            },
        )
    }

    fn on(seed: u64, link: LinkConditions) -> Self {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, seed, crowd_setup);
        hub.up = link;
        hub.down = link;
        hub.add_client();
        let mut rng = SplitMix64::new(seed ^ 0x5eed);
        let mut walkers = Vec::with_capacity(WALKERS);
        for i in 0..WALKERS {
            // A 16 x 16 grid 6 m apart around a point away from the origin (all axes signed).
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
            walkers.push(Walker {
                entity,
                heading,
                speed,
                turn_in,
            });
        }
        let dt = (1.0 / regia_net::runtime::tick_hz(&hub.server.world)) as f32;
        Self {
            hub,
            walkers,
            rng,
            dt,
        }
    }

    /// One tick of walking: everyone moves along its heading; a walker whose timer runs out turns
    /// to a new heading (and a new pace) and faces it.
    fn walk(&mut self) {
        for w in &mut self.walkers {
            w.turn_in = w.turn_in.saturating_sub(1);
            if w.turn_in == 0 {
                w.heading += (self.rng.next_f64() as f32 - 0.5) * 2.0;
                w.speed = 1.2 + (self.rng.next_f64() as f32) * 0.4;
                w.turn_in = 60 + (self.rng.next_u64() % 120) as u32;
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
        self.hub.run(1);
    }

    /// Everyone stops where they are.
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
}

fn tag(d: &Datagram) -> u8 {
    d.bytes.first().copied().unwrap_or(0)
}

/// The record count of a `STATE` frame: `tag u8 | tick varint | seq varint | count varint`.
fn record_count(d: &Datagram) -> u64 {
    let mut r = BitReader::new(&d.bytes);
    r.read_bits(8).expect("tag");
    r.read_varint().expect("tick");
    r.read_varint().expect("seq");
    r.read_varint().expect("count")
}

/// The acceptance bar of spec 120 § 8.2: a moving entity's update costs at most 10 bytes at the
/// default precision (the base measured 35.76 B on this workload: raw `f32` position and velocity
/// on every change). Bytes are whole state frames (frame headers included) over the update
/// records they carry; every walker moves on every tick, so every record is a moving entity's.
#[test]
fn a_walking_crowd_costs_at_most_10_bytes_per_moving_entity_update() {
    let mut crowd = Crowd::new(0x8467);
    for _ in 0..120 {
        crowd.walk();
    }
    let all_spawned = crowd.walkers.iter().all(|w| crowd.copy(w.entity).is_some());
    assert!(all_spawned, "every walker spawned on the client");
    let from = crowd.hub.client_logs[0].len();
    let ticks = 240;
    for _ in 0..ticks {
        crowd.walk();
    }
    let window: Vec<&Datagram> = crowd.hub.client_logs[0][from..]
        .iter()
        .map(|(_, d)| d)
        .filter(|d| matches!(tag(d), TAG_STATE | TAG_STATE_ENTROPY))
        .collect();
    assert!(
        window.iter().all(|d| tag(d) == TAG_STATE),
        "entropy is off by default: these are plain state frames"
    );
    let bytes: usize = window.iter().map(|d| d.bytes.len()).sum();
    let records: u64 = window.iter().map(|d| record_count(d)).sum();
    let send_ticks = (ticks / 2) as f64;
    let per_update = bytes as f64 / records as f64;
    let per_send = records as f64 / send_ticks;
    eprintln!(
        "walking crowd: {bytes} B in {} frames, {records} updates, {per_update:.2} B per moving \
         entity update, {per_send:.1} updates per send tick",
        window.len()
    );
    assert!(
        records as f64 >= send_ticks * 40.0,
        "the default budget still carries a crowd: {per_send:.1} updates per send tick"
    );
    assert!(
        per_update <= 10.0,
        "{per_update:.2} B per moving entity update (bar: 10)"
    );
}

/// The quantized record keeps every position within half the profile's precision (plus half an
/// `f32` unit in the last place), and rotations within the smallest-three bound, once the
/// walkers stop and the last records land.
#[test]
fn a_walking_crowd_lands_within_the_profile_precision() {
    let mut crowd = Crowd::new(0x1024);
    for _ in 0..200 {
        crowd.walk();
    }
    crowd.stop();
    crowd.hub.run(60);
    let step = NetProfile::default().position_step_m.expect("default precision");
    let mut worst = 0.0f32;
    let mut worst_angle = 0.0f32;
    for w in &crowd.walkers {
        let server = *crowd.hub.server.world.get::<Transform>(w.entity).expect("server");
        let copy = crowd.copy(w.entity).expect("copy");
        let client = *crowd.hub.clients[0]
            .world
            .get::<Transform>(copy)
            .expect("client transform");
        for axis in 0..3 {
            let (s, c) = (server.translation[axis], client.translation[axis]);
            let bound = 0.5 * step + 0.5 * f32::EPSILON * s.abs().max(1.0);
            assert!(
                (s - c).abs() <= bound * 1.0001,
                "axis {axis}: server {s}, client {c}, step {step}"
            );
            worst = worst.max((s - c).abs());
        }
        worst_angle = worst_angle.max(server.rotation.angle_between(client.rotation).to_degrees());
    }
    eprintln!("worst position error {worst} m (step {step}), worst rotation error {worst_angle} deg");
    // Smallest-three at 10 bits: each kept component within 1/1023 of the range sqrt(2) wide.
    assert!(worst_angle < 0.2, "rotation error {worst_angle} deg");
}

/// Deltas name their baseline, so loss, reordering and duplicates of state frames never put a
/// wrong value on a copy: a crowd on a bad link lands on the same positions as on a clean one.
#[test]
fn a_walking_crowd_converges_through_loss_reorder_and_duplicates() {
    for seed in [3u64, 17, 0x8467] {
        let mut crowd = Crowd::on(
            seed,
            LinkConditions {
                delay_ticks: 4,
                loss: 0.15,
                reorder: 0.2,
                duplicate: 0.1,
                ..LinkConditions::default()
            },
        );
        for _ in 0..240 {
            crowd.walk();
        }
        crowd.stop();
        crowd.hub.run(120);
        let step = NetProfile::default().position_step_m.expect("default precision");
        for w in &crowd.walkers {
            let server = *crowd.hub.server.world.get::<Transform>(w.entity).expect("server");
            let copy = crowd.copy(w.entity).expect("copy");
            let client = *crowd.hub.clients[0]
                .world
                .get::<Transform>(copy)
                .expect("client transform");
            for axis in 0..3 {
                let (s, c) = (server.translation[axis], client.translation[axis]);
                let bound = 0.5 * step + 0.5 * f32::EPSILON * s.abs().max(1.0);
                assert!(
                    (s - c).abs() <= bound * 1.0001,
                    "seed {seed}, axis {axis}: server {s}, client {c}"
                );
            }
            assert!(server.rotation.angle_between(client.rotation).to_degrees() < 0.2);
        }
    }
}

/// The protocol hash a build with the raw `f32` transform record (before ADR-8467) computed for
/// the same registrations: the session layout revision, then the message, component and
/// prediction registries, and nothing for the transform record.
fn old_record_protocol_hash(world: &World) -> u64 {
    let mut h = LayoutHasher::new();
    h.write_u64(regia_net::session::proto::SESSION_WIRE_REVISION);
    if let Some(m) = world.get_resource::<regia_net::message::NetMessageRegistry>() {
        m.hash_into(&mut h);
    }
    if let Some(r) = world.get_resource::<ReplicationRegistry>() {
        r.hash_into(&mut h);
    }
    if let Some(p) = world.get_resource::<regia_net::predict::PredictionRegistry>() {
        p.hash_into(&mut h);
    }
    h.finish()
}

fn old_record_types(world: &World) -> Vec<(String, u64)> {
    regia_net::runtime::protocol_types(world)
        .into_iter()
        .filter(|(n, _)| n != "regia_net::replication::transform_record")
        .collect()
}

fn hello(hash: u64) -> Hello {
    Hello {
        protocol_version: MIN_NET_PROTOCOL_VERSION,
        protocol_hash: hash,
        manifest: ContentManifest::build("0.6.0", "1.0.0", NET_PROTOCOL_VERSION, hash, &[]),
        player_names: vec!["Ann".to_owned()],
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: None,
    }
}

/// Spec 120 § 8.2: an old-record peer is refused, not misread. A client built with the raw `f32`
/// transform record has another protocol hash (the record's revision is part of it), so the
/// server refuses it with `protocol_mismatch` and a dev build names the transform record.
#[test]
fn a_client_with_the_old_transform_record_is_refused_cleanly() {
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    world.insert_resource(NetSettings::default());
    regia_net::runtime::ensure_resources(&mut world);
    let now = regia_net::runtime::protocol_hash(&world);
    let old = old_record_protocol_hash(&world);
    assert_ne!(
        now, old,
        "the transform record revision is part of the protocol hash"
    );

    let opts = HostOptions {
        level: "levels/arena.level.ron".into(),
        ..HostOptions::default()
    };
    let manifest = ContentManifest::build("0.6.0", "1.0.0", NET_PROTOCOL_VERSION, now, &[]);
    let mut hub = SessionHub::new(SessionSettings::default(), opts, manifest);
    hub.server.set_dev_build(true);
    hub.server
        .set_protocol_types(regia_net::runtime::protocol_types(&world));

    // A current client joins.
    let ok = hub.add_client(hello(now));
    assert!(
        hub.run_until(600, |h| h.clients.client(ok).in_game()),
        "{:?}",
        hub.clients.client(ok).state()
    );

    // An old-record client is refused with protocol_mismatch.
    let c = hub.add_client(hello(old));
    assert!(hub.run_until(600, |h| h.clients.client(c).disconnect_reason().is_some()));
    let reason = hub
        .clients
        .client(c)
        .disconnect_reason()
        .cloned()
        .expect("reason");
    assert_eq!(reason.code(), "protocol_mismatch", "{reason:?}");
    let DisconnectReason::ProtocolMismatch(detail) = reason else {
        panic!("not a protocol mismatch");
    };
    let explained = explain_protocol_mismatch(&detail, &old_record_types(&world));
    assert!(
        explained.contains("regia_net::replication::transform_record (missing on client)"),
        "{explained}"
    );
}
