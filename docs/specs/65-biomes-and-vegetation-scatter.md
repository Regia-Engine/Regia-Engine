# Spec 65: Biomes and vegetation scatter

Status: proposed (ADR-1596).
Reviewed: integ/specs (ADR-1585), 2026-10-28. Built baseline: `regia_terrain::{scatter, grass}` and `water_body`; no biome code exists. Needs specs 62, 63 and 64 (shapes, splines, roads) built first. Part of the procedural content graph (spec 62; nodes per spec 63 conventions). Behavior
written from the public-sources spec set, area `runtime-procedural` (ADR-1526, behavior only); the biome model, stage
order, footprint rule, child placement and all names are Regia's own.

## 1. Purpose

A **biome** describes what grows and lies on the ground in a kind of place (a pine forest, a dry riverbed, a meadow): which
trees, bushes, rocks and props, how dense, on what slopes and heights, how they cluster, and which wins where two
things compete for the same spot. Authors paint or draw **biome areas** (volumes, closed splines, a biome map texture);
one generator turns all areas inside its bounds into placed vegetation, deterministic and bakeable, with soft transitions
between neighboring biomes.

Everything here is built from spec 62 graph machinery: biome definitions are data assets, the pipeline is a starter graph
of `biome.*` nodes, and authors who need something unusual edit or extend that graph.

Baseline reused:
- `regia_terrain::scatter` v2 (spec 26 § 1): per-class rules and placement. A biome layer can use a scatter class as its
  candidate source (§ 5.1), and `regia procgen import-scatter` converts a `.scatter.ron` into a biome definition (§ 9).
- `regia_terrain::grass` (ADR-1400): GPU ground cover with a per-class CPU-baked mask. Biomes drive grass by producing a
  density multiplier per grass class (§ 8); grass blades stay on the GPU path.
- Streamed forests on the ECS frame (`book/agents/scatter-forest.md`): instance drawing, wind, alpha-tested shadows. Biome
  output uses the same instanced path (spec 62 § 7.3).
- `regia_terrain::WaterPlane` (the shoreline plane) and `regia_terrain::water_body::WaterBody` (plane or ribbon bodies, ADR-1399) for water-distance filters.

## 2. Regia terms

| Term | Meaning |
|---|---|
| **Biome** (`BiomeDef`) | Asset `<name>.biome.ron`: name, debug color, map key, priority, layers |
| **Layer** (`BiomeLayer`) | One kind of placement inside a biome (canopy trees, understory, rocks, props): a candidate source, channels, entries, a rank |
| **Channel** | A named sub-stream of a layer's candidates (for example `steep`, `flat`, `near_water`) that entries subscribe to |
| **Entry** (`BiomeEntry`) | One thing that can be placed: a mesh, prefab or nothing (a spacer), with weight, filters, footprint, placement rules and children |
| **Footprint** | Two radii per entry: `core_m` (trunk or base; what must not overlap other layers) and `spread_m` (canopy; spacing among the same entry) |
| **Biome area** (`BiomeArea`) | A level entity whose shape, closed spline or biome-map texture says where a biome applies |
| **Claim** | For a position, the biome that owns it and its weight (1 inside, falling to 0 across the blend band) |

## 3. Biome definition (`<name>.biome.ron`, format name `biome`, `format_version: 1`)

