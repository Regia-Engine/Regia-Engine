# Assets: what the running app can load

The runtime asset surface sits on the running [`AssetServer`]: it lists the cooked manifest,
reports load state, loads assets into the app and reloads them. Importing, creating, editing and
deleting asset *files* is editor/pipeline work (specs/12) and is not part of this surface.

## Listing and describing

```json
assets.list { "query": "helmet", "kind": "CpuMesh", "folder": "khronos", "limit": 200 }
→ { "assets": [{ "path": "assets/khronos/DamagedHelmet.glb", "id": "…uuid…", "type": "regia_asset::CpuMesh" }] }
```

`query` is a case-insensitive substring of the path, `kind` of the cooked type path, `folder` a
project-relative prefix. `assets.describe` and `assets.status` take an `asset` — a project-relative
path or an id — and report its manifest entry, load state (`not_loaded`, `loading`, `loaded`,
`failed`) and the cooked file size. Only `assets.status` adds `error` when the state is `failed`.
`assets.describe` answers `state: "failed"` and has no `error` field:

```json
assets.describe { "asset": "assets/khronos/DamagedHelmet.glb" }
→ { "path": "…", "id": "…", "type": "regia_asset::CpuMesh", "state": "loaded",
    "cooked_bytes": 1834567, "cooked_file": "cooked_….bin" }
```

MCP: the same entry is readable as the resource `regia://assets/{id}`.

## Loading and reloading

```json
assets.load   { "asset": "assets/khronos/DamagedHelmet.glb" }   → { "kind": "mesh", "state": "loaded" }
assets.reload { "asset": "assets/khronos/helmet.glb" }          → { "state": "loaded" }
```

A mesh answer also carries `"mesh": { "vertices", "triangles", "skin": { "joints", "influences" } | null,
"colors", "morph_targets": [names] }` (cooked mesh format v2), so an agent can check that a skinned
character kept its skin, colours and morph targets through the cook.

## Checking a skin against its skeleton

`assets.skin_check` skins a cooked mesh with a cooked skeleton on the CPU, the way the renderer
does (palette `global · inverse_bind`, the 4 strongest influences), in the bind pose and — with a
cooked `clip` — in that clip's pose at `time` seconds:

```json
assets.skin_check { "mesh": "pilot.glb#mesh/body/primitive_0", "skeleton": "pilot.glb#skeleton/hips",
                    "clip": "pilot.glb#clip/run", "time": 0.4 }
→ { "names_match": true, "missing_joints": [], "joints": 67, "skeleton_joints": 67,
    "vertices": 18234, "bind_offset_max": 0.00002,
    "bind_bounds": { "min": [-0.4, 0.0, -0.2], "max": [0.4, 1.82, 0.3] },
    "bounds": { "min": [-0.5, 0.0, -0.4], "max": [0.5, 1.78, 0.5] }, … }
```

- `bind_offset_max` is how far bind-pose skinning moves any cooked vertex. About 0 means the mesh
  and the skeleton agree on their frame; a large value means they were cooked from different files
  or importer versions (re-cook both).
- `bounds` is the posed extent in the skeleton's object space (metres, +Y up): a walking pilot
  stays about its height tall and near the origin. A pose that blows up or lies down is a frame
  or retarget problem.
- `missing_joints` lists skin joints the skeleton lacks (at most 16); those influences are left
  out. A mesh without a skin answers `invalid_params`.
- `stored_influences` is how many influences the cooked skin keeps per vertex (4, or 8 when the
  glTF has `JOINTS_1`). `draw_influences` is how many the renderer draws: 4 by default, 8 when
  the mesh was cooked with the `skin_influences: 8` importer setting (cooked mesh v3, ADR-1622;
  [skinning](skinning.md)). This check itself always uses the 4 strongest.

`load` decodes the cooked file into the app (mesh, texture or image — tried in that order);
`reload` re-reads cooked data, which is the hot-reload path after a cook. Both need the asset to be
cooked (`regia cook`); an unknown asset answers `F0016` with the list hint.

