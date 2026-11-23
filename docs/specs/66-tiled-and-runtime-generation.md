# Spec 66: Tiled, multi-scale and runtime generation

Status: proposed (ADR-1596). Part of the procedural content graph (spec 62). Behavior written from the public-sources spec
set, area `runtime-procedural` (ADR-1526, behavior only); the tile rules, halo, tier clipping, anchor scheduling formula
and all names are Regia's own.
Reviewed: integ/specs (ADR-1585), 2026-10-28.
Built (ADR-5646 audit, 2026-11-10): `regia_procgen::tiles` (cells, halo, tiers, scheduler, ECS host, overlay; ADR-1666,
ADR-2003, ADR-2421, ADR-5153), the agent methods of § 6 (`regia_agent::procgen_tiles`), the editor's "generate around view"
(ADR-2907) and the sample `samples/procgen_open_world`. Related built code: `regia_terrain::scatter::runtime` (64 m scatter
tiles around the camera); `regia_scene::stream_grid` (`CellCoord`, `StreamGrid`) and `regia_scene::stream_plan`
(`StreamPlanner`, load/unload radii with hysteresis; ADR-1566).

## 1. Purpose

Spec 62 runs a generator over its whole bounds at once. Large worlds need three more things:
1. **Tiles:** split a generator into world-aligned cells so an edit re-runs only the cells it touches and baked output
   streams by cell.
2. **Tiers:** let one graph work at several cell sizes at once (a coarse pass places forests, a fine pass places
   pebbles), passing data only from coarse to fine.
3. **Anchored runtime generation:** generate cells at runtime around **anchors** (the camera, the player, any entity)
   and release them when anchors move away, within per-frame budgets, with the same output as a bake of those cells.

Baseline: the runtime scatter already streams 64 m tiles around the camera (`regia_terrain::scatter::runtime`); the grass
system places patches around the camera on the GPU. Those stay. This spec gives the general graph the same ability.

Decision (ADR-1585): `regia_procgen::tiles` owns its scheduler (per-tier radii, view gate, priority and per-frame budgets
are not in the world streaming planner of ADR-1566, which has one load/unload radius pair and a byte budget). It reuses
the planner's grid math: cell indices are `regia_scene::stream_grid::CellCoord` (`i64` x/z) and distances come from
`StreamGrid::distance_to_cell` (ADR-2003: `tiles` uses them directly through its `TileCell` trait).
`regia_procgen` is layer 3 and may depend on `regia_scene`.

What this spec takes from spec 62 (summarized so this spec stands alone):
- `ProcGenerator` is the component that runs a graph over its bounds; modes `Baked` (default), `OnLoad`, `Anchored`.
  `Evaluator::run(&CompiledGraph, &RunInputs, &CancelToken)` is a pure function; world data reaches it only through a
  `WorldSnapshot` captured on the main thread before the run (`ProcGenSet::Capture`), which records dependencies.
- A run moves through `queued → capturing → running → applying → ready` (or `failed`, `cancelled`, `cleared`). Cancelling
  publishes nothing; `applying` swaps the old output for the new one in one frame.
- Outputs (`ProcOutput`) are instance sets, spawned entities (tagged `Generated`), generated meshes, exported masks and
  tables, terrain edits. `.pgout` is the baked output file (format `pgout`, `format_version: 1`).
- Node outputs are cached by a hash that includes tier and cell; seeds are per element and independent of schedule.

## 2. Tiles

`ProcGenerator::tiles: Option<TileSettings>` (adds a field to the spec 62 component; `ProcGenerator` is new in the same
batch, so this is part of its v1 shape):

```rust
pub struct TileSettings {
    pub cell_m: f32,          // base cell size, default 128; a power of two in meters between 8 and 4096
    pub halo_m: Option<f32>,  // None = 2 × the graph's declared reach (below), at least 4 m
    pub runtime: Option<AnchorSettings>, // § 4; required when mode is Anchored
}
```

- Cells are aligned to the world origin: cell `(i, j)` covers `[i·c, (i+1)·c) × [j·c, (j+1)·c)` in XZ (half-open, Y
  unbounded). Only cells that intersect the generator bounds run; each run's bounds are the cell clipped to the generator
  bounds.
