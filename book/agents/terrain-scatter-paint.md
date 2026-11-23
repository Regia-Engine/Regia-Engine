# Painted scatter (mesh instances with a brush)

A map's seeded scatter comes from its `.scatter.ron` ([terrain scatter](terrain-scatter.md)). Painted
scatter is the hand-placed half: a brush puts mesh instances of a `.scatter.ron` class where you
want them (ADR-1936). Painted instances go through the same field as seeded ones, so they get the
class's meshes, LOD chain, distance fade, shadow rules and impostors, and the frame renderer draws
them instanced.

A class meant only for painting has `density: 0.0`: the seed places nothing and the brush places
everything. Painted and seeded instances can also share a class.

## Paint

```json
{ "method": "terrain.scatter_stroke",
  "params": { "class": "pines", "center": [12.0, -30.0], "radius": 20.0, "density": 0.02,
              "scale_min": 0.8, "scale_max": 1.4, "slope_max": 0.6 } }
```

```json
{ "entity": 4294967301, "mode": "paint", "class": "pines", "added": 7, "removed": 0, "instances": 7,
  "undoable": true, "dirty": { "col_start": 90, "col_end": 121, "row_start": 66, "row_end": 96 },
  "cells": [[2, 2], [3, 2]] }
```

| Parameter | Meaning | Default |
|---|---|---|
| `class` | A class of the `.scatter.ron` (required to paint) | |
| `mode` | `paint` or `erase` | `paint` |
| `center`, `to`, `path` | Stroke start, then points to drag through, `[x, z]` in meters | |
| `radius`, or `shape: "rectangle"` with `width`, `length` | Brush size, meters (reach at most 4096) | |
| `density` | Target instances per square meter (above 0, at most 64) | 0.1 |
| `strength` | Fraction of the density placed, or of the instances removed, 0..1 | 1 |
| `hardness` | 0 fades from the center, 1 keeps full strength to the edge | 0 |
| `scale_min`, `scale_max` | Random multiplier on the class's scale | 1, 1 |
| `slope_min`, `slope_max` | Ground slope filter, radians from up | the class's |
| `height_min`, `height_max` | Ground altitude filter, meters | the class's |
| `min_distance` | Nothing lands closer than this to an instance of the class, meters | 0 (off) |
| `seed` | Picks the candidate grid | 0 |
| `wind` | Instances follow the scene wind when their mesh's material sways. `false` stores `PAINTED_FLAG_STILL` and the field draws the resting material | true |
| `entity` | Terrain entity | the first |

**Density is a target, not an amount.** The brush walks a jittered grid with one candidate per
`1 / density` square meters. Painting the same place again with the same brush adds nothing, and
`added` is 0; a soft edge fills in as the brush passes closer. Another `seed` is another grid: use
it to add a second layer on purpose, with `min_distance` to keep them apart.

The class's layer mask, exclusion zones and holes rule always apply. The brush replaces only the
slope and altitude filters. A candidate outside the map is dropped, and so is one whose scale is
past the float range.

A stroke is a function of the brush, the path, the terrain and the set's seed, so the same call
gives the same instances on every machine. Cutting a drag into several calls gives the same
instances as one call with a `path`, as long as `min_distance` is 0.

## Erase

```json
{ "method": "terrain.scatter_stroke",
  "params": { "mode": "erase", "class": "pines", "center": [12.0, -30.0], "radius": 6.0, "hardness": 1.0 } }
```

Without `class` the eraser takes every class. `strength` below 1 thins: a stable part of the
instances goes, and the same brush thins the same ones. The filters work here too, so
`slope_min: 0.9` clears a cliff and leaves the flat ground. An erase's filters default to open.

## Undo

In the editor every stroke, from the mouse or from `terrain.scatter_stroke`, is one undo step:
`editor.undo` and `editor.redo` restore the store bit for bit. `undoable` is false when the stroke
changed nothing.

Editor terrain tool: `9` is Scatter. `[` `]` brush size, `-` `=` density, `,` `.` class, Shift erases;
one drag is one undo step, and the instances appear as you drag.

## Sculpting under painted instances

Each instance keeps the ground height and normal it was seated with. A sculpt, an undone sculpt or a
heightmap import re-seats what it moved on the next frame, so nothing floats. Filters are not
re-checked: an instance on ground a sculpt made too steep stays.

## Check it

1. `terrain.scatter_paint` lists the paintable classes.
2. `seeing.screenshot` → `before.png`.
3. `terrain.scatter_stroke …`, then `play.step`.
4. `terrain.scatter_paint` shows the count; `terrain.scatter` shows `resident` and `emitted` grow.
5. `seeing.screenshot` → `after.png`, and `seeing.compare_images` on the two.

