# Terrain editing (sculpt and paint)

`terrain.stroke` runs one brush stroke on a terrain: the same brush core the editor's terrain
tool uses (`regia_terrain::brush`, ported from Fyrox's brush-stroke processor). Heights change
in place on the `Terrain` component; the renderer re-uploads only the dirty rectangle (the path
craters use), and the next save re-cooks only the cook tiles the stroke touched.

## One stroke

```json
{ "method": "terrain.stroke",
  "params": { "mode": "raise", "center": [120.0, -40.0], "radius": 12.0, "strength": 4.0 } }
```

| `mode` | What `strength` means | Effect |
|---|---|---|
| `raise` / `lower` | meters at full brush strength | adds / subtracts, falling off to the edge |
| `flatten` | opacity 0..1 | pulls toward the height under `center` |
| `assign` | opacity 0..1 | pulls toward `value` (meters) |
| `smooth` | opacity 0..1 | pulls toward the mean of the `(2k+1)²` neighborhood (`kernel_radius` k, default 2) |
| `paint` | opacity 0..1 | raises material layer `layer`'s painted weight to 1 |

Optional: `to` or `path` (`[x, z]` points) drag the brush; `shape: "rectangle"` with `width` /
`length`; `hardness` (0 soft, 1 hard edge); `layer` (paint any mode into a layer's weight map
instead of the heights); `entity` (default: the first terrain).

With `layer` set, `strength` is a fraction of that layer's 0..=255 weight, not meters. `raise`
adds `strength × 255` at full brush strength and `lower` subtracts it. `flatten`, `assign`,
`smooth` and `paint` blend toward their target by `strength` (`assign` and `paint` scale the
target weight by 255). The same `strength` on the heightfield stays in meters, as the table says.

The reply:

```json
{ "entity": 4294967297, "mode": "raise", "target": "height", "samples": 452,
  "dirty": { "col_start": 1270, "col_end": 1295, "row_start": 1012, "row_end": 1037 },
  "cells": [[39, 31], [40, 31]], "cell_size": 32, "height_before": 212.4, "height_after": 216.4 }
```

`cells` are the 32-sample edit cells the stroke changed; a save re-cooks only the cook tiles
that hold them.

A stroke changes each sample at most once (with the strongest brush coverage it reached), so the
same call always produces the same heights, and dragging back and forth over a spot does not
pile up.

## Check it

1. `seeing.screenshot` (or `editor.capture_viewport` in the editor) → `before.png`
2. `terrain.stroke …`
3. Screenshot again → `after.png`, then compare by eye.

In the editor every stroke, from the mouse or from `terrain.stroke`, is one undo step:
`editor.undo` restores the heights bit for bit. If the brush worker dies mid-stroke, the part that landed is one
undo step too. A linked draw (`TerrainSharedDraw` with its `height_patches_handle` inserted as the
`TerrainHeightPatches` resource) gets every stroke in the next `Update`. Without an editor, only the newest 256
strokes keep an undo record.

Painted mesh instances have their own brush: [painted scatter](terrain-scatter-paint.md). Its strokes
are undo steps too, and a sculpt re-seats what it moved.

## Painted layers

A terrain with a `TerrainPaint` component carries one 8-bit weight per material layer per
sample. Painted weight replaces the procedural slope/altitude weight in proportion to its
coverage (`TerrainLayers::blend_painted`), so an unpainted map looks exactly as before.
`paint` with `layer` that the map does not have returns an error naming the painted layer
count.
A weight map whose grid is not the heightfield's grid (one weight per heightfield sample) is
refused with an error that names both sizes, instead of painting the wrong samples.

After sculpting, `terrain.parity` ([terrain parity](terrain-parity.md)) reports samples raised or
lowered past the map's height range, which the saved heightfield clamps.

## Noise and ramp

`noise` adds seeded value noise: `strength` is the peak offset in meters (half the samples go up,
half down), `wavelength` the feature size in meters (default 8 samples), `seed` picks the field.
The same seed, brush and path give the same heights.

```json
{ "method": "terrain.stroke",
  "params": { "mode": "noise", "center": [0.0, 0.0], "radius": 30.0, "strength": 1.5 } }
```

`ramp` cuts a straight incline from `center` to `to` (or the last `path` point). The brush width
is the ramp's width and `hardness` its shoulder; `strength` is the opacity. The heights at the two
ends are the terrain's own unless `from_height` / `to_height` (meters) are given.

```json
{ "method": "terrain.stroke",
  "params": { "mode": "ramp", "center": [-40.0, 10.0], "to": [20.0, 10.0], "radius": 4.0,
              "hardness": 0.7 } }
```

In the editor's terrain tool, `6` is Noise (each stroke draws a new seed) and `7` is Ramp: press at
the start, drag, release at the end.

`8` is Hole: a click cuts a round hole of the brush's size, a drag cuts a slot along it as wide as the
brush, and Shift removes the named hole under the release point. Each is one undo step. The first
hole edit brings in the level's `<map>.holes.ron`; a file that cannot be read turns hole edits off
rather than being replaced. Agents cut the same holes with `terrain.set_hole`
([terrain runtime](terrain-runtime.md)).
`9` is Scatter, the brush for mesh instances: see [painted scatter](terrain-scatter-paint.md).

## Saving holes and paint

Every save a user asks for (Save, Save As, saving on close) and the agent's `editor.save_level` write
the same set: the level, the re-cooked tiles of a sculpted terrain, and next to the level the files
below, each only when it changed. The `editor.save_level` reply names them in `terrain_tiles_recooked`,
`terrain_holes_file` and `terrain_paint_file`. The autosave stays a recovery copy of the level alone.

| File | Holds | Agent |
|---|---|---|
| `<map>.holes.ron` | named and unnamed holes | `terrain.save_holes`, `terrain.load_holes` |
| `<map>.weights.bin` | painted layer weights (`LayerWeightMap`, versioned) | `terrain.save_paint`, `terrain.load_paint` |

`terrain.save_paint { path, entity? }` and `terrain.load_paint { path, entity? }` take a map or level
file (`maps/arena.level.ron` means `maps/arena.weights.bin`) or a name ending in `.weights.bin`, under
the project root. A save writes atomically and replaces the file; a load needs the terrain's grid.

- `terrain.save_paint` replies `{ entity, path, width, height, layers, bytes }`.
- `terrain.load_paint` replies `{ entity, path, width, height, layers }` and uploads the weights.
  A weights file over 1 GiB + 1 MiB (the largest weight map, 2^30 weights, plus its header) is
  refused before it is read (`height_io_decode`), so whatever `terrain.save_paint` wrote loads back.
- Errors: `F0002` (bad path), `F0003` (no terrain, or no paint to save), `height_io_write`,
  `height_io_read`, `height_io_decode`, `height_io_size_mismatch`.

Check it: `terrain.save_paint`, then `terrain.load_paint` in a fresh session and compare a
`seeing.screenshot` of the same camera.

A host that spawns the terrain loads these files itself, with `regia_terrain::load_holes_ron` and
`LayerWeightMap::from_bytes`. A level that names its terrain (scene v12, [Level terrain](level-terrain.md))
brings it and its side files in on its own, and [New terrain](#new-terrain) creates one.

## Heightmap export and import

A save re-cooks only the cooked heightfield. To carry sculpted heights back into the terrain's
source file, export them:

```json
{ "method": "terrain.export_heights", "params": { "path": "terrain/island.r16" } }
```

```json
{ "entity": 4294967297, "path": "terrain/island.r16", "format": "r16", "width": 4097,
  "height": 4097, "bytes": 33570818, "clamped_low": 0, "clamped_high": 12 }
```

- `path` is relative to the project root (no `..`); parent folders are created; the file is
  written atomically.
- `format`: `png` (16-bit grayscale) or `r16` (headerless, `endian` `little` by default or `big`);
  by default taken from the extension (`.png`, `.r16`, `.raw`).
- The samples are the map's 16-bit grid, the encoding a save uses; row 0 is the −Z edge, the order
  the cooker's PNG import reads. `clamped_low` / `clamped_high` count samples sculpted below or
  above the map's height range (written as 0 or 65535).

`terrain.import_heights` reads a file back onto the terrain:

```json
{ "method": "terrain.import_heights", "params": { "path": "terrain/island.r16" } }
```

<!-- not-a-request -->
```json
{ "entity": 4294967297, "path": "terrain/island.r16", "format": "r16", "width": 4097,
  "height": 4097, "changed": 5210, "undoable": true,
  "dirty": { "col_start": 1201, "col_end": 1390, "row_start": 640, "row_end": 702 } }
```

The file's grid must be the terrain's (`terrain.get` reports it). A RAW file without `width` and
`height` is read as a square. Only changed samples are written; the GPU re-uploads the dirty
rectangle and a save re-cooks the touched tiles. In the editor the import is one undo step
(`undoable` is false only past 4 194 304 changed samples).

| Code | Meaning |
|---|---|
| `height_io_bad_format` | unknown `format` or `endian`, or no extension to tell the format |
| `height_io_write` | the file could not be written |
| `height_io_read` | the file could not be read |
| `height_io_decode` | not a 16-bit grayscale PNG, or a RAW of the wrong length |
| `height_io_size_mismatch` | the file's grid is not the terrain's |

Check a round trip: `terrain.export_heights` to `a.r16`, `terrain.import_heights` from it, and
`changed` is 0 on a terrain that was saved and reloaded.

The headless editor runs the same CPU terrain frame effects as the windowed editor:
opening a level loads its named terrain, and agent strokes enter the editor undo stack.
This path works with `--no-gpu`; rendering and screenshot evidence still require a device.

## New terrain

`editor.terrain_create` makes a terrain from nothing, as the Terrain panel's **New terrain…**
dialog (and **Content ▸ New ▸ Terrain…**) does (ADR-7727). It is an editor method because the cook
runs in the editor's asset pipeline.

```json
{ "method": "editor.terrain_create",
  "params": { "name": "hills", "size": 129, "cell_size": 1.0, "height_range": [-10.0, 90.0] } }
```

<!-- not-a-request -->
```json
{ "job": "terrain_create:1", "state": "done",
  "layers": "assets/maps/hills/hills.terrain.layers.ron", "folder": "assets/maps/hills",
  "files": ["assets/maps/hills/heightfield.json", "assets/maps/hills/heightfield.png",
            "assets/maps/hills/hills.terrain.layers.ron", "assets/maps/hills/hills.terrain.layers.ron.meta"],
  "map_id": "…", "cooked": 2, "warnings": ["…"], "size": 129, "cell_size": 1.0,
  "world_width_m": 128.0, "height_range": [-10.0, 90.0],
  "terrain": { "layers": "maps/hills/hills.terrain.layers.ron", "map": "maps/hills/hills",
               "world_width_m": 128.0, "height_scale_m": 100.0, "height_offset_m": -10.0 },
  "entity": 4294967310, "loaded": true }
```

- **Files.** The cook convention (`docs/formats/terrain.md` § 9) in `assets/<folder>/<name>/`:
  the layer file and its `.meta` (the map id), `heightfield.png` (16-bit gray, flat at the height
  nearest 0 m inside `height_range`, or the heightmap's samples) and `heightfield.json`
  (`size_m`). Layers are `layers` (names, bottom first; `grass`, `rock` and `dirt` get slope
  presets), else grass, rock and dirt when the project has `assets/starter/terrain/<layer>/`
  for each (their `albedo`, `normal`, `orm`, `height` PNGs are copied to `detail/<layer>/`), else
  one `ground` layer. No format changes: these are the existing layer and heightfield files.
- **Sizes.** `size` is samples a side: 129, 257, 513, 1025, 2049 or 4097. `cell_size` is 0.25,
  0.5, 1, 2 or 4 m; the world width is `(size − 1) · cell_size`.
- **Sample layout** (ADR-10079). `sample_layout` is `VertexCentered` (the default: samples on the
  edges, the width above) or `CellCentered`: each sample is the center of a `cell_size` cell, so
  the terrain is `size · cell_size` wide and the level's block names the layout (use it for a
  heightmap whose pixels are areas). `heightfield.json` keeps the samples' span
  `(size − 1) · cell_size`. The New terrain dialog's **Samples at cell centers** box sets it. Any
  other value is `terrain_create_bad_params`.
- **Import as new.** `heightmap` (a project-relative 16-bit `.png`, `.r16` or `.raw`; `endian`
  for RAW) starts from a file: its samples are kept, its size is the terrain's (`size`, if given,
  must match). Exporting terrain A with `terrain.export_heights` and creating B from that file
  gives B A's samples.
- **The job.** The files are written and cooked in a staging folder under the project's state
  directory, then moved into `assets/` in one rename. Without `async` the call waits and answers
  the terrain; `async: true` answers `{ job, state: "running" }` (poll `agent.job_status`).
  `editor.cancel_job { id }` cancels it: the staging folder and the cooked files of the new map id
  are removed, so nothing is left. A cancel after the rename is too late and the job ends `done`.
- **The level.** The open level's `LevelSettings::terrain` names the terrain (one undo step;
  `terrain.level_terrain` reports it), and the terrain is built with a cook target, so
  `terrain.stroke` and the terrain tool work on it at once. `loaded` is false for a level that was
  never saved; it is built once the level has a path. `editor.undo` removes the level's reference
  and the built terrain; the files stay.

