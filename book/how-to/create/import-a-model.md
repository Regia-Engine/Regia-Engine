# Import a model

<!-- index: Import -->
<!-- index: glTF -->
<!-- index: FBX -->
<!-- index: OBJ -->
<!-- index: USDA -->
<!-- index: .meta | see: Asset id -->

Put a mesh the game can load into `assets/`, cook it, and confirm the cooked mesh exists.
The cook reads glTF directly. FBX and OBJ are converted to an in-memory glTF and then take
the same path. Settings, limits, and every other importer are in
[Importing assets](../../user-guide/importing-assets.md).

## Before you start

You need a project (`Regia.toml` and an `assets/` folder) and the `regia` command from the
same engine the project pins. A model file is one of:

| File | What you drop in `assets/` |
|---|---|
| `.glb` or `.gltf` | The model. Keep external images and buffers beside it, inside the project. |
| `.fbx` | Binary or ASCII, FBX 6.1 through 7.7. |
| `.obj` | The `.obj` and its `.mtl`. Textures are read from the model's directory only. |

Export from your DCC tool in meters, with +Y up, when the tool allows it. Use a `scale`
setting only to repair a file that was saved in the wrong unit.

## Steps

1. Copy the file under `assets/`, for example `assets/models/crate.glb`. Commit the source.
   Do not commit a cooked cache.

2. Optional: inspect the file without cooking. This writes nothing unless you pass `--out`.

   ```console
   $ regia import assets/models/crate.fbx --json
   $ regia import assets/models/crate.obj --scale 0.01 --out assets/models/crate.glb
   ```

   `--scale` is applied after the conversion to meters. `--no-animation` skips FBX animation
   stacks. `--sample-rate` (1 to 240, default 30) samples non-linear FBX curves. `--uv-set`
   chooses which FBX UV set becomes UV0. OBJ uses `--scale` only. A refusal prints a code
   such as `fbx_no_mesh` or `obj_parse` and exits non-zero. The same codes are on
   [FBX and OBJ import](../../agents/fbx-import.md).

3. Cook the project. The first successful import writes `assets/models/crate.glb.meta` (or
   the `.fbx` / `.obj` sidecar) next to the source. That file holds the asset id. Commit it
   with the source. Deleting it mints a new id and breaks references.

   ```console
   $ regia cook --project .
   ```

4. Optional: open the cooked model.

   ```console
   $ regia preview assets/models/crate.glb
   ```

   `preview` cooks incrementally, then shows that asset.

## Finished check

All of these are true:

- `regia cook --project .` exits 0. A line `skip … unsupported source extension` means that
  file is not a cooked asset. A line that is not a skip is a failed import: the code at the
  start of the message is the one to look up.
- The `.meta` sidecar exists beside the source and contains an `id`.
- `regia assets query --path 'assets/models/crate.glb*'` (after a cook) lists the model and
  its sub-assets, or `regia preview` draws the mesh.

A missing texture on an FBX or OBJ drops that slot with a warning. The import still succeeds.
The cook looks for the texture in the model's directory (the relative path, then the file
name) and does not follow `..` or an absolute path.

## Send the mesh back out as USDA

`regia export usda` reads a glTF **binary** and writes one USDA stage. It does not read
`.gltf`, `.fbx`, or `.obj`. Convert those first. The stage is an exchange file: it has no
`format_version`, and `regia upgrade-project` does not migrate it.

```console
$ regia export usda --glb assets/models/crate.glb --out build/exchange/crate.usda
```

A successful run prints the path, the byte count, the node count, and the mesh count, and
exits 0. The stage is an `Xform` named `World` (`metersPerUnit` 1, `upAxis` Y) with one
`Mesh` per triangle primitive. Materials, textures, skins, animation, cameras, lights, and
morph targets are not written. The texture `t` coordinate is copied as stored.

| Code | What happened |
|---|---|
| `usda_glb_missing` | The `--glb` path does not exist. |
| `usda_glb_invalid` | The file is not a glTF 2 binary, or a mesh value cannot be written. |
| `usda_empty` | There is no triangle mesh. The output path is left untouched. |
| `usda_write` | The stage could not be written. |

The field-by-field stage layout is on
[Exporting a glTF binary to USDA](../../agents/export-usda.md).

To export a level or a prefab, write a `.glb` first, then the stage:

```console
$ regia export glb --level assets/levels/room.level.ron --out build/exchange/room.glb
$ regia export usda --glb build/exchange/room.glb --out build/exchange/room.usda
```

## Where to go next

- Change import settings for a folder: [Import folder rules](../../user-guide/import-rules.md).
- Keep ids stable when the source is not in version control:
  [Asset ids](../../user-guide/asset-ids.md).
- LOD names, sockets, and material extensions:
  [glTF conventions](../../agents/gltf-conventions.md).
