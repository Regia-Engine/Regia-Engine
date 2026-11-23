// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 11-04 (ADR-2705): edge cases of the pure, untrusted-input and arithmetic parts of
//! `regia_net`, aimed at functions the coverage run showed under-exercised. Deterministic: no clocks,
//! no sockets; time is passed in as numbers.

use glam::{Quat, Vec3};
use regia_net::bits::{BitReader, BitWriter, DecodeError, varint_bits};
use regia_net::codec::{Angle, Quantize, Rotation, UnitVec};
use regia_net::join_code::{mint_join_code, normalize_join_code};
use regia_net::lagcomp::{
    ClaimInvalid, LagCompError, LagCompHistory, LagCompQueryError, history_report, last_queries_report,
    stats_report,
};
use regia_net::message::{Channel, Direction};
use regia_net::model::{NetMode, NetRole};
use regia_net::profile::{InterpolationMode, NetProfile, ProfileError, RelevanceShape};
use regia_net::rate_limit::{ReliableRateLimiter, ReliableSendGovernor};
use regia_net::send_clock::{client_full_due, is_send_tick, send_interval_ticks, stagger_period};
use regia_net::server_host::config::{SERVER_TOML_FORMAT_VERSION, SERVER_TOML_MIN_FORMAT_VERSION};
use regia_net::server_host::{CliOverrides, ServerSettings, resolve_config};
use regia_net::session::PeerId;
use regia_net::session::meter::{BandwidthView, ConnectionStats, DirMeter, LinkMeter};
use regia_net::snapshot_delta::FieldKind;
use regia_net::tick::{
    InputBuffer, InputBundle, InputHistory, MAX_BUNDLE_INPUTS, TICK_HISTORY, TickClock, TickDriver,
};
use regia_net::wire::{WireError, WireReader, WireWriter, fnv1a_64};

// ---- wire ----

#[test]
fn wire_scalars_round_trip_little_endian() {
    let mut w = WireWriter::new();
    w.u8(0xAB);
    w.u16(0x1234);
    w.u32(0xDEAD_BEEF);
    w.u64(u64::MAX - 1);
    w.i32(i32::MIN);
    w.f32(-0.0);
    assert_eq!(w.len(), 1 + 2 + 4 + 8 + 4 + 4);
    assert_eq!(&w.as_slice()[1..3], &[0x34, 0x12], "little endian");
    let bytes = w.finish();
    let mut r = WireReader::new(&bytes);
    assert_eq!(r.u8(), Ok(0xAB));
    assert_eq!(r.u16(), Ok(0x1234));
    assert_eq!(r.u32(), Ok(0xDEAD_BEEF));
    assert_eq!(r.u64(), Ok(u64::MAX - 1));
    assert_eq!(r.i32(), Ok(i32::MIN));
    assert!(r.f32().is_ok_and(|v| v == 0.0 && v.is_sign_negative()));
    assert_eq!(r.finish(), Ok(()));
}

#[test]
fn wire_truncated_reads_report_position_and_do_not_advance() {
    let bytes = [1u8, 2, 3];
    let mut r = WireReader::new(&bytes);
    assert_eq!(r.u16(), Ok(0x0201));
    assert_eq!(r.u32(), Err(WireError::Truncated { at: 2, needed: 4 }));
    assert_eq!(r.position(), 2, "a failed read leaves the cursor");
    assert_eq!(r.remaining(), 1);
    assert_eq!(r.u8(), Ok(3));
    assert!(r.is_empty());
    assert_eq!(r.u8(), Err(WireError::Truncated { at: 3, needed: 1 }));
}

#[test]
fn wire_finish_reports_trailing_bytes() {
    let mut r = WireReader::new(&[9, 9, 9]);
    assert_eq!(r.u8(), Ok(9));
    assert_eq!(r.finish(), Err(WireError::Trailing(2)));
}

#[test]
fn wire_length_prefixed_fields_enforce_their_maximum() {
    let mut w = WireWriter::new();
    assert_eq!(
        w.bytes_u16(&[0; 5], 4),
        Err(WireError::TooLong { len: 5, max: 4 })
    );
    assert!(w.is_empty(), "a refused field writes nothing");
    // A max above the prefix width is clamped to what the prefix can say.
    assert_eq!(
        w.str_u8(&"x".repeat(256), 1000),
        Err(WireError::TooLong { len: 256, max: 255 })
    );
    assert!(w.is_empty());
    w.bytes_u16(&[7, 8], 4).expect("fits");
    w.str_u8("héllo", 16).expect("fits");
    let bytes = w.finish();
    let mut r = WireReader::new(&bytes);
    assert_eq!(r.bytes_u16(4), Ok(&[7u8, 8][..]));
    assert_eq!(r.str_u8(16), Ok("héllo"));
    assert_eq!(r.finish(), Ok(()));
}

#[test]
fn wire_a_hostile_length_word_is_refused_before_it_is_read() {
    // Length 0xFFFF with only two bytes behind it, and a maximum of 8.
    let bytes = [0xFF, 0xFF, 1, 2];
    assert_eq!(
        WireReader::new(&bytes).bytes_u16(8),
        Err(WireError::TooLong { len: 0xFFFF, max: 8 })
    );
    // Within the maximum but not backed by bytes: truncated, not a panic.
    let bytes = [0x05, 0x00, 1, 2];
    assert!(matches!(
        WireReader::new(&bytes).bytes_u16(8),
        Err(WireError::Truncated { .. })
    ));
}

#[test]
fn wire_strings_must_be_utf8() {
    let bytes = [2u8, 0xC3, 0x28];
    assert_eq!(WireReader::new(&bytes).str_u8(8), Err(WireError::Utf8));
}

#[test]
fn fnv1a_matches_published_vectors() {
    // FNV-1a 64 test vectors from the algorithm's public description.
    assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
}

// ---- bits ----

#[test]
fn varint_bits_matches_the_written_size_at_every_group_boundary() {
    for value in [0u64, 1, 127, 128, 16_383, 16_384, 1 << 35, u64::MAX] {
        let mut w = BitWriter::new();
        w.write_varint(value);
        assert_eq!(w.bit_len(), varint_bits(value), "value {value}");
        let bytes = w.finish();
        assert_eq!(BitReader::new(&bytes).read_varint(), Ok(value));
    }
    assert_eq!(varint_bits(0), 8);
    assert_eq!(varint_bits(128), 16);
    assert_eq!(varint_bits(u64::MAX), 80);
}

#[test]
fn varint_overflow_and_overlong_encodings_are_refused() {
    // Ten groups whose last one carries more than the one bit that still fits.
    let mut bytes = vec![0xFF; 9];
    bytes.push(0x7F);
    assert!(matches!(
        BitReader::new(&bytes).read_varint(),
        Err(DecodeError::Invalid(_))
    ));
    // Eleven continuation bytes.
    assert!(matches!(
        BitReader::new(&[0x80; 11]).read_varint(),
        Err(DecodeError::Invalid(_))
    ));
    // The largest value is accepted: nine full groups and a final 1.
    let mut max = vec![0xFF; 9];
    max.push(0x01);
    assert_eq!(BitReader::new(&max).read_varint(), Ok(u64::MAX));
}

#[test]
fn bit_reader_refuses_widths_above_64_and_short_input() {
    let mut r = BitReader::new(&[0xFF]);
    assert!(matches!(r.read_bits(65), Err(DecodeError::Invalid(_))));
    assert_eq!(r.read_bits(3), Ok(7));
    assert_eq!(r.remaining_bits(), 5);
    assert_eq!(r.read_bits(6), Err(DecodeError::Truncated { at: 3, needed: 6 }));
    assert_eq!(r.bit_pos(), 3, "a failed read leaves the cursor");
    assert_eq!(r.read_bits(0), Ok(0));
}