| Code | Meaning |
|---|---|
| `terrain_create_bad_params` | a missing `name`, a parameter of the wrong kind, `height_range` not `[min, max]` with min < max, a bad layer list (none, more than 8, a name twice), or a `sample_layout` other than `VertexCentered` or `CellCentered` |
| `terrain_create_bad_name` | the terrain or a layer name is not 1..=64 ASCII letters, digits, `_` or `-` |
| `terrain_create_bad_path` | the folder is not under `assets/`, has `..`, or leads out of the project through a symlink; or the heightmap path does |
| `terrain_create_exists` | `assets/<folder>/<name>` exists |
| `terrain_create_bad_size` | `size` or `cell_size` is not an allowed value, or the heightmap is not square at an allowed size, or `size` differs from it |
| `terrain_create_bad_heightmap` | the heightmap is missing, over the size limit, not a `.png`/`.r16`/`.raw`, or does not decode |
| `terrain_create_no_project` | the editor has no project open |
| `terrain_create_busy` | another create runs |
| `terrain_create_failed` | a write, the cook or the move into `assets/` failed (nothing is left) |
| `terrain_create_cancelled` | the job was cancelled (nothing is left) |

## Brush shoulder controls

The Terrain panel has keyboard-focusable **Size**, **Strength**, **Falloff** and **Falloff curve**
sliders. Tab reaches a slider; arrow keys adjust it. Size is the circle radius in meters
(rectangles also expose Length). Falloff is hardness: 0 fades from the center, 1 keeps full
strength to the edge. The viewport's inner ring marks the full-strength interior. The existing
`[` / `]` and `-` / `=` shortcuts change these same size and strength values. In the existing
Grass density and Scatter modes, Strength follows those modes' target-density shortcuts.
The curve selector applies to height/layer strokes and is disabled for holes, scatter and grass density.

