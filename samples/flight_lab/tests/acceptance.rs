// SPDX-License-Identifier: MIT-0
//! The lab's physics without a renderer (ADR-3374): the asset is the reference trainer, two autopilot
//! flights give the same digests, the plane lifts off inside the stated range and levels off. No GPU,
//! window or wall clock.

use flight_lab::{LEVEL_ALTITUDE, PLANE_TEXT, run_headless};
use regia_physics::aero::asset::{AIRCRAFT_FORMAT_VERSION, AircraftAsset};
use regia_physics::aero::trainer;

#[test]
fn the_asset_is_the_reference_trainer() {
    let asset = AircraftAsset::from_ron(PLANE_TEXT).expect("loads");
    assert_eq!(asset.format_version, AIRCRAFT_FORMAT_VERSION);
    let reference = trainer();
    let config = asset.config;
    assert_eq!(config.surfaces.len(), reference.surfaces.len());
    assert_eq!(config.engines, reference.engines);
    assert_eq!(config.gear, reference.gear);
    assert_eq!(config.mass, reference.mass);
    assert_eq!(config.inertia, reference.inertia);
    for (a, b) in config.surfaces.iter().zip(&reference.surfaces) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.controls, b.controls);
        assert!(
            a.normal.abs_diff_eq(b.normal, 1.0e-6),
            "{}: {} vs {}",
            a.name,
            a.normal,
            b.normal
        );
        let ar = |s: &regia_physics::aero::AeroSurface| s.effective_aspect_ratio();
        assert!((ar(a) - ar(b)).abs() < 1.0e-5);
        assert_eq!(
            (a.span, a.chord, a.cl0, a.cd0, a.cm0, a.incidence_deg, a.position),
            (b.span, b.chord, b.cl0, b.cd0, b.cm0, b.incidence_deg, b.position)
        );
    }
}

/// Fixed steps of the headless flight: 90 s.
const FLIGHT_STEPS: usize = 5400;

#[test]
fn the_autopilot_takes_off_climbs_and_levels_off_the_same_way_twice() {
    let first = run_headless(FLIGHT_STEPS).expect("first flight");
    let second = run_headless(FLIGHT_STEPS).expect("second flight");
    println!("{first:?}");
    assert_eq!(first, second, "two runs differ");
    let distance = first.lift_off_distance.expect("lifted off");
    assert!((120.0..=500.0).contains(&distance), "lift-off after {distance} m");
    assert!(!first.stalled, "the autopilot stalled the plane");
    assert!(
        (first.altitude - LEVEL_ALTITUDE).abs() < 25.0,
        "ended at {} m, wanted {LEVEL_ALTITUDE}",
        first.altitude
    );
    assert!(
        first.climb_rate.abs() < 2.0,
        "still climbing at {} m/s",
        first.climb_rate
    );
    assert!(
        first.max_altitude < LEVEL_ALTITUDE + 60.0,
        "overshot to {} m",
        first.max_altitude
    );
}

/// Prints the autopilot's flight every half second (hand-run: `--ignored --nocapture`).
#[test]
#[ignore = "diagnostic print, no assertions"]
fn print_the_autopilot_flight() {
    let _ = flight_lab::trace_headless(FLIGHT_STEPS, 30);
}
