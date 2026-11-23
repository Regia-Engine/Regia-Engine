# Biomes: areas, claims, placement statistics, definitions

A **biome** (`<name>.biome.ron`) describes what grows and lies on the ground in a kind of place: layers
(canopy, understory, rocks, props) of **entries** — a mesh, a prefab, or a spacer that only claims
space — with weights, filters (altitude, slope, density, distance to water, masks, attributes), a
two-radius **footprint** (`core_m` for the trunk, `spread_m` for the canopy), placement rules and
children. **Biome areas** — the `regia_procgen::biome::ecs::BiomeArea` component on a level entity —
say where a biome applies: the entity's `Shape` when it has one (a box or sphere in world space; a
capsule or hull as its bounding box), otherwise the transform as a unit box (half extents `0.5 × scale`),
a closed `Spline` on the entity (`FromSpline`: its outline in the XZ plane), or a biome map texture whose
8-bit keys select biomes. A generator running the starter graph
`starter/biomes.pgraph.ron` (`regia_procgen::biome::STARTER_BIOMES_GRAPH`) turns every area inside its
bounds into placed instances (specs/65). The format is documented in `docs/formats/biome-ron.md`.

The methods need `ProcGenPlugin`; without it they answer `F0003` with `data.reason: "biome_unknown"`.

## Where does each biome apply?

```json
biome.areas { "generator": 4294967301 }
→ { "areas": [{ "entity": 4294967310, "guid": "…", "name": "north forest", "biome": "biomes/pine.biome.ron",
      "shape": "shape", "blend_m": 8.0, "priority_offset": 0, "preview_alone": false,
      "biomes": [{ "path": "biomes/pine.biome.ron", "name": "Pine forest", "priority": 20,
                   "layers": { "canopy": { "candidates": 712, "unassigned": 41, "placed": 236 } } }],
      "error": null }],
    "unknown_map_texels": 0 }

biome.claim { "point": [120.0, 0.0, 64.0] }
→ { "owner": { "biome": "Pine forest", "path": "biomes/pine.biome.ron", "area": 4294967310,
               "area_guid": "…", "priority": 20, "weight": 0.62 },
    "second": { "biome": "Meadow", "path": "biomes/meadow.biome.ron", "area": 4294967311,
                "area_guid": "…", "priority": 10, "weight": 0.38 } }
```

