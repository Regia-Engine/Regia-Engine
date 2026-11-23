# .autotile.ron — Frozen auto-tiling rules v2

Spec: `docs/specs/86-2d-toolkit.md` (ADR-1889). Code: `regia_2d::autotile` (`AutoTileSet`). Example:
[`autotile.example.ron`](autotile.example.ron).

## Overview

An `.autotile.ron` holds the rules that turn "this cell is grass" into the right edge, corner or
inner tile of one tileset. It is an authoring file: painting with it writes ordinary GIDs into a
`.tilemap.ron` tile layer, and the map format does not change. Version **2** (v2 added the `WangCorner` layout, ADR-2597; every v1 file reads unchanged); a newer
version is rejected before the body is parsed; the loader runs the migration chain on older files and `regia upgrade-project` lists the format (`specs/12` § 4a). The shape is
hashed by `cargo xtask check-formats`.

## Conventions

- **Tileset**: `tileset` names a tileset of the map; every tile id is a *local* id in it
  (`gid − first_gid`), checked against its `tile_count` when the rules are bound to a map.
- **Membership**: a terrain's member tiles are every tile its layout can output, its `fallback`, its
  `variants` (keys and alternatives) and its `members`. A cell belongs to the terrain whose member set
  holds its local id (flip bits ignored). Member sets of different terrains must be disjoint.
- **Neighbours**: row 0 is the top row, so north is `y − 1`. `border: Same` (default) counts cells
  off the map as the same terrain; `border: Empty` counts them as empty.
- **Edge16**: 16 ids indexed by N = 1 + E = 2 + S = 4 + W = 8.
- **Blob47**: 47 ids. The 8-bit mask is N = 1, NE = 2, E = 4, SE = 8, S = 16, SW = 32, W = 64,
  NW = 128; a corner bit counts only when both edges next to it are set. Entry *i* is drawn for the
  *i*-th of the 47 reduced masks in ascending order: 0, 1, 4, 5, 7, 16, 17, 20, 21, 23, 28, 29, 31,
  64, 65, 68, 69, 71, 80, 81, 84, 85, 87, 92, 93, 95, 112, 113, 116, 117, 119, 124, 125, 127, 193,
  197, 199, 209, 213, 215, 221, 223, 241, 245, 247, 253, 255.
- **WangCorner**: 16 ids indexed by the corner mask TL = 1, TR = 2, BR = 4, BL = 8. A corner is set
  when the cell and the three neighbours that touch that corner (the two edge neighbours and the
  diagonal one) all belong to the terrain, so a lone cell is entry 0 and a cell inside a block is
  entry 15. `regia_asset_pipeline::tiled_wang` imports Tiled's corner, edge and mixed Wang sets as
  `WangCorner`, `Edge16` and `Blob47` terrains.
- **Rules**: the first rule whose `pattern` matches wins; with none, `fallback` (default: the first
  rule's tile). A pattern is nine cells row by row from the top (whitespace and `/` ignored): `#` the
  neighbour is this terrain, `.` it is not, `?` either. The centre is `#` or `?`.
- **Variants**: `variants: { id: [(tile, weight), …] }` replaces a resolved tile `id` (and its flips)
  with a weighted pick that depends only on `(seed, x, y, id)` (SplitMix64 finalizer), so a cell keeps
  its variant whatever the paint order.
- **Flips**: `flip_x`, `flip_y`, `diagonal` set the `.tilemap.ron` GID flag bits.

## Schema

```
AutoTileSet {
    version: u32,                         // 2
    tileset: String,                      // 1..=256 bytes
    seed: u32,                            // default 0
    border: Same | Empty,                 // default Same
    terrains: [Terrain],                  // 1..=256, unique names
}
Terrain {
    name: String,                         // 1..=256 bytes
    layout: Edge16([u32; 16]) | Blob47([u32; 47]) | Rules([PatternRule])   // Rules: 1..=1024
          | WangCorner([u32; 16]),
    fallback: Option<u32>,                // Rules only; default None
    members: [u32],                       // default []
    variants: { u32: [WeightedTile] },    // 1..=64 entries each; default {}
}
PatternRule { pattern: String, tile: u32, flip_x: bool, flip_y: bool, diagonal: bool }   // flips default false
WeightedTile { tile: u32, weight: u32, flip_x: bool, flip_y: bool, diagonal: bool }     // weight 1..=65535, default 1
```

## Migration note

v1 → v2 only adds the `WangCorner` layout, so a v1 file loads as is: `autotile_migrations` holds a
`VersionStep { from: 1, to: 2 }` that changes nothing but the number, and `regia upgrade-project`
rewrites the number in place (comments kept).

## Cooked form

`regia cook` turns a `.autotile.ron` into `regia_2d::CookedAutoTileSet` (type path
`regia_2d::CookedAutoTileSet`, postcard, payload version 1: the validated rule set). A shipped game
loads it from the pak with `AutoTiles::load_from_server` (agent: `tilemap.autotile_register` with
`path`) and gets the same `AutoTileSet` the raw file parses to.
