# Spatial queries for AI

A **spatial query** answers a question an AI agent asks about the world: "where should I stand to shoot
the player without being seen", "which cover point is nearest and reachable", "which pickup should I go
for". It answers with data instead of code:

1. a **generator** produces candidate **items** (points, or entities with their positions);
2. **tests** drop the items that fail a condition (*filter*) and weight the rest (*score*);
3. a **pick** rule chooses the result.

The feature lives in `regia_ai::spatial_query` (`SpatialQueryPlugin`, `SpatialQueryManager`,
`RunSpatialQuery`, `SpatialQueryProbe`). Designers edit a `.spatialquery.ron` asset; behavior trees,
Rust code and agents run it and read the winner. Without `SpatialQueryPlugin` every `ai.query_*` method
answers `ai_query_unavailable`.

## Authoring a query asset

A `<name>.spatialquery.ron` file (format `spatialquery`, `format_version` 2; version 1 files still load, and
`regia upgrade-project` rewrites their version number):

```ron
(
    format_version: 2,
    name: "flank_position",
    item_type: Point,
    server_only: false,
    default_pick: Best,
    params: { "radius": Float(12.0), "need_cover": Bool(true) },
    generator: Lattice(center: Asker, half_size: ParamF32("radius"), spacing: 2.0, project_to_navmesh: true),
    tests: [
        Cast(id: "sees_player", purpose: Filter, from: Candidate(height: 1.6), to: Entity(tag: "player", height: 1.6),
             shape: Line, layers: ["static"], want_hit: false),
        Distance(id: "near", purpose: Score, to: Entity(tag: "player"), mode: Planar,
                 score: (shape: Linear, weight: -1.0, clamp_min: Seen, clamp_max: Seen)),
    ],
)
```

- `name` is the template name requests use. `item_type` is `Point` or `Entity`.
- `params` declares typed defaults: `Bool`, `Int`, `Float`, `Name`. Any numeric field takes a literal
  (`2.0`) or a parameter reference (`ParamF32("radius")`). A request overrides parameters by name. A
  parameter used but neither declared nor supplied ends the run with status `missing_param` (never a
  panic, never a silent default).
- `extents` (default `(1.0, 2.0, 1.0)`) is the half-extent box navigation-mesh lookups search.
- `server_only: true` marks assets a client-only package may omit. It is stored and round-trips; the cook
  ignores it until an authority-role build profile exists.
- Optional fields take a bare value (`tag: "player"`) or `Some(..)`.
- Unknown fields or variants are load errors with a line and column (`sq_unreadable`).

A file loads through the manager (`SpatialQueryManager::load_template`, or any request naming a path
relative to `asset_root`). A request names a loaded template by name or by path. A file that changed
on disk reloads at its next lookup; running requests finish on the version they started with.

### Load errors

Every load error names a line and column:

| Code | Cause |
|---|---|
| `sq_unreadable` | bad RON, an unknown field or variant, a duplicate test id, an unsupported `format_version`, more than `max_tests` tests, a `Snap` that is not a filter, a `Clearance` with a `Line` shape, a `Candidate` anchor in a generator, a `Custom` row with no name or in a version 1 file |
| `sq_item_type_mismatch` | the generator (or a `Union` child) produces the other item type, or a test does not accept the item type (`Snap` on an entity query) |
| `sq_no_threshold` | a `FilterBound` clamp on a test that does not set that `min` / `max` |

## Anchors

