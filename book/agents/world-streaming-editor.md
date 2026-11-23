# World Map: load and unload streamed cells in the editor

The World Map panel (the **World Map** tab of the default layout's bottom group, panel id `world_map`,
`Panel::WorldMap`; `editor.layout_reset` restores the default layout) edits a streamed level one cell at a
time (ADR-7802). The runtime streams cooked cells ([World streaming and HLOD](world-streaming.md),
[Streaming runtime](streaming-runtime.md)); the editor loads the authored entities of the cells you
pick into the edit world, where they show in the Outliner and the Inspector like any entity, and
saves a cell's edits back to the entity files of that cell only.

What it needs:

- The level in its **directory form** (`<name>.level.d`, one file per entity; split a single-file
  level with `regia level split`). A save writes one entity file per changed entity, so the files of
  every other cell keep their bytes.
- A **cooked world index** in the cooked cache (`regia cook`). The index is the manifest: the grid,
  the world bounds and one record per non-empty cell. A cell's members are the guids of its cooked
  blob, so an entity placed after the last cook joins a cell at the next cook.

The map draws the manifest's cells (+X right, +Z down), shaded by each cell's recorded content
height, loaded cells in the accent color, cells the runtime would refuse outlined in the error color
with a cross, selected cells outlined, and the editor camera as a dot with its view cone. Click a cell
to select it, Ctrl or Shift click to add or remove one, drag a box to select every cell it touches.
The cell list beside the map is the keyboard route: Tab to a row, Up and Down move between rows,
Enter or Space selects or deselects the row's cell. Every button is reached with Tab.

Entities whose guid is already in the edit world (the level was opened in full with File ▸ Open
level) are not adopted by a cell: loading skips them (`skipped`) and unloading never removes them.
Unloading clears the undo history, as opening a level does.

## Open a world: `editor.world_open`

```json
{ "path": "levels/forest.level.d" }
→ { "open": true, "path": "/proj/levels/forest.level.d", "level": "…", "cell_size_m": 128.0,
    "bounds": ["-4_-4", "3_3"], "loaded": [], "job": null, "last": null,
    "camera": { "x": 12.5, "z": -3.0, "cell": "0_-1" },
    "cells": [ { "cell": "-4_-4", "x": -4, "z": -4, "entities": 12, "min_y": 0.0, "max_y": 31.5,
                 "terrain": true, "status": "ready", "reason": null, "loaded": false,
                 "spawned": 0, "skipped": 0, "unsaved": 0 }, … ] }
```

`path` is absolute or under the project. `cache_dir` overrides the cooked cache (default: the
project's cache for the host platform). A cell whose blob file is missing reads
`"status": "error", "reason": "cell_missing"`. Opening another world while cells of the open one are
loaded is refused (`world_cells_loaded`).

## Read the map: `editor.world_cells`

No parameters; answers the state above. `unsaved` counts the members of a loaded cell whose record
changed or that were deleted since the load. `job` is `{ id, cells }` while a load runs.

## Load cells: `editor.world_load`

```json
{ "cells": ["0_0", "1_0"] }
→ { …state…, "load_job": "world_load:3", "job": { "id": "world_load:3", "cells": ["0_0", "1_0"] } }
```

Pass exactly one of `cells` (labels `"<x>_<z>"`) and `radius_m` (every manifest cell within that many
meters of the editor camera, the panel's **Load around camera**). The load is a cancellable editor
job: poll `editor.world_cells` (or `agent.job_status { job }`) until `job` is `null`, or pass
`wait: true` to have it applied before the answer. Cells already loaded are skipped.

The worker reads each cell's blob, checking it as the runtime does (the record's hash, the format
version), then its members' entity files. **Cancel** (`editor.cancel_job { id: "world_load:<n>" }`,
the panel's **Cancel load**) at any time before the editor applies the load leaves the edit world as
it was: nothing spawns, the job ends `cancelled`. A failed read spawns no cell, marks the cell with
the reason (`cell_corrupt`, ...), fails the job with it and shows it in `last`.

## Unload cells: `editor.world_unload`

```json
{ "cells": ["1_0"] }                → error world_unsaved, data.cells ["1_0"]
{ "cells": ["1_0"], "save": true }  → { …state…, "unloaded": ["1_0"],
                                        "written": ["entities/c0/c0de…0101.entity.ron"],
                                        "deleted": [], "discarded": 0, "despawned": 1 }
{ "cells": ["1_0"], "save": false } → { …, "written": [], "discarded": 1 }
```

With unsaved edits, `save` absent or `null` refuses and changes nothing (the panel asks: **Save and
unload**, **Discard and unload**, **Keep loaded**). `save: true` writes each changed member's entity
file (temporary file + rename) and removes the files of members deleted in the editor; `written` and
`deleted` are relative to the level directory. A failed write unloads nothing.

## Error codes

Refusals answer `INVALID_PARAMS` with `data.reason` and `data.cells`:

| Reason | Meaning |
|---|---|
| `world_not_open` | No world is open in the World Map. |
| `world_needs_level_dir` | The level is a single `.level.ron`; split it into `.level.d` first. |
| `level_not_found` | No level at that path. |
| `world_not_cooked` | The cooked cache has no world index for the level: cook the project. |
| `world_index_corrupt`, `world_index_decode`, `world_format_newer` | The world index did not read. |
| `stream_settings_invalid` | The index's streaming settings do not validate. |
| `world_no_cache` | No project is open and no `cache_dir` was given. |
| `world_cells_loaded` | Unload the open world's cells before opening another. |
| `world_cells_empty` | No cells were given, or none is within `radius_m` of the camera. |
| `world_too_many_cells` | More than 4096 cells at once. |
| `world_radius_invalid` | `radius_m` is negative, not finite or covers too many cells. |
| `world_no_camera` | The editor has no camera to load around. |
| `world_cell_unknown` | The manifest has no such cell. |
| `world_cell_error` | The cell is marked with a runtime reason and cannot load. |
| `world_load_running` | A load is running: cancel it or wait. |
| `world_cell_not_loaded` | The cell to unload is not loaded. |
| `world_unsaved` | The cell has unsaved edits: pass `save: true` or `save: false`. |
| `world_save_failed` | An entity file could not be written or removed; nothing was unloaded. |
| `job_start_failed` | The load worker could not start. |
| `path_outside_project` | `path` (or the panel's Streamed level field) is not relative to the project or an absolute path under the project or the temp directory, or holds `..` or a symlink leading out. |

Job failures (`agent.job_status`, the cell's `reason`, `last`): the runtime's cell reasons
`cell_missing`, `cell_corrupt`, `cell_decode`, `cell_format_newer`, and `world_entity_file_missing`
(a member's entity file is gone from the level directory), `world_entity_file_invalid` (it does not
parse), `world_load_failed` (the worker stopped). A cell whose records do not spawn is listed in the
job result's `failed` with `world_spawn_failed` and stays unloaded.

Members are found by guid among the streamed level's entities, so a play session that restores the
edit world with new entity ids keeps every loaded cell and its unsaved edits.

## Tests

`cargo test -p regia_editor --lib world_edit world_panel`: a 3x3 fixture world loads two cells, an
edit in one is refused without a choice and saved with `save: true` changing only that cell's entity
file; a cancel before the apply spawns nothing; missing and damaged blobs show and fail with the
runtime's reasons; discard writes nothing; Load around camera picks the cells in reach; the keyboard
alone selects and loads a cell; the prompt's Discard and unload drops the edit.
