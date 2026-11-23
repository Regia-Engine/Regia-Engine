# Verify navigation dirty slices and step budgets

The deterministic regression is `crates/regia_nav/tests/dirty_slice_budget_model.rs`:

```sh
cargo test -p regia_nav --test dirty_slice_budget_model
```

This verifies the existing `NavRegionQueue` contract with in-memory geometry. A fixed 16×16-cell
cache has 0.5 m cells, 2 m tiles, origin (-4, -4) in XZ, and zero agent-radius erosion. The oracle
scans closed tile rectangles and cell centers; it does not use the cache's tile intersection,
region-window or region-rebuild helpers to calculate expected results.

Tile slicing includes a shared edge on both adjacent tiles. A point at a tile corner can therefore
produce four slices. Reversed corners are normalized, negative coordinates retain their world
positions, and boxes are clipped at the grid boundary. This is the dirty-region rule; streaming
cell addressing remains half-open as documented in spec 78.

The bounded histories check every fixed step:

- Geometry-source call windows and order, including a refused slice retried ahead of later work.
- At most the configured number of attempts, with one refusal ending the step. Splitting a box
  does not count as a completed rebuild; invalid geometry consumes a work unit and counts `failed`.
- All queue counters, waiting boxes and source labels; changed-cell counts, pending tile order,
  per-tile rebuild counts, total rebuilds and revision. Overlapping slices may succeed without
  changing another cell or queuing another tile.
- Zero slice and tile budgets, an absent source subsequently installed, and pending tiles draining
  while no slice work is permitted. A refused source may have appended partial geometry; it must
  not leak into a later call.
- Covered-box replacement/drop and overflow coalescing, including arrivals after slicing. Two
  finite histories that coalesce to the same queue give identical step traces. Replays stay whole,
  never merge in either direction, and count as one work unit each.

There are no threads, sleeps, elapsed-time assertions, randomized seeds, files or golden images in
these tests. The oracle is a small original exhaustive scan of this fixture, not a navigation bake
or scheduling implementation. This coverage does not establish physical-device or performance passes.

The instrumented source and individual slice windows are internal Rust test machinery. The existing
agent surface is `nav.region_rebuild` and `nav.region_status`, documented in
[navigation world](navigation-world.md). No agent method was added for the regression. Dogfood the
existing happy and error paths on the normal build host with:

```sh
cargo test -p regia_agent --lib nav_world::tests::a_region_rebuild_without_triangles_queues_and_the_status_shows_it
cargo test -p regia_agent --lib nav_world::tests::region_errors_carry_reasons
```

On a host with `NavRegionPlugin`, `nav.region_rebuild` with `{"min":[0,0,0],"max":[1,1,1]}`
queues an `agent` request; `nav.region_status` reports its normalized bounds, source availability,
budgets and counters. Without the queue these calls return `data.reason: nav_no_region_queue`.
A direct rebuild with triangles and no cache returns `nav_no_tile_cache`. Box coordinates outside
the finite f32 range and malformed triangle arrays are invalid parameters. Non-finite triangle
vertices rejected by the cache use `nav_invalid_region`. These existing lowercase codes are unchanged.
The Rust `NavRegionQueue::push` API counts non-finite boxes in `rejected`.

For mutation sensitivity, temporarily change the slice intersection's strict separation check in
`region_queue.rs` from `lo_x > hi_x || lo_z > hi_z` to `lo_x >= hi_x || lo_z >= hi_z`. The boundary
enumeration must fail because shared-edge/point slices disappear. Restore the source before rerunning
the suite or committing. The lane report records the observed red and restored green output.

A second mutation replaces the unavailable-source branch's `return` with `continue`. The mixed-source
history must fail inside the source callback with `source retried beyond this step's attempts`.
The callback ceiling makes this otherwise unlimited retry fail by count, without a timeout.
Restore this mutation too before running the full crate checks.
