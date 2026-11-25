# Author a terrain

<!-- index: Terrain -->
<!-- index: Heightfield -->
<!-- index: Terrain cook -->

A terrain is a heightfield the level names, plus the images and the layer file the cook reads.
This is the short path from an empty project to a terrain you can stand on. The chapters are
[Terrain, tunnels and scatter](../../user-guide/terrain-vfx.md),
[Terrain source import](../../user-guide/terrain-sources.md),
[Photoreal terrain](../../user-guide/terrain-photoreal.md), and
[Terrain data: cooking and streaming](../../user-guide/terrain-data.md).
The format is `docs/formats/terrain.md`. The runtime is `docs/specs/26-terrain-vfx-highend.md`.

Caves and overhangs are not a heightfield. They are a volume mesh, and the integer voxel
core in `docs/specs/122-voxel-core.md` is not in this version. See
[Volumetric terrain](../../user-guide/volume-mesh.md).

## 1. Create the terrain

In the editor, open the **Terrain** tab and choose **New terrain…** (or **Content ▸ New ▸
Terrain…**). Set the name, the folder under `assets/`, the sample count (129, 257, 513, 1025,
2049, or 4097 a side), the cell size, the height range, and the layer names. **Create** writes
the folder, cooks it, and names it on the open level. **Cancel** writes nothing. Save the level
so the reference stays.

The same files, if you would rather write them:

```text
assets/maps/valley/valley.terrain.layers.ron
assets/maps/valley/heightfield.png          16-bit gray
assets/maps/valley/heightfield.json         optional extents
```

`heightfield.json` with `"schema": 2` keeps the source datum, the cell size, and the first
sample's world position. A file without `schema` keeps the older extent fields (`size_m`,
`origin`). The layer file is the RON in `docs/formats/terrain.md` § 1. Current files are version
5; `regia upgrade-project --write` re-saves an older file at version 5.

The editor panel is [Terrain](../../user-guide/editor-guide/terrain.md). This how-to does not
describe the sculpt tool.

## 2. Check a height source before you cook

`regia terrain source-inspect` prints the same report as the agent method
`terrain.inspect_source`. Point it at schema-2 JSON, a grayscale 8-bit or 16-bit PNG, a
`heightfield.tiles.json` tile set, or a PNG plus its mapping:

```sh
regia terrain source-inspect assets/maps/valley/heightfield.tiles.json
regia terrain source-inspect assets/maps/valley/heightfield.png
```

Read the JSON. `effective_limits` is the ceiling that applied (the request, the project policy,
and the engine ceiling: 128 MiB, 16,777,216 samples, JSON depth 32, 65,536 ignored metadata
values). An error that starts with `height_source_limit_` names the budget that ran out
(`bytes`, `samples`, `depth`, or `metadata`). The import does not resample. A rectangular grid,
or a center that is not the same number on X and Z, can cook and then fail to load with
`level_terrain_mapping`.

A raster that is north-up in the file, or that uses a no-data count, needs a raster descriptor.
Unequal X and Z spacing and missing samples are refused at cook. The rules are in
[Terrain source import](../../user-guide/terrain-sources.md).

## 3. Cook

From the project root:

```sh
regia cook
regia cook --stats
```

`regia cook` treats every `*.terrain.layers.ron` as one terrain map. It does not send that
folder down the generic texture path. `--stats` prints the cooked size of each slot. Add
`--platform android` when the package is Android: the tiles are ASTC 4×4 instead of BCn, and
the budget counts that size. A texture slot must be a square power of two, at least 256 pixels
on a side.

`regia cook --stats` is the size check. It is not a picture. To see the ground, open the level
in the editor or run `regia run` on a level that names the terrain.

## 4. Name it on the level

**World settings ▸ terrain** should name the terrain you created. From an agent,
`terrain.level_terrain` reads that reference. A level that does not name a terrain has nothing
to sculpt and nothing for the shoreline plane to sit on.

## 5. Optional: a shoreline

A terrain carries at most one `WaterPlane`: a flat surface at a fixed level, with ripples, a
depth color, and a foam band. It is part of the terrain format. In the runtime frame the water
plugin draws it as a water body when terrain planes are on. Set it in the editor or with
`terrain.set_water`. Lakes, rivers, swell, and an FFT ocean band are water bodies, not this
plane. See [Terrain, tunnels and scatter](../../user-guide/terrain-vfx.md) and
[Water bodies](../../agents/water.md).

Rain and snow are not stored on the heightfield. They are the project weather, off until
`assets/config/weather.ron` enables them. A missing file leaves them off. A layer's `weather`
block says how that layer responds. See [Sky Lab](../../user-guide/sky-lab.md).

## Finished check

All of these are true:

1. `regia terrain source-inspect` on the heightfield exits 0. With a schema-2 mapping, the
   report's `runtime_map.loadable` is true. A false value means the grid is not a square whose
   center matches on X and Z, so a level will refuse it at load.
2. `regia cook --stats` lists the map and a total, and the cook exits 0.
3. The open level names that terrain (`terrain.level_terrain`, or World settings).
4. If you added a shoreline, `terrain.water` returns the plane, and a screenshot shows the
   waterline on the beach rather than a sheet through the hills.

A cook that fails leaves the previously cooked terrain as it was. Fix the source and cook again.
Do not hand-edit a cooked tile.
