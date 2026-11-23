# Startup asset work (cook skip, stamps, batched model loads)

What a dev start costs before the first frame, and how to check it. Design: ADR-530. Spec:
`docs/specs/12-assets.md` § 6.

## The cook before a dev run

`regia run`, `regia preview`, the editor's content panel and the live cooker cook the project
incrementally first. A glTF/GLB, image (PNG, JPEG, TGA, BMP, DDS, EXR, HDR, SVG, PSD), channel-pack recipe (`.texpack.ron`) or audio (WAV, OGG, FLAC, MP3) source is skipped **without running its importer** when
none of the files that importer reads changed since its last cook:

- the source and its `.meta`;
- for a `.gltf`, the buffers and images it references by URI and their `.meta` overrides;
- for a glTF, the project's `lod_classes.toml`;
- the platform, the importer version, and the build of the cooker. A rebuilt or updated engine imports
  everything once.

The source also needs its outputs still in the cooked cache. Other importers (material graphs, atlases,
tilemaps, scatter, baked GI, terrain) import and then compare their cache key, as before.

A file whose size and modification time match its last hash is not read again. A file modified less than
2 s before it was hashed is read again on the next cook.

## Checking it

Run the cook and read its summary line (stderr):

```text
$ regia cook --project game
cook: 0 cooked, 412 skipped, 0 errors, 0 non-cookable files skipped (38.0 ms; 412 skipped without import, 0.0 MiB hashed)
```

- On an unchanged project, `skipped without import` should equal `skipped` minus the sources of other
  importers. A glTF or PNG that is imported on every run points at an input that changes each time,
  such as a generator rewriting a `.meta` or a sidecar.
- `MiB hashed` is 0 when every input is older than 2 s and unchanged.
- Hosts read the same counts from `CookReport::imports_skipped` and `CookReport::hashed_bytes`.

To force a full import, run `regia cook --clean`. That is also the fix after a tool rewrote a file with
the same size and a restored modification time (`touch -r`, `cp -p` of other bytes). The keys and stamps
are a cache in `.regia/assetdb/cook-inputs-<platform>.bin`, and deleting that file is safe.

## Loading cooked models

`regia_render::cooked_model::load_cooked_models(&mut server, &gltf_ids)` loads several cooked glTFs in
one batch:

- the part lists and materials load through the server;
- then every mesh and every distinct texture decode together on the compute pool;
- each texture is read once, and a texture several models share is decoded once.

`load_cooked_model` is the one-model case. Meshes and textures loaded this way show `"state": "loaded"`
in `assets.status`, which reports where their bytes came from (they are marked with
`AssetServer::note_external_load`). They are not held in `AssetServer::textures()` / `images()`.

To measure a map load, use `pak_load_bench load --paks DIR` (sample `lookdev`). It prints
`PAK_LOAD {json}`, where `cpu_load_ms` is the batched load. For the boot side, use
`render.boot_timeline` ([instant load](instant-load.md)).

To retain authored LODs above zero and select generated index levels without rebuilding tangents,
use the additive [all-LOD cooked model loader](cooked-model-all-lods.md). It loads compact and
GPU-ready meshes (`REGIA_PAK_GPU_MESHES=1` desktop paks are its fast path) and shares GPU image
identities across the batch; this legacy loader keeps its behavior.
