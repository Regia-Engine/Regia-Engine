# Baked GI across volumes and seams

A world can be lit by several baked GI volumes at once: tiles of a large map baked one by one, a
streamed region's own bake beside the map's, or a small high-resolution bake around a set piece
inside a large one. The renderer blends them in the same screen pass, so the light is continuous
across every seam between them. The volumes themselves are described in
[Baked GI volumes](baked-gi.md); how a level names its own volume is in
[Baked GI from the level](baked-gi-maps.md).

## How volumes blend

The renderer blends up to **8 volumes** a frame: the level's own volume (`set_baked_gi`) and the
layers set with `FrameRenderer::set_baked_gi_layers`. A layer (`regia_render::baked_gi::BakedGiLayer`) is
a volume with three settings:

| Field | Meaning |
|---|---|
| `priority` | Where volumes overlap, the higher priority lights first and the lower ones fill what it leaves. Equal priorities blend by weight. The level's own volume has 0. |
| `blend_distance` | Meters inside the volume's footprint over which it fades in from its edge. 0 (the default): no fade, only the volume's own one-cell fringe outside its outer probes. X and Z always fade. Y fades on a brick box and stays open on a terrain layer, so a blend distance set for a horizontal overlap does not fade the ground or the sky. It fades every closed edge, so a tile at the world's edge fades there too. |
| `weight` | 0-1 multiplier: a streamed volume fades in from 0 and out to 0. |

At each point every volume's own lookup is weighted by `weight × coverage × edge`. Volumes of one
priority are averaged by those weights; their coverage is the sum, capped at 1. Priorities are
then layered, highest first, each taking what the ones above left. Each of those factors is
continuous, so the light is too.

What that means for content:

- **Tiles cut one probe column apart** (tile B's first column one spacing past tile A's last):
  no blend distance. Across the seam cell the two fringes add up to full coverage, and the light
  goes linearly from A's edge probes to B's, as one volume over both would.
- **Tiles that overlap** by `o` meters: set `blend_distance` to `o`. The weights then add up to
  exactly 1 over the overlap, along both axes and where four tiles meet at a corner, and the light
  goes linearly from one tile to the other. A blend distance larger than the overlap leaves a dip
  in coverage there.
- **A nested set piece**: give it a higher priority and a blend distance for the hand-over width.
  Inside, only its light shows; over the blend distance it hands back to the volume around it.
- **A gap** wider than the fringe between two volumes: the coverage falls to 0 between them, and
  the frame's other GI (Balanced or Depth) fills in. That fade is continuous too.

Volumes past 8, in priority order, are left out, and so is any volume that would push the
combined size past the device's storage-binding limit. Both are counted in
`volumes_dropped`. With one volume at full weight and no blend distance, the lookup is the
single-volume lookup, unchanged bit for bit.

## Streamed levels

A level opened with `LoadMode::Additive` on top of the open map (a streamed region) can name its
own `.fgiv` in `LevelSettings.look.baked_gi`, just as a map does. The framework
(`regia_framework::baked_gi_stream`, part of `BakedGiLevelPlugin` and so of `DefaultPlugins`)
does the rest:

- **The region loads:** its volume is loaded (the cooked asset, or the file under `assets/` in a dev
  run) and joins the blend. It fades in over `fade_seconds` (default 1 s, real time, so it also
  runs while gameplay is paused).
- **The region unloads:** its volume fades out over the same time and is then removed. If the
  region streams back in during the fade-out, the fade turns around from where it was.
- **Hot reload:** a recooked or rewritten file is loaded again without reopening the region.
- **Failure:** a missing or unreadable file sets that region's `error` and `error_code`
  (`volume_not_found`, `volume_unreadable`), logs one warning and adds nothing. The other volumes
  keep lighting the frame.
- A region that names the map's own volume, or none, adds nothing.
- An additive load no longer replaces the open map's `LevelSettings` (look, sun, baked GI).
  The streamed level's own settings stay in the `LevelLibrary`. A level opened additively into an
  empty world still becomes the current level and brings its settings.

`BakedGiStreamSettings` (a resource) sets `fade_seconds`, `blend_distance` and `priority` for every
streamed volume. The plugin sets the renderer's layers only while it has streamed volumes, and
once more to clear them. A host that sets layers of its own, and streams no baked regions, keeps
its layers.

## Agent methods

### `level.get_baked_gi_stream` (read only)

| Field | Meaning |
|---|---|
| `plugin` | the stream state exists (the level plugin is installed) |
| `tiles` | one entry per streamed volume: `level` (id), `volume` (path), `weight` (0-1, the fade), `loaded` (its level is still loaded; `false` while it fades out), `installed` (loaded and in the blend), `probes`, `error`, `error_code` |
| `settings` | `{ fade_seconds, blend_distance, priority }`, or `null` |
| `renderer` | `{ installed, layers: [{ probes, priority, blend_distance, weight }], volumes, volumes_dropped, max_volumes, active }`, or `null` without a renderer. `volumes` is how many the last frame blended. |

### `level.set_baked_gi_stream`

