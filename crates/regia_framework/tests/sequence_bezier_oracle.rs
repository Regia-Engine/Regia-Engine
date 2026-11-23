// SPDX-License-Identifier: MPL-2.0
//! Independent parametric and coordinate-covariance checks for weighted sequence Bezier tracks.

use regia_framework::sequencer::{Channel, Ease, ScalarKey, Tangent, Vec3Key};

#[derive(Clone, Copy)]
struct Segment {
    ta: f32,
    tb: f32,
    va: f32,
    vb: f32,
    outgoing: f32,
    incoming: f32,
    outgoing_weight: f32,
    incoming_weight: f32,
}

fn cubic(points: [f64; 4], u: f64) -> f64 {
    let v = 1.0 - u;
    v * v * v * points[0] + 3.0 * v * v * u * points[1] + 3.0 * v * u * u * points[2] + u * u * u * points[3]
}

/// Evaluate the defining parametric curve independently, then supply its time coordinate.
fn oracle(segment: Segment, parameter: f64) -> (f32, f32) {
    let mut wa = f64::from(segment.outgoing_weight);
    let mut wb = f64::from(segment.incoming_weight);
    let total = wa + wb;
    if total > 1.0 {
        wa /= total;
        wb /= total;
    }
    let dt = f64::from(segment.tb) - f64::from(segment.ta);
    let ta = f64::from(segment.ta);
    let va = f64::from(segment.va);
    let vb = f64::from(segment.vb);
    let x = cubic([0.0, wa, 1.0 - wb, 1.0], parameter);
    let y = cubic(
        [
            va,
            va + f64::from(segment.outgoing) * wa * dt,
            vb - f64::from(segment.incoming) * wb * dt,
            vb,
        ],
        parameter,
    );
    ((ta + x * dt) as f32, y as f32)
}

fn assert_close(actual: f32, expected: f32, tolerance: f32, context: &str) {
    let error = (actual - expected).abs();
    assert!(
        error <= tolerance,
        "{context}: got {actual:.9}, expected {expected:.9}, error {error:.3e} > {tolerance:.3e}"
    );
}

fn scalar_channel(segment: Segment) -> Channel {
    Channel::Float(vec![
        ScalarKey {
            tangent: Tangent::Broken(0.0, segment.outgoing),
            weights: Some((0.31, segment.outgoing_weight)),
            ..ScalarKey::new(segment.ta, segment.va, Ease::Bezier)
        },
        ScalarKey {
            tangent: Tangent::Broken(segment.incoming, 0.0),
            weights: Some((segment.incoming_weight, 0.27)),
            ..ScalarKey::new(segment.tb, segment.vb, Ease::Linear)
        },
    ])
}

fn generated_segment(index: usize) -> Segment {
    let n = index as f32;
    let weights = match index % 4 {
        0 => (0.0, 0.42),
        1 => (0.9, 0.8),
        2 => (0.3, 0.2),
        _ => (0.57, 0.68),
    };
    let ta = 0.25 + n * 0.375;
    let dt = 0.75 + (index % 7) as f32 * 0.375;
    Segment {
        ta,
        tb: ta + dt,
        va: -3.0 + (index % 9) as f32 * 0.625,
        vb: 2.75 - (index % 11) as f32 * 0.375,
        outgoing: -1.5 + (index % 8) as f32 * 0.4375,
        incoming: 1.25 - (index % 6) as f32 * 0.5,
        outgoing_weight: weights.0,
        incoming_weight: weights.1,
    }
}

#[test]
fn bounded_scalar_tracks_match_independent_parametric_oracle() {
    // Zero handles and a pair whose unnormalized handle lengths exceed the segment are
    // intentional valid cases; all values and slopes remain far from f32 saturation.
    // Bisection in parameter space avoids relying on the evaluator's time inversion.
    let parameters = [0.0, 1.0e-4, 0.07, 0.23, 0.5, 0.81, 0.9999, 1.0];
    for case_index in 0..16 {
        let segment = generated_segment(case_index);
        let channel = scalar_channel(segment);
        for parameter in parameters {
            let (time, expected) = oracle(segment, parameter);
            let actual = channel.sample_component(time, 0).expect("scalar component");
            assert_close(
                actual,
                expected,
                3.0e-5,
                &format!("case {case_index}, parameter {parameter}"),
            );
        }
    }
}

