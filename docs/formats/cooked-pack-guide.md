# Cooked pack guide (for DO27's build-time cooker)

Status: guideline, 2026-10-08 (lane instant-load). Formats referenced here are frozen per
`docs/specs/12-assets.md` § 4a: **nothing in this guide changes a format**; where a better layout needs a
format change, it is listed under "Engine follow-ups" and will arrive with a migrator.

The goal is DO27's "feels instant" budget: launch → branding < 1 s, menu interactive < 1 s, Ready → in
control < 1 s with no loading screen, on the minimum tier (RTX 2080 class, 8 GB VRAM, 16 GB RAM, NVMe).
That only works if the runtime does **no decode and no re-encode**: every byte in a pak is in the form the
GPU (or the CPU system that owns it) consumes, so a load is `mmap` → (optional lz4) → staging → GPU.

## 1. What `regia cook` already does vs what DO27 must do

| Step | `regia cook` / `regia_asset_pipeline` today | DO27's cooker |
|---|---|---|
| Texture compression | BC7 sRGB (colour), BC7 linear (ORM / masks), BC5 (normals, Z reconstructed), BC4 (single channel), full CPU mip chains, KTX2 output (`texture_compress.rs`, `ktx2_write.rs`); ASTC via vendored astcenc (`astc_compress.rs`) for Android | Run the cook for every map texture at the **authored size** (4 K) — never ship PNG/JPG for runtime use. Set usage per texture in its `.meta` (`Color`, `ColorLinear`, `NormalMap`, `Mask`). |
| Set-piece material maps | `cook_material_maps` (mem-stream) cooks a material's maps to BC7 4 K with full mips; the renderer uploads them as `CompressedMaps` | Use it for every set piece; mem-stream measured 3.67 → 1.42 GiB GPU and 11.6 → 2.1 s load on Switchbacks. |
| Meshes | meshopt vertex-cache / overdraw / fetch optimisation, LODs, separate vertex streams (`mesh_cook.rs`, spec 12 § 7.2). Desktop paks built with `REGIA_PAK_GPU_MESHES=1` (opt-in; never Android) store each mesh **GPU-ready** instead (`regia_asset::gpu_mesh`, layout v3 (ADR-10080; v1 and v2 still read; v2 added submeshes, ADR-2788): the renderer's 96-byte vertices with MikkTSpace tangents, `u32` indices, every LOD, and the compact mesh's submesh ranges with their material slots, so destruction cut faces keep their material), converted by `regia_render::cooked_model::gpu_mesh_file_from_cooked` (a mesh with `COLOR_0` converts and keeps the stream in the vertex color slot, ADR-7919; a mesh with a skin or morph targets converts too and keeps them in the payload's trailing block, layout v3, ADR-10080; `CpuMesh::from_cooked` of such an entry reuses the stored tangents, no MikkTSpace at load; `pak_migrate` converts an existing pak; ADR-285); the cooked cache keeps the compact form. Without the variable paks keep the compact form. `regia cook --ship` (ADR-1594) does the same per command line and then checks every pak it wrote: a texture the platform cannot sample as stored, a supercompressed KTX2 or a legacy mesh fails the cook (`build/paks/<platform>/cooked_pack.json`; book: Cooked packs). Mounted paks must not be overwritten or truncated in place while the game runs (views of the mapping would `SIGBUS`; replace by rename) | Cook every glTF through it; no runtime glTF parse. Decode mesh entries with `load_mesh_bytes` / `AssetServer::load_mesh` (both forms), not `CookedMesh::from_postcard`. |
| Cluster (meshlet) data for the visibility buffer | Builder (`build_meshlet_mesh_with_attributes`, 64 vertices / 124 triangles, DAG). `regia cook` writes meshlet format v1 `.fmlt` sub-assets (`MESHLET_FORMAT_VERSION`) when a glTF's `.meta` sets `meshlets: true`; derived data, so it is recooked on a version change and not in `format-hashes.ron`. Without the flag the vis-buffer builds clusters at load (`MeshletMesh::single_level` fallback) | Nothing (see follow-ups). Do not invent a side format. |
| Materials | `.material.ron` v3 cooked to postcard; `.matgraph.ron` / `.matinst.ron` / `.matfunc.ron` cooked graphs (WGSL generated at cook, pipelines prewarmed async at runtime) | Author v3 materials and graphs; let the cooker emit the cooked graph. List every graph a map uses in the map's load set (§ 4) so its pipelines warm behind the menu. |
| Terrain | Tiled terrain pack: BC7/BC5/BC4 layer arrays + macro tiles, resident tails, tiled heightfield (`terrain_cook.rs`, `docs/formats/terrain.md`) | Cook per map; ship the terrain pack in the map's chunk. |
| Audio | Banks/streams per spec 16 | One bank per map + one shared UI bank. |
| Paks | `write_pak` / `pack_cooked_directory`: 4096-aligned blobs, lz4 256 KiB blocks, raw storage when lz4 does not pay, chunking by folder globs → `chunk_<name>.fpak` | Chunk as in § 2. |
| Runtime read | `PakReader::open` **maps** the file; `read_shared` serves an entry stored raw as a view of the mapping (no heap copy) and decompresses an lz4 entry into an owned buffer (`read_decompressed` always copies). `AssetServer` and the terrain `PakTileSource` read through `read_shared`: a cooked texture's `ktx2` and its `CompressedTexture` mip levels are views of that payload (`PakPayload`), so a raw BC7 entry goes from the mapping to `write_texture` without a CPU copy, and an lz4 one is decompressed once and never copied again. `PakStreamer` reads per asset on a worker with read-ahead (`madvise(WILLNEED)`), faults a mapped payload's pages in on the worker, and yields `PakPayload`s that convert into `UploadBytes` without a copy. lz4 entries of 8+ blocks decompress their blocks in parallel. On the first launch after an update the `AssetServer` closes a pak's zero-copy gate until the background `pak_verify` check passes: loads then copy through the file handle (no mapping, so a pak truncated in place is an error, not `SIGBUS`). Independently, each read `fstat`s the open file: a pak seen shorter or re-stamped since it was opened logs "pak changed on disk" once and is read through the file handle from then on (views already served stay exposed). `load_cooked_model` decodes meshes on the compute pool while it loads materials | Nothing. |

## 2. Chunking

- `base.fpak`: engine defaults, UI (fonts, menu art), the shared audio bank, the boot splash image, shared
  materials/graphs. Everything the **menu** needs and nothing else — it is mapped at launch.
- `map_<name>.fpak`: one per map: its terrain pack, set pieces (meshes + BC7 maps), scatter, map audio,
  map-only material graphs. A map pak is mapped when the map is preloaded; nothing is read until streamed.
- Patches/DLC: later paks override ids (spec 12 § 5); keep ids stable across builds (the `.meta` GUID).

Why per-map: the residency query (`is_resident("map:switchbacks")`) and the memory admission check
(`render.memory_budget`) work per load set; a pak per map lets DO27 preload both alpha maps behind the menu
and drop one without touching the other.

## 3. Alignment, compression, ordering

- Blobs are 4096-aligned (page size) by the writer; keep it — `mmap` + `madvise` work in pages.
- **Store GPU-native texture blobs uncompressed** unless lz4 saves ≥ 15 %: BC7 compresses poorly with lz4
  (typically 5–10 %), and a raw blob is read straight from the mapping into the upload queue with no
  decompress CPU on the critical path. Meshes, cooked graphs, materials, terrain masks and audio metadata
  compress well — keep lz4 for them. The writer chooses per blob (ADR-7971): a blob is stored raw when lz4
  does not shrink it, would pass the decompression-bomb ratio, or saves less than
  `PakWriteOptions::min_ratio`, which `regia cook --ship` sets to 0.15 (a plain cook keeps any saving). To
  keep a whole chunk raw whatever lz4 saves, write `compress = false` under its `[cook.chunks.<name>]`
  table in `Regia.toml` (`book/agents/content-chunks.md`).
- **Order blobs in load order** inside a map pak: terrain resident tails and the heightfield first, then the
  set pieces nearest the spawn, then the rest. The streamer reads in request order and the readahead hints
  the next four; a pak in that order turns into sequential NVMe reads.
- Full mip chains always (the streamer uploads coarse mips first; texture streaming fades finer ones in).

## 4. Load sets (what DO27 declares per screen)

Ship a list per map (RON in `base.fpak` or generated code) and define it at startup:

```rust
let mut set = LoadSet::new("map:switchbacks");
set.push(LoadKind::Texture, "maps/switchbacks/pad_albedo.ktx2", 0, 22 << 20);   // ram, vram estimate
set.push(LoadKind::Mesh, "maps/switchbacks/pad.mesh", 0, 3 << 20);
set.push(LoadKind::TerrainTile, "switchbacks:tail", 0, 240 << 20);
set.push(LoadKind::AudioBank, "audio/switchbacks.bank", 40 << 20, 0);
set.push(LoadKind::Pipeline, "map:switchbacks frame", 0, 0);                    // a warm job label
world.resource::<LoadSets>().define(set);
```

The cooker knows every size, so it should emit the estimates: VRAM = the BC level bytes (+ mips) / vertex +
index bytes; RAM = what the CPU keeps (audio PCM or compressed stream buffers, collision copies).

## 5. Android

ASTC instead of BC (spec 12 § 7.1 table), same chunking. Paks are extracted from the APK on first launch;
put map paks in Play asset packs (Tier 2) so the base APK stays small.

## 6. Engine follow-ups (format changes; each needs a migrator, none is DO27's job)

1. Cooked meshlet/cluster stream: shipped as separate `.fmlt` sub-assets (format v1) for `meshlets: true` meshes; folding it into `CookedMesh` is not done, and meshes without the flag still build clusters at load.
2. ~~`PakWriteOptions::min_ratio`~~: shipped with ADR-7971 (no format change: raw and lz4 entries always
   mixed in one pak). `regia cook --ship` stores a blob raw when lz4 saves under 15 %; `[cook.chunks.<name>]
   compress = false` keeps a whole chunk raw.
3. A cooked `LoadSet` asset (`.loadset`) emitted by `regia cook` per chunk, so the list in § 4 is generated.
4. Pak content hash verification (header field exists; verification is Tier 2).
