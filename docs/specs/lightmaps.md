# Lightmaps (v0.3.0): what shipped

Status: **built** (v0.2.4 to v0.2.14, the v0.3.0 line). This page describes the lightmap path as the code has it on
`integ/v037c` (2026-11-05), re-audited against `lane/sprint-integ-v047` on 2026-11-10 (ADR-5663: receiver specular,
the ADR-4108 baker and denoiser, the merged phone, bake-at-scale and gallery work), and lists what is still open (§ 11).
It replaces the phase plan of ADR-1100: that design's
cooked-mesh UV2, `.flmp` set format, `editor.lightmap_uv` and importer modes were not merged (ADR-1910 (3)); the shipped
path is the one below. The rewrite is ADR-3211; the unwrap and per-tier rules follow ADR-3001.

| Part | Where | Decisions |
|---|---|---|
| UV2 atlas, unwrapped by the baker | `regia_asset_pipeline::lightmap_uv`, `::atlas` | ADR-860, ADR-1101, ADR-1483, ADR-2024 |
| CPU baker | `regia_pt::lightmap` | ADR-860, ADR-1101, ADR-1556 |
| GPU baker (tiles, memory chunks, device loss, PSNR floor) | `regia_pt::lightmap::gpu` (`lightmap-gpu` feature) | ADR-2024, ADR-2125 |
| Denoise, BC6H and ASTC 4×4 HDR texels | `regia_pt::lightmap::denoise`, `regia_asset::hdr_blocks` | ADR-2314, ADR-2331 |
| Crease placement, seam stitching, automatic density | `regia_pt::lightmap::{crease, stitch, density}` | ADR-2404 |
| `.flmap` format (version 7) and migrator | `regia_asset::lightmap`, `docs/formats/lightmap.md` | ADR-860, 1556, 2024, 2107, 2314, 2656, 3300 |
| Cook (per-platform texel encoding) | `regia_asset_pipeline::lightmap_cook` | ADR-1556, ADR-2314 |
| `regia bake-lightmaps` (built-in scenes, `.bake.ron`, levels) | `regia_cli` `commands/bake_lightmaps` | ADR-860, ADR-2024, ADR-2313 |
| Runtime: receivers, the `LIGHTMAP` forward variant | `regia_render::lightmap`, `shaders/lightmap.wgsl` | ADR-1623 |
| Receiver indirect specular from the lightmap's radiance field | `regia_render::lightmap` (`LightmapSettings::specular`) | ADR-4762 |
| Per-tier lightmaps from one bake | `regia_asset_pipeline::lightmap_tiers`, `regia cook --lightmap-tiers` | ADR-3120, 4930, 4995, 5125 |
| Every path and tier (GPU-driven, visibility buffer, Mobile lite) | `regia_render` | ADR-2107, ADR-2403, ADR-2689 |
| Receivers saved in levels, warmed on load | `regia_framework` `LightmapLevelPlugin`, scene v11 `look.lightmap` | ADR-2313 |
| Editor Lightmaps panel, chart-seam metric | `regia_editor` `lightmap_panel.rs` | ADR-2024 |
| Evidence pack against the path tracer | `docs/evidence/lightmaps-v030/`, `regia_pt/tests/lightmap_pt_evidence.rs` | ADR-2315 |

How to use it: `book/agents/lightmaps.md` (commands, JSON, error codes, agent methods). The file format:
`docs/formats/lightmap.md`. The UV atlas generator: `book/agents/atlas.md`.

A lightmap stores the baked indirect light of a static surface per texel, in a second UV set (UV2) unique to that
surface. It adds what the probe volume cannot give: light that follows walls, floors and set pieces at texel scale
(contact darkening at a wall foot, a lit doorway on a floor, an interior that stays dark next to a sunlit outside wall)
without probes leaking through thin geometry. Dynamic objects keep the probe volume.

## 1. Scope

| In | Out (stays on the probe volume or live lighting) |
|---|---|
| Static meshes (`GiMobility::Static` in a level; every non-emissive, lit mesh of a `.bake.ron`) | Skinned, morphing and moving meshes; units; VFX |
| Standard opaque and masked materials, rigid, drawn whole (LOD 0) | Material graphs, blended and transmissive materials, LOD chains and cross-fades, mask decals (`forward.unsupported`) |
| Sky, sun bounce, emissive surfaces, point, spot and sphere lights (and the sun seen directly with `--direct-sun`) | The live sun's specular (it stays live) |
| A receiver's indirect diffuse, and its indirect specular read from the same baked field (ADR-4762, § 7.1) | Specular of a `--direct-sun` map (its atlas cannot separate direct and indirect light: probes and sky give it) |
| Desktop (RGB9E5 or BC6H) and Android (RGB9E5 or ASTC 4×4 HDR) | The terrain heightfield (its own terrain GI covers it) |

