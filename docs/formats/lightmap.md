# Lightmap (`.flmap`) format, version 7

A baked lightmap: one atlas of per-texel irradiance for the static meshes of a map, plus the second UV set
(UV2) each mesh was unwrapped with. Written by `regia bake-lightmaps`, read by
`regia_asset::lightmap::Lightmap` and installed at runtime through `regia_render::lightmap`. Design and
status: ADR-860 (version 1), ADR-1556 (version 2: directions), ADR-2024 (version 3: entity keys and seam figures), ADR-2107 (version 4: clusters), ADR-2314 (version 5: block-compressed texels, denoise record), ADR-2656 (version 6: source mesh hashes), ADR-3300 (version 7: atlas pages).
Agent page: `book/agents/lightmaps.md`.

Status: **version 7, not frozen.** Version 1 was new in v0.1.28; version 2 adds the direction section; version 3
adds each entry's entity key and seam figures; version 4 adds the cluster section; version 5 adds texel encodings 2 (BC6H)
and 3 (ASTC 4×4 HDR) and the denoise record; version 6 adds each entry's source mesh hash; version 7 adds atlas pages
(each entry's page, the page count, sections holding every page). The format is frozen with the v0.3.0 lightmap release
(`docs/specs/12-assets.md` § 4a). Until then a shape change still bumps the version and ships a migrator, so files baked
today keep loading. A version 1 file reads as a lightmap without directions, a version 1 or 2 file as one whose entries
have no key and no figures, a version 1 to 3 file as one with no clusters, a version 1 to 4 file as one with RGB9E5 texels and
no denoise record, a version 1 to 5 file as one with no source hashes, a version 1 to 6 file as one page, and `regia upgrade-project
--write` rewrites any of them (§ 4).

## 1. File layout

All integers are little-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | magic `FLMP` |
| 4 | 4 | format version (`u32`, 7; 1 to 6 still read) |
| 8 | 4 | header length `H` (`u32`) |
| 12 | `H` | postcard `LightmapHeader` |
| … | | zero padding to a multiple of 16 |
| `sections[0].offset` | `pages ·` (`4 · width · height` (encoding 1), `16 · ⌈width / 4⌉ · ⌈height / 4⌉` (encodings 2, 3)) | texels |
| `sections[1].offset` | `pages · width · height` | coverage |
| `sections[2].offset` | see below | geometry |
| `sections[3].offset` | `pages ·` (`4 · width · height`, `16 · ⌈width/4⌉ · ⌈height/4⌉` (encodings 2, 3)) or 0 | directions (version 2; empty without directions) |
| `meshlet_section.offset` | sum of `meshlet_bytes` or 0 | clusters (version 4; empty without clusters) |

Every section starts on a 16-byte boundary. The file ends padded to 16 bytes. The texel, coverage and direction sections
hold the atlas pages one after another (version 7, ADR-3300): page 0's rows, then page 1's, and so on, each page
`width × height`, so a texel's index is `(page · height + y) · width + x`. A block encoding encodes every page on its
own (blocks never span two pages), so each page's blocks are what a one-page file of that page holds. A one-page file's
sections are byte for byte what version 6 wrote.
`payload_hash` is the blake3 of the sections, in order, the clusters last. An empty direction or cluster section adds no bytes,
so an older file keeps its hash when it is migrated. A reader refuses a file whose sections run past its
end or whose hash does not match.

### Header (`LightmapHeader`, postcard)

| Field | Type | Meaning |
|---|---|---|
| `width`, `height` | `u32` | Atlas page size in texels, `1..=8192` (every page the same size) |
| `texel_encoding` | `u32` | 1 = RGB9E5; version 5: 2 = BC6H unsigned float, 3 = ASTC 4×4 HDR (§ Sections). A version 1-4 file with anything but 1 is `lightmap_payload` |
| `exposure` | `f32` | Encode multiplier, a power of two. Stored = irradiance × exposure |
| `density` | `f32` | Requested texels per metre |
| `padding` | `u32` | Gutter texels around every chart |
| `entries` | `Vec<LightmapEntry>` | One per placed mesh |
| `bake` | `LightmapBakeInfo` | How the texels were baked |
| `direction_encoding` | `u32` | 0 = no direction section, 1 = RGBA8 (version 2; absent in version 1), 2 = BC7, 3 = ASTC 4×4 LDR (version 5, ADR-2635) |
| `sections` | `[SectionRef; 4]` | `{ offset: u64, len: u64 }` of texels, coverage, geometry, directions (`[SectionRef; 3]` in version 1) |
| `meshlet_section` | `SectionRef` | The cluster section (version 4) |
| `meshlet_bytes` | `Vec<u64>` | Bytes of each entry's cluster blob, in entry order; empty when no entry has one, else one per entry, 0 for an entry without (version 4) |
| `payload_hash` | `[u8; 32]` | blake3 of the sections |
| `denoise` | `Option<LightmapDenoiseInfo>` | The denoiser the bake ran (version 5; `None` when it did not run, and for older files) |
| `source_hashes` | `Vec<u64>` | The source mesh of each entry (version 6): empty when the bake recorded none, else one per entry in entry order, 0 for an entry without. A list of any other length is `lightmap_layout`. The hash is `regia_asset::lightmap::lightmap_source_hash`: the first 8 bytes (little-endian, never 0) of the blake3 of the vertex count (`u64`), each mesh-space position's three `f32` (a negative zero as zero), the index count (`u64`) and the `u32` indices. `regia bake-lightmaps` writes it; `render.get_gi_mode` `lightmaps.stale_instances` compares it |
| `pages` | `u32` | Atlas pages (version 7, ADR-3300; 1 for older files): one past the highest entry `page`, `1..=64` (`MAX_LIGHTMAP_PAGES`). Any other value is `lightmap_layout` |

`LightmapDenoiseInfo` (version 5): `iterations` (`u32`, `1..=8` à-trous passes), `sigma_luminance` (`f32`, > 0, the
luminance edge-stopping width in standard errors of the texel's measured noise), `normal_power` (`f32`, >= 0),
`sigma_position` (`f32`, > 0, texels). Out-of-range values are `lightmap_layout`. With a record, `bake.filtered` is false:
the denoiser replaces the 3×3 filter.

`LightmapEntry`:

| Field | Type | Meaning |
|---|---|---|
| `name` | `String` | Placement name, unique in the file (`instance_<k>` from a description or a built-in, `entity_<guid>_<part>` from a level) |
| `source_vertices` | `u32` | Vertex count of the source mesh; every `xref` is below it |
| `rect` | `[u32; 4]` | `x, y, width, height` in texels of the entry's page, gutters included. Entry rectangles on one page never overlap |
| `scale_offset` | `[f32; 4]` | Local UV2 → atlas UV of the entry's page: `atlas = uv2 · xy + zw` |
| `vertex_count`, `index_count` | `u32` | Unwrapped vertices and indices in the geometry section |
| `first_vertex`, `first_index` | `u32` | Where they start (entries are contiguous, in order) |
| `charts` | `u32` | Charts of the unwrap |
| `area_m2` | `f32` | World-space surface area |
| `texels_per_metre` | `f32` | Density the entry was laid out at, after the atlas fit |
| `key` | `Option<{ entity: Guid, part: u32 }>` | The scene entity this entry was baked for and the part of its mesh source (version 3; `regia bake-lightmaps --scene`; `None` otherwise) |
| `seams` | `Option<LightmapSeams>` | Chart-seam figures measured at the bake (version 3; `None` for an older file or an entry with no seam) |
| `page` | `u32` | The atlas page (texture array layer) the rectangle is in (version 7; 0 for older files). A mesh's charts are never split over two pages |

`LightmapSeams`: `edges` (`u32`), `mean`, `p95`, `max` (`f32`), `worst_chart` (`u32`). See § 2a.

`LightmapBakeInfo`: `contents` (bits below), `samples`, `bounces`, `seed`, `sun_toward` (`[f32; 3]`, zero without a
sun), `sun_illuminance` (lux × the luminance of the sun colour), `filtered`.

`contents` bits: 1 `sky`, 2 `sun_bounce`, 4 `sun_direct`, 8 `emissive`, 16 `local_lights`. Any other bit is refused.
Light that is not in the lightmap stays live at runtime. By default the direct sun and its shadows stay live
(`sun_direct` is off). With `sun_direct` a receiver drops the live sun's diffuse term and keeps its specular (ADR-2024).

### Sections

- **Texels**: one `u32` per texel, row-major, top row first. RGB9E5 (`EXT_texture_shared_exponent`) of linear
  irradiance in lux times `exposure`. The runtime uploads this section unchanged as an `Rgb9e5Ufloat` texture.
  Version 5 encodings 2 and 3 store the same values as 16-byte 4×4 blocks instead, row-major over
  `⌈width / 4⌉ × ⌈height / 4⌉`, an edge block repeating the last column and row (`regia_asset::hdr_blocks`, ADR-2314):
  - **2, BC6H** unsigned float (`Bc6hRgbUfloat`), one-region modes 11 to 14 of the BC6H mode table only (10-bit
    endpoints; 11 + 9, 12 + 8, 16 + 4 bits base + delta), 4-bit indices.
  - **3, ASTC 4×4 HDR** (`Astc { B4x4, Hdr }`): block mode `0x053` (a 4×4 weight grid, one plane, 8 weight levels), one
    partition, colour endpoint mode 11 (HDR RGB direct) at the 256-level endpoint range.

  Any other block (another mode or layout) is `lightmap_payload`: the CPU decoder reads exactly the subset the encoder
  writes. A reader decodes the blocks into RGB9E5 texels as well, so every CPU path (sampling, seams, compare) sees
  what the GPU samples. The runtime uploads the blocks as they are where the device samples the format and the atlas
  is whole blocks (`regia_render::lightmap::lightmap_native_blocks`), else the RGB9E5 decode. Error against the RGB9E5
  texels the blocks were encoded from, per channel relative to the texel's brightest channel: at most 5 % (BC6H) and
  6 % (ASTC HDR) on smooth content (`BC6H_MAX_REL_ERROR`, `ASTC_HDR_MAX_REL_ERROR`); empty texels do not pull the fit.
- **Coverage**: one byte per texel. 0 empty (no surface), 1 baked (the texel centre lies on a surface), 2 dilated (a
  gutter texel, or a texel whose centre was inside geometry, holding the mean of its filled neighbours). Any other
  value is refused.
- **Geometry**: all entries' unwrapped vertices, then all entries' indices. A vertex is 12 bytes: `xref` (`u32`, the
  source vertex it copies), then UV2 `u`, `v` (`f32`, `0..=1` over the entry's rectangle). An index is a `u32` into
  the entry's own unwrapped vertices. The triangle list keeps the source mesh's triangle order. A chart seam splits a
  vertex, so `vertex_count >= ` the source's used vertices.
- **Directions** (version 2, `direction_encoding` 1): one `u32` per texel, row-major like the texels, RGBA8 unorm
  with red in the low byte. `rgb = d · 0.5 + 0.5`, where `d` is the luminance-weighted mean direction (world space)
  of the light baked into the texel, `|d| <= 1` (1 when it all comes from one direction, 0 when it comes evenly
  from everywhere). `a = n0 · d` clamped to `0..=1`, where `n0` is the normal the texel was baked for. The runtime
  uploads the section unchanged as an `Rgba8Unorm` texture. Gutters and inside texels are filled from their
  neighbours like the irradiance. A `direction_encoding` above 3, or a section of the wrong size, is
  `lightmap_payload`.
- **Block-compressed directions** (ADR-2635, `direction_encoding` 2 or 3, still version 5: the header's shape
  is unchanged, and an older reader refuses the new values as `lightmap_payload`): the same RGBA8 directions as BC7
  (unorm) or ASTC 4×4 LDR (linear) blocks, 16 bytes each, row-major over `⌈width/4⌉ × ⌈height/4⌉`. A reader decodes them
  into `Lightmap::directions` (so every CPU path reads one form) and keeps the blocks for the GPU, which samples them
  where it can (`Bc7RgbaUnorm`, `Astc 4×4 Unorm`) and gets the decode elsewhere. Only the cook writes them
  (`.meta` settings `directions: rgba8 | bc7 | astc_ldr | auto`, `regia_asset_pipeline::lightmap_cook`); a bake writes
  RGBA8.
- **Clusters** (version 4, `regia bake-lightmaps --meshlets`): the entries' blobs, one after another with no padding. A blob is a
  meshlet payload in the `.fmlt` layout (`regia_render::visbuffer::MeshletMesh::to_bytes`, its own magic and version) built over
  the entry's unwrapped vertices and indices, in the source mesh's object space. `regia_asset` does not read the blobs. Sizes that
  do not add up to the section are `lightmap_payload`. A blob the runtime cannot decode, of another meshlet version for example,
  costs only the visibility path: the receiver draws in the batches until a re-bake. Clusters are derived data, as cooked
  `.fmlt` are, so the migration rule for a meshlet version change is a re-bake.

## 2. UV2 and texel-density rules

`regia_asset_pipeline::lightmap_uv` builds the layout; `regia bake-lightmaps` exposes the settings.
`--unwrap atlas` (the default, `build_atlas_layout`, ADR-2024) uses the UV atlas generator
(`book/agents/atlas.md`) for the charts and packs each mesh by the charts' footprints, cropped to its rectangle; rules
2 to 5 below hold, with the chart's size coming from its flattened outline. `--unwrap charts`
(`build_lightmap_layout`, ADR-860) uses rule 1.

1. **Charts.** Triangles are grouped over shared edges, with positions welded so that UV0 or normal seams in the
   source do not cut a chart. A triangle joins while its normal stays within `--chart-angle` (default 60°) of the
   chart's area-weighted normal. A member that ends more than 85° from the final normal is charted again. Each chart
   is projected onto its plane and turned to its minimum-area bounding rectangle.
2. **Density.** A chart's interior is `ceil(extent · density)` texels per side, at least 2. Every chart gets
   `--padding` (default 2) gutter texels on each side. `density = --density (default 8 texels/m) × the mesh's
   density_scale × fit_scale`.
3. **Per-mesh scale.** `UnwrapInput::density_scale` raises (hero pieces) or lowers (background) one mesh's density.
   A level's `LightmapSurface::density_scale` sets it; a built-in or a bake description uses 1.
4. **Atlas fit.** Charts are packed into the mesh's own rectangle, then mesh rectangles into an atlas whose width is
   a power of two and whose height is a multiple of 4. If the atlas would pass `--max-atlas` (default 4096),
   `fit_scale` drops by 0.8 and everything is laid out again, up to 24 times. The dry run reports `fit_scale` and the
   attempts. Then `lightmap_uv_atlas_full`.
5. **Texel size check.** Each entry records the density it got (`texels_per_metre`), so a review can find
   under-resolved meshes without baking.

**Pages (ADR-3300).** When the rectangles do not fit one atlas of `--max-atlas`, the layout opens pages of `--max-atlas`
(rounded down to whole 4×4 blocks) a side instead of lowering the density: the skyline packer fills page 0, and the
rectangles it could not place go to the next page, in the same order, up to `--max-pages` (default 4). Only past that
budget does `fit_scale` drop by 0.8 per attempt, as before. A mesh's rectangle is one entry on one page, so its charts never
straddle two pages; a mesh larger than a page lowers the density. A level that fits one atlas is laid out exactly as before,
whatever `--max-pages` says. Every page has the size of the tallest page's packing.

## 2a. Chart seams

Where a smooth surface is cut in UV2 the two sides read different texels. `regia_asset::lightmap::seam_report`
measures it at the bake, against the source mesh the `.flmap` does not hold. A seam edge is an edge whose two sides
read different UV2, with positions welded so a glTF normal or UV0 split does not hide it. It usually joins two charts
(connected sets over the unwrapped vertices), but a chart can also meet itself along a slit, as a tube unrolled into one
chart does; that edge counts once. A face bend above 30° is a crease, where a step in the light is correct: it is counted as `hard_edges`
and left out. On each seam edge, at up to 16 points along it, the step is `|La - Lb| / max((La + Lb) / 2, floor)`, where
`La`, `Lb` are the luminances the runtime's bilinear lookup reads on each side and `floor` is 2 % of the entry's mean baked
luminance. `mean` and `p95` are weighted by the edge's length in texels. The budget limits `p95`: `[lightmaps]
seam_budget` in `Regia.toml` (default 0.25) in a shipping `regia package`, `--seam-budget` on one bake; past it is
`lightmap_seam_budget`.

## 3. Reader rules

- Versions `1..=7` read. A version 1 header decodes with the version 1 shape (`LightmapHeaderV1`) and comes back
  with `direction_encoding` 0 and an empty fourth section; a version 1 or 2 header's entries (`LightmapEntryV2`) come back
  with no `key` and no `seams`; a version 1 to 3 header (`LightmapHeaderV3` for 3) comes back with an empty cluster section;
  a version 1 to 4 header (`LightmapHeaderV4` for 4) comes back with no denoise record, and only texel encoding 1 is accepted
  below version 5; a version 1 to 5 header (`LightmapHeaderV5` for 5) comes back with no source hashes; a version 3 to 6 header's entries
  (`LightmapEntryV6`) and a version 6 header (`LightmapHeaderV6`) come back as one page, every entry on page 0. A newer file is `lightmap_version`, a wrong magic `lightmap_bad_magic`, a header that does
  not decode `lightmap_header`, a section or hash problem `lightmap_payload`, an inconsistent layout
  `lightmap_layout`, and a file-system error `lightmap_io`.
- `Lightmap::read_header` reads only the header (listings, `--inspect`).
- Sampling (`Lightmap::sample`, the CPU mirror of `shaders/lightmap.wgsl`): bilinear in atlas UV with texel centres
  at `(i + 0.5) / width`, on page 0 (`Lightmap::sample_page` for another page, `sample_entry` on the entry's page; the
  filter never reads across pages, as a texture array's layers do not filter into each other). Empty texels get weight 0 and the rest renormalise. The GPU sampler is plain bilinear: the
  dilated gutters stand in for the renormalisation.
- Bicubic (`Lightmap::sample_bicubic`, the runtime default): a uniform cubic B-spline over the 4×4 texels around
  the lookup, clamp to edge, no renormalisation. The GPU evaluates it with four bilinear taps. Its footprint reaches
  two texels past the lookup, which the bake's `padding + 1` dilation rounds cover at the default padding of 2.
- Directional shading (`regia_asset::lightmap::shade_directional`): for a unit shading normal `n`,
  `E(n) = E · max(1 + n · d, 0) / max(1 + a, 1)`. It is `E` at the baked normal, unchanged when `d = 0`, and at
  most `2 E`. The direction is read with the plain bilinear sampler (`Lightmap::sample_direction`).

## 4. Migrator plan

The registry entries are `lightmap` in `cargo xtask check-formats` (the header's serde shape) and in
`regia upgrade-project` (suffix `.flmap`, `FLMP` + version). Version 1 → 2 is `regia_asset::lightmap::migrate_lightmap_file`: decode with the version 1
rules, encode at version 2 with `direction_encoding` 0. Texels, coverage and geometry are copied byte for byte and
the payload hash stays the same (`version_1_files_read_and_migrate_to_version_2_unchanged`, checked-in fixture
`crates/regia_asset/tests/fixtures/lightmap-v1.flmap`, written by the v0.1.28 encoder).

Version 2 → 3 is the same function: decode with the version 2 rules, encode at version 3 with no key and no figures (a
re-bake from a level fills them). The sections are copied byte for byte and the payload hash stays the same
(`version_2_files_read_and_migrate_to_version_3_unchanged`, fixture `crates/regia_asset/tests/fixtures/lightmap-v2.flmap`,
written by the v0.2.10 encoder).

Version 3 → 4 is the same function: decode with the version 3 rules, encode at version 4 with an empty cluster section (a
re-bake with `--meshlets` fills it). The sections are copied byte for byte and the payload hash stays the same
(`version_3_files_read_and_migrate_to_version_4_unchanged`, fixture `crates/regia_asset/tests/fixtures/lightmap-v3.flmap`:
the version 2 fixture with a key and seam figures on its first entry, written by the version 3 encoder).

Version 4 → 5 is the same function: decode with the version 4 rules, encode at version 5 with no denoise record and the
texels kept RGB9E5 (encoding to blocks is a cook or bake choice, never a migration). The sections are copied byte for
byte and the payload hash stays the same (`version_4_files_read_and_migrate_to_version_5_unchanged`, fixture
`crates/regia_asset/tests/fixtures/lightmap-v4.flmap`: the fixture with directions and a cluster blob on entry `a`,
written by the version 4 encoder).

Version 5 → 6 is the same function: decode with the version 5 rules, encode at version 6 with no source hashes (a re-bake
fills them, and until then a receiver of the file reports as unverified, not stale). The sections are copied byte for byte
and the payload hash stays the same (`version_5_files_read_and_migrate_unchanged`, fixture
`crates/regia_asset/tests/fixtures/lightmap-v5.flmap`: the version 4 fixture migrated by the version 5 encoder).

Version 6 → 7 is the same function: decode with the version 6 rules (every entry on page 0), encode at version 7 with
`pages` 1. The sections are copied byte for byte and the payload hash stays the same
(`version_6_files_read_as_one_page_and_migrate_unchanged`, fixture `crates/regia_asset/tests/fixtures/lightmap-v6.flmap`:
the version 5 fixture as the version 6 migrator writes it, its header ending in an empty source-hash list).

The next planned shapes, each a version bump with its migrator:

| Version | Change | Migration |
|---|---|---|
| 2 (done, ADR-1556) | Directional lightmaps: a fourth section with the dominant light direction per texel and `direction_encoding` in the header. No contents bit: the encoding field says it | A v1 file reads as v2 with no direction section. `upgrade-project --write` rewrites the header with the new section table; the texels are unchanged |
| 3 (done, ADR-2024) | `key` (the entity and part) and `seams` (the chart-seam figures) on every entry | A v1 or v2 file reads with neither. `upgrade-project --write` rewrites the header; the sections are unchanged |
| 4 (done, ADR-2107) | A section of clusters, one blob per entry, built over the entry's unwrap | An older file reads with none. `upgrade-project --write` rewrites the header; the other sections are unchanged |
| 5 (done, ADR-2314) | Texel encodings 2 (BC6H) and 3 (ASTC 4×4 HDR) in the texel section; the `denoise` record | An older file reads with RGB9E5 texels and no record. `upgrade-project --write` rewrites the header; the sections are unchanged |
| 6 (done, ADR-2656) | `source_hashes`: a hash of each entry's source mesh | An older file reads with none, and its receivers report as unverified. `upgrade-project --write` rewrites the header; the sections are unchanged |
| 7 (done, ADR-3300) | Atlas pages: `page` on every entry, `pages` in the header, sections holding every page | An older file reads as one page. `upgrade-project --write` rewrites the header; the sections are unchanged |
| 8 | UV2 in the cooked mesh instead of the geometry section (not on cooked mesh v3, which stores UV0 only; `docs/specs/12-assets.md` § 7.2) | The v7 → v8 step moves each entry's unwrap into the cooked mesh. A file whose mesh changed since the bake cannot migrate, so its entries are flagged for a re-bake |

`Lightmap::from_bytes` keeps reading every older version, as `.fgiv` does (`BakedGiVolume::from_bytes`).

## 5. Cook

A `.flmap` under `assets/` cooks (`regia cook`, `regia_asset_pipeline::cook`) to a cooked file whose payload is the
lightmap re-encoded at the current version (type path `regia_asset::lightmap::Lightmap`). An older file comes out
migrated; a damaged or newer one is a cook error with the reader's `lightmap_*` code. Packages ship the cooked copy
in the pak, not the raw file. At runtime `regia_render::lightmap::read_cooked_lightmap` reads it through the asset
server by its manifest path (`assets/maps/bore.flmap` or `maps/bore.flmap`), and `render.set_lightmaps { path }`
tries the cooked copy before the raw file.

The texels are kept as they are by default. The lightmap's `.meta` `settings` string (RON,
`regia_asset_pipeline::lightmap_cook::LightmapCookSettings`) may ask for blocks (ADR-2314):

```ron
(encoding: auto)
```

`rgb9e5` (the default), `bc6h`, `astc_hdr`, or `auto`: BC6H for desktop platforms and ASTC 4×4 HDR for `android`. A phone
without ASTC HDR still loads the file (the renderer uploads the CPU decode), so `auto` is safe there, but it costs the
decode at load and the RGB9E5 memory. Settings that do not parse are a cook error (`lightmap_cook_settings`).
