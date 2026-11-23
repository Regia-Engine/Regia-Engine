# Mesh query slab boundary regressions

`regia_core::mesh_query::TriBvh` ray queries have deterministic coverage for parallel slab axes,
signed zero direction components, closed-box faces/edges/corners, inward and outward directions,
and inclusive maximum distance. The independent test oracle enumerates the mesh triangles instead
of walking the tree. Edge and corner hits may belong to multiple triangles; tests accept any of the
equally nearest IDs and compare distance and barycentrics within `2e-6`. Triangle intersection
uses f64 internally, permits `1e-9` barycentric slack at shared edges, and the mesh builder keeps
degenerate triangles while ray queries skip them.

The fixtures also apply exact power-of-two scales and exactly representable translations, checking
that transforms do not change the query result. A collinear triangle overlaps a valid hit's
projected point to ensure it cannot suppress that hit. Counters are reported as deterministic
node-box and triangle counts; they are not performance thresholds.

Run the focused regression and the existing mesh-query unit tests:

```sh
cargo test -p regia_core --test mesh_query_slab_boundaries
cargo test -p regia_core --lib mesh_query
```

The existing `agent.run_tests` call can run and inspect these tests in a project that grants its
documented `build` permission. For example, request target `mesh_query_slab_boundaries` or `lib`
with package `regia_core`; its normal job events and result report test names, outcomes, compiler
errors and failures. Stable existing run-test errors include `run_tests_bad_name`,
`run_tests_busy`, `run_tests_no_manifest` and `run_tests_spawn_failed` (see [Jobs and test runs](jobs.md)).
This test-only coverage adds no agent method or state to inspect.