## 2. UV2 atlas

**Automatic unwrap.** The baker lays out UV2 itself for every static mesh, whatever UV sets it carries (`TEXCOORD_0`
only, a tiling texture UV far outside 0..1, is the normal case for game content): a mesh needs no authored second UV set,
and the source glTF is never written. The unwrap is stored per entry in the `.flmap` geometry section (positions,
normals, UV2, indices of the receiver mesh), so the runtime draws a receiver from its entry's mesh, with the extra
vertices a chart seam needs, in place of the entity's own mesh (ADR-1623). The cook carries that geometry section into
the package with the texels (§ 6.2), so the shipped game has the unwrap without a lightmap UV in any source or cooked
mesh, and after a bake the only new file under `assets/` is the `.flmap` (ADR-3001 (3); the `lightmap_gallery` tests
`texcoord0_only_meshes_unwrap_at_bake` and `the_cook_carries_the_unwrap`).

- `--unwrap atlas` (the default): the UV atlas generator (`regia_asset_pipeline::atlas`, ported from xatlas with its MIT
  notice, ADR-1483): low-stretch charts on curved meshes, packed by footprint. `book/agents/atlas.md`.
- `--unwrap charts`: the planar generator of ADR-860 (`lightmap_uv::generate_lightmap_uv`): charts grown over shared edges
  inside a normal cone (`--chart-angle`, default 60°), planar projection, minimum-area rectangle, skyline packing.
- Chart borders lie on texel centres; a border texel traces from just inside its triangle, so rays do not leave through
  the joint it touches (ADR-2024).
- Authored `TEXCOORD_1` is read only through the Rust API (`regia_pt::lightmap::bake_mesh_lightmap`, one entry with a given
  size and gutter). The CLI always unwraps. The cooked mesh asset itself carries no UV2: the unwrap lives in the cooked
  `.flmap` (§ 11).

## 3. Texel density

- `--density` texels per metre of world surface (default 8), `--padding` gutter texels per chart (default 2),
  `--max-atlas` the largest atlas side (default 4096). When the charts do not fit, the density scales down until they do
  (`atlas.fit_scale` below 1; every entry reports the `texels_per_metre` it got).
- `LightmapSurface { density_scale, receive }` on a level entity multiplies the density for it (hero pieces above 1) or
  leaves it to the probes while it still shadows and bounces (`receive: false`).
- **Automatic density** (ADR-2404, `--auto-density on`, the default): a receiver whose predicted chart-seam `p95`
  (`K · κ / d`, `κ` the mean curvature of its smooth bent edges) passes the seam budget (`--seam-budget`, else 0.25) is laid
  out at the smallest power-of-two scale up to 8 that brings it under. A scale the level set is kept; a receiver still
  predicted over the budget is the warning `lightmap_density_low`.
- `--min-area` leaves small props to the probes, so the atlas spends its texels on walls and set pieces.
- **Per tier** (ADR-3001 (4), (5)): lightmap resolution is set per package tier by the bake's texel density. A game
  can bake each tier with `--density`, or derive package files from one HIGH/ULTRA bake through
  the per-tier cook (§ 6.2). Derived tiers pass the seam budget and the dark-point check since ADR-5125 (§ 11):

  | Package tier | Density (texels/m) | Notes |
  |---|---|---|
  | ULTRA | 16 | |
  | HIGH | 8 | The CLI default |
  | LOW | 4 | |
  | ULTRA_LOW (iGPU 720p30, 2.5 GB RAM, 1.5 GB VRAM) | 2, or no lightmap | Block-compressed and `--no-directional` (the direction section is 4 bytes a texel in every encoding); without a lightmap the probe volume lights it |

  The runtime tier then caps the lookup (§ 7.2), not the texel count. A device whose 2D texture limit is below the atlas
  size draws every receiver with the probe light (`forward.unsupported`), so bake that tier with a smaller `--max-atlas`.
  A lightmap is part of its map's load set (`docs/specs/12-assets.md` § 5b), loaded before the player has control, so
  its bytes at the tier's encoding count against the map's instant-load and memory budgets like any texture; a tier
  whose budget does not hold it ships without it. Bytes per tier on the interior gallery:
  `docs/evidence/lightmaps-v030/SUMMARY.md` § 8. The single-bake tier cook is implemented
  (ADR-3120). Since ADR-5125 the derived LOW and ULTRA_LOW tiers of the gallery are under the 0.25 seam budget (p95
  0.063 and 0.147) with no dark point, measured in the bake's own two-texel window in world units (the ADR's
  refinement of the tier-texel window; `docs/evidence/lightmaps-v030/closure-e-2026-11-10.md`).

