// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (lane testgaps-fluid-2d-xr): the MAC grid, the FNV digest and the seeded generator.
//! Fixed inputs, hand-computed expectations, no clock.

use glam::Vec3;
use regia_fluid::grid::{FaceField, Fnv1a, GridDims, MacVelocity, Rng};

fn dims() -> GridDims {
    GridDims {
        nx: 4,
        ny: 3,
        nz: 2,
        dx: 0.5,
        origin: Vec3::new(-1.0, 2.0, 0.0),
    }
}

/// `cells`, `cell` (x fastest), `max`, `local` and `cell_center` against hand-computed values.
#[test]
fn grid_dims_index_and_placement_math() {
    let d = dims();
    assert_eq!(d.cells(), 24);
    assert_eq!(d.cell(0, 0, 0), 0);
    assert_eq!(d.cell(3, 0, 0), 3);
    assert_eq!(d.cell(0, 1, 0), 4);
    assert_eq!(d.cell(0, 0, 1), 12);
    assert_eq!(d.cell(3, 2, 1), 23);
    assert_eq!(d.max(), Vec3::new(1.0, 3.5, 1.0));
    assert_eq!(d.local(Vec3::new(0.0, 3.0, 0.25)), Vec3::new(2.0, 2.0, 0.5));
    assert_eq!(d.cell_center(0, 0, 0), Vec3::new(-0.75, 2.25, 0.25));
    assert_eq!(d.cell_center(3, 2, 1), Vec3::new(0.75, 3.25, 0.75));
}

/// `cell_of` at the borders: the minimum corner is inside, the maximum corner and anything
/// past it, negative, NaN or infinite is outside.
#[test]
fn cell_of_border_cases() {
    let d = dims();
    assert_eq!(d.cell_of(d.origin), Some((0, 0, 0)));
    assert_eq!(d.cell_of(Vec3::new(-0.51, 2.49, 0.49)), Some((0, 0, 0)));
    assert_eq!(d.cell_of(Vec3::new(-0.5, 2.5, 0.5)), Some((1, 1, 1)));
    assert_eq!(d.cell_of(Vec3::new(0.99, 3.49, 0.99)), Some((3, 2, 1)));
    assert_eq!(d.cell_of(d.max()), None, "the max corner belongs to no cell");
    assert_eq!(d.cell_of(Vec3::new(-1.001, 2.5, 0.5)), None);
    assert_eq!(d.cell_of(Vec3::new(0.0, 1.999, 0.5)), None);
    assert_eq!(d.cell_of(Vec3::new(f32::NAN, 2.5, 0.5)), None);
    assert_eq!(d.cell_of(Vec3::new(0.0, f32::INFINITY, 0.5)), None);
    assert_eq!(d.cell_of(Vec3::new(f32::NEG_INFINITY, 2.5, 0.5)), None);
}

/// Face array sizes are one longer along their own axis, and the sample offsets are 0 there and
/// 0.5 elsewhere (the staggering).
#[test]
fn face_fields_are_staggered() {
    let d = dims();
    assert_eq!(d.face_dims(0), [5, 3, 2]);
    assert_eq!(d.face_dims(1), [4, 4, 2]);
    assert_eq!(d.face_dims(2), [4, 3, 3]);
    for axis in 0..3 {
        let f = FaceField::new(&d, axis);
        let mut off = Vec3::splat(0.5);
        off[axis] = 0.0;
        assert_eq!(f.offset, off);
        assert_eq!(f.data.len(), f.size.iter().product::<usize>());
        assert!(f.data.iter().all(|v| *v == 0.0));
    }
    let f = FaceField::new(&d, 0);
    assert_eq!(f.idx(4, 2, 1), f.data.len() - 1);
    assert_eq!(f.idx(1, 0, 0), 1);
    assert_eq!(f.idx(0, 1, 0), 5);
}

fn filled(axis: usize, f: impl Fn(Vec3) -> f32) -> FaceField {
    let d = dims();
    let mut field = FaceField::new(&d, axis);
    for k in 0..field.size[2] {
        for j in 0..field.size[1] {
            for i in 0..field.size[0] {
                let p = Vec3::new(i as f32, j as f32, k as f32) + field.offset;
                let idx = field.idx(i, j, k);
                field.data[idx] = f(p);
            }
        }
    }
    field
}

/// The trilinear kernel's weights are non-negative, sum to one and index inside the array, for
/// points inside, on the border, outside and non-finite.
#[test]
fn weights_are_a_partition_of_unity_everywhere() {
    let d = dims();
    let probes = [
        Vec3::new(0.3, 1.7, 0.4),
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(4.0, 3.0, 2.0),
        Vec3::new(-5.0, 10.0, 7.0),
        Vec3::new(f32::NAN, 1.0, 1.0),
        Vec3::new(1.0, f32::INFINITY, 1.0),
        Vec3::new(2.0, 1.5, f32::NEG_INFINITY),
    ];
    for axis in 0..3 {
        let f = FaceField::new(&d, axis);
        for q in probes {
            let w = f.weights(q);
            let sum: f32 = w.iter().map(|(_, w)| *w).sum();
            assert!((sum - 1.0).abs() < 1e-5, "axis {axis} q {q:?}: sum {sum}");
            for (i, wt) in w {
                assert!(i < f.data.len(), "axis {axis} q {q:?}: index {i}");
                assert!(
                    (0.0..=1.0 + 1e-6).contains(&wt),
                    "axis {axis} q {q:?}: weight {wt}"
                );
            }
        }
    }
}

