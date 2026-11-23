// SPDX-License-Identifier: MPL-2.0
//! Regression coverage for random-access procedural element streams.

use regia_core::math::Rng;
use regia_core::math::hash::mix;
use regia_procgen::seeds::{cell_key, draw, element_rng, point_seed, q};

#[derive(Clone, Copy, Debug)]
struct Key {
    element: u32,
    node: u32,
    slot: u32,
}

fn oracle(key: Key) -> f64 {
    Rng::with_stream(
        (u64::from(key.element) << 32) | u64::from(key.node),
        u64::from(key.slot),
    )
    .f64()
}

#[test]
fn slot_draws_are_identical_across_partitions_orders_and_intervening_consumers() {
    let keys = [
        Key {
            element: 0,
            node: 0,
            slot: 0,
        },
        Key {
            element: u32::MAX,
            node: 7,
            slot: 1,
        },
        Key {
            element: 0x8000_0001,
            node: u32::MAX,
            slot: 17,
        },
        Key {
            element: 42,
            node: 99,
            slot: u32::MAX,
        },
        Key {
            element: 42,
            node: 99,
            slot: 0,
        },
        Key {
            element: 42,
            node: 99,
            slot: 1,
        },
        Key {
            element: 42,
            node: 100,
            slot: 0,
        },
        Key {
            element: 43,
            node: 99,
            slot: 0,
        },
    ];
    let baseline: Vec<_> = keys.iter().copied().map(oracle).collect();
    for (key, expected) in keys.iter().copied().zip(baseline.iter().copied()) {
        assert_eq!(
            draw(key.element, key.node, key.slot).to_bits(),
            expected.to_bits()
        );
    }

    // Forward, reverse, and uneven partitions deliberately request unrelated streams in between.
    let schedules: [&[usize]; 3] = [
        &[0, 1, 2, 3, 4, 5, 6, 7],
        &[7, 6, 5, 4, 3, 2, 1, 0],
        &[3, 0, 6, 1, 7, 4, 2, 5],
    ];
    for schedule in schedules {
        let mut observed = vec![None; keys.len()];
        for chunk in schedule.chunks(3) {
            for index in chunk {
                let key = keys[*index];
                for salt in 0..5 {
                    let mut unrelated = element_rng(0xdead_beef ^ salt, 0xa5a5_5a5a, salt);
                    let _ = unrelated.next_u64();
                    let _ = unrelated.f64();
                }
                let value = draw(key.element, key.node, key.slot);
                assert_eq!(value.to_bits(), baseline[*index].to_bits());
                observed[*index] = Some(value.to_bits());
            }
        }
        assert_eq!(
            observed,
            baseline
                .iter()
                .map(|value| Some(value.to_bits()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn published_stream_vectors_pin_seed_stream_and_first_draw() {
    // Values are the independent explicit-stream Rng oracle, stored as f64 bit patterns.
    let vectors = [
        (
            Key {
                element: 0,
                node: 0,
                slot: 0,
            },
            0x3fec_9828_f106_f38c,
        ),
        (
            Key {
                element: 42,
                node: 99,
                slot: 17,
            },
            0x3fe9_3f86_76cd_0f28,
        ),
        (
            Key {
                element: u32::MAX,
                node: 7,
                slot: 1,
            },
            0x3fef_a780_cc1d_9111,
        ),
    ];
    for (key, expected_bits) in vectors {
        assert_eq!(
            draw(key.element, key.node, key.slot).to_bits(),
            oracle(key).to_bits()
        );
        assert_eq!(oracle(key).to_bits(), expected_bits);
    }
}

#[test]
fn point_quantization_rounds_half_steps_away_from_zero_and_saturates() {
    assert_eq!(q(0.000_499), 0);
    assert_eq!(q(0.000_5), 1);
    assert_eq!(q(0.000_501), 1);
    assert_eq!(q(-0.000_499), 0);
    assert_eq!(q(-0.000_5), u32::MAX);
    assert_eq!(q(-0.000_501), u32::MAX);
    assert_eq!(q(f32::MAX), i32::MAX as u32);
    assert_eq!(q(f32::MIN), i32::MIN as u32);

    let tier = 0x1234_5678;
    assert_eq!(
        point_seed([0.000_499, 0.0, 0.0], tier),
        point_seed([0.0, 0.0, 0.0], tier)
    );
    assert_eq!(
        point_seed([0.000_5, 0.0, 0.0], tier),
        point_seed([0.001, 0.0, 0.0], tier)
    );
    assert_eq!(
        point_seed([-0.000_5, 0.0, 0.0], tier),
        point_seed([-0.001, 0.0, 0.0], tier)
    );
}

#[test]
fn signed_cell_keys_and_repeated_slot_creation_are_stable_and_isolated() {
    let cells = [
        ((-17, 3), mix((-17_i32) as u32, 3)),
        ((3, -17), mix(3, (-17_i32) as u32)),
        ((i32::MIN, i32::MAX), mix(i32::MIN as u32, i32::MAX as u32)),
        ((-1, -1), mix(u32::MAX, u32::MAX)),
        ((0, 0), mix(0, 0)),
        ((1, 1), mix(1, 1)),
    ];
    for ((i, j), expected) in cells {
        assert_eq!(cell_key(i, j), expected);
    }

    let first_slot = draw(0xface_cafe, 0x1020_3040, 9).to_bits();
    let other_slot = draw(0xface_cafe, 0x1020_3040, 10).to_bits();
    for _ in 0..64 {
        assert_eq!(draw(0xface_cafe, 0x1020_3040, 9).to_bits(), first_slot);
    }
    assert_eq!(draw(0xface_cafe, 0x1020_3040, 10).to_bits(), other_slot);
    assert_eq!(
        other_slot,
        oracle(Key {
            element: 0xface_cafe,
            node: 0x1020_3040,
            slot: 10
        })
        .to_bits()
    );
}