`terrain.stroke` accepts `falloff: "smooth" | "linear" | "sphere" | "tip"`. Omission preserves
the existing smooth curve and its deterministic output. For inward coverage `t` after hardness:
Smooth uses the existing raised cosine, Linear uses `t`, Sphere uses `sqrt(t*(2-t))`, and Tip
uses `1-sqrt(1-t*t)`. The curve affects height and layer-weight strokes, including threaded
viewport strokes. An unknown curve or a non-string `falloff` returns the standard invalid-params
error (`F0002`); no stroke is applied.

## Terrain regions: editor.terrain_region

Enable **Region mode** in the Terrain panel and drag a rectangle on the terrain. The panel's
Column, Row, Width and Height fields provide the same selection entirely from the keyboard.
The paste destination is the cursor's sample column/row, also editable in Paste column/row.
**Copy region**, **Paste region**, **Export region** and **Import region** are focusable buttons;
Tab then Enter operates them. Quarter turns and Height offset control paste placement.

```json
{"method":"editor.terrain_region","params":{"op":"copy","rect":[10,20,8,4]}}
{"method":"editor.terrain_region","params":{"op":"paste","at":[30,20],"rotation":90,"height_offset":2.5}}
{"method":"editor.terrain_region","params":{"op":"export","rect":[10,20,8,4],"path":"assets/maps/region.png"}}
{"method":"editor.terrain_region","params":{"op":"import","rect":[30,20,8,4],"path":"assets/maps/region.png"}}
```

