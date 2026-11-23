# Terrain source import limits

Inspect a heightfield with `terrain.inspect_source`; bake its overview with
`terrain.bake_overview`. Both accept schema-2 JSON or grayscale 8/16-bit PNG.
The [agent guide](../agents/terrain-source.md) explains the fields, datum and
placement. Import retains each count and converts heights with
`(f32(count) - f32(zero_counts)) * meters_per_count`. It does not resample terrain.

Set a smaller import budget for previews or restricted projects:

```json
{"method":"terrain.inspect_source","params":{
  "path":"terrain/source.json",
  "limits":{"max_bytes":1048576,"max_samples":65536,
            "max_json_depth":16,"max_unknown_values":1024}
}}
```

The result's `effective_limits` shows the applied ceilings. A bake reports them
inside `source.effective_limits`. The effective limit is the minimum of the
request, the host's project policy and the engine ceiling. Omitted fields use the
engine ceiling: 128 MiB encoded, 16,777,216 samples, JSON depth 32 and 65,536 ignored
metadata values/keys. Setting zero refuses any use of that budget; zero ignored
metadata still permits a document containing only recognized fields.

JSON counts are charged before decoding and storing each sample, even when
`counts` appears before `grid`. Dimensions use checked multiplication. Unknown
metadata is discarded within shared depth/work limits; it is never built into a
generic JSON tree. Duplicated recognized fields and malformed input are rejected.
The PNG reader retains its dimension/sample check before allocating pixels.

Errors beginning `height_source_limit_` identify the exhausted budget (`bytes`,
`samples`, `depth`, or `metadata`) and include `effective_limits`. Correct the
source or raise a lower request/project limit within the engine ceiling. A failed
bake preserves an existing preview, writes no new preview, and leaves scene
terrain unchanged. These limits do not change the cooked heightfield format.

Native hosts insert `regia_agent::TerrainSourcePolicy { limits }` into their
project's world after loading project settings. This resource is a host policy;
there is no new serialized project-file field or agent permission to raise it.
`ImportLimits::restricted_by` and `effective` expose the same clamping for Rust
callers. See the agent guide for observable work counters and cancellation.

## Cooking a count source (ADR-8471)

To keep a source's datum, scale, cell size and placement in a cooked terrain, put
the counts in the terrain's 16-bit `heightfield.png` and the mapping in the
`heightfield.json` beside it, with `"schema": 2`:

```json
{"schema": 2,
 "grid": {"cols": 33, "rows": 33, "cell_m": 0.5, "sample0_x_m": 37.25, "sample0_z_m": 37.25},
 "units": {"kind": "counts", "meters_per_count": 0.03125, "zero_counts": 20000}}
```

`regia cook` stores the counts unchanged and the mapping in the terrain index. A
level that names the terrain then places every sample at the source's world X/Z
and computes its height from the count exactly as `terrain.inspect_source` does;
the level's height scale and offset are ignored for such a terrain. Editing only
the datum, the scale, the cell size or the first-sample position recooks the
terrain. A `heightfield.json` without `schema` keeps working exactly as before.

Before you cook, preview the result:
`terrain.inspect_source { path: "…/heightfield.png", png: {…}, mapping: "…/heightfield.json" }`
reports the world bounds (`world_min`, `world_max`) and `runtime_map.loadable`. A
level loads a square grid whose center is the same number on X and Z; a
rectangular grid or an off-diagonal center cooks exactly but is refused at load with
`level_terrain_mapping`, rather than being stretched or resampled.

Older cooked terrain indexes (version 1) still load. `regia upgrade-project --write`
rewrites them as version 2 with no mapping and keeps a `.bak`; a run that fails on
any file changes nothing.

## Importing a source onto a terrain (ADR-8541)

**Terrain panel ▸ Import source** brings a count source (schema-2 JSON or 16-bit PNG,
with an optional `heightfield.json` mapping) onto the terrain of the open level:

1. Type the source path (and the mapping or PNG settings) and press **Preview**. The
   source is read in the background; the panel shows a progress bar and **Cancel**.
2. The preview lists the source's size, units and datum, its world bounds, the
   effective import limits, the terrain's bounds and height range, whether the
   mapping matches, how many samples change and whether the import is one undo step.
3. If the source's sample positions or heights do not fit the terrain, tick
   **Use the terrain's sample positions** or **Clamp heights to the terrain's range**:
   **Apply** refuses until you choose. A source of another size is refused; it is
   never resampled.
4. **Apply** changes the heights; **Cancel import** discards the preview and changes
   nothing.

If the source file or the terrain changed while the preview was open, Apply refuses
it; press Preview again. An import that changes more samples than the undo limit
(about four million) cannot be undone step by step: the preview says so, and you must
tick **Apply over the undo limit**. The editor first saves the original heights as a
16-bit PNG under the project's state folder (`terrain_source_backups/`); importing
that file with the Heightmap **Import** button restores them exactly.

**Reimport** repeats the last applied import of the terrain, with the same source path,
settings and choices, after you edit the source. Saving the level re-cooks only the
parts of the terrain that changed.

From a script, `regia terrain source-inspect <path>` prints the preview, and
`regia terrain source-import <path> --dest <terrain folder>/heightfield.png` writes the
source's counts and mapping as the terrain's cook input (`--dry-run` to preview,
`--adopt-source` to replace a different mapping).