## 4. Seams and gutters

1. **Where seams go.** Chart borders follow hard edges and the generator's stretch limits.
2. **Gutters.** `padding` empty texels around every chart. Lightmaps have no mip chain and are sampled at level 0, so mips
   cannot bleed across charts. After the bake, empty texels inside an entry's rectangle next to filled ones take the mean
   of their filled neighbours, `padding + 1` rounds, so a bilinear or bicubic tap at a chart edge reads the chart's own light.
3. **Inside texels.** A texel whose hemisphere rays mostly hit back faces (more than half, `inside_threshold` 0.5) sits
   inside geometry (a floor under a wall): it is dropped and refilled from its neighbours (`report.texels_inside`).
4. **Crease placement** (ADR-2404, `--crease on`): such a texel is first moved to the nearest point of its own chart outside
   the geometry within 0.75 texels and sampled there, so it holds the light at the joint (the contact darkening). Only a
   texel with no such point is refilled, and the first refill ring continues the light's gradient across the edge
   (`2 · v_edge − v_inner`, clamped), so the bicubic lookup keeps the edge texel's light at the crease.
5. **Stitching** (ADR-2404, `--stitch on`): along each smooth 3D edge shared by two charts (bend under 30°), sample pairs at
   twice the texel spacing and solve `Σ (g · x)² + 0.05 Σ (x − x₀)²` by preconditioned conjugate gradients after the
   refill, over the baked and gutter texels the two lookups read (Iwanicki, *Lighting Technology of The Last of Us*,
   SIGGRAPH 2013). Creases are left alone: a step in the light is right there. Every pair of faces on a welded edge
   is a seam pair, not just the first face against the others, so four faces meeting where two closed blocks touch are
   all stitched (ADR-4108 (5)).
6. **Seam metric** (ADR-2024): every bake measures the relative luminance step across the edges where a smooth surface is
   cut in UV2, per entry and per chart (`seams`, `docs/formats/lightmap.md` § 2a; every face pair of a welded edge,
  ADR-4108 (5)), and writes the figures into the file.
   `--seam-budget` fails a bake over it after writing the file; a shipping `regia package` reads `[lightmaps]
   seam_budget` in `Regia.toml`.
7. **Block compression.** A 4×4 block that straddles two charts loses more precision; the gutters keep most blocks inside
   one chart.

## 5. Baking

`regia bake-lightmaps` bakes a built-in scene (`cornell`, `furnace`, `rolling-hills`, `quad-box`), a GI bake description
(`.bake.ron`, the file `regia bake-gi` reads) or a level (`--scene <level>`, the same export as `regia bake-gi
--from-scene`: its static meshes, sun and sky; entries keyed by entity and part). It is a separate command from
`regia bake-gi`; both read the same inputs, so the volume and the lightmap see the same lights.

- **CPU baker** (`regia_pt::lightmap`): per texel, Monte Carlo path tracing over Lambertian surfaces with cosine-weighted
  hemisphere rays and next-event estimation at every vertex (the sun as a cone, point, spot and sphere lights, emissive
  triangles picked by power). The sun seen directly is baked only with `--direct-sun`; by default it stays live with
  its shadows and the lightmap holds the sky, the sun's bounces, emissive surfaces and local lights. Binned SAH BVH,
  Möller-Trumbore tests, origins offset by a few float steps along the face normal. The first bounce is stratified
  (ADR-4108 (1)): a texel's hemisphere direction `i` takes point `i` of the first two Sobol dimensions with a per-texel
  random digital shift (Kollig and Keller 2002), in the CPU and the GPU baker alike; deeper dimensions keep the PCG stream.
- **Directions** (ADR-1556): every light contribution adds its direction weighted by its luminance; the mean is the
  texel's dominant direction, stored in the direction section so normal maps bend the baked light. It costs no extra
  random numbers (`--no-directional` leaves it out).
