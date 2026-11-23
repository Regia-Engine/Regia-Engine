# Exporting a map's bake input

A map's bake input is a `.bake.ron` description ([Baking GI from meshes](baked-gi-bake.md)). You no longer write
it by hand: `regia bake-gi --from-scene <level>` and the agent's `level.export_bake` build it from the level.
A stamp next to it tells you when the level has changed since the export. Design: ADR-310 and ADR-311.

## What the export reads

| From the level | Becomes |
|---|---|
| Every entity with `GiMobility::Static` and a `MeshSource` | One `meshes` entry: `id` = the entity's guid, `source` = the mesh path, `transform` = the world transform (parents included), `layer` = `scatter` if the entity is tagged `scatter`, else `setpiece`. |
| The `Static` mesh entity tagged `terrain` | `terrain_mesh` (`source`, `transform`, `spacing` from `--terrain-spacing`, default 1 m). At most one. |
| `<map>.holes.ron` next to the level (CLI only) | `terrain_mesh.holes`. |
| `LevelSettings.atmosphere` | `sun`, `sky`, `sky_bodies` (below). |
| The first `DirectionalLight` (by guid) | The sun when the atmosphere has no `time_of_day`. |

Skipped and counted in the summary (`skipped`, `skipped_by`, `skipped_entities`):

- `dynamic`: an entity with a mesh and `GiMobility::Dynamic` (or no `GiMobility`: the default is `Dynamic`).
- `no_mesh`: a `Static` entity without a mesh.
- `builtin_mesh`: a `Static` entity whose mesh is a built-in primitive (`builtin:cube`, ...). The bake only reads
  mesh files. Use a glTF instead.

Entities without a mesh that are not `Static` (cameras, groups, lights) are not counted. Prefab instances whose
template is not loaded are not expanded; the CLI summary lists them under `warnings`.

### Lights

- **Sun direction**: `--sun` when given, else the level's sun clock (`atmosphere.time_of_day`), else the
  authored directional light, else no sun. A sun below the horizon is dropped.
- **Sun strength**: on a planet with air, the illuminance and color the atmosphere passes to the ground. When the
  planet is airless, the top-of-atmosphere value (`sun_illuminance_lux` or the preset's). An authored light
  without a clock keeps its own color and illuminance. The sun's angular radius follows the distance its
  illuminance implies (0.00465 rad at 128 klx).
- **Sky**: none when airless. Otherwise one uniform radiance: the atmosphere's cosine-weighted sky radiance, so
  an upward-facing surface gets the same irradiance as under the real sky. The bake does not trace the sky's
  gradient.
- **Sky bodies** (`sky_body`, `moons`): a disc whose radiance times its solid angle is the body's planet-shine
  fill, tinted by its albedo.

Light values are rounded to 5 significant digits so the text is the same on every machine.

### Layout

- With a terrain: the terrain layer, 8 m columns, levels 1, 2, 4, 8 and 16 m.
- Without a terrain: one brick over the statics' bounds plus 4 m, at 4 m spacing (coarser if an axis would hold
  more than 64 probes). `--bounds minx,miny,minz,maxx,maxy,maxz` replaces those bounds. The level format has no
  bounds field (scene v10), so the override is a command-line option only.
- Automatic detail bricks over the `setpiece` placements and the interiors: 1 m spacing, 2 m margin, at most
  262,144 probes.

## Command line

```text
regia bake-gi --from-scene <LEVEL> [--from-static-meshes FILE] [--interiors FILE]
                [--project DIR] [--desc FILE] [--terrain-spacing M] [--bounds minx,miny,minz,maxx,maxy,maxz]
                [--sun elevation_deg,azimuth_deg,lux]
                [--check | --bake | --write-level] [--dry-run] [--out FILE]
                [--rays N] [--samples N] [--bounces N] [--seed N] [--probes-per-batch N] [--no-filter]
```

- The project root is the nearest folder above the level with a `Regia.toml`, else the folder above its
  `assets/` (`--project` sets it). Mesh paths are relative to it.
- The description goes to `<project>/bake/<stem>.bake.ron` (`--desc` sets it). `<stem>` is the name of the
  level's `look.baked_gi.volume` up to its first dot, else the level file's. It stays outside `assets/`, so it
  never ships. The stamp is `<stem>.bake.stamp.json` next to it.