/// Trilinear interpolation reproduces an affine field exactly inside the sampled range.
#[test]
fn sampling_reproduces_a_linear_field() {
    let lin = |p: Vec3| 2.0 * p.x - 3.0 * p.y + 0.5 * p.z + 1.0;
    for axis in 0..3 {
        let f = filled(axis, lin);
        for q in [
            f.offset + Vec3::new(0.25, 0.5, 0.75),
            f.offset + Vec3::new(1.5, 1.25, 0.5),
            f.offset + Vec3::ZERO,
        ] {
            let got = f.sample(q);
            assert!(
                (got - lin(q)).abs() < 1e-4,
                "axis {axis} q {q:?}: {got} vs {}",
                lin(q)
            );
        }
    }
}

/// Outside the field the sample holds the edge value (clamped), never extrapolates, never NaN.
#[test]
fn sampling_clamps_outside_the_field() {
    let f = filled(0, |p| p.x * 10.0);
    let hi = f.sample(Vec3::new(100.0, 1.0, 1.0));
    let lo = f.sample(Vec3::new(-100.0, 1.0, 1.0));
    assert!((hi - 40.0).abs() < 1e-4, "{hi}");
    assert!(lo.abs() < 1e-4, "{lo}");
    assert!(f.sample(Vec3::new(f32::NAN, f32::NAN, f32::NAN)).is_finite());
}

/// A one-face-thick field (size 1 on an axis) does not index out of range.
#[test]
fn degenerate_single_cell_grid_samples() {
    let d = GridDims {
        nx: 1,
        ny: 1,
        nz: 1,
        dx: 1.0,
        origin: Vec3::ZERO,
    };
    let mut v = MacVelocity::new(&d);
    for c in &mut v.c {
        c.data.iter_mut().for_each(|x| *x = 2.0);
    }
    let s = v.sample(Vec3::new(0.5, 0.5, 0.5));
    assert!((s - Vec3::splat(2.0)).length() < 1e-5, "{s:?}");
    let s = v.sample(Vec3::new(9.0, -9.0, 0.0));
    assert!((s - Vec3::splat(2.0)).length() < 1e-5, "{s:?}");
}

/// `MacVelocity::sample` combines the three components on their own staggering.
#[test]
fn mac_velocity_samples_each_component() {
    let d = dims();
    let mut v = MacVelocity::new(&d);
    v.c[0].data.iter_mut().for_each(|x| *x = 1.0);
    v.c[1].data.iter_mut().for_each(|x| *x = -2.0);
    v.c[2].data.iter_mut().for_each(|x| *x = 3.0);
    let s = v.sample(Vec3::new(1.7, 1.3, 0.6));
    assert!((s - Vec3::new(1.0, -2.0, 3.0)).length() < 1e-5, "{s:?}");
}

/// FNV-1a 64 published test vectors, and `write_f32` hashes the little-endian bits.
#[test]
fn fnv1a_matches_the_published_vectors() {
    assert_eq!(Fnv1a::new().finish(), 0xcbf2_9ce4_8422_2325);
    let mut h = Fnv1a::default();
    h.write(b"a");
    assert_eq!(h.finish(), 0xaf63_dc4c_8601_ec8c);
    let mut h = Fnv1a::new();
    h.write(b"foobar");
    assert_eq!(h.finish(), 0x8594_4171_f739_67e8);
    let mut a = Fnv1a::new();
    a.write_f32(1.5);
    let mut b = Fnv1a::new();
    b.write(&1.5f32.to_bits().to_le_bytes());
    assert_eq!(a.finish(), b.finish());
    // Split writes equal one write.
    let mut c = Fnv1a::new();
    c.write(b"foo");
    c.write(b"bar");
    assert_eq!(c.finish(), 0x8594_4171_f739_67e8);
    // -0.0 and 0.0 are different states (different bits): the digest must see it.
    let mut z = Fnv1a::new();
    z.write_f32(0.0);
    let mut nz = Fnv1a::new();
    nz.write_f32(-0.0);
    assert_ne!(z.finish(), nz.finish());
}

/// The generator is a pure function of its seed, never sticks at zero, and its floats stay in range.
#[test]
fn rng_is_deterministic_bounded_and_never_stuck() {
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);
    for _ in 0..1000 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
    assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    // The seed that would XOR to zero internally still produces a moving stream.
    let mut z = Rng::new(0x9e37_79b9_7f4a_7c15);
    let first = z.next_u64();
    let second = z.next_u64();
    assert_ne!(first, 0);
    assert_ne!(first, second);
    let mut r = Rng::new(7);
    let (mut sum, n) = (0.0f64, 20_000);
    for _ in 0..n {
        let u = r.unit();
        assert!((0.0..1.0).contains(&u), "{u}");
        sum += f64::from(u);
        let s = r.signed();
        assert!((-1.0..1.0).contains(&s), "{s}");
    }
    let mean = sum / f64::from(n);
    assert!((mean - 0.5).abs() < 0.02, "mean {mean}");
}
