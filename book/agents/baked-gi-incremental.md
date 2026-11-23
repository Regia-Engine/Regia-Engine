# Incremental GI bakes

A map bake traces every probe. After an edit (a moved crate, a new wall, a raised hill), most probes see nothing
new. `--incremental` keeps a state file between bakes. The next bake traces only the probes near what changed and
reuses the rest from the state. Design: ADR-1094.

## Bake incrementally

```text
regia bake-gi <scene> --incremental <state> [--influence METERS] [--tile-columns N] [--cpu] [--out FILE]
regia bake-gi --from-scene <level> --bake --incremental <state> [--write-level]
```

`<state>` is a file path. The first bake finds no state, traces everything and writes the state there. Each later
bake reads it, traces what changed, writes the volume, and writes the new state back. The state is a build cache,
not content: keep it out of `assets/` (for example `bake/state/<map>.fgis`). Deleting it only makes the next bake
a full one.

The bake splits the volume into cells:

- one cell per detail brick;
- one cell per tile of `N × N` terrain-layer columns (`--tile-columns`, default 8).

A cell is traced again when:

- **`layout`**: the state has no cell with the same probes. It is a new or moved brick, or a terrain tile whose
  ground changed. Adding or removing a brick does not touch the others, even though their probe indices move.
- **`scene`**: something within the influence margin of the cell changed. That covers a placed mesh's geometry,
  material, textures or transform, a local light, or a heightfield sample. The margin defaults to the layout's
  `max_distance` and is set with `--influence`.
- **`state`**: the stored cell does not fit the layout (a damaged state).

Every cell is traced again (`full`) when there is no state, or when something that lights every probe changed:

| `full` | What changed |
|---|---|
| `no_state` | The state file does not exist yet. |
| `state_unreadable` | The state file is damaged, truncated or from another version (warning `bake_state_unreadable`). |
| `tracer` | Regia Engine version, `--cpu` or GPU, or the GPU and its driver. |
| `settings` | Rays, samples, bounces, seed, relocation, filter, moment resolution, distance clamp, or `--influence`. The batching (`--probes-per-batch`, samples per GPU submission) is not compared. |
| `sky` | The sun, the sky, sky bodies, stars or media. |
| `terrain` | The heightfield's size, spacing or material, or the terrain view albedo. |
| `camera` | The description's camera (texture detail fades with the distance to it). |

The volume is rebuilt from all probes. The 3×3×3 filter and the encode run over the whole volume, so:

- a rebake of an unchanged scene traces nothing and writes the same bytes;
- a reused cell's probes are the previous bake's, bit for bit;
- an incremental bake with no state is the normal bake, bit for bit.

### What is approximate

An edit farther than the margin from a cell does not rebake that cell, so the cell keeps the old scene's light.
The summary says so with `"approximate": true`, which means cells were reused although the scene changed
somewhere. A cell that is traced again gets other random noise than in a full bake. It matches a full bake in
expectation, not bit for bit. For a volume you ship, run one bake without a state, or delete the state first.

## The summary

The summary is the normal bake summary ([Baking GI from meshes](baked-gi-bake.md)) with three more keys:

```json
{"ok": true, "out": "assets/maps/courtyard.fgiv", "report": {"probes": 1924, "valid": 1924},
 "incremental": {"full": null, "cells": 12, "cells_reused": 11, "cells_baked": 1,
                 "probes_reused": 1860, "probes_baked": 64, "scene_changed": true, "approximate": true,
                 "influence_margin": 32.0,
                 "baked": [{"cell": {"brick": 3}, "probes": 64, "reason": "scene"}]},
 "state": {"path": "bake/state/courtyard.fgis", "written": true, "bytes": 512344},
 "warnings": []}
```

The numbers above only show the shape.

- `incremental.baked` lists the traced cells and why (`layout`, `scene`, `state`). It is empty when `full` is set.
  A cell is `{"brick": index}` or `{"terrain": [i, j]}` (tile column `i` along X, row `j` along Z).
- `state.written` is false when the state could not be written (warning `bake_state_write_failed`). The volume is
  written anyway, and the next bake traces everything.

## Plan without tracing

```text
regia bake-gi <scene> --incremental <state> --plan-incremental [--cpu]
```

This prints which cells a bake would reuse and trace, and why. It traces nothing and writes nothing. It returns
`{"ok": true, "plan_incremental": true, "state": {"path", "exists"}, "incremental": {...}, "warnings", "layers"}`.
With `--cpu` it needs no GPU. Without it, the GPU adapter is opened to compare the device, but nothing is traced. It
does not combine with `--from-scene --bake`: export first, then plan on the exported `.bake.ron`.

Use it before a long bake to see how much will be traced. Use it after an edit to check that the edit reaches the
cells you expect.

## Errors and warnings

| Code | When |
|---|---|
| `bake_incremental_invalid` | In the error message when the options are built in code with a negative or non-finite margin or 0 columns per tile. On the command line, `--influence` (a finite distance, 0 or more) and `--tile-columns` (1 or more) are checked by the parser first: a usage error, exit 2. |
| `bake_state_unreadable` | Warning: the state could not be read or used. Every cell is traced and a new state is written. |
| `bake_state_write_failed` | Warning: the volume is written but the state is not. The next bake is a full one. |

`--incremental` does not combine with `--dry-run`, `--sample`, `--check-volume` or `--from-scene --check`. With
`--from-scene` it needs `--bake`: an export alone reads no state. `--check-volume` passes on an
incremental bake's volume. It compares the recorded scene hash, layout, lights and settings, and an incremental
bake records them like a full bake.

## Library

`regia_pt::bake_incremental`: `bake_cells`, `cell_scene_hashes`, `global_hashes`, `plan_incremental`,
`bake_gi_volume_incremental`, `BakeState` (`read`, `write`, `to_bytes`, `from_bytes`). The trace and the finish of a
bake are split into `regia_pt::probe::bake_probes` (any subset of probes) and `regia_pt::probe::finish_bake`.