## North-up, unequal spacing and missing data (ADR-8542)

Schema-2 JSON assumes one cell size, row 0 at the south edge, column 0 at the west
edge, sample-center placement and that every count is real data. When your raster
differs, describe it with a raster descriptor instead of editing the counts:

```json
{"raster_descriptor": 1, "spacing_m": [2.0, 0.5], "row0": "north",
 "registration": "edge", "origin_m": [100.0, -40.0], "nodata_counts": 65535}
```

- `spacing_m` gives X and Z their own spacing.
- `row0: "north"` (or `col0: "east"`) says which corner is stored first; the engine
  reorders samples exactly, it never interpolates.
- `registration` must be written: `center` when `origin_m` is the corner sample's
  position, `edge` when it is the corner of the cell around it (exports that treat a
  pixel as an area). With `edge`, every sample sits half a spacing inside.
- `nodata_counts` and `valid_mask` mark samples that hold no data.

Preview with `terrain.inspect_source { path, raster }` (a file path or the object
itself) and `terrain.bake_overview { path, raster, output }`. Missing samples show as
magenta pixels in the overview, are listed in the report and never become heights.
A runtime terrain refuses missing samples unless the host explicitly asks for holes
over them; fill the gaps in your source or cut holes on purpose. Rotated or sheared
rasters are refused: rectify them to north-up axes before import.

To cook such a raster, put the descriptor beside the terrain's `heightfield.png` as
`heightfield.raster.json` and keep the schema-2 `heightfield.json` for the datum and
scale. `regia cook` reorders north-first or east-first counts exactly and stores the
registered first sample. It refuses unequal X/Z spacing and missing samples for now
(a later terrain index version will hold them); editing the descriptor recooks.

## Large heightfields in several files (ADR-8595)

A single source file is limited to 128 MiB and about 16.7 million samples. For a larger
terrain, export it as tiles and describe them in `heightfield.tiles.json` beside the
terrain's `*.terrain.layers.ron` (instead of `heightfield.png`):

```json
{
  "terrain_source_tiles": 1,
  "grid": { "cols": 2049, "rows": 1025, "cell_m": 1.0, "sample0_m": [-1024.0, -512.0] },
  "units": { "meters_per_count": 0.0078125, "zero_counts": 32768 },
  "tiles": [
    { "path": "tiles/west.png", "origin": [0, 0], "size": [1025, 1025] },
    { "path": "tiles/east.png", "origin": [1024, 0], "size": [1025, 1025] }
  ]
}
```

- `origin` is the tile's first column and row in the whole grid; `size` is its columns
  and rows. Rows run south to north and columns west to east.
- Tiles may share a one-sample border (raise `max_overlap` if your tool repeats wider
  borders). Shared samples must be identical in both tiles.
- Every tile uses the descriptor's units and cell size. JSON tiles carry their own
  placement, which must match their origin; PNG tiles are placed by the descriptor.

Check a tile set before cooking with **Terrain ▸ Import source** (Preview) or
`regia terrain source-inspect maps/basin/heightfield.tiles.json`: the report
lists the tiles, how many samples they share, and any gap, overlap, misplaced tile or
disagreeing border by name. `regia cook` then cooks the tiles a region at a time;
when you change one tile, the next cook only rebuilds the part of the terrain that tile
covers. A failed or cancelled cook leaves the previously cooked terrain as it was.

## Overview images with a world mapping (ADR-8596)

An overview is a north-up shaded picture of a source for maps, briefings and review.
Terrain panel ▸ **Overview** bakes the source named in Import source: choose the
output PNG, **Keep aspect ratio (letterbox)** and the size, then **Bake overview**
(**Cancel overview** stops it; nothing is written). From a shell:

```sh
regia terrain source-overview assets/maps/source.json --output assets/maps/overview.png \
    --fit preserve_aspect --size 1024x512 --marker spawn=120,-40 --marker base=0,0
```

Beside the PNG goes `overview.overview.json`, which records what the picture was
baked from and how to read it:

- **Where a pixel is.** North is up and west is left. The `mapping` gives the world
  rectangle (`world_min`, `world_max`: the outer edges of the source's cells) and where
  it sits in the image (`content_px`). Pixel corners map linearly: the top-left corner
  of the image's source rectangle is (`world_min` X, `world_max` Z), and each pixel is
  `meters_per_pixel` wide.
- **Stretch or keep aspect.** The default (`stretch`, as before) fills the whole image,
  so a rectangular area in a square image is stretched. `preserve_aspect` keeps the
  shape and fills the rest with navy.
- **Colors.** Gray relief; magenta where the source has no data (a height of zero is
  gray, never magenta); red plus signs for markers.
- **Scale.** `legend.scale_bar_m` is a round length (1, 2 or 5 times a power of ten
  meters) and `scale_bar_px` its width in pixels; `elevation_range_m` is the height
  range the brightness spans.

Baking again with the same source and settings does nothing (the CLI prints
`"cached": true`); changing the source, its mapping or raster descriptor, a setting
or a marker bakes again. Overviews are for people: nothing in the game or simulation
reads them, and two machines may differ by one brightness step on a few pixels.
