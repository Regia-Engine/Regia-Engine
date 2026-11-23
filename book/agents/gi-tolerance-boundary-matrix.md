# GI tolerance boundary matrix

The GI validator compares an observed measurement with the existing scene record. The decision rules use absolute distance from the physical neutral point: zero for signed shadow bias and one for energy ratio. Relative RMSE is nonnegative and uses its recorded value as its baseline. For each metric, the allowed distance is `abs(recorded distance) * (1 + relative) + absolute`; a value exactly at that limit passes.

This page documents the comparison checks in `regia_test_utils::gi_error`. They use bounded scalar values only and do not render plates, change calibration targets, or edit stored records. Run the test through the existing agent surface with build permission:

```sh
regia agent call agent.run_tests '{"package":"regia_test_utils","target":"lib","filter":"gi_error"}'
```

The equivalent host command is `cargo test -p regia_test_utils --lib gi_error`. Its boundary tests compare against a separately written scalar formula and cover zero baselines, equal magnitudes with opposite signs, values at and one representable float beyond tolerance, relative-only, absolute-only, and combined tolerance cases. Finite record values also round-trip at `f32` comparison precision.

There are no new agent methods or error codes. Existing `agent.run_tests` errors remain `run_tests_bad_name`, `run_tests_busy`, `run_tests_no_manifest`, and `run_tests_spawn_failed`; the common job errors are `job_id_invalid`, `job_not_found`, and `job_not_cancellable` (see [Jobs](jobs.md)). Improvement notes are advisory and do not fail a comparison.
