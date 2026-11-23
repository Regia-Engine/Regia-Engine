# Spec 33: Spatial queries for AI

Status: proposed (ADR-1577). Plan item G2 (`BUILD-PLAN-1027.md` § 3), rung v0.3.x (b). Behavior written from the
public-sources spec set (ADR-1526); set area `runtime-ai` (spatial query authoring and running). A builder reads only this page.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built baseline: none for this feature; it builds on `regia_ai` (behavior trees, `BtTaskRegistry`, perception, smart objects), `regia_nav::NavMesh::{find_nearest, find_path}`, `regia_physics` queries (`raycast`, `shapecast`, `overlap`), `regia_core::math::Rng` (PCG32) and `regia_render::gizmos` groups.

## 1. Purpose

An AI agent often asks a spatial question: "where should I stand to shoot the player without being seen", "which
cover point is nearest and reachable", "which pickup should I go for". A **spatial query** answers it with data
instead of code: a *generator* produces candidate **items** (points or entities), *tests* remove items that fail a
condition and score the rest, and a *pick rule* chooses the result. Designers edit a `.spatialquery.ron` asset; behavior trees
(and any Rust code or agent) run it and read the winner.

`regia_ai` has behavior trees, a blackboard, perception, smart objects (spec 31) and a navigation-mesh crate
(`regia_nav`) it can query; it has no way to rank candidate places. This spec adds that, in `regia_ai::spatial_query`.

## 2. Regia terms