- **Ownership:** every output point (and every spawned instance or prefab) belongs to exactly the cell containing its
  final XZ position. After a cell's run, outputs outside the cell's half-open interior are dropped. So no instance is
  placed twice or lost at a seam. Outputs that span cells follow fixed rules:
  - generated meshes (spec 64 § 6: ribbons, spline meshes, junction patches, markings) are cut at cell boundaries: a
    ribbon is split between rows at the cell edge (one extra row is inserted exactly on the edge so both halves share it);
    a spline-mesh piece or junction patch belongs whole to the cell containing its start point (piece) or centroid (patch);
  - exported tables and masks are per cell (consumers read the cells they need);
  - exported point sets (`out.points`) concatenate across cells. Cells of one tiled output give the
    same point-set name one schema: cells that disagree on an attribute's type fail the run (and a
    bake holding them fails to decode) with `attr_type_conflict`, naming both cells and the
    attribute (v0.5.1; the second cell's points used to be dropped silently);
  - terrain edits (spec 64 § 7.3) are not allowed in tiled generators with anchors; in baked tiled generators each cell
    records the part of the edit inside its rectangle.
- **Declared reach.** Every node type reports `reach_m(settings)`: how far from an element its decision can look
  (`points.prune`: 2 × `max_footprint_m` (default 8), the setting that bounds the largest box half-diagonal it sees, since
  reach is known before the run (ADR-5646);
  `points.neighbors`: `radius_m`; `sample.poisson`: `2 · min_dist_m`; `biome.resolve`:
  2 × the largest `core_m`; most nodes: 0). The graph's reach is the maximum over its nodes; the default halo is twice it.
- **Halo:** a cell runs over its rectangle grown by `halo_m` so that neighborhood nodes (`points.prune`,
  `points.neighbors`, `biome.resolve`) see the points their decisions depend on. Decisions near a seam
  match an untiled run unless a chain of dependent decisions reaches farther than the halo; the halo test measures the
  difference (§ 8 test 2, ≤ 0.1 % of instances on its test map) and the run report counts `seam_mismatch_risk` points (points
  whose decision used a neighbor within 1 m of the halo edge).
- Seeds (spec 62 § 6.2): point seeds and per-element draws never depend on the cell, so an element computed in a halo
  gets exactly the values it gets in its own cell (that is what makes halos agree). `cell_key = mix(i as u32, j as u32)`
  only feeds per-run draws of custom node types.
- Edits: a dependency change (spec 62 § 5.3) marks only the cells whose halo-grown rectangle intersects the change.
  Only terrain changes carry a rectangle today, and graphs with road nodes, `terrain.flatten_road` or `graph.call` re-run every
  cell even for those: they read the ground farther than the halo (ADR-2003).
- Baked tiled output: `.pgout` version 3 carries a cell table (cell coords, tier, byte range, output hash per cell;
  spec 62 § 8). Versions 1 and 2 are migration sources. Re-baking one cell rewrites the file
  atomically with the other cells' sections copied unchanged.
- Untiled generators behave exactly as spec 62 (one "cell" equal to the bounds, key 0).

## 3. Tiers (multi-scale graphs)

- A graph declares tiers with `tiles.tier` nodes: pins `in` → `out` (data passes through unchanged), setting
  `cell_m: Base | Whole | Meters(f32)` (default `Base`; `Meters(m)` must equal the generator's base `cell_m` times 2^k
  for an integer k, positive or negative, and stay within 8 to 4096 m; so 32 m and 256 m are both valid tiers of a 64 m
  base). Every node downstream of a `tiles.tier` node runs at that tier. A node's tier is the **finest** tier among
  its inputs, where **source nodes** (nodes without inputs: `in.*`, `gen.*`, `biome.areas` with an unconnected `bounds`)
  are tier-neutral: they adopt the tier of their consumers and are captured once per consuming cell at that tier. A graph
  without `tiles.tier` nodes runs entirely at the base tier. Tier index: `whole` = 0, then tiers by decreasing cell size.
- Tiers must nest: every finer cell lies inside exactly one coarser cell (guaranteed by sizes that differ by powers of two
  on the shared origin). A size that is not base × 2^k is a compile error `tier_not_nested`.
- **Data flows coarse to fine only.** An edge from a finer-tier node into a coarser-tier node is a compile error
  `tier_upward_edge`. When a fine cell consumes coarse-tier data, the coarse items are **clipped** to the fine cell plus
  halo (points outside dropped, splines and polygons kept whole since they are regions); the run report shows the clip.
  A `tiles.no_clip` node (pins `in` → `out`, no settings) placed on the fine side of such an edge keeps the coarse data
  whole for its consumers, for nodes that need global context, at memory cost.
- Scheduling order: a fine cell runs only after the coarse cell containing it is `ready` (and the `whole` tier before
  anything). Each tier caches its cell results separately.
- Typical use: `whole` tier computes biome claims and road exclusions once; a 256 m tier places trees; a 32 m tier places
  ground clutter around them.
- Debug: the overlay draws each tier's grid; `procgen.inspect` takes `cell: [i, j], tier: n` to pick a cell's data.

## 4. Anchored runtime generation

### 4.1 Anchors

```rust
/// Marks an entity whose position pulls anchored generation toward it.
/// `Default` is hand-written: `reach_scale` 1, `use_view_direction` true, `format_version` 1.
#[derive(Component, Reflect, Clone)]
#[reflect(Component, Default)]
pub struct ProcGenAnchor {
    pub reach_scale: f32,      // multiplies every tier's build radius for this anchor, default 1
    pub use_view_direction: bool, // camera anchors: weight cells in front higher, default true
    pub format_version: u32,   // 1
}
```

- `AnchorSettings::anchors: PrimaryView | Tagged | Both` (default `PrimaryView`): the primary view is an implicit anchor;
  `Tagged` uses entities with `ProcGenAnchor`. A local player's camera should carry `ProcGenAnchor` in multi-view games.
- In the editor, the editor viewport camera is an anchor when the editor's "generate around view" switch is on (spec 67),
  so authors see runtime generation without PIE.

### 4.2 Radii, priority and visibility

Per tier (`AnchorSettings::tiers: Vec<TierReach>`, matched to the graph's tiers by `cell_m`; a tier with no entry uses
`base_reach_m`):

```rust
pub struct TierReach {
    pub cell_m: f32,
    pub build_m: f32,              // cells within this distance of an anchor are wanted
    pub release_factor: f32,       // release beyond build_m × release_factor; clamped to ≥ 1.1 (hysteresis)
    pub view_bias: f32,            // 0..1, how much facing direction raises priority (default 0.5)
    pub view_gate: Option<ViewGate>, // build only cells near the view frustum
}
pub struct ViewGate { pub build_margin_deg: f32, pub release_margin_deg: f32 } // release margin clamped to ≥ build margin

pub struct AnchorSettings {
    pub anchors: AnchorSource,           // PrimaryView (default) | Tagged | Both
    pub base_reach_m: f32,               // default 256; used by tiers without a TierReach and by untiled Anchored generators
    pub tiers: Vec<TierReach>,
    pub max_new_per_frame: Option<u32>,  // None = platform default (§ 4.3)
    pub max_running: Option<u32>,
    pub apply_budget_ms: Option<f32>,
    pub apply_budget_elements: Option<u32>, // deterministic stand-in used by tests and render-out
    pub pool_cells: u32,                 // default 64
    pub max_resident_mb: Option<u32>,    // None = platform default (§ 5)
    pub wait_on_load: bool,              // default true
    pub quality_reach: [f32; 4],         // default [0.5, 0.75, 1.0, 1.25]
}
```

An untiled generator in `Anchored` mode is a single cell: it is wanted while any anchor is within `base_reach_m` of its
bounds and released beyond `base_reach_m × 1.25`.

- Distance: from the anchor's XZ position to the nearest point of the cell's XZ rectangle, divided by the anchor's
  `reach_scale`.
- A cell is **wanted** when its distance ≤ `build_m` for some anchor (and, with a view gate, it intersects the view frustum
  widened by `build_margin_deg`). A resident cell is **released** when its distance > `build_m × release_factor` for all
  anchors (and, with a view gate, it is outside the frustum widened by `release_margin_deg`). Between the two it stays.
  Because release ≥ build in both distance and angle, a cell never flips every frame at a boundary.
- The validation warns `tier_reach_order` when a finer tier's `build_m` exceeds a coarser tier's (fine detail farther than
  its parent data is wasted: the parent cell would not be resident). As built (ADR-5646) the scheduler logs it when it plans
  the generator and `procgen.tiles` lists it under `warnings`.
- Priority of a wanted cell (lower first): `p = (d / build_m) · (1 − view_bias · facing)`, `facing = max(0, cos θ)` with
  θ the angle between the anchor's view direction and the direction to the cell center (facing = 0 without a view
  direction). Ties: coarser tier first, then cell `(i, j)` ascending.