```ron
BiomeDef(
    format_version: 1,
    name: "Pine forest",
    color: (0.15, 0.45, 0.2),           // debug display; also the default map key
    map_key: None,                      // Some((r, g, b)) 8-bit key in biome map textures; default from `color`
    priority: 20,                       // higher wins where areas overlap
    layers: [
        (
            name: "canopy",
            rank: 30,                   // higher-rank layers keep their spots in the cross-layer resolve (§ 6.4)
            sharing: Exclusive,         // Exclusive | Shared (Shared: neither removes nor is removed in § 6.4)
            source: Ground(spacing_m: 6.0, jitter: 0.9, noise: Some((scale_m: 60.0, octaves: 3, min: 0.2, max: 1.0))),
            channels: [ (name: "steep", rule: Slope(min_deg: 25.0, max_deg: 90.0)), (name: "flat", rule: Rest) ],
            entries: [
                (
                    name: "pine_tall",
                    spawn: Mesh("trees/pine_a.glb"),
                    weight: 3.0, channels: ["flat"], enabled: true,
                    filters: [ Altitude(min: 0.0, max: 1400.0, feather: 50.0), Slope(min_deg: 0.0, max_deg: 30.0, feather: 4.0) ],
                    footprint: (core_m: Some(0.6), spread_m: Some(3.5)),
                    placement: (upright: 1.0, scale: Range(0.8, 1.25), yaw: Random, sink_m: 0.15, offset: None),
                    render: (cast_shadows: true, draw_distance_m: 1500.0, fade_m: 100.0, collision: Simple),
                    children: [ (entry: "fern", count: (2, 5), radius_m: (1.0, 3.0), rate: 0.6, keep_clear_of_parent: Core) ],
                ),
                ( name: "fern", spawn: Mesh("plants/fern.glb"), weight: 0.0, channels: [],   // child-only (weight 0)
                  footprint: (core_m: Some(0.2), spread_m: Some(0.5)), placement: (upright: 0.3, scale: Range(0.7, 1.1), yaw: Random) ),
            ],
            max_child_depth: 2,
        ),
    ],
    grass: [ (class: "forest_floor", density: 0.6, outside: Keep) ],   // optional grass multipliers (§ 8)
)
```

Rules:
- Names are unique within their scope (layer names in a biome, entry names in a layer); an entry is referenced by
  `biome/layer/entry` elsewhere (debug output, statistics, agent).
- An entry with `weight: 0` and no channels is **child-only**: it never takes root candidates but can be a child.
- `spawn: None` is a **spacer**: it claims space (footprint, children) but places nothing; used to keep clearings.
- `enabled: false` keeps the entry in the file but it never places and is skipped as a child.
- Every field has a documented default (shown in the format doc `docs/formats/biome-ron.md`, written by the builder).
- Frozen-format rule (`specs/12` § 4a): register `biome` in `check-formats`; shape changes bump `format_version` with a
  `VersionStep` migrator and `regia upgrade-project` support.

### 3.1 Candidate sources (`source`)

| Source | Meaning |
|---|---|
| `Ground(spacing_m, jitter, noise)` | `sample.ground` over the claimed region (spec 63 § 4), density × optional noise |
| `Poisson(min_dist_m)` | `sample.poisson` over the region |
| `ScatterClass(set, class)` | The candidates a `.scatter.ron` class would place in the region (its density map, noise and filters), from `regia_terrain::scatter::placement` without its mesh pick; mesh weights become the layer's entries when imported (§ 9) |
| `Graph(graph, params)` | Any graph called with the region as input; must output points |

### 3.2 Channels

Channel rules split candidates in order; a candidate goes to the first channel whose rule matches: `Slope(min_deg,
max_deg)`, `Altitude(min, max)`, `TerrainLayer(name, min_weight)`, `Mask(asset, channel, min)`, `Attr(sel, min, max)`,
`Random(fraction)` (a deterministic fraction by draw), `Rest` (everything left). A layer without channels has one implicit
channel `all`. Entries list the channels they take (`[]` with weight > 0 means all channels).

### 3.3 Entry filters

