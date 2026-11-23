// SPDX-License-Identifier: MPL-2.0
//! Independent boundary checks for the wire-6 far record review.

use super::*;

fn grid(n: [i64; 3]) -> QTransform {
    QTransform {
        mode: TransformReplication::PositionOnly,
        pos: QPos::Grid {
            step: DEFAULT_WIRE_STEP_M.to_bits(),
            n,
        },
        rot: None,
        vel: None,
        scale: None,
    }
}

/// Enumerates every representable field value and each rounding boundary in positive and
/// negative cells. The oracle uses division/multiplication independently of the codec shifts.
#[test]
fn every_far_field_value_and_half_step_boundary_round_trips() {
    for (axis, width, stride) in [(0, 15, 32i64), (2, 15, 32), (1, 10, 1024)] {
        for cell in [-2i64, 0, 1] {
            for coarse in 0..(1i64 << width) {
                for offset in [0, stride / 2 - 1, stride / 2, stride - 1] {
                    let mut exact = [0; 3];
                    exact[axis] = cell * CELL_STEPS + coarse * stride + offset;
                    let now = grid(exact);
                    let mut baseline = exact;
                    baseline[axis] = cell * CELL_STEPS;
                    let base = grid(baseline);
                    let rounded = coarse + i64::from(offset >= stride / 2);
                    let far = now.far_against(&base);
                    if rounded == 1 << width {
                        assert!(far.is_none(), "rounding out of the cell must use an exact record");
                        continue;
                    }
                    let far = far.expect("inside the cell");
                    let mut w = BitWriter::new();
                    QTransform::write_far(&far, &base, &mut w);
                    let bits = w.bit_len();
                    assert!(bits <= 41);
                    let bytes = w.finish();
                    let mut r = BitReader::new(&bytes);
                    let back = QTransform::read_far(&base, &mut r).expect("decode");
                    let mut want = exact;
                    want[axis] = cell * CELL_STEPS + rounded * stride;
                    assert_eq!(back.pos, grid(want).pos);
                    assert_eq!(r.bit_pos(), bits);
                    assert!((want[axis] - exact[axis]).abs() <= stride / 2);
                }
            }
        }
    }
}

/// Independently constructed absolute words check x/z/y order and every field's minimum,
/// maximum and one-bit boundaries. Six bytes include the form bit; only 40 are position bits.
#[test]
fn absolute_far_words_have_the_specified_bit_layout() {
    // x = -1 is in cell -1 but rounds out of it: this forces the writer's absolute form.
    let base = grid([-1, CELL_STEPS, 0]);
    for x in [0u64, 1, 16383, 16384, 32767] {
        for z in [0u64, 1, 16383, 16384, 32767] {
            for y in [0u64, 1, 511, 512, 1023] {
                let word = 1 | (x << 1) | (z << 16) | (y << 31);
                let bytes = word.to_le_bytes();
                let mut r = BitReader::new(&bytes);
                let back = QTransform::read_far(&base, &mut r).expect("absolute");
                assert_eq!(r.bit_pos(), 41);
                assert_eq!(
                    back.pos,
                    grid([
                        -CELL_STEPS + x as i64 * 32,
                        CELL_STEPS + y as i64 * 1024,
                        z as i64 * 32
                    ])
                    .pos
                );
                let far = back.far_against(&base).expect("same cells");
                let mut encoded = BitWriter::new();
                QTransform::write_far(&far, &base, &mut encoded);
                assert_eq!(encoded.bit_len(), 41);
                assert_eq!(
                    BitReader::new(&encoded.finish()).read_bits(41).expect("word"),
                    word,
                    "the writer must produce the independently packed x/z/y word"
                );
            }
        }
    }
}