### 4.2a Scheduler types (`regia_procgen::tiles`)

```rust
pub struct TileKey { pub tier: u8, pub cell: CellCoord } // tier index as in § 3 (whole = 0)
pub enum TileState { Queued, Capturing, Running, Applying, Resident, Releasing, Failed } // agent strings in § 6
pub struct ProcGenTiles { /* per generator: BTreeMap<TileKey, TileEntry>, pool, counters */ }
impl ProcGenTiles {
    pub fn state(&self, generator: Entity, key: TileKey) -> Option<TileState>;
    pub fn all_ready_near(&self, generator: Entity, anchor: Vec3, tier: Option<u8>) -> bool;
    pub fn step(&mut self, anchors: &[AnchorView], settings: &AnchorSettings, budget: &mut ApplyBudget); // pure planning half
}
pub struct AnchorView { pub pos: Vec3, pub view_dir: Option<Vec3>, pub frustum: Option<Frustum>, pub reach_scale: f32 }
```

As built (ADR-1666 (5)), `step` is a method of the per-generator `GeneratorTiles` (`ProcGenTiles::generators` holds one per
generator); `ProcGenTiles` keeps `state` and `all_ready_near` per generator.
`step` is the deterministic planning half (wanted/released sets, queue order, which cells start); the ECS system wraps it
(`ProcGenSet::Schedule`, after `Capture`'s previous frame, before apply). Tests drive `step` directly with synthetic anchors.
`ProcGenSettings::paused: bool` is a resource field on the spec 62 settings resource. Runs of different cells reuse the
spec 62 lifecycle per cell (a failed cell stays `failed` until the next `procgen.tiles_rebuild` or until it is no longer
wanted and is wanted again).

### 4.3 Scheduler and budgets (`ProcGenTiles` resource, `ProcGenSet::Schedule` system set)

Each frame:
1. Update anchors; compute wanted and released cell sets per anchored generator. (As built, ADR-1666 (5) and ADR-5153:
   the full wanted set is recomputed when the anchors or settings change and reused otherwise; the "anchors that moved more
   than 1/8 cell" increment is not used, since the results are identical.)
2. Released cells: outputs removed (instances, spawned entities, colliders, generated meshes) within the apply budget;
   their instance buffers return to the generator's **pool** (default 64 cells' worth; `pool_cells: 0` disables pooling).
3. Wanted cells not resident and not queued: queued by priority. At most `max_new_per_frame` (desktop 8, Android 4) enter
   `capturing` per frame; at most `max_running` (desktop 4, Android 2) cells evaluate at once across all generators.
4. Finished cells apply within `apply_budget_ms` (desktop 1.0, Android 0.5) on the main thread; an apply that would exceed
   the budget continues next frame (a cell is visible only when fully applied; coarse-tier content stays visible meanwhile).
5. A queued cell that stops being wanted before it starts is dropped; a running cell that stops being wanted is cancelled
   (spec 62 § 5.2, nothing published).

**Teleport:** when an anchor moves more than `2 × build_m` of the coarsest tier in one frame, queued cells far from every
anchor are dropped and the queue re-sorted at once.

**Waiting:** `ProcGenTiles::all_ready_near(anchor, tier?) -> bool` and the agent method `procgen.tiles_wait` let loading
screens and tests wait until every wanted cell around an anchor is resident. Level load with `Anchored` generators waits for
the cells around the start anchor by default (`wait_on_load: true`), counted in the load report. (As built, ADR-2421: the procgen load gate waits until no cell of the generator is queued or running, and reports it as `anchored`.)

**Baked + anchors:** a `Baked` tiled generator with `runtime: Some(...)` streams its baked cells with the same scheduler
(load and apply only, no evaluation), which is how very large baked worlds stay within memory. (As built, ADR-7287: a
streamed generator that a placed or additive level brought in opens `<guid>.pgout` in the primary bake directory first,
else its own level's file under its source guid (`ProcLevelBakeDirs`, spec 62 § 8), from disk or the pak, and records it
in `ProcBakeOrigin`; `procgen.get` shows it as `bake { dir, kind: cells }`. As built, ADR-7331: a generator inside a
prefab template of a placed level maps back the same way, `procgen.bake` / `regia procgen bake` re-bake such a member's
cells into the file it streams from, and `regia procgen verify` / `procgen.verify` name an entity that has a cell bake but
lost its generator as `anchored_generator_missing`.)

### 4.4 Determinism and multiplayer

A cell's output depends only on (graph hash, parameters, generator seed, tier, cell key, captured inputs over the cell plus
halo). It never depends on anchor paths, frame timing, budgets or which cells were resident before. Every client in a
multiplayer game therefore sees the same cell content for the same inputs; generation is local, nothing is replicated.
Inputs that change at runtime (terrain craters) only affect cells re-run with `refresh_on_change: true`, which clients must
agree on through the game's own replicated events.