Params (all optional; an unset field keeps its value): `fade_seconds` (≥ 0; 0 = at once),
`blend_distance` (meters, ≥ 0), `priority` (integer). Returns `{ before, after }`. Mutating
(`edit_scene` permission).

### `level.sample_baked_gi` (read only)

Params: `points` (1-4096 world points `[x, y, z]`, in order along the line to check), `normal`
(default `[0, 1, 0]`), `view` (toward the viewer; default the normal). Returns:

| Field | Meaning |
|---|---|
| `samples` | `[{ p, irradiance_over_pi, coverage }]`: the blended baked light (irradiance / π, RGB) and coverage at each point, as the renderer blends the volumes, with the last frame's sun and fill (before the first baked frame, the sun-bounce and fill components read 0) |
| `max_step` | `{ irradiance_over_pi, coverage }`: the largest change of any channel, and of the coverage, between neighboring points |
| `volumes` | volumes installed (the level's own plus layers) |
| `installed` | the renderer has a volume to light with |

Errors: `invalid_params` for a missing or empty `points`, a point that is not 3 finite numbers,
or a zero `normal` or `view`. `not_found` when the app has no renderer.

## Checking a seam

1. `level.get_baked_gi_stream`: each tile has `installed: true`, `weight: 1` and `error: null`.
   `renderer.volumes` counts the map's volume plus the tiles, and `volumes_dropped` is 0.
2. `level.sample_baked_gi` along a line across the seam, a point every 10 cm, on the floor
   (`y` a few centimeters above the ground), `normal` up. Compare `max_step.irradiance_over_pi`
   with the same call along a line inside one tile. A seam shows as a single large step (the whole
   difference between the tiles); a good blend changes no faster than the light already does
   inside a tile. `max_step.coverage` near 0 with every `coverage` 1 means no hole.
3. Look at it: `seeing.screenshot` with the camera across the seam. Tiles that
   disagree a lot (baked with different light) meet without a step, but the change in light can
   still be visible. Rebake them with the same lights, or overlap them and set `blend_distance`
   to the overlap.
4. While a region streams in, step a few frames (`play.step`): `tiles[].weight` rises by
   `dt / fade_seconds` a frame, and `level.sample_baked_gi` at a point inside the region moves
   gradually, never all at once.

Tests: `regia_render` `baked_gi::blend` (continuity across abutting, overlapping, cornering,
nested and gapped volumes; two unit cubes stacked in Y and the same fixture in X; the uniform's
layout against the shader), `tests/baked_gi_seams.rs`
(the GPU screen pass against its CPU mirror across a seam), `regia_pt` `tests/baked_gi_seams.rs`
(two path-traced tiles against one volume baked over both), `regia_framework`
`baked_gi_stream` (fade in and out in fixed steps, no pop, missing files) and `regia_scene`
`an_additive_load_keeps_the_current_levels_settings`. The hand-run GPU bench
`bench_blended_volumes_at_1440p` (`regia_render` `tests/baked_gi_seams.rs`, `--ignored`) prints the
screen pass's cost for one volume, four abutting tiles and four overlapping tiles.

## A seam in Y

A brick volume (no terrain layer) is a closed box. Its edge weight multiplies a Y ramp by the
same rule as X and Z: 0 on the face, 1 at `blend_distance` inside, and 1 everywhere when
`blend_distance` is 0. The volume's own coverage is that hard cut. Two such volumes stacked in
Y, overlapping by `o` meters with `blend_distance` set to `o`, add to weight 1 through the
overlap and to the one volume's weight outside it. A point outside both has coverage 0. The
same two cubes overlapped in X follow the same rule. Four tiles meeting at a corner still sum
to full coverage.

A terrain layer is open in Y. Its blend distance fades the vertical faces of the footprint and
does not fade the ground or the open sky. The layer's own coverage fades above its top probes.
A tile seam checked on the floor is unchanged.

Query it with `level.sample_baked_gi` (no separate method). Pass points every 10 cm up a
vertical line through the overlap, and a horizontal `normal` so the sample is not pushed
through the floor or the ceiling. `coverage` stays 1 through the slab when the blend distance
equals the overlap, and the irradiance moves from the lower volume to the upper one.
`max_step.irradiance_over_pi` stays within that slope. The same call along X through an overlap
of the same width is the same blend. Errors are the ones `level.sample_baked_gi` already
returns (`invalid_params`, `not_found`).

For unit probe boxes at `[0, 0, 0]..[1, 1, 1]` and
`[0, 0.75, 0]..[1, 1.75, 1]`, both installed with `blend_distance: 0.25`, query:

```json
{"method":"level.sample_baked_gi","params":{"points":[[0.5,0.75,0.5],[0.5,0.8125,0.5],[0.5,0.875,0.5],[0.5,0.9375,0.5],[0.5,1.0,0.5]],"normal":[0,0,1],"view":[0,0,1]}}
```

Expect five samples with coverage 1. For uniform light in each box, each step changes the
irradiance by one quarter of the difference between the two volumes. The deterministic
`baked_gi::blend::tests::stacked_cubes_shader_matches_analytic_weights_and_cpu_light` test
executes this fixture in the production shader on lavapipe, then repeats it in X and with a
zero blend distance. No reference image is needed.
