# Cooked packs

A shipped Regia Engine game never parses glTF, never decodes PNG or JPEG and never transcodes a
texture at runtime. Everything it loads comes out of `.fpak` files in the form the GPU (or the CPU
system that owns it) consumes. This page shows how to build those paks, how to check them, and
what the runtime does with them. Agents: [Cooked packs (agents)](../agents/cooked-packs.md).
Decision record: ADR-1594.

## What you get with no extra work

The runtime crates (`regia_asset`, `regia_render`, the `regia` umbrella crate) cannot read glTF
or PNG: the glTF parser and the image decoders live only in `regia_asset_pipeline`, the cook. A
game that loads through `AssetServer` (or `regia_render::cooked_model::load_cooked_model`) already
reads cooked data only:

| Source | Cooked to | Loaded as |
|---|---|---|
| `.gltf` / `.glb` | compact `CookedMesh` per primitive, materials, a model part list, skeletons, clips | mesh: unpacked at load and given tangents (compact), or copied as is (GPU-ready) |
| `.png` / `.jpg` (color, normal, mask) | KTX2 with full mips: BC7 / BC5 / BC4 on desktop, ASTC on Android | the mip levels go from the pak to the upload queue as stored |
| a small or `compress: false` image | RGBA8 `CookedImage` | uploaded as stored |

Two pieces of load-time work were still possible before cooked packs, and nothing told you about
them: compact meshes (unpacked and given MikkTSpace tangents on every load) and textures the
device cannot sample as stored. A desktop cook of BC1, BC3, BC4 unorm, BC5 unorm or BC7 copied to
Android is decoded to RGBA8 on the CPU. BC2, signed BC4 and BC5, and BC6H stay unbound, as do
ASTC HDR and signed EAC on a device that cannot sample them. ASTC LDR and unsigned ETC2 / EAC on
a desktop GPU take the same RGBA8 fallback. A cooked pack removes the first on desktop and makes
the second a build error.

## Building cooked packs

```sh
regia cook --platform linux --ship
regia cook --platform windows --ship
regia cook --platform android --ship
```

`--ship` does three things after the normal cook:

1. **GPU-ready meshes** (desktop): every mesh entry is stored in the renderer's own vertex layout
   (96-byte vertices with tangents, `u32` indices, every LOD; `regia_asset::gpu_mesh`). Loading it
   is one copy. A mesh with `COLOR_0` is stored GPU-ready too: the colors go into the vertex color
   slot, `AssetServer::load_mesh` returns them in `CpuMesh::colors`, and `load_cooked_model` still
   draws the mesh white, as it draws the compact form. A mesh with a skin or morph targets is
   stored GPU-ready as well (layout v3 keeps the skin, both influence sets and the morph deltas
   after the indices, ADR-10080): `load_mesh` returns them, and `CpuMesh::from_cooked` builds the
   render vertices from the stored tangents with no MikkTSpace pass. Android meshes stay compact. `--gpu-meshes` alone does this part without the check (the same as
   `REGIA_PAK_GPU_MESHES=1`, which keeps working).
2. **The cooked-pack check**: every pak written is inspected entry by entry, each texture's KTX2
   header included, against the platform's rules (below). An error fails the command.
3. **The report**: `build/paks/<platform>/cooked_pack.json` lists each pak's entries by form and
   every finding with up to eight example paths. It is written before the check is judged, so a
   failed cook leaves the evidence.

`regia cook --pack-report` prints the same report for the paks already on disk (no cook) as JSON,
and exits non-zero when a pak has an error.

`regia package` cooks with the plain cook when its paks are stale. For a shipping package, run
`regia cook --platform <p> --ship` first; `regia package` then finds the paks fresh and stages
them, and its ship check runs the cooked-pack rules again on the staged paks.

## The rules

| Finding | Severity | What it means |
|---|---|---|
| `texture_family_mismatch` | error | A texture in a family the platform's GPUs cannot sample as stored: BC on Android, ASTC or ETC2 on desktop. |
| `supercompressed_textures` | error | A KTX2 with BasisLZ, Zstandard or ZLIB supercompression: the runtime has no transcoder. Cook the source image instead. |
| `unreadable_textures` | error | A texture entry that does not read or validate. |
| `legacy_meshes` | error | A mesh from a cook before M4 round 3. Recook with `--clean`. |
| `compact_meshes` | warning, desktop | A mesh unpacked at load. Since GPU mesh v3 (ADR-10080) skinned and morphing meshes are GPU-ready too: cook again. |
| `large_uncompressed_images` | warning | An RGBA8 image of 256 × 256 or more; set `compress: true` in its `.meta`. |

## The shipping-package checks

A shipping `regia package` (`--profile shipping` or a named `[profiles.<name>]`) runs, besides the
cooked-pack rules on the staged paks:

- **Importer in the binary**: the shipped binary must not contain the cook crate
  (`regia_asset_pipeline`) or the glTF parser (`gltf-json`). A named profile fails; the plain
  `shipping` profile warns.