## 5. Runtime cost controls

- Per-generator memory cap for resident cells (`max_resident_mb`, desktop 512, Android 128): when exceeded, the farthest
  resident cells of the finest tier are released early (logged once per minute, `procgen_memory_cap`).
- `ProcGenSettings::paused` freezes scheduling (resident cells stay) for cinematics and render-out; render-out
  (ADR-267) waits for `all_ready_near` before each shot.
- `flow.quality` (spec 63 § 10) reads the running `regia_render::quality::QualityTier`; `AnchorSettings::quality_reach`
  (`[f32; 4]`, default `[0.5, 0.75, 1.0, 1.25]`, indexed low/medium/high/ultra) scales every radius. Decision (ADR-1585):
  the seven tiers map `MobileLow`, `MobileHigh`, `Low` → index 0, `Deck`, `Medium` → 1, `High` → 2, `Ultra` → 3.

## 6. Agent protocol surface (page section "Tiles and anchors" in `book/agents/procgen.md`)

| Method | Class | Params | Result |
|---|---|---|---|
| `procgen.tiles` | read | `{ entity, tier?, state? }` | `{ cell_m, tiers, cells: [{ i, j, tier, state, instances, ms, mb }], pool: { free, used }, memory_mb, warnings }` |
| `procgen.anchors` | read | `{}` | `{ primary_view, tagged, extra }` — the anchors by source plus host-supplied extras (`ProcGenExtraAnchors`), each with position, view direction, reach scale |
| `procgen.tiles_wait` | read | `{ entity?, anchor?, tier?, timeout_ms? }` | deferred; `{ ready: true, cells }` or error `procgen_tiles_timeout` |
| `procgen.tiles_set` | edit_scene | `{ entity, tier, build_m?, release_factor?, view_bias? }` | `{ before, after }` (live tuning; not saved unless the editor saves the level) |
| `procgen.tiles_rebuild` | edit_scene | `{ entity, cells?: [[i, j]], tier? }` | re-runs the named cells (or all resident) |

