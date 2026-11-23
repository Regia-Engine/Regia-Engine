# `cells.json` (format_version 1)

The cell index of `regia bake-lightmaps --scene <level> --cells` (`regia_pt::lightmap::cells::CellBakeIndex`,
ADR-3318). It sits in the cell directory beside the cell lightmaps, `<level lightmap stem>.cells/cells.json`
(default `assets/maps/<level>.cells/cells.json`), and records what each cell's bake read so a re-run skips a cell
whose inputs did not change. The runtime does not read it: a streamed cell finds its lightmap by name
(`<stem>.cells/<x>_<z>.flmap`, a normal `.flmap`, `docs/formats/lightmap.md`).

It is a bake cache. `regia upgrade-project` knows it (`lightmap-cells`, v1, suffix `cells.json`, a probe that skips
a `cells.json` that is not an index) but never migrates it: an index of another version is refused
(`lightmap_cells_version`), the user deletes it and every cell bakes again. An index whose `cell_size_m` or `ring`
differs from the run's is ignored (every cell bakes). The file is written atomically (temp file and rename) after
every cell file.

```json
{
  "format_version": 1,
  "level": "assets/levels/world.level.ron",
  "cell_size_m": 128.0,
  "ring": 1,
  "exposure": 0.125,
  "primary_hash": "5b8f…",
  "cells": [
    { "key": "0_0", "x": 0, "z": 0, "file": "0_0.flmap", "input_hash": "0f1e…", "entries": 41, "width": 512, "height": 384 }
  ]
}
```

| Field | Meaning |
|---|---|
| `format_version` | 1 |
| `level` | The level as given on the command line (informational) |
| `cell_size_m` | The level's `LevelStreaming.cell_size_m` at the bake |
| `ring` | `--cell-ring`: neighbor cells baked as occluders |
| `exposure` | The exposure every cell file was written at (`CELL_LIGHTMAP_EXPOSURE`, 1/8) |
| `primary_hash` | BLAKE3 hex of the always-loaded set's bake inputs (the primary file) |
| `cells[].key` | `"<x>_<z>"`; must equal `x` and `z` (`lightmap_cells_index` otherwise) |
| `cells[].file` | The cell's lightmap, relative to the index; no `/` and no `..` |
| `cells[].input_hash` | BLAKE3 hex of the cell's own inputs: its parts' meshes, transforms, materials and keys, the sun, sky, lights and media, and the bake settings (`threads` left out) |
| `cells[].entries`, `width`, `height` | The file's entries and atlas size |

A neighbor's edit does not change a cell's hash (only its own parts count), so editing one cell re-bakes one cell;
`--cells-force` re-bakes them all. Fuzzed by `regia_pt/tests/fuzz.rs` `lightmap_cells_index_never_panics`.
