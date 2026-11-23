// SPDX-License-Identifier: MPL-2.0
//! Unit tests of the reference hit queries and the step's edge cases (ADR-8509).

use super::*;

fn query(from: [f32; 3], to: [f32; 3], radius_m: f32) -> SegmentQuery {
    SegmentQuery {
        from,
        to,
        radius_m,
        ignore_body: None,
    }
}

#[test]
fn plane_hits_only_a_sphere_moving_into_it() {
    let mut hits = LinearScanHits::default();
    hits.planes.push(HitPlane::ground(0.0));
    let hit = hits
        .first_hit(&query([0.0, 2.0, 0.0], [0.0, -2.0, 0.0], 0.5))
        .expect("crosses");
    assert_eq!(hit.t, 1.5 / 4.0);
    assert_eq!(hit.point, [0.0, 0.5, 0.0]);
    assert_eq!(hit.target, HitTarget::World);
    // Moving out, or parallel, or staying above: no hit.
    assert!(
        hits.first_hit(&query([0.0, 0.5, 0.0], [0.0, 3.0, 0.0], 0.5))
            .is_none()
    );
    assert!(
        hits.first_hit(&query([0.0, 1.0, 0.0], [5.0, 1.0, 0.0], 0.5))
            .is_none()
    );
    assert!(
        hits.first_hit(&query([0.0, 3.0, 0.0], [0.0, 1.0, 0.0], 0.5))
            .is_none()
    );
    // Starting inside and moving deeper: a contact at t = 0.
    let hit = hits
        .first_hit(&query([0.0, -1.0, 0.0], [0.0, -2.0, 0.0], 0.0))
        .expect("deeper");
    assert_eq!(hit.t, 0.0);
}

#[test]
fn box_and_sphere_report_the_entry_face_and_the_earliest_contact_wins() {
    let mut hits = LinearScanHits::default();
    hits.boxes.push(HitBox {
        min: [10.0, 0.0, 0.0],
        max: [11.0, 1.0, 1.0],
    });
    hits.spheres.push(HitSphere {
        center: [5.0, 0.5, 0.5],
        radius_m: 1.0,
        body: 3,
    });
    let q = query([0.0, 0.5, 0.5], [20.0, 0.5, 0.5], 0.0);
    let hit = hits.first_hit(&q).expect("sphere first");
    assert_eq!(hit.target, HitTarget::Body(3));
    assert_eq!(hit.point, [4.0, 0.5, 0.5]);
    assert_eq!(hit.normal, [-1.0, 0.0, 0.0]);
    let hit = hits
        .first_hit(&SegmentQuery {
            ignore_body: Some(3),
            ..q
        })
        .expect("then the box");
    assert_eq!(hit.target, HitTarget::World);
    assert_eq!(hit.point, [10.0, 0.5, 0.5]);
    assert_eq!(hit.normal, [-1.0, 0.0, 0.0]);
    // From above, onto the top face, with a radius.
    let hit = hits
        .first_hit(&query([10.5, 5.0, 0.5], [10.5, -5.0, 0.5], 0.25))
        .expect("top");
    assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
    assert_eq!(hit.point[1], 1.25);
    // Passing beside it.
    assert!(
        hits.first_hit(&query([0.0, 3.0, 0.5], [20.0, 3.0, 0.5], 0.0))
            .is_none()
    );
}

#[test]
fn a_pair_of_sources_takes_the_earlier_and_the_first_on_a_tie() {
    let mut world = LinearScanHits::default();
    world.planes.push(HitPlane::ground(0.0));
    let mut bodies = LinearScanHits::default();
    bodies.spheres.push(HitSphere {
        center: [0.0, 2.0, 0.0],
        radius_m: 0.5,
        body: 9,
    });
    let q = query([0.0, 5.0, 0.0], [0.0, -5.0, 0.0], 0.0);
    let mut both = (&mut world, &mut bodies);
    assert_eq!(both.first_hit(&q).map(|h| h.target), Some(HitTarget::Body(9)));
    let mut tie = (world.clone(), world.clone());
    tie.1.planes[0] = HitPlane::ground(0.0);
    assert_eq!(tie.first_hit(&q).map(|h| h.t), world.first_hit(&q).map(|h| h.t));
    let mut none = (NoHits, NoHits);
    assert!(none.first_hit(&q).is_none());
}

#[test]
fn wind_pulls_toward_the_air_speed_and_vacuum_ignores_it() {
    let params = ProjectileParams {
        wind_response: 1.0,
        gravity_scale: 0.0,
        drag_per_m: 0.05,
        ..ProjectileParams::DEFAULT
    };
    let mut vel = [0.0_f32; 3];
    // Quadratic drag closes the gap as 1 / (1 / 10 + k t): about 0.2 m/s left after 100 s.
    for _ in 0..6000 {
        vel = step_velocity(&params, 9.81, 1.2, [10.0, 0.0, 0.0], vel, 1.0 / 60.0);
    }
    assert!(vel[0] > 9.5 && vel[0] <= 10.0, "{vel:?}");
    let still = step_velocity(&params, 9.81, 0.0, [10.0, 0.0, 0.0], [0.0; 3], 1.0 / 60.0);
    assert_eq!(still, [0.0; 3]);
    // Linear drag slows a shot in air only.
    let linear = ProjectileParams {
        linear_drag_per_s: 1.0,
        drag_per_m: 0.0,
        gravity_scale: 0.0,
        ..ProjectileParams::DEFAULT
    };
    let slowed = step_velocity(&linear, 9.81, 1.2, [0.0; 3], [60.0, 0.0, 0.0], 1.0 / 60.0);
    assert!((slowed[0] - 59.0).abs() < 1.0e-4, "{slowed:?}");
    let kept = step_velocity(&linear, 9.81, 0.0, [0.0; 3], [60.0, 0.0, 0.0], 1.0 / 60.0);
    assert_eq!(kept[0], 60.0);
}

#[test]
fn timer_ticks_round_to_the_tick_and_saturate() {
    assert_eq!(timer_ticks(1.0, 1.0 / 60.0), 60);
    assert_eq!(timer_ticks(0.0, 1.0 / 60.0), 1);
    assert_eq!(timer_ticks(f32::MAX, 1.0 / 60.0), u32::MAX);
    assert_eq!(timer_ticks(1.0, 0.0), u32::MAX);
}

#[test]
fn a_sphere_on_a_box_face_moving_away_is_not_stopped() {
    let mut hits = LinearScanHits::default();
    hits.boxes.push(HitBox {
        min: [5.0, -1.0, -1.0],
        max: [6.0, 1.0, 1.0],
    });
    // On the low face, leaving: free. On the low face, going in: a contact at 0 with the face's normal.
    assert!(
        hits.first_hit(&query([5.0, 0.0, 0.0], [4.0, 0.0, 0.0], 0.0))
            .is_none()
    );
    let hit = hits
        .first_hit(&query([5.0, 0.0, 0.0], [5.5, 0.0, 0.0], 0.0))
        .expect("into the box");
    assert_eq!((hit.t, hit.normal), (0.0, [-1.0, 0.0, 0.0]));
    // Inside near the top face, moving up and out: free; moving down: the top face is still the
    // nearest, so a contact at 0 with its normal.
    assert!(
        hits.first_hit(&query([5.5, 0.9, 0.0], [5.5, 2.0, 0.0], 0.0))
            .is_none()
    );
    let hit = hits
        .first_hit(&query([5.5, 0.9, 0.0], [5.5, 0.5, 0.0], 0.0))
        .expect("deeper");
    assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
}
