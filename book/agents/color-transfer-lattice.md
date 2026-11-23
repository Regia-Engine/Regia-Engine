# Color transfer validation

`regia_core::math::Color` stores linear RGB and straight, linear alpha. `from_srgb_u8` and `from_hex`
decode authored sRGB values with the IEC 61966-2-1 transfer curve; `to_srgb_u8` clamps and rounds the
encoded color channels and alpha to bytes. The byte API preserves every 8-bit authored channel and
alpha value on a round trip. The f32 transfer helpers retain their documented out-of-domain behavior;
they do not clamp HDR or negative inputs.

The unit tests in `crates/regia_core/src/math/color.rs` provide a dense deterministic validation
lattice: every byte independently in each RGB channel against every alpha byte, f32 comparisons with
an independently written f64 reference around both piecewise knots and across `[0, 1]`, monotonicity
with the published rounded encoder-knot seam isolated, and bounded inverse round-trip error. Hex tests
cover optional `#`, six/eight digit forms, ASCII case,
invalid digits at each byte position, and non-ASCII characters near valid byte lengths. These tests
check typed parse errors and do not introduce image goldens.

Run the package tests directly:

```sh
cargo test -p regia_core --lib math::color
```

The same test target can be selected through the existing `agent.run_tests` method, where a project
has granted the `build` permission. For example, from the project root:

```sh
regia agent call agent.run_tests '{"target":"lib","package":"regia_core","filter":"math::color"}' --project <project-root>
```

Poll the returned job with `agent.job_status` until it is done, then inspect its test counts and
failures. Stable test-runner reason codes are `run_tests_bad_name`, `run_tests_busy`,
`run_tests_no_manifest`, and `run_tests_spawn_failed`; job controls also use `job_id_invalid`,
`job_not_found`, and `job_not_cancellable`. `play.expect` asserts agent-visible project state, while
these pure color unit tests have no project state to assert, so `agent.run_tests` and its job result
are the relevant existing verification surface.

```sh
regia agent call agent.job_status '{"job":"tests:<job-id>","since":0}' --project <project-root>
```