Parameters:

- `op` (required): `copy`, `paste`, `export` or `import`.
- `entity`: terrain entity id; default the selected terrain, otherwise the first terrain.
- `rect`: `[column,row,width,height]` in samples; default the panel selection. Both ends of a
  pointer-selected rectangle are included. The rectangle must be wholly inside the terrain.
- `at`: paste's minimum `[column,row]`; default the cursor/panel destination.
- `rotation`: clockwise degrees, exactly 0, 90, 180 or 270; default the panel's Quarter turns.
  A 90/270-degree rotation swaps width and height. Rotation permutes samples without interpolation.
- `height_offset`: meters added to pasted heights; default the panel value (initially 0).
- `path`: project-relative `.png`, `.r16` or `.raw` interchange path; default the panel's Region file.
- `endian`: RAW byte order, `little` or `big`; default the panel's Region RAW big-endian setting (initially off).

Copy keeps the exact floating-point heights and every byte weight plane in session memory.
Paste requires a destination with the same weight-layer count and grid dimensions as its own
terrain. It writes all changes as **one shared undo entry**, including overlapping copies and
rotations. A zero height offset copies float bits verbatim. Undo and redo restore original/final
bits, including weights. A paste that changes nothing adds no undo entry. The clipboard is not
saved with the project.

