// SPDX-License-Identifier: MPL-2.0
//! Independent geometry checks for trilinear sampling on the three staggered MAC fields.

use glam::Vec3;
use regia_fluid::grid::{GridDims, MacVelocity};
use std::collections::BTreeMap;

const TOLERANCE: f32 = 3.0e-5;

fn affine(axis: usize, p: Vec3) -> f32 {
    let (a, b, c, d) = match axis {
        0 => (0.75, -1.25, 0.5, 2.0),
        1 => (-0.4, 0.9, 1.1, -3.0),
        _ => (1.3, 0.2, -0.65, 0.25),
    };
    a * p.x + b * p.y + c * p.z + d
}

fn face_position(dims: &GridDims, axis: usize, i: usize, j: usize, k: usize) -> Vec3 {
    let mut q = Vec3::new(i as f32, j as f32, k as f32);
    q += Vec3::splat(0.5);
    q[axis] -= 0.5;
    dims.origin + q * dims.dx
}

fn fill_affine(dims: &GridDims) -> MacVelocity {
    let mut velocity = MacVelocity::new(dims);
    for axis in 0..3 {
        let field = &mut velocity.c[axis];
        for k in 0..field.size[2] {
            for j in 0..field.size[1] {
                for i in 0..field.size[0] {
                    let index = field.idx(i, j, k);
                    field.data[index] = affine(axis, face_position(dims, axis, i, j, k));
                }
            }
        }
    }
    velocity
}

/// This reference derives bounds and interpolation fractions from integer face coordinates,
/// independently of `FaceField::stencil` and `FaceField::weights`.
fn reference_weights(field: &regia_fluid::grid::FaceField, q: Vec3) -> BTreeMap<usize, f32> {
    let mut axis_samples = [([0usize; 2], [0.0f32; 2]); 3];
    for axis in 0..3 {
        let position = (q[axis] - field.offset[axis]).clamp(0.0, (field.size[axis] - 1) as f32);
        let low = position.floor() as usize;
        let high = (low + 1).min(field.size[axis] - 1);
        let fraction = position - low as f32;
        axis_samples[axis] = ([low, high], [1.0 - fraction, fraction]);
    }
    let mut weights = BTreeMap::new();
    for z in 0..2 {
        for y in 0..2 {
            for x in 0..2 {
                let index = field.idx(axis_samples[0].0[x], axis_samples[1].0[y], axis_samples[2].0[z]);
                let weight = axis_samples[0].1[x] * axis_samples[1].1[y] * axis_samples[2].1[z];
                if weight > 0.0 {
                    *weights.entry(index).or_default() += weight;
                }
            }
        }
    }
    weights
}

fn reference_sample(field: &regia_fluid::grid::FaceField, q: Vec3) -> f32 {
    reference_weights(field, q)
        .into_iter()
        .map(|(index, weight)| field.data[index] * weight)
        .sum()
}

fn check_weight_contract(field: &regia_fluid::grid::FaceField, q: Vec3) {
    let mut combined = BTreeMap::<usize, f32>::new();
    for (index, weight) in field.weights(q) {
        assert!(
            index < field.data.len(),
            "index {index} outside {}",
            field.data.len()
        );
        assert!(weight >= 0.0 && weight.is_finite(), "invalid weight {weight}");
        *combined.entry(index).or_default() += weight;
    }
    let total: f32 = combined.values().sum();
    assert!(
        (total - 1.0).abs() <= TOLERANCE,
        "weights sum to {total} at {q:?}"
    );
    let expected = reference_weights(field, q);
    for (&index, &actual) in &combined {
        let expected_weight = expected.get(&index).copied().unwrap_or_default();
        assert!(
            (actual - expected_weight).abs() <= TOLERANCE,
            "index {index} weight {actual} != independent {expected_weight} at {q:?}"
        );
    }
    for (index, expected_weight) in expected {
        let actual = combined.get(&index).copied().unwrap_or_default();
        assert!((actual - expected_weight).abs() <= TOLERANCE);
    }

    let constant = 7.25;
    let mut constant_field = field.clone();
    constant_field.data.fill(constant);
    assert!(
        (constant_field.sample(q) - constant).abs() <= TOLERANCE,
        "constant field changed at {q:?}"
    );
}

