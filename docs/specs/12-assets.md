# Spec 12 — Assets: `regia_asset` (runtime) and `regia_asset_pipeline` (desktop tools)

## 1. Concepts

| Term | Meaning |
|---|---|
| Source asset | A file under `assets/` (glTF, PNG, WAV, `.material.ron`, `.level.ron`, ...) |
| `.meta` sidecar | `<file>.meta` RON file next to each source asset: id, importer, settings, sub-asset ids |
| Asset | A typed runtime object (`Texture`, `Mesh`, ...) identified by `AssetId` |
| Sub-asset | An asset produced from part of a source (one glTF → meshes, materials, skeleton, clips, textures) |
| Cooked asset | Platform-specific runtime binary produced by an importer/cooker |
| Pak | Archive of cooked assets for shipping |

## 1a. Provenance
Every `.meta` may carry a `provenance` block: `origin`, `tool` (`hand`, `agent`, `generator`, `import`),
`generator` + `seed`, agent client/session/model, `created_utc`, `reviewer` (`pending`/`owner`/name), plus
project-defined fields declared in `config/provenance.ron` (DO27 adds `foreign_game_file`,
`third_party_mod_file`). Project policy can make `cook --quality final` refuse assets that violate it
(spec 23 § 5). The content browser shows provenance and review state; `regia gen verify` checks
generator reproducibility. Built in ADR-2428: the policy format is `docs/formats/provenance-ron.md` (v1;
required fields per importer id, allowed tools, origins and licences, accepted review states, extension fields),
`regia assets validate` reports it (validator `provenance_policy`), `cook --quality final` and `package` refuse
with `provenance_policy`, and the Content Browser badge shows maker, review state and policy errors.

## 2. Identity and references

- `AssetId(Guid)`: random v4 at first import; never changes when files move or are renamed (the
  `.meta` moves with it — the editor's content browser always moves both; the asset DB detects
  orphaned `.meta` files and re-pairs moved sources by content hash).
- Sub-asset ids: deterministic `Guid` derived from `hash(parent_id, sub_asset_path)` where
  `sub_asset_path` is e.g. `"mesh/Body"`, `"material/Skin"`, `"clip/Run"`; stored in `.meta` so they
  survive re-ordering inside the source file. Renamed sub-assets: importer settings allow manual
  re-mapping; otherwise new id + warning listing broken references.
  As built (ADR-2428): every import takes the `.meta`'s stored id for a path, so editing an entry's
  `path` (keeping its `id`) is the manual re-map; renamed nodes, materials, clips, skeletons, LOD groups and
  variant sets are matched automatically by equal cooked payload, then by a group's sole pair; an unmatched
  one warns `asset_subasset_missing` with its referrers. Orphaned `.meta` files are re-paired before each
  cook by the asset database's source hash (by a unique file name without a database row).
- `AssetRef<T>`: serializable reference (`AssetId` + phantom type). Used in components and assets.
- `Handle<T>`: runtime strong reference (Arc-counted index); `WeakHandle<T>`; `UntypedHandle`.
  Dropping the last strong handle schedules unload after a grace period (default 2 s; 0 in shipping
  low-memory mode).
  *As built (ADR-5611):* `Handle<T>` is a `Copy` slot index plus generation with no reference count
  (`regia_asset::id`); a reload keeps the generation, so handles stay valid (ADR-126). Values of the
  built-in stores leave `Assets<T>` only through `Assets::remove`. The counted handle is the generic
  API's `StrongHandle<T>` / `WeakAssetHandle<T>` (ADR-7202): when the last strong handle drops, the asset
  unloads after a grace counted in frames (default 120, two seconds at 60 Hz; 0 = next frame), and a
  handle taken inside the grace keeps it. *As built (ADR-7256):* the built-in kinds (mesh, texture,
  image, material) take counted handles too (`load::<CpuMesh>(id)`, ...), stored in the same slabs; a
  typed load (`load_mesh`, ...) pins its id, so its uncounted `Handle<T>` stays valid (`is_pinned`).
  `AssetId::new_random` is a v4 GUID; a sub-asset id is BLAKE3 of the parent id and the
  path with the v4 bits set in its little-endian bytes (frozen: every cooked id depends on it).

## 3. Runtime: `AssetServer`

```rust
let tex: Handle<Texture> = assets.load(asset_ref);          // async, returns immediately
let lvl: Handle<Level>   = assets.load_path("levels/intro.level.ron"); // dev convenience; path→id via manifest
match assets.state(&tex) { LoadState::Loading | Loaded | Failed(err) => ... }
assets.get::<Texture>(&tex) -> Option<&Texture>             // via Assets<T> resource
```

- `Assets<T>` resource per asset type stores loaded values; `AssetEvent<T>` (`Added`, `Modified`,
  `Removed`, `LoadFailed`) events.
- Loaders: `trait AssetLoader { type Asset; fn type_tag() -> AssetTypeTag; async fn load(bytes, ctx) -> Result<Self::Asset> }`.
  `LoadContext` allows loading dependencies (returns handles) — dependencies are listed in the cooked
  header so they can be prefetched in parallel.
- Load groups: `assets.load_group(&[refs])` returning `LoadGroupHandle` with progress `0..=1` (used by
  loading screens).
- Path → id manifest: the cooker writes `asset_manifest.bin` (path ↔ id ↔ type) into every pak and the
  dev cache, so `load_path` works everywhere.
- GPU-backed assets (Texture, Mesh) go through a two-step: CPU asset loaded on IoPool → `PrepareAsset`
  in render world uploads to GPU. CPU copy is dropped after upload unless `keep_cpu_data` (needed for
  physics trimesh, picking).
- Memory budget tracking per asset type (bytes, CPU and GPU estimate) exposed to diagnostics.

*As built (ADR-5611).* `AssetServer` loads per type and on the calling thread: `load_mesh`,
`load_texture`, `load_image`, `load_material`, `load_model` by `AssetId`, `load_mesh_path` and
`load_material_path` by path (through `id_for_path` and the manifest), `state(id)` / `mesh_state(&handle)`,
and the `meshes()` / `textures()` / `images()` / `materials()` stores. Streaming and background loads go
through `PakStreamer` and `LoadSets` (§ 5b); loading screens read `LoadSets::load_progress`
(agent `assets.load_progress`). Hot reload reports `AssetReloaded { id, kind }` and `SourceAssetChanged`
(ADR-126). `asset_manifest.bin` is built as written. *As built (ADR-7202):* the generic API exists beside
the typed loads: `AssetLoader { type Asset; type_tag; load(payload, &mut LoadContext); cpu_bytes; kind }`
with a synchronous `load` the server runs on the IO pool (async for the caller); `LoadContext::load::<D>`
returns dependency handles and records the edges (`dependencies_of` / `dependents_of`; a hot reload of a
dependency reloads its users, `LoadedKind::Generic`); `load::<T>(id)` / `load_ref` / `load_path::<T>`
return `StrongHandle<T>` at once (`asset_not_found` for an unlisted path, `asset_no_loader`); the
outcome arrives with `AssetServer::update` (one per frame in `PreUpdate`). `load_group::<T>` returns a
`LoadGroupHandle` whose progress is a `LoadSets` set (`load_group:<n>`), completion latched once
(`LoadGroupCompleted`). `Assets<T>` records `AssetEvent` `Added` / `Modified` / `Removed` (plus the
server's `LoadFailed`) once events are on; the `AssetPlugin` forwards them as messages for the built-in
stores and every registered loader. A per-type CPU budget for generic assets (`set_memory_budget`)
evicts released assets oldest first; agent `assets.handles`. `PrepareAsset` runs on every load and
reload of a generic type, then the loader's `release_cpu_data` strips the CPU copy in place unless
`LoadSettings::keep_cpu_data` (`load_with`). *As built (ADR-7256):* the built-in kinds have loaders
registered at `open`, so counted loads of meshes, textures, images and materials run the same two-step:
`PrepareAsset::prepare` on load, the CPU streams (mesh vertices and indices, KTX2 bytes, RGBA8 pixels)
released after it unless `keep_cpu_data` or the id is pinned by a typed load, and `PrepareAsset::unload`
when the value leaves memory (grace or budget). The per-type budget holds CPU plus GPU bytes
(`budgeted_memory_use`) for every kind; `assets.texture_memory` stays the renderer's report of the
textures it uploads itself (one place per byte). `assets.handles` lists the built-in kinds and reports
reference cycles (Tarjan) as warnings. *As built (ADR-7284):* the renderer's cooked-model path has a counted
form, `regia_render::cooked_model::load_cooked_models_counted`: each part's mesh is a counted `CpuMesh` that
`AssetMeshUploads` uploads once in the frame renderer's vertex layout (the vertices `load_cooked_model`
builds), so the CPU streams go after the upload unless a reader opted in, the per-type budget and
`assets.handles` count the upload once, and the last handle's drop frees it after the grace frames. A
`FrameRenderer` given the uploader (`set_asset_meshes`) draws a `FrameMesh` keyed `asset_mesh_key(id)` with
no CPU vertices straight from those buffers (byte-identical pixels; rigid, undithered draws; the paths that
need CPU vertices, such as visibility-buffer clusters, lightmap UVs and the GI proxy brick, skip it).
`load_cooked_model` itself keeps its CPU `LabMesh`es. Counted texture loads keep a view of a mapped pak's
KTX2 bytes, counted built-in loads record their load forms (`load_forms`, `assets.cooked_pack` `loads`), and
a counted texture whose cooked file became an RGBA8 image (or back) takes the new kind on hot reload (the
count follows the asset; `reload_id` reports the new `LoadedKind`). *As built (ADR-7330):* the ECS path
draws counted meshes too: `regia_render::counted_mesh(handle)` (or `MeshRenderer::counted`) is a stand-in
`Arc<CpuMesh>` with no vertices whose mesh key is the asset's and which holds the handle while it lives, so
the extracted frame carries no CPU vertices and the renderer draws the upload; `CountedMeshPlugin`
registers the renderer's uploaders with the App's asset server and the runtime renderer. Counted textures
have a renderer `PrepareAsset` (`AssetTextureUploads`: KTX2 `CookedTexture` and RGBA8 `CookedImage`,
uploaded once in the color space set before the load, counted by the asset server, freed after the grace);
a material slot holding `MaterialImage::Asset` binds that texture instead of the renderer's own upload. A
counted draw that brings a second UV set with one entry per uploaded vertex (`MeshUv1`, a lightmap
receiver's UV2) gets it as its `uv1` stream, so lightmap receivers (a 1:1 unwrap) and graphs that read
`TexCoord1` draw counted meshes (the cooked mesh format itself has no second UV stream). The mesh cache
never evicts an adopted counted mesh and reports its bytes apart (`CacheBudgets::adopted_mesh_bytes`,
`render.stats` `mesh_cache`). Still needing CPU data, so not drawn from a counted upload: the
visibility-buffer clusters, the GI proxy (signed-distance) brick, skinning, morph targets, LOD chains,
and a lightmap unwrap that splits vertices; load such meshes with `keep_cpu_data`.
*As built (ADR-8610):* a counted texture is one GPU texture with an sRGB and a linear view (wgpu
`view_formats`), and each slot binds the view of its own color space, so one counted image in a
base-color and a mask slot samples like the CPU path's two uploads (`set_srgb` now picks only the space
the mip chain is filtered in; a device without view formats or a format with no sRGB variant keeps one
view). A material entry records the upload serials of the counted textures it binds and is rebuilt when
one moves, so an App material shows a hot-reloaded counted texture on the next frame without a new key.
Material-graph texture slots and the foliage-sway twin's four maps take counted textures
(`GraphTexture::asset`). A counted mesh with a skin of at most `MAX_JOINTS_PER_SKIN` joints uploads its
four strongest influences; a skinned `MeshRenderer` on it skins on the vertex-shader path from the upload
(the joint reach, bind boxes and one-joint fold are measured at the upload), and drawn without joints it
reads identity palette slots and stands at its rest pose; the CPU release drops the skin's per-vertex
influences too (the joint names stay). Still CPU-only for counted meshes: morph
targets, LOD chains, the visibility-buffer clusters, the GI proxy brick and the compute-skinning twin.
*As built (ADR-7428):* LOD chains draw from the counted upload: `MeshLodChain::from_cooked_asset` reads a
counted asset's chain from its cooked payload (no vertex kept), and each LOD, shadow-LOD or cross-fade draw of
a counted `MeshRenderer` shares the adopted vertex buffer and binds its level's index list, uploaded once per
level (LOD 0 binds the upload's own indices; `render.stats` `counted.lod_index_uploads`). A counted base map
that is also a detail, film, parallax or curvature slot of the same material decodes for that slot, and a
counted stand-in respawned between two extractions keeps the adopted entry. Morph targets, the GI proxy brick
and the compute-skinning twin draw from the upload since ADR-7721 (below).

*As built (ADR-7721):* the deformers draw from the counted upload too. Morph targets: the upload packs the
mesh's cooked targets against the vertices it uploads (`AssetMeshUploads::morph_targets`, the set
`MorphTargets::from_cooked` builds on the CPU path), and a morphed counted draw deforms a GPU copy of the
adopted vertex buffer (its span and motion poses start as GPU copies too); `anim.morph_weights`
`renderer.counted`. The compute-skinning twin skins from the adopted vertex buffer (bound as storage where the
upload has it, otherwise a GPU copy) and the four strongest influences it uploaded; `render.skinning`
`last_frame.counted`. The probes size a counted mesh by its upload's box and index count, and bake its
signed-distance brick from a GPU readback of its buffers made on the brick's miss path only (waited for in
synchronous bakes; dropped after the bake; `AssetMeshUploads::gi_readbacks`). Uploaded buffers gain `COPY_SRC`.
Still not from the upload: a counted LOD or cross-fade variant is neither compute-skinned nor traced by the
probes, and a counted emissive mesh adds no emitter clusters. No format changed.

*As built (ADR-7462):* a counted mesh draws its visibility-buffer clusters from its upload. The meshlet bridge
inserts its `.fmlt` under the stand-in's key (`asset_mesh_key`), checked against the upload's counts
(`FrameRenderer::asset_mesh_counts`). The cluster arena takes the same ranges as for CPU vertices, and the base
vertices are a GPU-owned range that the arena's CPU mirror never writes. One compute pass
(`vis_vertex_repack.wgsl`) fills it from the adopted vertex buffer, which the upload gives `STORAGE` where the
device has the path. A growing arena stream now copies its old buffer on the GPU instead of uploading the mirror.
`render.stats` `counted.vis` reports meshes, clusters, repacks and forward fallbacks. A counted mesh still draws
forward on the visibility-buffer profile in these cases: cluster residency is on, it is drawn as a LOD or
cross-fade variant, it has no cooked meshlets, or its vertex buffer is past the device's storage-binding limit.
The voxel far extension needs CPU vertices, so a counted mesh with a `VoxelFarSource` takes its plain meshlets.
No format changed.

*Hot reload (ADR-7542):* the renderer keeps the cooked meshlets of every counted mesh it admitted (reported as
`counted.vis.kept_meshlet_bytes`), so a reload of the `.fmesh` alone keeps its clusters. When the `.fmesh` and the
`.fmlt` reload in the same update, the fresher meshlets win in either system order: meshlets the bridge inserted
since the admission replace the kept ones. The bridge checks a counted stand-in's `.fmlt` again whenever its mesh
is uploaded again (`FrameRenderer::asset_mesh_upload_serial`) and inserts it again. A counted mesh is admitted
only with a vertex buffer within the device's storage-binding limit.