- **Importer in `Cargo.toml`**: `regia_asset_pipeline`, `gltf` or `gltf-json` under
  `[dependencies]` (or a `[target.*.dependencies]` table, renamed or not) fails a named profile and
  warns on plain `shipping`. A cook step in `build.rs` (`[build-dependencies]`) and tests
  (`[dev-dependencies]`) are fine.

## Checking a running game

`AssetServer::load_forms()` counts what the server and the renderer's model loader loaded:
GPU-ready, compact and legacy meshes, KTX2 textures, RGBA8 images, reads served by a pak and reads
served by the dev cooked cache, plus the process-wide count of textures the renderer decoded on the
CPU. `LoadForms::cooked_only()` is true when none of the decode counters moved and nothing came
from the dev cache. `AssetServer::mounted_pak_summaries(inspect)` gives each mounted pak's
entries by form; `regia_asset::cooked_pack::check_pak_summary` applies the rules to one. Agents
read all of this with `assets.cooked_pack`.

## Load time

`cargo run --release -p regia_asset_pipeline --example cooked_pack_bench` builds a textured glTF, cooks it,
writes a plain pak and a cooked pack, and times loading the same model three ways on the CPU (a fresh
`AssetServer` per load, page cache warm, no GPU upload; run it with `REGIA_PAK_VERIFY=0`). Median of 7 on
the build server (shared 128-thread host under load, 2026-10-28):

| Content | Runtime glTF import (parse, PNG decode, tangents; no mips) | Plain pak (compact mesh) | Cooked pack (`--ship`) |
|---|---|---|---|
| 524,288 triangles, two 2048² textures, lz4 paks | 929 ms | 906 ms | 8.8 ms |
| same, uncompressed paks | 859 ms | 914 ms | 9.2 ms |
| 8,192 triangles, two 4096² textures, lz4 paks | 335 ms | 17 ms | 6.2 ms |

The compact mesh's cost is the MikkTSpace tangent pass, which a runtime glTF import pays too; the
texture cost of a runtime import is the PNG decode, which no pak pays. The price is disk: GPU-ready
vertices are 96 bytes each, so the 524k-triangle pak is 24.4 MB against 23.1 MB with lz4 and 74.3 MB
against 36.4 MB uncompressed. Use `--grid N --texture N --runs N --raw` for your own content size.

## Limits

- A compact mesh is still a valid cooked pack entry on Android, and in a desktop pak cooked
  before GPU mesh layout v3 (its skinned and morphing meshes); it is reported as a warning so you
  can see how many there are. Those meshes still get the tangent pass at load until the pak is
  cooked again.
- A `COLOR_0` stream that is white on every vertex loads from a GPU-ready entry as no stream
  (`CpuMesh::colors` empty); white multiplies the same.
- `--ship` does not change chunking (one `base.fpak` per platform). It does change compression per
  blob: a blob lz4 saves less than 15 % of is stored raw (ADR-7971), so BC and ASTC textures load as
  views of the mapping with no decompress while meshes and metadata stay lz4. To keep a whole chunk
  raw, write `compress = false` under its `[cook.chunks.<name>]` table (`book/agents/content-chunks.md`);
  see `docs/formats/cooked-pack-guide.md` for chunking.
- Loose files a game reads by path (`assets://`, outside the paks) are not checked.

## Shared external glTF images (v0.5.1)