- **Bounded samples** (ADR-1556): `--samples` 1..=65536, `--bounces` at most 64 (`lightmap_bake_settings`). The bake
  reports its noise before any filter (`report.noise`, the relative standard error of each texel's mean luminance);
  `--noise-target F --max-samples N` keeps a texel sampling until it is under `F` or at `N`.
- **GPU baker** (ADR-2024, ADR-2125; `--gpu`, a CLI built with `lightmap-gpu`): the same light up to float rounding, held
  to the CPU baker at 40 dB PSNR (`LIGHTMAP_GPU_PSNR_FLOOR_DB`, `regia_pt/tests/lightmap_gpu.rs`). It runs in tiles of
  about `--gpu-tile-rays` rays with at most one queued, can pause between tiles (`--gpu-yield-ms`) so an editor on the
  same GPU stays responsive, bakes the texel list in chunks under `--gpu-memory-mib`, ends with
  `lightmap_gpu_device_lost` on a driver reset (nothing written) and falls back to the CPU with `--gpu-fallback`. Tiles,
  chunks and pauses never change the file. Measured on the RTX 5090: 9.8 to 10.4 times the CPU bake.
- **Denoise** (ADR-2314, `--denoise on`, the default for new bakes): an edge-avoiding à-trous filter (Dammertz et al.,
  HPG 2010), 5×5 taps at spacing 1, 2, 4, … (`--denoise-iterations`, 1..=8), weighted by normal, world distance and the
  luminance difference in units of each texel's own measured noise (Schied et al., HPG 2017). Without
  `--denoise-iterations` the pass count is the one whose reach is nearest 3.75 m: 2 / 3 / 4 / 5 passes at
  2 / 4 / 8 / 16 texels/m (`default_denoise_iterations`, ADR-4108 (4)). A tap that leaves its chart reads only the surface
  that continues it: the nearest baked texel of another chart within 0.75 texel whose normal is within 30°, found
  through the chart's UV2 Jacobian, and in sight of the texel (no geometry on the segment between the two, lifted off
  their surfaces); creases, other walls and other entries stay out, and so does a parallel surface behind a wall (a
  sealed room's floor and the ground outside; ADR-4108 (2), ADR-7590, `chart_borders_stay_exact`,
  `a_wall_between_two_charts_blocks_the_taps_across`). Each pass blends the taps' mean toward a weighted plane fit by its estimated error, so a
  one-sided kernel at a border follows a real gradient and does not extrapolate noise (ADR-4108 (3)).
  `--denoise off` runs the older 3×3 edge-aware filter (unless `--no-filter`).
- **Determinism:** every texel's random stream is a hash of the seed, the entry's name, the texel's place inside the
  entry and the sample, so the bytes depend neither on `--threads` nor on where the packer put the entry. A level bake
  does not depend on the order of the level's entities.
- **Editor:** the Lightmaps panel runs the CLI in a child process (lay out, bake with progress, cancel), shows the atlas by
  density or by seam `p95`, and draws the worst seams in the viewport (`editor.lightmap_*`, § 9).

## 6. Storage

### 6.1 `.flmap`

One or more atlas pages of per-texel irradiance with the unwrap of every entry. Version 7, not frozen until the
v0.3.0 release; versions 1 through 6 read, and `regia upgrade-project --write` rewrites them at version 7, preserving
their texel sections and treating older entries as page 0. Full layout: `docs/formats/lightmap.md`.

| Version | Adds | Decision |
|---|---|---|
| 1 | Texels (RGB9E5), coverage, geometry (per-entry receiver mesh with UV2) | ADR-860 |
| 2 | Direction section (dominant direction per texel, RGBA8) | ADR-1556 |
| 3 | Entry keys (entity and part) and the chart-seam figures | ADR-2024 |
| 4 | Cluster section: visibility-buffer clusters built over each entry's unwrap (`--meshlets`) | ADR-2107 |
| 5 | Texel encodings 2 (BC6H) and 3 (ASTC 4×4 HDR), the denoise record | ADR-2314 |
| 6 | Per-entry source mesh hashes for receiver staleness | ADR-2656 |
| 7 | Atlas pages, per-entry page selection, page-major sections | ADR-3300 |

- **Staleness** (built, ADR-2656): `.flmap` version 6 records a hash of each entry's source mesh (positions and triangles, `regia bake-lightmaps`). A receiver's
  UV2 comes from the entry itself and the file's payload hash guards it, so only the mesh can go stale. `lightmaps.stale_instances`
  (the `lightmaps` object of `render.get_gi_mode` and `level.get_baked_gi`, § 9) counts receivers whose hash differs, whose triangles no longer fit their entry, or whose entry is gone. Other
  triangles and a missing entry fall back to the volume as before (`lightmap_layout`, `lightmap_entry`); moved vertices
  over the same triangles follow `LightmapSettings::stale_receivers` (ADR-3006): by default they are reported and still
  drawn, because the light is close and the fix is a re-bake; `fall_back` draws them with the frame's other light and
  reports `lightmap_receiver_stale`.
- **Pages** (built, ADR-3300; `docs/formats/lightmap.md` version 7): a level past one atlas keeps its density on up to
  `--max-pages` pages of `--max-atlas` (each entry records its page, `rect` stays in page texels), baked page by page on
  the stacked grid so filters, gutters and seams never cross a page, and sampled at run time from a texture array layer
  picked by the entry's page (packed into `scale_offset.z`). Additive levels stack in one page while it fits and append
  pages when one would overflow.

