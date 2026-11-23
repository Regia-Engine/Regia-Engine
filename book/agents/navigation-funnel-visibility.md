# Navigation: checking continuous corridor paths

Use the existing [`nav.find_path` method](navigation.md#paths). Its `length` measures the returned world-space waypoint polyline. Polygon graph costs and the geometric length of the smoothed path are different quantities.

For a flat corridor with a single polygon route, default area costs and no links or obstacles, regression tests compare that polyline to an independent visibility-graph shortest path. Both waypoints and every segment between them must lie in the closed corridor, including boundary edges and portal vertices. Equivalent polygon triangulations should preserve this continuous optimum. This check does not assert that A* chooses a globally shortest continuous path among several polygon routes.

The bounded recipe is:

```sh
cargo test -p regia_nav --test funnel_visibility_oracle
cargo test -p regia_agent --lib nav::tests::
cargo test -p regia_agent --lib coverage_calls::every_dispatched_method_is_called
```

The first command checks straight, L-shaped and alternating narrow strips in both directions, triangulation variants, portal endpoints and collinear vertices. The reference clips the entire segment against convex polygons and checks the union of covered intervals; it does not sample points or call the engine's containment/funnel helpers. Integer fixture geometry makes reference determinant signs exact in `f64`; interval-division roundoff and `f32` length accumulation have separate documented tolerances.

The existing agent tests exercise successful nearest/path handlers and dispatch the registered protocol methods, including `nav.find_path`. No method or capability is added by this test recipe.

The regression first reproduced a portal-start detour in the funnel: the triangulated alternating corridor from `[4, 0, 1]` to `[24, 0, 5]` included an unnecessary `[4, 0, 2]` corner (22.94427191 m versus 22.47213595 m). The funnel now skips a portal that already holds the start or the goal, and the test passes.