Export uses the existing 16-bit heightmap interchange encoding (map scale/offset, row 0 at −Z),
with clamp counts. RAW defaults to little-endian; PNG always uses its standard byte order. Import requires the file dimensions to match `rect`,
changes heights only, and is one undo step. Interchange regions must be at least 2×2 samples.
Interchange quantizes heights to 16 bits; the in-memory
clipboard does not. No layer weights are stored in these interchange files.

Replies: copy `{width,height,layers}`; paste/import `{changed,undoable:true}` (changed counts height
and weight values); export `{width,height,bytes,clamped_low,clamped_high}`. The maximum is 2^22 values
across heights and weight planes. The tool refuses oversized or out-of-bounds transfers, a missing
clipboard/terrain, mismatched weights, invalid parameters, unsafe paths and decode/I/O failures
with `terrain_region_invalid`. An active terrain stroke must finish before any region operation.
Rejected edits leave terrain data and undo/redo history unchanged. Dispatcher limits and permission
errors also apply; these methods require scene-edit permission.

Verification: `terrain_region` unit tests cover each non-square rotation, overlapping copy,
all weight planes, exact undo/redo, overflow/bounds rejection, offsets, and PNG/RAW interchange.
`terrain_kit_tools` drives the real dispatcher and the keyboard controls without a display.

## Edit layers (non-destructive)

A terrain can stack named **edit layers** over its base heights (ADR-7825). The terrain you see,
draw, collide with and cook is the base folded through every shown layer, bottom first; the
cooked heightfield holds that flattened result, so nothing changes at run time. Two blends:

- **add** layers add `opacity × delta`. Their order among themselves never changes a bit (the
  deltas of a run of add layers are summed in a fixed order before they are added).
- **set** layers move the height below them toward their own height where they were painted, by
  `coverage × opacity`. Their order matters: a set layer on top replaces the add layers under it;
  moved to the bottom, the add layers raise it.

