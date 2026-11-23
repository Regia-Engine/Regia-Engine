// SPDX-License-Identifier: MPL-2.0
//! Unit tests of `net_hover` (ADR-8819): the command and state codecs, the comparison, the hull key,
//! restore-then-step and the cross-target pin. The loopback tests are in `tests/net_hover.rs`.

use regia_detsim::hover::RollingField;
use regia_detsim::hover::WorldPhysics;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{CodecContext, ReplicateCodec};
use regia_net::predict::NetInput;

use super::*;
use crate::det_hover::{DetHoverHull, DetHoverInput, DetHoverState, hop_speed, tick_hull};

const DT: f32 = 1.0 / 60.0;

fn round_trip<C: ReplicateCodec>(v: &C) -> C {
    let cx = CodecContext::detached();
    let mut w = BitWriter::new();
    v.encode(&mut w, &cx);
    let bytes = w.as_bytes().to_vec();
    let mut r = BitReader::new(&bytes);
    C::decode(&mut r, &cx).expect("decode")
}

fn delta_trip<C: ReplicateCodec>(v: &C, base: &C) -> (C, usize) {
    let cx = CodecContext::detached();
    let mut w = BitWriter::new();
    v.encode_delta(base, &mut w, &cx);
    let bits = w.bit_len();
    let bytes = w.as_bytes().to_vec();
    let mut r = BitReader::new(&bytes);
    (C::decode_delta(base, &mut r, &cx).expect("decode delta"), bits)
}

/// The scripted driving of the pin and the restore test: thrust, carve, strafe, hops and a jump
/// over the rolling field.
fn script(t: u32) -> HoverNetInput {
    HoverNetInput {
        throttle: if (t / 90).is_multiple_of(4) { 0.35 } else { 1.0 },
        strafe: if (t / 50).is_multiple_of(3) { -0.6 } else { 0.0 },
        steer: ((t as f32) * 0.021).sin() * 0.8,
        hop: t % 120 == 60,
        jump: t == 333,
    }
}

fn run_ticks(body: &mut HoverBody, from: u32, to: u32, mut each: impl FnMut(u32, &HoverBody)) {
    let hull = DetHoverHull::DEFAULT;
    let world = WorldPhysics::EARTH;
    let speed = hop_speed(&hull, &world, DT);
    for t in from..to {
        let input = script(t).hover_input();
        tick_hull(&hull, body, &input, &RollingField, &world, speed, DT);
        each(t, body);
    }
}

fn state_bits(body: &HoverBody) -> Vec<u32> {
    let (f, b, p) = state_words(body);
    let mut out: Vec<u32> = f.iter().map(|v| v.to_bits()).collect();
    out.extend(b.iter().map(|v| u32::from(*v)));
    out.push(p.map_or(u32::MAX, f32::to_bits));
    out
}

// ---- the command --------------------------------------------------------------------------------

#[test]
fn the_command_travels_quantized_and_quantize_is_idempotent() {
    let raw = HoverNetInput {
        throttle: 0.333,
        strafe: -1.7,
        steer: f32::NAN,
        hop: true,
        jump: false,
    };
    let q = raw.quantized();
    assert_eq!(q, q.quantized(), "idempotent");
    assert_eq!(q.strafe, -1.0, "clamped");
    assert_eq!(q.steer, 0.0, "a NaN axis reads as 0");
    assert_eq!(round_trip(&raw), q, "the wire carries the quantized command");
    assert_eq!(
        round_trip(&HoverNetInput::default()).throttle.to_bits(),
        0.0f32.to_bits(),
        "zero is exact"
    );
}

#[test]
fn the_delta_sends_two_bits_and_a_flag_when_only_buttons_repeat() {
    // A delta's base is a value that travelled (quantized), as on the wire.
    let base = HoverNetInput {
        throttle: 1.0,
        steer: -0.25,
        ..HoverNetInput::default()
    }
    .quantized();
    let next = HoverNetInput { hop: true, ..base };
    let (back, bits) = delta_trip(&next, &base);
    assert_eq!(back, next.quantized());
    assert_eq!(bits, 3, "one change bit and the two buttons");
    let moved = HoverNetInput { strafe: 0.5, ..base };
    let (back, bits) = delta_trip(&moved, &base);
    assert_eq!(back, moved.quantized());
    assert_eq!(bits, 1 + 24 + 2);
}

