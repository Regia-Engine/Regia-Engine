// SPDX-License-Identifier: MPL-2.0
//! Wire-6 review: exact/far ordering, malformed field boundaries and revision-3 handshake.

use glam::Vec3;
use regia_ecs::prelude::World;
use regia_net::bits::{BitReader, BitWriter};
use regia_net::replication::ClientReplication;
use regia_net::runtime::NetEntities;
use regia_net::testing::state_frame_fuzz as fuzz;
use regia_net::*;
use regia_scene::transform::Transform;

fn seed_header() -> (u64, u64, NetId) {
    let frames = fuzz::recorded();
    let mut r = BitReader::new(&frames[frames.len() - 2]);
    assert_eq!(r.read_bits(8).expect("tag"), 7);
    let tick = r.read_varint().expect("tick");
    let seq = r.read_varint().expect("seq");
    assert_eq!(r.read_varint().expect("count"), 1);
    (tick, seq, NetId(r.read_varint().expect("id") as u32))
}

fn frame(tick: u64, seq: u64, id: NetId, body: &BitWriter) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_bits(7, 8);
    w.write_varint(tick);
    w.write_varint(seq);
    w.write_varint(1);
    w.write_varint(u64::from(id.0));
    w.write_varint(body.bit_len() as u64);
    w.append(body);
    w.finish()
}

fn header(kind: u64) -> BitWriter {
    let mut w = BitWriter::new();
    w.write_bits(kind, 2);
    w.write_bool(false); // version is frame tick
    w.write_bool(true);
    w.write_bool(false);
    w.write_bits(1, 5); // base_ago = 2
    w
}

fn far_body() -> BitWriter {
    let mut w = header(3);
    w.write_bool(true);
    w.write_bits(103, 15);
    w.write_bits(300, 15);
    w.write_bits(2, 10);
    w.write_bool(false); // no component entries
    w
}

fn exact_body(delta: bool) -> BitWriter {
    let mut w = header(2);
    w.write_bool(false); // parent unchanged
    w.write_bool(true); // transform present
    w.write_bool(false); // not a teleport
    w.write_bool(false); // not steady
    w.write_bool(delta);
    if delta {
        w.write_bool(true); // position changed
        w.write_bool(true); // same grid, no baseline velocity
        w.write_bool(true); // x changed
        w.write_bits(0, 2);
        w.write_bits(1, 4); // zigzag(+1) - 1
        w.write_bool(false); // y unchanged
        w.write_bool(false); // z unchanged
    } else {
        w.write_bits(1, 2); // position-only mode
        w.write_bool(true); // grid
        w.write_bool(true); // default step
        for offset in [3301, 2100, 9601] {
            w.write_zigzag(0);
            w.write_bits(offset, 20);
        }
    }
    w.write_bool(false); // no component entries
    w
}

fn world_with_grid_baseline() -> World {
    let mut world = fuzz::scratch_world();
    let frames = fuzz::recorded();
    for f in &frames[..frames.len() - 2] {
        fuzz::apply(&mut world, f).expect("fixture");
    }
    world
}

#[test]
fn exact_wins_in_both_arrival_orders_and_is_a_valid_delta_baseline() {
    let (tick, seq, id) = seed_header();
    let far = frame(tick, seq, id, &far_body());
    let exact = frame(tick, seq + 1, id, &exact_body(false));
    let next = frame(tick + 2, seq + 2, id, &exact_body(true));
    for exact_first in [false, true] {
        let mut world = world_with_grid_baseline();
        let order = if exact_first {
            [&exact, &far]
        } else {
            [&far, &exact]
        };
        for f in order {
            fuzz::apply(&mut world, f).expect("valid record");
        }
        let entity = world.resource::<NetEntities>().entity(id).expect("copy");
        let at = world.get::<Transform>(entity).expect("transform").translation;
        assert!(at.distance(Vec3::new(3.301, 2.1, 9.601)) < 1e-5, "{at}");
        fuzz::apply(&mut world, &next).expect("delta after exact promotion");
        let at = world.get::<Transform>(entity).expect("transform").translation;
        assert!(at.distance(Vec3::new(3.302, 2.1, 9.601)) < 1e-5, "{at}");
        let counters = world.resource::<ClientReplication>().state.counters;
        assert_eq!((counters.record_errors, counters.baseline_missing), (0, 0));
    }
}

#[test]
fn transform_delta_against_a_far_only_version_is_refused() {
    let (tick, seq, id) = seed_header();
    let mut world = world_with_grid_baseline();
    fuzz::apply(&mut world, &frame(tick, seq, id, &far_body())).expect("far");
    let before = world.resource::<ClientReplication>().state.counters;
    fuzz::apply(&mut world, &frame(tick + 2, seq + 1, id, &exact_body(true))).expect("frame");
    let after = world.resource::<ClientReplication>().state.counters;
    assert_eq!(after.record_errors, before.record_errors + 1);
    assert_eq!(after.applied, before.applied);
}