Loaded meshes are viewable with `seeing.screenshot` `target: "asset"` or
`seeing.contact_sheet`.

## Live reload status

In the editor and in `regia run --dev` a watcher re-cooks saved sources and reloads them in the
running app within a second (meshes, textures, materials, material graphs and instances, engine
WGSL, levels and prefabs, terrain layers and scatter, decals, VFX, audio, UI layouts).
`assets.watch_status` reports it:

```json
assets.watch_status { "limit": 2 }
→ { "watching": true, "roots": ["assets"], "debounce_ms": 120, "reloads": 7, "errors": 1,
    "last": "cook error in assets/crate.material.ron: 3:5: expected `)`",
    "recent": [{ "path": "assets/albedo.png", "kind": "texture", "ms": 95, "ok": true },
               { "path": "assets/crate.material.ron", "kind": "material", "ms": 4, "ok": false,
                 "error": "3:5: expected `)`" }] }
```

After writing a file, poll until `reloads` (or `errors`) grows, then look: `seeing.screenshot`.
A failed cook or compile keeps the previous version live, so the app never shows a broken asset.

## Texture memory and the budget

Every game preset streams material textures under a resident budget per tier
(`TextureStreaming::for_preset`: Android 256/512 MiB at a 1 K/2 K cap, Deck and Desktop Low 1 GiB
at 2 K, Desktop High 2 GiB at 4 K, Desktop Ultra 4 GiB at the authored size). A new material
uploads as much as the budget has room for, so frames match an unbudgeted renderer while memory
lasts; over the budget the least-recently-seen materials drop their top mips (a GPU copy, nothing
re-decoded) and the visible ones keep what their screen size needs. Once uploaded, the renderer
keeps no CPU copy of a texture's pixels.

```json
assets.texture_memory { "top": 3 }
→ { "frame": 912, "streaming": true, "budget_bytes": 2147483648,
    "resident_bytes": 1610612736, "streamed_bytes": 1577058304, "unstreamed_bytes": 33554432,
    "cpu_bytes": 0, "uncompressed": 212, "uncompressed_bytes": 1342177280, "tiny_uncompressed": 122,
    "uncompressed_limit": 64, "over_budget": false,
    "problems": [{ "code": "uncompressed_textures", "message": "212 uncompressed (RGBA8) material textures resident (1280 MiB, limit 64): cook PNG/JPEG sources … largest: M_Hull base_color 4096×4096 …" }],
    "wrong_platform": { "decoded": 0, "decoded_bytes": 0, "unbound": 0, "formats": [], "labels": [] },
    "textures": [{ "material_key": 4211, "name": "M_Hull", "slot": "base_color",
                   "width": 8192, "height": 8192, "resident_width": 4096, "resident_height": 4096,
                   "format": "Rgba8UnormSrgb", "compressed": false, "streamed": true,
                   "gpu_bytes": 89478485, "cpu_bytes": 0, "last_seen_frame": 912 }, …] }
```

`name` is the material's asset name (materials built by `LabMaterial::from_authored_resolved` or
`from_cooked` are named; a host names its own with `regia_render::name_material_textures`).
`budget_bytes` is `null` with streaming off. The same two problems — `texture_budget_exceeded`
and `uncompressed_textures` (more than `uncompressed_limit`, default 64, RGBA8 material textures:
the level ships PNG/JPEG instead of a cooked pak) — appear in `render.problems` and as differences
in the host checklist, naming the five largest offenders. Since v0.5.1 textures of at most 4×4
texels (placeholders) are not counted in `uncompressed`, never raise the problem and are never
named in it: `tiny_uncompressed` counts them. The limit is `TextureMemoryReport::uncompressed_limit`
(`None` = 64), so a host checking its own report can hold a level to a tighter one.

`wrong_platform` counts the block-compressed textures (material maps and terrain layer arrays)
that reached a device which cannot sample their format since the process started: BC on a phone
(decoded to RGBA8 on the CPU, 4–8× the memory and a slow load), or ASTC LDR and ETC2 on a desktop
GPU (decoded the same way; an ASTC block larger than 4×4 expands further, about 9× for 6×6, 16×
for 8×8 and 36× for 12×12). HDR ASTC, BC6H and signed EAC stay unbound. Either means the content
was cooked for another platform; `regia cook --platform
android` writes ASTC. The problem `textures_cooked_for_other_platform` appears in `render.problems`
and the host checklist while any are counted.