An anchor is a frame of reference: `Asker` (the entity running the query), `Candidate(height: h)` (the
item under test; tests only; write `Candidate()` for no offset), `Entity(tag: "t" | name: "n", height: h)`
(every live entity with the scene tag or name), `Point((x, y, z))`, `Blackboard("key")` (a vec3 or entity
on the asker's blackboard), `Provider("name")` (a Rust `AnchorProvider` registered on the manager) and
`Offset(anchor: .., height: h)` to raise any anchor.

An anchor can yield several values. A filter combines them with `require: Every` (default) or `Some`; a
score with `combine: Mean` (default), `Lowest`, `Highest` or `Product`. An anchor with **no** value fails a
filter for every item (`failed_reason: "no_anchor"`) and adds nothing to a score (not zero: the test is
left out of the item's normalization).

Directions (`Facing`, arcs, wedges) are `Forward(anchor)` (the entity's -Z; world -Z for a plain point)
or `Between(a, b)` (from `a` to `b`, every pair of values).

## Generators

Every point generator can project onto the navigation mesh (`project_to_navmesh: true`; points with no
mesh within `extents` are dropped) and lower its points to the ground (`drop_to_ground: Some(h)`: a
downward ray from `h` above to `h` below; points with no ground are dropped). Angles are measured in the
XZ plane from +X toward +Z.

| Generator | Example | Notes |
|---|---|---|
| `NearbyEntities` | `NearbyEntities(tag: "pickup", radius: Some(10.0))` | entity items, entity id ascending; `radius` is inclusive; `None` searches the whole world; `<= 0` finds nothing |
| `AtAnchor` | `AtAnchor(anchor: Blackboard("target"))` | one point per anchor value; for checking tests in isolation |
| `Ring` | `Ring(radius: 6.0, count: 16, height: 0.5)` | `count` or `gap` (a gap wider than the circumference gives one point); `arc: Some((toward: Forward(Asker), degrees: 90.0))` |
| `Wedge` | `Wedge(facing: Forward(Asker), degrees: 60.0, range: 8.0, angle_step: 10.0, aligned_spacing: 2.0, include_center: true)` | rays angle ascending, points along each ray; `angle_step` below 1 clamps to 1 |
| `Annulus` | `Annulus(inner_radius: 2.0, outer_radius: 8.0, rings: 4, points_per_ring: 12, spiral: Some(15.0))` | ring by ring, inner first; `outer < inner` swaps; one ring uses the mid radius |
| `Lattice` | `Lattice(half_size: 10.0, spacing: 1.0)` | row-major, rows along X (X varies fastest), then Z; `spacing <= 0` or more than `max_items` points is `sq_too_many_items` |
| `ReachLattice` | `ReachLattice(half_size: 10.0, spacing: 2.0, reach_limit: 1.5)` | a lattice on the navigation mesh, keeping points whose path from the center is at most `reach_limit * half_size`; no mesh is `sq_no_navmesh` |
| `Union` | `Union(children: [Ring(..), AtAnchor(..)])` | runs every child; duplicates (same position within 1e-4 and same entity) keep the first; entity children in a point query need `allow_mixed: true` |
| `Custom` | `Custom(name: "cover_points", center: Asker, args: { "reach": ParamF32("radius"), "rows": 2.0 })` | a Rust `GeneratorImpl` registered by name, see [Custom generators and tests](#custom-generators-and-tests); the projection and ground trace apply to its point items |

## Tests

Every test has an `id` (unique in the asset) and a `purpose`: `Filter`, `Score` or `Both`.

| Test | Raw value | Example |
|---|---|---|
| `Distance` | float, per anchor value | `Distance(id: "near", purpose: Score, to: Asker, mode: Planar)`; modes `Full3d`, `Planar`, `Vertical` (signed), `AbsVertical` |
| `Facing` | float -1..1 | `Facing(id: "ahead", purpose: Score, a: Forward(Asker), b: Between(Asker, Candidate()), planar: true)` |
| `InsideZone` | boolean | `InsideZone(id: "in_room", purpose: Filter, tag: "room", mode: Exact)`: a zone is an entity with the scene tag and a physics `Collider`; `Exact` tests the shape (box, sphere, capsule, cylinder; others their bounds), `Bounds` the world bounding box |
| `TagMatch` | boolean | `TagMatch(id: "hostile", purpose: Filter, query: "faction.enemy")`: the item entity (or the entity nearest a point) through the host's tag matcher |
| `Cast` | boolean | see the example asset: a ray (`Line`) or a swept `Sphere(r)`, `Box((x, y, z))`, `Capsule(r, h)` between `from` and `to`; `want_hit: false` keeps items with a clear line; the asker, the item entity and the anchor entities are never hit |
| `Clearance` | boolean | `Clearance(id: "room", purpose: Filter, shape: Capsule(0.4, 0.9), offset: (0.0, 1.0, 0.0))`: anything overlapping a volume at the item; `skip_asker` (default true) |
| `Route` | boolean or float | `Route(id: "reach", purpose: Filter, to: Asker)`; `measure: Length` scores path length; `drop_unreachable: true` drops unreachable items instead of scoring them worst |
| `RouteAll` | like `Route` | skips pairs farther apart (straight line) than `scan_limit` (default 1.5 times the largest item-to-anchor distance, found by a pre-pass that costs one work unit per item) |
| `Snap` | rewrites the item | `Snap(id: "floor", purpose: Filter, up: 1.0, down: 2.0, offset: (0.0, 0.1, 0.0))` onto the navigation mesh or `target: Geometry(["static"])`; items that cannot snap are dropped; point queries only |

Layers are physics layer names; with no layer table every layer interacts, and an unknown name is
`sq_bad_param`. `TagMatch` needs a matcher: `regia_ai` cannot depend on `regia_framework`, so the host
registers one: `manager.set_tag_matcher("gameplay_tags", my_matcher)` with
`fn my_matcher(world: &World, entity: Entity, query: &str) -> bool`.

A test whose backing system is absent (no physics world, no navigation mesh, no tag matcher, an
unregistered provider) fails the run with `sq_missing_system` naming the system.

### Filtering and scoring

- **Filter bounds:** `min` keeps `v >= min`, `max` keeps `v <= max`. A boolean test keeps items whose
  value equals the wanted one (`want_hit` for `Cast`/`Clearance`, `want` for the others).
- **Score** (`score: (..)`): resolve the bounds `lo`/`hi` (`clamp_min`/`clamp_max`: `Fixed(x)`,
  `FilterBound` or `Seen`, the smallest/largest raw value among the survivors), clamp, normalize
  (`origin: Floor` gives `(v - lo) / (hi - lo)`, `Zero` gives `v / hi`; a zero denominator gives 0),
  optionally peak (`peak_at: Some(r)`), clamp to 0..1, shape (`Flat`, `Linear`, `Quadratic`, `Falling`,
  `Root`), times `weight` (negative weights prefer low values). A boolean test contributes `weight` when it
  matches.
- **Item score** = sum of contributions / sum of the absolute weights (0 with no score test), so scores
  lie in -1..1 and compare across queries.
- **Order:** snaps run first, then the filters in authoring order (each over the survivors), then the
  score passes over the final survivors. Authoring order of filters and scorers does not change the
  result.

## Picks

| Pick | Result |
|---|---|
| `Best` | the highest score; ties go to the lower generation index |
| `Sample(band: 0.1)` | one item drawn (from the request seed) among the items with `score >= best - band * (best - worst)` |
| `All` | every survivor, score descending, ties by generation index |

No survivor is status `failed` for `Best` and `Sample`, and `success` with zero items for `All`.

## Custom generators and tests

A query can name Rust code the same way it names `Ring` or `Distance`. Register the implementation on the manager
under a name, and use `Custom(name: ...)` in the asset:

```rust,wrap
# use regia_ai::spatial_query::*;
# use regia_ecs::prelude::*;
# use regia_app::prelude::App;
# use regia_core::math::Vec3;
# let mut app = App::new();
struct CoverPoints;
impl GeneratorImpl for CoverPoints {
    fn generate(&self, input: &GeneratorInput<'_>) -> Vec<GeneratedItem> {
        let reach = input.args.get("reach").copied().unwrap_or(10.0);
        input.centers.iter().map(|c| GeneratedItem::Point(*c + Vec3::X * reach)).collect()
    }
}

struct Exposure;
impl TestImpl for Exposure {
    fn evaluate(&self, input: &TestInput<'_>) -> TestValue {
        TestValue::Float(input.anchor.map(|a| (a - input.position).length()))
    }
}

let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
manager.register_generator("cover_points", Box::new(CoverPoints));
manager.register_test("exposure", Box::new(Exposure));
```

```ron
(
    format_version: 2,
    name: "cover",
    item_type: Point,
    params: { "radius": Float(12.0) },
    generator: Custom(name: "cover_points", center: Asker, args: { "reach": ParamF32("radius") }),
    tests: [
        Custom(id: "exposed", purpose: Both, name: "exposure", value: Float, to: Entity(tag: "player"),
               max: 20.0, score: (shape: Falling, weight: 1.0), combine: Lowest),
        Custom(id: "quiet", purpose: Filter, name: "is_quiet", want: true),
    ],
)
```

A custom row is a full member of the pipeline. A generator's items take the generation order it returns them in. A
test row has the usual `purpose`, `min`, `max`, `score`, `require` and `combine`, and declares what its code returns
with `value: Bool` (the default, compared with `want`) or `value: Float`. With a `to` anchor it is evaluated once per
anchor value, with none once per item. `args` are numbers, literals or parameter references, so a missing parameter
is `missing_param` like anywhere else. Both traits also see `params`, every parameter of the request, and a generator
sees `seed`, the request seed.

Rules an implementation must follow, or a run stops being repeatable: it must be a pure function of what it is
handed, and any randomness must come from `seed`.

| What goes wrong | What happens |
|---|---|
| the name is not registered | the run ends `failed` with `sq_missing_system`, naming it |
| a generator returns the wrong item type (an entity in a point query without `allow_mixed`, a point in an entity query) | `sq_item_type_mismatch` |
| a generator returns more than `max_items` items | `sq_too_many_items` |
| a generator returns a non-finite position, or an entity with no position | that item is dropped |
| a test returns the other kind of value than its row's `value` | `sq_bad_param` |
| a float test returns `None` or a non-finite value | no value for this item: it fails a `min` or `max` filter and scores worst |

`ai.query_impls` lists the registered names. Registering is code, so the names are checked when a run starts, not when
the asset loads: an asset can load before the code that serves it is registered.

## From Rust and behavior trees

```rust,wrap
use regia_ai::spatial_query::*;
# use regia_ecs::prelude::*;
# use regia_app::prelude::App;
# let mut app = App::new();
# let agent = app.world_mut().spawn_empty().id();

app.add_plugins(SpatialQueryPlugin);
app.world_mut().resource_mut::<SpatialQueryManager>()
    .load_template("ai/flank.spatialquery.ron")?;

// At once (ignores the per-tick budget):
let result = app.world_mut().resource_scope(|world, mut m: Mut<SpatialQueryManager>| {
    m.run_instant(world, SpatialQueryRequest::new("flank_position", agent).with_param("radius", ParamValue::Float(8.0)))
});

// Sliced across fixed ticks:
let id = app.world_mut().resource_mut::<SpatialQueryManager>()
    .submit(SpatialQueryRequest::new("flank_position", agent));
// ... later ...
if let Some(result) = app.world_mut().resource_mut::<SpatialQueryManager>().poll(id) {
    /* status, items, tests */
}
```

The async tick runs in `FixedUpdate` before `AiStep` and spends at most `work_units_per_tick` (4,096) work
units per tick, breadth first in submit order by default: each round gives every active request an even share of
the units left, so one request with a stage larger than the budget cannot hold back the ones behind it. A *work unit* is one generated item or one
(item, test, anchor value) evaluation; units are counted, never timed. `abort(id)`, `abort_owner(entity)`
and `fail_all()` (world teardown) end requests; a despawned owner ends its requests with `owner_lost`. A finished
result waits for its poll; once its owner is gone it is dropped after 600 fixed ticks unpolled. The tick runs after
`PhysicsStep`, so the tests read this step's physics world.

The behavior-tree leaf `RunSpatialQuery` submits on its first tick, returns `Running` while the query
processes, writes the winner (a vec3 for point items; an entity plus `<key>_position` for entity items)
and returns `Success`, or returns `Failure` and writes nothing. Aborting the node aborts its request.
Register a configured leaf under a name the tree uses:

```rust,wrap
# use regia_ai::spatial_query::*;
# use regia_ai::*;
# let mut registry = todo!();
RunSpatialQuery::new("flank_position", "flank_spot")
    .with_param("radius", BtParam::Key("search_radius".into()))
    .register(&mut registry, "game::FindFlank");
```

## The probe and gizmos

A `SpatialQueryProbe` component (reflected, editor-placeable) runs its query with its own entity as the
asker and keeps the last result (`SpatialQueryProbeState`): `template`, `params`, `pick`,
`rerun_on_move` (re-run after moving 0.25 m, checked once per fixed tick), `draw_failed`, `labels`,
`highlight` (`Scaled`, `Uniform`, `Top(band)`), `only_through_test` (run the tests up to that authoring
index and show the survivors after it) and `max_units` (stop with `aborted` past that many units; 0 = no
limit). The probe system is compiled only with the `regia_ai/spatial-query-probe` feature, which
`regia_agent` enables: shipping builds without the agent never run probes.

Turn on the `spatial_query` gizmo group (`debug.set_gizmos { "group": "spatial_query", "enabled": true }`)
to draw probe results: spheres colored red (worst) to green (best), the winner larger with a ring, failed
items grey when `draw_failed`, labels when `labels`.

## Agent methods

| Method | Permission | Params | Result |
|---|---|---|---|
| `ai.query_list` | read | `{}` | `{ templates: [{ name, path, item_type, generator, tests, params, server_only }], providers, matcher }` |
| `ai.query_run` | play | `{ template, owner, params?, pick?, seed?, keep_failed?, limit? }` | `{ status, items (at most limit, default 50), item_count, tests: [{ id, kind, in, out }], units, message? }` |
| `ai.query_submit` | play | as `ai.query_run` | `{ request }` |
| `ai.query_poll` | read | `{ request, limit? }` | `{ status: "processing", request, units }` while active, then the result as `ai.query_run` (once). A poll that omits `limit` uses the limit stored by `ai.query_submit` for that request (default 50). An explicit poll `limit` wins. The stored cap is dropped when the poll returns a finished result, when the request is unknown, or when `ai.query_abort` drops it. |
| `ai.query_abort` | play | `{ request }` or `{ owner }` | `{ aborted }` (a count) |
| `ai.query_stats` | read | `{}` | `{ active, submitted, completed, aborted, failed, units_last_tick, warnings }` |
| `ai.query_probe` | read | `{ entity, limit? }` | the probe's last result as `ai.query_run`, plus `runs` |
| `ai.query_impls` | read | `{}` | `{ generators, tests, providers, matcher }`: the registered custom generators and tests, anchor providers and the tag matcher ([also here](ai-squads.md)) |

`pick` is `"best"`, `"sample"` (band 0.1), `"all"` or `{ "sample": 0.25 }`. Each item reports `position`,
`entity`, `score`, `passed`, `failed_test`, `failed_reason` (`filter`, `no_anchor`, `snap`,
`unreachable`), `generation_index` and `contributions` (per score test id). Statuses: `processing`,
`success`, `failed`, `aborted`, `owner_lost`, `missing_param`.

```json
{ "template": "flank_position", "owner": 4294967296, "params": { "radius": 8.0 }, "pick": "best" }
→ { "status": "success", "item_count": 37, "units": 412,
    "items": [ { "position": [4.0, 0.0, -2.0], "entity": null, "score": 0.81, "passed": true,
                 "failed_test": null, "failed_reason": null, "generation_index": 12,
                 "contributions": { "near": -0.19 } } ],
    "tests": [ { "id": "sees_player", "kind": "Cast", "in": 81, "out": 37 },
               { "id": "near", "kind": "Distance", "in": 37, "out": 37 } ] }
```

### Error codes

| Code | Meaning |
|---|---|
| `ai_query_unavailable` | `SpatialQueryPlugin` is not installed |
| `sq_unknown_template` | no loaded template has the name and no file exists at the path |
| `sq_unreadable` | the template file does not load (`data.line`, `data.column`) |
| `sq_item_type_mismatch` | a generator child or test does not fit the item type (`data.line`, `data.column`), or a custom generator returned the wrong type (in the result's `code`) |
| `sq_no_threshold` | a `FilterBound` clamp without the filter bound (`data.line`, `data.column`) |
| `sq_unknown_owner` | the `owner` entity does not exist |
| `sq_bad_param` | a bad request value (`params`, `pick`, `seed`, `limit`), a parameter of the wrong type, an unknown physics layer, or a non-finite number (`nan`/`inf`, a literal or a resolved parameter) in a built-in generator or test field |
| `sq_missing_system` | a test or generator needs a physics world, a navigation mesh, a tag matcher, a provider or a custom generator or test that is absent |
| `sq_too_many_items` | the generator would produce more than `max_items` (20,000) items, or a spacing/gap is `<= 0` |
| `sq_no_navmesh` | `ReachLattice` without a navigation mesh |
| `sq_unknown_request` | `ai.query_poll` of an id that is neither processing nor waiting to be polled |
| `F0003` | `ai.query_probe` of an entity without a `SpatialQueryProbe` |

A missing parameter is not an error: `ai.query_run` returns `status: "missing_param"` with a `message`
naming the parameter.

## Performance

The budgets in spec 33 § 6 (desktop, release: 0.6 ms for a 21 x 21 lattice with one cast and one distance test, 5 ms
for 20,000 distance-only items) are measured by hand, never by the gate:

```text
cargo test -p regia_ai --release --test ai_perf -- --ignored spatial_query --nocapture
```

## Determinism

A run is a pure function of (world snapshot, template, parameters, seed): iteration follows generation
order, score ties go to the lower generation index, and the `Sample` pick draws from the request seed
(default: derived from the owner's spawn-order arrival and the request id, never its entity bits; pass
`seed` to reproduce a run). `ai.query_run` with
the same seed on the same world answers identical items and scores. Physics and navigation queries are
read-only and see the world at evaluation time, so an async request may see a world that changed between
ticks; use `run_instant` (`ai.query_run`) for an exact snapshot. Wall-clock time appears only in the
tracing spans `spatial_query_tick`, `spatial_query_generate` and `spatial_query_test`.