<!-- not-a-request -->
```json
{ "entity": 4294967301, "instances": 1420, "unsaved": true, "paintable": ["pines", "bushes"],
  "classes": [ { "name": "pines", "instances": 1420, "tiles": 6, "in_set": true } ] }
```

`in_set: false` means the `.scatter.ron` no longer names that class. Its instances stay in the store
and in the file, and are not drawn. The same goes for a mesh the class no longer lists.

## Save and load

| File | Holds | Written by |
|---|---|---|
| `<map>.scatterpaint.bin` | painted instances, per class and per tile (format 1, `docs/formats/terrain.md` § 10) | `terrain.save_scatter_paint`, the editor's Save |

`terrain.save_scatter_paint { path, entity? }` and `terrain.load_scatter_paint { path, entity? }` take
a map or level file (`maps/arena.level.ron` means `maps/arena.scatterpaint.bin`) or a
`.scatterpaint.bin` name, relative to the project root.

- A save writes atomically and replaces the file. It replies `{ entity, path, instances, bytes }`.
- A load replaces the terrain's store and seats it on the terrain as it is now. It replies
  `{ entity, path, instances, reseated }`. It is not an undo step.

The editor loads the file when a level opens, and every save writes it after an edit. A file that
exists and cannot be read turns scatter edits off, so it is never replaced.

The file is a raw asset: it ships next to the map, as `<map>.holes.ron` does.

## Picking the class (editor)

In the editor, the Terrain panel's Scatter brush section lists the level's scatter classes in a combo
box, with a Scatter mode toggle (key `9`) and a density slider. `editor.terrain_scatter_brush` does the
same for an agent:

```json
{ "method": "editor.terrain_scatter_brush",
  "params": { "class": "bushes", "density": 0.5, "enabled": true } }
```

All parameters are optional; with none it only reads. Every parameter of one call is validated
before any of them is applied: a call that fails changes nothing, so a refused pick never becomes
the class the next mouse stroke paints. The reply is
`{ enabled, classes: [name], class, class_index, density }`. `class` names one of `classes` (the same
class the `,` and `.` keys step through, and the one the next mouse stroke paints); before the tool
is used it names the first class, like the panel's picker; `density` is
instances per square meter, above 0 and at most 64; `enabled` true selects Scatter mode, false returns
to Raise. Errors: `scatter_brush_no_classes` and `scatter_brush_unknown_class` (`F0003`),
`scatter_brush_bad_density`, `scatter_brush_bad_class` and `scatter_brush_bad_enabled` (`F0002`).

## Error codes

| Code | Meaning | Fix |
|---|---|---|
| `scatter_paint_no_set` | No scatter set to paint from | Insert a `RuntimeScatter`, or a `ScatterPaintSet` for a host that draws its own field |
| `scatter_paint_unknown_class` | The brush names a class the set does not have | `terrain.scatter_paint` lists the classes |
| `scatter_paint_bad_brush` | A brush parameter or the path is out of range, or the class has no meshes | The message names the field |
| `scatter_paint_too_large` | The stroke walks more than 4 Mi candidates, or the store would pass 16 Mi instances | Lower the density or the brush |
| `scatter_paint_write` | The file could not be written | Check the directory under the project root |
| `scatter_paint_read` | The file could not be read | Pass an existing file |
| `scatter_paint_decode` | The file is corrupt or of a version this build does not read | Save it again from the map |

## Host contract

A terrain's instances live in its `TerrainScatterPaint` component. `TerrainPlugin` re-seats them after
height edits.

- With `RuntimeScatter` and `ScatterRuntimePlugin`, nothing else is needed: the frame hands the field
  the lowest-numbered terrain's store.
- A host that draws its own `ScatterField` inserts `ScatterPaintSet(Arc::new(set))`, so the brush has
  classes, and calls `field.set_painted(&paint.0)` before `field.update(eye)`. The call compares one
  generation number when nothing changed. Only the tiles a stroke changed are placed again.

A game loads the store with `load_scatter_paint(&scatter_paint_path(&map))`, or
`PaintedScatter::from_bytes` for bytes it read itself, and inserts `TerrainScatterPaint(store)`.

`terrain.scatter_stroke` with `wind: false` stores `PAINTED_FLAG_STILL` (flags bit 0) on every new
instance, and the field draws that mesh's swaying material at rest. Omitting `wind` leaves the
default `true`: flags stay 0 and the instance sways. The other flag bits stay 0. Spec 88.