Applied in list order after assignment (§ 6.2). Each filter has a window and an optional `feather` (in the filter's unit):
inside the window density is unchanged; within `feather` outside the window density falls linearly to 0; beyond, the point
is removed.

`Altitude(min, max)` meters; `Slope(min_deg, max_deg)` from the ground normal; `Density(min, max)` the candidate's density;
`WaterDistance(min_m, max_m)` horizontal distance to the nearest terrain cell below the water level (negative inside water;
from a Euclidean distance transform of the underwater mask at heightfield resolution, computed once per run; a texel is underwater when it lies below the level of a `WaterPlane` or a `WaterBody` plane covering it, or inside a ribbon body's outline);
`TerrainLayer(name, min, max)` the painted or procedural layer weight; `Mask(asset, channel, min, max)`; `Attr(sel, min,
max)`; `Graph(graph, attr, min, max)` (calls a graph on the points that writes `attr`, then tests it). An entry may set
`filters_off: true` to skip all of its filters (fast iteration on placement only).

### 3.4 Placement

`upright: 0..1` blends the rotation from ground-normal alignment (0) to world up (1); `yaw: Random | Fixed(deg) |
AlongSlope` (faces downhill); `scale: Keep | Range(lo, hi) | Fixed(s)` (`Range` draws uniformly per point; it overrides any
scale from the source unless `scale_from_source: true`); `sink_m` lowers the instance along its up axis (hides root
seams); `offset: Some((t, r_deg, s))` a local pivot fix applied before the point transform.

### 3.5 Footprint

`core_m` and `spread_m` default from the spawned mesh's local bounds: `spread_m` = half the larger horizontal extent,
`core_m` = 0.25 × `spread_m`. Both scale with the instance scale. Uses:
- **Self spacing:** within one entry, no two instances closer than the sum of their `spread_m` (prune, larger first, ties
  by seed, spec 63 `points.prune` with circular footprints).
- **Cross-layer resolve** (§ 6.4): compares `core_m` circles only, so understory can stand under a canopy but not inside
  a trunk.
- **Children:** `keep_clear_of_parent: Core | Spread | None` removes children inside the parent's core (default),
  inside its spread, or keeps them all.

### 3.6 Children

`children: [ (entry, count: (lo, hi), radius_m: (lo, hi), rate: 0..1, keep_clear_of_parent, scale_with_parent: false,
graph: None) ]`.
- `rate` is the fraction of parents (chosen by draw) that get children at all.
- Default placement: `count` drawn uniformly in `[lo, hi]`; child `k` of `n` sits at angle `θ0 + k · 137.508°` (golden angle,
  θ0 drawn per parent) and radius `lo_r + (hi_r − lo_r) · sqrt((k + 0.5) / n)`, projected onto the ground. This spreads
  children evenly without a grid look.
- `graph: Some(path)` replaces the default with a graph called per parent (input: the parent point; output: child points
  in the parent's frame).
- Children then go through their own entry's placement, filters and footprint, and may have children themselves.
  Recursion stops at the layer's `max_child_depth` (0 = no children); an entry that lists itself as a child is fine because
  the depth cap always ends it. Children per run are limited by the run's point cap (spec 62 § 5.2), checked per parent
  before its children are allocated (the layer's points so far plus the children drawn so far at this depth, before
  placement, filters and spacing thin them). `count.1` above the desktop per-item point cap (4 194 304,
  `MAX_CHILDREN_PER_PARENT`) is `biome_invalid` at load (v0.5.1).

## 4. Biome areas (`BiomeArea` component, `format_version: 1`, check-formats `biome-area`)

```rust
pub struct BiomeArea {
    pub biome: Option<AssetRef<BiomeDef>>, // .biome.ron; None for Map areas, which use map_biomes
                                      // (as built: a project-relative path String, ADR-5645 (5))
    pub shape: AreaShape,             // FromShape (entity's Shape) | FromSpline (closed Spline) | Map { texture, rect }
    pub blend_m: f32,                 // width of the soft edge inside the area, default 8
    pub blend_noise: Option<(f32 /*scale_m*/, f32 /*amplitude 0..1*/)>,
    pub priority_offset: i32,         // added to the biome priority for this area only (default 0)
    pub preview_alone: bool,          // editor: regenerate this area by itself (§ 7)
    pub map_biomes: Vec<AssetRef<BiomeDef>>,    // Map areas: candidate biomes, matched by `map_key` (paths, ADR-5645 (5))
    pub format_version: u32,
}
```

- An area's **claim value** at `p` is 1 deeper than `blend_m` inside its shape, falling linearly to 0 at the boundary; with
  `blend_noise`, the distance is perturbed by value noise (amplitude × `blend_m`) so edges wander. Map areas: the texel's
  key selects the biome (exact 8-bit match; unknown keys = no biome, counted in the statistics); claims blur over
  `blend_m` by box-filtering each biome's key mask.
- A closed spline is required for `FromSpline` (an open spline is an error `biome_area_open`).
- Overlap: at each position the area with the highest effective priority (biome priority + offset) **owns** the spot with
  its claim value `w`; the next-highest area (if any) gets `1 − w` there; lower ones get nothing. Ties in priority break by
  `EntityGuid` (lower wins), so results never depend on spawn order.
- Areas outside the biome generator's bounds are ignored; an area partly outside is clipped.

## 5. Nodes (spec 63 conventions)

| Id | Pins | Behavior |
|---|---|---|
| `biome.areas` | bounds (any spatial; usually `in.self`), out | Captures every `BiomeArea` whose shape intersects the `bounds` items' AABB (or the generator bounds when `bounds` is unconnected) (snapshot, spec 62 § 5.3) and emits one item per (area, biome) with the claimed region as a `Mask`-like claim field and item scope `biome`, `area`, `priority` |
| `biome.layer` | claims, ground, out | For each claim item and each layer of its biome: generates candidates (§ 3.1) inside the claim, multiplies density by the claim value, splits channels, assigns entries, applies placement, filters, self spacing and children (§ 6.1-6.3). Output: one point item per (area, layer), points with attributes `entry` (Name `biome/layer/entry`), `layer_rank`, `sharing`, `core_m`, `spread_m`, `depth` (0 = root) |
| `biome.resolve` | in*, out | Cross-layer resolve of all layers' points (§ 6.4) |
| `biome.exclude` | in, out | Removes points inside exclusion regions: entities tagged `procgen_exclude` with a `Shape` or a closed `Spline`, open splines tagged `procgen_exclude` (removed within the spline's width), road exclusions (spec 64 § 7.2) and any extra region pins |
| `biome.spawn` | in, out | Spawns each point's entry (mesh instances or prefabs) with the entry's render settings; identical to `spawn.mesh`/`spawn.prefab` keyed by `entry` |
| `biome.grass_mask` | claims, out (one `Mask` item per grass class, tagged with the class name) | Produces, per grass class named in the biomes' optional `grass: [(class, density)]` lists, a multiplier image over the grass mask's grid: the claim-weighted density of each biome for that class (§ 8) |
| `biome.stats` | in, out | Passes through; writes the per-entry and per-filter counts into the run report |

The shipped starter graph `starter/biomes.pgraph.ron`:
`in.self → biome.areas → biome.layer (+ in.ground) → biome.resolve → biome.exclude → biome.spawn`, with
`biome.areas → biome.grass_mask → out.mask("grass_multiplier")` (one exported mask per class, named
`grass_multiplier/<class>`).

## 6. Pipeline semantics

### 6.1 Root candidates

Per claim item and layer: candidates from the source within the claim's bounds; density × claim value; points with
density 0 dropped. The claim value is read from the claim raster `biome.areas` emits (ADR-5645 (6)), so it is exact at the
raster's resolution (`cell_m`, default 1 m). Candidates on the blend band of the owning area therefore thin out while the second area's candidates
(density × `1 − w`) thin in.

### 6.2 Assignment and filters

1. Channel split (§ 3.2).
2. Entry pick per candidate among enabled entries that take its channel: roulette by weight with the candidate's draw
   (slot 0). Candidates whose channel has no entry are dropped (counted as `unassigned`).
3. Placement (§ 3.4).
4. Filters (§ 3.3) in order; each records how many points it removed.
5. `density.realize random` (spec 63 § 5): the surviving density becomes existence.

### 6.3 Self spacing and children

6. Self spacing per entry (§ 3.5).
7. Children (§ 3.6), depth by depth: all depth-1 children are created, then placed, filtered and spaced, then depth 2, and
   so on. Children of an entry are spaced against each other and against existing points of their own entry.

### 6.4 Cross-layer resolve

All points from all layers of all biomes in the run are sorted by `(owner priority desc, layer_rank desc, depth asc,
entry index asc, seed asc, point index asc)`. Points are accepted in that order; a point is removed when its `core_m`
circle overlaps an accepted point's `core_m` circle **and** the accepted point's group (biome, layer) ranks strictly higher
(by owner priority, then `layer_rank`) **and** neither layer is `Shared`. Points from groups of equal rank never remove each
other. A spatial hash over accepted cores (cell = 2 × the largest `core_m`) keeps this linear. The result is order
independent within equal keys because the sort key is total.

### 6.5 Exclusions and spawn

8. `biome.exclude`; 9. `biome.spawn`. Instances are grouped per entry render settings into instance sets.

## 7. Editing loop

- Moving, reshaping or repainting an area marks the biome generator stale (spec 62 § 5.3). With tiling (spec 66), only the
  cells the change touches re-run.
- `preview_alone: true` on an area makes the editor regenerate that area by itself into a temporary preview output (no
  cross-area resolve with neighbors, no bake write) while the author tunes it; turning it off runs the full generator.
- Changing a `.biome.ron` hot-reloads and re-runs every generator whose areas use it.
- Debug overlay (spec 67 § 3): claims tinted by biome color, blend bands, entry footprints (core and spread circles),
  points removed by each filter (shown in a dim color with the filter name on hover).
  As built (ADR-7238): `regia_editor::procgen::biome_overlay`; the outlines are `biome_overlay`, the other views
  `biome_tint`, `biome_bands`, `biome_footprints` and `biome_filtered` on `editor.procgen_view_set`; the filter
  names come from `biome.layer`'s second pin `filtered` (preview runs only) and the hover is
  `editor.procgen_biome_overlay { at }`; the cursor over a filtered point now shows the same
  name as a viewport tooltip (`regia_editor::procgen::view`, view-only). Tint and bands drape on the terrain under each cell
  since ADR-7275 (center and corners at the ground height; the area's height where no terrain lies under a cell).
  As built (ADR-7743): the tooltip gathers the filtered marks once per preview run and looks the cursor ray up once
  per hover change (`regia_editor::procgen::view::BiomeTooltipCache`), not every frame.
- As built (ADR-7743): **Biome editor.** `regia_editor::biome_panel` (`Panel::Biome`) edits a `.biome.ron` as typed
  forms over the definition's serde shape, by the paths `validate` reports (`layers/<layer>/entries/<entry>/weight`);
  an edit the types refuse or that adds a rule issue changes nothing, renames cascade into children and entry channels,
  128 undo steps (a drag is one), and Save writes `BiomeDef::to_ron` (RON comments are lost; the panel warns when the
  file opens). Content ▸ New ▸ Biome creates one; the procgen Area tool picks its biome from the project's files.
  Agent: `editor.biome_panel_open`, `_get`, `_edit`, `_save` (`book/agents/biome-editor.md`).

## 8. Grass from biomes

`regia_terrain::grass::mask` gains optional per-class multipliers (an R8 image per class on the mask's grid, multiplied into
the density channel before quantization): a new field `multipliers: Option<&'a GrassMultipliers>` on `GrassMaskInputs`
(`crates/regia_terrain/src/grass/mask.rs`), where `GrassMultipliers { by_class: BTreeMap<String, MultiplierImage> }` and
`MultiplierImage { side: u32, texels: Vec<u8> }` are new public types in `regia_terrain::grass`. `GrassMaskInputs::new` sets
it to `None`, so calls through `new` or `..GrassMaskInputs::new(..)` keep their behavior; a struct literal without `..` stops
compiling (list it under Upgrade in the release fragment). `GrassMultipliers` is also a Regia resource (an ECS `Resource`) defined in
`regia_terrain` (layer 2). No grass file format changes.

`biome.grass_mask` produces the multipliers from biome `grass: [(class: "meadow", density: 0.8, outside: Keep)]` lists
(a field of `.biome.ron` v1): the multiplier at a texel is the claim-weighted sum of each owning biome's density for that
class. Outside every biome area the value is 1 (the class's own mask is kept) when every biome that names the class says
`outside: Keep` (the default), and 0 when any says `outside: Clear`. Classes no biome names get no multiplier at all.

Handoff (layering: `regia_terrain` is layer 2 and cannot read procgen output): the biome generator exports each class's
multiplier as a mask `grass_multiplier/<class>` (spec 62 § 7.3). A system in `regia_procgen` (`ProcGenSet::Apply`, after a run
finishes or is cleared) gathers, for each grass class, the mask with that name among the level's generators (first by
`dependency_priority`, then guid), resamples it nearest-texel onto the class's mask grid and writes it into the
`GrassMultipliers` resource (removing the class's entry on clear). The terrain's grass setup treats a changed resource as a
re-bake trigger for the affected classes and passes it as `multipliers`. The bake stays a pure function of its inputs.

**As built (ADR-7214):** the host opts in with the `regia_procgen::biome::ecs::GrassRebake` resource (the `GrassSet` its
layer draws, the layer's `TerrainGrassHandle`, the texel size). `grass_rebake` (after `biome_grass_handoff`) re-bakes
every class of the set from the primary terrain (map, layers, holes) with the current multipliers when the resource
changes or is removed, and queues the mask with `TerrainGrassHandle::replace_mask`; the GPU layer takes it on its next
frame when its side and class count match (`masks_replaced` / `masks_refused`). Classes naming a density map re-bake with
the maps the host supplies in `GrassRebake::density_maps` (ADR-7262); a map missing there fails the bake
(`GrassRebake::last_error`) and the old mask stays.

## 9. Import from scatter sets

`regia procgen import-scatter <file.scatter.ron> --out <dir>` writes one `.biome.ron` per scatter set: one layer per class
(`source: ScatterClass(set, class)`), one entry per weighted mesh (weights carried over), render settings from the class
(cast shadows, distances, fade), footprint from the class's size intent if present. A sample check: the imported biome over
the same map places points at exactly the scatter positions (same candidate source) with the same mesh choice frequencies
within ±1 % (different random streams for the pick, so not the same per-instance meshes).

## 10. Agent protocol surface (`book/agents/biomes.md`)

| Method | Class | Params | Result |
|---|---|---|---|
| `biome.areas` | read | `{ generator? }` | areas: entity, biome, shape kind, priority, preview flag, point counts per layer |
| `biome.claim` | read | `{ point: [x, y, z], generator? }` | `{ owner: { biome, area, weight }, second: { ... } \| null }` |
| `biome.stats` | read | `{ generator }` | per `biome/layer/entry`: candidates, assigned, removed per filter, spaced out, children, resolved out, excluded, placed |
| `biome.def_get` | read | `{ path }` | the parsed definition with defaults filled in |
| `biome.def_edit` | assets | `{ path, ops: [set {pointer, value} \| add_entry {layer, entry} \| remove_entry {layer, name}], save?: bool }` | `{ errors }`; `pointer` is a JSON pointer into the definition |
| `biome.preview` | edit_scene | `{ area, on: bool }` | `{ preview_alone }` |

Errors (answered as `F0002`, or `F0003` for an unknown path or entity, with `data.reason` = the code; lowercase, listed on the page;
the page and a `book/COVERAGE.md` row cover every `biome.*` method; node ids in § 5 are graph nodes, not agent methods, even
where a name such as `biome.areas` is shared): `biome_unknown`, `biome_invalid` (with the list), `biome_area_open`, `biome_entry_unknown`, `biome_not_generator`,
`biome_pointer_invalid`.

## 11. Determinism

Every draw follows spec 62 § 6 (per-point seeds). `biome.layer` runs several stages inside one node, so each stage uses its
own stream salt `mix(node_seed, stage)` (spec 63 § 1): stage 1 candidate source (its own node rules: jitter slots 0-1,
noise), stage 2 channel split (`Random` rule, slot 0), stage 3 entry pick (slot 0), stage 4 placement (slots 0-2 yaw and
tilt, 3 scale), stage 5 filters (none random), stage 6 realize (slot 0), stage 7 children (slot 0 rate, 1 count, 2 angle,
3 radius jitter); child points then get their own position seeds. Sorting keys are total (§ 6.4). The water-distance transform and claim blur use
integer grids and fixed operation order.

## 12. Tests and acceptance

Unit and node tests (no GPU), synthetic 512 m × 512 m terrain with a hill, a valley with water, two painted layers:
1. Format round trip and defaults; a child entry name that does not exist fails with `biome_entry_unknown`.
2. Claims: two overlapping box areas, priorities 10 and 20: inside the overlap, the 20 area owns with weight 1 deeper than
   `blend_m`; across its band the weights of owner and second sum to 1; equal priorities pick the lower guid every run.
3. Weights: two entries 3:1 in one channel over 20 000 candidates place 15 000 ± 300 and 5 000 ± 300 (recorded).
4. Filters: altitude window with `feather` 50 m keeps everything inside, nothing 50 m beyond, and linearly thinned between
   (count in 10 m bins matches the ramp within 3 %); water distance negative inside the lake.
5. Self spacing: no two instances of one entry closer than the sum of their spreads.
6. Children: depth cap 0 gives no children; a self-referencing child entry with depth cap 3 terminates with exactly 3
   generations; `rate: 0.5` gives children to 50 % ± 2 % of parents; `keep_clear_of_parent: Core` leaves no child inside a
   core.
7. Resolve: a rock layer (rank 40) removes overlapping bush cores (rank 10) but not vice versa; a `Shared` layer keeps
   both; equal ranks keep both.
8. Exclusion: a tagged box removes every point inside it.
9. Determinism: the full starter graph on the test map gives a recorded output hash with 1 and 8 threads, on Linux,
   Windows and Android aarch64.
10. Import: the scatter import check of § 9.
11. Grass: the multiplier image for a meadow biome over half the map is 0.8 inside the area (beyond the blend band) and
    outside the areas the multiplier is 1 with `outside: Keep` and 0 with `outside: Clear`; the grass bake with the multiplier
    equals the bake without it where the multiplier is 1 (byte-identical rows).

Acceptance (sample `samples/procgen_biomes`): three biomes (pine forest, meadow, riverbed) on a terrain with a river
spline and a road (spec 64) whose exclusion clears the road. Through the agent: `procgen.run` → `ready`; `biome.stats`
shows non-zero placed counts per entry and the removal counts per filter; `biome.claim` at three known points names the
expected biomes; screenshots for the owner's look review (no golden; lead TODO).

## 13. Platforms and performance

Pure CPU; all targets. Budgets (desktop reference, 8 workers): a 2 km × 2 km map with 3 biomes, 6 layers, 20 entries,
≈ 600 000 placed instances ≤ 6 s for a full bake; incremental re-run of one 256 m cell after an area edit ≤ 300 ms (spec
66 tiling). Android `OnLoad` use is discouraged above 100 000 instances (bake instead); the guide page says so.

As built (ADR-7238): the run's output and world hashes are fed through a buffer; measured before/after in
`docs/perf-log.md` (round "post-v0.5 spline and biome perf").

As built (ADR-7252): `biome.layer` runs its (claim, layer) jobs on the procgen pool (layers that run a subgraph stay
serial), ground-grid candidates and placement in element chunks and self spacing per entry; `biome.areas` and
`biome.grass_mask` fill texel chunks on the pool. Output bytes are the same on every schedule. Measured in
`docs/perf-log.md` (round "post-v0.5 biome bake on the pool").

## 14. Out of scope

Growing plant meshes procedurally (trees, bushes) from growth rules: spec 68; runtime-only GPU detail tiers are spec 66; ecological simulation (seed spreading over
time, competition); seasonal changes (a material concern).

## 15. Provenance

Behavior of data-driven biome layering in public user documentation of procedural generation, via the public-sources
spec set area `runtime-procedural` (set spec id `runtime-procedural-biomes`, with `-overview`; their Sources pages; no
field names, asset paths, stage names or wording carried). Algorithms: golden-angle spiral placement (Vogel, "A better
way to construct the sunflower head", Mathematical Biosciences 1979); Euclidean distance transform (Felzenszwalb and
Huttenlocher, "Distance Transforms of Sampled Functions", Theory of Computing 2012); roulette selection and spatial
hashing (textbook; Ericson, *Real-Time Collision Detection* ch. 7); value-noise fBm (existing rows). The two-radius
footprint, claim blending, channels, stage order and resolve key are Regia's own.

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

## 16. Ecology (ADR-2456)

A biome may have an **ecology profile** `<name>.ecology.ron` beside it (format `ecology` v1,
`docs/formats/ecology-ron.md`): the site it stands for (soil moisture 1-10, soil reaction 1-9, altitude, slope, a soil
label), which entry is which species in which role (dominant, associated, understory) with its share of the layer, the
competition rules of `biome.ecology`, and project species. `regia_procgen::biome::ecology::validate_ecology` checks a
biome's species against the site and each other with the built-in species table (indicator-value scales, shade
tolerance, altitude, slope, stratum, succession; provenance § 15 and `docs/provenance.md`); entries without a profile
row are matched by name tokens, and without a profile the site is inferred from the dominant. Issues carry `ecology_*`
codes and a reason (`book/agents/biomes.md` "Ecology"); `biome.validate_ecology` and `cargo xtask check-ecology` run it.

| Node | Pins | What it does |
|---|---|---|
| `biome.ecology` | in (multi), out | Competition-aware thinning per profile: canopy self-thinning by summed stand density `Σ scale^1.605` per world-anchored cell (smallest stems first); shade-needing understory (light ≤ 3) only under a remaining crown; light-demanding understory (light ≥ 7) not under a crown casting deep shade (≥ 4); spring plants exempt. Declares `reach_m` (default 50) |

`starter/biomes_ecology.pgraph.ron` is the starter graph with `biome.ecology` between `biome.resolve` and
`biome.exclude`; the starter graph itself is unchanged. Six presets ship in `starter/biomes/` with profiles: birch
woodland (wet acid), oak-birch woodland (acid), beech forest (base-rich), oak-hornbeam forest, boreal spruce forest,
boreal pine forest.