Cell `state` strings: `queued`, `capturing`, `running`, `applying`, `resident`, `releasing`, `failed`. Errors:
`procgen_not_tiled`, `procgen_tiles_timeout`, `procgen_unknown_cell`, `procgen_tier_unknown`. `procgen.tiles_wait` is a deferred method
and uses the same pending list as `play.wait_until` (64 entries, `too_many_pending` when full).
Errors follow spec 62 § 9 (lowercase codes, listed on the page). `procgen.tiles_wait` and `procgen.tiles_rebuild` are
`heavy`; the rest are cheap. Page `book/agents/procgen.md` gets the section "Tiles and anchors"; `book/COVERAGE.md` has one
row per method above.

## 7. Debug views

Editor and `debug_gizmos` overlay: cell grid per tier colored by state; build and release circles around each anchor;
priority numbers on queued cells; a cell with seam-risk points outlined in magenta instead of its state color (any
state; the priority number stays the queued gray). Toggled by `procgen.set_debug { tiles: true }` (defined in
the spec 62 § 9 method table).

## 8. Tests and acceptance

Unit tests (`regia_procgen::tiles`, no GPU):
1. Cell math: cells are world aligned; a point on a cell's max edge belongs to the next cell; the union of all cells'
   owned outputs equals the set of outputs (no duplicates, none missing) for a graph without neighborhood nodes, compared
   with the untiled run (exact equality of sorted output hashes).
2. Halo: with `points.prune` on the test map (a flat 2 km × 2 km terrain, 400 000 candidate points from a fixed seed,
   footprints ≤ 4 m, halo 8 m), the tiled output differs from the
   untiled output in ≤ 0.1 % of instances (recorded count); with halo 0 the difference is larger (proves the halo works).