#[test]
fn bit_writer_masks_high_bits_and_ignores_zero_widths() {
    let mut w = BitWriter::new();
    w.write_bits(0xFF, 4);
    w.write_bits(u64::MAX, 0);
    assert_eq!(w.bit_len(), 4);
    assert_eq!(w.as_bytes(), &[0x0F]);
    w.write_bits(u64::MAX, 64);
    assert_eq!(w.bit_len(), 68);
    let bytes = w.finish();
    let mut r = BitReader::new(&bytes);
    assert_eq!(r.read_bits(4), Ok(0xF));
    assert_eq!(r.read_bits(64), Ok(u64::MAX));
}

#[test]
fn bit_writer_append_at_every_bit_offset_equals_writing_in_place() {
    for offset in 0..8u32 {
        for tail in [1usize, 5, 8, 13, 24] {
            let mut head_then_tail = BitWriter::new();
            head_then_tail.write_bits(0b1011_0101, offset);
            let mut tail_only = BitWriter::new();
            for i in 0..tail {
                let bit = (i * 7 + 3) % 2 == 0;
                head_then_tail.write_bool(bit);
                tail_only.write_bool(bit);
            }
            let mut appended = BitWriter::new();
            appended.write_bits(0b1011_0101, offset);
            appended.append(&tail_only);
            assert_eq!(
                appended.bit_len(),
                head_then_tail.bit_len(),
                "offset {offset} tail {tail}"
            );
            assert_eq!(
                appended.as_bytes(),
                head_then_tail.as_bytes(),
                "offset {offset} tail {tail}"
            );
        }
    }
}

#[test]
fn bit_writer_append_slice_clamps_its_bit_count_to_the_slice() {
    let mut w = BitWriter::new();
    w.append_slice(&[0xAB], 100);
    assert_eq!(w.bit_len(), 8);
    w.append_slice(&[], 4);
    assert_eq!(w.bit_len(), 8, "an empty slice adds nothing");
    assert_eq!(w.as_bytes(), &[0xAB]);
}

#[test]
fn read_len_enforces_its_maximum_and_bytes_check_the_remainder() {
    let mut w = BitWriter::new();
    w.write_varint(10);
    w.write_bytes(&[1, 2, 3]);
    let bytes = w.finish();
    let mut r = BitReader::new(&bytes);
    assert_eq!(r.read_len(9), Err(DecodeError::TooLong { len: 10, max: 9 }));
    let mut r = BitReader::new(&bytes);
    assert_eq!(r.read_len(10), Ok(10));
    assert!(matches!(r.read_bytes(10), Err(DecodeError::Truncated { .. })));
    assert_eq!(r.read_bytes(3), Ok(vec![1, 2, 3]));
}

#[test]
fn floats_round_trip_bit_exactly_including_nan_payloads() {
    let nan = f32::from_bits(0x7FC0_1234);
    let mut w = BitWriter::new();
    w.write_bool(true);
    w.write_f32(nan);
    w.write_f64(f64::NEG_INFINITY);
    let bytes = w.finish();
    let mut r = BitReader::new(&bytes);
    assert_eq!(r.read_bool(), Ok(true));
    assert_eq!(r.read_f32().map(f32::to_bits), Ok(0x7FC0_1234));
    assert_eq!(r.read_f64(), Ok(f64::NEG_INFINITY));
}

// ---- codec quantizers ----

#[test]
fn quantize_reports_clamping_and_pins_the_ends() {
    let q = Quantize {
        min: -10.0,
        max: 10.0,
        bits: 8,
    };
    assert_eq!(q.quantize(-10.0), (0, false));
    assert_eq!(q.quantize(10.0), (255, false));
    assert_eq!(q.quantize(-11.0), (0, true));
    assert_eq!(q.quantize(11.0), (255, true));
    assert_eq!(q.quantize(f32::NAN), (0, true), "NaN clamps to the minimum");
    assert_eq!(q.quantize(f32::INFINITY), (255, true));
    assert_eq!(q.dequantize(0), -10.0);
    assert!((q.dequantize(255) - 10.0).abs() < 1e-4);
    assert!(
        (q.dequantize(u64::MAX) - 10.0).abs() < 1e-4,
        "an out-of-range code is clamped"
    );
}

#[test]
fn quantize_error_is_at_most_half_a_step() {
    let q = Quantize {
        min: 0.0,
        max: 100.0,
        bits: 10,
    };
    let half_step = 100.0 / 1023.0 / 2.0 + 1e-4;
    for i in 0..=400 {
        let v = i as f32 * 0.25;
        let back = q.dequantize(q.quantize(v).0);
        assert!((back - v).abs() <= half_step, "v {v} back {back}");
    }
}

#[test]
fn quantize_with_an_empty_or_nan_range_is_a_clamped_zero() {
    for q in [
        Quantize {
            min: 1.0,
            max: 1.0,
            bits: 8,
        },
        Quantize {
            min: 2.0,
            max: 1.0,
            bits: 8,
        },
        Quantize {
            min: f32::NAN,
            max: 1.0,
            bits: 8,
        },
    ] {
        assert_eq!(q.quantize(0.5), (0, true));
    }
    // Widths are clamped into 1..=32 rather than shifting out of range.
    let wide = Quantize {
        min: 0.0,
        max: 1.0,
        bits: 99,
    };
    assert_eq!(wide.quantize(1.0).0, u64::from(u32::MAX));
    let zero = Quantize {
        min: 0.0,
        max: 1.0,
        bits: 0,
    };
    assert_eq!(zero.quantize(1.0).0, 1);
}

#[test]
fn angle_wraps_at_pi_and_rejects_non_finite_input() {
    let a = Angle { bits: 8 };
    let q_pi = a.quantize(std::f32::consts::PI);
    let q_minus_pi = a.quantize(-std::f32::consts::PI);
    assert_eq!(q_pi, q_minus_pi, "+pi and -pi are one direction");
    assert_eq!(a.quantize(0.0), 128);
    // Adding whole turns changes nothing.
    assert_eq!(a.quantize(1.0), a.quantize(1.0 + std::f32::consts::TAU * 3.0));
    assert_eq!(a.quantize(f32::NAN), 0);
    assert_eq!(a.quantize(f32::INFINITY), 0);
    let step = std::f32::consts::TAU / 256.0;
    for k in 0..256u64 {
        let v = a.dequantize(k);
        assert_eq!(a.quantize(v), k, "code {k}");
        assert!(v >= -std::f32::consts::PI - step && v < std::f32::consts::PI);
    }
    assert_eq!(a.dequantize(256 + 5), a.dequantize(5), "codes wrap");
}

#[test]
fn unit_vec_round_trips_every_octant_and_the_degenerate_input() {
    let u = UnitVec { bits: 16 };
    for x in [-1.0f32, -0.3, 0.0, 0.7] {
        for y in [-0.9f32, 0.0, 0.4] {
            for z in [-1.0f32, -0.2, 0.0, 0.5, 1.0] {
                let v = Vec3::new(x, y, z);
                if v.length() < 1e-3 {
                    continue;
                }
                let n = v.normalize();
                let (qx, qy) = u.quantize(n);
                let back = u.dequantize(qx, qy);
                assert!((back.length() - 1.0).abs() < 1e-3, "{n:?}");
                assert!(back.dot(n) > 0.9995, "{n:?} came back {back:?}");
            }
        }
    }
    // Zero and non-finite vectors map to a defined direction, not NaN.
    for v in [
        Vec3::ZERO,
        Vec3::new(f32::NAN, 0.0, 0.0),
        Vec3::splat(f32::INFINITY),
    ] {
        let (qx, qy) = u.quantize(v);
        let back = u.dequantize(qx, qy);
        assert!(back.is_finite() && (back.length() - 1.0).abs() < 1e-3, "{v:?}");
    }
}