- `--sun el,az,lux` replaces the level's sun: degrees above the horizon, degrees clockwise from north (+Z, with
  +X east, as the sun clock uses), top-of-atmosphere lux. It is stored with the stamp's options, so the agent
  uses it too.
- The default writes the description and the stamp only when something changed, then prints one JSON line:

```json
{"ok":true,"desc":"/p/bake/yard.bake.ron","stale":true,"statics":2,"skipped":2,
 "skipped_by":{"dynamic":1,"no_mesh":1},"skipped_entities":[...],"terrain":"assets/meshes/ground.gltf",
 "triangles":24,"bounds":{"min":[-0.5,0.5,-0.5],"max":[5.5,1.5,0.5]},"bounds_override":false,
 "sun_source":"time_of_day","sun":true,"sky":false,"sky_bodies":0,"bundle":null,"hash":"…","warnings":[]}
```

  `stale` is true when this run wrote a new description. `triangles` and `bounds` come from loading each mesh
  through the bake's loader.
- `--check` writes nothing. It prints `{"ok", "check", "desc", "hash", "stale", "reason"}` and exits **0** when the
  description is current, **2** when it is stale, **1** on an error (the same codes as `regia prepare --check`).
  `reason`: `desc_missing`, `stamp_missing`, `inputs_changed`, `desc_changed` (the file was edited by hand).
- `--dry-run` exports in memory, then validates and lays out the description with the bake's own reader. It
  writes nothing and needs no GPU. The line adds `layers` and `plan` as in `regia bake-gi --dry-run`.
- `--bake` bakes right after the export, the same code as `regia bake-gi <file>`, and writes `--out`, else
  `assets/<look.baked_gi.volume>`, else `assets/maps/<stem>.fgiv`. The line adds `volume` and `bake` (the bake
  summary), and the stamp records which input hash the volume was baked from.
- `--write-level` names the volume in the level's `look.baked_gi.volume` and keeps its overrides. The level is
  read and written back through the scene loader and writer, atomically. The file comes back in canonical form:
  its comments are not kept, and an older scene version is saved at the current one. Nothing is written when
  the level already names that volume.

### What makes it stale

