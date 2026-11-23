# Terrain operators: terraces, craters, pads, domain warp and noise

Five heightfield operators reshape terrain from a few numbers instead of brush strokes:

| Operator | What it does |
|---|---|
| `terrace` | Cuts the ground into flat treads `step_height_m` apart with smooth risers; noise shifts where each riser falls (`width_jitter`) and lets patches slump back to the original slope (`collapse`). |
| `crater` | Adds an impact crater: a bowl from `-depth_m` at the center to `+rim_height_m` at the rim crest, an ejecta blanket falling off as `r^-ejecta_falloff` to zero at `ejecta_extent` radii, and an optional central peak. It follows the ground's slope. |
| `flatten_pad` | Levels a circle or a turned rectangle (a build pocket, a landing pad, a resource site) to one height and feathers it into the ground over `feather_m` meters. Every sample inside the footprint gets exactly the pad height. |
| `domain_warp` | Moves heights sideways by up to `strength_m` along two smooth noise fields, so straight ridges and valleys meander. Heights keep their values, so the range never grows. |
| `noise` | Adds fBm (`-amplitude..amplitude`) or ridged noise (`0..amplitude`, sharp crests) with an optional grain: features `anisotropy` times longer along `grain_angle_deg`. |

They run in three places:

- **On the agent protocol:** `terrain.apply_operator` runs one operator over the live terrain and queues
  the change through the runtime height-patch queue (`terrain.patch_heights`), so the drawn terrain, the
  CPU map, physics and navigation all see it.
- **In procgen bakes:** the nodes `terrain.terrace`, `terrain.crater`, `terrain.flatten_pad`,
  `terrain.domain_warp` and `terrain.noise` record terrain edits, like `terrain.flatten_road`.
- **In Rust:** `regia_terrain::ops` (pure functions over a height slice and a `GridFrame`).

Angles are degrees from +X toward +Z. Centers are world `[x, z]` meters. Noise is a function of the
world position worked out from the full-terrain sample index, so a window of a map, or a tile of a
tiled bake, gives a shared sample the same bits. Results are the same on every platform: the few
transcendentals go through `regia_core::math::detmath`.

## `terrain.apply_operator`

```json
{"method":"terrain.apply_operator","params":{
  "op":"flatten_pad",
  "params":{"center":[120.0,-40.0],"shape":{"circle":{"radius_m":18.7}},"feather_m":20.0},
  "dry_run":true}}
```

| Param | Meaning |
|---|---|
| `op` | `terrace`, `crater`, `flatten_pad`, `domain_warp` or `noise` (required). |
| `params` | The operator's settings (below); a missing setting takes its default. |
| `rect` | `[col0, row0, col1, row1)` sample window to run over; default the whole map. At least 2 × 2 samples. A crater or pad is still placed by world center; samples outside the window are not changed. |
| `dry_run` | `true` reports the change and queues nothing. |

The answer (the same call without `dry_run`):

```json
terrain.apply_operator {"op":"flatten_pad","params":{"center":[120.0,-40.0],"shape":{"circle":{"radius_m":18.7}},"feather_m":20.0}}
→ {"op":"flatten_pad","rect":[0,0,1025,1025],"changed":[540,438,580,478],
 "changed_samples":1240,"min_delta_m":-3.91,"max_delta_m":2.06,
 "dry_run":false,"patches":[7],"queue":{"queued_patches":1,"...":"..."}}
```

`changed` is the sample rectangle that differs (full-map indices), `null` when nothing changed. The
patches are applied a budget of samples per frame; `wait_until` `terrain.get`'s `patches.completed`
reaches the last id in `patches`. Heights are quantized to the map's 16-bit grid when they land, as for
every runtime patch. The method acts on the terrain with the lowest entity id.

### Settings

`terrace`:

| Setting | Default | Meaning |
|---|---|---|
| `step_height_m` | 6.0 | Vertical spacing of the treads (> 0). |
| `base_m` | 0.0 | Treads sit at `base_m + k * step_height_m`. |
| `width_jitter` | 0.3 | 0 to 1: how far noise shifts each riser, as a fraction of a step. Treads stay at their levels. |
| `jitter_wavelength_m` | 150.0 | Wavelength of the jitter noise. |
| `edge_sharpness` | 0.8 | 0 leaves the ground unchanged; 1 gives near-vertical risers and exactly flat treads. |
| `collapse` | 0.0 | 0 to 1: about this share of the area slumps back to the original slope (1 restores everything). |
| `collapse_wavelength_m` | 120.0 | Size of the slumped patches. |
| `seed` | 0 | Seed of the jitter and collapse noise. |