#[test]
fn rotation_round_trip_keeps_the_orientation_and_ignores_the_quaternion_sign() {
    let r = Rotation { bits: 12 };
    let samples = [
        Quat::IDENTITY,
        Quat::from_rotation_x(1.2),
        Quat::from_rotation_y(-2.9),
        Quat::from_axis_angle(Vec3::new(1.0, 2.0, 3.0).normalize(), 3.0),
        Quat::from_rotation_z(std::f32::consts::PI),
    ];
    for q in samples {
        let (largest, kept) = r.quantize(q);
        assert!(largest < 4);
        let back = r.dequantize(largest, kept);
        assert!(back.dot(q).abs() > 0.9995, "{q:?} came back {back:?}");
        let (l2, k2) = r.quantize(-q);
        assert_eq!((l2, k2), (largest, kept), "q and -q are one rotation");
    }
    // Unusable input becomes the identity; an out-of-range index is clamped.
    let (l, k) = r.quantize(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0));
    let back = r.dequantize(l, k);
    assert!(back.dot(Quat::IDENTITY).abs() > 0.999);
    assert!(r.dequantize(99, [0, 0, 0]).is_finite());
}

// ---- join codes, rate limiters, send clock ----

#[test]
fn join_codes_are_a_pure_function_of_the_seed() {
    assert_eq!(mint_join_code(0), mint_join_code(0));
    assert_ne!(mint_join_code(0), mint_join_code(u64::MAX));
    let code = mint_join_code(42);
    let typed = format!(" {}-{} ", code[..3].to_lowercase(), &code[3..]);
    assert_eq!(normalize_join_code(&typed).as_deref(), Some(code.as_str()));
    assert!(normalize_join_code("").is_none());
}

#[test]
fn reliable_limiter_window_resets_exactly_at_one_second() {
    let mut l = ReliableRateLimiter::new(2);
    assert!(l.try_admit(1_000));
    assert!(l.try_admit(1_999));
    assert!(!l.try_admit(1_999), "cap used up inside the window");
    assert!(!l.try_admit(1_999));
    assert_eq!(l.refused_total(), 2);
    assert!(
        l.try_admit(2_000),
        "a full second after the window start opens a new one"
    );
    // A clock that steps backwards does not reopen the window.
    let mut l = ReliableRateLimiter::new(1);
    assert!(l.try_admit(5_000));
    assert!(!l.try_admit(4_000));
    let mut off = ReliableRateLimiter::new(0);
    assert!((0..1000).all(|i| off.try_admit(i)));
    assert_eq!(off.refused_total(), 0);
}

#[test]
fn send_governor_limits_each_peer_alone_and_forgets_removed_peers() {
    let mut g: ReliableSendGovernor<u32> = ReliableSendGovernor::new(1);
    assert!(g.try_admit(1, 0));
    assert!(!g.try_admit(1, 10));
    assert!(g.try_admit(2, 10), "another peer has its own window");
    assert_eq!(g.refused(&1), 1);
    assert_eq!(g.refused(&2), 0);
    g.remove(&1);
    assert_eq!(g.refused(&1), 0, "a removed peer's count is gone");
    assert!(g.try_admit(1, 20), "and its window starts over");
}

#[test]
fn send_clock_never_divides_by_zero_and_staggers_clients_evenly() {
    assert_eq!(send_interval_ticks(60, 20), 3);
    assert_eq!(send_interval_ticks(60, 0), 60);
    assert_eq!(send_interval_ticks(0, 0), 1);
    assert_eq!(
        send_interval_ticks(10, 60),
        1,
        "send rate above tick rate sends every tick"
    );
    assert!(!is_send_tick(5, 0));
    assert!(is_send_tick(0, 3) && is_send_tick(6, 3) && !is_send_tick(7, 3));
    assert_eq!(stagger_period(20, 0), 20);
    assert_eq!(stagger_period(5, 20), 1);
    // Over one period every client gets exactly one full state.
    let period = stagger_period(20, 4);
    assert_eq!(period, 5);
    for client in 0..10u64 {
        let due = (0..period)
            .filter(|t| client_full_due(client, *t, period))
            .count();
        assert_eq!(due, 1, "client {client}");
    }
    assert!(client_full_due(7, 12, 0), "period 0 is treated as 1");
}

// ---- tick: input buffers, the tick clock, the driver, redundant input bundles ----

#[test]
fn input_buffer_evicts_the_oldest_only_for_a_newer_input() {
    let mut b = InputBuffer::new(3);
    for t in [10u64, 11, 12] {
        assert!(b.insert(t, t));
    }
    assert!(
        !b.insert(9, 9),
        "full and older than everything: dropped, nothing evicted"
    );
    assert_eq!(b.first_tick(), Some(10));
    assert!(b.insert(13, 13), "a newer input pushes the oldest out");
    assert_eq!(b.first_tick(), Some(11));
    assert_eq!(b.len(), 3);
    assert!(!b.has(10) && b.has(13));
    // Capacity 0 is treated as 1.
    let mut one = InputBuffer::new(0);
    assert!(one.insert(1, 'a'));
    assert!(one.insert(2, 'b'));
    assert_eq!(one.len(), 1);
}

#[test]
fn input_buffer_take_discards_older_inputs_and_blocks_replays() {
    let mut b = InputBuffer::new(8);
    for t in [3u64, 4, 5, 7] {
        b.insert(t, t * 10);
    }
    assert_eq!(b.take(5), Some(50), "older inputs 3 and 4 are dropped on the way");
    assert_eq!(b.len(), 1);
    assert!(!b.insert(5, 0), "the consumed tick cannot be refilled");
    assert!(!b.insert(4, 0));
    assert_eq!(b.take(6), None, "missing tick");
    assert!(!b.insert(6, 0), "taking a missing tick still closes it");
    assert_eq!(b.take(7), Some(70));
    assert!(b.is_empty());
    assert_eq!(b.take(2), None, "taking an older tick does not reopen the past");
    assert!(!b.insert(7, 1));
}

#[test]
fn tick_clock_carries_the_remainder_and_caps_the_catch_up() {
    let mut c = TickClock::new(60, 5);
    assert_eq!(c.tick_us(), 16_666);
    assert_eq!(c.advance(0), 0);
    // 100 ms = 6.0006 ticks of 16 666 us, but at most 5 run.
    assert_eq!(c.advance(100), 5);
    // The spiral is cut: the leftover debt is dropped, not carried.
    assert_eq!(c.advance(0), 0);
    // Ordinary frames keep the fraction: 3 x 16.666 ms is just under 3 ticks of exact time.
    let mut c = TickClock::new(60, 5);
    let total: u32 = (0..60).map(|_| c.advance(17)).sum();
    assert_eq!(total, 61, "60 frames of 17 ms = 1020 ms = 61.2 ticks at 60 Hz");
    // The rate is clamped into 1..=1000 and the catch-up cap is at least 1.
    assert_eq!(TickClock::new(0, 0).tick_rate_hz(), 1);
    assert_eq!(TickClock::new(1_000_000, 1).tick_rate_hz(), 1000);
    assert_eq!(TickClock::new(1, 0).advance(5000), 1);
    assert_eq!(
        TickClock::new(30, 3).advance(u64::MAX),
        3,
        "huge elapsed time saturates"
    );
}