Strokes (the editor's terrain tool, `terrain.stroke`, height imports) write the **active** layer:
the newest layer, the one picked with `terrain.edit_layer_set { active: true }`, or the base. A
stroke that lands is moved into the layer and its samples are rewritten from the fold, so hiding a
layer gives back exactly what the other layers made, bit for bit. A hidden, fully transparent or
locked layer refuses strokes. Showing, hiding, reordering, deleting or changing the opacity of a
layer recomputes only the samples that layer covers (`recomputed` in the reply,
`recomputed_samples` in `terrain.edit_layers`). In the editor every change and every layered
stroke is one undo step and restores the map and the layer bit for bit.

```json
{ "method": "terrain.edit_layer_create", "params": { "name": "Hills" } }
```

<!-- not-a-request -->
```json
{ "entity": 4294967297, "layer": 1, "active": 1, "recomputed": 0, "merged": 0,
  "layers": [ { "id": 1, "name": "Hills", "index": 0, "blend": "add", "enabled": true,
                "locked": false, "opacity": 1.0, "procgen": null, "bounds": null } ],
  "recomputed_samples": 0, "unsaved": true }
```

```json
{ "method": "terrain.stroke",
  "params": { "mode": "raise", "center": [120.0, -40.0], "radius": 12.0, "strength": 4.0,
              "edit_layer": "Hills" } }
```

`edit_layer` is a layer id, a layer name or `"base"`; without it the stroke writes the active
layer. (`layer` stays the painted material-weight layer.) The reply's `edit_layer` names the layer
written (`null` on a terrain without edit layers).

```json
{ "method": "terrain.edit_layer_set", "params": { "layer": "Hills", "enabled": false } }
```

```json
{ "method": "terrain.edit_layer_reorder", "params": { "layer": "Hills", "index": 0 } }
```

```json
{ "method": "terrain.edit_layer_flatten", "params": {} }
```

| Method | Does |
|---|---|
| `terrain.edit_layers` | the stack, bottom first: id, name, index, blend, enabled, locked, opacity, procgen (`{ graph, output }` or null), bounds; the active layer |
| `terrain.edit_layer_create { name, blend? }` | a layer on top (`add` by default, or `set`), made active |
| `terrain.edit_layer_set { layer, name?, enabled?, locked?, opacity?, active? }` | rename, show or hide, lock, opacity `0..=1`; `active: true` makes it (or `"base"`) the stroke target |
| `terrain.edit_layer_delete { layer }` | removes an unlocked layer; its samples go back to the layers below |
| `terrain.edit_layer_reorder { layer, index }` | moves a layer, 0 is the bottom |
| `terrain.edit_layer_flatten` | bakes every shown layer into the base and removes them (hidden ones stay); the terrain does not change by a bit. Enabled weight deltas are baked into the base weights |
| `terrain.edit_layer_weights { col, row }` | the folded material weights at one sample: `weights` (eight numbers) and `sum` (1) |
| `terrain.edit_layer_paint_weights { layer?, col, row, channels }` | sets the base weights or one layer's signed delta. A short `channels` array is padded with 0. The reply's `weights` are the folded channels and sum to 1 |
| `terrain.edit_layer_procgen { layer, graph?, output?, detach? }` | attaches a procgen graph and output, or `detach: true` clears it. Undoable |

```json
{ "method": "terrain.edit_layer_paint_weights",
  "params": { "layer": "Hills", "col": 0, "row": 0, "channels": [0.0, 1.0] } }
```

```json
{ "method": "terrain.edit_layer_weights", "params": { "col": 0, "row": 0 } }
```

```json
{ "method": "terrain.edit_layer_procgen",
  "params": { "layer": "Hills", "graph": "assets/hills.pgraph.ron", "output": "height" } }
```

```json
{ "method": "terrain.edit_layer_procgen", "params": { "layer": "Hills", "detach": true } }
```

Weights fold like this (ADR-8381). Start from the base (a missing sample is channel 0 at weight 1). Each enabled
layer with opacity above 0 adds `opacity ×` its signed delta, per channel. Clamp every negative channel to 0, then
divide by the sum. A sum of 0 is channel 0 at weight 1, the base material. An explicit all-zero base paint is stored
and is not the same as a missing sample; the fold of that paint with no deltas is still channel 0 at weight 1.

A layer with a procgen target is owned by that graph. `terrain.stroke` and `terrain.edit_layer_paint_weights` on it
return `edit_layer_owned` until `terrain.edit_layer_procgen` detaches the target. Publish from that graph writes only
the owned layer. If two layers name the same graph and output, the topmost wins. A generator with no target still
writes the base, as before.

In the editor the Terrain panel's **Edit layers** section does the same: Add layer (with Set
height layer for a set layer), a row per layer (top of the stack first) with select, Hide/Show,
Lock/Unlock, Move up, Move down, Delete and Opacity, a Name field and Rename for the active layer,
Base, and Flatten into base. An owned layer shows `owned by <graph> · <output>` and **Detach procgen**.
The active layer that is not owned has Procgen graph and Output fields and **Attach procgen**.
**Paint weight** writes channel 1 at sample (0, 0) and is disabled while the active layer is owned.
The panel shows the folded weights at (0, 0). Every control is reachable with Tab and works with
Enter, Space or the arrow keys. A level save writes `<map>.terrain.editlayers` version 2 beside the
level (`docs/formats/terrain-editlayers.md`); opening the level reads it back, including a version 1
file through the migrator. A file whose height fold no longer matches the cooked terrain (the terrain
was changed outside the layers) is refused, left on disk untouched, and the panel says why.

