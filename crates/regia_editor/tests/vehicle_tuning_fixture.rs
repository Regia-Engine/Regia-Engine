// SPDX-License-Identifier: MPL-2.0
//! The packaged vehicle-tuning fixture stays identical to the vehicle lab's authored car.

#[test]
fn vehicle_tuning_fixture_matches_the_lab_car() {
    assert_eq!(
        include_bytes!("fixtures/vehicle_tuning/rwd_car.vehicle.ron").as_slice(),
        include_bytes!("../../../samples/vehicle_lab/assets/rwd_car.vehicle.ron").as_slice(),
        "the editor tuning tests must exercise the vehicle lab's car",
    );
}