#[test]
fn affine_value_and_time_coordinates_preserve_the_parametric_curve() {
    let base = Segment {
        ta: 0.75,
        tb: 3.75,
        va: -1.5,
        vb: 2.75,
        outgoing: 1.125,
        incoming: -0.625,
        outgoing_weight: 0.72,
        incoming_weight: 0.61,
    };
    let original = scalar_channel(base);
    let value_scale = -1.75_f32;
    let value_offset = 0.625_f32;
    let value_transformed = scalar_channel(Segment {
        va: value_offset + value_scale * base.va,
        vb: value_offset + value_scale * base.vb,
        outgoing: value_scale * base.outgoing,
        incoming: value_scale * base.incoming,
        ..base
    });

    let time_scale = 3.25_f32;
    let time_offset = 2.5_f32;
    let time_transformed = scalar_channel(Segment {
        ta: time_offset + time_scale * base.ta,
        tb: time_offset + time_scale * base.tb,
        outgoing: base.outgoing / time_scale,
        incoming: base.incoming / time_scale,
        ..base
    });
    for parameter in [0.0, 1.0e-4, 0.11, 0.38, 0.73, 0.9999, 1.0] {
        let (time, expected) = oracle(base, parameter);
        let got = original.sample_component(time, 0).expect("base sample");
        let transformed_value = value_transformed
            .sample_component(time, 0)
            .expect("value transformed sample");
        assert_close(
            transformed_value,
            value_offset + value_scale * got,
            4.0e-5,
            "affine value map",
        );
        let (mapped_time, _) = oracle(
            Segment {
                ta: time_offset + time_scale * base.ta,
                tb: time_offset + time_scale * base.tb,
                ..base
            },
            parameter,
        );
        let transformed_time_value = time_transformed
            .sample_component(mapped_time, 0)
            .expect("time transformed sample");
        assert_close(transformed_time_value, got, 4.0e-5, "time affine map");
        assert_close(got, expected, 3.0e-5, "oracle at affine probe");
    }
}

#[test]
fn each_vec3_component_matches_its_scalar_curve_and_oracle() {
    for case_index in 0..8 {
        let base = generated_segment(case_index);
        let va = [base.va, base.va + 0.75, base.va - 1.25];
        let vb = [base.vb, base.vb - 0.5, base.vb + 1.0];
        let outgoing = [base.outgoing, base.outgoing + 0.5, -base.outgoing * 0.25];
        let incoming = [base.incoming, base.incoming - 0.375, -base.incoming * 0.5];
        let vec3 = Channel::Vec3(vec![
            Vec3Key {
                tangent: Tangent::Broken([0.0; 3], outgoing),
                weights: Some((0.2, base.outgoing_weight)),
                ..Vec3Key::new(base.ta, va, Ease::Bezier)
            },
            Vec3Key {
                tangent: Tangent::Broken(incoming, [0.0; 3]),
                weights: Some((base.incoming_weight, 0.4)),
                ..Vec3Key::new(base.tb, vb, Ease::Linear)
            },
        ]);
        for component in 0..3 {
            let segment = Segment {
                ta: base.ta,
                tb: base.tb,
                va: va[component],
                vb: vb[component],
                outgoing: outgoing[component],
                incoming: incoming[component],
                outgoing_weight: base.outgoing_weight,
                incoming_weight: base.incoming_weight,
            };
            let scalar = scalar_channel(segment);
            for parameter in [0.0, 1.0e-4, 0.19, 0.5, 0.88, 0.9999, 1.0] {
                let (time, expected) = oracle(segment, parameter);
                let got = vec3.sample_component(time, component).expect("vec3 component");
                let scalar_got = scalar.sample_component(time, 0).expect("scalar component");
                assert_close(got, expected, 3.0e-5, "vec3 oracle");
                assert_close(got, scalar_got, 1.0e-6, "vec3/scalar agreement");
            }
        }
    }
}

#[test]
fn endpoint_holds_and_near_endpoint_samples_follow_the_curve() {
    let segment = Segment {
        ta: 0.25,
        tb: 1.75,
        va: -0.5,
        vb: 1.25,
        outgoing: 2.0,
        incoming: -1.0,
        outgoing_weight: 0.64,
        incoming_weight: 0.59,
    };
    let channel = scalar_channel(segment);
    assert_eq!(channel.sample_component(segment.ta, 0), Some(segment.va));
    assert_eq!(channel.sample_component(segment.tb, 0), Some(segment.vb));
    assert_eq!(channel.sample_component(segment.ta - 1.0, 0), Some(segment.va));
    assert_eq!(channel.sample_component(segment.tb + 1.0, 0), Some(segment.vb));
    for parameter in [1.0e-5, 0.99999] {
        let (time, expected) = oracle(segment, parameter);
        let actual = channel.sample_component(time, 0).expect("near endpoint sample");
        assert_close(actual, expected, 3.0e-5, "near endpoint oracle");
    }
}