#[test]
fn tick_driver_hands_over_inputs_in_peer_order_and_counts_misses() {
    let mut d: TickDriver<u32> = TickDriver::new(8);
    let (a, b) = (PeerId(2), PeerId(1));
    d.add_peer(a);
    d.add_peer(b);
    d.add_peer(a); // idempotent
    assert_eq!(
        d.peers().collect::<Vec<_>>(),
        vec![b, a],
        "peers iterate in id order"
    );
    assert!(d.push_input(a, 0, 200));
    assert!(d.push_input(b, 0, 100));
    assert!(d.push_input(a, 1, 201));
    assert!(!d.push_input(PeerId(9), 0, 0), "unknown peer");
    assert!(d.all_ready_for(0));
    assert!(!d.all_ready_for(1), "b has nothing for tick 1");
    let mut seen = Vec::new();
    let got = d.advance(|t, i| seen.push((t, i.len())));
    assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![(b, 100), (a, 200)]);
    let got = d.advance(|t, i| seen.push((t, i.len())));
    assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![(a, 201)]);
    assert_eq!(seen, vec![(0, 2), (1, 1)]);
    assert_eq!((d.missed(a), d.missed(b)), (0, 1));
    assert_eq!(d.tick(), 2);
    assert_eq!(d.inputs_at(0).map(|i| i.len()), Some(2));
    assert_eq!(d.inputs_at(1).map(|i| i.len()), Some(1));
    assert!(d.inputs_at(2).is_none());
    d.remove_peer(b);
    assert_eq!(d.missed(b), 0, "a removed peer's count is forgotten");
    assert_eq!(d.peers().collect::<Vec<_>>(), vec![a]);
}

#[test]
fn tick_driver_refuses_inputs_for_the_past_and_too_far_ahead() {
    let mut d: TickDriver<u8> = TickDriver::new(4);
    let p = PeerId(1);
    d.add_peer(p);
    assert!(d.push_input(p, 3, 1), "within the window");
    assert!(!d.push_input(p, 4, 1), "a window of 4 ticks ends at tick 3");
    d.advance(|_, _| {});
    d.advance(|_, _| {});
    assert!(!d.push_input(p, 1, 1), "the past");
    assert!(d.push_input(p, 5, 1), "the window moved with the tick");
    // An empty driver is never "ready".
    let e: TickDriver<u8> = TickDriver::new(4);
    assert!(!e.all_ready_for(0));
}

#[test]
fn tick_driver_keeps_a_bounded_history() {
    let mut d: TickDriver<u8> = TickDriver::new(2);
    d.add_peer(PeerId(1));
    for _ in 0..(TICK_HISTORY as u64 + 10) {
        d.advance(|_, _| {});
    }
    assert!(d.inputs_at(0).is_none(), "the oldest ticks left the history");
    assert!(d.inputs_at(9).is_none());
    assert!(d.inputs_at(10).is_some());
    assert!(d.inputs_at(TICK_HISTORY as u64 + 9).is_some());
}

#[test]
fn input_bundles_round_trip_and_stamp_each_input() {
    let b = InputBundle {
        first_tick: 100,
        inputs: vec![7u32, 8, 9],
    };
    let mut w = WireWriter::new();
    b.encode(&mut w);
    let bytes = w.finish();
    let back = InputBundle::<u32>::decode(&mut WireReader::new(&bytes)).expect("decodes");
    assert_eq!(back, b);
    assert_eq!(
        back.iter().collect::<Vec<_>>(),
        vec![(100, &7), (101, &8), (102, &9)]
    );
    // Stamps saturate at the end of the tick space instead of wrapping.
    let top = InputBundle {
        first_tick: u64::MAX,
        inputs: vec![1u8, 2],
    };
    assert_eq!(
        top.iter().map(|(t, _)| t).collect::<Vec<_>>(),
        vec![u64::MAX, u64::MAX]
    );
}

#[test]
fn an_oversized_bundle_keeps_the_newest_inputs_with_the_right_first_tick() {
    let inputs: Vec<u32> = (0..(MAX_BUNDLE_INPUTS as u32 + 5)).collect();
    let b = InputBundle {
        first_tick: 1000,
        inputs,
    };
    let mut w = WireWriter::new();
    b.encode(&mut w);
    let bytes = w.finish();
    let back = InputBundle::<u32>::decode(&mut WireReader::new(&bytes)).expect("decodes");
    assert_eq!(back.inputs.len(), MAX_BUNDLE_INPUTS);
    assert_eq!(
        back.first_tick, 1005,
        "the five oldest were dropped and the stamp moved with them"
    );
    assert_eq!(back.inputs[0], 5);
    assert_eq!(back.inputs.last(), Some(&(MAX_BUNDLE_INPUTS as u32 + 4)));
}

#[test]
fn a_hostile_bundle_count_or_short_body_is_refused() {
    let mut w = WireWriter::new();
    w.u64(0);
    w.u8(MAX_BUNDLE_INPUTS as u8 + 1);
    let bytes = w.finish();
    assert!(matches!(
        InputBundle::<u32>::decode(&mut WireReader::new(&bytes)),
        Err(WireError::TooLong { .. })
    ));
    let mut w = WireWriter::new();
    w.u64(0);
    w.u8(3);
    w.u32(1); // two inputs missing
    let bytes = w.finish();
    assert!(matches!(
        InputBundle::<u32>::decode(&mut WireReader::new(&bytes)),
        Err(WireError::Truncated { .. })
    ));
}

#[test]
fn input_history_repeats_the_last_n_and_restarts_on_a_gap() {
    let mut h: InputHistory<u32> = InputHistory::new(3);
    assert!(h.bundle().is_none());
    for t in 10..15u64 {
        h.push(t, t as u32);
    }
    let b = h.bundle().expect("bundle");
    assert_eq!((b.first_tick, b.inputs.clone()), (12, vec![12, 13, 14]));
    h.push(20, 20); // a jump: the old run is not contiguous with it
    let b = h.bundle().expect("bundle");
    assert_eq!((b.first_tick, b.inputs), (20, vec![20]));
    // Redundancy is clamped into 1..=MAX_BUNDLE_INPUTS.
    let mut zero: InputHistory<u8> = InputHistory::new(0);
    zero.push(1, 1);
    zero.push(2, 2);
    assert_eq!(zero.bundle().map(|b| b.inputs.len()), Some(1));
    let mut big: InputHistory<u8> = InputHistory::new(1000);
    for t in 0..100u64 {
        big.push(t, 0);
    }
    assert_eq!(big.bundle().map(|b| b.inputs.len()), Some(MAX_BUNDLE_INPUTS));
}

#[test]
fn input_history_at_the_end_of_the_tick_space_does_not_overflow() {
    let mut h: InputHistory<u8> = InputHistory::new(4);
    h.push(u64::MAX - 1, 1);
    h.push(u64::MAX, 2);
    h.push(0, 3); // wrapped: not contiguous, starts over
    assert_eq!(h.bundle().map(|b| (b.first_tick, b.inputs)), Some((0, vec![3])));
}

// ---- vocabulary ----