#[test]
fn an_axis_the_encoder_never_writes_is_refused() {
    let mut w = BitWriter::new();
    w.write_bits(0x80, 8);
    w.write_bits(0, 8);
    w.write_bits(0, 8);
    w.write_bool(false);
    w.write_bool(false);
    let bytes = w.as_bytes().to_vec();
    let mut r = BitReader::new(&bytes);
    let e = HoverNetInput::decode(&mut r, &CodecContext::detached()).expect_err("-128 refused");
    assert_eq!(e, DecodeError::Invalid(CODE_INPUT_RANGE));
}

#[test]
fn a_missing_command_keeps_the_axes_and_drops_the_presses() {
    let last = HoverNetInput {
        throttle: 0.8,
        strafe: 0.2,
        steer: -0.4,
        hop: true,
        jump: true,
    };
    let next = HoverNetInput::on_missing(&last);
    assert_eq!((next.throttle, next.strafe, next.steer), (0.8, 0.2, -0.4));
    assert!(!next.hop && !next.jump);
    let late = HoverNetInput {
        jump: true,
        ..HoverNetInput::default()
    };
    let now = HoverNetInput {
        throttle: 0.5,
        hop: true,
        ..HoverNetInput::default()
    };
    let merged = HoverNetInput::merge_late(&late, &now);
    assert!(merged.hop && merged.jump, "presses are kept");
    assert_eq!(merged.throttle, 0.5, "the newer axes");
}

// ---- the state ----------------------------------------------------------------------------------

#[test]
fn the_state_travels_bit_for_bit() {
    let mut body = HoverBody::at_rest([3.0, 0.0, -2.0], 0.4, &DetHoverHull::DEFAULT.params);
    run_ticks(&mut body, 0, 400, |_, _| {});
    body.state.ground_roll = -0.0;
    let back = round_trip(&DetHoverState(body));
    assert_eq!(state_bits(&back.0), state_bits(&body), "every field, -0.0 kept");
    let unset = DetHoverState(HoverBody::new([1.0, 2.0, 3.0]));
    assert_eq!(unset.0.state.prev_terrain, None);
    assert_eq!(state_bits(&round_trip(&unset).0), state_bits(&unset.0));
}

#[test]
fn the_state_delta_sends_only_what_changed() {
    let mut body = HoverBody::at_rest([0.0, 0.0, 0.0], 0.0, &DetHoverHull::DEFAULT.params);
    run_ticks(&mut body, 0, 200, |_, _| {});
    let base = DetHoverState(body);
    let (same, bits) = delta_trip(&base, &base);
    assert_eq!(state_bits(&same.0), state_bits(&base.0));
    assert_eq!(bits, STATE_FLOATS + STATE_FLAGS + 1, "one bit per field at rest");
    let mut next = body;
    run_ticks(&mut next, 200, 201, |_, _| {});
    let (back, _) = delta_trip(&DetHoverState(next), &base);
    assert_eq!(
        state_bits(&back.0),
        state_bits(&next),
        "a stepped state rebuilt from its base"
    );
}

/// Restore-then-step equals the uninterrupted step: the state saved at tick 150, sent through the
/// wire codec into a fresh body and stepped on, matches the hull that never stopped, bit for bit, at
/// every tick to 600.
#[test]
fn restore_then_step_equals_the_uninterrupted_step() {
    let start = HoverBody::at_rest([0.0, 0.0, 0.0], 0.0, &DetHoverHull::DEFAULT.params);
    let mut straight = start;
    let mut trace = Vec::new();
    run_ticks(&mut straight, 0, 600, |t, b| trace.push((t, state_bits(b))));
    let mut first = start;
    run_ticks(&mut first, 0, 150, |_, _| {});
    let mut restored = round_trip(&DetHoverState(first)).0;
    let mut i = 150;
    run_ticks(&mut restored, 150, 600, |t, b| {
        assert_eq!((t, state_bits(b)), trace[i], "tick {t}");
        i += 1;
    });
    assert_eq!(i, 600);
    assert!(
        straight.state.velocity[2].abs() + straight.state.velocity[0].abs() > 1.0,
        "it moved"
    );
}

