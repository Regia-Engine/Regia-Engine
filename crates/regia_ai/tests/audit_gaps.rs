// SPDX-License-Identifier: MPL-2.0
//! Test-gap audit (sprint 2026-10-21): the sight cone's boundaries, which the unit tests only
//! sample well inside or well outside the cone. The straight-above target and the inclusive
//! hearing radii are pinned in `edge_cases.rs`; this file holds the cone edge, the ground-plane
//! range and the 3D hearing asserts only.

use regia_ai::{HearingEvent, Perception, can_hear, in_sight_cone};
use regia_core::math::Vec3;

#[test]
fn the_cone_edge_is_inclusive_and_a_hair_outside_is_not() {
    let eye = Vec3::ZERO;
    let forward = Vec3::Z;
    let half = 45f32.to_radians();
    // On the edge ray: 45° off the facing, 5 m away.
    let on_edge = Vec3::new(5.0, 0.0, 5.0);
    assert!(
        in_sight_cone(eye, forward, on_edge, half, 10.0),
        "the edge ray is inside"
    );
    // 46° off the facing is outside.
    let outside = Vec3::new(46f32.to_radians().sin(), 0.0, 46f32.to_radians().cos()) * 5.0;
    assert!(!in_sight_cone(eye, forward, outside, half, 10.0));
    let inside = Vec3::new(44f32.to_radians().sin(), 0.0, 44f32.to_radians().cos()) * 5.0;
    assert!(in_sight_cone(eye, forward, inside, half, 10.0));
    // The mirror edge on the other side behaves the same.
    assert!(in_sight_cone(eye, forward, Vec3::new(-5.0, 0.0, 5.0), half, 10.0));
}

#[test]
fn the_range_is_inclusive_and_measured_on_the_ground_plane() {
    let eye = Vec3::new(1.0, 0.0, 1.0);
    let forward = Vec3::X;
    let half = 30f32.to_radians();
    assert!(
        in_sight_cone(eye, forward, eye + Vec3::new(10.0, 0.0, 0.0), half, 10.0),
        "exactly at range"
    );
    assert!(!in_sight_cone(
        eye,
        forward,
        eye + Vec3::new(10.01, 0.0, 0.0),
        half,
        10.0
    ));
    // Height is dropped before both tests: a target 50 m up but 5 m away on the ground is seen.
    assert!(in_sight_cone(
        eye,
        forward,
        eye + Vec3::new(5.0, 50.0, 0.0),
        half,
        10.0
    ));
    // A facing that points up has no horizontal part, so the cone is a full disk.
    assert!(in_sight_cone(
        eye,
        Vec3::Y,
        eye + Vec3::new(-5.0, 0.0, 0.0),
        half,
        10.0
    ));
}

#[test]
fn hearing_counts_height_three_dimensionally() {
    // The radius is a 3D distance: (8, 8, 0) is 11.3 m out, outside a 10 m hearing radius,
    // where the ground-plane reading of 8 m would be inside. The inclusive radii themselves
    // are pinned in `edge_cases.rs::hearing_edge_cases`.
    let listener = Perception {
        hearing_radius: 10.0,
        ..Perception::default()
    };
    let event = HearingEvent {
        position: Vec3::new(8.0, 8.0, 0.0),
        radius: 20.0,
        ..HearingEvent::default()
    };
    assert!(
        !can_hear(&listener, Vec3::ZERO, &event),
        "the height counts: 11.3 m"
    );
}
