# Import reports: Aseprite and LDtk

`regia cook` imports Aseprite sprites (`.aseprite`, `.ase`) and LDtk worlds (`.ldtk`) into the
existing 2D assets — a cooked `SpriteAtlas` of per-tag strips, and a cooked tilemap (the merged
world plus one `level/<identifier>` sub-asset per level). Neither format needs a new runtime
asset, so everything the importer decided — frames, tags, layers, levels, and anything it dropped —
lands in `import_reports.json` in the cooked cache, one entry per source (ADR-3358). The agent
method `import.report` reads it.

The report is written by the cook: **run `regia cook` first** (an editor import cooks too). A
packaged game does not carry it.

## The method

`import.report` (read-only):

```json
{ "method": "import.report", "params": { "path": "assets/sprites/hero.aseprite" } }
→ {
    "path": "assets/sprites/hero.aseprite",
    "importer": "aseprite",
    "data": {
      "canvas": [32, 32],
      "frame_count": 4,
      "frames": [{ "index": 0, "duration_ms": 100 }, ...],
      "layers": [{ "name": "body", "visible": true, "tilemap": false,
                   "reference": false, "skipped": false }],
      "tags": [{ "name": "walk", "from": 0, "to": 3, "direction": "forward", "repeat": null }],
      "clips": [{ "name": "walk", "frames": 4, "duration_ms_total": 400, "repeat": null }],
      "sprites": [{ "name": "walk", "size": [128, 32], "pivot": [0.5, 0.5],
                    "nine_slice": null }],
      "palette_size": 32
    },
    "warnings": []
  }
```

`data` for an LDtk source instead lists `size`, `tile_size`, `tilesets` (name, `first_gid`,
`tile_count`) and `levels` (identifier, `world_x`, `world_y`, `size_px`, layer names, entity
count). `path` is project-relative; Windows separators and extension case are accepted.

Every feature the importer dropped or adjusted is one line of `warnings`: a non-Normal blend
layer, a reference or tilemap layer, a slice whose bounds vary inside a clip, per-tile opacity, a
background image, a level without a world position, tiles that did not resolve to a tileset, and
so on. The same lines are cook warnings in the log.

## Errors

| Code | Meaning |
| --- | --- |
| `F0002` (`import_bad_path`) | `path` is missing, empty, escapes the project, or is not an `.aseprite`/`.ase`/`.ldtk` source. |
| `import_no_report` | The cooked cache has no `import_reports.json`, or no entry for this path (cook first; a package does not carry the report). |
| `import_report_invalid` | The report file does not parse at the expected `format_version` — cook again with this build. |
| `F0016` | The app has no `AssetServer`. |

## Cook-side error codes

The importers refuse hostile input with documented codes (they fail the cook for that source):

- Aseprite: `aseprite_truncated` (the file ends early), `aseprite_bad_magic` (file or frame
  header), `aseprite_unsupported_depth` (not 8/16/32 bpp), `aseprite_limits` (a size, count or
  decompressed-pixel cap below), `aseprite_zlib` (a cel stream does not decode), `aseprite_structure`
  (contradictory structures, e.g. a tag covering frames the file does not have).
- LDtk: `ldtk_json` (the JSON does not parse), `ldtk_unsupported_version` (`jsonVersion` major is
  not 1), `ldtk_external` (an external `.ldtkl` level is missing, does not parse, or its path
  leaves the source folder), `ldtk_limits` (a cap below), `ldtk_structure` (contradictory
  structures, or the merged map fails `.tilemap.ron` validation).

## Limits

Aseprite: 256 MiB files; canvases to 16 384 px a side; 4 096 frames; 1 024 layers, tags and
slices; a cel decompresses to at most 64 Mpx, all cels together to 512 Mpx, the composited
frames (canvas × frame count) to 512 Mpx and all clip strips together to 512 Mpx; compositing,
linked cels included, to 4 Gpx of blends; a clip strip is at most 16 384 px wide; strings to
64 KiB; palettes to 65 536 entries. The `.meta` settings take the `.atlas.ron` bounds (`max_size`
16 to 16 384, `padding` and `extrude` at most 16); other values fail with `aseprite_structure`.

LDtk: 64 MiB files (project and each external level); 1 024 levels; 16 384 layers in total;
16 Mi tiles and 1 Mi entities and 4 Mi non-zero IntGrid cells in total; grids to 4 096 px; the
merged map to 65 536 tiles an edge and 64 Mi tiles; the tile-layer grids (one merged-map grid
plus one level grid per kept tile layer) to 1 Gi cells in total; identifiers to 1 024 bytes; tileset gids stay
below the flip bits (`0x20000000`). A level's pixel size must be positive, and a level's world
position plus that size must fit in a signed 64-bit integer. An external level path stays inside
the source folder.

## What the importers keep and drop

Aseprite: RGBA, grayscale and indexed pixel formats; raw, zlib-compressed and linked cels; layer,
group and cel opacity (multiplied down the group stack); hidden and reference layers skipped;
frame durations (falling back to the deprecated header speed, then 100 ms); tags expanded into
playback order — `forward`, `reverse`, `ping-pong` (`a b c b`), `ping-pong-reverse` (`c b a b`) —
with repeat counts; 9-patch and pivot slices attached to the clip they cover (slice bounds varying
inside a clip warn and use the first frame's key). Dropped with a warning: every blend mode other
than Normal, tilemap layers/cels/tilesets, user data. Output is one horizontal strip per tag in
the cooked `SpriteAtlas`, so a clip plays with the existing sheet animation: `AnimatedSprite` over
the strip with the clip's per-frame durations from this report.

LDtk: one level or a whole world in a single map — a level's tiles are written into the merged
cell grid at their world pixel, and the layer offset stays 0 so the tile batch does not add that
position again; tile, auto-layer and IntGrid layers (rule-placed tiles import as stored); LDtk's flip
bits become the GID flip bits; IntGrid values become a `<layer>_intgrid` Point-object layer with
the value as an `value` property, including a values-only IntGrid; entities become objects with their fields as properties
(`Color` becomes `#AARRGGBB`; arrays become their JSON text); a tile-rendered entity becomes a
Tile object; level fields become the (first) level map's properties. `Free` and `GridVania`
keep a negative `worldX`/`worldY`. Dropped with a warning:
per-tile opacity, background images, parallax, world depths other than 0, multi-world projects
(merged), linear-layout levels without a position (placed in a row), embedded LDtk icon atlases.
Two levels that share an identifier are `ldtk_structure` (each level is its own sub-asset).
Each level is also a `level/<identifier>` sub-asset map with level-local coordinates. The
level-local layer offset is only the layer's own pixel offset, not also baked into the cell.