A project cook shares an external glTF image by its canonical source file and effective texture
settings (ADR-8055). Two models using the same image and settings encode it once and reference
one cooked texture. The manifest and asset registry publish that texture at the image's path,
for example `assets/textures/paint.png`, using the image's `.meta` id. Keep that `.meta` in source
control along with the image. Relative URI spellings and in-project symlinks resolve to the same
source; embedded images keep their model-private ids. A referenced 16-bit grayscale PNG cooks
only as a texture unless an import rule or its image `.meta` sets `heightfield: Some(true)`.
With that setting it also keeps its standalone heightfield id and type; glTFs share a separate
RGBA8 texture variant. See [the explicit heightfield rule](../agents/cooked-packs.md#explicit-heightfield-sidecars-adr-8143).

The image's `.meta` and import folder rules still control usage, size, quality, channel transforms,
alpha coverage and streaming. A glTF slot supplies usage when no override supplies it. Settings
that differ from the standalone image's settings get a deterministic variant at
`assets/textures/paint.png#gltf_texture/<settings hash>`. Equal variants share across models too.
The cook logs `shared_texture_settings` when one source needs multiple variants; it keeps their
pixels, color spaces and compression choices separate. It does not merge images merely because
their source bytes happen to match.

Shared material slots also carry the shared id in their cooked dependency lists, so chunk rules
and registry dependency queries can find the image. A pak containing the two models stores their
shared texture once. `registry.query` with `type: "CookedTexture"` and `assets.cooked_pack` expose
the resulting record and stored texture counts through the existing agent protocol.

Payload-only deduplication of separately identified packed ORM results is deferred to v0.6. In
FPAK v1 each index range includes the FCKD header, whose asset id differs for those results. Sharing
only their KTX2 payload while preserving each entry's header needs a new index and a migrator.
This release changes neither FPAK nor FCKD; old paks continue loading as before.

### Upgrade: stored texture sub-asset ids

Recook and repack each shipping platform. The glTF importer revision invalidates the old private
external-image cooks. Materials produced by the cook pick up the shared ids automatically.

A host or authored file that saved `model.gltf#texture/N` ids must replace those references. Before
recooking, retain its old `(source model path, texture/N, id)` mapping; after recooking, resolve the
same entry in the model's `.meta` sub-assets or read the cooked material's texture slot and map the
old id to the new one. Prefer canonical image paths or material slots for future lookups. The public
manifest now publishes external images at their source/variant paths, not at each model's texture
sub-path. There is no automatic migration for opaque ids in host-owned save files. Embedded image,
mesh and material identities are unchanged. Existing paks need no migration unless being rebuilt.
Validate migrated references against rebuilt paks: old private files may still be present in a dev cache.

The first v0.5.1 cook creates `<image>.meta` beside each external in-project glTF image, so the
source tree must be writable for that first cook. Commit these sidecars before cooking on other
machines; independently generated random ids prevent byte-identical paks. Shared textures also
move chunk placement to the image path: review [content chunk migration](content-chunks.md)
before comparing base downloads or Android install-time packs.

## Smaller texture downloads by tier

`regia cook --texture-tier NAME=MAX_EDGE_PX` derives additional pak sets from the full cooked
mip chains. Repeat the flag for each download tier:

```sh
regia cook --project . --platform linux --ship \
  --texture-tier LOW=1024 --texture-tier HIGH=2048 --texture-tier ULTRA=0
```

The ordinary output stays in `build/paks/linux/`. Alternatives live in
`build/paks/linux/texture_tiers/LOW/`, `HIGH/` and `ULTRA/`. For example, a chunk named
`textures` becomes `texture_tiers/LOW/textures.fpak`; without chunk rules, textures remain in
`texture_tiers/LOW/base.fpak`. Each directory contains a complete chunk set, its registry
siblings, `content-manifest.ron`, `chunk-map.bin` and the existing reason/host/trust records.
No new manifest format is required. Tier names normalize to uppercase; names must start with
an ASCII letter and contain only ASCII letters, digits, `_` or `-`, up to 40 characters.
Duplicate names (ignoring case) and Windows device names are errors. This option is CLI-only;
there is no `[cook.texture_tiers]` table.

The cap is the largest edge of a retained mip, in texels. `LOW=1024` drops the 4096 and 2048
levels of a 4096 chain; the retained compressed payloads equal the original chain's tail byte
for byte. There is no second BC/ASTC/ETC encode. Non-power-of-two caps select the next whole mip
at or below the cap. KTX2 dimensions, level count and offsets are rebuilt, the DFD and key/value
metadata are retained, and the cooked wrapper records the new dimensions. A texture already
under the cap is unchanged. `0` means full resolution: `ULTRA=0` copies the ordinary paks byte
for byte. With no flag, the ordinary cook's bytes and behavior are unchanged.

This transforms `CookedTexture` entries wherever chunk rules placed them, including textures
in `base`, glTF texture subassets and shared copies. It preserves IDs, dependencies, delivery
rules and chunk assignment. Other asset types, including raw RGBA images and embedded terrain
material payloads, remain unchanged. An incomplete mip chain that cannot meet the cap fails
with `texture_tier_missing_mip`; enable mip generation and recook. A supercompressed KTX2 that
needs reduction fails with `texture_tier_supercompressed`: the runtime needs a source cook in
its platform format. The full cache is never reduced. A failed tier write preserves the previous
complete directory for that tier.

**Host selection:** choose one directory and use its manifest and sidecars together. Point
`AssetServerSettings::content_dir` at that directory, or stage its install chunks and sidecars
into the game's `content/`, with the other chunks on the download host. Each reduced tier manifest
has a distinct build ID; `ULTRA=0` preserves the ordinary manifest too. For HTTP, publish under the
exact `<build_id>/` in that manifest. Build DLC against the selected tier's manifest. Never mount the ordinary and reduced paks together:
the IDs intentionally match. Normal `regia package` still selects the ordinary output; a host
with tier downloads stages the chosen set explicitly. Old unrequested tier directories remain
on disk, so regenerate every tier you intend to publish after a source change.

`--lightmap-tiers` is independent and can be used in the same command: it derives baked lightmap
atlases in the cooked cache and writes `lightmap_tiers.json`. It does not select a texture tier
or place those lightmap variants in these paks. `--ship` checks both the ordinary and requested
tier paks; `--pack-report` inspects the ordinary directory only. The runtime texture residency
cap still applies to the selected chain; increasing it cannot restore levels omitted from the
download. See [the agent verification recipe](../agents/texture-tiers.md) and
[content chunks](../agents/content-chunks.md).
