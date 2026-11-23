// SPDX-License-Identifier: MIT-0
//! The lab's 10 s run calls `regia_physics::vehicle_bike::bike_step` and nothing else.

use bike_lab::{BikeInput, BikeState, LabInputs, bike_step, run, steady_lean};

#[test]
fn ten_second_run_calls_the_same_stepper() {
    let inputs = LabInputs::default();
    assert_eq!(inputs.steps, 600, "10 s at 1/60 s");
    let report = run(&inputs).expect("lab");

    let mut state = BikeState::default();
    let command = BikeInput {
        dt: inputs.dt,
        steer: inputs.steer,
        speed: inputs.speed,
        drive: inputs.drive,
        front_ray: inputs.front_ray,
        rear_ray: inputs.rear_ray,
    };
    for _ in 0..inputs.steps {
        bike_step(&mut state, &inputs.config, &command).expect("the same stepper");
    }
    assert_eq!(report.digest, state.digest());
    assert_eq!(report.lean.to_bits(), state.lean.to_bits());
    assert!(!report.fallen);

    let expected = steady_lean(inputs.speed, inputs.steer, &inputs.config);
    let error = (report.lean - expected).abs();
    assert!(
        expected > 0.0 && error <= 0.10 * expected,
        "lean {} closed form {expected}",
        report.lean
    );

    let again = run(&inputs).expect("second run");
    assert_eq!(again.digest, report.digest);
}