Without `generator`, `biome.areas` lists every area in the level and `biome.claim` uses all of them.
An area's **claim** is 1 deeper than `blend_m` inside its outline and falls to 0 at the edge. Where
areas overlap, the one with the highest priority (biome priority + the area's `priority_offset`)
**owns** the spot with its weight `w`; the next one gets `1 − w`, so the owner thins out across the
band while the second thins in. Equal priorities break by guid (lower wins). An area that failed to
load shows `error: { reason, message }` (`biome_unknown`, `biome_invalid`, `biome_entry_unknown`,
`biome_area_open` for a `FromSpline` area whose spline is open, has fewer than 3 points, or is missing).

## Why did (or didn't) something get placed?

Run the generator (`procgen.run { entity, wait: true }`), then:

```json
biome.stats { "generator": 4294967301 }
→ { "ran": true,
    "entries": { "Pine forest/canopy/pine_tall": { "candidates": 712, "assigned": 671,
        "removed_by_filter": { "0:Altitude": 0, "1:Slope": 58 }, "realized_out": 230,
        "spaced_out": 147, "children": 0, "resolved_out": 0, "excluded": 12, "placed": 224 } },
    "layers": { "Pine forest/canopy": { "candidates": 712, "unassigned": 41 } } }
```

The pipeline, in order: candidates from the layer's source × the claim weight → channel split →
entry pick by weight (`unassigned` when no entry takes the channel) → placement → each filter
(`removed_by_filter` counts points a filter's window and feather drop entirely; the feather band
only thins) → realize (`realized_out`) → self spacing (`spaced_out`: no two instances of an entry
closer than the sum of their spreads; children inside their parent's core or spread are counted here
too) → children (`children` placed) → cross-layer resolve (`resolved_out`: a core overlapping a core of a
strictly higher-ranked layer; `Shared` layers neither remove nor are removed) → exclusions
(`excluded`: entities tagged `procgen_exclude`, `regions` items) → spawn (`placed`; spacers are not
counted). `procgen.inspect { entity, node: 4 }` (with `keep_debug`) shows the points with their `entry`,
`layer_rank`, `sharing`, `core_m`, `spread_m` and `depth` attributes.

Child counts are bounded (v0.5.1): a child rule's `count.1` above the desktop per-item point budget
(4 194 304) makes the definition `biome_invalid` (the issue's path ends in `children/<k>/count`), and a
run checks each parent's drawn child count, together with the points the layer already holds, against
the run's `points_per_item` before it allocates them: past it the run fails with `limit_exceeded`
naming `points_per_item`. Children are checked before realize and spacing thin them, as a layer's
source candidates are.

## Read and edit a definition

```json
biome.def_get { "path": "biomes/meadow.biome.ron" }
→ { "path": "biomes/meadow.biome.ron", "def": { "format_version": 1, "name": "Meadow", "priority": 10,
      "layers": [{ "name": "flowers", "rank": 0, "sharing": "Exclusive",
                   "source": { "Ground": { "spacing_m": 2.0, "jitter": 0.5, "noise": null } }, … }] } }

biome.def_edit { "path": "biomes/meadow.biome.ron", "ops": [
    { "op": "set", "pointer": "/layers/0/entries/0/weight", "value": 2.0 },
    { "op": "add_entry", "layer": "flowers", "entry": { "name": "stone", "spawn": { "Mesh": "props/stone.glb" } } },
    { "op": "remove_entry", "layer": "flowers", "name": "daisy" } ], "save": true }
→ { "errors": [], "applied": true, "saved": true, "stale_generators": 1 }
```

`def_get` fills every default. `def_edit` applies its ops to that JSON (enum values as
`{"Variant": …}` or `"Variant"`), then checks the rules: a result that breaks one is answered with
`errors: [{ reason, path, message }]` and `applied: false`. Otherwise the edit applies at once as an
unsaved overlay and `save: true` writes the file; every generator whose last run read the file is marked
stale (`stale_generators`), so live editor generators re-run. Moving an area or editing its `BiomeArea`
component marks the generators that read it stale too. At most 256 ops per call.

`biome.preview { "area": 4294967310, "on": true }` → `{ "preview_alone": true }` sets the area's
`preview_alone` flag: in the editor, a generator whose bounds hold a previewing area regenerates only
the previewing areas (no resolve with neighbors) while you tune it.

## Import a scatter set

`regia procgen import-scatter maps/valley.scatter.ron --out biomes/` writes `biomes/valley.biome.ron`: one layer per scatter
class whose source is `ScatterClass(set, class)` — the exact positions the scatter set places — and one entry per mesh with
its weight. Footprints start at zero (scatter has no spacing rule); raise them to space the entries out.

## Grass

A biome's optional `grass: [(class, density, outside)]` list scales the terrain's grass classes. The
starter graph's `biome.grass_mask` node exports one mask per class (`grass_multiplier/<class>`), and a
`regia_procgen` system copies them into the `regia_terrain::grass::GrassMultipliers` resource, which the
grass bake multiplies into each class's density (`GrassMaskInputs::multipliers`). Outside every area
the multiplier is 1 (`outside: Keep`, the default) or 0 when any biome naming the class says `Clear`.

## Ecology: could these species grow together?

A biome names what to place; an **ecology profile** (`<name>.ecology.ron` beside `<name>.biome.ron`, format
`docs/formats/ecology-ron.md`) says what site it stands for and which entry is which species. `biome.validate_ecology`
checks the mix (ADR-2456):

```json
// the birch woodland preset with an `oak` entry added to its canopy
biome.validate_ecology { "path": "starter/biomes/birch_woodland.biome.ron" }
→ { "plausible": false, "results": [{ "path": "starter/biomes/birch_woodland.biome.ron",
      "ecology": "starter/biomes/birch_woodland.ecology.ron", "biome": "Birch woodland", "plausible": false,
      "site": { "moisture": [7.5, 9.5], "reaction": [1.5, 4.0], "altitude_m": [0.0, 1200.0], "slope_deg": [0.0, 12.0],
                "soil": "peaty gley at a bog margin" },
      "site_inferred": false, "floor_light": 8.25,
      "species": [{ "entry": "canopy/downy_birch", "species": "betula_pubescens", "role": "dominant", "share": 0.75,
                    "weight_share": 0.75, "source": "profile" }, …],
      "unmapped": [],
      "issues": [{ "reason": "ecology_moisture", "severity": "error", "path": "canopy/oak", "species": "quercus_robur",
                   "message": "pedunculate oak needs soil moisture F 3-7 (dry to moist); the site is F 7.5-9.5 (wet to waterlogged, peaty gley at a bog margin)" }] }] }
```

Without `path` it checks every biome the level's areas name; `presets: true` adds the shipped presets; `ecology` picks
another profile. Paths under `starter/biomes/` resolve to the shipped presets when the project has no such file.

Each enabled entry gets a species: the profile's `species` list first, then its name or mesh path matched against the
species table by whole tokens (`trees/silver_birch_a.glb` → silver birch, `oak01` → pedunculate oak; `limestone` is not
`lime`). Entries with no species (rocks, logs, spacers, unknown plants) are listed in `unmapped` and never flagged.
Without a profile the site is **inferred** from the dominant's indicator class: the tree the biome is named after
(`Birch forest` → birch), else the canopy entry with the largest weight. So a "Birch forest" holding birch, oak and
maple flags the maple (it needs base-rich soil; birch indicates acid soil) but not the oak, because acid oak-birch woods
are a real community. Give the biome a profile to state the soil instead.

| Reason | What is implausible |
|---|---|
| `ecology_moisture`, `ecology_reaction` | The species' soil moisture (1 very dry … 10 waterlogged) or reaction (1 strongly acid … 9 calcareous) window misses the site's |
| `ecology_altitude`, `ecology_slope` | The entry's own `Altitude`/`Slope` filter, or the site, lies outside the species' altitude band or past the steepest ground it holds |
| `ecology_pair_exclusive` | Two species whose moisture or reaction windows never overlap (peat moss and reindeer lichen) |
| `ecology_understory_light` | An understory plant needs more light than the canopy leaves (heather under beech); spring plants that grow before leaf-out are exempt |
| `ecology_competition` | A shade-intolerant pioneer shares dominance with a deep-shade tree (silver birch with beech) |
| `ecology_name_mismatch` | The biome is named after a tree that does not dominate it, or does not grow in it |
| `ecology_share_sum`, `ecology_share_mismatch`, `ecology_dominance_order`, `ecology_dominant_missing` | Profile proportions: a layer's shares do not sum to 1, a share disagrees with the entry weights by more than 0.05, an associated species outnumbers a dominant, canopy trees with no dominant |
| `ecology_species_unknown`, `ecology_entry_unknown`, `ecology_invalid` | The profile names a species not in the table, an entry the biome does not have, or a bad site window |

`cargo xtask check-ecology` runs the same check over every `.biome.ron` in the repository and fails on any of these.

### Shipped presets

`starter/biomes/` holds six European presets, each a `.biome.ron` with dominant, associated and understory layers whose
weights are the profile's shares (`regia_procgen::biome::ecology::PRESETS`; copy them into a project):

| Preset | Site | Canopy | Below |
|---|---|---|---|
| `birch_woodland` | wet, acid peaty gley | downy birch (dominant), Scots pine | rowan, alder buckthorn, eared willow; purple moor-grass, bilberry; peat moss |
| `oak_birch_woodland` | dry to mesic, acid sandy soil | sessile oak and silver birch (dominants), aspen | rowan, alder buckthorn; bracken, wavy hair-grass, bilberry; feather moss |
| `beech_forest` | mesic, base-rich mull over limestone | beech (dominant), sycamore maple, ash | woodruff, dog's mercury, wood anemone, male fern |
| `oak_hornbeam_forest` | mesic, base-rich loam | pedunculate oak and hornbeam (dominants), small-leaved lime, Norway maple | hazel; wood anemone, woodruff, dog's mercury, male fern |
| `boreal_spruce_forest` | mesic, acid podzol | Norway spruce (dominant), downy birch, Scots pine | rowan; bilberry, lingonberry, wavy hair-grass, wood sorrel; feather moss |
| `boreal_pine_forest` | dry, strongly acid sandy podzol | Scots pine (dominant), silver birch | juniper; heather, lingonberry; reindeer lichen, feather moss |

### Competition-aware placement (opt-in)

The starter graph does not apply ecology. The opt-in graph `starter/biomes_ecology.pgraph.ron`
(`regia_procgen::biome::STARTER_BIOMES_ECOLOGY_GRAPH`) adds `biome.ecology` between `biome.resolve` and `biome.exclude`.
With a profile's `competition: (shade_rules, self_thinning)` (without a profile: shade rules on, no thinning):

- **self-thinning**: in each world-anchored cell of `cell_m`, while the stand density `Σ scale^exponent` passes
  `max_stems_per_ha` per hectare, the smallest stem goes (ties by seed);
- **shade**: a shade-needing understory plant (light ≤ 3) outside every remaining tree crown (`spread_m`) is removed, and a
  light-demanding one (light ≥ 7) under a crown that casts deep shade (≥ 4) is removed; spring plants are exempt.

`biome.stats` then shows `ecology_thinned`, `ecology_unshaded_out` and `ecology_shaded_out` per entry. The output is a
pure function of the inputs and the seed, like the rest of the pipeline.

## Platforms and cost

Pure CPU on every platform; the output is deterministic (the same areas, definitions, terrain and seed
give the same output hash everywhere). On Android, run large biome generators as `Baked` rather than
`OnLoad`: above about 100 000 instances, bake instead.

## Errors

Answered as `F0002` (invalid params or definition) or `F0003` (unknown path or entity) with
`data.reason`:

| Reason | When |
|---|---|
| `biome_unknown` | The `.biome.ron` file, area entity or map texture does not exist; or `ProcGenPlugin` is missing |
| `biome_invalid` | Bad params, a definition that does not parse or breaks a rule (`data.errors` lists every issue), an unknown op or layer, or a `save` path that is absolute, holds a drive or `..`, or leads out of the project through a symlink (the message names the `path_*` sub-code, as in `procgen.graph_edit`) |
| `biome_area_open` | A `FromSpline` area whose entity has no `Spline`, or an open one (reported in the area's `error.reason` in `biome.areas`; no method answers it as an RPC error) |
| `biome_entry_unknown` | A child or `remove_entry` names an entry the layer does not have |
| `biome_not_generator` | `generator` is missing or the entity has no `ProcGenerator` |
| `biome_pointer_invalid` | A `set` op's JSON pointer names no value |
| `ecology_invalid` | `biome.validate_ecology`: the `.ecology.ron` does not parse or is newer than this build |

## Painted weights and density maps

`TerrainLayer` rules and filters read the same weights the terrain's material does: procedural by slope and altitude,
blended with the painted weights of `TerrainPaint` (the nearest heightfield sample) in proportion to their coverage. Editing
`TerrainPaint` marks generators that read the ground stale.

A `ScatterClass` source whose class names a density map places through that map. Runtime and tools hand it the CPU pixels
in the `ProcDensityMaps` resource, keyed by the path the `.scatter.ron` names (`regia_terrain::scatter::placement::DensityMap`);
a class whose map is missing places nothing and is reported as `biome_unknown` in `biome.areas` problems and the log. The map
path is a dependency of the run.

## Sample

`samples/procgen_biomes` runs the three biomes of the spec (pine forest, meadow, riverbed) on rolling terrain with a river
ribbon and a road whose exclusion clears the road:

```text
cargo test -p procgen_biomes                         # procgen.run, biome.stats, biome.claim through the agent
cargo run -p procgen_biomes -- --out target/biomes   # overview.png for review
```