The input hash covers the export rules' version, the options (terrain spacing, bounds, bundle, holes), every
exported entity's guid, mesh path, material path, layer and world transform (bit for bit), the content of every
mesh file (and a `.gltf`'s external buffers), the hole mask, the atmosphere settings and the authored sun. Moving
a static entity, editing a mesh or changing the time of day makes the description stale. Renaming the volume
(`--write-level`) does not.

## DO27 bake bundles

`--from-static-meshes <bundle>/static_meshes.json` adds a DO27 bundle, with or without a level. With a level the
map's look comes from the level, and without one there is no sun or sky (a warning says so).

- The bundle folder is the description's `root`. The description names `static_meshes.json`, `interiors.json`
  (next to it, or `--interiors`) and `MANIFEST.sha256` when present. The bake reads them itself: `source`
  glTFs, mirrored transforms with flipped winding, rock hints, skipped generated scatter and interiors
  (see [Baking GI from meshes](baked-gi-bake.md)).
- The exporter checks the files it reads first, with the bake's readers: the manifest hash of
  `static_meshes.json` and `interiors.json`, the header (`version`, `units`, `up`, `handedness`) and the required
  entry fields. Errors use the bake's codes and name the field (`static_meshes.json: header.version`).
- The terrain is the `terrain`-layer entry whose source is `terrain.*`, else the first `terrain` entry. Use
  `--terrain-spacing 1.25` for DO27's 1.25 m grid.
- A DO27 map keeps its sun in `map_look.json` (`atmosphere.sun`: elevation, azimuth, lux) in the sim frame. The
  bundle is in Regia Engine's frame (x negated), so pass `--sun <elevation>,<360 - azimuth>,<lux>`.
- Scene meshes are written with absolute paths when a bundle is the root. They lie outside the bundle, so the
  manifest does not check them.
- The summary adds `bundle_entries`, `bundle_placed`, `bundle_skipped`, `bundle_interiors` and
  `bundle_manifest_verified`. With a manifest, the input hash covers the manifest instead of every bundle file.

Export and bake Bore Twelve with the sky body from its level and a sun given on the command line (24° up,
azimuth 175° in Regia Engine's frame, 4.4 klx):

```bash
regia bake-gi --from-scene maps/bore_twelve.level.ron \
    --from-static-meshes <build-dir> \
    --terrain-spacing 1.25 --sun 24,175,4400 --dry-run      # check the layout first
regia bake-gi --from-scene maps/bore_twelve.level.ron \
    --from-static-meshes <build-dir> \
    --terrain-spacing 1.25 --sun 24,175,4400 --bake --write-level
```

## Agent methods

| Method | Params | Result |
|---|---|---|
| `level.export_bake` | `path?` (project-relative, no `..`, ends in `.bake.ron`) | The summary above plus `ok`, `desc`, `stale`. |
| `level.get_baked_gi` | none | Also `bake_input_stale`, `bake_input_hash` and `bake_input: { desc, hash, stale, reason }`. |

- `level.export_bake` exports the running game's open level (its live entities) to `path`, default
  `bake/<volume stem>.bake.ron` under the project root. It needs the `assets` permission. The options of an
  existing stamp at that path (terrain spacing, bounds, a bundle the CLI named) are kept, so the agent and the
  CLI agree on the hash. The agent loads no meshes, so `triangles` and `bounds` are `null`. Without a terrain,
  the description then has no volume brick. Export from the CLI for one.
- `level.get_baked_gi` computes the open level's input hash and compares it with the stamp at the default path.
  It reports `null` when the level names no volume. With no stamp it reports `stale: true` with
  `reason: "stamp_missing"`. When the export itself fails, `reason` is the error code. It reads every mesh file
  to hash it, so call it after an edit, not every frame.

Errors carry `data.reason`:

| `data.reason` | When |
|---|---|
| `export_no_level` | No level is open (no `LevelSettings`). |
| `export_path_invalid` | `path` is absolute, has `..` or `:`, names a Windows reserved device (`NUL`, `CON`, …), does not end in `.bake.ron` or leads out of the project, or the level names no volume and no `path` was given. Also a mesh, material, holes or glTF buffer path the level names outside the project, or a bundle file name that leaves the bundle (`data.entity` names it). |
| `export_mesh_missing` | A static entity's mesh file cannot be read. `data.entity` is `name (guid)`. |
| `export_terrain_ambiguous` | Two static meshes are tagged `terrain`, or the level has one and the bundle names a terrain. |
| `export_no_input` | No static mesh, no terrain and no bundle. |
| `export_read_failed` | A bundle file or the level file cannot be read. |
| `export_write_failed` | The description, the stamp or the level cannot be written. |

The CLI prints the same codes as `{"ok": false, "code", "entity", "message"}` and adds:

| `code` | When |
|---|---|
| `export_path_invalid` | The description would be written inside the bundle (read-only). Give `--desc` or `--project`. Without a level, the project defaults to the working directory. Also: the level's `look.baked_gi.volume` is absolute, has `..` or a drive prefix, or leads out of `assets/` through a symlink (ADR-1816; `--out` may name any path). |
| `export_level_parse` | The level does not load, or a rewritten level does not load back. |
| `export_mesh_load` | A mesh does not load through the bake's loader (bounds). |
| `export_bake_failed` | `--bake` failed (no GPU, a trace error, the volume could not be written). |
| `bake_*` | A bundle check failed: `bake_desc_invalid`, `bake_desc_parse`, `bake_manifest_mismatch`, `bake_manifest_missing_entry`, `bake_source_missing` ([Baking GI from meshes](baked-gi-bake.md)). |

## Workflow

1. Tag the map's set pieces, rocks and terrain `GiMobility::Static` and give the terrain the `terrain` tag.
2. `regia bake-gi --from-scene maps/x.level.ron --dry-run` and read `statics`, `skipped_entities` and the
   brick plan.
3. `regia bake-gi --from-scene maps/x.level.ron --bake --write-level`.
4. In CI or before a release: `regia bake-gi --from-scene maps/x.level.ron --check` fails with exit 2 when the
   map changed since its bake input was exported.
5. In a running game: `level.get_baked_gi` → `bake_input_stale`; `level.export_bake` to refresh the description.