## Faster loads: pak environment and tools

- `REGIA_PAK_GPU_MESHES=1` at pack time stores each mesh GPU-ready (`regia_asset::gpu_mesh`, magic `FGPM`,
  layout v3). Opt-in, desktop only, never Android. A mesh with vertex colors converts and `load_mesh` returns its
  `COLOR_0` (ADR-7919); a mesh with a skin or morph targets converts too and `load_mesh` returns them (layout v3,
  ADR-10080; v1 and v2 entries still load).
  Old compact paks still load; `load_mesh_bytes` reads both forms.
- `pak_migrate <old.fpak> <new.fpak> [--raw]` (a binary of `regia_asset_pipeline`) rewrites an existing desktop pak
  with its meshes GPU-ready, every other entry byte for byte; it prints how many meshes it converted.
- `REGIA_UPLOAD_RING=0` turns off the mapped upload ring and falls back to `Vec` + `write_buffer` chunks. Output
  is identical; use it to rule the ring out when a frame hitches during streaming.
- `REGIA_PAK_VERIFY=0|force` controls the background `pak_verify` check. Until a pak passes it (first launch
  after an update) its reads copy through the file handle instead of serving views of the mapping. The terrain
  `PakTileSource` opens its own paks and follows the same rule (`pak_verify::gate_until_verified`): an unverified
  pak serves it copies until the next launch. `PakReader::read_raw` goes through the same check.
- On Windows a mounted pak cannot be opened for writing by another process (the reader withholds write sharing), so
  a patcher writing it in place gets a sharing error instead of changing bytes under the game.
- A mounted pak that is truncated or re-stamped while the game runs is detected on the next read: the log shows
  `pak changed on disk` (target `regia_asset`, with the pak path) once, and that pak is read through the file
  from then on. A cut entry then fails with `PakError::Corrupt` instead of crashing. Replace paks by rename.
- `assets.status` also reports the mounted paks and how many serve zero-copy views now:

  ```json
  assets.status { "asset": "assets/khronos/helmet.glb" }
  → { "path": "…", "id": "…", "state": "loaded", "cooked_bytes": 1834567,
      "paks": { "mounted": 2, "zero_copy": 2 } }
  ```

  `zero_copy` below `mounted` means a pak is still waiting for its first verify pass (first launch after an
  update), failed it, or changed on disk; `AssetServer::zero_copy_pak_count()` is the same count for hosts.

## Generating materials from texture sets

```json
assets.texture_sets { "folder": "assets/textures/rocks", "write": true, "recursive": false }
→ { "folder": "assets/textures/rocks", "write": true,
    "sets": [{ "name": "boulder", "folder": "…", "material_path": "…/boulder.material.ron",
               "material_exists": false, "written": true,
               "textures": { "base_color": { "path": "…", "id": "…uuid…",
                                             "settings": "(usage: Some(color))" }, … },
               "alpha_mode": "mask",
               "unreferenced": [{ "path": "…/boulder_Opacity.png",
                                  "reason": "the standard material reads alpha from the base colour map's alpha channel; …" }],
               "conflicts": [], "warnings": [] }],
    "leftovers": [{ "path": "…", "reason": "unknown_suffix" }] }
```

`assets.texture_sets` is the agent surface of `regia assets texture-sets` (spec 22 T2.14,
ADR-3320). It detects PBR texture sets — maps named `<base>_<suffix>.<ext>` grouped by folder
and base name over one shared suffix table (`BaseColor`/`Albedo`/`Diffuse`/`Color`, `Normal`/
`N`/`Nrm`/`NormalGL`/`NormalDX`, `Roughness`, `Metallic`, `AO`, `ORM`/`ARM`/`RMA`/`MRA`,
`Height`/`Displacement`, `Emissive`/`Emission`, `Opacity`/`Alpha`/`Mask`) — and generates one
`.material.ron` per set that names the set's texture ids. Without `write` (the default) nothing
is written and texture ids that have no `.meta` yet read as `null` ("new on write"). An existing
material is never touched (`material_exists`); two files claiming one slot are reported as a
conflict and neither is used.