/// The residual code classes change at asymmetric positive/negative zigzag boundaries.
/// Both signs, all axes and both exact/far forms of the baseline must decode identically.
#[test]
fn residual_class_boundaries_and_baseline_forms_agree() {
    let deltas = [
        -32767i64, -8193, -8192, -8191, -1024, -1023, -129, -128, -127, -9, -8, -7, -1, 0, 1, 7, 8, 9, 127,
        128, 129, 1023, 1024, 8191, 8192, 8193, 32767,
    ];
    for (axis, stride, max) in [(0, 32, 32767), (2, 32, 32767), (1, 1024, 1023)] {
        for delta in deltas {
            if delta.abs() > max {
                continue;
            }
            let mut from = [0; 3];
            from[axis] = (-delta).max(0) * stride;
            let base_coarse = grid(from);
            from[axis] += stride / 2 - 1;
            let base_exact = grid(from);
            let mut to = [0; 3];
            to[axis] = delta.max(0) * stride;
            let now = grid(to);
            let mut encodings = Vec::new();
            for base in [base_exact, base_coarse] {
                let far = now.far_against(&base).expect("same cell");
                let mut w = BitWriter::new();
                QTransform::write_far(&far, &base, &mut w);
                let bits = w.bit_len();
                let bytes = w.finish();
                assert_eq!(
                    QTransform::read_far(&base, &mut BitReader::new(&bytes)).expect("decode"),
                    now
                );
                encodings.push((bits, bytes));
            }
            assert_eq!(encodings[0], encodings[1]);
        }
    }
}

/// Malformed residuals include varint overflow, cell escapes and truncation; absolute
/// positions in the lowest partial cell cannot escape the signed-grid bound.
#[test]
fn hostile_far_boundaries_are_refused_without_panicking() {
    let origin = grid([0; 3]);
    for residual in [-1i64, 32768, i64::MIN + 1, i64::MAX] {
        let mut w = BitWriter::new();
        w.write_bool(false);
        w.write_bool(true);
        w.write_bits(3, 2);
        let zigzag = ((residual as u64) << 1) ^ ((residual >> 63) as u64);
        w.write_varint(zigzag - 1);
        w.write_bool(false);
        w.write_bool(false);
        assert!(QTransform::read_far(&origin, &mut BitReader::new(&w.finish())).is_err());
    }
    for malformed in [u64::MAX, u64::MAX - 1] {
        let mut w = BitWriter::new();
        w.write_bool(false);
        w.write_bool(true);
        w.write_bits(3, 2);
        w.write_varint(malformed);
        assert!(QTransform::read_far(&origin, &mut BitReader::new(&w.finish())).is_err());
    }
    let low = grid([-MAX_STEPS, 0, 0]);
    let zero_absolute = [1u8, 0, 0, 0, 0, 0];
    assert!(QTransform::read_far(&low, &mut BitReader::new(&zero_absolute)).is_err());
    for len in 0..6 {
        assert!(QTransform::read_far(&origin, &mut BitReader::new(&zero_absolute[..len])).is_err());
    }
}

#[test]
fn signed_grid_extrema_and_custom_steps_obey_the_integer_error_bound() {
    for step in [0.0005f32, DEFAULT_WIRE_STEP_M, 1.0 / 1024.0, 0.25] {
        for (axis, stride) in [(0, 32i64), (2, 32), (1, 1024)] {
            for edge in [-MAX_STEPS, -CELL_STEPS, 0, CELL_STEPS, MAX_STEPS] {
                for offset in -1025..=1025 {
                    let n = edge + offset;
                    if n.abs() > MAX_STEPS {
                        continue;
                    }
                    let mut now = grid([0; 3]);
                    let mut coordinates = [0; 3];
                    coordinates[axis] = n;
                    now.pos = QPos::Grid {
                        step: step.to_bits(),
                        n: coordinates,
                    };
                    // A valid exact baseline need not itself have a far form.
                    let base = now;
                    let Some(far) = now.far_against(&base) else {
                        let rounded = n.div_euclid(stride) * stride
                            + i64::from(n.rem_euclid(stride) >= stride / 2) * stride;
                        assert!(
                            rounded.abs() > MAX_STEPS
                                || rounded.div_euclid(CELL_STEPS) != n.div_euclid(CELL_STEPS)
                        );
                        continue;
                    };
                    let mut w = BitWriter::new();
                    QTransform::write_far(&far, &base, &mut w);
                    let back = QTransform::read_far(&base, &mut BitReader::new(&w.finish())).expect("far");
                    let QPos::Grid {
                        step: shown_step,
                        n: shown,
                    } = back.pos
                    else {
                        panic!("far must be grid");
                    };
                    assert_eq!(shown_step, step.to_bits());
                    assert!((shown[axis] - n).abs() <= stride / 2);
                    assert_eq!(shown[axis].div_euclid(CELL_STEPS), n.div_euclid(CELL_STEPS));
                    assert!(back.to_net().translation.is_finite());
                }
            }
        }
    }
}
