# Showcase statistics at numeric boundaries

The existing `cargo xtask showcase-table` command turns measured `render_ms` samples into the
published per-view min / average / max FPS and p50 / p99 frame times. Its pure helpers live in
`xtask::showcase_table`; this page documents their validation and regression tests. It adds no
agent method or output field.

`view_stats` returns no statistics for an empty sample set, any non-finite or non-positive sample,
or any set whose derived FPS values cannot be represented as finite positive `f64` values. The
average is `1000 / mean(frame_ms)`, which is also the harmonic mean of the per-frame FPS values.
The mean is accumulated incrementally so a sum of finite positive frame times cannot overflow.
Min FPS comes from the slowest frame and max FPS from the fastest frame. The existing nearest-rank
index policy for p50 and p99 is unchanged.

`render_ms_from_manifest` accepts only a `done` job with exactly the requested number of numeric,
finite, positive values. The render-out manifest shape and valid samples are unchanged. Incomplete
or error jobs, count mismatches, nulls, strings, zeroes, and negative samples follow the existing
error path; showcase-table records a failed view as `not run` instead of publishing statistics.

## Verify

The existing test runner can run the focused Rust unit tests from the project root:

```sh
regia agent call agent.run_tests '{"target":"lib","package":"xtask","filter":"showcase_table"}'
```

The equivalent command is `cargo test -p xtask --lib showcase_table`. The tests compare singleton,
two-frame, 100-frame, replicated, permuted, scaled, and stall-heavy populations to a separate
sorted-vector rank oracle. They also check arithmetic means, inverse FPS extrema, overflowing sums,
subnormal reciprocal overflow, invalid samples, and manifest status, shape, count, and value rules.
Rust `assert` checks are the relevant expectation surface for these pure helpers; `play.expect`
asserts world state and cannot call this host-side statistics function.

The existing `agent.run_tests` errors retain their stable protocol reason codes: `run_tests_bad_name`,
`run_tests_busy`, `run_tests_no_manifest`, and `run_tests_spawn_failed` (error code `F0002`). Test
failures are returned in the job result with the test name and output; they do not add a protocol
error code.
