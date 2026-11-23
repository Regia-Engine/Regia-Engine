# World Map

The **World Map** tab edits a large streamed level a few cells at a time. It draws the level's streaming grid; you
pick cells and load their entities into the edit world, where they appear in the Outliner and the Inspector like any
other entity. Saving a cell writes back only that cell's entity files, so the rest of the world keeps its bytes.
Depth: [World Map (agents)](../../agents/world-streaming-editor.md), [World streaming and
HLOD](../../agents/world-streaming.md).

## What it needs

- The level in its **directory form** (`<name>.level.d`, one file per entity). Split a single-file level with
  `regia level split`.
- A **cooked world index**: run `regia cook` first. The cook decides which cell each entity belongs to, so an entity
  you place after the last cook joins a cell at the next cook.

## Open it

The tab is in the default layout's bottom group (**View ▸ Reset layout** brings it back). From the keyboard press
Ctrl+Shift+P and pick **Open the World Map tab** (`window.open_world_map`). An agent brings it forward with
`editor.layout_apply {"open": "world_map"}`.

## Open a world

Type the streamed level's path in **Streamed level** (absolute or under the project) and press **Open world**. With
no world open the tab says "No world is open. Cook the project, then open a streamed level." You cannot open another
world while cells of this one are loaded.

## Pick cells

The map draws the grid with +X to the right and +Z down. Cells are shaded by their content height; loaded cells are in
the accent color; a cell the runtime would refuse (a missing or damaged cooked cell) is outlined in the error color with
a cross; the editor camera is a dot with its view cone.

- Click a cell to select it; Ctrl-click or Shift-click adds or removes one; drag a box to select every cell it
  touches. **Clear selection** empties the selection.
- From the keyboard, use the cell list beside the map: Tab to a row, Up and Down move between rows, Enter or Space
  selects or deselects the row's cell.

## Load and unload

1. **Load selected** loads the selected cells; **Load around camera** loads every cell within the radius next to it
   (in meters) of the editor camera. Loading runs as an editor job ("Loading (world_load:N)"); **Cancel load** stops it
   before anything is added, and the edit world stays as it was.
2. A cell that fails to read loads nothing and shows the reason; the other cells of the job are not loaded either.
3. **Unload selected** removes the selected cells' entities from the edit world. If you changed any of them, the tab
   asks: **Save and unload** writes each changed entity's file (and removes the files of entities you deleted),
   **Discard and unload** drops the changes, **Keep loaded** cancels.

Entities that were already in the edit world because the whole level was opened with **File ▸ Open level** are not
taken over by a cell: loading skips them and unloading never removes them. Unloading clears the undo history, as
opening a level does.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open a streamed level | **Streamed level**, **Open world** | `editor.world_open {"path"}` |
| Read the grid, the loaded cells and a running load | The map and the cell list | `editor.world_cells` |
| Load cells | **Load selected** | `editor.world_load {"cells": ["0_0"]}` |
| Load the cells around the camera | **Load around camera** | `editor.world_load {"radius_m"}` |
| Cancel a load | **Cancel load** | `editor.cancel_job {"id": "world_load:N"}` |
| Unload cells | **Unload selected**, then **Save and unload** or **Discard and unload** | `editor.world_unload {"cells", "save"}` |