#[test]
fn channel_direction_mode_and_role_names_are_distinct_snake_case() {
    let all: Vec<&str> = [
        Channel::ReliableOrdered,
        Channel::ReliableUnordered,
        Channel::Unreliable,
        Channel::UnreliableSequenced,
    ]
    .iter()
    .map(|c| c.as_str())
    .chain(
        [Direction::ToServer, Direction::ToOwner, Direction::Broadcast]
            .iter()
            .map(|d| d.as_str()),
    )
    .chain(
        [
            NetMode::Offline,
            NetMode::DedicatedServer,
            NetMode::ListenServer,
            NetMode::Client,
        ]
        .iter()
        .map(|m| m.as_str()),
    )
    .chain(
        [
            NetRole::Local,
            NetRole::Authority,
            NetRole::Predicted,
            NetRole::Proxy,
        ]
        .iter()
        .map(|r| r.as_str()),
    )
    .collect();
    let unique: std::collections::BTreeSet<_> = all.iter().collect();
    assert_eq!(unique.len(), all.len(), "{all:?}");
    assert!(
        all.iter()
            .all(|n| n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    );
    assert!(Channel::ReliableOrdered.is_reliable() && Channel::ReliableUnordered.is_reliable());
    assert!(!Channel::Unreliable.is_reliable() && !Channel::UnreliableSequenced.is_reliable());
}

#[test]
fn net_modes_partition_into_server_and_client_roles() {
    let table = [
        (NetMode::Offline, true, false, true, false),
        (NetMode::DedicatedServer, true, false, false, true),
        (NetMode::ListenServer, true, false, true, true),
        (NetMode::Client, false, true, true, false),
    ];
    for (m, server, client, local, accepts) in table {
        assert_eq!(m.is_server(), server, "{}", m.as_str());
        assert_eq!(m.is_client(), client, "{}", m.as_str());
        assert_eq!(m.has_local_players(), local, "{}", m.as_str());
        assert_eq!(m.accepts_connections(), accepts, "{}", m.as_str());
        assert_ne!(m.is_server(), m.is_client(), "exactly one of server or client");
    }
    assert_eq!(NetMode::default(), NetMode::Offline);
}

#[test]
fn authority_belongs_to_local_and_authority_roles_only() {
    use regia_net::model::has_authority;
    assert!(has_authority(&NetRole::Local) && has_authority(&NetRole::Authority));
    assert!(!has_authority(&NetRole::Predicted) && !has_authority(&NetRole::Proxy));
}

// ---- snapshot field kinds ----

#[test]
fn field_kinds_report_their_wire_width() {
    assert_eq!(FieldKind::Bool.wire_bits(), Some(1));
    assert_eq!(FieldKind::U8.wire_bits(), Some(8));
    assert_eq!(FieldKind::U16.wire_bits(), Some(16));
    assert_eq!(FieldKind::U32.wire_bits(), Some(32));
    assert_eq!(
        FieldKind::I32.wire_bits(),
        None,
        "a zig-zag varint has no fixed width"
    );
    assert_eq!(
        FieldKind::Float {
            min: 0.0,
            max: 1.0,
            bits: 12
        }
        .wire_bits(),
        Some(12)
    );
    assert_eq!(
        FieldKind::Float {
            min: 0.0,
            max: 1.0,
            bits: 0
        }
        .wire_bits(),
        Some(1),
        "width clamps up to 1"
    );
    assert_eq!(
        FieldKind::Float {
            min: 0.0,
            max: 1.0,
            bits: 99
        }
        .wire_bits(),
        Some(32),
        "and down to 32"
    );
    let v = FieldKind::Vec3Quantised {
        min: Vec3::splat(-1.0),
        max: Vec3::splat(1.0),
        bits: 10,
    };
    assert_eq!(v.wire_bits(), Some(30));
    assert_eq!(
        FieldKind::QuatSmallestThree { bits: 10 }.wire_bits(),
        Some(32),
        "2 index bits + 3 x 10"
    );
    assert_eq!(
        FieldKind::QuatSmallestThree { bits: 0 }.wire_bits(),
        Some(8),
        "component width clamps up to 2"
    );
}

#[test]
fn enum_fields_take_the_bits_of_the_largest_variant_index() {
    let bits = |variants| FieldKind::Enum { variants }.wire_bits();
    assert_eq!(bits(0), Some(1));
    assert_eq!(bits(1), Some(1));
    assert_eq!(bits(2), Some(1));
    assert_eq!(bits(3), Some(2));
    assert_eq!(bits(4), Some(2));
    assert_eq!(bits(5), Some(3));
    assert_eq!(bits(256), Some(8));
    assert_eq!(bits(257), Some(9));
    assert_eq!(bits(u32::MAX), Some(32));
}

#[test]
fn quantisation_error_bounds_shrink_with_width_and_are_exact_for_integers() {
    for exact in [
        FieldKind::Bool,
        FieldKind::U8,
        FieldKind::U16,
        FieldKind::U32,
        FieldKind::I32,
        FieldKind::Enum { variants: 3 },
    ] {
        assert_eq!(exact.max_quantisation_error(), Some(0.0), "{}", exact.name());
    }
    assert_eq!(
        FieldKind::QuatSmallestThree { bits: 10 }.max_quantisation_error(),
        None
    );
    let err = |bits| {
        FieldKind::Float {
            min: 0.0,
            max: 100.0,
            bits,
        }
        .max_quantisation_error()
        .expect("bounded")
    };
    assert!(err(8) > err(12) && err(12) > err(16));
    assert!(err(8) >= 100.0 / 255.0 / 2.0, "at least half a step");
    // The vector bound is the widest axis.
    let narrow = FieldKind::Vec3Quantised {
        min: Vec3::ZERO,
        max: Vec3::new(1.0, 1.0, 1.0),
        bits: 10,
    };
    let wide = FieldKind::Vec3Quantised {
        min: Vec3::ZERO,
        max: Vec3::new(1.0, 1.0, 64.0),
        bits: 10,
    };
    assert!(wide.max_quantisation_error() > narrow.max_quantisation_error());
    assert!(
        FieldKind::Float {
            min: 0.0,
            max: 100.0,
            bits: 99
        }
        .max_quantisation_error()
        .is_some_and(f32::is_finite)
    );
}

#[test]
fn field_kind_names_are_stable_and_distinct() {
    let kinds = [
        FieldKind::Bool,
        FieldKind::U8,
        FieldKind::U16,
        FieldKind::U32,
        FieldKind::I32,
        FieldKind::Float {
            min: 0.0,
            max: 1.0,
            bits: 8,
        },
        FieldKind::Vec3Quantised {
            min: Vec3::ZERO,
            max: Vec3::ONE,
            bits: 8,
        },
        FieldKind::QuatSmallestThree { bits: 8 },
        FieldKind::Enum { variants: 2 },
    ];
    let names: std::collections::BTreeSet<_> = kinds.iter().map(FieldKind::name).collect();
    assert_eq!(names.len(), kinds.len());
}

// ---- net profiles ----

#[test]
fn the_default_profile_validates_and_round_trips_through_text() {
    let p = NetProfile::default();
    p.validate().expect("default is valid");
    let text = p.to_ron().expect("writes");
    let back = NetProfile::from_ron(&text).expect("reads");
    assert_eq!(back, p);
    assert_eq!(back.interp_profile().interpolation, p.interpolation);
    assert_eq!(back.interp_profile().snap_distance_m, p.snap_distance_m);
}

#[test]
fn profile_validation_names_the_offending_field() {
    let field = |p: NetProfile| match p.validate() {
        Err(ProfileError::Invalid { field, .. }) => field,
        other => panic!("expected an Invalid error, got {other:?}"),
    };
    let base = NetProfile::default();
    assert_eq!(
        field(NetProfile {
            priority: 0.0,
            ..base.clone()
        }),
        "priority"
    );
    assert_eq!(
        field(NetProfile {
            priority: f32::NAN,
            ..base.clone()
        }),
        "priority"
    );
    assert_eq!(
        field(NetProfile {
            min_interval_ticks: 0,
            ..base.clone()
        }),
        "min_interval_ticks"
    );
    assert_eq!(
        field(NetProfile {
            min_interval_ticks: 4,
            max_interval_ticks: Some(3),
            ..base.clone()
        }),
        "max_interval_ticks"
    );
    assert_eq!(
        field(NetProfile {
            owner_boost: 0.5,
            ..base.clone()
        }),
        "owner_boost"
    );
    assert_eq!(
        field(NetProfile {
            position_step_m: Some(0.0),
            ..base.clone()
        }),
        "position_step_m"
    );
    assert_eq!(
        field(NetProfile {
            position_step_m: Some(2.0),
            ..base.clone()
        }),
        "position_step_m"
    );
    assert_eq!(
        field(NetProfile {
            snap_distance_m: -1.0,
            ..base.clone()
        }),
        "snap_distance_m"
    );
    assert_eq!(
        field(NetProfile {
            max_extrapolate_ms: f32::INFINITY,
            ..base.clone()
        }),
        "max_extrapolate_ms"
    );
    assert_eq!(
        field(NetProfile {
            relevance: RelevanceShape::Spatial {
                radius: 0.0,
                hysteresis: 0.1
            },
            ..base.clone()
        }),
        "relevance.radius"
    );
    assert_eq!(
        field(NetProfile {
            relevance: RelevanceShape::Spatial {
                radius: 10.0,
                hysteresis: 1.5
            },
            ..base.clone()
        }),
        "relevance.hysteresis"
    );
    // The boundaries themselves are valid.
    NetProfile {
        position_step_m: Some(1.0),
        ..base.clone()
    }
    .validate()
    .expect("1.0 is allowed");
    NetProfile {
        position_step_m: Some(1e-5),
        ..base.clone()
    }
    .validate()
    .expect("1e-5 is allowed");
    NetProfile {
        min_interval_ticks: 3,
        max_interval_ticks: Some(3),
        ..base
    }
    .validate()
    .expect("equal is allowed");
}

#[test]
fn spatial_radius_includes_hysteresis_and_world_relevance_has_none() {
    let spatial = NetProfile {
        relevance: RelevanceShape::Spatial {
            radius: 50.0,
            hysteresis: 0.2,
        },
        ..NetProfile::default()
    };
    assert!((spatial.spatial_radius().expect("spatial") - 60.0).abs() < 1e-4);
    let world = NetProfile {
        relevance: RelevanceShape::World,
        ..NetProfile::default()
    };
    assert_eq!(world.spatial_radius(), None);
    let conflict = NetProfile {
        owner_only: true,
        sent_to_all: true,
        ..NetProfile::default()
    };
    assert!(conflict.is_conflicting());
    assert!(
        !NetProfile {
            owner_only: true,
            ..NetProfile::default()
        }
        .is_conflicting()
    );
}

#[test]
fn profile_text_from_a_newer_build_or_with_garbage_is_refused() {
    let newer = NetProfile {
        format_version: 999,
        ..NetProfile::default()
    }
    .to_ron()
    .expect("writes");
    assert!(matches!(
        NetProfile::from_ron(&newer),
        Err(ProfileError::TooNew { found: 999, .. })
    ));
    assert!(matches!(
        NetProfile::from_ron("(priority: )"),
        Err(ProfileError::Parse(_))
    ));
    assert!(matches!(NetProfile::from_ron(""), Err(ProfileError::Parse(_))));
    let invalid = NetProfile {
        priority: -1.0,
        ..NetProfile::default()
    }
    .to_ron()
    .expect("writes");
    assert!(matches!(
        NetProfile::from_ron(&invalid),
        Err(ProfileError::Invalid {
            field: "priority",
            ..
        })
    ));
    assert_eq!(InterpolationMode::Hermite.as_str(), "hermite");
    assert_eq!(InterpolationMode::Linear.as_str(), "linear");
    assert_eq!(InterpolationMode::None.as_str(), "none");
}

// ---- server.toml ----

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-net-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn fields(errors: Vec<regia_net::server_host::FieldError>) -> Vec<String> {
    errors.into_iter().map(|e| e.field).collect()
}

#[test]
fn an_empty_file_gives_the_defaults() {
    // The defaults carry no level rotation, so an empty file is refused for that one field.
    let err = ServerSettings::from_toml("").expect_err("a server needs a level");
    assert_eq!(fields(err), vec!["levels.rotation".to_string()]);
    let s = ServerSettings::from_toml("[levels]\nrotation = [\"a.level.ron\"]").expect("valid");
    assert_eq!(
        s,
        ServerSettings {
            rotation: vec!["a.level.ron".to_string()],
            ..ServerSettings::default()
        }
    );
    assert!(s.validate().is_empty());
}

#[test]
fn every_documented_key_is_read() {
    let text = r#"
format_version = 1
[server]
name = "Arena"
bind = "0.0.0.0"
port = 9000
max_players = 16
password = "pw"
tick_hz = 30
idle_tick_hz = 5
motd = "hello"
[levels]
rotation = ["levels/a.level.ron", "levels/b.level.ron"]
start_index = 1
match_end_delay_s = 7
[net]
client_rate_bytes = 1000
server_rate_bytes = 2000
send_every_ticks = 2
encryption = "off"
identity_file = "id.key"
[admin]
token_file = "admin.tok"
[status]
enabled = true
bind = "127.0.0.2"
port = 9100
[logs]
dir = "logs/x"
keep_files = 3
"#;
    let s = ServerSettings::from_toml(text).expect("valid");
    assert_eq!(
        (s.name.as_str(), s.port, s.max_players, s.tick_hz, s.idle_tick_hz),
        ("Arena", 9000, 16, 30, 5)
    );
    assert_eq!(s.bind.to_string(), "0.0.0.0");
    assert_eq!(s.rotation.len(), 2);
    assert_eq!((s.start_index, s.match_end_delay_s), (1, 7.0));
    assert_eq!(
        (s.client_rate_bytes, s.server_rate_bytes, s.send_every_ticks),
        (Some(1000), Some(2000), Some(2))
    );
    assert_eq!(s.status_port, 9100);
    assert_eq!(
        (s.log_dir.as_str(), s.keep_log_files, s.admin_token_file.as_str()),
        ("logs/x", 3, "admin.tok")
    );
    assert_eq!(s.game_addr().to_string(), "0.0.0.0:9000");
    assert_eq!(s.status_addr().to_string(), "127.0.0.2:9100");
}

#[test]
fn wrong_types_and_bad_values_name_their_field() {
    let cases = [
        ("[server]\nport = \"x\"", "server.port"),
        ("[server]\nname = 5", "server.name"),
        ("[server]\nbind = \"not an ip\"", "server.bind"),
        ("[levels]\nrotation = [1, 2]", "levels.rotation"),
        ("[levels]\nstart_index = -1", "levels.start_index"),
        ("[levels]\nmatch_end_delay_s = -3", "levels.match_end_delay_s"),
        ("[net]\nencryption = \"maybe\"", "net.encryption"),
        ("[status]\nenabled = 1", "status.enabled"),
        ("server = 3", "server"),
        ("format_version = \"one\"", "format_version"),
    ];
    for (text, field) in cases {
        let err = ServerSettings::from_toml(text).expect_err(text);
        assert!(fields(err).contains(&field.to_string()), "{text}");
    }
    // Versions outside what this build reads: one past the newest, one below the oldest.
    for version in [SERVER_TOML_FORMAT_VERSION + 1, SERVER_TOML_MIN_FORMAT_VERSION - 1] {
        let text = format!("format_version = {version}");
        let err = ServerSettings::from_toml(&text).expect_err(&text);
        assert!(fields(err).contains(&"format_version".to_string()), "{text}");
    }
    let err = ServerSettings::from_toml("this is = = not toml").expect_err("syntax");
    assert_eq!(fields(err), vec!["server.toml".to_string()]);
}

#[test]
fn unknown_keys_are_ignored_not_fatal() {
    ServerSettings::from_toml(
        "future_key = 1\n[server]\nmystery = true\n[extra]\nx = 1\n[levels]\nrotation = [\"a\"]",
    )
    .expect("unknown keys only warn");
}

#[test]
fn validation_checks_every_range_and_reports_all_of_them() {
    let base = ServerSettings {
        rotation: vec!["a".to_string()],
        ..ServerSettings::default()
    };
    let check = |s: ServerSettings| fields(s.validate());
    assert!(
        check(ServerSettings {
            port: 0,
            ..base.clone()
        })
        .contains(&"server.port".to_string())
    );
    assert!(
        check(ServerSettings {
            port: 65_536,
            ..base.clone()
        })
        .contains(&"server.port".to_string())
    );
    assert!(
        check(ServerSettings {
            port: 7778,
            status_port: 7778,
            ..base.clone()
        })
        .contains(&"server.port".to_string())
    );
    assert!(
        check(ServerSettings {
            max_players: 0,
            ..base.clone()
        })
        .contains(&"server.max_players".to_string())
    );
    assert!(
        check(ServerSettings {
            max_players: 257,
            ..base.clone()
        })
        .contains(&"server.max_players".to_string())
    );
    assert!(
        check(ServerSettings {
            tick_hz: 9,
            ..base.clone()
        })
        .contains(&"server.tick_hz".to_string())
    );
    assert!(
        check(ServerSettings {
            tick_hz: 241,
            ..base.clone()
        })
        .contains(&"server.tick_hz".to_string())
    );
    assert!(
        check(ServerSettings {
            idle_tick_hz: 0,
            ..base.clone()
        })
        .contains(&"server.idle_tick_hz".to_string())
    );
    assert!(
        check(ServerSettings {
            send_every_ticks: Some(0),
            ..base.clone()
        })
        .contains(&"net.send_every_ticks".to_string())
    );
    assert!(
        check(ServerSettings {
            rotation: vec![],
            ..base.clone()
        })
        .contains(&"levels.rotation".to_string())
    );
    assert!(
        check(ServerSettings {
            start_index: 1,
            ..base.clone()
        })
        .contains(&"levels.start_index".to_string()),
        "one level, index 1"
    );
    assert!(
        check(ServerSettings {
            format_version: 9,
            ..base.clone()
        })
        .contains(&"format_version".to_string())
    );
    // The edges are valid.
    for ok in [
        ServerSettings {
            port: 1,
            status_port: 2,
            ..base.clone()
        },
        ServerSettings {
            port: 65_535,
            ..base.clone()
        },
        ServerSettings {
            max_players: 256,
            tick_hz: 240,
            idle_tick_hz: 240,
            ..base.clone()
        },
        ServerSettings {
            tick_hz: 10,
            ..base.clone()
        },
    ] {
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());
    }
    // Several faults are all reported in one pass.
    let many = check(ServerSettings {
        port: 0,
        max_players: 0,
        tick_hz: 0,
        ..base
    });
    assert!(many.len() >= 3, "{many:?}");
}

