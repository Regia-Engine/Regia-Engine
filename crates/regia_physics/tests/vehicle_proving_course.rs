// SPDX-License-Identifier: MPL-2.0
//! The proving ground's tracks and the towed trailer (ADR-7745): fixed-step runs, no clock.

use regia_physics::vehicle::VehicleConfig;
use regia_physics::vehicle_proving::{
    ProvingCourse, ProvingScript, ProvingTrack, add_proving_track, proving_tow_hitch, run_proving,
    run_proving_course,
};
use regia_physics::vehicle_trailer::{TrailerConfig, car_tow_hitch};
use regia_physics::world::PhysicsWorld;

fn course(track: ProvingTrack, trailer: Option<TrailerConfig>) -> ProvingCourse {
    ProvingCourse { track, trailer }
}

#[test]
fn vehicle_course_flat_solo_is_the_plain_run() {
    let config = VehicleConfig::default();
    let plain = run_proving(&config, ProvingScript::Accel, 4.0).expect("plain");
    let flat = run_proving_course(
        &config,
        ProvingScript::Accel,
        4.0,
        &course(ProvingTrack::Flat, None),
    )
    .expect("course");
    assert_eq!(
        flat.run, plain,
        "the flat course must drive the plain proving world"
    );
    assert_eq!(flat.track, ProvingTrack::Flat);
    assert!(flat.trailer.is_none());
}

#[test]
fn vehicle_course_tracks_add_generated_colliders() {
    for (track, at_least) in [
        (ProvingTrack::Flat, 0),
        (ProvingTrack::Ramp, 1),
        (ProvingTrack::Slope, 2),
        (ProvingTrack::BumpStrip, 100),
    ] {
        let mut physics = PhysicsWorld::default();
        let added = add_proving_track(&mut physics, track);
        assert!(added >= at_least, "{}: {added} colliders", track.name());
        assert_eq!(ProvingTrack::from_name(track.name()), Some(track));
        if track == ProvingTrack::Flat {
            assert_eq!(added, 0);
        }
    }
    assert_eq!(ProvingTrack::from_name("moon"), None);
}

/// Acceptance (item 3): on the bump strip the number of wheels touching the ground changes from
/// step to step; on the flat plane it never does.
#[test]
fn vehicle_course_bump_strip_changes_the_contact_count() {
    let config = VehicleConfig::default();
    let flat = run_proving_course(
        &config,
        ProvingScript::Accel,
        10.0,
        &course(ProvingTrack::Flat, None),
    )
    .expect("flat");
    let wheels = config.wheels.len() as u32;
    assert_eq!(flat.contact_changes, 0, "{flat:?}");
    assert_eq!((flat.min_contacts, flat.max_contacts), (wheels, wheels));
    let bumps = run_proving_course(
        &config,
        ProvingScript::Accel,
        10.0,
        &course(ProvingTrack::BumpStrip, None),
    )
    .expect("bumps");
    println!(
        "bump strip: changes {} min {} max {} rise {}",
        bumps.contact_changes, bumps.min_contacts, bumps.max_contacts, bumps.max_rise
    );
    assert!(bumps.contact_changes > 0, "{bumps:?}");
    assert!(bumps.min_contacts < wheels, "{bumps:?}");
    // Fixed steps, no clock: the same run twice is the same run.
    let again = run_proving_course(
        &config,
        ProvingScript::Accel,
        10.0,
        &course(ProvingTrack::BumpStrip, None),
    )
    .expect("again");
    assert_eq!(again, bumps);
}

#[test]
fn vehicle_course_ramp_and_slope_lift_the_car() {
    let config = VehicleConfig::default();
    let ramp = run_proving_course(
        &config,
        ProvingScript::Accel,
        6.0,
        &course(ProvingTrack::Ramp, None),
    )
    .expect("ramp");
    let slope = run_proving_course(
        &config,
        ProvingScript::Accel,
        12.0,
        &course(ProvingTrack::Slope, None),
    )
    .expect("slope");
    println!("ramp rise {} slope rise {}", ramp.max_rise, slope.max_rise);
    assert!(ramp.max_rise > 1.0, "{ramp:?}");
    assert!(slope.max_rise > 3.0, "{slope:?}");
}

#[test]
fn vehicle_course_tow_hitch_matches_the_default_car_hitch() {
    // 2.1 + 0.3 in f32 is one ulp under 2.4.
    let hitch = proving_tow_hitch(&VehicleConfig::default());
    assert!(hitch.distance(car_tow_hitch()) < 1e-5, "{hitch:?}");
}

/// Acceptance (item 1): the proving ground hitches the trailer to the tuned car, and the run reads
/// the trailer's edited values.
#[test]
fn vehicle_course_tows_the_edited_trailer() {
    let config = VehicleConfig::default();
    let light = TrailerConfig::car_trailer();
    let mut heavy = TrailerConfig::car_trailer();
    heavy.chassis.chassis_mass = 1500.0;
    let run = |trailer: &TrailerConfig| {
        run_proving_course(
            &config,
            ProvingScript::Accel,
            6.0,
            &course(ProvingTrack::Flat, Some(trailer.clone())),
        )
        .expect("towing run")
    };
    let solo = run_proving(&config, ProvingScript::Accel, 6.0).expect("solo");
    let a = run(&light);
    let b = run(&heavy);
    let ta = a.trailer.expect("trailer run");
    let tb = b.trailer.expect("trailer run");
    assert!(ta.coupled && tb.coupled, "{ta:?} {tb:?}");
    assert_eq!(ta.chassis_mass, 750.0);
    assert_eq!(tb.chassis_mass, 1500.0);
    println!(
        "solo {} m, 750 kg trailer {} m, 1500 kg trailer {} m",
        solo.distance_m, a.run.distance_m, b.run.distance_m
    );
    assert!(a.run.distance_m < solo.distance_m, "towing must cost distance");
    assert!(
        b.run.distance_m < a.run.distance_m,
        "a heavier trailer must cost more"
    );
    assert_ne!(a.run.digest, b.run.digest);
    assert_eq!(run(&light), a, "deterministic");
}