/// The same 600 scripted ticks reach the same bits on every target (x86-64, aarch64): the hover
/// step is `regia_detsim` lockstep arithmetic and the command is quantized. The pin is FNV-1a over
/// the wire bytes of the state after each of the 600 ticks.
#[test]
fn the_scripted_run_matches_the_pin() {
    // Re-pinned 2026-11-22 (lead OK): merge 929175099c (v07-det-atan2-canon, ADR-8876, MATH_BITS_VERSION 1)
    // made `math::atan2` correctly rounded, which HoverBody's pitch/roll use; x86-64 (the test machine) and aarch64 (Mac)
    // agree on the new value and final state (integ-v07-freeze-e report, red #4).
    const PIN: u64 = 0xf87c_53f3_6675_2834;
    let mut body = HoverBody::at_rest([0.0, 0.0, 0.0], 0.0, &DetHoverHull::DEFAULT.params);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut hops = 0;
    run_ticks(&mut body, 0, 600, |_, b| {
        let cx = CodecContext::detached();
        let mut w = BitWriter::new();
        DetHoverState(*b).encode(&mut w, &cx);
        for byte in w.as_bytes() {
            h = (h ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        hops += usize::from(b.state.jump_active);
    });
    assert!(hops > 0, "the script jumped or hopped");
    assert_eq!(h, PIN, "pin {h:#018x}; final state {body:?}");
}

// ---- the comparison -----------------------------------------------------------------------------

#[test]
fn the_comparison_follows_the_tolerances() {
    let mut body = HoverBody::at_rest([0.0, 0.0, 0.0], 0.0, &DetHoverHull::DEFAULT.params);
    run_ticks(&mut body, 0, 120, |_, _| {});
    let a = DetHoverState(body);
    assert!(hover_state_matches(&a, &a));
    let mut b = a;
    b.0.pos[0] += 0.005;
    b.0.state.velocity[2] += 0.03;
    b.0.state.hop_cooldown += 1.0 / 480.0;
    assert!(hover_state_matches(&a, &b), "under every tolerance");
    let mut far = a;
    far.0.pos[1] += 0.02;
    assert!(!hover_state_matches(&a, &far), "2 cm off");
    let mut high = a;
    high.0.state.altitude += 0.02;
    assert!(!hover_state_matches(&a, &high), "hover height off");
    let mut flag = a;
    flag.0.state.airborne = !flag.0.state.airborne;
    assert!(!hover_state_matches(&a, &flag), "flags are exact");
    let mut unseeded = a;
    unseeded.0.state.prev_terrain = None;
    assert!(!hover_state_matches(&a, &unseeded));
    let (mut w1, mut w2) = (a, a);
    w1.0.state.yaw = std::f32::consts::PI - 0.002;
    w2.0.state.yaw = -std::f32::consts::PI + 0.002;
    assert!(hover_state_matches(&w1, &w2), "yaw compares wrapped");
    let mut timer = a;
    timer.0.state.hop_cooldown += 0.01;
    assert!(!hover_state_matches(&a, &timer));
}

// ---- the hull key -------------------------------------------------------------------------------

#[test]
fn the_hull_key_names_every_step_input() {
    let base = DetHoverHull::DEFAULT;
    let key = hull_key(&base);
    assert_eq!(key, hull_key(&DetHoverHull::default()));
    let wide = base.with_footprint(2.0, 1.5);
    assert_ne!(hull_key(&wide), key, "footprint");
    let mut zero = base;
    zero.params.ride_height = -0.0;
    let mut plus = base;
    plus.params.ride_height = 0.0;
    assert_ne!(hull_key(&zero), hull_key(&plus), "raw bits");
    let mut clamped = base;
    clamped.flight = regia_detsim::hover::FlightMode::Clamped;
    assert_ne!(hull_key(&clamped), key, "flight mode");
    let mut list = NetHoverHulls::default();
    assert_eq!(list.len(), 1);
    assert_eq!(list.get(key), Some(&base));
    assert_eq!(list.insert(wide), hull_key(&wide));
    assert_eq!(list.insert(wide), hull_key(&wide));
    assert_eq!(list.len(), 2, "listed once");
    assert_eq!(NetHover::for_hull(&wide, 1).hull, hull_key(&wide));
}

#[test]
fn a_held_hull_input_maps_through_the_command() {
    let input = DetHoverInput {
        throttle: 0.5,
        strafe: -0.25,
        steer: 1.0,
        hop: true,
        jump: false,
    };
    let back = HoverNetInput::from_hover_input(&input).hover_input();
    assert_eq!(back.hop, input.hop);
    assert!((back.throttle - 0.5).abs() <= 0.5 / AXIS_STEPS);
    assert_eq!(back.steer, 1.0);
}
