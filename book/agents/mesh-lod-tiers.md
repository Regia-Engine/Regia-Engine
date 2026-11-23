# Mesh LOD tiers (agents)

Rigid meshes with a `MeshLodChain` pick a level each frame from their projected size
(ADR-208). Since v0.1.20 the render profile has two tier knobs on top of
`mesh_lod_bias`, so the top tiers keep close-up detail:

| Preset | DO27 tier | `mesh_lod_bias` | `mesh_lod_distance_scale` | `mesh_lod_shadow_scale` |
|---|---|---|---|---|
| Mobile Low | ULTRA LOW (candidate) | 0.5 | 1 | 0.5 |
| Mobile High | - | 0.7 | 1 | 0.5 |
| Desktop Low | LOW | 0.85 | 1 | 0.5 |
| Deck | MEDIUM | 0.8 | 1 | 0.5 |
| Desktop High | HIGH | 1 | 2 | 0.35 |
| Desktop Ultra | ULTRA / MAX | 1.15 | 6 | 0.15 |

- **Colour.** The projected size is multiplied by `mesh_lod_bias` (clamped 0.25..4, as before), then
  by `mesh_lod_distance_scale`. The product is clamped to 0.25..16. Switch distances scale
  linearly: Desktop Ultra holds every colour level 1.15 × 6 = 6.9 times as far as a bias of 1.
- **Shadows.** The shadow LOD is chosen at the colour size times `mesh_lod_shadow_scale` (clamped
  0.05..1). Below 1, shadows drop detail earlier than colour. A shadow switch distance is the colour
  one times the shadow scale. Desktop Ultra's 0.15 makes its shadow factor 6.9 × 0.15 ≈ 1, so its
  shadows select about as a bias-1 colour does; at 0.3 the unit-budget bench measured 2.3 ms more
  for 40 battle hulls at 1440p on an RTX 5080, all of it in the shadow passes.
- **Unchanged.** Distance scale 1 with shadow scale 0.5 selects exactly as before, bit for bit
  (the 8-frame dither cross-fade and the ±15 % hysteresis included).

## Methods

- `render.get_profile` returns the three values under `profile` (`mesh_lod_bias`,
  `mesh_lod_distance_scale`, `mesh_lod_shadow_scale`).
- `render.set_profile {"mesh_lod_distance_scale": 3}` overrides a scale live, clamped to its range
  (`mesh_lod_shadow_scale` likewise). `preset` is optional when a scale is given; a preset in the
  same call is applied first. The profile then reads as customized (`preset: null`). Not persisted.
- `render.mesh_lod {radius?, thresholds?, height?, fov_y?}` returns the scales in effect
  (`bias`, `distance_scale`, `shadow_scale`, `color_factor`), their clamps, and for a mesh of
  bounding radius `radius` metres with the stored screen sizes `thresholds` (LOD 0 first; default
  `[1, 0.5, 0.25, 0]`), the distances in metres where its colour and shadow LOD switch
  (`switch.color_m`, `switch.shadow_m`), for the active profile and every preset (`presets`).
  `height` defaults to the last presented window height (1080 headless), `fov_y` to 1 radian.
  Read-only.

```json
{"method": "render.mesh_lod", "params": {"radius": 6.2, "thresholds": [2.4, 0.9, 0.3, 0], "height": 1440}}
{"method": "render.set_profile", "params": {"mesh_lod_distance_scale": 4, "mesh_lod_shadow_scale": 0.25}}
```

To check a hull: read its chain's thresholds (the cooked `CookedMesh::lods` screen sizes, or the
authored `LodGroup`), call `render.mesh_lod` with its bounding radius, then fly the camera to just
inside and outside a printed distance and compare `seeing.screenshot`s.

## Authored LOD thresholds from data

An authored `Stem_LOD0`…`Stem_LODn` chain takes each level's screen size from, in this order:

1. the `_LODn` node's glTF extras, `{"lod_screen_size": 0.45}`;
2. the project's `lod_classes.toml` (next to `assets/`), by the class named in the root node's
   extras `{"lod_class": "battle"}`, else by the first `[[assign]]` glob matching the asset path;
3. the importer default (1, 0.5, 0.25, 0.125, …; halve at each level, ADR-7869).

From v0.5.1, LOD3 has its own interval in chains of five or more levels. Re-cook with the
upgraded importer before packaging; existing authored extras/class values stay in control.
No format migrator is needed. Inspect a five-level fallback chain with
`render.mesh_lod {"thresholds": [1, 0.5, 0.25, 0.125, 0.0625]}` and compare the distinct
switch distances. See [glTF conventions](../../docs/formats/gltf-conventions.md#lod-screen-sizes-v0120-adr-260).

```toml
version = 1

[classes]
light = [0.9, 0.35, 0.12, 0.04]
battle = [1.2, 0.45, 0.16, 0.05]

[[assign]]
glob = "assets/units/light/**"
class = "light"
```

A chain that used any data must be strictly decreasing: a level that is not is set to half the
previous one, with a cook warning. Bad values (NaN, negative, above 64, a 0 before the last level),
bad classes and a class the table lacks fall back to the next source with a warning; `regia
cook` lists the table's own problems in its warnings. The file is capped at 64 KiB, 256 classes,
16 levels and 1024 rules. There is no protocol method for the table (the cook is desktop
tooling); check a result with `assets.describe` on the `#lod_group/<Stem>` sub-asset.

Format: `docs/formats/gltf-conventions.md` § LOD screen sizes. Bench:
[Performance & benches](../user-guide/perf-benches.md#the-unit-budget-bench) (`close20` and `ultra`
layouts, and the per-tier switch-distance table).

Per object, a `ShadowLodHint` caps or floors the LOD the shadow draws use (ADR-1622):
[Shadow LOD hint](shadow-lod-hint.md).