#[test]
fn staggered_affine_fields_reproduce_unclamped_interior_samples() {
    let cases = [
        (
            GridDims {
                nx: 5,
                ny: 4,
                nz: 3,
                dx: 0.2,
                origin: Vec3::new(-1.3, 2.1, 0.7),
            },
            41,
        ),
        (
            GridDims {
                nx: 3,
                ny: 6,
                nz: 4,
                dx: 0.0375,
                origin: Vec3::new(4.0, -2.5, 9.25),
            },
            29,
        ),
    ];
    for (dims, count) in cases {
        let velocity = fill_affine(&dims);
        // Verify physical placement independently of the field's own offset metadata.
        for axis in 0..3 {
            let mut expected_offset = Vec3::splat(0.5);
            expected_offset[axis] = 0.0;
            assert_eq!(velocity.c[axis].offset, expected_offset, "component {axis}");
            assert_eq!(velocity.c[axis].size, dims.face_dims(axis));
            for cell in [[0, 0, 0], [1, 2, 1]] {
                let (i, j, k) = (cell[0], cell[1], cell[2]);
                if i < dims.face_dims(axis)[0] && j < dims.face_dims(axis)[1] && k < dims.face_dims(axis)[2] {
                    let expected_position = face_position(&dims, axis, i, j, k);
                    assert_eq!(
                        velocity.c[axis].data[velocity.c[axis].idx(i, j, k)],
                        affine(axis, expected_position)
                    );
                }
            }
        }

        let mut seed = 0x6a09_e667_f3bc_c909_u64;
        for _ in 0..count {
            let mut q = Vec3::ZERO;
            for axis in 0..3 {
                seed ^= seed >> 12;
                seed ^= seed << 25;
                seed ^= seed >> 27;
                let bits = seed.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40;
                let unit = bits as f32 / (1u32 << 24) as f32;
                // Leave half a face interval at each edge for every component's full stencil.
                let cell_count = [dims.nx, dims.ny, dims.nz][axis] as f32;
                q[axis] = 0.75 + unit * (cell_count - 1.5);
            }
            let world = dims.origin + q * dims.dx;
            for axis in 0..3 {
                let got = velocity.c[axis].sample(q);
                let want = affine(axis, world);
                assert!(
                    (got - want).abs() <= TOLERANCE * want.abs().max(1.0),
                    "axis {axis}, dims {dims:?}, q {q:?}: {got} != {want}"
                );
            }
            let sampled = velocity.sample(q);
            for axis in 0..3 {
                assert!(
                    (sampled[axis] - affine(axis, world)).abs()
                        <= TOLERANCE * affine(axis, world).abs().max(1.0)
                );
            }
        }
    }
}

#[test]
fn clamped_boundary_and_degenerate_axes_match_independent_reference() {
    let cases = [
        GridDims {
            nx: 1,
            ny: 4,
            nz: 2,
            dx: 0.125,
            origin: Vec3::new(-3.0, 1.25, 7.0),
        },
        GridDims {
            nx: 3,
            ny: 1,
            nz: 5,
            dx: 0.4,
            origin: Vec3::new(0.5, -8.0, 2.0),
        },
        GridDims {
            nx: 2,
            ny: 3,
            nz: 1,
            dx: 1.75,
            origin: Vec3::new(2.0, 3.0, -4.0),
        },
        GridDims {
            nx: 1,
            ny: 1,
            nz: 1,
            dx: 0.3,
            origin: Vec3::new(-0.2, 0.9, 1.1),
        },
    ];
    let points = [
        Vec3::new(-3.0, 0.0, 0.0),
        Vec3::new(0.0, 0.5, 1.5),
        Vec3::new(1.0, 2.0, 3.0),
        Vec3::new(8.0, 9.0, 10.0),
        Vec3::new(-2.0, 1.0, 0.25),
    ];
    for dims in cases {
        let velocity = fill_affine(&dims);
        for q in points {
            for axis in 0..3 {
                check_weight_contract(&velocity.c[axis], q);
                let got = velocity.c[axis].sample(q);
                let want = reference_sample(&velocity.c[axis], q);
                assert!(
                    (got - want).abs() <= TOLERANCE * want.abs().max(1.0),
                    "axis {axis}, dims {dims:?}, q {q:?}: {got} != clamped reference {want}"
                );
            }
        }
    }
}
