# Navigation area corridor regression recipe

Run `cargo test -p regia_nav --test area_path_oracle -- --nocapture` to verify the existing
area-weighted polygon corridor query against independent simple-path enumeration. This is a CPU
regression recipe, with no new agent method or Cargo feature. Existing protocol coverage remains
in [COVERAGE](../COVERAGE.md)'s existing `nav.find_path` → `nav_find_path` row and the area methods documented in
[navigation areas and lanes](navigation-lanes.md).

The objective comes from `crates/regia_nav/src/query.rs`, not the length of a funnel path.
For adjacent polygon centers A/B and portal midpoint M, let d=|A−B|, u=|A−M|, v=|B−M|
(all in XZ), and r/s be mesh cost × filter multiplier. Equal rates cost d×r.
Otherwise the edge costs d×(u×r+v×s)/(u+v), with d×max(r,s) when the denominator is zero.
The centers are vertex averages. There is no endpoint-to-center charge. A same-polygon corridor
costs zero even when its two waypoint positions differ.

The public builder creates two and three competing corridors plus a disconnected island. The
reference copies public vertices and portal adjacency and enumerates simple paths without the
production search or cost helpers. Every returned pair must be adjacent and every returned polygon
must pass the area mask. The cost calculation uses f64, with a relative/absolute 2e-5 allowance for
production f32 rounding. Any optimal corridor is accepted when costs tie.

The sweep uses mesh rates `[2, 0.5, 4, 1]` for area ids `[0, 1, 2, 63]`; filter rates are
`[0.5, a, b, c]`, where a/b/c range independently over `[0.125, 1, 4]`. It checks every ordered
endpoint-polygon pair and every subset of the fixture's area ids. All 64 filter multipliers are
then scaled by 0.25 and 8: the complete reference optimal set must remain unchanged, and the query
must still return an optimum. The fixtures currently have five and six polygons (with an asserted upper bound of 12).
The sweep executes 62,856 queries, including 1,152 tied-optimum cases and 49,410 refusals.
Enumeration allows at most
4096 simple paths and 16,384 traversal visits per pair. Effective sweep rates stay between
1/64 and 128. No random input, timing assertion, sleep, file, or golden is involved.

The endpoint test checks snapping an excluded point to a known trunk boundary from both query
directions. Tight extents refuse the same point. Disconnected and invalid endpoints/extents refuse
after a successful query, and the next valid query must recover its optimum. The hand-calculated
reference test distinguishes portal fractions, equal rates, off-axis portal geometry, and waypoint
length from the graph objective.

For a temporary negative control, replace the reference's destination-area rate lookup with the
area-zero lookup, run the sweep and require a failure, then restore the lookup and rerun. Never
commit the mutation. A real production counterexample must remain an ignored diagnostic with a
clear reason until production is fixed; do not relax the optimum assertion.

For protocol inspection in an existing navigation sample, use `nav.area_status`, `nav.set_area_cost`
and `nav.find_path` with the documented `filter.area_costs` and `filter.exclude_areas` fields.
These existing methods expose area setup and the returned corridor; the independent optimality
check belongs to the Rust regression above. See [navigation](navigation.md) for query parameters.

Existing examples in `tests/areas.rs` cover a single across/around choice
(`the_cost_decides_between_across_and_around`), per-query scaling
(`a_filter_scales_the_mesh_cost_per_query`), cheap ground, mask closure, and an excluded endpoint
snapping away. `tests/tq_gaps.rs::repeated_path_queries_are_bit_identical` covers repeatability.
The query unit test `find_nearest_honors_each_extent_axis_far_outside_the_hash_reach` and
`tests/edge_cases.rs::queries_with_odd_extents` cover per-axis and vertical snap reach.
This recipe extends those examples with an independent optimum across their interactions;
it introduces no alternate cost contract and does not test off-mesh links or obstacles.