| Code | Meaning |
|---|---|
| `edit_layer_not_found` | no layer with that id or name |
| `edit_layer_no_terrain` | the entity has no terrain |
| `edit_layer_locked` | the layer is locked (strokes, delete and flatten are refused) |
| `edit_layer_hidden` | the layer is hidden or at opacity 0, so a stroke would not show |
| `edit_layer_limit` | already 32 layers |
| `edit_layer_bad_name` | a name is 1 to 64 printable characters |
| `edit_layer_bad_opacity` | opacity is in `0..=1` |
| `edit_layer_size_mismatch` | the layers were made for another heightfield size |
| `edit_layer_file_invalid` | the `.terrain.editlayers` file is malformed (editor) |
| `edit_layer_file_version` | the file is from a newer engine (editor) |
| `edit_layer_file_stale` | the terrain changed outside its layers since the file was written (editor) |
| `edit_layer_owned` | the layer is owned by a procgen graph; detach the target before a brush or a weight paint |
| `edit_layer_bad_weight` | `channels` has more than 8 numbers, or a number is not finite |
| `edit_layer_bad_target` | `graph` is not 1 to 256 characters, or `output` is not 1 to 64, or either is blank or has a control character |

## Unlayered height writers

Height patches, procedural publications, imports and direct map deformations are retained in
the edit-layer base (ADR-8202). Their height change is measured against the last composed
height, so toggling an Add layer does not erase it. A Set layer can cover a base change until
that layer is hidden. The editor reconciles direct writes before saving the v2
sidecar; named-layer strokes still use `terrain.stroke { edit_layer }`.

A host using the low-level map API should recompute the affected rectangle through
`TerrainEditLayers::recompute_terrain` after the write. This absorbs the change, updates the
composite and notes the dirty region for drawing and collision. Reconcile before directly
calling `flatten_into_base` or `to_bytes`; the world-level layer operations and editor save
perform that step automatically. That preservation adds no method parameters of its own.
On reopening cooked terrain, the loader recognizes its exact 16-bit cook/decode heights and
keeps the sidecar's authored precision. Other height mismatches still refuse the stale sidecar.

Material weights and a persisted procgen target are version 2 of the sidecar (ADR-8381). Paint
them with `terrain.edit_layer_paint_weights` and read the fold with `terrain.edit_layer_weights`.
Attach a target with `terrain.edit_layer_procgen`. A generator that carries `ProcTerrainOutput`
publishes into the matching layer and restores only that layer. A generator without that component
still publishes into the base and restores by the composite-value check.