With `write: true` the members' `.meta` settings are set so cooks agree: base colour and emissive
sRGB, other maps linear; a `_NormalDX` normal records `normal_green: directx` (flipped at cook,
never on disk); an `_RMA`/`_MRA` packed map records `orm_channel_order` (swizzled to `ORM` at
cook). An opacity map switches the material to alpha cutout. What the standard material has no
slot for is reported, never guessed: separate roughness/metallic maps (until the packed-ORM
packer lands) and the opacity map itself (the shader reads alpha from the base colour map's
alpha channel) appear in `unreferenced` with the reason.

The suffix table and the default green convention extend per project in `Regia.toml`
`[import.texture_sets]` (see [Materials, textures and import](../user-guide/materials.md#texture-sets-a-folder-of-maps-becomes-a-material)).

Errors: `F0002` (`folder` missing/invalid or escaping the project — `data.reason`
`path_outside_project`), `F0003` (a write failed — `data.path` names the file), `F0016` (the
folder does not exist under the project).

## Generic assets: loaders, handles and load groups

A crate that owns an asset type registers an `AssetLoader` for it (`AssetAppExt::register_asset_loader`
on the app, or `AssetServer::register_loader`). `AssetServer::load::<T>(id)` and `load_path::<T>(path)`
return a reference-counted `StrongHandle<T>` at once and decode on the IO task pool, reading through the
same mounted paks and cooked cache as the mesh and texture loads. A loader loads what it depends on
through its `LoadContext` (`ctx.load::<D>(id)` returns a handle); the server records the edges both ways,
so a hot reload of a dependency reloads every asset that uses it (each reported as
`AssetReloaded { kind: Generic }`).

When the last strong handle drops, the asset unloads after a grace period counted in frames (default
120, `set_unload_grace_frames`; 0 unloads on the next frame); taking a handle again inside the grace
keeps it. Unloading drops the value and with it the handles it held, so its dependencies then run their
own grace. A per-kind CPU budget (`set_memory_budget`) unloads released assets early, oldest release
first; an asset that still has a strong handle is never evicted. A load group (`load_group`) keeps its
members loaded and is a load set named `load_group:<n>`, so `assets.load_progress` shows it beside the
hand-made sets.

```json
assets.handles { "kind": "Note", "limit": 100 }
→ { "grace_frames": 120,
    "kinds": [{ "kind": "Note", "loaded": 2, "loading": 0, "strong_handles": 2, "pending_unloads": 1,
                "pinned": 0, "cpu_bytes": 8, "gpu_bytes": 0, "budget": null }],
    "assets": [{ "id": "…uuid…", "kind": "Note", "strong": 0, "unload_in_frames": 6, "cpu_bytes": 5,
                 "gpu_bytes": 0, "cpu_data_kept": true, "dependencies": 0, "pinned": false }, …],
    "truncated": false,
    "groups": [{ "name": "load_group:0", "loaded": 1, "failed": 0, "total": 1, "fraction": 1.0,
                 "complete": true, "failures": [] }],
    "cycles": [["…uuid a…", "…uuid b…"]],
    "warnings": ["reference cycle: …uuid a…, …uuid b… hold each other and never unload by count"] }
```

- `kind` matches a loader's kind name (its asset type without the module path), case-insensitively;
  omit it for every kind. The built-in kinds are `mesh`, `texture`, `image` and `material`; they are
  listed only while they hold or load something. An unknown kind, or a built-in kind that holds nothing,
  is an `invalid_params` error that lists the known kinds.
- `pinned` marks a built-in asset a typed load (`load_mesh`, `load_texture`, `load_image`,
  `load_material`) stored: its uncounted `Handle<T>` keeps it, so it never unloads by count and keeps
  its CPU copy (`strong` counts only the counted handles beside it). A counted load
  (`load::<CpuMesh>`, ...) unloads after the grace like any generic asset (ADR-7256).
- `strong` counts live handles, including those held inside other assets and by load groups.
  `unload_in_frames` is set only for an asset with no strong handle left; it unloads when it reaches 0.
- `gpu_bytes` is the estimate a registered `PrepareAsset` returned (the step that uploads a GPU-backed
  asset); after it the CPU copy is released (`cpu_data_kept: false`, `cpu_bytes` drops) unless the load
  asked for `keep_cpu_data` (`AssetServer::load_with`).
- `groups` lists every live load group (not filtered by `kind`); `complete` is set once, when every
  member loaded or failed, and the app gets one `LoadGroupCompleted` message for it.
- `cycles` lists reference cycles among loaded assets (A loads B through its context, B loads A), each
  as its member ids in order, with one `warnings` line each. A cycle keeps its members loaded after
  every outside handle drops; break it in the loader (a weak handle, or no edge back). Not filtered by
  `kind`.
- Budgets (`AssetServer::set_memory_budget`) hold CPU plus GPU bytes per kind, so a texture whose CPU
  copy went after its upload still counts by its GPU estimate. `assets.texture_memory` stays the
  renderer's own report of the textures it uploaded itself; a texture is counted in one of the two.
- Counted built-in loads count in the `loads` block of `assets.cooked_pack` (mesh form, `textures`,
  `images_rgba8`, `pak_loads`, `cache_loads`) exactly as typed loads do, and a counted texture loaded
  from a pak that serves views keeps a view of its KTX2 bytes (no copy). A counted texture whose cooked
  file became an RGBA8 image (or back) is listed under its new kind after the hot reload; its handles
  still count it, and `load::<CookedImage>` shares the count (ADR-7284).
- A cooked model loaded with `regia_render::cooked_model::load_cooked_models_counted` shows one `mesh`
  row per part. With `AssetMeshUploads` registered its `gpu_bytes` is the upload, `cpu_bytes` 0 and
  `cpu_data_kept` false: the frame renderer draws it from the upload (`FrameRenderer::set_asset_meshes`)
  and the budget counts it once (ADR-7284).
- Errors: `F0016` when the app has no asset server. Load errors carry stable codes in their text and
  through `AssetLoadError::code`: `asset_not_found` (the manifest lists no such path), `asset_no_loader`
  (no loader for the type), `asset_type_mismatch`, `asset_io`, `asset_decode`.

The built-in mesh, texture, image and material stores keep their `Copy` slot handles (`Handle<T>`, no
reference count) for the typed loads, which pin what they load; counted loads of the same kinds go to the
same stores (ADR-7256). With the `AssetPlugin` they also send `AssetEvent<T>` messages (`Added`,
`Modified`, `Removed`, `LoadFailed`).

## Counted meshes and textures in an App (ADR-7330)

Add `regia_render::CountedMeshPlugin` next to `RenderPlugin` and `AssetPlugin`, before the first counted
load. It registers the renderer's uploaders (`CountedAssetUploads`: meshes, and KTX2 textures plus RGBA8
images) with the asset server and the runtime renderer. Then:

- `MeshRenderer::counted(server.load::<CpuMesh>(id)?, material)` draws the mesh from its GPU upload. The
  component and the extracted frame hold no vertices; `assets.handles` shows the mesh row with
  `gpu_bytes` and `cpu_bytes: 0`. Despawning the last entity that draws it lets the mesh unload after the
  grace frames.
- A material slot `MaterialImage::Asset(AssetImage::image(handle, &uploads.textures))` (or
  `AssetImage::texture` for KTX2) binds the one GPU texture of that asset, whatever number of materials
  hold it. Each slot samples it in its own color space (ADR-8610: one texture, an sRGB and a linear
  view), so one image may serve a base-color slot and a mask slot. `uploads.textures.set_srgb(id, true)`
  before the load filters the mip chain for color; the base level is exact in both views.
- A material-graph texture slot takes one too (`GraphTexture::from_asset(image, srgb)`), and
  `FoliageSway::adopt` keeps a counted map in the sway twin's graph slots (ADR-8610).
- A hot reload of a counted texture shows on the next frame in every material that binds it, without
  touching the material: `render.stats` `counted.texture_rebinds` counts the entries rebuilt, and
  `counted.textures` reports the App's counted texture table ([Skinned instancing and draw
  counters](skinned-instancing.md)).