`crater`: `center` (`[0, 0]`), `radius_m` (50, the rim crest), `depth_m` (8), `rim_height_m` (2),
`ejecta_falloff` (2.5), `ejecta_extent` (2.5 radii, > 1), `central_peak_m` (0, off),
`central_peak_radius_frac` (0.12 of the radius).

`flatten_pad`: `center` (`[0, 0]`), `shape` (`{"circle": {"radius_m": 20}}` or
`{"rect": {"size_m": [along, across], "rotation_deg": 0}}`, full sizes), `height_m` (omit for the mean
of the ground inside the footprint), `feather_m` (20; 0 is a hard edge).

`domain_warp`: `strength_m` (40, the largest displacement; 0 changes nothing), `wavelength_m` (300),
`octaves` (3), `seed` (0). Reads near the window edge clamp to it: give a window `strength_m` of margin.

`noise`: `kind` (`fbm` or `ridged`), `amplitude_m` (20), `wavelength_m` (400, the first octave),
`octaves` (5, 1 to 16), `lacunarity` (2), `gain` (0.5), `grain_angle_deg` (0), `anisotropy` (1, ≥ 1),
`sharpness` (ridged only, 2), `seed` (0).

### Error codes

| Code | When |
|---|---|
| `op_unknown` | `op` names no operator. |
| `op_bad_param` | A setting is out of range or of the wrong type; the message names it. |
| `op_bad_grid` | `rect` is not a window of at least 2 × 2 samples inside the map. |
| `op_non_finite` | A terrain height is not finite. |
| `op_pad_empty` | A `flatten_pad` without `height_m` covers no sample of the window. |
| `patch_no_target`, `patch_queue_full` | From the height-patch queue ([terrain runtime](terrain-runtime.md)). |
| `F0003` | No terrain, or no `TerrainHeightPatches` resource (add `TerrainPlugin`). |

## Procgen nodes

All five are bake-only terrain edits (spec 64 § 7.3): they need a `Baked` generator with
`allow_terrain_edits` and `[procgen] terrain_edits` in the project, or they fail with
`terrain_edits_not_allowed` / `flatten_runtime_forbidden`. Each takes the `ground` pin (a `Ground` item
from `in.ground`) and outputs the changed ground, so they chain:

```
in.ground → terrain.domain_warp → terrain.terrace → terrain.flatten_pad (in: pad points) → graph.output
```

Each node records one terrain edit holding the samples it changed and their heights before it.
Applied in node order they give the final ground; clearing the generator unwinds them in reverse,
bit for bit. A second bake with the same inputs is the same bake.

| Node | Pins | Notes |
|---|---|---|
| `terrain.terrace` | `ground` → `out` | Settings as above; `seed` is added to the node seed. |
| `terrain.domain_warp` | `ground` → `out` | Reads `strength_m` away: in a tiled bake it extends the halo. |
| `terrain.noise` | `ground` → `out` | `kind` `fbm` or `ridged`. |
| `terrain.crater` | `in` (points), `ground` → `out` | One crater per point at its XZ; `radius_m` times the point's `scale.x`. |
| `terrain.flatten_pad` | `in` (points), `ground` → `out` | One pad per point. `shape` `circle` (`radius_m` times `scale.x`) or `rect` (`length_m` along the point's yaw plus `rotation_deg`, `width_m` across). `height`: `mean` (ground inside), `point` (the point's y) or `fixed` (`height_m`). A pad that covers no captured sample is skipped with the warning `pad_empty`. Its mean height reads the whole pad, so it extends a tiled bake's halo. |

In a tiled bake each node records only the samples its cell owns; noise and terrace agree across cell
edges because they depend on world position only.

## From Direct Order 27's terrain tools

These operators replace `tools/terrain/terrain_features.py` (terrace, crater, flatten pad, domain warp
and noise) and `tools/terrain/terrain_noise.py`. Their test cases are ported as
`crates/regia_terrain/tests/terrain_ops.rs`. Differences:

- The resource-pad radius constant is not built in: pass the radius (DO27's standard is 18.7 m).
- `collapse` is a pointwise noise threshold (the same in every tile) instead of a rank over the whole
  grid, so it is an approximate area share.
- Grids are placed by world position and may be any size up to 8193 × 8193 samples.
- The noise uses the value-noise lattice of `terrain_features.py`; the gradient noise of
  `terrain_noise.py` is not ported (the same use, other bits).
