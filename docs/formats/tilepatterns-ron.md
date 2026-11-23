# .tilepatterns.ron — Frozen saved tile patterns v1

Spec: `docs/specs/86-2d-toolkit.md` § 6 (ADR-7853). Code: `regia_2d::tilepatterns` (`TilePatternLibrary`).
Example: [`tilepatterns.example.ron`](tilepatterns.example.ron).

## Overview

A `.tilepatterns.ron` holds the named patterns (multi-cell stamps) the editor's 2D mode saved for one
tileset. It sits next to the tileset's image with the image's extension replaced (`tiles/grass.png` →
`tiles/grass.tilepatterns.ron`, the path relative to the map's folder), so every map that uses the
tileset sees the same patterns. It is an authoring file: pasting a pattern writes ordinary GIDs into a
`.tilemap.ron` tile layer, and the map format does not change. Version **1**; a newer version is rejected
before the body is parsed; `regia upgrade-project` lists the format (`specs/12` § 4a). The shape is hashed
by `cargo xtask check-formats`.

## Fields

| Field | Type | Meaning |
|---|---|---|
| `version` | u32 | `1`. |
| `tileset` | string | The tileset's name (as in the map's tileset entry), 1 to 128 bytes. |
| `patterns` | list | At most 1024 patterns, in palette order. |
| `patterns[].name` | string | Unique in the file, 1 to 128 bytes, not blank. |
| `patterns[].width`, `height` | u32 | The size in cells; `width * height` is 1 to 1 048 576. |
| `patterns[].cells` | list of u32 | `width * height` cells, row by row, top row first. A cell is the tile's *local id + 1* (0 is an empty cell, left alone when pasted) with Tiled's flip bits on top (x `0x80000000`, y `0x40000000`, diagonal `0x20000000`). |

Local ids make a pattern independent of the tileset's `first_gid` in a map: the editor adds the map's
`first_gid − 1` when it pastes, and refuses a cell past the tileset's `tile_count`.
