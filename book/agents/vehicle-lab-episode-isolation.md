# Vehicle lab: episode isolation regression

This recipe records the CPU-only isolation contract for repeated vehicle-lab runs. It drives the
existing `physics.vehicles`, `physics.hovercraft`, `physics.stats`, `physics.vehicle_input`,
`physics.vehicle_telemetry`, `play.pause` and `play.step` methods against a fresh headless app in
each episode. The test uses a new `regia_test_utils::temp::unique_temp_dir` project for each run.

## A/B/A sequence

Run `cargo test -p vehicle_lab --test acceptance agent_driven_a_b_a_episodes_are_isolated_in_one_process`.
The test runs the existing fixed-step full-throttle course (A), a steering and braking disturbance
(B), then A again, all in one test process. Each run settles for 63 fixed app frames, pauses through
the protocol and advances only through `play.step`. A's world digest, step count, gearbox report,
car speed and hovercraft speed must agree exactly across the two runs.

At the start of each run the test checks one car controller, one car and one hovercraft through the
existing agent methods, zero driver input and the same physics body/collider counts. B must move
under steering and then slow under braking, with the final input visible in `physics.vehicles`.
Every endpoint is read from that episode's project, its own `WillTerminate` cleanup is exercised,
and the endpoint and temporary project are checked absent before the next episode completes.
Projects and bearer tokens are distinct across the sequence; each endpoint is read only from its
own project's agent directory.

This recipe checks teardown and reconstruction in-process. It does not measure elapsed time and
does not capture or compare screenshots.
