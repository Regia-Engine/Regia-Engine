// SPDX-License-Identifier: MPL-2.0
//! Regression coverage for trail path-distance accounting and sample expiry.

use regia_core::math::Vec3;
use regia_vfx::{TrailRig, TrailStyle};

fn style() -> TrailStyle {
    TrailStyle {
        width: 0.2,
        lifetime: 0.5,
        color: [0.8, 0.6, 0.4, 0.7],
        min_segment: 0.3,
    }
}

#[test]
fn distance_tracks_the_polyline_through_decimation_and_expiry() {
    let points = [0.0_f32, 0.1, 0.2, 0.3, 0.35, 0.4, 0.45, 1.0];
    let expected_distance: f32 = points.windows(2).map(|pair| (pair[1] - pair[0]).abs()).sum();
    let mut rig = TrailRig::new(style());

    for (index, x) in points.into_iter().enumerate() {
        rig.update(Vec3::new(x, 0.0, 0.0), if index == 0 { 0.0 } else { 0.05 });
    }
    assert!((rig.total_distance() - expected_distance).abs() < 1.0e-6);
    assert!(
        rig.sample_count() < points.len(),
        "sub-threshold travel is decimated"
    );

    // Age every retained point while stationary, then cross the expiry boundary.
    rig.update(Vec3::X, 0.5);
    assert_eq!(
        rig.sample_count(),
        1,
        "the newest sample is exactly on its lifetime boundary"
    );
    rig.update(Vec3::X, 0.125);
    assert_eq!(rig.sample_count(), 0, "samples older than lifetime expire");
    assert!((rig.total_distance() - expected_distance).abs() < 1.0e-6);

    // The first post-expiry position starts a new sample history; distance remains cumulative.
    rig.update(Vec3::new(1.4, 0.0, 0.0), 0.0);
    assert_eq!(rig.sample_count(), 1);
    assert!((rig.total_distance() - (expected_distance + 0.4)).abs() < 1.0e-6);
}

#[test]
fn expiry_boundary_and_stationary_partitions_are_consistent() {
    let mut single_step = TrailRig::new(style());
    single_step.update(Vec3::ZERO, 0.0);
    single_step.update(Vec3::ZERO, 0.5);
    assert_eq!(single_step.sample_count(), 1, "age equal to lifetime is retained");
    single_step.update(Vec3::ZERO, 0.125);
    assert_eq!(single_step.sample_count(), 0, "age greater than lifetime expires");

    let mut partitioned = TrailRig::new(style());
    partitioned.update(Vec3::ZERO, 0.0);
    for _ in 0..4 {
        partitioned.update(Vec3::ZERO, 0.125);
    }
    assert_eq!(
        partitioned.sample_count(),
        1,
        "equal total stationary time retains the boundary sample"
    );
    partitioned.update(Vec3::ZERO, 0.125);
    assert_eq!(partitioned.sample_count(), single_step.sample_count());
}

#[test]
fn strip_geometry_uses_camera_right_and_stays_finite_and_bounded() {
    let mut strip_style = style();
    strip_style.min_segment = 0.15;
    let mut rig = TrailRig::new(strip_style);
    for (index, x) in [0.0_f32, 0.2, 0.4].into_iter().enumerate() {
        rig.update(Vec3::new(x, 0.0, 0.0), if index == 0 { 0.0 } else { 0.05 });
    }

    let head_color = [0.8, 0.6, 0.4, 0.7];
    let mut along_y = Vec::new();
    let mut along_z = Vec::new();
    rig.build_vertices(&mut along_y, Vec3::Y, head_color);
    rig.build_vertices(&mut along_z, Vec3::Z, head_color);

    assert_eq!(along_y.len(), 12, "two segments produce six vertices each");
    assert_eq!(along_y.len() % 3, 0, "triangle-list vertex count");
    assert_eq!(along_y.len(), along_z.len());
    assert!(
        along_y
            .iter()
            .zip(&along_z)
            .any(|(a, b)| a.position != b.position)
    );
    for vertices in [&along_y, &along_z] {
        for vertex in vertices.iter() {
            assert!(vertex.position.iter().all(|value| value.is_finite()));
            assert!(vertex.position.iter().all(|value| value.abs() <= 1.0));
            assert!((0.0..=1.0).contains(&vertex.color[3]));
        }
    }
}