### 6.2 Cook

A `.flmap` under `assets/` cooks into the pak at the current version (`regia cook`), every section included: the
geometry section with each entry's unwrap travels with the texels, so the package needs no mesh with a lightmap UV
(`cook_lightmap` in `regia_asset_pipeline::cook`). The runtime reads the cooked copy first (`render.set_lightmaps { path }`, the level's `look.lightmap`). The lightmap's `.meta` `settings` picks the texel
encoding per platform: `(encoding: auto)` gives BC6H on desktop platforms and ASTC 4×4 HDR on Android, 1 byte a texel
instead of 4; `rgb9e5` (the default) keeps the texels. A device that cannot sample the blocks (ASTC HDR is optional on
phones) gets their RGB9E5 decode, so a file always loads. The encodings stay within 5 % (BC6H) and 6 % (ASTC HDR) of the
baked texels per channel on smooth light; on the quad-box floor 33.31 dB against 33.52 dB for the denoised RGB9E5 map.
`directions` does the same for the direction section (ADR-2635): `rgba8` (the default), `bc7`, `astc_ldr`, or `auto`
(BC7 on desktop platforms, ASTC 4×4 LDR on Android), for example `(encoding: auto, directions: auto)`
(`regia_asset_pipeline::lightmap_cook`; a failed direction encode is `lightmap_direction_encode`).

Per-tier lightmaps (built, ADR-3120): one bake at the top tier's density; the cook derives every package tier's
lightmap from it (`regia_asset_pipeline::lightmap_tiers`: the same unwrap, entry rectangles scaled by a power of two,
chart-aware area-mean downsample and gutters, the tier's encoding, the direction section dropped where the tier says),
`Regia.toml` `[lightmaps.tiers]` over ADR-3001's table, `[profiles.<name>] lightmap_tier` for the package, `regia cook
--lightmap-tiers` for the per-tier bytes and budget verdicts. No `.flmap` change. The first derived gutter ring continues
same-chart baked gradients with the baker's bounded linear extrapolation (ADR-4930); later waves keep chart-local means.
Then a bounded least-squares fit restores the bake's stitched border light along every chart border where the bake's
light is near-linear over two tier texels, so seams the bake stitched stay stitched without the mesh (ADR-4995).
Each tier rule has a chart `padding` (`[lightmaps.tiers.<name>] padding`, 1..=16 texels; unset, 2 when the tier resolves
to a 4×4 block encoding on the cook's platform and 1 otherwise, ADR-5621; out of range is refused when the table loads).
When the bake's
padding over the factor leaves fewer whole tier texels per chart side than that (ULTRA_LOW from the default 2-texel
bake padding), the derive repacks each entry's charts at the tier's own resolution: every baked chart (split unwrap
pieces the bake packed touching move together) keeps its tier size, moves by whole tier texels, and is packed with the
baker's skyline packer with `padding` gutter texels per side. Only UV2 changes, by a per-chart translation, and the
tier's `.flmap` carries its own unwrap; receivers are built from the installed file (ADR-5125). The default table's
ULTRA_LOW, which ships a 4×4 block encoding, pads by two texels so no block holds two charts' borders. On a steep border
(the bake's light not linear over two tier texels inward) the border fit holds the on-chart texels and still fits the
gutter texels, bounded by the range between their fill and the bake's border light. A derived tier's dark-point
acceptance compares against the bake's own two-texel window in world units (ADR-5125).

## 7. Runtime

### 7.1 Receivers

A *receiver* is a static mesh entity with a `LightmapReceiver { entry }` (`regia_render::lightmap`, reflected and saved
with the level). While the lightmap is installed and enabled (`LightmapSettings::enabled`, off by default), the renderer
draws the receiver from its entry's mesh and UV2 stream with the `LIGHTMAP` forward variant: one
`lightmap_receiver_irradiance` call (bicubic by default, with the direction rebalance) takes the place of the probe or
flat-ambient diffuse (ADR-1623). A receiver of a `--direct-sun` map (`sun_direct` in its contents) also drops the live
sun's diffuse and keeps its specular. A receiver's indirect specular reads the same baked light as a radiance field in
the GGX lobe (`lightmap_lobe_radiance`; a uniform field without directions), in place of the sky, probe and interior
specular, and the baked diffuse gives up the reflected share (`LightmapSettings::specular`, default on,
`render.set_lightmaps { specular }`; ADR-4762). A local capture's share and a pane's traced hit keep theirs, and the
stochastic SSR replaces it where a reflection hits, as any environment specular; a `--direct-sun` map keeps the older specular sources, since its atlas cannot separate direct and indirect
light. Everything else keeps the probe light, and a frame without an enabled lightmap is unchanged. An entry whose mesh does not match the receiver's (the mesh changed since the bake) is `lightmap_layout`.

### 7.2 Paths and tiers (ADR-2107)

Receivers draw on every path: GPU-driven batches (one instanced draw per mesh and material, a per-slot `scale_offset`
table), the visibility buffer (cluster instances over the bake's `--meshlets` clusters, UV2 in its own range of the
geometry words; the resolve takes the `LIGHTMAP` variant per material, ADR-2689) and the Mobile lite shader (from the CPU
color loop). `RenderProfile::lightmap_tier` caps the lookup:

| Preset | Tier | Lookup |
|---|---|---|
| Mobile Low | `bilinear` | One bilinear fetch; the direction section is not uploaded (half the GPU bytes) |
| Mobile High | `directional` | One bilinear fetch for the light, one for the direction |
| Deck, every Desktop preset, Cinematic | `full` | Bicubic (four fetches) and the direction |

The lite shader reads every sun cascade the frame planned (ADR-2403), so a forced lite tier keeps its shadows.
ADR-4930 refines its direct-light approximation to the existing GGX/Smith/Schlick model with the
material's optional direct energy compensation. One shared DFG-table fetch per fragment also supplies
the lightmapped indirect energy partition. This changes neither the tier's atlas-fetch count nor its
direction upload, shadow limits, file encoding or original quality bars.

### 7.3 Levels and warming (ADR-2313)

`regia bake-lightmaps --scene <level> --write-level` names the file in the level (scene format v11, `look.lightmap`).
Opening the level, the framework's `LightmapLevelPlugin` asks for the lightmap warm, installs the file (cooked first),
enables it, and gives a receiver to every drawn entity an entry was baked for, part entities (`MeshPartOf`) included.
Closing the level removes what it installed and puts the host's settings back. A frame warm with a lightmap installed
builds the `LIGHTMAP` variants of every target and path it drew, so no pipeline is created mid-play for a receiver.

## 8. GI interplay with the probe volume

- **Who reads what.** A receiver takes its diffuse indirect light from the lightmap, and with
  `LightmapSettings::specular` (the default) its indirect specular too (ADR-4762, § 7.1); every other draw, and the
  specular of a receiver with the switch off or of a `--direct-sun` map, keep the probes. The forward pass takes the
  lightmap *instead of* the probe terms (a per-draw variant, no blend), so no light is counted twice.
- **Interiors.** A lightmapped interior wall never reads the probe volume's outdoor light, so the inside/outside leak
  of a thin wall goes away for static surfaces: the evidence's sealed box holds 0.000 % of the lit ground inside (the flat
  ambient leaks 16.5 %). INTERIOR bricks (ADR-281) still light the dynamic objects inside.
- **Policy** (ADR-207 `baked_gi::policy`) is unchanged: DDGI and SSGI stay off while baked light lights the frame.
- **Screen-space AO** (ADR-2635). The bake holds a receiver's occlusion, so the frame's SSAO does not multiply the
  lightmap's diffuse term (the full forward shader, the visibility-buffer resolve and the lite shader alike); the
  material's occlusion texture still does, and the receiver's specular keeps the SSAO.
- **Additive levels** (ADR-2635). The renderer binds one lightmap; an additive level's lightmap joins the primary's
  as rows stacked below it in one atlas (`combine_lightmaps`), receivers resolved by key across all of them.
- **Capsule occluders** (ADR-282) darken the volume's ambient cube in the screen pass, which lightmapped draws do not
  read. Built (ADR-2656): while capsules are set the screen pass writes one more word per lookup texel, the
  cosine-weighted occluder visibility at the depth normal (the CPU's `capsule_visibility`, without the SSAO share since a
  receiver's diffuse term takes no SSAO), and a receiver multiplies its baked diffuse term by it, so a parked vehicle
  darkens a lightmapped floor too. A frame with no capsule neither writes nor reads the word. The pass runs only while a
  volume lights the frame: a `.flmap` with no `.fgiv` has no screen pass, so capsules do nothing there.
- **Debug views** (built, ADR-2656, `regia_render::lightmap_debug`): lightmap density (a checker per texel, color by `k`,
  where `k` is the entry's texels per metre over the bake's density), lightmap only, invalid texels. They swap each mesh
  for a debug twin, as the surface debug views do, so no shader changes.

## 9. Agent protocol

| Surface | What it does |
|---|---|
| `regia bake-lightmaps` | Bake, `--dry-run` layout, `--inspect` a file, `--compare` with a reference (PSNR); one JSON object on stdout |
| `render.get_lightmaps` | Installed file, switches (`specular` included), GPU bytes, texel encoding, denoise record, the last frame's `forward` counters (`drawn`, `instanced`, `visbuffer`, `lite`, `tier`, `fallback`, `unsupported`), receivers, the open level's lightmap |
| `render.set_lightmaps` | Enable, intensity, bicubic, directional, specular (ADR-4762), load or clear a file, set receivers, `auto_receivers` by entity key |
| `render.get_gi_mode`, `level.get_baked_gi` | The `lightmaps` object: installed, pages, GPU bytes, receivers, `stale_instances`, `unverified_instances` (ADR-2656, § 6.1) |
| `render.set_debug_view` | `lightmap_density`, `lightmap_only`, `lightmap_invalid` (ADR-2656, § 8) |
| `regia cook --lightmap-tiers` | Per-tier lightmap bytes and budget verdicts (ADR-3120, § 6.2) |
| `render.lightmap_encode_report` | Measure BC6H or ASTC HDR on a copy of the installed map before cooking it |
| `editor.lightmap_bake`, `editor.lightmap_status`, `editor.lightmap_cancel`, `editor.lightmap_preview` | The Lightmaps panel |

Every error and warning code (`lightmap_*`, `bake_*`) is listed on `book/agents/lightmaps.md`.

## 10. Evidence

`docs/evidence/lightmaps-v030/SUMMARY.md` (ADR-2315, lavapipe, the gate's reference), colour room with a sphere, every
mesh a receiver, 16 texels/m, 256 samples, against the path tracer at 1024 spp:

| | Indirect FLIP mean (≤ 0.08 ★) | FLIP p95 (≤ 0.20 ★) | Energy | PSNR dB |
|---|---|---|---|---|
| Flat ambient (no lightmap) | 0.693 | 0.917 | 0.383 | 14.4 |
| Lightmap, CPU bake | 0.120 | 0.186 | 0.980 | 32.3 |
| Lightmap, GPU bake | 0.120 | 0.186 | 0.980 | 32.3 |
| Lightmap, Mobile Low lite shader (after ADR-2403) | 0.157 | 0.280 | 1.036 | 29.3 |

With ADR-2404 the CPU bake is at FLIP 0.115 (0.109 with the frame's SSAO off, the ADR-2635 lane), and the 0.35 m sphere's
seam `p95` drops from 0.58 to 0.013 at 16 texels/m. The FLIP mean target is a ★ first guess in `specs/27` § 2.2, never
calibrated; p95, energy and the leak bound are met.

Closure evidence, 2026-11-09 (ADR-4930): the original room mean/p95 limits are now
asserted by `lightmap_pt_evidence`, with the original full-direct subtraction and mask.
The corrected Mobile Low row is 0.07711886 / 0.10851907 on a locked RTX 5090;
no target was recalibrated. Derived LOW and ULTRA_LOW failed their seam and dark-point
criteria on that date; closure E (2026-11-10, ADR-5125) brings both under the seam budget with no dark point in the
bake's window (§ 3, § 11). The corrected Low golden also needs owner review
(SSIM 0.9458 against its unchanged 0.9800 bar). Measurements and isolated controls:
`docs/evidence/lightmaps-v030/closure-c-2026-11-09.md`.

Tier agreement, 2026-11-11 (ADR-5945): on the quad-box golden view the Mobile Low lite frame and the full shader's
frame are held to each other without a reference image (`the_lite_low_frame_agrees_with_the_full_frame`: flat 8 x 8
windows within 0.5 code values of mean luma, and SSIM 0.98 between the two; measured 0.11 and 0.9855). The blessed Low
image is the frame of the lite shader before ADR-4930 exactly (0 of 12,288 pixels differ from that control), which is
all its 0.9458 measures; against the full shader at Mobile Low's own shadow filter and lightmap tier today's frame
scores 0.9995. On receivers with an authored material the lite shader now adds the multi-scatter share of the baked
reflection and takes the multi-bounce form of the occlusion map on the baked diffuse, as the full shader does
(`the_lite_tiers_draw_the_full_shaders_baked_light_on_every_material`: nine materials at Mobile Low and High within
1.5 code values, measured 0.86 at most; metals were 6.4 to 11.2 darker and an occluded grey floor 4.1).
Mobile Low keeps one bilinear fetch and no directions; a smooth metal receiver there reads the baked light as uniform
(3.8 code values off the full shader at most on the roughness 0.4 alloy).

Reference images: `lightmap_quad_box` (the baked floor atlas, tone-mapped), `lightmap_forward_quad_box` (the floor drawn as
a receiver next to its red box, sun live) and `lightmap_forward_quad_box_lite_low` (the same frame on the Mobile Low lite
shader). All three tests fail without their image (ADR-3211); the owner approves each image before it is blessed.

## 11. Open

| Item | State |
|---|---|
| Owner look sign-off of v0.3.0; the interior plate review | Open (owner). DO27 content cannot serve as the fixture; v0.3.0 uses a Regia-built interior gallery (ADR-3001), merged: `crates/regia_cli/tests/lightmap_gallery` and `docs/evidence/lightmaps-v030/` (plates, ADR-4108 mottling and seam fixes) |
| Original color-room FLIP mean 0.08 / p95 0.20 | Met by the current full CPU/GPU and Mobile Low rows (ADR-4930); the corrected Low golden remains open. No target was recalibrated. |
| Unwrap into the cooked mesh | Not built, not needed for v0.3.0. The automatic unwrap is made at bake time, per placed instance from its world-space density, and reaches the package inside the cooked `.flmap` (§ 2, § 6.2; why ADR-2107 (12) kept clusters out of the cook). A cook step that writes UV2 into Regia's own cooked mesh (never into source glTF files), so instances of one mesh share an unwrap, needs a per-mesh reference density with a per-instance scale, and would need a new `.flmap` version (versions 6 and 7 went to source mesh hashes and atlas pages, ADR-2656, ADR-3300) |
| All tiers from one bake | Cook implemented (ADR-3120, § 6.2). Since ADR-5125 (chart repack at the tier's resolution, ULTRA_LOW padding 2 for its block encoding, the steep-border gutter fit, the dark check in the bake's window): gallery seam p95 LOW 0.0634, ULTRA_LOW 0.147 (budget 0.25); no dark point on the bake, LOW or ULTRA_LOW, and a planted hole is still found on both (`docs/evidence/lightmaps-v030/closure-e-2026-11-10.md`). The derived tiers' dark-point window changed to the bake's in world units (ADR-5125, accepted by the lead 2026-11-10); no numeric bar moved, no waiver. |
| Authored `TEXCOORD_1` in the CLI | Not built (Rust API only) |
| Android memory and lite-path cost on phones | Merged: `samples/lightmap_device`, `forward.uploaded_bytes` / `native_blocks` / `uploaded_directions`, `gpu_bytes` as an estimate (ADR-2856), device shader bisects (ADR-3145), tier packages on the phone (ADR-5382); measured rows in `docs/perf-log.md`, recipe in `book/agents/lightmaps.md` "Measuring lightmaps on a phone". Low-end/Adreno devices left the build machines (2026-11-06): open |
| Bake time at scale | Merged (ADR-2838, ADR-2955, ADR-3121): parallel layout, a linear `load`, `density_scaled`; numbers and limits in `book/agents/lightmaps.md` "Bake time at scale". The largest measured level is a generated 1 km² prop field, not a production map |
| ASTC HDR sampled on a phone | Native blocks, live Low/High profile switches and suspend/resume pass on TEST Pixel 9 Pro 54101FDAP0015H (ADR-4930); low-end/Adreno coverage remains unverified. |
| Lightmap specular | Built (ADR-4762, § 7.1): receivers read their indirect specular from the baked radiance field on every preset; owner look approval 2026-11-09. Specular of `--direct-sun` maps stays on the probes |

## 12. Risks

- **Vertex cost.** A receiver's mesh adds UV2 and the seam splits (the cube: 8 to 24 vertices), and every instance holds
  its own unwrap (and clusters with `--meshlets`) for its life in memory.
- **One direction per texel.** Light from two opposite sides averages toward no direction, so a normal map then changes
  little.
- **Bake time.** Texel count × samples; the GPU baker is about ten times the CPU on a 5090, and the largest measured scene
  is small (§ 11).
- **Mobile texture limits.** An atlas larger than the device's 2D texture limit leaves every receiver on the probe light;
  the tier's bake density and `--max-atlas` (§ 3) are what keep a phone package under it.

## 11. Streamed worlds: per-cell bakes (ADR-3318)

A streamed level (spec 78) bakes one `.flmap` per cell with `regia bake-lightmaps --scene <level> --cells`:
`<primary stem>.cells/<x>_<z>.flmap`, each cell baked against its neighbor ring (`--cell-ring`, default 1) and the
always-loaded set as occluders that receive no texels, a seam pass between edge-adjacent cells' files, and
`cells.json` to skip unchanged cells. At run time a loaded cell's file stacks under the level's atlas in a fixed slot
(`regia_framework::lightmap_level::stack_cell_lightmaps`) and leaves when the cell unloads; the other cells' texels and
coordinates do not change. The stack is one atlas of at most 8192 rows until multi-page atlases land; then a cell takes a
page. The `.flmap` format does not change. User guide: `book/agents/lightmaps.md` "Per-cell bakes of a streamed world".
