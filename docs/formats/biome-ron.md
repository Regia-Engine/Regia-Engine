# `.biome.ron` (biome definition, format v1)

A biome: what grows and lies on the ground in one kind of place (spec 65 § 3, `regia_procgen::biome::def::BiomeDef`,
agent page `book/agents/biomes.md`). Frozen (`docs/specs/12-assets.md` § 4a): a shape change bumps `format_version`
with a `VersionStep` migrator (`regia_procgen::biome::def::BIOME_MIGRATIONS`, applied to the parsed RON value before
it is typed) and `regia upgrade-project` support; `cargo xtask check-formats` holds the field-list hash (`biome`).

Every field has a default, so a file names only what it changes; `BiomeDef::to_ron` writes the canonical form with
every field. Struct names (`BiomeDef(...)`) are optional when reading. `biome.def_get` shows a definition with the
defaults filled in.

```ron
BiomeDef(
    format_version: 1,                  // default 1
    name: "Pine forest",                // default ""; the first part of `biome/layer/entry`
    color: (0.15, 0.45, 0.2),           // default (0.5, 0.5, 0.5); debug tint and the default map key
    map_key: None,                      // Some((r, g, b)): 8-bit key in biome map textures
    priority: 20,                       // default 0; higher wins where areas overlap
    layers: [(
        name: "canopy",                 // required, unique in the biome
        rank: 30,                       // default 0; higher-rank layers keep their spots in the resolve
        sharing: Exclusive,             // Exclusive (default) | Shared
        source: Ground(spacing_m: 6.0, jitter: 0.9, noise: Some((scale_m: 60.0, octaves: 3, min: 0.2, max: 1.0))),
        channels: [ (name: "steep", rule: Slope(min_deg: 25.0, max_deg: 90.0)), (name: "flat", rule: Rest) ],
        entries: [(
            name: "pine_tall",          // required, unique in the layer
            spawn: Mesh("trees/pine_a.glb"),   // None (default: a spacer) | Mesh(path) | Prefab(path)
            weight: 3.0,                // default 1.0; weight 0 and no channels = child-only
            channels: ["flat"],         // default []: every channel (when weight > 0)
            enabled: true,              // default true
            filters: [ Altitude(min: 0.0, max: 1400.0, feather: 50.0) ],   // default []
            filters_off: false,         // default false
            footprint: (core_m: Some(0.6), spread_m: Some(3.5)),  // None: from the mesh bounds
            placement: (upright: 1.0, yaw: Random, scale: Range(0.8, 1.25), scale_from_source: false,
                        sink_m: 0.15, offset: None),
            render: (cast_shadows: true, draw_distance_m: 1500.0, fade_m: 100.0, collision: Simple),
            children: [ (entry: "fern", count: (2, 5), radius_m: (1.0, 3.0), rate: 0.6,
                         keep_clear_of_parent: Core, scale_with_parent: false, graph: None) ],
        )],
        max_child_depth: 2,             // default 1; 0 = no children
    )],
    grass: [ (class: "forest_floor", density: 0.6, outside: Keep) ],   // default []
)
```

## Defaults

| Field | Default |
|---|---|
| `source` | `Ground(spacing_m: 4.0, jitter: 0.8, noise: None)` |
| `Ground.noise` fields | `scale_m: 50.0, octaves: 3, min: 0.0, max: 1.0` |
| `Poisson.min_dist_m` | `2.0` |
| `Graph.params` | `{}` (parameter name → RON text) |
| `channels` | `[]`: one implicit channel `all` |
| channel rules | `Slope(min_deg: 0, max_deg: 90)`, `Altitude(min: -1e9, max: 1e9)`, `TerrainLayer(name, min_weight: 0)`, `Mask(asset, channel: "r", min: 0)`, `Attr(sel, min: -1e9, max: 1e9)`, `Random(fraction: 0)`, `Rest` |
| filters | `feather: 0`; windows `Altitude`, `WaterDistance`, `Attr`, `Graph` `(-1e9, 1e9)`; `Slope (0, 90)`; `Density`, `TerrainLayer`, `Mask` `(0, 1)`; `Mask.channel: "r"` |
| `placement` | `upright: 1.0, yaw: Random, scale: Keep, scale_from_source: false, sink_m: 0.0, offset: None` |
| `render` | `cast_shadows: true, draw_distance_m: 300.0, fade_m: 20.0, collision: None` |
| `footprint` | `core_m: None, spread_m: None`: `spread_m` = half the larger horizontal mesh extent, `core_m` = 0.25 × `spread_m`; 1 m spread when the mesh geometry is not available (warning `biome_footprint_default`) |
| child rule | `count: (1, 3), radius_m: (0.5, 2.0), rate: 1.0, keep_clear_of_parent: Core, scale_with_parent: false, graph: None` |
| `grass` entry | `density: 1.0, outside: Keep` |

## Rules (checked on load; `biome_invalid` lists every issue)

- Layer names are unique and non-empty in the biome; entry and channel names are unique and non-empty in their layer.
- An entry's `channels` name channels of its layer (`all` when the layer has none).
- Weights are finite and ≥ 0; `upright` is in 0..1; `Range(lo, hi)` has `0 < lo ≤ hi`; filter windows have
  `min ≤ max` and `feather ≥ 0`; child rules have `count.0 ≤ count.1`, `0 ≤ radius_m.0 ≤ radius_m.1`, `rate` in 0..1.
- A child rule naming an entry the layer does not have is `biome_entry_unknown`.
- `format_version` above 1 is refused (`biome_invalid`).

## Scatter import

`regia procgen import-scatter <file.scatter.ron> --out <dir> [--set-path <project-relative path>]` writes
`<stem>.biome.ron`: one layer per scatter class with source `ScatterClass(set, class)` (the exact positions the scatter set
places), one entry per weighted mesh, zero footprints, `upright = 1 − align_to_normal`, `scale: Keep`, render settings from
the class. A class with a density map reads it from the host's `ProcDensityMaps` resource; without it the run reports the
class and places nothing for it.

## Terrain layers

`TerrainLayer(name, ...)` rules and filters read the terrain's layer stack (`.terrain.layers.ron`): the procedural
slope/altitude weight of the named layer at the point, blended with the painted weights of `TerrainPaint` when the terrain
has them (ADR-2003). A missing
layer (or no terrain) makes the rule never match and the filter pass, with warning `biome_terrain_layer_unavailable`.