- A skinned `MeshRenderer` (with `SkinJoints`) on a counted mesh skins from the upload; the joints stay
  CPU (`render.stats` `mesh_cache.adopted_skinned_meshes`).
- A counted mesh with a `MeshUv1` (one entry per vertex) or a `LightmapReceiver` whose unwrap is 1:1
  with the mesh draws its second UV set. `render.get_lightmaps` reports the receivers it refuses with
  `lightmap_receiver_counted` or `lightmap_receiver_counted_pending`
  ([Lightmaps](lightmaps.md#errors)).
- `render.stats` `mesh_cache` reports the counted meshes the renderer draws from the asset server's
  buffers apart from its own cache: `adopted_meshes`, `adopted_mesh_bytes`. The cache never evicts
  them; the asset server frees them.

- A counted mesh with a `MeshLodChain` draws every level from the upload (ADR-7428): read the chain with
  `MeshLodChain::from_cooked_asset(&server.payload_reader(), id)` (no vertex kept) and insert it next to
  the counted `MeshRenderer`. Each level's index list uploads once, shared by its colour, shadow and
  cross-fade draws: `render.stats` `counted.lod_index_uploads` counts them ([Skinned instancing and draw
  counters](skinned-instancing.md)).

- **Visibility-buffer clusters** (ADR-7462): on a profile with the visibility buffer, a counted mesh
  draws its clusters from the upload. Give it a `MeshletSource` like any mesh: the meshlet bridge
  checks the `.fmlt` against the upload's counts and inserts it under the stand-in's key. Its vertices
  are copied into the cluster arena on the GPU, once. `render.stats` `counted.vis` reports `meshes`,
  `clusters`, `repacks` and `forward_fallbacks` (counted whole draws of the last visibility-buffer
  frame that drew forward for want of clusters), and `kept_meshlet_bytes`: the renderer keeps the
  cooked clusters of every counted mesh it admitted on the CPU, for a hot reload of the `.fmesh` alone
  (ADR-7542). A LOD or cross-fade variant draw, cluster residency, and the voxel far extension keep
  the forward path or the plain meshlets.
- Hot reload on the visibility buffer (ADR-7542): a re-import that reloads the `.fmesh` and the
  `.fmlt` in one update draws the new clusters whichever system runs first, and the bridge checks the
  `.fmlt` again whenever the mesh is uploaded again (`FrameRenderer::asset_mesh_upload_serial`), so a
  `.fmlt` that reloaded an update early is inserted when its `.fmesh` lands.

Deformers on a counted mesh (ADR-7721) draw from its upload too:
- Morph targets: the upload packs the cooked targets against the vertices it uploads
  (`AssetMeshUploads::morph_targets(id)`, the set `MorphTargets::from_cooked` builds on the CPU path);
  put it in a `MeshMorphTargets` beside the `MeshRenderer::counted`. The renderer deforms a GPU copy
  of the adopted vertex buffer; `anim.morph_weights` reports such instances in `renderer.counted`.
- The compute-skinning twin skins from the adopted vertex buffer and the four strongest influences
  the upload carries; `render.skinning` reports them in `last_frame.counted`.
- The GI probes size a counted mesh by its upload's box and bake its signed-distance brick from a GPU
  readback of its buffers, made once when the brick is first needed (`AssetMeshUploads::gi_readbacks`)
  and dropped after the bake.

Not drawn from a counted upload: a counted LOD or cross-fade variant on the compute-skinning path or
as a probe brick (they keep the vertex-shader path and are not traced), a counted emissive mesh's
probe emitters, and a lightmap unwrap that splits vertices. Load those meshes with `keep_cpu_data`.