#[test]
fn the_restart_only_and_hot_change_lists_partition_the_edits() {
    let a = ServerSettings::default();
    assert!(a.restart_only_changes(&a).is_empty() && a.hot_changes(&a).is_empty());
    let b = ServerSettings {
        port: a.port + 1,
        tick_hz: a.tick_hz + 1,
        ..a.clone()
    };
    let restart = a.restart_only_changes(&b);
    assert!(
        restart.contains(&"server.port") && restart.contains(&"server.tick_hz"),
        "{restart:?}"
    );
    let c = ServerSettings {
        motd: "new".into(),
        max_players: a.max_players + 1,
        ..a.clone()
    };
    let hot = a.hot_changes(&c);
    assert!(
        !hot.is_empty(),
        "a message of the day or a player cap applies without a restart"
    );
    for key in &hot {
        assert!(!restart.contains(key), "{key} is on both lists");
    }
    assert!(a.restart_only_changes(&c).iter().all(|k| !hot.contains(k)));
}

#[test]
fn command_line_beats_environment_beats_file_beats_defaults() {
    let dir = unique_temp_dir("tq1104_server_cfg");
    std::fs::write(
        dir.join("server.toml"),
        "[server]\nport = 9001\nmax_players = 10\nname = \"file\"\n[levels]\nrotation = [\"a\"]\n",
    )
    .expect("write");
    let env = |k: &str| match k {
        "REGIA_SERVER_PORT" => Some("9002".to_string()),
        "REGIA_SERVER_MAX_PLAYERS" => Some("20".to_string()),
        _ => None,
    };
    let none = CliOverrides::default();
    let r = resolve_config(&dir, &none, &env).expect("resolves");
    assert_eq!(
        (r.settings.port, r.settings.max_players, r.settings.name.as_str()),
        (9002, 20, "file"),
        "env over file"
    );
    assert!(r.path.is_some());
    let cli = CliOverrides {
        port: Some("9003".into()),
        name: Some("cli".into()),
        ..CliOverrides::default()
    };
    let r = resolve_config(&dir, &cli, &env).expect("resolves");
    assert_eq!(
        (r.settings.port, r.settings.max_players, r.settings.name.as_str()),
        (9003, 20, "cli"),
        "cli over env"
    );
    let bad_env = |k: &str| (k == "REGIA_SERVER_PORT").then(|| "not a number".to_string());
    let err = resolve_config(&dir, &none, &bad_env).expect_err("bad env");
    assert!(fields(err).contains(&"REGIA_SERVER_PORT".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_default_file_is_fine_but_a_missing_named_file_is_not() {
    let dir = unique_temp_dir("tq1104_server_cfg_missing");
    let env = |_: &str| None;
    let err = resolve_config(&dir, &CliOverrides::default(), &env).expect_err("no level");
    assert_eq!(fields(err), vec!["levels.rotation".to_string()]);
    let with_level = CliOverrides {
        level: Some("a".into()),
        ..CliOverrides::default()
    };
    let r = resolve_config(&dir, &with_level, &env).expect("defaults");
    assert!(r.path.is_none());
    let named = CliOverrides {
        config: Some("nope.toml".into()),
        ..CliOverrides::default()
    };
    let err = resolve_config(&dir, &named, &env).expect_err("named file must exist");
    assert!(fields(err).contains(&"--config".to_string()));
    let level = CliOverrides {
        level: Some("levels/x.level.ron".into()),
        ..CliOverrides::default()
    };
    let r = resolve_config(&dir, &level, &env).expect("ok");
    assert_eq!(
        (r.settings.rotation.clone(), r.settings.start_index),
        (vec!["levels/x.level.ron".to_string()], 0)
    );
    let bad_bind = CliOverrides {
        bind: Some("nonsense".into()),
        ..CliOverrides::default()
    };
    assert!(
        fields(resolve_config(&dir, &bad_bind, &env).expect_err("bad bind")).contains(&"--bind".to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn command_line_parsing_accepts_both_spellings_and_collects_every_error() {
    let args = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    let cli = CliOverrides::parse(&args(&[
        "--port=1",
        "--max-players",
        "4",
        "--agent",
        "--bind",
        "::1",
        "--log-dir=l",
        "--config",
        "c.toml",
    ]))
    .expect("ok");
    assert_eq!(
        (
            cli.port.as_deref(),
            cli.max_players.as_deref(),
            cli.bind.as_deref()
        ),
        (Some("1"), Some("4"), Some("::1"))
    );
    assert!(cli.agent && !cli.console);
    assert_eq!(cli.config.as_deref(), Some(std::path::Path::new("c.toml")));
    let err = CliOverrides::parse(&args(&["--nope", "--also-bad", "--port"])).expect_err("errors");
    assert_eq!(err.len(), 3, "{err:?}");
    assert!(CliOverrides::parse(&[]).expect("empty") == CliOverrides::default());
}

// ---- traffic meters ----

#[test]
fn a_gap_of_more_than_a_second_reads_as_zero_traffic() {
    let mut d = DirMeter::default();
    d.record("game", 100, 100, Some(1.0));
    d.record("game", 100, 1100, Some(1.0)); // rolls bucket 0 into the last full second
    assert_eq!(d.bytes_per_s(1500), 100.0);
    // Two idle seconds later neither the live bucket nor the last full one counts.
    assert_eq!(d.bytes_per_s(3500), 0.0);
    d.record("game", 40, 3600, Some(1.0));
    assert_eq!(d.bytes_per_s(3700), 0.0, "the second before the gap was empty");
    d.record("game", 10, 4100, Some(1.0));
    assert_eq!(d.bytes_per_s(4200), 40.0);
    assert_eq!(d.packets_per_s(4200), 1.0);
    assert_eq!((d.packets, d.bytes), (4, 250), "totals never reset");
}

#[test]
fn round_trip_smoothing_follows_the_documented_gains() {
    let mut d = DirMeter::default();
    d.record("k", 1, 0, Some(40.0));
    assert_eq!(
        (d.delay_ms, d.jitter_ms),
        (40.0, 20.0),
        "the first sample seeds the estimate"
    );
    d.record("k", 1, 1, Some(80.0));
    // jitter += 0.25 * (|40 - 80| - 20); delay += 0.125 * (80 - 40)
    assert!((d.jitter_ms - 25.0).abs() < 1e-9, "{}", d.jitter_ms);
    assert!((d.delay_ms - 45.0).abs() < 1e-9, "{}", d.delay_ms);
    // A lost message counts against loss and leaves the estimate alone.
    d.record("k", 1, 2, None);
    assert_eq!((d.delay_ms, d.lost, d.packets), (45.0, 1, 3));
    assert!((d.loss_fraction() - 1.0 / 3.0).abs() < 1e-6);
    assert_eq!(DirMeter::default().loss_fraction(), 0.0, "no packets, no loss");
}

#[test]
fn link_rtt_and_jitter_add_both_directions() {
    let mut link = LinkMeter::default();
    link.up.record("a", 10, 0, Some(10.0));
    link.down.record("a", 10, 0, Some(30.0));
    assert_eq!(link.rtt_ms(), 40.0);
    assert_eq!(link.jitter_ms(), 5.0 + 15.0);
}

#[test]
fn connection_stats_copy_the_meter_and_print_optional_columns_only_when_set() {
    let mut link = LinkMeter::default();
    link.up.record("game", 500, 100, Some(10.0));
    link.up.record("game", 500, 1100, None);
    link.down.record("game", 200, 100, Some(10.0));
    link.down.record("game", 200, 1100, Some(10.0));
    let stats = ConnectionStats::from_meter("client-1", Some(1), "connected", true, &link, 1500);
    assert_eq!((stats.bytes_out, stats.bytes_in), (1000, 400));
    assert_eq!(stats.bytes_out_per_s, 500.0);
    assert_eq!(stats.packets_per_s, 2.0);
    assert_eq!(stats.loss_out, 0.5);
    assert_eq!(stats.loss_in, 0.0);
    assert!(stats.simulated && stats.peer == Some(1));
    let plain = stats.line();
    assert!(plain.contains("client-1") && plain.contains("connected"));
    assert!(!plain.contains("corr") && !plain.contains("interp"));
    let rich = ConnectionStats {
        corrections_per_s: Some(2.5),
        interp_delay_ms: Some(100.0),
        ..stats
    };
    let line = rich.line();
    assert!(
        line.contains("corr 2.5/s") && line.contains("interp 100 ms"),
        "{line}"
    );
}

#[test]
fn the_bandwidth_view_sums_links_sorts_by_size_and_shares_add_up() {
    let mut a = LinkMeter::default();
    a.up.record("game", 300, 0, Some(1.0));
    a.up.record("chat", 100, 0, Some(1.0));
    a.down.record("game", 100, 0, Some(1.0));
    let mut b = LinkMeter::default();
    b.up.record("game", 100, 0, Some(1.0));
    b.up.record("ack", 100, 0, Some(1.0));
    let view = BandwidthView::from_meters([&a, &b]);
    assert_eq!((view.total_out, view.total_in), (600, 100));
    assert_eq!(view.kinds[0].kind, "game");
    assert_eq!(
        (
            view.kinds[0].bytes_out,
            view.kinds[0].bytes_in,
            view.kinds[0].packets_out
        ),
        (400, 100, 2)
    );
    // Equal sizes sort by name.
    assert_eq!(
        view.kinds.iter().map(|k| k.kind.as_str()).collect::<Vec<_>>(),
        vec!["game", "ack", "chat"]
    );
    let shares: f64 = view.kinds.iter().map(|k| k.share).sum();
    assert!((shares - 1.0).abs() < 1e-9, "{shares}");
    let lines = view.lines();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains("600") && lines[0].contains("100"));
    let empty = BandwidthView::from_meters(std::iter::empty());
    assert!(empty.kinds.is_empty() && empty.lines().len() == 1);
}

// ---- lag compensation reports ----

fn server_world() -> regia_ecs::prelude::World {
    let mut w = regia_ecs::prelude::World::new();
    w.insert_resource(NetMode::DedicatedServer);
    w
}

#[test]
fn lagcomp_reports_refuse_with_their_protocol_codes() {
    let client = {
        let mut w = regia_ecs::prelude::World::new();
        w.insert_resource(NetMode::Client);
        w
    };
    let no_mode = regia_ecs::prelude::World::new();
    let bare_server = server_world();
    for w in [&client, &no_mode] {
        assert_eq!(stats_report(w).expect_err("must fail"), LagCompError::NotServer);
        assert_eq!(
            last_queries_report(w, 5).expect_err("must fail").code(),
            "net_not_server"
        );
    }
    assert_eq!(
        stats_report(&bare_server).expect_err("must fail"),
        LagCompError::NotInstalled
    );
    let entity = regia_ecs::prelude::Entity::PLACEHOLDER;
    assert_eq!(
        history_report(&bare_server, entity)
            .expect_err("must fail")
            .code(),
        "lagcomp_not_installed"
    );
    assert_eq!(
        history_report(&client, entity).expect_err("must fail").code(),
        "net_not_server"
    );
}

#[test]
fn an_installed_but_empty_history_reports_zeroes_and_every_claim_reason() {
    let mut w = server_world();
    w.insert_resource(LagCompHistory::default());
    let stats = stats_report(&w).expect("installed");
    assert_eq!((stats.entities, stats.ticks_kept, stats.range), (0, 0, None));
    assert_eq!(
        (stats.queries_per_s, stats.clamped_per_s, stats.claims_valid),
        (0, 0, 0)
    );
    assert_eq!(stats.mean_rewind_ms, 0.0);
    assert!(!stats.occlusion && !stats.joint_source);
    assert_eq!(stats.claims_invalid_by_reason.len(), ClaimInvalid::ALL.len());
    assert!(stats.claims_invalid_by_reason.values().all(|n| *n == 0));
    assert!(last_queries_report(&w, 10).expect("ok").is_empty());
    assert_eq!(
        history_report(&w, regia_ecs::prelude::Entity::PLACEHOLDER).expect_err("must fail"),
        LagCompQueryError::NotLagCompensated
    );
    assert_eq!(
        LagCompQueryError::NotLagCompensated.code(),
        "entity_not_lag_compensated"
    );
}

#[test]
fn lagcomp_error_and_claim_codes_are_distinct_lowercase_identifiers() {
    let codes: Vec<&str> = [
        LagCompError::NotServer,
        LagCompError::NotInstalled,
        LagCompError::PeerUnknown,
        LagCompError::TickOutOfHistory,
    ]
    .iter()
    .map(|e| e.code())
    .chain(ClaimInvalid::ALL.iter().map(|c| c.code()))
    .collect();
    let unique: std::collections::BTreeSet<_> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "{codes:?}");
    assert!(
        codes
            .iter()
            .all(|c| c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    );
    assert_eq!(
        LagCompQueryError::from(LagCompError::PeerUnknown).code(),
        "peer_unknown"
    );
    let fresh = LagCompHistory::default();
    assert_eq!(fresh.mean_rewind_ticks(), 0.0, "no queries, no rewind");
    assert_eq!(fresh.per_second(), (0, 0));
}