#[test]
fn every_truncated_far_body_is_refused_or_counted_as_a_record_error() {
    let (tick, seq, id) = seed_header();
    let full = far_body();
    let bits = full.bit_len();
    let bytes = full.finish();
    for len in 0..bits {
        let mut world = world_with_grid_baseline();
        let before = world.resource::<ClientReplication>().state.counters.record_errors;
        let mut short = BitWriter::new();
        short.append_slice(&bytes, len);
        let result = fuzz::apply(&mut world, &frame(tick, seq, id, &short));
        let after = world.resource::<ClientReplication>().state.counters.record_errors;
        assert!(
            result.is_err() || after > before,
            "accepted a {len}/{bits}-bit far body"
        );
    }
}

#[test]
fn revision_three_client_is_refused_and_the_diagnostic_names_the_changed_row() {
    use regia_net::codec::LayoutHasher;
    use regia_net::replication::{TRANSFORM_RECORD_REVISION, TRANSFORM_RECORD_TYPE};
    use regia_net::session::hub::SessionHub;
    use regia_net::session::lifecycle::{HostOptions, SessionSettings, explain_protocol_mismatch};
    use regia_net::session::manifest::ContentManifest;
    use regia_net::session::proto::{DisconnectReason, Hello};

    let world = world_with_grid_baseline();
    let mut old = LayoutHasher::new();
    old.write_u64(regia_net::session::proto::SESSION_WIRE_REVISION);
    if let Some(m) = world.get_resource::<regia_net::message::NetMessageRegistry>() {
        m.hash_into(&mut old);
    }
    if let Some(r) = world.get_resource::<regia_net::replication::ReplicationRegistry>() {
        r.hash_into(&mut old);
    }
    if let Some(p) = world.get_resource::<regia_net::predict::PredictionRegistry>() {
        p.hash_into(&mut old);
    }
    old.write_str(TRANSFORM_RECORD_TYPE);
    old.write_u64(3);
    let old = old.finish();
    assert_eq!(TRANSFORM_RECORD_REVISION, 4);
    assert_eq!(REGIA_NET_WIRE_VERSION, 6);
    let now = regia_net::runtime::protocol_hash(&world);
    assert_ne!(old, now);
    let manifest = ContentManifest::build("0.7.0", "1.0.0", NET_PROTOCOL_VERSION, now, &[]);
    let options = HostOptions {
        level: "levels/arena.level.ron".into(),
        ..HostOptions::default()
    };
    let mut hub = SessionHub::new(SessionSettings::default(), options, manifest);
    hub.server.set_dev_build(true);
    hub.server
        .set_protocol_types(regia_net::runtime::protocol_types(&world));
    let hello = |hash| Hello {
        protocol_version: MIN_NET_PROTOCOL_VERSION,
        protocol_hash: hash,
        manifest: ContentManifest::build("0.7.0", "1.0.0", NET_PROTOCOL_VERSION, hash, &[]),
        player_names: vec!["Review".to_owned()],
        password: None,
        auth: None,
        client_rate_bytes: 0,
        reconnect_token: None,
    };
    let current = hub.add_client(hello(now));
    assert!(hub.run_until(600, |h| h.clients.client(current).in_game()));
    let previous = hub.add_client(hello(old));
    assert!(hub.run_until(600, |h| h.clients.client(previous).disconnect_reason().is_some()));
    let reason = hub.clients.client(previous).disconnect_reason().expect("refusal");
    assert_eq!(reason.code(), "protocol_mismatch");
    let DisconnectReason::ProtocolMismatch(detail) = reason else {
        panic!("wrong refusal: {reason:?}");
    };
    let mut old_types = regia_net::runtime::protocol_types(&world);
    old_types
        .iter_mut()
        .find(|(n, _)| n == TRANSFORM_RECORD_TYPE)
        .expect("row")
        .1 = 3;
    let diagnostic = explain_protocol_mismatch(detail, &old_types);
    assert!(
        diagnostic.contains("regia_net::replication::transform_record (different)"),
        "{diagnostic}"
    );
}

#[test]
fn seeded_mutations_target_both_far_forms_with_and_without_interpolation() {
    let frames = fuzz::recorded();
    let mut rng = regia_core::fuzz::FuzzRng::new(0xfab1_e006);
    let mut errors = 0;
    for iteration in 0..2048 {
        let at = frames.len() - 2 + iteration % 2;
        let input = fuzz::mutate(frames, at, &mut rng);
        let world = if iteration.is_multiple_of(4) {
            fuzz::scratch_world_interpolated()
        } else {
            fuzz::scratch_world()
        };
        let (_, counters) = fuzz::apply_after_in(world, &frames[..at], &input, &frames[at + 1..])
            .unwrap_or_else(|message| panic!("iteration {iteration}: {message}; input {input:02x?}"));
        errors += counters.record_errors;
    }
    assert!(errors > 0, "the mutations must reach the record decoders");
}