## 4. Cooked format

```
CookedHeader { magic: b"FCKD", format_version: u32, type_tag: u64 (TypePathHash), asset_id: Guid,
               importer_version: u32, dependencies: Vec<AssetId>, payload_len: u64 }
payload: postcard-serialized metadata + raw aligned blobs (vertex/index buffers, PCM...)
```

`TypePathHash` is FNV-1a of the type path with the engine's crate names in their spelling from before
the rename to Regia Engine (ADR-1429), so tags written by any release stay valid.

Textures are cooked to **KTX2** (inside the payload) so tools can inspect them. Meshes use a custom
layout (§ 7). Each runtime crate defines its own cooked types and loader; the pipeline crate writes them.

## 4a. Content formats and the freeze schedule
Game assets (DO27) are produced **in parallel** with the engine, mostly by generators. To stop that work
from being thrown away:

| Format | Kind | Usable for production from |
|---|---|---|
| glTF 2.0 (`.glb`/`.gltf`, metallic-roughness PBR, skins, animations) | neutral source | now |
| PNG/TGA/EXR/HDR textures, 16-bit PNG heightfields | neutral source | now |
| WAV (48 kHz) / OGG / FLAC audio | neutral source | now |
| TTF/OTF fonts | neutral source | now |
| `.meta` (ids, import settings, provenance; v2 adds the importer setting `lossless_clips`, v1 migrates with it off, ADR-7819, [`docs/formats/meta.md`](../formats/meta.md)) | Regia Engine | M4 (v2 since v0.5.x) |
| `.material.ron` (standard + layered material; v2 hard surface, v3 glTF glass/IOR/emission strength, ADR-074; v4 thin film, ADR-1977; v5 weather response, ADR-2032; v6 subsurface scattering, ADR-2049; v7 transmission roughness, ADR-2075) | Regia Engine | M4 (layers final in M5) |
| `.prefab.ron`, `.level.ron` (scene format v14 since ADR-10079: `LevelTerrain::sample_layout`, `VertexCentered` (the default) or `CellCentered` terrain samples, v13 levels migrate `VertexCentered` (number-only, the same heights); v13 since ADR-2860: `LevelLook::captures`, the level names its local reflection capture bake, and the saved `LocalCapture` component; v12 levels migrate naming none; v12 since ADR-2411: `LevelSettings::terrain`, [`docs/formats/scene-files.md`](../formats/scene-files.md)) | Regia Engine | M6 |
| `.ui.ron` (v2 since v0.1.29: menu widgets, built-in v1 migrator, ADR-1381; v4: text effects, built-in v3 step, ADR-2045; v5: `backdrop_blur`, built-in v4 step, ADR-2129; v6: `repeat`, `converter`, built-in v5 step, ADR-2489; v7: `group_opacity`, `clip_shape`, `mask`, built-in v6 step, ADR-2866; v8: components `component`, `use`, `params`, `bind_root`, `slots`, `slot`, built-in v7 step, ADR-3147; v9: `image`/`mask` named by `asset` or `icon`, `nine_slice`, built-in v8 step, ADR-3148), `UiTheme`, `.ftl` localization | Regia Engine | M7 |
| `.theme.ron` (UI themes: tokens, `extends` chain, widget types and variations; v0 = the pre-v1 `UiTheme` shape, migrated; [`docs/formats/theme-ron.md`](../formats/theme-ron.md), ADR-274, ADR-277, ADR-290) | Regia Engine | v0.1.24; v1 frozen with lane D6 (ADR-290) |
| `.uisettings.ron` (the project's UI settings at `assets/ui/project.uisettings.ron`: DPI curve, layer budget; [`docs/formats/uisettings-ron.md`](../formats/uisettings-ron.md), ADR-2866) | Regia Engine | v0.1.28 |
| `.audiopatch.ron`, `SoundCue`, music stem metadata (`.cue.ron` v2 since v0.1.24: `version` field, migrator from the versionless v1, ADR-272; v3 since integ/v028 adds the sound `caption`, v1 and v2 migrate, ADR-1697; `.music.ron` v2 adds `version`, `segment_bars`, `stingers` and `transitions`, the versionless v1 migrates, ADR-2517, [`docs/formats/music-ron.md`](../formats/music-ron.md)) | Regia Engine | M8 |
| `.animgraph.ron` | Regia Engine | M12 |
| terrain layer/scatter rules | Regia Engine | M13 |
| `.vfx.ron` | Regia Engine | M14 |
| `.shots.ron` (camera paths and shot sequences: cuts, fades, FOV and depth-of-field tracks, [`docs/formats/shots-ron.md`](../formats/shots-ron.md)) | Regia Engine | v0.1.22 |
| `.seq.ron` (the sequencer timeline: one master clock of tracks animating a named target's property — float/vec3/bool keys with Bezier tangents — animation-clip, audio (flat or spatial, with a voice time scale, Doppler, occlusion and a pitch-preserving mode), sub-sequence (once or looping) and video sections, event-channel keys, a per-track lock, and markers, spawnable bindings that name a prefab and `restore_on_stop`; v6, v1 to v5 migrate (v4 → v5 turns Doppler off on spatial sections; v5 → v6 adds `Event` keys and `Track::locked`), [`docs/formats/seq-ron.md`](../formats/seq-ron.md), ADR-1507, ADR-1880, ADR-2405, ADR-2862, ADR-2948, ADR-3197) | Regia Engine | v0.3.x (data model frozen; the full sequencer is v0.4.0) |
| cooked mesh (`regia_asset::CookedMesh`; v2 skin, `COLOR_0` and sparse morph blocks, ADR-226; v3 `skin_influences` byte, 4 default or 8 opt-in, migrator v1/v2 → v3, ADR-1622; [`docs/formats/cooked-mesh.md`](../formats/cooked-mesh.md)) | Regia Engine (cooked) | batch v020 |
| lightmap (`.flmap`, `regia_asset::lightmap`; v5: RGB9E5, BC6H or ASTC 4×4 HDR atlas, coverage, per-entry UV2 unwrap, optional RGBA8 directions, entry keys and seam figures, optional per-entry clusters, denoise record; v1 to v4 migrate, ADR-860, ADR-1556, ADR-2024, ADR-2107, ADR-2314, [`docs/formats/lightmap.md`](../formats/lightmap.md)) | Regia Engine (baked) | v0.3.0 (versioned and migrated from v1 on) |
| post volume (the reflected `PostVolume` component inside `.level.ron` / `.prefab.ron`; v3 since ADR-2323 (convex-hull shape and optional `lut`; v2 and v1 read unchanged), v2 ADR-1863 (optional lens overrides), v1 ADR-1464, [`docs/formats/post-volume-v3.md`](../formats/post-volume-v3.md)) | Regia Engine | v0.1.28 |
| `.render_job.ron` (a sequence-driven render-out request: takes by frames, seconds, markers or shots, path tokens, cuts, output settings, a pre-roll cap, a sequence binding table, encoder settings and a product to draw; `regia_agent::render_job`, v4, older versions migrate, ADR-1616, ADR-1880, ADR-3369, ADR-2757, [`docs/formats/render-job-ron.md`](../formats/render-job-ron.md)) | Regia Engine | v0.3.x |
| `.shot.ron` (a list of look-review shots for `seeing.capture_plate`: camera pose or named camera, time of day, profile, size, warm-up, AOVs; `regia_agent::shot_list`, v1, ADR-2463, [`docs/formats/shot-ron.md`](../formats/shot-ron.md)) | Regia Engine | v0.1.28 |
| `.destructible.ron` (destruction level 2 chunk asset: chunks, bonds, material; v1, ADR-1597; v2 adds optional structural load and falling sections with a v1 migrator, ADR-2734, [`docs/formats/destructible.md`](../formats/destructible.md)) | Regia Engine | v0.1.28 (v1 frozen on merge; v2 frozen on merge) |
| `.destructionfx.ron` (destruction effects table: `fx_tag` → sound cue, VFX system, decal, caps; v1, ADR-2734, [`docs/formats/destructionfx-ron.md`](../formats/destructionfx-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.fracture.ron` (pre-fractured chunk sets: chunks, joints, render meshes with normals, UVs and tangents; v2 adds the render meshes, v1 migrates, ADR-1567, ADR-1967, [`docs/formats/fracture.md`](../formats/fracture.md)) | Regia Engine | v0.1.28 (v2 frozen on merge) |
| `.vehicle.ron` (vehicle configuration: chassis, drive limits, ray layers, wheels with their tire model, drivetrain, anti-roll, aero, aids; v3, ADR-2301, ADR-2598, ADR-2719, [`docs/formats/vehicle.md`](../formats/vehicle.md)) | Regia Engine | v0.1.28 (v3; v1 and v2 load through the migrator) |
| `.vehicle-snapshot.ron` (one vehicle's saved state: chassis, rapier's wheel records, drivetrain, slip tires and transient history; v2, v1 migrates, ADR-3080/4590, [`docs/formats/vehicle-snapshot.md`](../formats/vehicle-snapshot.md)) | Regia Engine | v0.1.28 |
| `.physmat.ron` (physics material: friction, restitution, combine rules, density; colliders name it with `PhysicsMaterialRef`; v1, ADR-2424, [`docs/formats/physmat.md`](../formats/physmat.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| cooked convex hull set (`FCHS` sub-asset `collision/<node>/primitive_<i>` inside a cooked `.fckd`, from the glTF setting `collision: ConvexDecomposition`; derived data, registered as `hull-set` with a layout fingerprint; v1, ADR-2301, [`docs/formats/hull-set.md`](../formats/hull-set.md)) | Regia Engine (cooked) | v0.1.28 (v1 frozen on merge) |
| `.statetree.ron` (AI state trees; v2 adds enter sets, holds, ambient tasks and includes, v1 migrates by rewriting the version, ADR-1639, [`docs/formats/statetree-ron.md`](../formats/statetree-ron.md)) | Regia Engine | v0.1.28 |
| `.smartobject.ron` (smart object definition: activity tags, rules, conditions, behaviors, slots and entrances; v1, ADR-1638, [`docs/formats/smartobject-ron.md`](../formats/smartobject-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.utility.ron` (utility AI definition: actions, considerations with response curves, weights, momentum, cooldowns, selection and seed; v1, ADR-2565, [`docs/formats/utility-ron.md`](../formats/utility-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.slots.ron` (skeleton slot table: montage slot names and masks; v1, ADR-1644, [`docs/formats/slots-ron.md`](../formats/slots-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.montagevar.ron` (montage variant: per-segment clip overrides of a parent montage; v1, ADR-1644, [`docs/formats/montage-variant-ron.md`](../formats/montage-variant-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.camera.ron` (data-driven gameplay camera: parameters, input slots, rigs, transitions, director; v1, ADR-1637, [`docs/formats/camera-ron.md`](../formats/camera-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `config/weather.ron` (the project's weather: effects, presets, model numbers; v1, ADR-1847, [`docs/formats/weather-ron.md`](../formats/weather-ron.md)) | Regia Engine | v0.2.x (v1 frozen on merge) |
| `config/voice_chat.ron` (voice chat project settings: quality, mode, jitter, processing, VAD, channels; v1, ADR-1647, [`docs/formats/voice-chat-ron.md`](../formats/voice-chat-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.pgraph.ron`, `.pgvar.ron`, `.pgout` (procedural graphs, graph variants and baked outputs; the `ProcGenerator`, `InstancedMeshSet` and `PaintedPoints` components inside `.level.ron`; v1 each, ADR-1640, specs 62, 63, 67; the agent page [`book/agents/procgen.md`](../../book/agents/procgen.md)); `.pgout` and `ProcGenerator` are v2 since integ/v028 (spec 66 tiles: cell table with tier and cell hash, `ProcGenerator::tiles`; v1 migrates, ADR-1875); `.pgout` is v3 since ADR-2003 (generated meshes, terrain edits, road decals; v1 and v2 migrate) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| content manifest and chunk map (spec 40, v1 each, ADR-1667, [`docs/formats/content-manifest.md`](../formats/content-manifest.md)), install record `content-state.ron` (`content-state` v1, its own check-formats row since ADR-7236, [`docs/formats/content-state.md`](../formats/content-state.md)); `.level.d/` per-entity level directories and `.layers.ron` layer definitions (spec 37, v1 each, ADR-1667, [`docs/formats/level-dir.md`](../formats/level-dir.md), [`docs/formats/layers-ron.md`](../formats/layers-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| chunk reason table `chunk-why.bin` and chunk host record `content-host.ron` (spec 40, v1 each, derived data, ADR-1876, [`docs/formats/chunk-why.md`](../formats/chunk-why.md), [`docs/formats/content-host.md`](../formats/content-host.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| functional tests: the `FunctionalTest` component in `.level.ron`, `.functest.ron` scripts, `<level>.expected.ron` and the `TestReport` JSON (spec 41, v1 each, ADR-1673, [`docs/formats/functional-test.md`](../formats/functional-test.md), [`docs/formats/functest-ron.md`](../formats/functest-ron.md), [`docs/formats/expected-ron.md`](../formats/expected-ron.md), [`docs/formats/test-report.md`](../formats/test-report.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.curves.ron` and `.chooser.ron` (curve tables and choosers, spec 51, v1 each, [`docs/formats/curve-table-ron.md`](../formats/curve-table-ron.md), [`docs/formats/chooser-ron.md`](../formats/chooser-ron.md)); `.attrs.ron`, `.effect.ron`, `.ability.ron` (attributes, effects, abilities, spec 53, v1 each, [`docs/formats/attrs-ron.md`](../formats/attrs-ron.md), [`docs/formats/effect-ron.md`](../formats/effect-ron.md), [`docs/formats/ability-ron.md`](../formats/ability-ron.md)); registered by ADR-1695 | Regia Engine | v0.1.28 (v1 frozen on merge) |
| cooked curve tables and choosers (pak entries, cooked v1 each, a cache cooked again at another version, ADR-2012, [`docs/formats/curve-table-ron.md`](../formats/curve-table-ron.md), [`docs/formats/chooser-ron.md`](../formats/chooser-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.ragdoll.ron` (ragdoll profile: simulated bones, joint limits, facing axis; v1, ADR-1693, [`docs/formats/ragdoll-ron.md`](../formats/ragdoll-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `Spline` and `Shape` components in `.level.ron`, `.road.ron` road profiles and the `RoadOf` component (spec 64, v1 each, ADR-1664, [`book/agents/splines.md`](../../book/agents/splines.md)); registered by ADR-1875 | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.biome.ron` and the `BiomeArea` component (spec 65, v1 each, ADR-1665, [`docs/formats/biome-ron.md`](../formats/biome-ron.md)); `<name>.plants.ron` plant variant index and the `ProcGenAnchor` component (specs 68 and 66, v1 each, ADR-1666) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.conv.ron`, `speakers.ron`, `conversation.ron` and `cue_events.ron` (conversations and captions, spec 56, v1 each, ADR-1697, [`docs/formats/conversation-ron.md`](../formats/conversation-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.video.ron` and `.videolist.ron` (video sources and playlists, spec 50, v1 each, ADR-1670, [`book/agents/video-playback.md`](../../book/agents/video-playback.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.spatialquery.ron` (AI spatial queries, spec 33; v2 adds the `Custom` generator and test, v1 migrates by rewriting the version, ADR-1674, ADR-2709, [`book/agents/spatial-queries.md`](../../book/agents/spatial-queries.md)); registered by ADR-1875 | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.navlinks.ron` (authored off-mesh links: ends, direction, kind, user id, snap radius, cost scale, filter flags, enable switch, traversal time; v1, ADR-1821, [`docs/formats/navlinks-ron.md`](../formats/navlinks-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.navlanes.ron` (authored crowd lanes: centre line, width, speed limit, filter flags, user id, next lanes; v1, ADR-1934, [`docs/formats/navlanes-ron.md`](../formats/navlanes-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.motiondb.ron` (motion-matching database sources, spec 52 § 4; v2 adds contact labels and the `foot_contact` / `foot_phase` weights, v1 migrates by rewriting `format_version`, ADR-1831, ADR-2858, [`book/agents/motion-matching.md`](../../book/agents/motion-matching.md), [`docs/formats/motiondb-ron.md`](../formats/motiondb-ron.md)) | Regia Engine | v0.1.28 (v2; v1 loads through the migrator) |
| `.retarget.ron` (retarget setups, spec 85, v1, ADR-1857; v2 since v0.6.0: IK goals at retarget bake, `migrate_retarget`, ADR-8476; no step in format batch 2) | Regia Engine | v0.1.28 (v1 frozen on merge); v0.6.0 (v2) |
| `.autotile.ron` (2D auto-tiling rules, spec 86, v1, ADR-1889) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `luamod.ron` (a Lua mod's manifest, spec 84, v1, ADR-1751) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.regiademo` (demo recording, RON; v2 adds per-step input timestamps, v1 migrates, [`docs/formats/regiademo.md`](../formats/regiademo.md)) and `.frec` (the same record as a binary match record with a CRC, v1, [`docs/formats/frec.md`](../formats/frec.md)); user data written by games, registered by ADR-1878 (RR-7) | Regia Engine | v0.1.28 (frozen on merge) |
| `loudness.ron` (loudness table: measured loudness and normalization gain per sound; a cook output, derived data; v1, ADR-1855, [`docs/formats/loudness-ron.md`](../formats/loudness-ron.md)) | Regia Engine | v0.5.0 (v1 first) |
| `<map>.scatterpaint.bin` (painted mesh instances, per class and tile; magic `RSPT`, v1, ADR-1936, [`docs/formats/terrain.md`](../formats/terrain.md) § 10) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `luastate` (Lua mod state, a section inside a save file, spec 84 § 11, v1, ADR-1943, [`book/agents/lua-mods.md`](../../book/agents/lua-mods.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.texpack.ron` (channel-pack recipe: up to four source images, each output channel a source channel or a constant; v1, ADR-3090, [`docs/formats/texpack-ron.md`](../formats/texpack-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `config/net.ron` (`net-settings`: network settings, spec 70-77 and 89; v1; registered by ADR-2316, [`docs/formats/net-ron.md`](../formats/net-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `.netprofile.ron` (`netprofile`: replication profiles, spec 75; v1; registered by ADR-2316; v2 adds the optional `extrapolation_gate_m`, v1 migrates with the gate off, ADR-8522, [`docs/formats/netprofile-ron.md`](../formats/netprofile-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `server.toml` (`server-toml`: dedicated server configuration, spec 76; hashed from `SERVER_TOML_KEYS`; v1 when registered, v4 since v0.6.0 (the v4 row below); registered by ADR-2316, [`docs/formats/server-toml.md`](../formats/server-toml.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `bans.ron` (`bans`: the server ban list, spec 77 § 7.3; v1; registered by ADR-2316, [`docs/formats/bans-ron.md`](../formats/bans-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `net-manifest.ron` (`net-manifest`: the connect-time content manifest, spec 77; v1; registered by ADR-2316, [`docs/formats/net-manifest-ron.md`](../formats/net-manifest-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `server.key` (`server-key`: a dedicated server's key pair, machine-local; v1; registered by ADR-2316, [`docs/formats/server-key.md`](../formats/server-key.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `*.siminput.ron` (`siminput`: simulated-client inputs, dev only, spec 77 § 10.3; v1; registered by ADR-2316, [`docs/formats/siminput-ron.md`](../formats/siminput-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| Lobby keys (`lobby-keys`: the engine's `regia.*` lobby data, spec 89 § 4; layout v2 since ADR-8897 (optional `regia.mode` and `regia.playlist`; v1 reads unchanged), v1 registered by ADR-2316, not a file, no `upgrade-project` entry, [`docs/formats/lobby-keys.md`](../formats/lobby-keys.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `config/xr.ron` (`xr-config`: XR settings, spec 58 § 4.1; v1; registered by ADR-2316, [`docs/formats/xr-config-ron.md`](../formats/xr-config-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `assets/xr/actions.xr.ron` (`xr-actions`: the XR action manifest, spec 58; v1; registered by ADR-2316, [`docs/formats/xr-actions-ron.md`](../formats/xr-actions-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `*.xrsim.ron` (`xrsim`: XR simulator scripts, spec 58; v1; registered by ADR-2316, [`docs/formats/xrsim-ron.md`](../formats/xrsim-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `regia-engine.release.ron` (`engine-release`: the packaged engine release manifest, spec 94 § 4.1; v1; registered by ADR-2316, [`docs/formats/engine-release-ron.md`](../formats/engine-release-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `.session.ron` (`session`: the session harness file, spec 41 § 2.8; v2, v1 migrates by a version rewrite, ADR-2137; registered by ADR-2316, [`docs/formats/session-ron.md`](../formats/session-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `config/validation.ron` (`validation`: asset validation rules, spec 32 § 4.4; v1; registered by ADR-2316, [`docs/formats/validation-ron.md`](../formats/validation-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `.uianim.ron` (`uianim`: UI animation libraries, ADR-1842; v1, a file without `format_version` reads as v1; registered by ADR-2316, [`docs/formats/uianim-ron.md`](../formats/uianim-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `.table.ron` (`data-table`: data-table sources, ADR-1466; v1, v0 bare lists migrate; the cooked form is `data-table-cooked`; registered by ADR-2316, [`docs/formats/data-table-ron.md`](../formats/data-table-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `config/asset_tags.ron` (`asset-tags`: asset tag rules, ADR-1466; v1; registered by ADR-2316, [`docs/formats/asset-tags-ron.md`](../formats/asset-tags-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `config/asset_collections.ron` and the personal `.regia/asset_collections.ron` (`asset-collections`: Content Browser collections, favorite folders, folder colors and browser settings, ADR-7759; v1; [`docs/formats/asset-collections-ron.md`](../formats/asset-collections-ron.md)) | Regia Engine | v0.6.x (frozen on merge) |
| `config/import_presets.ron` (`import-presets`: named importer settings presets per importer kind, ADR-7904; v1; [`docs/formats/import-presets-ron.md`](../formats/import-presets-ron.md)) | Regia Engine | v0.6.x (frozen on merge) |
| `asset_registry.bin`, `<pak>.registry.bin` (`asset-registry`: the cooked asset registry, a cook output; v1; registered by ADR-2316, [`docs/formats/asset-tags-ron.md`](../formats/asset-tags-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `input_overrides.ron` and `input_profiles/<name>.ron` (`input-overrides`: rebound bindings; v2, v1 reads as a subset; registered by ADR-2316, [`docs/formats/input-overrides-ron.md`](../formats/input-overrides-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `permutations.json` (`permutations`: the cooked shader-permutation list, ADR-2026, a cook output; v1; registered by ADR-2316, [`docs/formats/permutations.md`](../formats/permutations.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `<id>.convc` (`convc`: cooked conversations, spec 56 § 4.3, a cook output; v1; registered by ADR-2316, [`docs/formats/convc.md`](../formats/convc.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `.fcap` (`fcap`: a level's local reflection capture bake, binary with a CRC, named by `LevelLook::captures`; v1, ADR-2860, [`docs/formats/fcap.md`](../formats/fcap.md)) | Regia Engine (baked) | v0.1.28 (v1 frozen on merge) |
| `.bt.ron` (`bt`: behavior tree definitions; no version field, v1 by `BT_FORMAT_VERSION`; registered by ADR-2316, [`docs/formats/bt-ron.md`](../formats/bt-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| The cooked nav cell (`nav-cell`: inside a cell blob, ADR-1934; hashed from `NAV_CELL_LAYOUT`; v2 since ADR-2589 (per-tile links; v1 loads in the loader), not a file, no `upgrade-project` entry; registered by ADR-2316, [`docs/formats/nav-cell.md`](../formats/nav-cell.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `<map>.weights.bin` (`terrain-weights`: painted terrain layer weights; postcard, v1; registered by ADR-2316, [`docs/formats/terrain-weights.md`](../formats/terrain-weights.md)) | Regia Engine | v0.2.x (frozen on merge) |
| `<map>.grassdensity.bin` (`grass-density`: the painted grass density, one 8-bit multiplier per texel over the map; postcard, v1, ADR-2411, [`docs/formats/grass-density.md`](../formats/grass-density.md)) | Regia Engine | v0.4.0 (v1 frozen on merge) |
| `.variant.ron` (prefab variants: a base prefab id and the edits that make the variant; v1, `prefab-variant` in `check-formats`, ADR-2323, `docs/specs/83-prefab-profiler-volumes.md` § 2.3, [`docs/formats/prefab-variant-v1.md`](../formats/prefab-variant-v1.md)) | Regia Engine | v0.3.x |
| lens effects (the reflected `LensEffects` component inside `.level.ron` / `.prefab.ron`; v1, `lens-effects` in `check-formats`, ADR-3196, [`docs/formats/lens-effects-v1.md`](../formats/lens-effects-v1.md)) | Regia Engine | v0.3.x |
| physical camera (the reflected `PhysicalCamera` component inside `.level.ron` / `.prefab.ron`; v1, `physical-camera` in `check-formats`, ADR-2323, [`docs/formats/physical-camera-v1.md`](../formats/physical-camera-v1.md)) | Regia Engine | v0.3.x |
| `.rprof.json` (profile captures; diagnostics, never loaded by the engine; v2 since ADR-7325 (the optional per-thread timeline; v0 and v1 read as v2), `profile-capture` in `check-formats`, ADR-2323, [`docs/formats/profile-capture-v2.md`](../formats/profile-capture-v2.md)) | Regia Engine | v0.3.x |
| Product viewer package (`manifest.json` of `regia.viewer_package/1` plus `mesh.rvm`, magic `RVM1`: quantized meshes, recipe, option data, material entries, BLAKE3 hashes; cooked data with no migrator, re-cooked on a version change; v1, ADR-2540, [`docs/formats/viewer-package.md`](../formats/viewer-package.md)) | Regia Engine (cooked) | v0.1.28 (v1 frozen on merge) |
| cooked voxel far proxy (`FVOX` sub-asset `voxels/<node>/primitive_<i>` inside a cooked `.fckd`, from the glTF setting `voxel_far: Some((resolution))`; derived data, recooked on a version change, registered as `voxel-far` with a layout fingerprint; v2 adds a sub-voxel box per voxel, v3 a sampled box error per level, v4 a coverage estimate per level, v1 to v3 still load and `regia upgrade-project` rewrites them, ADR-2445, ADR-2655, ADR-3108, ADR-3227, [`docs/formats/voxel-far.md`](../formats/voxel-far.md)) | Regia Engine (cooked) | v0.1.28 (v4 frozen on merge) |
| `.kind.ron` (entity kind declaration: name, icon, color, description, category, required component type paths, optional code marker; v1, ADR-2488, [`docs/formats/kind-ron.md`](../formats/kind-ron.md)) | Regia Engine | v0.1.28 (v1 frozen on merge) |
| `.ambience.ron` (`ambience`: an ambience, looping beds plus seeded random one-shots; v1, `version` field, `ambience` in `check-formats`, validated by the cook, `upgrade-project` leaves v1 as is; ADR-2721, [`docs/formats/ambience-ron.md`](../formats/ambience-ron.md)) | Regia Engine | v0.2.x (frozen on merge) |
| traversal components (the reflected `TraversalSettings`, `Ladder` and `TraversalJumpPoint` components inside `.level.ron` / `.prefab.ron`; each v1, `traversal-settings`, `traversal-ladder` and `traversal-jump-point` in `check-formats`, ADR-2971, [`docs/formats/traversal-settings.md`](../formats/traversal-settings.md), [`traversal-ladder.md`](../formats/traversal-ladder.md), [`traversal-jump-point.md`](../formats/traversal-jump-point.md)) | Regia Engine | v0.2.x (v1 frozen on merge) |
| `.logic.ron` (logic graph: event → condition → action rules, locals, timer seed; v1, the sequence/net/input arms and world scopes parse but fail compile until phase 3, spec 102, ADR-7201, [`docs/formats/logic-ron.md`](../formats/logic-ron.md)) | Regia Engine | v0.1.28 |
| `.glyphs.ron` (the project's button-prompt glyphs at `assets/input/project.glyphs.ron`: glyph per action per device family; [`docs/formats/input-glyphs-ron.md`](../formats/input-glyphs-ron.md), ADR-7070) | Regia Engine | v0.1.28 |
| `<user config>/regia/editor_prefs.ron` (`editor-prefs`: editor preferences, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-prefs-ron.md`](../formats/editor-prefs-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `<user config>/regia/editor_keymap.ron` (`editor-keymap`: editor shortcuts, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-keymap-ron.md`](../formats/editor-keymap-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `editor_layout.ron` (user default), `.regia/editor_layout.ron`, `layouts/<name>.editor_layout.ron` (`editor-layout`: dock layouts and named layouts, the editor's own file; v2, v1 migrates by a version rewrite (v2 added `floating`); registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-layout-ron.md`](../formats/editor-layout-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `<user config>/regia/editor_window_state.ron` (`editor-window-state`: the main window and open documents per project, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-window-state-ron.md`](../formats/editor-window-state-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `<user config>/regia/inspector_pins.ron` (`inspector-pins`: pinned Inspector fields, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/inspector-pins-ron.md`](../formats/inspector-pins-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `.regia/editor_profile.ron`, `profiles/<name>.editor_profile.ron` (`editor-profile`: editor feature profiles, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-profile-ron.md`](../formats/editor-profile-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `.regia/editor_view.ron` (`editor-view-state`: per-project editor view state, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-view-state-ron.md`](../formats/editor-view-state-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `.regia/viewport_snap.ron` (`viewport-snap`: per-project gizmo snap steps, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/viewport-snap-ron.md`](../formats/viewport-snap-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `<user config>/regia/saved_searches.ron` (personal searches and palette favorites) and `config/saved_searches.ron` (shared) (`saved-searches`: universal-search saved searches, ADR-7766, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/saved-searches-ron.md`](../formats/saved-searches-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `<user config>/regia/themes/<name>.editor_theme.ron` (`editor-theme`: saved editor themes, ADR-7838, the editor's own file; v1; registered by ADR-7902 through `regia_core::editor_formats`, [`docs/formats/editor-theme-ron.md`](../formats/editor-theme-ron.md)) | Regia Engine (editor) | v0.6.x (frozen on merge) |
| `.levelvariant.ron` (level variants: a base level id and only the differences, resolved by the loader, flattened by `regia package`; v1, `level-variant` in `check-formats`, ADR-7823, [`docs/formats/level-variant-v1.md`](../formats/level-variant-v1.md)) | Regia Engine | v0.6.0 |
| `<map>.terrain.editlayers` (`terrain-editlayers`: non-destructive terrain edit layers, the base where a layer covers it and each layer's values in 64-sample chunks, plus per-layer material weights and an optional procgen target; postcard, v2, v1 migrates with empty weights and no target, ADR-7825, ADR-8381, [`docs/formats/terrain-editlayers.md`](../formats/terrain-editlayers.md); editor-only, the cooked map holds the flattened result) | Regia Engine | v0.6.0 (v2 frozen on merge) |
| `.tilepatterns.ron` (`tilepatterns`: the editor's saved tile patterns of one tileset, next to the tileset image; v1, `version` field, `tilepatterns` in `check-formats` and `regia upgrade-project`, [`docs/formats/tilepatterns-ron.md`](../formats/tilepatterns-ron.md), ADR-7853) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `<map>.meshlayers.ron` and `<map>.passages.ron` (`terrain-mesh-layers`: the mesh layers and world geometry source and the passage descriptions a `Passages` layer names; RON, v1, ADR-8112, spec 121 § 9.1, [`docs/formats/terrain.md`](../formats/terrain.md) § 12; `regia terrain passages import` converts DO27's foreign `do27-tunnels/1`, which is never frozen) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `<map>.meshlayers` (`terrain-mesh-layers-cooked`: the cooked canonical world geometry in `i32` mm, its BLAKE3 `content_hash` the lockstep handshake compares, and the presentation section; postcard after the `RGML` magic, v1, ADR-8112, spec 121 § 9.2, [`docs/formats/terrain.md`](../formats/terrain.md) § 13; a cache: a mismatch recooks; v2 since v0.7: the canonical section's counts at a fixed width behind a hashed head, v1 migrates, ADR-8816) | Regia Engine | v0.6.0 (v1 frozen on merge; v2 in v0.7) |
| `<map>.detnav` (`nav-det-navmesh`: the deterministic navmesh a lockstep authority arms, `NVM2` + `u32` version, polygons with their links, canonical float bits; DO27's cooked navmesh copied whole, enters at v2; ADR-8174, [`docs/formats/nav-det-navmesh.md`](../formats/nav-det-navmesh.md)) | Regia Engine | v0.6.0 (v2 frozen on merge) |
| `.formation.ron` (`ai-formation`: named formation slots relative to the anchor's facing, with role tags, a spacing scale and a fill order; v1, `format_version`, `ai-formation` in `check-formats` and `regia upgrade-project`, [`docs/formats/formation-ron.md`](../formats/formation-ron.md), ADR-8358) | Regia Engine | v0.6.x (v1 frozen on merge) |
| `.track.ron` (`key-track`: a scripted key track for `regia play-gate`; RON, v1, ADR-8403, [`docs/formats/key-track-ron.md`](../formats/key-track-ron.md); the version is read before the full parse and unknown fields are refused, ADR-8553) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `.contract.ron` (`delivery-contract`: per-class scale, axes, origin, LOD chain and triangle budgets, plus joints, required nodes, sockets, clips, materials, textures, footprint and socket clearance; v1, ADR-8401, ADR-8450, [`docs/formats/contract-ron.md`](../formats/contract-ron.md); must merge before the v0.6 freeze; unknown fields are refused, ADR-8553) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `.playlist.ron` (`playlist-ron`: dedicated-server playlist rows of map, mode and options; v1, ADR-8345, [`docs/formats/playlist-ron.md`](../formats/playlist-ron.md); `regia upgrade-project` recognizes it and refuses any other version; the version is read before the full parse, unknown fields and an empty `map_id` are refused, ADR-8553; v2 (format batch 2, ADR-10064): same shape, unknown fields inside `order` refused, the fingerprint covers the `order` variants and payloads; v1 loads as v2 and `regia upgrade-project` rewrites its version line; corpus `playlist-ron/v1.ron` and `v2.ron`; fuzz target `net.playlist_ron`) | Regia Engine | v0.6.0 (v1 frozen on merge); v0.7.0 (v2, batch 2) |
| `server.toml` v4 (`server-toml`: one v4 holds the silence policy `[net] interrupt_ms`, `timeout_ms`, `start_grace_ms` (ADR-7950), the optional `[net] mtu_probing` (ADR-8300) and the optional `levels.playlist`, exclusive with `levels.rotation` (ADR-8345) and the optional `[lockstep]` section (ADR-8558), folded into the unreleased v4 by ADR-8529, ADR-8553 and ADR-7724; v1, v2 and v3 still load and `regia upgrade-project` rewrites them to v4; corpus `server-toml/v4.ron`, `v4-playlist.ron` and `v4-lockstep.ron`, [`docs/formats/server-toml.md`](../formats/server-toml.md)) | Regia Engine | v0.6.0 (v4; v1–v3 load through the migrator) |
| `.assetids.ron` (`asset-ids`: the committed id of each source whose `.meta` is not in version control; v1, ADR-8460, [`docs/formats/assetids-ron.md`](../formats/assetids-ron.md); unknown fields refused, version read first; two rows equal under case folding are refused and an import refuses two sources on disk with one id, ADR-8553) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `*.desync.ron` (`desync`: the lockstep desync bundle, a diagnostic file, spec 120 § 7.6; v1; registered by ADR-8455, [`docs/formats/desync-ron.md`](../formats/desync-ron.md)) | Regia Engine | v0.6.x (frozen on merge) |
| cooked terrain index (`terrain-index`, `terrain/index` in a cooked map; postcard; v2 adds optional exact heights, holes and a photoreal material (ADR-8024) and then the count mapping (ADR-8471), one v2 (ADR-8619), v1 migrates; v3 (v0.7, ADR-8871) adds the source grid's Z spacing and valid-data mask, v1 and v2 migrate) and final heights (`terrain-final-heights`, `terrain/heights`, v1, ADR-8024; [`docs/formats/terrain.md`](../formats/terrain.md) § 14) | Regia Engine (cooked) | v0.6.0 (frozen on merge) |
| `config/audio.ron` (`audio-buses`: the project bus tree: buses with volume, mute, effect chain and post-fader sends, send buses, named snapshots; v0 = the versionless shape v0.5.0 shipped, migrated by inserting the version; v1 adds `version`, `snapshots` and the `Bypassed` effect, ADR-8105 (g), ADR-8354, [`docs/formats/audio-buses-ron.md`](../formats/audio-buses-ron.md)) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `config/source_lint.ron` (`source-lint`: project source-lint policy, version 1, ADR-8248, [`docs/formats/source-lint-ron.md`](../formats/source-lint-ron.md); evidence reports are a separate versioned JSON contract, not this file) | Regia Engine | v1 (new file; `config/validation.ron` is unchanged) |
| `release-chunks.json` (`release-chunks`: chunks under a byte cap, deterministic ustar then zstd, JSON plus TSV and SHA-256; v1, ADR-8404, [`docs/formats/release-chunks.md`](../formats/release-chunks.md); `regia upgrade-project` owns the whole file name) | Regia Engine | v0.6.0 (v1 frozen on merge; must merge before the v0.6 freeze) |
| Finished-match record (`match-result`: `<root>/<utc-date>/<match_id>.json`, JSON `version` 1; host-written, read back as untrusted by `rules.match_results`, `rules.match_result` and `regia server results`; fuzz target `match_result_json`; `regia upgrade-project` probes it by its `format` key; corpus `match-result/v1.json`, [`docs/formats/match-result.md`](../formats/match-result.md); unknown keys refused in every object, a timeline row included, ADR-10064) | Regia Engine | v0.7.0 (v1 frozen in format batch 2, ADR-10064) |
| `.hoverhull.ron` (`hoverhull`: one named hover hull, optional `HoverParams` and `HoverLimits` fields filled from the engine defaults, optional flight-mode name; host-parsed text, lockstep handshake is `HoverParams::to_le_bytes`; v1, ADR-8512, [`docs/formats/hoverhull.md`](../formats/hoverhull.md)) | Regia Engine | v0.6 (v1 frozen on merge) |
| `.bike.ron` (`bike`: one named two-wheel bike, every `BikeConfig` field optional and filled from `BikeConfig::default()`, an optional note; tuned in the Vehicle Tuning panel; v1, ADR-8630, [`docs/formats/bike-ron.md`](../formats/bike-ron.md)) | Regia Engine | v0.6 (v1 frozen on merge) |
| `.detutility.ron` (`ai-det-utility`: a deterministic utility catalogue, inputs and options by name with their considerations and response curves; RON, v1, `format_version` peeked before the typed parse, unknown fields refused, `ai-det-utility` in `check-formats` and `regia upgrade-project`, ADR-8609, [`docs/formats/ai-det-utility.md`](../formats/ai-det-utility.md)) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `heightfield.raster.json` (`terrain-raster-descriptor`: how a count raster sits in the world: X/Z spacing, stored row and column order, sample-center or cell-edge registration, origin, nodata count and valid mask; JSON, `"raster_descriptor": 1`, unknown fields refused; ADR-8542, registered by ADR-8639, [`docs/formats/terrain.md`](../formats/terrain.md) § 14, schema [`terrain-raster-descriptor.schema.json`](../formats/terrain-raster-descriptor.schema.json); a hashed cook input) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `heightfield.tiles.json` (`terrain-source-tiles`: a heightfield source in several files: the global count grid, its units, `max_overlap` and each tile's path, origin and size; JSON, `"terrain_source_tiles": 1`, unknown fields refused; ADR-8595, registered by ADR-8639, [`docs/formats/terrain.md`](../formats/terrain.md) § 15, schema [`terrain-source-tiles.schema.json`](../formats/terrain-source-tiles.schema.json); the descriptor and every tile are the cook's ground hash. The overview descriptor `<name>.overview.json` (§ 16) is a cache and is not frozen) | Regia Engine | v0.6.0 (v1 frozen on merge) |
| `.rules.ron` (`rules-ron`: a rule set, a match mode as data: skeleton, `extends` (at most 4 ancestors), teams, session overrides field by field (since ADR-8748 also the genre sections `waves`, `race`, `zone`, `elimination` and `victory`, grown inside unreleased v1 under the ADR-8619 rule), win conditions and a typed option schema; v1, ADR-8694, [`docs/formats/rules-ron.md`](../formats/rules-ron.md); the version is read before the typed parse, unknown fields are refused (also inside the shared settings values, ADR-8799), the schema fingerprint includes tagged-enum payload fields (ADR-8799), every float is read straight to `f32`, at most 256 KiB; `regia upgrade-project` checks v1 and refuses any other version; fuzz target `ruleset_ron`; option `bind` added inside unreleased v1, ADR-8881; corpus `rules-ron/v1.ron`, `rules-ron/v1-sections.ron` and `rules-ron/v1-binds.ron`) | Regia Engine | v0.7.0 (v1 frozen in format batch 2, ADR-10064) |
| `*.lsreplay` (`lsreplay`: the lockstep match replay, binary; v1; registered by ADR-8513, [`docs/formats/lsreplay.md`](../formats/lsreplay.md)) | Regia Engine | v0.7.0 (format batch 2; v1 frozen on merge) |
| `hub.toml` (`hub-toml`: lobby hub listen port, key path, caps and per-address and per-identity rate limits; v1, `hub-toml` in `check-formats` and `regia upgrade-project`, [`docs/formats/hub-toml.md`](../formats/hub-toml.md), ADR-8648) | Regia Engine | v0.6.x (v1 frozen on merge) |
| `manifest-<platform>.json.sig` (`release-signature`: the detached Ed25519 signature of a release manifest: format, algorithm, key id, public key, date, manifest SHA-256, signature; JSON, `"format": 1`, unknown fields refused; ADR-8687, registered by ADR-8846, [`docs/formats/release-signature.md`](../formats/release-signature.md); `regia upgrade-project` lists `.json.sig` files that read as this format) | Regia Engine | v0.7.0 (v1 frozen on merge; format batch 2) |
| `.gait.ron` (`gait`: a walker's gait settings: patterns (a preset or explicit offsets and duty) with switch speeds, speed limits, step, lift, clearance, slope and reach limits, settle and turn thresholds, foothold search, ground band, optional posture, the body joint and the legs by joint name with optional hip offset and reach overrides; RON, v1, `format_version` read before the typed parse, unknown fields refused, checked by the stepper's own `GaitParams::validate`, shipped as text and hot reloaded, `gait` in `check-formats` and `regia upgrade-project`, gap G7, [`docs/formats/gait-ron.md`](../formats/gait-ron.md)) | Regia Engine | v0.7 (v1 frozen in v0.7 format batch 2, ADR-10266) |
| `*.netcapture` (`netcapture`: a client capture with timing: socket reads, decrypted connection packets, the server's accept, refusal or goodbye, input stamps, flushes and frame markers, in the client's own milliseconds; binary, magic `RNETCAPT`, v1; spec 120 § 15.5, ADR-8221, landed by ADR-8890, [`docs/formats/netcapture.md`](../formats/netcapture.md); `regia upgrade-project` lists it and never rewrites it; corpus `netcapture/v1.bin`, checked by `regia_net`'s `format_corpus` test; row recorded in `format-hashes.ron` by format batch 2's freeze, ADR-10183) | Regia Engine | v0.7.0 (v1 frozen in format batch 2) |

**Old-bytes corpus (ADR-2743).** "The loader keeps reading every older layout" is checked with old bytes:
`crates/regia_agent/tests/fixtures/format-corpus/<format>/v<N>.<ext>` holds a fixture for every version of every
frozen format (old versions are real bytes from history or hand-written from the old struct, marked in the
folder's `index.ron`; never output of today's writer). Each loads, migrates to the current version, reads back
equal, migrates idempotently, and a copy patched to current + 1 is refused with an error naming the version.
`cargo xtask check-formats` fails when a registered format has no fixture for a version from v1 to its current one,
unless `docs/formats/format-corpus-exceptions.ron` lists it with a reason. A bump keeps the old current fixture
and adds a new one (`book/changing-a-content-format.md`).

Rules: every Regia Engine format carries `format_version`; after its freeze milestone a format only changes
with a migrator run by `regia upgrade-project` (tested on DO27's content); each format gets a JSON
Schema (from reflection) and an example published in `docs/formats/` so generators can target it
before the editor exists. Cooked outputs that ship in paks (the cooked mesh) follow the same rule: a
layout change bumps the payload `format_version`, the loader keeps reading every older layout, and
`upgrade-project` rewrites cooked files in the dev cache. Until a format is frozen, generators should emit neutral sources plus a
small parameter file the generator can re-emit once the format lands.

"Usable now" for glTF means the glTF 2.0 standard as written: metres, +Y up, asset front facing +Z,
metallic-roughness materials. That is what Blender's glTF exporter produces from a model whose nose
points along Blender's −Y, so generators keep modelling as they do today; the importer turns the asset to
Regia Engine's −Z forward (`02-architecture.md` § 3, ADR-009). The remaining conventions the importer relies
on — node naming for authored LODs, sockets and collision meshes, texture channel packing — are
published as `docs/formats/gltf-conventions.md` at the start of M3 and are frozen with the other M4
formats.

**As planned for v0.6.0 (ADR-7724).** One format bump: each frozen format moves at most one version step from the
version v0.5.0 shipped; a format already one step ahead may take a later additive change folded into that unreleased
version (its frozen fixture stays and keeps loading; a second fixture of the same version covers the new part).
`cargo xtask check-formats` checks the rule against `docs/formats/format-hashes-v0.5.0.ron` (`--since-tag` names another
baseline). Since format batch 2's freeze (ADR-10183) the default baseline is `docs/formats/format-hashes-v0.6.0.ron`,
the v0.6.0 tag's table: the v0.7 line moves each format at most one step past v0.6.0.

| Format | v0.5.0 → v0.6.0 | Migrator | `upgrade-project` | Status |
|---|---|---|---|---|
| `audiopatch` | 1 → 2 | number-only | yes | on the line (ADR-7226) |
| `cooked-clip` | 2 → 3 | binary (old clips → `Raw` v3) | yes | on the line (ADR-7227) |
| `pgout` | 3 → 4, plus the prefab-override section folded into v4 | binary | yes | v4 on the line (ADR-7214); overrides planned (ADR-2724) |
| `procgen-painted-points` | 1 → 2 | reflected component step | yes (embedded) | on the line (ADR-7215) |
| `profile-capture` | 1 → 2 | read as current, write | yes | on the line (ADR-7325) |
| `ui` | 10 → 11, plus spec 81 § 10 items folded into v11 | number-only | yes | v11 on the line (ADR-9980); additions planned |
| `vfx` | 7 → 8 | load, re-save | yes | on the line (ADR-7210) |
| `motiondb-cooked` | 2 → 3 | cache: v1/v2 still load | no (re-cooked) | planned (staged 3001) |
| `bake` | 2 → 3 (`extra_suns`) | number-only | yes | planned (staged 3002) |
| `nav-cell` | 2 → 3 (conservative rasterisation, ADR-7558) | cache: v1/v2 still load | no (re-cooked) | planned (w11-fmt-nav-cell-v3) |
| `shots` | 2 → 3 (key times past the shot end, ADR-8640) | number-only | yes | planned (w11-seq-preview-fixes item 4) |
| new v1 | `abilities-cooked`, `content-state`, `data-asset` (v0 editor record migrates, ADR-7257), `glyphs`, `gpu-breadcrumbs`, `logic`, `regiatrace`, `viscells` | first version | per format | on the line or merging |
| `matgraph` | 4 → 5 (the WGSL Code node, ADR-7852) | number-only | yes | on the line (w12-shader-code-node) |
| `matfunc` | 3 → 4 (a Code node in a function) | number-only | yes | on the line (w12-shader-code-node) |
| `terrain-mesh-layers`: `<map>.meshlayers.ron`, the mesh layers and world geometry source, with the `<map>.passages.ron` a `Passages` layer names (same family and version) (ADR-8019 on ADR-7974, spec 121 § 9.1, built by ADR-8112; [`docs/formats/terrain.md`](../formats/terrain.md) § 12) | new v1 | none at v1; a newer file is refused | yes | on the line (lane W-06, ADR-8112) |
| `terrain-mesh-layers-cooked`: `<map>.meshlayers`, the cooked canonical world geometry (i32 mm) and layer chunks (ADR-8019, spec 121 § 9.2, built by ADR-8112; [`docs/formats/terrain.md`](../formats/terrain.md) § 13) | new v1 | cache: a mismatch recooks | listed (derived data, never migrated) | on the line (lane W-06, ADR-8112) |
| `editor-layout` | 2 → 3 (panel instances: `<panel>#<n>` tab ids, per-instance `instances` state, P-003) | number-only, from 1 and 2 | yes (editor-format row) | proposed (w13, ADR-8105 (a); job 3605) |
| `matgraph` | v5 fold: `ParamDecl` `category`, `tooltip`, `range` (min, max, step), P-169 | none new | yes | proposed (w13, ADR-8105 (b); job 3613; needs v5 from integ-v06l) |
| `sequence` | 5 → 6 (`Event` channel keys; `Track::locked`; solo not saved, P-210/P-217/P-218) | number-only | yes | proposed (w13, ADR-8105 (c); job 3618) |
| `uisettings` | 1 → 2 (`scale_mode`, the project scale rule, P-256) | number-only | yes | built (w13-ui-uisettings-screen-fields, ADR-8252; ADR-8105 (d)) |
| `theme` | 1 → 2 (`focus_visible` layer and structured style `rules`, one step, P-272/P-257) | number-only | yes | proposed (w13, ADR-8105 (e); job 3624) |
| `ui` | v11 fold (not v12): `back`, `disabled_subtree`, `pivot` (P-266, P-273, P-253) | none new | yes | built (w13-ui-uisettings-screen-fields, ADR-8252; ADR-8105 (f)) |
| `audio-buses` (`config/audio.ron`) | new v1; the versionless shape is v0 (`Bypassed` effects, `snapshots`, P-227) | v0 → v1 inserts `version` | yes (new row) | proposed (w13, ADR-8105 (g); job 3626) |
| `vfx` | v8 fold: `SystemModule::bounds`, authored fixed bounds (P-225) | none new | yes | proposed (w13, ADR-8105 (h); job 3627) |
| `logic-graph-ref` | new v1 (reflected component for prefab logic, P-165; `scene` stays 13) | first version | no (scene-embedded) | proposed (w13, ADR-8105 (N2); job 3615) |
| `key-track` | new v1 | none (v1 is the first version) | leaves v1 | this lane (ADR-8403); must merge before the v0.6 format freeze; version peek and unknown-field refusal (ADR-8553) |
| `delivery-contract` | new v1 (`.contract.ron`); kit fields folded into v1 before any tag (ADR-8450) | none (first version; serde defaults, no migrator) | yes | must merge before the v0.6 freeze (ADR-8401, ADR-8450); unknown fields refused (ADR-8553) |
| `voxel-field`: `<map>.voxfield`, the integer voxel field of DO27's D2 volumes (magic `VXFD`; not `.fvox`, which is the voxel far proxy; ADR-8116, spec 122 § 13) | new v1 | a migrator table from v1 on; a newer file is refused | yes (rewrites older versions; v1 is left as is) | proposed (lane V-02; `docs/formats/proposed-formats.ron`); line later, after DO27's API sign-off |
| `server-toml` | 3 → 4: silence policy (ADR-7950) and `[net] mtu_probing` (ADR-8300), plus `levels.playlist` (ADR-8345) folded into the same v4 (no v5) | version-line rewrite | yes | on the line (ADR-8529, ADR-8553); corpus `v4.ron` (silence keys) and `v4-playlist.ron` (playlist, MTU) |
| `playlist-ron` | new v1 (`.playlist.ron`) | none (first version); any other version is `no migrator from playlist.ron vN` | yes (refuses other versions) | on the line (ADR-8345); version peek and unknown-field refusal before the freeze (ADR-8553) |
| `asset-ids` | new v1 (`.assetids.ron`) | none (first version, `unmigrated`) | yes | on the line (ADR-8460); case-fold duplicate rows and shared ids of present sources refused before the freeze (ADR-8553) |
| `terrain-index` | 1 → 2 (exact heights, holes, pak-backed material; ADR-8024); new `terrain-final-heights` v1 | loader migrates v1; cooked `.fckd` rewritten | yes (probe by type tag) | on the line (ADR-8024) |
| `terrain-editlayers` | 1 → 2 (material weights and a named procgen target, ADR-8381) | binary (v1 heights kept bit for bit) | yes | on the line (ADR-8381) |
| `release-chunks` | new v1 | `migrate_json` re-encodes v1; a newer file is refused | yes (`release-chunks.json`) | this lane (ADR-8404); must merge before the v0.6 freeze |
| `meta` (`<asset>.meta`) | 1 → 2 (the importer setting `lossless_clips`, ADR-7819) | `migrate_meta`: a v1 `settings` drops a `lossless_clips` key no v1 importer honored, otherwise number-only | yes | on the line (ADR-8588 state at integ-v06r 6fe730622, as every row below); **replaces** ADR-7724 row (c)'s "no bump" and the "Not format changes" sentence below for the lossless clip switch |
| `net-settings` (`config/net.ron`) | 2 → 3 (the silence policy: `interrupt_ms`, `timeout_ms`, `start_grace_ms`, ADR-7950) | `migrate_net_settings` | yes | on the line (not in ADR-7724's table) |
| `server-toml` (`server.toml`) | 3 → 4 (the silence policy, ADR-7950; `net.mtu_probing`; `levels.playlist` folded into v4, ADR-8529) | `migrate_server_toml` | yes | on the line (not in ADR-7724's table) |
| `test-report` (the functional-test report JSON) | 1 → 2 (optional per-test `notes`, ADR-7980) | the loader reads v1 (`test_report_migration_sources`) | **no row yet**: `regia upgrade-project` and `format_corpus_upgrade` need a row or a reason (freeze blocker; lane v06-format-corpus-upgrade-reds, PARTIAL) | on the line |
| `editor-layout` | 2 → 3 (the row above) | `editor_migrator`, from 1 and 2 | yes (editor-format row) | on the line (job 3605; ADR-8105 (a)) |
| `sequence` (`.seq.ron`) | 5 → 6 (the row above) | `migrate_sequence`, number-only | yes | on the line (job 3618; ADR-8105 (c)) |
| `theme` (`.theme.ron`) | 1 → 2 (the row above) | `migrate_theme`, number-only | yes | on the line (job 3624; ADR-8105 (e)) |
| earlier rows of this table | `motiondb-cooked` 3, `bake` 3, the `matgraph` `ParamDecl` fold, the `pgout` override fold (ADR-7750), the `ui` v11 spec 81 § 10 fold | as above | as above | on the line (ADR-7724 rows a, b, h, i, l, m landed) |
| new v1 on the line, not listed above | `asset-collections`, `asset-ids`, `assetpkg`, `import-presets`, `launch-profiles`, `layer-file`, `level-variant`, `node-tracks`, `playlist-ron`, `regia-det-golden-1`, `version-probe`, `tilepatterns`, `terrain-editlayers`, `ai-formation`, `editor-panel` and the `editor-*` user files (`editor-keymap`, `editor-prefs`, `editor-profile`, `editor-theme`, `editor-view-state`, `editor-window-state`, plus `inspector-pins`, `saved-searches`, `viewport-snap`) | first version | per format | on the line |
| `nav-det-navmesh` (`<map>.detnav`) | new at v2 (DO27's v2; no v1 exists, ADR-8174) | first version | per format | on the line |
| `shots` (`.shots.ron`) | 2 → 3 (ADR-8640; ADR-7724 row e) | `migrate_shots` (source 2 added) | yes | landing: w11-seq-preview-fixes fc02c2e2b, PARTIAL (the code writes v3; the branch's `format-hashes.ron` is not updated) |
| `uisettings` (`.uisettings.ron`) | 1 → 2 (`scale_mode`, the project scale rule), with the `ui` v11 fold of `back`, `disabled_subtree`, `pivot` (the rows above) | `migrate_uisettings` | yes | landing: w13-ui-uisettings-screen-fields b88ac51b4, DONE, not merged (ADR-8252) |
| `audio-buses` (`config/audio.ron`) | new v1 (the versionless shape is v0), plus the send `tap` fold (pre/post fader) | `migrate_audio_buses` (v0 → v1) | yes | landing: w13-ui-audio-mixer-chain 74954b57f, PARTIAL (ADR-8354); the tap fold: lane v06-audio-bus-prefader-sends |
| `vfx` (`.vfx.ron`) | v8 fold: `SystemModule::bounds` (the row above) | none new | yes | landing: w13-ui-vfx-bounds 4e5fad718, PARTIAL (ADR-8355; the branch's hash is not updated) |
| `terrain-editlayers` (`<map>.terrain.editlayers`) | 1 → 2 (material weights, a named procgen target; v1 never released) | `migrate_edit_layers` | yes | landing: w13-ui-terrain-layers-v2 110b749b9, PARTIAL (ADR-8381) |
| `logic-graph-ref` | new v1 (the row above; `scene` stays 13) | first version | no (scene-embedded) | landing: w13-ui-prefab-logic c6e910b24, RUNNING (ADR-8566; needs w12-logic-debugger merged) |
| `hoverhull` (`.hoverhull.ron`) | new v1 | `migrate_hoverhull` | yes | landing: buildout-mp-hover-hull-format b27b6517a, RUNNING (ADR-8512) |
| `bike` (`.bike.ron`) | new v1 | `migrate_bike` (brief) | yes | not started: staged v06-bike-hover-tuning; v0.7 if the freeze comes first |
| `source-lint` (`config/source_lint.ron`) | new v1 | `unmigrated` | per format | landing: integ-content-a 99cbcf6d5 (ADR-8248); waits for the content-b roll-up |
| `desync` (the lockstep desync bundle) | new v1 (spec 120 § 7.6, ADR-8455) | first version | per format | landing: integ-v06r-net f7652994c, which merges into the v0.6 line last |
| `netprofile` (`.netprofile.ron`) | 1 → 2 (distance rate tiers, ADR-8031) | `migrate_net_profile` | yes | landing: integ-v06r-net f7652994c |
| `retarget` (`.retarget.ron`) | 1 → 2 (IK goals at retarget bake, ADR-8476) | `migrate_retarget` | yes | landing: integ-v06r-det d935c5fd8 |
| `release-chunks` (`release-chunks.json`) | new v1 | `migrate_release_chunks` | yes | landing: release-chunks 3d9d27d03, DONE (ADR-8404) |
| `decal` (`.decal.ron`) | 2 → 3 (custom textures) | `migrate_decal` | yes | landing: decal-custom-textures c973bab8d, PARTIAL, integration owed (ADR-8121) |
| `terrain-index`, with `terrain-final-heights` | `terrain-index` 1 → 2 (v2 shipped unregistered in v0.5.0); `terrain-final-heights` new v1 (cooked) | `migrate_terrain_index`; final heights are a cache (recooked) | yes; no | landing: do27-terrain-cooked-photoreal 991d2edff, PARTIAL (ADR-8024) |
| `delivery-contract` (`.contract.ron`) | v1 fold: kit fields (the hash moves at the same version; v1 never released) | none new | yes | landing: delivery-contract-b a8dc1ce7b, RUNNING (ADR-8450) |
| `nav-cell` | 2 → 3 (the row above) | cache | no | **owner hold** (ADR-7724 row d: may v0.6.0 re-cook DO27's navigation?); the registry is at v2 on the line; v0.7 unless the owner answers before the freeze |
| `scene` (`.level.ron`, `.prefab.ron`): TerrainMap cell-centered sample spacing (ADR-7724 row g, plan W-24) | 13 → 14 (v0.7 format batch 2): `LevelTerrain::sample_layout` (`VertexCentered` default, `CellCentered`), with the host API `TerrainMap::with_sample_layout` (ADR-10079) | `migrate_v13_to_v14`, number-only (`respell_level_settings`: a v13 terrain block gains `sample_layout: VertexCentered`) | yes (`scene-level`, `scene-prefab`) | landing: v07-fmt-terrainmap-spacing (ADR-10079); corpus `scene/v14.ron` and `scene/v14-cell-centered.level.ron` |
| `voxel-field` | new v1 (the proposed row above) | — | — | v0.7: after DO27's API sign-off (ADR-8116) |
| `audio-buses` v1 fold: send tap (optional per-send `tap: PostFader \| PreFader`, default `PostFader`; P-227) | none new (stays v1) | none (additive, `serde(default)`) | yes (unchanged row) | proposed (v06-audio-bus-prefader-sends, ADR-8590; corpus `audio-buses/v1-prefader.ron`; must merge before the v0.6 format freeze) |
| `ai-det-utility` (`.detutility.ron`, the deterministic utility catalogue, ADR-8609; [`docs/formats/ai-det-utility.md`](../formats/ai-det-utility.md)) | new v1 | none (v1 is the first version); a newer file is refused | yes | must merge before the v0.6 format freeze (ADR-8609) |
| `terrain-raster-descriptor`: `heightfield.raster.json`, a count raster's layout (ADR-8542; [`docs/formats/terrain.md`](../formats/terrain.md) § 14) | new v1 | none at v1; a newer file is refused (`terrain_raster_version`) | yes (listed, v1 left in place) | on the line (ADR-8639) |
| `terrain-source-tiles`: `heightfield.tiles.json`, a heightfield in several files (ADR-8595; [`docs/formats/terrain.md`](../formats/terrain.md) § 15) | new v1 | none at v1; a newer file is refused (`terrain_tiles_version`) | yes (listed, v1 left in place) | on the line (ADR-8639) |
| `nav-cell` (v0.7, format batch 2; owner approved 2026-11-19) | 2 → 3: conservative rasterisation, the byte layout and hash unchanged (ADR-8899; the held row above) | cache: v1/v2 still load (warning, re-cook) | no (re-cooked by `regia cook`) | built (w11-fmt-nav-cell-v3, ADR-8899); corpus `nav-cell/v3.bin`; DO27 re-cooks its navigation |
| `release-signature`: `manifest-<platform>.json.sig`, a release manifest's detached signature (ADR-8687; [`docs/formats/release-signature.md`](../formats/release-signature.md)) | new v1 (format batch 2, v0.7) | none at v1; a newer file is refused (`release_signature_newer`) | yes (listed, v1 left in place) | this lane (v07-release-sign-hosting, ADR-8846) |
| `gait` (`.gait.ron`, gait settings, gap G7; [`docs/formats/gait-ron.md`](../formats/gait-ron.md)) | new v1 | none (v1 is the first version); a newer file is refused (`gait_version`) | yes (listed, v1 passes through) | frozen in v0.7 format batch 2 (ADR-10266; built by buildout-anim-gait-format, the v0.6 batch closed at 240 formats); corpus `v1.ron` plus `v1-hexapod-switching.ron`, `v1-quadruped.ron`, `v1-biped.ron` cover every pattern variant and optional field; version and hash unchanged |

Not format changes (no bump): the per-asset lossless clip cook switch and the fracture support box (both importer
settings inside the `.meta` `settings` string). Needs DO27: TerrainMap cell-centred sample spacing. Not in v0.6.0:
field-level optional references (ADR-7236).

Changed before the v0.6 freeze, no version bump (the format is unreleased): `terrain-mesh-layers-cooked` v1's section
zones store the canonical yaw (the lockstep `atan2`: no `-0`, no `-π`), half extents through `m_to_mm` and a
`reverb_size_m` that is never `-0`, so the cooked section bytes and `content_hash` of every map with sections moved
(ADR-8579; the pins were re-cut on x86-64 and aarch64).

**State of the rows (ADR-8588, 2026-11-18).** The rows from `meta` on were appended at integ-v06r 6fe730622. The
lossless clip cook switch is no longer "no bump": it became `meta` 1 → 2 (ADR-7819). The fracture support box is still
no bump. A "landing" row lands before the lead freezes the v0.6 hashes or moves to v0.7; the lead drops or moves its
row when the freeze list is final.

**v0.7 batch (formats added after the v0.6 freeze).** These rows ship in the v0.7 format window; the lead freezes
their hashes with the v0.7 list.

| Format | Change | Migrator | `upgrade-project` | Status |
|---|---|---|---|---|
| `rules-ron` (`.rules.ron`) | new v1 (a rule set: a match mode as data, ADR-8694; genre sections `waves`, `race`, `zone`, `elimination`, `victory` added inside unreleased v1, ADR-8748) | none (first version); any other version is `no migrator from rules.ron vN` | yes (parses and checks v1, refuses other versions) | frozen in format batch 2 (ADR-10064; ADR-8799 and ADR-8879 strict readers and fingerprint, ADR-8881 option `bind`); corpus `rules-ron/v1.ron`, `rules-ron/v1-sections.ron`, `rules-ron/v1-binds.ron` |
| `playlist-ron` (`.playlist.ron`) | 1 → 2 (same shape; unknown fields inside `order` refused; the fingerprint gains the `order` variants and their payloads, ADR-10064) | number-only (`migrate_playlist_ron` rewrites the version line; the loader reads v1 as v2); sources `[1]` | yes (rewrites v1 to v2, refuses other versions) | frozen in format batch 2 (ADR-10064); corpus `playlist-ron/v1.ron`, `playlist-ron/v2.ron` |
| `match-result` (finished-match JSON) | new v1 (ADR-8652, ADR-8880; timeline rows refuse unknown keys, ADR-10064, no hash move) | none (first version; the stub refuses every `from`) | yes (probes the `format` key, reports the version, never rewrites) | frozen in format batch 2 (ADR-10064); corpus `match-result/v1.json` |

**Format batch 2 (v0.6.0 → v0.7.0; owner-approved 2026-11-19).** One step per format, each with a migrator from the
version v0.6.0 shipped (ADR-7724's rule; freeze Sat 2026-11-28, `queue/staged/v07/FORMAT-BATCH-2.md`). A second
change to a format already bumped on the line folds into that step.

| Format | v0.6.0 → v0.7.0 | Migrator | `upgrade-project` | Status |
|---|---|---|---|---|
| `terrain-mesh-layers-cooked`: `<map>.meshlayers` (spec 121 § 9.2; [`docs/formats/terrain.md`](../formats/terrain.md) § 13) | 1 → 2: the canonical section stores a hashed head, then the lattice counts as little-endian `u16` (no varints); `content_hash` is the BLAKE3 of the head, which holds one BLAKE3 per 64 KiB block of the counts | binary, in the loader (`cooked::migrate_canonical`: a v1 section decodes to the same value; its `content_hash` is the v2 hash) | yes (`cooked::migrate_file` rewrites the file; the cook pipeline recooks a v1 cache) | on the line (lane w11-index-build, ADR-8816; corpus `terrain-mesh-layers-cooked/v2.bin`) |
| `logic` (`.logic.ron`) | 1 → 2: optional typed `entity_vars` and `world_vars` (P-177, audit row L9) | number-only (`logic_migration_sources` `[1]`) | yes (`migrate_logic` replaces `unmigrated`) | built (v07-fmt-logic-uianim, ADR-8900); corpus `logic/v2.ron`, fuzz `logic-vars` |
| `uianim` (`.uianim.ron`) | 1 → 2: optional frame `events` per animation (the row ADR-8105 lacked) | number-only; a file without `format_version` is re-saved at v2 | yes (`migrate_uianim` replaces `unmigrated`) | built (v07-fmt-logic-uianim, ADR-8900); corpus `uianim/v2.ron`, fuzz `uianim-events` |
| `terrain-index`: `terrain/index` in a cooked map ([`docs/formats/terrain.md`](../formats/terrain.md) § 9b) | 2 → 3: two optional entries after `source_mapping`: `source_spacing_z_m` (the source grid's Z spacing when it differs from X) and `source_valid_mask` (`TerrainValidMaskIndex`: the samples without data, packed bits) | binary, in the loader (`TerrainIndex::from_postcard` decodes a v1 or v2 payload with its own shape; both entries `None`) | yes (`migrate_terrain_index`, probe by type tag; a full cook writes v3, an incremental cook keeps an unchanged terrain's v2 index, which loads) | on the line (lane v07-fmt-terrain-index-v3, ADR-8871; corpus `terrain-index/v3.bin`). The batch plan listed w11-index-build's fixed-width counts under this format: they are `terrain-mesh-layers-cooked` (row above), so this is the only `terrain-index` step |

**As planned for v0.7 (format batch 2, owner 2026-11-19; freeze Sat 2026-11-28; ADR-7724 one-step rule).**

| Format | Change | Migrator | `upgrade-project` | State |
|---|---|---|---|---|
| `gpu-mesh` (the GPU-ready mesh of desktop paks, `regia_asset::gpu_mesh`) | 2 → 3: an optional trailing block after the indices (magic `FGPX`, `u32` length, postcard skin + `skin_influences` + sparse morph targets) so skinned and morphing meshes are stored GPU-ready; stored tangents reused at load (ADR-10080) | the reader reads v1 and v2 in memory (sources `[1, 2]`); a cache: the next cook writes v3 | no: a pak entry, never a project file (`NOT_HANDLED` in the corpus-upgrade test) | lane v07-fmt-gpu-mesh-v3 (ADR-10080); corpus `v2.bin` kept, `v3.bin` added |

**As planned for v0.7 (format batch 2; freeze Sat 2026-11-28; ADR-7724 one-step rule: one version step per format
since v0.6.0, a later change folds into that step).** Rows are appended by each batch-2 lane.

| Format | Change | Migrator | `upgrade-project` | State |
|---|---|---|---|---|
| `decal` (`.decal.ron`) | 2 → 3: optional `texture` (custom cooked RGBA8 image, ADR-8121/8203) and, folded into the same step, optional footstep `surface` (ADR-8829); replaces the v0.6 `decal` row above (v0.6.0 shipped v2) | `migrate_decal_ron`, number-only from 1 and 2 | yes (`decal` row, unchanged) | landing: v07-fmt-decal-v3 (ADR-8829); corpus `decal/v3.ron` and `decal/v3-surface.ron`; the v0.6.0 loader refuses v3 with `decal prefab has version 3, but this engine reads version 2` |
| `lobby-keys` (the `regia.*` lobby data, not a file) | 1 → 2: optional `regia.mode` and `regia.playlist` (ADR-8897); a v1 lobby reads unchanged | none needed (v1 keys are a subset of v2) | no: lobby platform data, never a project file (`NOT_HANDLED` in the corpus-upgrade test) | on the line (playlist rotation, ADR-8897; integ-v07b ADR-10076); corpus `lobby-keys/v1.ron`, `v2.ron` |
| `lsreplay` (`*.lsreplay`, the lockstep match replay) | new v1 (ADR-8513) | none (first version); a newer file is refused | yes (listed, v1 left in place) | on the line (v07-net-land-replay-audit-stats); corpus `lsreplay/v1.bin` |
| `netcapture` (`*.netcapture`, a client capture with timing) | new v1 (ADR-8221, ADR-8890) | none (first version); a newer file is refused | yes (listed, never rewritten) | on the line (v07-net-land-capture-replay); corpus `netcapture/v1.bin`; its `format-hashes.ron` row was missing until the freeze (ADR-10183) |
| `hub-toml` (`hub.toml`, the lobby hub config) | new v1 (ADR-8648; not in the v0.6.0 table) | none (first version) | yes (`hub-toml`, v1 left in place) | on the line (lobby-hub-binary); corpus `hub-toml/v1.toml`, its `FROZEN.txt` line added by the freeze (ADR-10183) |
| `scene` (`.level.ron`, `.prefab.ron`) | 13 → 14: `LevelTerrain::sample_layout` (TerrainMap spacing, ADR-10079; the row in the v0.6 table above) | `migrate_v13_to_v14`, number-only | yes (`scene-level`, `scene-prefab`) | on the line; corpus `scene/v14.ron`, `scene/v14-cell-centered.level.ron` |
| net wire (`REGIA_NET_WIRE_VERSION`, a protocol constant, not a `format-hashes.ron` row) | 5 → 6 (one-round-trip resume, ADR-10067 channel ids) | none: peers of different wire versions refuse each other at the handshake | no (not a file) | on the line |
| `net-settings` (`config/net.ron`) | 3 → 4: `session.rate_control.first_flight` (ADR-10156's first flight, opt-in: default off by the lead's revised ruling, ADR-10231) | `migrate_net_settings`, number-only (source 3 added; a v1 to v3 file loads with the default) | yes (`net-settings`) | on the line (integ-v07-net); corpus `net-settings/v4.ron` |
| `netcapture` (`.netcapture`, new in v0.7) | 1, folded: record kind 11 `handshake` (the handshake's shortest round trip in front of the accept, so a replay anchors its clock as the run did; ADR-10231). Additive: a capture without the record loads and replays as before | none (the version does not move; the format has not shipped) | recognized, rewrites nothing (unchanged) | on the line (integ-v07-net); corpus `netcapture/v1.bin` unchanged; shape hash updated |
| Not on the line at the freeze run (ADR-10183): the Stage B sim-version step (`MATH_BITS_VERSION` still 1; v07-det-atan2-canon), `server-toml` 4 → 5 (only if the `--relay` off switch lands; v07-net-ki-hosting-lobby merged without it), `bake` (`.bake.ron`) 3 → 4 (only if v07-meshlayers-gi-export's ADR picks a shape change; not merged) | — | — | — | each lands with its own one-step row before the freeze run, or waits for batch 3 (v0.8) |
| `hub-toml` (`hub.toml`, the lobby hub config) | 1 → 2: `[relay]`, `[bans]` and `[log]`; a fresh file has `relay.enabled = true` and port 7812; the migrator writes `relay.enabled = false` so a v0.7.0 file does not open the relay port (ADR-10397). A missing `format_version` stays 1. AskReach keys under `[limits]` stay optional and are not inserted | `migrate_hub_toml`, number-only from 1 | yes (`hub-toml`) | on the v0.7.1 line (v071-net-hub-relay, ADR-10397); corpus `hub-toml/v1.toml` kept, `v2.toml` added; fuzz `net.hub_toml` |

## 5. Pak files

```
FPAK header: magic b"FPAK", version u32, flags, toc_offset u64, toc_len u64, content_hash [u8;32]
data: per-asset blobs, each aligned to 4096, compressed per block (lz4_flex, 256 KiB blocks) or stored
TOC (postcard): Vec<Entry { id, type_tag, offset, size, compressed_size, block_table_offset }>
               + manifest (path ↔ id) + pak dependencies
```

- Multiple paks mount in order; later paks override earlier ids (patches/DLC — Tier 2 feature, but
  the format supports it from day one). *As built:* the TOC's `dependencies` list orders mounting
  (ADR-5700, ADR-5877) — a stable topological pass puts every listed dependency before the pak
  that declares it (matched by case-folded file name, else stem; an unmatched entry is ignored with a
  debug log). When several mounted paks share the matched name or stem (a downloaded chunk whose
  file is named like a content pak), the entry binds to every one of them, so the dependent mounts
  after all of them (ADR-9705). The paks of a dependency cycle mount as one group, in the incoming order among
  themselves, after every outside dependency of the group and before any pak that depends on it
  (a warning names them); paks no edge constrains keep the content-manifest order, then the file
  name (case-folded), then the path. A chunk mounted at runtime (`mount_chunk_pak`: a download or a
  remount) goes in by that order too, then the whole list is re-sorted with the same rule. A patch pak declares its base in `dependencies`
  instead of needing a file name that sorts after it.
- Desktop: `memmap2` + decompress on IoPool. Android: AAsset streaming reads + decompress.
- *As built (ADR-7971):* compression is chosen per blob. With `compress`, the writer stores a blob raw
  (`compressed_size == size`, no block table) when lz4 does not make it smaller, when it would pass the
  reader's decompression-bomb ratio, or when it saves less than `PakWriteOptions::min_ratio` of the blob
  (default 0, any saving keeps lz4; `regia cook --ship` uses 0.15, so block-compressed textures stay
  raw and are read as views of the mapping). A chunk whose rule says `compress = false` (spec 40 § 2.2)
  is written raw. The format is unchanged: raw and lz4 entries could always mix in one pak.
- Chunking: `Regia.toml` may assign folders to chunks (`base`, `level1`, ...); one pak per chunk.
  Default single pak. Android AAB asset packs map to chunks (Tier 2).
- Fuzz the pak and cooked-header parsers (`cargo fuzz`, see testing doc).

## 5b. Streaming, load sets and memory budgets (instant-load, ADR-093)

- **Mapped paks.** `PakReader::open` keeps the `mmap` (no copy of the file; asset pages fault in on first
  read); entries are indexed by id; `prefetch(id)` issues `madvise(WILLNEED)` for one asset's bytes.
  Every reader of a pak (asset server, terrain tile source, `read_raw`) applies the same zero-copy gate
  (`pak_verify::gate_until_verified`); on Windows the mapped pak's handle denies other writers. Views
  served before a pak is cut in place by another process remain the POSIX trade-off of zero-copy loading.
- **`PakStreamer`.** Per-asset reads + lz4 decompress on a worker thread in request order, prefetching the
  next four requests; finished assets wait in a bounded channel (back-pressure, default 8) so a consumer
  that uploads under a frame budget slows the reader instead of growing RAM. Progress counters; failures
  are marked in the load-set registry.
- **`UploadQueue`** (`regia_render`, render-plugin resource, pumped once per frame before the windows
  draw): buffer writes and texture levels, written in bands of whole block rows within a per-frame time
  budget (1.5 ms default) at the measured upload rate (moving average; at least 256 KiB a frame), no single
  write over 1 MiB. The clock is checked after every write too, so a frame whose writes run slower than the
  estimate stops at the budget (overshoot ≤ one write). A finished upload runs its callback (typically
  `LoadSets::mark_resident`). Create streamed textures off the frame thread: a texture that needs a new
  device-memory block costs ~3 ms to create on NVIDIA (instant-load soak).
- **Load sets.** `LoadSet { name, items: [{ kind, key, ram_bytes, vram_bytes }] }` with kinds texture, mesh,
  material, terrain tile, audio bank, pipeline (a warm job label), other. `LoadSets` (asset-plugin resource,
  shareable across threads): `define`, `mark_resident / mark_evicted / mark_failed`, `is_resident(name)`,
  `load_progress(name)` (per kind, items and bytes), `pending(kind)`, `on_progress` callbacks (run after the registry lock is released, so they may call back
  into `LoadSets`). Asset items
  keyed by asset path follow the asset server's load state each `PreUpdate`; GPU-side owners (uploads, the
  terrain tile cache, the warm queue) mark their own. Agent `assets.load_progress`.
- **Memory budget.** `MemoryBudget { ram, vram }` per `BudgetTier` (minimum = RTX 2080 class: 6.5 GiB VRAM,
  10 GiB RSS; Deck 3 / 5 GiB; Desktop High 10 / 16 GiB); `memory_use(report, sets)` = measured GPU bytes by
  category + RSS + every set's estimated and still-pending bytes; `admit(budget, use, sets, name)` answers
  "can I preload this set now?" from the set's non-resident items. Agent `render.memory_budget`.
- **Staging flush** (ADR-1346). The renderer's queue writes (`write_texture`, `write_buffer`) go through
  wgpu's per-call staging, held until the next submit. Once the bytes written on a thread since its last
  submit reach the cap (128 MiB; `REGIA_STAGING_FLUSH_MIB`, `0` = off; `set_staging_flush_cap`), the
  pending writes are submitted on their own, and the frame path waits for its previous flush: a level's
  first frame holds about two caps of staging, not the level. Pixels are unchanged. Counters in
  `seeing.memory_report` `staging`.
- **Lightmaps per tier** (ADR-3001 decision 5, ADR-3120). A map's `.flmap` is in its load set; a package built for a
  tier (`[profiles.<name>] lightmap_tier`) ships that tier's lightmap, derived from the one bake, and leaves it out when its
  bytes pass the tier's `[lightmaps.tiers.<tier>] budget_mib` (the receivers keep the probe light). `regia cook
  --lightmap-tiers` reports the bytes per tier.
- Cooked-content layout for fast loads: `docs/formats/cooked-pack-guide.md`.

## 6. Asset database (pipeline)

- Lives in the editor and CLI; stored in `.regia/assetdb/index.bin` (postcard, rewritten atomically).
  It is a cache: deleting it triggers a full rebuild from sources + `.meta` files. No database dependency.
- Tracks per source: path, `AssetId`, source hash (blake3), settings hash, importer id + version,
  produced sub-assets, dependencies (e.g. material → textures, level → prefabs), cooked outputs per
  platform with their cache keys.
- Cache key = blake3(source bytes ‖ settings ‖ importer version ‖ target platform ‖ dependency cooked
  hashes when the importer embeds them). Identical keys reuse cooked output (also across branches).
  *As built (ADR-5611):* a `DbEntry` holds the source path, `AssetId`, source and settings hashes, importer
  id and version, one cache key and the cooked file; sub-asset ids live in the `.meta`. The DB key is
  blake3 of the source hash, settings hash, importer id and version (plus the ASTC policy revision on
  Android); platforms stay apart by their per-platform cooked directories and ledgers. Dependencies and
  the platform enter the ADR-530 skip key (`cook_inputs`), which hashes every file an importer reads.
- Full scan on editor start (parallel hashing, mtime+size shortcut), then incremental via `notify`.
  The mtime+size shortcut and an import skip keyed on every file an importer reads (glTF, images including EXR/HDR/SVG, audio) are
  ADR-530: an unchanged source is neither re-read nor re-imported.
- Reimport on change; dependents (anything referencing a changed asset whose cooked form embeds it)
  are re-cooked. Errors are recorded per asset and shown in the editor.
- CLI `regia cook --platform X` runs the same DB headless.
- **Shared cook cache** (ADR-2612, off by default): `Regia.toml` `[cook.shared_cache]` `path`, `mode = "read" |
  "read_write"` (or `REGIA_SHARED_COOK_CACHE`, or `regia cook --shared-cache PATH [--read-only]`) names a directory a
  team shares. The importers ADR-530 skips (glTF/GLB, images, DDS, sounds) fetch their cooked outputs from it under a
  key of every importer input plus the engine build id (no machine-local stamp, no absolute path), and read-write cooks
  publish what they import. Entries verify by hash; a corrupt one is re-cooked with a warning. The last summary is kept
  in the platform's cook ledger; `regia cook cache prune --max-size` trims it least recently used first.

## 7. Importers (Tier 1)

| Source | Produces | Notes |
|---|---|---|
| `.gltf`, `.glb` | Mesh(es), Material(s), Texture(s), Skeleton, AnimationClip(s), Prefab (node hierarchy) | Settings: `scale` (uniform, default 1, baked into the scene-root matrix with the forward yaw; positions and node translations scale, normals are renormalized; a non-finite or non-positive value cooks as 1), `forward: Convert | Keep` (default `Convert`, ADR-009: a 180° turn about +Y is baked into meshes, node transforms, inverse bind matrices and animation tracks, so glTF's +Z front becomes Regia Engine's −Z forward; camera and light nodes are compensated so they still look at the same things down their local −Z; `Keep` does not bake that turn), generate tangents (MikkTSpace) if missing, generate LODs (count, ratios) or use authored LOD meshes, import animations (y/n, clip splitting by time ranges; clips are imported from M12, when `regia_animation` defines their cooked type — M4 imports meshes, materials, textures, skeletons and skins), import materials (or remap to existing), import cameras/lights as entities, merge meshes by material (optional), collision generation (none/convex/trimesh/simplified convex decomposition — Tier 2 for decomposition). |
| `.png .jpg .tga .bmp` | Texture | Settings: usage (`Color` sRGB / `Linear` / `NormalMap` / `Mask` / `UI`), mips (yes/box/Kaiser), max size per platform, compression override, address modes, filtering, `premultiply_alpha`. |
| `.hdr .exr` | Texture (HDR) or Environment (cubemap + prefiltered specular mips + SH9 irradiance) | Equirect → cube conversion and GGX prefiltering on GPU in the importer (wgpu compute) with CPU fallback. |
| `.wav .ogg .flac .mp3` | Sound | Settings: `load_mode` (`Static` decoded PCM / `Streaming` keeps compressed Ogg-Vorbis), resample target, force mono, normalise, loop points. |
| `.ttf .otf .ttc .otc` | Font | Raw files the game reads through `assets://` (cosmic-text loads TTF). Cook and package subset a TrueType font to the shipped locales' characters in retain-glyph-ids mode; settings `subset: auto\|off\|locales([...])`, `extra_chars`, `keep_ascii`, `keep_punctuation`, `keep_pseudo`; CFF, collections and color fonts stay whole with a warning (ADR-2697, `book/agents/font-subset.md`). |
| `.wgsl` | Shader | Validated with naga at import; composition via naga_oil `#import`. |
| `.material.ron`, `.prefab.ron`, `.level.ron`, `.animgraph.ron`, `.ui.ron`, `.ftl`, `.data.ron` | Corresponding asset | Engine-native authoring formats; cooked to postcard. |

**Implementation status (ADR-2284 review, 2026-11-02):** the cook always converts (`forward: Convert`); `scale`, `forward: Keep`, camera/light compensation and camera/light import are not implemented on the cook (`Keep` exists only for direct `import_gltf` calls), and the texture, audio, font and WGSL settings below are partly planned: the cooked settings are those in `GltfImporterSettings`, `TextureImporterSettings` and `AudioImporterSettings` (audio: spec 95); `.hdr` and `.exr` cook through `hdr_import`; `.ttf .otf .wgsl` return `UnsupportedExtension`.

### 7.1 Texture compression

| Usage | Desktop | Android |
|---|---|---|
| Color (opaque) | BC7 (or BC1 at "low" quality setting) | ASTC 4x4 (low: 6x6, or 8x8 where 6 does not divide) |
| Color + alpha | BC7 (or BC3 sRGB at low quality when any base-level alpha is below 255) | ASTC 4x4 (low: 6x6 / 8x8) |
| Normal map | BC5 (RG, reconstruct Z) | ASTC 4x4: material maps in astcenc's normal mode (X in RGB, Y in A); terrain arrays as RG (decoded like BC5) |
| Mask/roughness/metal/AO packed | BC7 linear | ASTC 4x4 linear (low: 6x6 / 8x8) |
| Single channel | BC4 | ASTC 4x4 (low: 8x8) |
| HDR | BC6H | ASTC HDR **not** assumed → RGBA16F uncompressed (or RG11B10 for env maps) |
| UI | BC7 sRGB (BC3 sRGB at low quality when any base-level alpha is below 255, otherwise BC1 sRGB). RGBA8 only when `compress: Some(false)` or a side is under 4 px | ASTC or ETC2 sRGB (same `compress: Some(false)` and sub-block exception; not raw RGBA8) |

Every PNG/JPEG cooks this way **by default**, with a full mip chain (lane tex-budget, ADR-130). A glTF image takes the
usage of the material slot it fills; a standalone image takes it from its file-name suffix (`_n`, `_nrm`,
`_normal` → normal map; `_orm`, `_rough`, `_metal`, `_ao`, `_mask`, `_height`, … → linear; else colour). The image's
`.meta` `settings` override it, for the standalone cook and for every glTF that references the image:
`(usage: Some(color|linear|normal|mask|ui), max_size: Some(2048), compress: Some(false), quality: Some(high|low))`.
UI images keep BC7 unless `compress: Some(false)`. ASTC needs level 0 in whole blocks, so a block that does not
divide the edge steps from 6×6 to 8×8, and to 4×4 only when 8 does not divide.

**Android block sizes, by measurement** (lane android-astc, ADR-144; `regia_asset_pipeline` example `astc_survey` on
DO27's terrain layer sets, 16 maps per usage at the half-size level a mobile tier samples first, astcenc `medium`):

| Usage | BC (desktop) | ASTC 4x4 | 5x5 | 6x6 | 8x8 |
|---|---|---|---|---|---|
| Colour, PSNR dB mean / worst | BC7 51.7 / 43.5 | **53.6 / 47.2** | 47.6 / 38.7 | 42.7 / 32.6 | 38.4 / 26.6 |
| Linear (ORM), PSNR | BC7 47.6 / 40.3 | **50.2 / 42.6** | 45.8 / 38.6 | 43.1 / 36.3 | 40.3 / 33.3 |
| Normal, angle error ° mean / p99 | BC5 1.4 / 5.2 | **1.6 / 5.9** (RG layout 1.9 / 7.6) | 3.1 / 11.3 | 4.1 / 14.9 | 5.7 / 20.3 |
| One channel, PSNR | BC4 50.2 / 43.7 | **66.8 / 58.0** | 54.9 / 48.5 | 49.6 / 43.3 | 44.8 / 38.2 |

ASTC 4x4 matches or beats the desktop cook at the same 8 bits a texel; 6x6 (3.56 bits) costs about 9 dB on colour
(32.6 dB on rock, visible blocking) and nearly triples the normal error, to save a few MiB a layer array at the mobile
tiers. So the default (`quality: high`) is 4x4 for every usage and `quality: low` trades to 6x6 / 8x8. Every ASTC
chain runs down to 1×1 (levels smaller than a block are padded by the encoder). Terrain layer and macro arrays
(`cook_terrain_material(.., "android", ..)`) cook the same way; their normals use the RG layout because the terrain
shader reads every normal array as `.rg`. A desktop-cooked KTX2 can be re-encoded for Android level by level with
`transcode_bcn_ktx2_to_astc` (for pre-cooked packs whose sources are not at hand). Android cache rows key on the ASTC
policy revision, so a policy change recooks only them.

Pre-cooked KTX2 can also be shrunk without sources by dropping top mip levels (byte copy, no re-encode): proposed in
[`ktx2-level-drop.md`](ktx2-level-drop.md) (ADR-271).

**Runtime.** A device uploads every block format it can sample as it is (ASTC on phones, BC on desktop). A map on a
device that cannot sample its format is decoded to RGBA8 on the CPU, every cooked level (at least 4× the block chain,
about 9× for ASTC 6×6, 16× for ASTC 8×8 and 36× for ASTC 12×12; BC and ETC2 4×4 stay in 4–8×; slow loads),
when a decoder exists: BC1, BC3, BC4 unorm, BC5 unorm and BC7 through `regia_asset::bcn_decode`, ASTC LDR through
`regia_asset::astc_decode`, and unsigned ETC2 / EAC (RGB8, RGB8A1, RGBA8, EAC R11, EAC RG11) through
`regia_asset::etc2_decode` (ADR-2063; original decoders from the Khronos Data Format Specification, the ASTC one
bit-exact with astcenc); terrain arrays take the same fallback. Formats without a decoder stay unbound: BC2, signed
BC4, signed BC5, BC6H, HDR ASTC and signed EAC. Either case is a content pack cooked for the wrong platform: it
warns once per format and raises the `render.problems` code `textures_cooked_for_other_platform` (also a
host-checklist difference and `assets.texture_memory.wrong_platform`). `render.get_capabilities` `texture_paths`
says per family (`bc`, `astc`, `etc2`) whether the device takes it `native`, by `cpu_decode` or leaves it `unbound`.

**ETC2 / EAC (ADR-1379).** For Android GPUs without ASTC the pipeline also writes ETC2 / EAC:
`cook_texture_rgba8_android(.., AndroidTextureFormat::Etc2)` cooks ETC2 RGB8 (RGBA8 when the base level has alpha)
for colour and linear data, EAC R11 for masks and EAC RG11 for normal maps (X, Y; Z rebuilt as for BC5). One bit
rate: 4 bits a texel, 8 with alpha or for normals. The encoder (`etc2_compress.rs`, original code from the Khronos
Data Format Specification) searches the ETC1-compatible individual and differential modes and the ETC2 planar, T and
H modes (ADR-2063) and keeps the one with the least error. The default Android cook stays ASTC. A project selects the Android family and the ASTC block ladder
in `Regia.toml` (ADR-2063):

```toml
[cook.textures.android]
format = "etc2"              # "astc" (default) or "etc2"
astc_profile = "per_texture" # "per_texture" (default: each image's .meta quality), "high" (4x4) or "low" (6x6 / 8x8)
```

The policy covers the generic texture cook (standalone images, glTF material maps, packed detail and aux maps, DDS)
and `cook_terrain_material` (normals EAC RG11, heights EAC R11, the layouts the terrain shader reads). It is part of the
Android cache key (empty for the default, so older caches stay valid). A bad table fails the cook.
`regia cook --size-report [--json]` measures every standalone image and every image of a glTF file as the desktop cook
(BC), ASTC at the project's profile and ETC2 / EAC: chain bytes and level-0 PSNR, with totals
(`book/agents/texture-compression.md`).
`android_format_report` and the `etc2_survey` example print the ASTC and ETC2 bytes of one texture's full chain and
the level-0 PSNR of both.

Encoder: `intel_tex_2`. **Risk check in M4:** if it fails to build on Windows MSVC (or, later, macOS arm64),
use a fallback encoder path (e.g. port of a permissive pure-Rust BCn encoder, or uncompressed in dev
+ cook on a Linux/Windows host) and record an ADR. Encoding is slow: run per texture on
`AsyncComputePool`, cache aggressively, and use a "fast" quality preset for dev cooks.

**Alpha-tested mips (ADR-3090).** A texture `.meta` `alpha_cutoff: Some(c)` (the material's cutoff) keeps
each mip's alpha-test coverage at mip 0's: after the box-filtered chain is built, each level's alpha (red for a
`mask`) is scaled so the fraction of texels at or above `c` is the closest it can be to mip 0's (Castano 2010),
before any encoder runs. Unset (the default) changes nothing. Standalone images, glTF-referenced images and
`.texpack.ron` outputs honour it; HLOD proxy atlases with holes opt in. **Channel packing:** a `.texpack.ron`
(`docs/formats/texpack-ron.md`) packs up to four source images into one texture that cooks like a PNG with the
recipe's `.meta` settings; its sources are inputs of the cook key and the live cooker.

### 7.1a Mip streaming metadata (T2.14, ADR-1942)

A cooked texture carries `regia_asset::MipStreamMeta` in its KTX2 key/value data under `regia.mipstream`
(a postcard value with its own `version`). Readers skip unknown keys, so `CookedTexture`'s frozen shape does
not change and a texture without the key streams by the tier policy alone.

- `detail_px`: the largest edge at which a level differs from the bilinear magnification of the next
  level by more than 2 of 255 in any channel of any texel; at least 16. All slots of a material skip the
  same number of levels, each from its own top, so the streamer keeps a level out of memory only when no
  slot needs the level at that depth.
- `stream` (`false` holds the material at the tier cap) and `min_resident_px` (a higher floor) come from the
  image's `.meta`: `stream: Some(false)`, `stream_min_px: Some(512)`.

`assets.cooked_pack { "inspect": true }` counts the textures that carry it (`mip_metadata_textures`,
`mip_capped_textures`, `unstreamed_textures`).

### 7.1b Shared external glTF images (v0.5.1, ADR-8055)

Project cooks share an external image by its canonical in-project source and normalized effective
RGBA8 texture settings. The standalone image's `.meta` id owns its default settings; other settings
use deterministic `gltf_texture/<settings hash>` variants. Equal variants across models encode once
per cook and publish one manifest/registry row at the image or variant path. Material slots and
cooked header dependencies use the shared ids. Embedded images retain private ids. The cook logs
`shared_texture_settings` when one source needs multiple variants. No frozen format changes.
A glTF-referenced L16 source cooks only as a texture unless an import rule or its image `.meta`
sets `heightfield: Some(true)` (ADR-8143). With that explicit setting it also keeps its standalone
Heightfield identity and the glTF RGBA8 views share a separate texture variant. This applies to
same-directory and parent-relative image URIs. Unreferenced L16 sources still default to
heightfields; `heightfield: Some(false)` disables heightfield decoding. The setting lives inside
the existing extensible RON settings string; no container or cooked format changes.

Importer revision 1.14.1 invalidates old glTF cooks, including the initial shared-image implementation. Hosts that persisted old `texture/N` ids or
manifest paths follow the cooked-packs book Upgrade instructions; the glTF `.meta` sub-asset table
still records each image index's current id. Existing paks remain readable without migration.
Identical packed ORM payloads with different FCKD headers still need separate blobs in FPAK v1;
ADR-8055 records the v0.6 header/payload index separation, version bump and migrator design.

### 7.2 Mesh cooking
- Vertex streams (separate buffers, so depth/shadow passes fetch positions only):
  stream 0 position `f32x3`; stream 1 normal+tangent `snorm16×4` only (octahedral normal xy, a zero,
  and the tangent sign; 8 bytes a vertex — there is no `snorm8x4` alternative);
  stream 2 UV0 `f16×2` only (4 bytes a vertex). A coordinate is always stored as `f16` (outside the
  finite half range it becomes a half infinity); there is no `f32` fallback and no second UV.
  Lightmap UV2 is not a mesh stream: the current `.flmap` keeps it in the geometry section
  (`docs/formats/lightmap.md`).
  stream 3 color `unorm16x4` linear (optional; `COLOR_0`); stream 4 skin: joints `u8x4` (≤ 256 joints) or
  `u16x4`, weights `unorm16x4` summing to exactly 65535, plus an optional second set for 8 influences
  (optional). Joint indices are the cooked skeleton's joint ids; influence slots keep the authored order.
  Morph targets are sparse: per target a name, the moved vertex indices (`u32`), position/normal/tangent
  deltas `f16x3` and the position-delta bounds (cooked mesh v2, ADR-226, `docs/formats/cooked-mesh.md`).
  Cooked mesh v3 (ADR-1622) adds `skin_influences`: 4 by default, 8 when the glTF `.meta` settings say
  `skin_influences: 8` and the skin has a second set (the streams are the same either way).
- Index `u16` when ≤ 65535 vertices else `u32`.
- meshopt: `optimize_vertex_cache`, `optimize_overdraw` (threshold 1.05), `optimize_vertex_fetch`.
- LODs: `simplify` with attribute-aware weights on normals and UV0 (the same weights as the cluster
  cooker: normal weight 0.05 of the bounding diameter, UV weight 1 times the mesh's world length per
  UV unit), lock borders → up to 4 LODs, each with screen-size
  threshold stored in the mesh; LODs share the vertex buffer where meshopt permits (index-only LODs).
- Bounds: one mesh AABB (`aabb_min`, `aabb_max`) and one bounding sphere (`bounding_sphere`:
  centre xyz, radius in w). Submeshes are an index byte range plus a material slot
  (`index_byte_offset`, `index_byte_length`, `material_slot`). They have no bounds of their own.
- Optional CPU copy for collision (`keep_cpu_data`) and a precomputed BVH for editor picking fallback.

## 8. Hot reload
- Watcher (desktop dev builds and editor only, `notify`): source change → asset DB reimport → cooked
  output written → runtime `AssetServer` receives `Reload(id)` → reloads in place, keeping all handles
  valid → `AssetEvent::Modified`. Delivery of `Reload(id)`: an in-process channel in the editor; in a
  standalone dev build, the game's `dev-assets` watcher on the cooked cache, with the asset DB hosted by
  the `regia run` process (this is the only path before the editor exists, M4–M9); a local socket from
  the editor to a dev game it launched is Tier 2.
- Systems that cache derived data (pipelines, physics colliders from meshes) must listen to
  `AssetEvent::Modified` and rebuild.
- *As built (ADR-126, ADR-5611):* the message is `AssetReloaded { id, kind }` for a cooked asset replaced in
  place and `SourceAssetChanged { path, kind }` for a source the runtime reads; owners reload on those
  (`book/user-guide/live-asset-reload.md`). `AssetEvent::Modified` is not sent.

## 9. Acceptance (M4, M11, M16)
- M4: glTF (Khronos sample models: DamagedHelmet, Sponza, FlightHelmet, Fox, CesiumMan) import, cook
  for the host platform and load at runtime; cached recook of an unchanged project is < 1 s for
  1,000 assets; texture hot reload visible within 1 s in a running dev build; `regia preview` shows and
  agent `screenshot(asset)` captures DamagedHelmet; JSON Schema + example for `.meta` and `.material.ron`
  published in `docs/formats/`; `regia upgrade-project` runs a no-op v1 migration through the migrator
  registry.
- M11: move/rename in content browser preserves all references; deleting a referenced asset warns and
  lists referencers.
- M16: `regia cook --platform android` produces ASTC paks; the same project cooks for
  `windows`/`linux` with BCn; pak loads are byte-identical to the dev cache (test).
- M11 as built (ADR-5700): the content browser's Rename / Move… moves a source *and* its `.meta`
  (the asset id never changes, so `asset:<guid>` references need no edit), rewrites the string
  literals that name the old path in referencer files before the rename (ADR-5877: `/` is part
  of a path, field names and comments are never edited), is all-or-nothing (a failed step rolls
  back), and is one undo step (a move back); the agent
  method is `editor.rename_asset` (`book/agents/content-browser.md`). Deleting a referenced asset
  is refused with its referencers listed (`regia_editor` `delete_asset`), and a source moved
  outside the editor keeps its id because the asset DB re-pairs it with its `.meta` by content hash.
- As measured (ADR-5611): the 1,000-asset recook budget is 1 s in a release test binary and 3 s in a debug
  one (`recook_benchmark.rs`, best of three); the "migrator registry" `regia upgrade-project` runs is the
  CLI's frozen-format table (`regia_cli::commands::upgrade_project`), where a current `.meta` plans as up to
  date; `regia_asset::default_registry` is the library form of the same no-op steps.

## 10. Import folder rules (`config/import_rules.ron`, ADR-3321)
*Status (2026-11-06, ADR-3321, proposed):* built — the v1 file (`docs/formats/import-rules-ron.md`),
cook-time resolution and cook-key wiring (`regia_asset_pipeline::import_rules`, `cook_inputs`),
`regia assets import-rules check`, and the agent method `assets.import_rules`.
- A project can give every source under a path glob the same partial importer settings without
  editing each `.meta`: an ordered rule list, each rule a project-relative glob (`` `*` ``, `?`,
  `**`), an optional importer filter, and a partial settings object in the `.meta` settings syntax.
- Precedence, low to high: importer default → file-name inference (a standalone texture's usage
  from its name) → matching rules in file order (later rules win field by field) → the explicit
  fields of the asset's own `.meta`.
- Rules resolve at cook time; the `.meta` keeps only explicit overrides. The matched rules are
  hashed into the source's cook key (ADR-530 local key and ADR-2612 shared key), so editing a rule
  recooks exactly the matching sources. A broken file fails the cook before anything cooks.
- Supported importers today: textures (incl. DDS/HDR), glTF (also via FBX/OBJ conversion), BVH and
  audio. `regia assets import-rules check` reports effective settings per source, the findings
  `import_rule_unused` and `import_rule_unknown_field`, and merged values the importer cannot
  parse. The editor's per-asset settings panel (lane 1482) calls
  `regia_asset_pipeline::import_rules::effective_settings` for the same numbers.

## 11. Runtime access diagnostics (C-11, ADR-8160)

`REGIA_ASSET_TRACE` opts into an append-only log of cooked-cache reads, pak opens
and payload reads, and asset-server manifest resolution. It is sampled once;
unset or empty disables it. Outcomes distinguish success, absence and other
errors, with escaped paths and no timestamps. Diagnostics never change load
results. The read-only `assets.access_trace` agent method exposes bounded recent
rows, counters and sink health. Coverage, syntax and limits are in
[`book/agents/asset-access-trace.md`](../../book/agents/asset-access-trace.md).
No cooked or authored format changes, new dependency, or preload interception.

### v0.6 batch addition: node-tracks v1 (P-11, ADR-8157)

Register `node-tracks` v1, embedded in appended `RawNodes` / `CompressedNodes`
variants of unreleased cooked-clip v3. The preexisting v3 fixtures and both
older payload readers remain supported; old clips have no node body. The
existing cooked-clip migrator and `upgrade-project` path preserve new node
bodies. There is no separate content suffix. Both writers store node channels
losslessly. See [node-tracks](../formats/node-tracks.md). This addition must
merge before the v0.6 format freeze under § 4a / ADR-7724.

## 12. Package parity (C-12, ADR-8428)

`regia package parity` runs one scripted path against the dev tree and against
an extracted package with `REGIA_ASSET_TRACE` set, then diffs the two traces.
A dev open the package does not account for fails the gate. Files in the
package that neither run touched are listed and do not fail. A source the dev
run read counts as present when the package served its cooked equivalent.
`package.parity` is the read-only agent method for a diff of traces the caller
already has. Reports are markdown and JSON. No cooked format changes.
`serde_json`, already a workspace crate, parses glTF image URIs for the diet
check. Details are in
[`book/agents/package-parity.md`](../../book/agents/package-parity.md).

### Cook-time texture tier packages (ADR-8059)

`regia cook --texture-tier NAME=MAX_EDGE_PX` is repeatable and derives complete alternative
chunk sets in `build/paks/<platform>/texture_tiers/<NAME>/`. `0` retains the full chain; no flag
preserves ordinary output. The tier step copies retained KTX2 level payloads from the full cooked
paks without encoding again, updates dimensions/indices/offsets, and preserves IDs and metadata.
The host chooses one set with its existing-format manifest, registry siblings and maps. Chunk
rules and runtime residency caps remain in force; `--lightmap-tiers` is independent. No frozen
schema changes. Scope, failure codes and selection are in `book/agents/texture-tiers.md`; this
is separate from the proposed standalone level-drop tool.

ADR-8144 restricts reduction to 2D LDR textures referenced by ordinary cooked standard
materials' residency-capped slots (base color, normal, ORM, emissive, packed detail albedo/normal).
UI usage, HDR/float, cube/array/volume, generated terrain VT/HLOD/voxel assets and textures
without such a reference keep their full cooked bytes. References are resolved across chunk
manifests, so chunk placement does not decide eligibility. No new frozen fields are needed.
If an eligible authored chain has no mip at or below the cap, retain its entire cooked file
and warn `texture_tier_missing_mip` once per id per tier-writing operation (including multiple
tiers and duplicate chunks). Other textures still reduce. Malformed data and unsupported
supercompression remain errors that preserve the previously published tier.
