// SPDX-License-Identifier: MIT-0
//! ADR-3117: four AI cars lap the lab's oval of lanes on line, apart, and the same way every run.

use nav_world_lab::traffic::run_traffic;

#[test]
fn four_ai_cars_lap_the_oval_and_runs_repeat() {
    let first = run_traffic(2400).expect("first run");
    println!("{first}");
    let second = run_traffic(2400).expect("second run");
    assert_eq!(first, second, "two runs differ");
    assert!(
        first.laps.iter().all(|&l| l >= 1),
        "every car lapped: {:?}",
        first.laps
    );
    assert!(
        first.max_cross_track <= 1.5,
        "cross-track {}",
        first.max_cross_track
    );
    assert!(first.closest_gap > 2.0, "cars kept apart: {}", first.closest_gap);
}