| Term | Meaning |
|---|---|
| Query asset | A `.spatialquery.ron` file: one generator, an ordered list of tests, declared parameters, a default pick |
| Item | One candidate: a world position and, for entity items, the entity |
| Item type | `Point` or `Entity`. Tests declare which they accept; a `Union` generator forces one type on its children |
| Anchor | A frame of reference a generator or test points at: the asker, the item itself, a named entity, a position, a blackboard key or a Rust provider |
| Asker | The entity running the query (usually the AI agent's entity) |
| Test | An evaluator with a purpose: `Filter` (drop failing items), `Score` (weight survivors) or `Both` |
| Pick | How the result is chosen from the scored items |
| Request / result | One execution: template + owner + parameters + pick; the result carries items, scores and a status |

## 3. Data model

### 3.1 Query asset: `<name>.spatialquery.ron` (format name `spatialquery`, v1)

```ron
(
    format_version: 1,
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

- `generator` is a single value; several generators are combined with `Union([..])` and all children run.
- `params` declares typed defaults (`Bool`, `Int`, `Float`, `Name`); any numeric field of a generator or test may
  be a literal or `Param*("name")`. A caller overrides by name at request time; a parameter used but neither
  declared nor supplied ends the run with status `missing_param` (never a panic, never a silent default).
- `server_only: true` marks assets that a client-only package may omit. Regia has no authority-role build profile
  today, so the flag is stored and ignored by the cook until one exists; ADR-1585 decision: do not add profile logic
  in this spec.
- Unknown fields or variants are a load error with line and column (`sq_unreadable`).
- **Format rules (spec 12 § 4a):** `format_version` 1; a later change needs a version bump, a migrator and
  `regia upgrade-project` support; the lead registers the format in `check-formats`. There is no earlier version.

### 3.2 Anchors

`Anchor` values (RON): `Asker`; `Candidate` (the item under test; only valid inside tests); `Entity { tag | name,
height }` (all live entities with the tag or name; empty set is a legal anchor that yields no value); `Point(Vec3)`;
`Blackboard(key)` (a `BbValue` vec3 or entity on the asker's `Blackboard`); `Provider(name)` (a Rust `AnchorProvider`
registered by name); each may carry a vertical `height` offset (default 0).

```rust
pub trait AnchorProvider: Send + Sync {
    fn provide(&self, world: &World, asker: Entity) -> AnchorData;   // positions and/or entities
}
pub struct AnchorData { pub points: Vec<Vec3>, pub entities: Vec<Entity> }
```

An anchor may yield several values. A test whose anchor yields several values evaluates each and combines:
filtering with `Every` or `Some` (default `Every`), scoring with `Mean`, `Lowest`, `Highest` or `Product`
(default `Mean`); see § 3.5. An anchor that yields **no** value makes a filter test fail every item (reason `no_anchor`)
and a score test contribute nothing (not zero).

### 3.3 Generators

All generators produce `Point` items unless noted; every point is optionally projected onto the navigation mesh
(`project_to_navmesh`, using `regia_nav::NavMesh::find_nearest` with the query's `extents`, items with no mesh point
within the extents are dropped) and optionally lowered by a downward trace to ground (`drop_to_ground: height`).

| Generator | Output | Parameters | Limits and edge cases |
|---|---|---|---|
| `NearbyEntities` | `Entity` items | `tag` or `name`, `center: Anchor`, `radius: Option<f32>` | `radius: Some(r)` with `r <= 0` returns no items; `None` searches the whole world |
| `AtAnchor` | one `Point` per value of an anchor | `anchor` | for validating tests in isolation |
| `Ring` | points on a circle edge | `center`, `radius`, spacing as `gap` or `count`, optional arc (`direction`, `angle` degrees), vertical offset | `radius <= 0` or `count == 0` yields nothing; `gap` larger than the circumference yields one point |
| `Wedge` | points along a cone | `center`, facing, `degrees`, `range`, `angle_step` (≥ 1 degree, smaller values are clamped to 1), `aligned_spacing`, `include_center` | `include_center` adds the origin once and ignores the spacing |
| `Annulus` | points on concentric rings | `center`, `inner_radius`, `outer_radius`, `rings`, `points_per_ring`, optional arc, optional spiral (rotates each ring by a fixed angle) | `outer < inner` is swapped; `rings == 1` uses the mid radius |
| `Lattice` | square lattice | `center`, `half_size`, `spacing` | `spacing <= 0` or more than `max_items` points yields `sq_too_many_items` (see § 3.6) |
| `ReachLattice` | like `Lattice`, then keeps only points the navigation mesh can reach from the center | plus `reach_limit` (default 1.5: the largest allowed center distance as a multiple of `half_size`) | needs a navigation mesh; without one it fails with `sq_no_navmesh` |
| `Union` | union | `children: [Generator]` | forces the declared `item_type`; a child of another type is an `sq_item_type_mismatch` load error unless `allow_mixed: true`, in which case entity items are flattened to their positions |

Generation order is deterministic: row-major for `Lattice` and `ReachLattice`, angle ascending for `Ring` and `Wedge`, ring
by ring for `Annulus`, tag-arrival order for tag entity sets and `Name`-arrival order for name sets (each via its
spawn-order table, falling back to the entity id for entities the tracking never saw,
ADR-4651). All angular offsets come from the deterministic sin/cos kernel (`regia_core::math::det_sin_cos`),
not the platform libm. Duplicate items (same position to within 1e-4 and same entity) are
removed in `Union` keeping the first.

### 3.4 Tests

Every test has an `id` (unique in the asset; used in debug output), a `purpose` (`Filter | Score | Both`) and the anchors
it needs. A test accepts items of one type (§ 3.1); using it on the other type is a load error. Tests fall in three
groups by what they read.

**Geometry of the item** (pure math, no other system):

| Test | Raw value | Notes |
|---|---|---|
| `Distance` | distance from the item to each anchor value | `mode`: `Full3d`, `Planar` (ignore height), `Vertical` (signed height difference), `AbsVertical` |
| `Facing` | dot product of two unit directions, -1..1 | each direction is an anchor's forward or the direction between two anchors; `Planar` or 3D; `abs: bool` |
| `InsideZone` | boolean | the point is inside any zone entity carrying a tag; `mode`: `Exact` or `Bounds` |
| `TagMatch` | boolean | the item entity (or the entity nearest a point) satisfies a tag query (ADR-1463 `TagQuery`); entities without tags fail |

**Physics world** (`regia_physics` queries with a `QueryFilter` built from named layers):

| Test | Raw value | Notes |
|---|---|---|
| `Cast` | boolean (blocked or not) | `shape`: `Line`, `Sphere(r)`, `Box(half)`, `Capsule(r,h)` through `raycast` / `shapecast`; `from`/`to` anchors with per-end heights; `layers`; `blocking_only`; `want_hit` is the desired boolean (`false` keeps points with a clear line to the anchor) |
| `Clearance` | boolean (anything overlaps) | box, sphere or capsule with extents and offset at the item through `overlap`; `layers`, `blocking_only`, `skip_asker` |

**Navigation mesh** (`regia_nav`):

| Test | Raw value | Notes |
|---|---|---|
| `Route` | boolean (a path exists) or float (cost or length) | between the item and an anchor, `direction: ToAnchor | FromAnchor`; `drop_unreachable` makes unreachable items fail instead of scoring worst |
| `RouteAll` | like `Route` for every anchor value in one pass | `scan_limit` (default 1.5 times the largest item-to-anchor distance seen) bounds the examined distance |
| `Snap` | rewrites the item | moves it to the navigation mesh (or to geometry on named layers) searching `up`/`down` distances, then adds `offset`; items that cannot snap are dropped |

`TagMatch` needs a tag matcher: `regia_ai` and `regia_framework` are siblings in the layer table and no edge between
them is allowed, so the host registers a `TagMatcher` function (`fn(&World, Entity, &str) -> bool`) on the manager.
Decision (ADR-1585): matcher callback, no new crate edge. A query using a test whose backing system is absent (no
physics world, no navigation mesh, no matcher) fails with `sq_missing_system` naming the system.

### 3.5 Float tests: filter and score

A float test yields a raw value `v` per item and anchor value.

- **Filter bounds.** `min: Option<f32>` keeps `v >= min`, `max: Option<f32>` keeps `v <= max`; both set is an inclusive
  range. A boolean test keeps items whose boolean equals the wanted value.
- **Score** (when the purpose is `Score` or `Both`), in this order:
  1. Resolve the bounds `lo` and `hi`. Each of `clamp_min` / `clamp_max` is `Fixed(f32)`, `FilterBound` (the filter
     `min` / `max`; `sq_no_threshold` at load if that bound is absent) or `Seen` (the smallest / largest `v` among items
     still alive when this test's score pass starts).
  2. `v_c = clamp(v, lo, hi)`.
  3. Normalize to `n`: with `origin: Floor` (default) `n = (v_c - lo) / (hi - lo)`; with `origin: Zero`, `n = v_c / hi`.
     A zero denominator gives `n = 0`.
  4. With `peak_at: Some(r)`: `n = 1 - |v_c - r| / max(|lo - r|, |hi - r|)`; a zero denominator gives `n = 1`.
  5. Shape, applied to `n` clamped to `0..=1`: `Flat` 1, `Linear` `n`, `Quadratic` `n^2`, `Falling` `1 - n`, `Root` `sqrt(n)`.
  6. Contribution = shape * `weight` (may be negative). A boolean test contributes `weight` when it matches the wanted
     value, else 0.
- **Item score** = sum of contributions / sum of the absolute `weight`s of the score tests (0 when none), so scores lie
  in -1..1 and compare across queries. Decision (ADR-1585): this normalization is accepted.
- **Several anchor values** (an anchor may yield many): filtering takes `require: Every | Some` (default `Every`);
  scoring combines per-value results with `combine: Mean | Lowest | Highest | Product` (default `Mean`). An anchor that
  yields **no** value makes a filter test fail every item (`failed_test` set, reason `no_anchor`) and a score test
  contribute nothing (not zero).
- **Order.** Filters (and the filter half of `Both` tests) run first, in authoring order, each over survivors only; then
  score passes run over the final survivors. A stable partition makes authoring order irrelevant to the result.

### 3.6 Execution, picking and limits

Pipeline per request: generate, filter, score, pick. All iteration follows the generation order of § 3.3; score ties go
to the lower generation index; randomness comes only from the request `seed` through `regia_core::math::Rng`.

| Pick | Result |
|---|---|
| `Best` | the single highest-scored item |
| `Sample { band }` | one item drawn uniformly from the items with `score >= best - band * (best - worst)`; `band` in 0..=1, default 0.1 |
| `All` | every survivor, score descending, ties by generation index |

An empty survivor set is status `Failed` for `Best` and `Sample`, and `Success` with zero items for `All`.
Decision (ADR-1585): `Sample` draws from a score band rather than a share of the item count (the count form is unstable
when many items tie).

Limits (`SpatialQueryConfig`, overridable): `max_items` 20,000 generated items per request; `max_tests` 32;
`work_units_per_tick` 4,096 (async); `active_warn_threshold` 64 active requests; `slow_warn_units` 200,000 work units
per request. A *work unit* is one generated item or one (item, test, anchor value) evaluation, counted never timed, so
slicing and warnings are deterministic. `RouteAll`'s default `scan_limit` pre-pass costs one unit per alive item and
pauses with the budget like the evaluations (ADR-7559). Wall-clock time appears only in tracing spans.

`QueryStatus` (also the agent string): `processing`, `success`, `failed`, `aborted`, `owner_lost`, `missing_param`.

## 4. Runtime API (`regia_ai::spatial_query`)

```rust
pub struct SpatialQueryManager { /* resource */ }       // registers templates, providers, matchers; owns the active list
pub struct SpatialQueryPlugin;                            // adds the resource, the tick system (FixedUpdate) and the gizmo group
pub struct SpatialQueryRequest {
    pub template: String,              // project-relative path or asset name
    pub owner: Entity,                 // the asker
    pub params: BTreeMap<String, ParamValue>,
    pub pick: Option<Pick>,            // None = the asset's default pick
    pub seed: u64,                     // default: derived from the owner's spawn-order arrival
                                        // (behavior-tree arrival, ADR-4651) and the request id
}
impl SpatialQueryManager {
    pub fn run_instant(&mut self, world: &mut World, req: SpatialQueryRequest) -> SpatialQueryResult;   // synchronous, ignores the budget
    pub fn submit(&mut self, req: SpatialQueryRequest) -> RequestId;                                // async: advances on ticks
    pub fn poll(&mut self, id: RequestId) -> Option<SpatialQueryResult>;                            // None while processing; takes the result
    pub fn abort(&mut self, id: RequestId) -> bool;                                             // true if it was active
    pub fn abort_owner(&mut self, owner: Entity) -> usize;                                      // count aborted
    pub fn fail_all(&mut self);                                                                 // world teardown: every active request ends `failed`
    pub fn stats(&self) -> SpatialQueryStats;
    pub fn register_provider(&mut self, name: &str, p: Box<dyn AnchorProvider>);
}
pub struct SpatialQueryResult { pub status: QueryStatus, pub items: Vec<ScoredItem>, pub tests: Vec<TestSummary> }
pub struct ScoredItem { pub position: Vec3, pub entity: Option<Entity>, pub score: f32, pub passed: bool,
                        pub failed_test: Option<String>, pub contributions: Vec<(String, f32)> }
```

- Items that failed a filter are present only when `keep_failed: true` on the request (debug); the default
  result holds survivors only.
- A request whose owner is despawned before it finishes ends with `owner_lost`; one whose parameter is missing
  ends with `missing_param` naming the parameter in `SpatialQueryResult::message`.
- `abort(id)` on a finished or unknown id returns false. `fail_all` runs from the `World` teardown path (editor
  stop, level unload of the whole world).
- Async slicing is **breadth first by default**: each tick, active requests in submit order each advance by one
  test (or the generator) until `work_units_per_tick` is spent; a request that cannot finish in the tick resumes
  next tick in the same order. `breadth_first: false` finishes the oldest request before the next.
- Template lookup order: an already-loaded template by name, then the asset server by path. Templates reload when
  the file changes in dev builds (running requests finish on the version they started with).

### 4.1 Behavior-tree integration

A leaf task `RunSpatialQuery` (registered with `BtTaskRegistry` like other tasks) with fields `template`, `key`
(blackboard key for the winner), `pick`, `params: {name → literal | blackboard key}`. First tick: submits and
returns `Running`. Next ticks: `Running` while processing; on `success` writes the best item into `key` (a vec3 for
`Point` items, an entity for `Entity` items, which also writes `<key>_position` as a vec3) and returns `Success`; on
any other status or an empty result returns `Failure` and writes the reason — the result message, else the status
name — into `<key>_error` (a string), so a missing parameter and a real miss are distinguishable on the blackboard.
A finished result is kept (not polled) while the agent has no `Blackboard`; the leaf fails that tick and retries.
When the tree aborts the node, the task aborts its request. The task never blocks the tick.

### 4.2 Debug and preview

- Gizmo group `SpatialQueryGizmos` (`"spatial_query"`, off by default): spheres at items colored by score (red worst → green
  best, scaled to the result's min/max), the winner larger with a ring, failed items grey when `keep_failed`, text
  labels (score, or the failing test id) when enabled. A *highlight mode* shows all items at one size, or emphasizes
  the best 10 % (or a chosen band) and shrinks the rest.
- **`SpatialQueryProbe`** (component, reflected, editor-placeable): `template`, `params`, `pick`, `rerun_on_move`
  (re-run when the entity has moved ≥ 0.25 m since the last run, evaluated once per fixed tick, default true),
  `draw_failed`, `labels`, `highlight`, `only_through_test: Option<usize>` (draw the survivors after only that test),
  `max_units` (stops computing a preview past that many work units, 0 = no limit). The probe keeps its last result
  for the gizmos and the agent. A probe never runs in packaged builds (the system is compiled out of `ship`
  profiles).

## 5. Agent protocol surface

Page `book/agents/spatial-queries.md`, guide topic `spatial-queries`, `book/COVERAGE.md` rows. Degrades with
`ai_query_unavailable` when `SpatialQueryPlugin` is not installed.

| Method | Permission | Params | Result |
|---|---|---|---|
| `ai.query_list` | read | `{}` | loaded templates `{name, path, item_type, tests, params}`; provider and matcher names |
| `ai.query_run` | play | `{ template, owner, params?, pick?, seed?, keep_failed?, limit? }` | `{ status, items (≤ limit, default 50), item_count, tests: [{id, in, out}], units }` |
| `ai.query_submit` | play | same as run | `{ request }` |
| `ai.query_poll` | read | `{ request }` | `{ status, ... }` as run; `processing` while active |
| `ai.query_abort` | play | `{ request }` or `{ owner }` | `{ aborted }` |
| `ai.query_stats` | read | `{}` | `{ active, submitted, completed, aborted, failed, units_last_tick, warnings }` |
| `ai.query_probe` | read | `{ entity }` | the probe's last result as `ai.query_run` |

Errors (lowercase codes, documented in the book page): `ai_query_unavailable`, `sq_unknown_template`,
`sq_unreadable` (with line/column), `sq_unknown_owner`, `sq_bad_param`, `sq_missing_system`,
`sq_too_many_items`, `sq_item_type_mismatch`, `sq_no_threshold`, `sq_no_navmesh`, `sq_unknown_request`.
`run` on a missing parameter does not error: it returns `status: "missing_param"`.

## 6. Determinism, performance and platforms

- Pure function of (world snapshot, template, params, seed). Two runs on the same world give bit-identical items and
  scores. Physics and navigation queries are read-only and use the state at the time of evaluation; an async request
  may therefore see a world that changed between ticks (documented; use `run_instant` for exact snapshots).
- Budgets (desktop, release): a 21×21 lattice (441 items) with one cast and one distance test ≤ 0.6 ms; `run_instant`
  of 20,000 distance-only items ≤ 5 ms; async mode never exceeds `work_units_per_tick` units per tick (checked by
  counter). `tracing` spans: `spatial_query_tick`, `spatial_query_generate`, `spatial_query_test{id}`.
- Linux, Windows, Android aarch64: pure Rust over existing systems; no platform code.

## 7. Tests and acceptance

No golden, GPU or wall clock is needed.

1. Generate → filter → score → pick returns the highest-weighted survivor (synthetic world with kinematic
   obstacles).
2. Authoring order of filters and scorers does not change the result (permutation test over a 4-test query).
3. `Union` runs all children; a child of the wrong item type is rejected at load unless `allow_mixed`.
4. `NearbyEntities` with `radius` ≤ 0 returns nothing; `None` returns all matches; the radius is inclusive.
5. `Cast` with `want_hit: false` keeps exactly the points with a clear line to the anchor.
6. `Distance` scoring with `weight -1.0` ranks nearer survivors higher.
7. Each generator's exact point set for a fixed input (ring count/gap/arc, wedge with `include_center`, annulus
   spiral, lattice with `project_to_navmesh` on a small mesh, `ReachLattice` dropping an island).
8. Float scoring: each shape, both origins, each clamp source, `peak_at`, and the zero-denominator cases
   with expected values to 1e-6.
9. Multi-value anchor: `Every` vs `Some` filtering; `Mean`, `Lowest`, `Highest`, `Product` combination; empty anchor
   rules (filter fails all, score contributes nothing).
10. Picks: `Best`, `Sample` at band 0.1 and 0.25 (same seed → same pick; different seeds eventually pick every
    band member; the pick is always inside the band), `All` ordering and ties by generation index.
11. A parameter used but not declared or supplied → `missing_param`; a supplied value overrides the default.
12. `abort(id)` ends a processing request with `aborted`; `abort_owner` counts; `fail_all` ends all with `failed`;
    despawning the owner ends the request with `owner_lost`.
13. Async slicing: with `work_units_per_tick = N`, no tick spends more than N units (+ the single indivisible unit),
    breadth-first order is submit order, and the results equal `run_instant`'s.
14. `RunSpatialQuery` writes a vec3 for point items and an entity (plus `_position`) for entity items; failure writes
    nothing; aborting the tree node aborts the request.
15. `server_only` round-trips through load and is ignored by the cook.
16. Load errors: unknown field, bad test type for the item type, duplicate test id, threshold clamp without a
    threshold; each with line and column.
17. Agent: every method and error code; `ai.query_run` identical twice for the same seed.
18. Probe: `rerun_on_move` re-runs after a move ≥ 0.25 m and not before; `only_through_test` shows the survivors after test k.
19. `cargo xtask agent-coverage`, `book-check`, `check-layers` pass.

## 8. Out of scope

Time-sliced generation inside one generator; learning-based scoring; asset-graph editor UI (the asset is RON; the
editor shows it in the inspector and previews it with a probe); a navigation-area cost model beyond what `regia_nav`
already answers; network replication of queries; querying other worlds.

Custom generators and tests registered from Rust through a trait were out of scope for format v1 and are built (ADR-2709,
format v2, `book/agents/spatial-queries.md`): `GeneratorImpl` (`Generator::Custom`) and `TestImpl` (`Test::Custom`),
registered by name on the manager. A name with no implementation ends the run with `sq_missing_system`, as an
unregistered `Provider` does; a custom generator's wrong item type is `sq_item_type_mismatch`; a custom test's wrong kind
of value is `sq_bad_param`. Version 2 only adds the two variants, so a v1 file means the same thing and
`regia upgrade-project` rewrites its version number; a v1 file using a custom row is `sq_unreadable`.

## 9. Migration

New format `spatialquery` v1; nothing existing changes. `regia_ai` gains a module and plugin; new public items only. The lead runs `cargo xtask check-formats --update` and `cargo xtask manual`.

## 10. Provenance

Public user documentation of the spatial-query workflow (behavior, parameters and debugging views only); no
algorithm beyond standard geometry. Set area: `runtime-ai`. Row in `docs/provenance.md`
(ADR-1577).

### Public-page citations

Sources-section URLs recorded by the spec writer on 2026-11-08 (ADR-1526; ADR-1661).
These are documentation/reference citations, including public API-reference pages; only the
URLs were transcribed from the external set.

- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 11. Guide page outline (`book/agents/spatial-queries.md`)

Authoring a query asset; a first query (flank position) end to end; anchors; each generator and test with one
example; picks; BT usage; the probe and gizmos; agent methods and error codes; determinism notes.

## 12. Owner decisions

None. Technical points are decided in the text (score band, normalization, tag matcher callback, `server_only`).

## 13. Conformance notes (ADR-5633, 2026-11-10)

The built API refines § 4 without changing behavior:

- `SpatialQueryRequest::seed` is `Option<u64>`: `None` derives the seed from the owner's arrival order and the request id
  (ADR-4651). The request also has `keep_failed`.
- `SpatialQueryResult` also has `item_count`, `units`, `message` and `code` (the agent error code of a failure).
- `ScoredItem` also has `failed_reason` (`filter`, `no_anchor`, `snap`, `unreachable`) and `generation_index`.

Behavior the spec left open:

- `fail_all` is a host call. No engine path calls it: the editor's play stop drops the whole play world, and its
  manager goes with it. Level travel despawns the owners, so their requests end `owner_lost`.
- `drop_to_ground: h` traces downward from `h` above the point over `2h`, so a point can rise or fall by up to `h`.
  A point with no ground inside the trace is dropped, as a failed `project_to_navmesh` is.
- § 7 item 14's "failure writes nothing" means that no winner is written. `<key>_error` is still written (§ 4.1,
  ADR-4910).

Evidence for §§ 3.2-3.6 and § 4.2 that the § 7 tests do not cover is in
`crates/regia_ai/tests/spatial_query_conformance.rs`.
