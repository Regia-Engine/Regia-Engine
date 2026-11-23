// SPDX-License-Identifier: MPL-2.0
//! Blend space weights under degenerate layouts and hostile values: duplicate and non-finite
//! sample points, collinear layouts, points far outside the hull, stale triangulations. The
//! contract under test: weights are finite, non-negative, reference real samples and sum to 1.
//! Random-looking points come from a fixed-seed generator.

use regia_animation::blendspace::{BlendSample1D, BlendSample2D, BlendSpace1D, BlendSpace2D};

struct Lcg(u64);
impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn space2d(points: &[[f32; 2]]) -> BlendSpace2D {
    let mut space = BlendSpace2D {
        param_x: "x".into(),
        param_y: "y".into(),
        samples: points
            .iter()
            .enumerate()
            .map(|(i, &point)| BlendSample2D {
                point,
                clip: format!("c{i}"),
            })
            .collect(),
        triangles: vec![],
    };
    space.triangulate();
    space
}

fn space1d(values: &[f32]) -> BlendSpace1D {
    BlendSpace1D {
        param: "p".into(),
        samples: values
            .iter()
            .enumerate()
            .map(|(i, &value)| BlendSample1D {
                value,
                clip: format!("c{i}"),
            })
            .collect(),
    }
}

fn assert_valid(weights: &[(usize, f32)], samples: usize, context: &str) {
    assert!(!weights.is_empty(), "{context}: no weights");
    let mut sum = 0.0;
    for &(i, w) in weights {
        assert!(i < samples, "{context}: index {i} past {samples} samples");
        assert!(w.is_finite() && w >= 0.0, "{context}: weight {w}");
        sum += w;
    }
    assert!((sum - 1.0).abs() < 1e-3, "{context}: sum {sum} of {weights:?}");
}

#[test]
fn one_d_weights_are_a_partition_of_unity_for_any_value() {
    let space = space1d(&[2.0, 0.0, 1.0, 1.0]);
    for v in [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        -9.0,
        0.0,
        0.5,
        1.0,
        1.5,
        2.0,
        9.0,
    ] {
        assert_valid(&space.weights(v), 4, &format!("1D at {v}"));
    }
    // Exact sample points yield one entry.
    assert_eq!(space.weights(0.0).len(), 1);
    // The interpolation is linear between neighbours, whatever the declared order.
    let w = space.weights(0.5);
    assert_eq!(w.len(), 2);
    assert!(w.iter().all(|&(_, weight)| (weight - 0.5).abs() < 1e-6));
    // Empty and non-finite layouts.
    assert!(space1d(&[]).weights(1.0).is_empty());
    assert!(space1d(&[f32::NAN, f32::INFINITY]).weights(1.0).is_empty());
    let one_good = space1d(&[f32::NAN, 3.0]);
    assert_eq!(
        one_good.weights(100.0),
        vec![(1, 1.0)],
        "the NaN sample is ignored"
    );
    // Determinism of repeated queries.
    assert_eq!(space.weights(0.7), space.weights(0.7));
}

#[test]
fn two_d_weights_are_a_partition_of_unity_for_fixed_seed_points() {
    let hull = space2d(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0], [0.5, 0.5]]);
    let mut rng = Lcg(2024);
    for i in 0..300 {
        let p = [rng.unit() * 6.0 - 2.5, rng.unit() * 6.0 - 2.5];
        assert_valid(&hull.weights(p), 5, &format!("hull point #{i} {p:?}"));
    }
    // A point at a sample gets that sample's full weight.
    let w = hull.weights([1.0, 0.0]);
    let top = w
        .iter()
        .cloned()
        .fold((0, 0.0), |a, b| if b.1 > a.1 { b } else { a });
    assert_eq!(top.0, 1);
    assert!(top.1 > 0.999, "{w:?}");
    for p in [
        [f32::NAN, 0.0],
        [0.0, f32::INFINITY],
        [f32::NEG_INFINITY, f32::NAN],
        [1e30, -1e30],
    ] {
        assert_valid(&hull.weights(p), 5, &format!("hull non-finite {p:?}"));
    }
}

#[test]
fn degenerate_two_d_layouts_still_give_valid_weights() {
    let layouts: Vec<(&str, Vec<[f32; 2]>)> = vec![
        ("one", vec![[1.0, 1.0]]),
        ("two", vec![[0.0, 0.0], [2.0, 0.0]]),
        ("two coincident", vec![[1.0, 1.0], [1.0, 1.0]]),
        ("collinear", vec![[0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [3.0, 3.0]]),
        ("all coincident", vec![[1.0, 2.0]; 4]),
        (
            "duplicate corner",
            vec![[0.0, 0.0], [0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
        ),
        ("tiny triangle", vec![[0.0, 0.0], [1e-7, 0.0], [0.0, 1e-7]]),
        ("huge triangle", vec![[0.0, 0.0], [1e15, 0.0], [0.0, 1e15]]),
    ];
    for (name, points) in layouts {
        let space = space2d(&points);
        for p in [[0.0, 0.0], [0.5, 0.5], [1.0, 1.0], [5.0, -5.0], [f32::NAN, 1.0]] {
            assert_valid(&space.weights(p), points.len(), &format!("{name} at {p:?}"));
        }
    }
    assert!(space2d(&[]).weights([0.0, 0.0]).is_empty());
}

#[test]
fn non_finite_sample_points_do_not_hang_or_poison_the_weights() {
    // These must return (the test would never finish on an infinite loop) and keep every
    // weight finite.
    let layouts = [
        vec![[0.0, 0.0], [1.0, 0.0], [f32::NAN, 1.0]],
        vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [f32::INFINITY, 0.5]],
        vec![[f32::NAN, f32::NAN]; 3],
    ];
    for points in layouts {
        let space = space2d(&points);
        let weights = space.weights([0.3, 0.3]);
        for &(i, w) in &weights {
            assert!(i < points.len());
            assert!(w.is_finite(), "{points:?}: {weights:?}");
        }
    }
}

#[test]
fn stale_and_missing_triangulations_are_rebuilt() {
    let mut space = space2d(&[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]);
    let good = space.weights([0.25, 0.25]);
    // Never triangulated.
    let mut untriangulated = space.clone();
    untriangulated.triangles.clear();
    assert_eq!(untriangulated.weights([0.25, 0.25]), good);
    // A sample removed after the triangulation: indices now point past the list.
    space.samples.pop();
    assert_valid(&space.weights([0.25, 0.25]), 3, "stale");
    // Garbage triangles are ignored.
    space.triangles = vec![[0, 1, 99], [7, 8, 9]];
    assert_valid(&space.weights([0.25, 0.25]), 3, "garbage triangles");
    // triangulate is idempotent and deterministic.
    let mut a = space2d(&[[0.0, 0.0], [2.0, 0.0], [1.0, 2.0], [1.0, 0.7]]);
    let first = a.triangles.clone();
    a.triangulate();
    assert_eq!(a.triangles, first);
    assert_eq!(first.len(), 3);
}
