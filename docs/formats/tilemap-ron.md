# .tilemap.ron — Frozen tilemap v2

Spec: `docs/specs/22-tier2.md` § T2.10. Code: `regia_2d::tilemap` (`TileMapAsset`). Example:
[`tilemap.example.ron`](tilemap.example.ron).

## Overview

A `.tilemap.ron` is a tile map (orthogonal, isometric, staggered or hexagonal): tilesets (one image cut into a grid each), tile layers
and object layers. It is exactly what the Tiled importer (`regia_asset_pipeline::tiled_import`)
produces from a `.tmx` or `.tmj`, so a generator can target it directly, and `regia cook` accepts
`.tilemap.ron`, `.tmx` and `.tmj` alike (`.tsx`/`.tsj` tilesets are read through the maps that use
them). Version **2** (v2 added the `Isometric`, `Staggered` and `Hexagonal` orientations, ADR-2597;
every v1 file reads unchanged and `regia upgrade-project` rewrites its version number); a newer version is rejected before the body is parsed; the loader runs the
migration chain on older files and `regia upgrade-project` lists the format (`specs/12` § 4a).
The shape is hashed by `cargo xtask check-formats`.

## Conventions

- **Rows**: `data` is row-major with row 0 at the **top** (as in Tiled). In the world the map's
  origin (its entity's `Transform`) is the **bottom-left** corner and +Y is up.
- **Orientation** (`orientation`, default `Orthogonal`): cell `(x, y)` is always the column and row
  of the layer data; the orientation decides where it sits. `Isometric` is the diamond layout (the
  map box is `(w + h)·tw/2` by `(w + h)·th/2`); `Staggered(axis: Y|X, index: Odd|Even)` offsets
  every other row (`Y`) or column (`X`); `Hexagonal(axis, index, side_length)` is the same with a
  hexagon whose straight edge is `side_length` pixels (`Y` = pointy-topped, `X` = flat-topped; the
  step between rows is `(th + side_length)/2`). `tile_width` x `tile_height` is always a cell's
  bounding box and tile images draw from its bottom-left. Draw order is back to front: row by row
  (`X`-staggered maps by the top edge of the cell). Geometry: `regia_2d::grid`.
- **GIDs**: `0` = empty; otherwise `first_gid + local id` of a tileset. The top bits are Tiled's flip
  flags: `0x80000000` horizontal, `0x40000000` vertical, `0x20000000` diagonal (applied first);
  `0x10000000` (hex rotation) is masked off.
- **Tilesets**: sorted by `first_gid`, non-overlapping. `image` is relative to the map file.
  `spacing`/`margin` as in Tiled. `tiles` lists only tiles with data, sorted by `id`.
- **Collision**: per tile `Empty` (default), `Solid` or `OneWay` (jump-through: blocks only from
  above). The importer derives it from a `collision` string property (`solid`, `one_way`/`oneway`/
  `jump_through`), a bool `solid`/`one_way` property, the tile class, or — `Solid` — any collision
  shape drawn in Tiled.
- **Animated tiles**: `animation` frames of `(tile, duration_ms)`, looping.
- **Objects**: Tiled coordinates (map pixels, +Y down); shapes `Rect`, `Ellipse`, `Point`,
  `Polygon(points)`, `Polyline(points)`, `Tile(gid)`.
- **Properties**: `Bool`, `Int`, `Float`, `String`, `Color("#AARRGGBB")`, `File`, `Object(id)`.

## Schema

```
TileMapAsset {
    version: u32,                       // 2
    orientation: Orthogonal | Isometric | Staggered(axis, index)
               | Hexagonal(axis, index, side_length),   // default Orthogonal
                                        // axis: Y | X (default Y); index: Odd | Even (default Odd)
    width: u32, height: u32,            // in tiles
    tile_width: u32, tile_height: u32,  // in pixels
    background: Option<String>,         // "#AARRGGBB"
    tilesets: [TilesetDef],
    layers: [Tiles(TileLayer) | Objects(ObjectLayer)],   // bottom first
    properties: { String: PropertyValue },
}
TilesetDef { name, first_gid, image, image_width, image_height, tile_width, tile_height,
             columns, tile_count, spacing = 0, margin = 0, tiles: [TileDef] = [] }
TileDef { id, class = "", collision = Empty, animation: [TileFrame] = [], properties = {} }
TileFrame { tile: u32, duration_ms: u32 }
TileLayer { name, visible = true, opacity = 1.0, offset = (0, 0), data: [u32], properties = {} }
ObjectLayer { name, visible = true, objects: [MapObject], properties = {} }
MapObject { id, name = "", class = "", position: (x, y), size = (0, 0), rotation = 0,
            shape: ObjectShape, properties = {} }
```

Validation: positive sizes, a hexagon side length no larger than the tile size along its axis, every tile layer has `width × height` entries, every GID resolves,
tilesets sorted and non-overlapping, tile data sorted.

## Cooked form

`regia_2d::CookedTileMap` (type path `regia_2d::CookedTileMap`, postcard, own payload version):
the map plus one uncompressed RGBA8 image per tileset (pixel art stays exact; BCn would blur it).

## Importer limits

Finite maps of every Tiled orientation (`orthogonal`, `isometric`, `staggered`, `hexagonal` with
`staggeraxis`, `staggerindex` and `hexsidelength`). Object layers of non-orthogonal maps keep
Tiled's own object coordinates. Image layers, image-collection tilesets, list/class properties and text
objects are skipped with a warning (text → rectangle). zstd-compressed layers are not supported.
Tiled XML over 32 MiB or with more than 256 attributes on one tag is rejected (ADR "Accept
quick-xml 0.31 advisories under the Tiled importer"). The LDtk importer (`regia_asset_pipeline::
ldtk_import`, ADR-3358) targets the same structure: a whole `.ldtk` world (external `.ldtkl`
levels included) merges into one map with each level's world position as its layers' pixel
offsets, per-level maps are `level/<identifier>` sub-assets, LDtk flip bits land in the GID flip
bits, IntGrid values become a Point-object layer and entities become objects with their fields as
properties; see `book/agents/import-report.md` for the dropped features and the limits.
