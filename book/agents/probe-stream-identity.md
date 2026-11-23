# CPU probe stream identity

`ProbeTracer::trace_streams` on `CpuProbeTracer` assigns one stable random stream key to each
probe. The same key, position, scene and settings produce the same `ProbeTrace` when probes are reordered,
split into separate calls, or scheduled with different worker counts and batch sizes. This is CPU
bit determinism; it does not promise bit equality with the GPU tracer.

The regression lives in `cpu_probe::tests::explicit_stream_identity_survives_permutations_subsets_and_scheduling`.
It traces a lit enclosed scene with explicit keys, compares the complete trace (first-hit data,
distance moments, counters and every lighting component), then checks a permutation, disjoint
subsets, worker counts 1/2/6, and batch sizes including a one-item tail and batches larger than the
input. It also covers repeated keys at equal positions, distinct keys at a shared position, key 0,
the maximum accepted key, the rejected `2^24` key, mismatched lengths, cancellation and a clean run
after cancellation, and empty input.

Run it directly:

```sh
cargo test -p regia_pt --lib cpu_probe
```

In a project opened by Regia, the existing `agent.run_tests` method can run the same library test
target (`package: "regia_pt"`, `target: "lib"`, `filter: "cpu_probe"`). Use the existing
`agent.job_status` method to read the test job's result. No probe-specific agent method is added.

The stable CPU tracer errors exercised here are `PtError::Scene` messages for a stream count that
does not match the position count, a key at or above `2^24`, and cancellation (`probe trace
canceled`). Empty input returns an empty vector without invoking progress.