3. Tiers: with base 64 m, a `whole` tier → 256 m → 32 m graph compiles; a 96 m tier fails with `tier_not_nested`; an edge from the 32 m
   tier into the 256 m tier fails with `tier_upward_edge`; coarse points are clipped per fine cell (counts per cell match
   the coarse points inside the cell plus halo).
4. Stale cells: an edit in one cell's interior re-runs exactly the cells whose halo rectangles contain it (1 cell in the
   middle, up to 4 near a corner).
5. Scheduler (fake clock-free steps): an anchor moving along a straight line at 10 m per step, build 200 m, release factor
   1.25: each cell is requested once and released once; at no step is a cell inside `build_m` missing after its queue time;
   no cell is released while within `build_m × 1.25` (property test over 1 000 seeded paths).
6. Hysteresis: an anchor oscillating ±1 m around the build boundary causes no repeated build/release.
7. Teleport: a jump of 5 km drops queued cells near the old position in the same step.
8. View gate: cells behind the camera are not built when gated; a cell inside the release margin is not released.
9. Determinism: two different anchor paths that end at the same position give the same resident cell output hashes; 1 vs
   4 concurrent runs give the same hashes.
10. Budgets: with `apply_budget_ms` replaced in tests by an element budget (`apply_budget_elements`, the deterministic
    stand-in the scheduler also exposes), a cell of 50 000 instances applies over the expected number of frames and is not
    drawn until complete.
11. Baked tiled: re-baking one cell leaves other cells' bytes in the file unchanged; a baked tiled generator with anchors
    loads only the cells around the anchor (load counter).

Acceptance (sample `samples/procgen_open_world`, 4 km × 4 km procedural terrain from `regia_terrain::generate`, the
spec 65 starter biomes with a 32 m clutter tier): fly the camera through a scripted path (agent `play.step`, fixed steps);
`procgen.tiles_wait` succeeds at each waypoint; `procgen.tiles` never shows more than the memory cap; the hitch report
(`play.hitch_report`) shows no frame over the frame budget attributable to `procgen.*` spans on the desktop reference and
on the Pixel 6 (device run, lead or owner). Screenshots at waypoints for review (no golden; lead TODO).

## 9. Platforms and performance

All targets; Android uses its own defaults (§ 4.3, § 5). Budgets (desktop reference): scheduling update ≤ 0.1 ms per frame
for 4 generators and 2 000 candidate cells; evaluation of a 32 m clutter cell (≈ 3 000 instances) ≤ 8 ms on one worker;
apply ≤ 1 ms per frame (by construction). Android (Pixel 6): clutter cell ≤ 30 ms on one worker, apply ≤ 0.5 ms. `tracing`
spans: `procgen.schedule`, `procgen.cell` (`tier`, `i`, `j`), `procgen.apply`.

## 10. Out of scope

GPU evaluation of fine tiers (the seam is the node trait; ground cover stays in `regia_terrain::grass`); replication of
generated content; predictive prefetch along a planned path beyond the view bias (games can add `ProcGenAnchor` entities
ahead of the player, which covers it); per-cell LOD swapping of generated content (instances use the renderer's LOD and
fade; impostor far fields remain a scatter feature).

## 11. Formats and migration

`TileSettings`, `AnchorSettings`, `TierReach` and `ViewGate` are part of the `ProcGenerator` v1 shape (spec 62). The `.pgout`
cell table of § 2 is part of `.pgout` v3 (ADR-2003 (1); v1 and v2 are migration sources). `ProcGenAnchor` is a new saved component v1 (check-formats name `procgen-anchor`; the lead runs `cargo xtask check-formats
--update` and `cargo xtask manual`). All of these are new at `format_version: 1`, so no migrator is needed; no existing
format changes, so `regia upgrade-project` has nothing to add.

## 12. Provenance

Behavior of partitioned, multi-scale and runtime generation around generation sources in public user documentation of
procedural generation, via the public-sources spec set area `runtime-procedural` (set spec ids
`runtime-procedural-overview`, `-graph-authoring`, `-biomes`; their Sources pages; no setting names, console variables,
defaults or wording carried). Algorithms: uniform grids and hysteresis thresholds for streaming (textbook practice; Ericson
ch. 7 for grids); priority by normalized distance and facing is Regia's own formula. Halo ownership, tier clipping, the
seam-risk count and the deterministic apply budget are Regia design choices.

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
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>
- <<public engine documentation>>

## 13. Owner decisions

None.
