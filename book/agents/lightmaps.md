# Lightmaps

A lightmap stores baked irradiance per texel for the static meshes of a map. `regia bake-lightmaps` lays out a
second UV set (UV2) for every mesh, bakes on the CPU or the GPU (`--gpu`), denoises, and writes a `.flmap`. At runtime the
lightmap is installed and switched on by the level that names it (`look.lightmap`) or through `render.set_lightmaps`;
without either the flag is off. Format reference: `docs/formats/lightmap.md`. What shipped for v0.3.0 (including the
automatic unwrap the cook carries and the bake density per package tier), and what is still open: `docs/specs/lightmaps.md`. Design: ADR-860, ADR-1556 (directions, bicubic
filtering, noise check, cook), ADR-1623 (the forward pass), ADR-2024 (the UV atlas, baking from a level, chart seams,
entity keys, the Lightmaps panel, the GPU baker), ADR-2107 (every path and tier), ADR-2125 (GPU tiles), ADR-2313 (levels),
ADR-2314 (denoise, block encodings), ADR-2404 (creases, stitching, automatic density).

**Reference images.** `cargo test -p regia_pt --test lightmap_quad_box` (the baked floor atlas) and `--test
lightmap_forward_quad_box` (the floor drawn as a receiver beside the red box, on the full shader and on the Mobile Low
lite shader) compare with `crates/regia_test_utils/goldens/lightmap_quad_box.png`,
`lightmap_forward_quad_box.png` and `lightmap_forward_quad_box_lite_low.png`. A missing image fails the test (ADR-3211).

**Closure status (ADR-4930).** The Mobile Low room passes the original numerical quality bars after the direct/indirect
energy correction. Derived LOW and ULTRA_LOW still miss their original seam and dark-point checks, and the corrected
Mobile Low reference frame needs owner review. No golden or threshold was changed; see
`docs/evidence/lightmaps-v030/closure-c-2026-11-09.md` for the controls and retained failures.

**The Mobile tiers against the full shader (ADR-5945).** Two tests in `lightmap_forward_quad_box` need no reference
image: `the_lite_low_frame_agrees_with_the_full_frame` draws the golden view with both shaders and holds every flat
8 x 8 window to the same mean brightness (0.5 code values) and the pair to the golden bar, and
`the_lite_tiers_draw_the_full_shaders_baked_light_on_every_material` does the same on nine receiver materials at
Mobile Low and Mobile High, metals and occlusion maps among them (1.5 code values; Mobile Low is not held on a smooth
metal, where its missing directions show). The manual study `lite_low_golden_study` (ignored; set
`REGIA_LIGHTMAP_LITE_STUDY=<dir>` for its table and plates) attributes the reference image's SSIM 0.9458: the blessed
`lightmap_forward_quad_box_lite_low.png` is the frame of the lite shader before ADR-4930, pixel for pixel, and today's
frame matches the full shader at Mobile Low's own shadow and lightmap settings in all but 48 pixels. The image still
waits for the owner's re-bless.

**Since ADR-1556 (format version 2).** The bake also stores the direction the baked light comes from per texel, so
normal maps bend it at runtime; the lookup is bicubic by default; the bake reports its noise before any filter and
can sample adaptively up to a hard cap; `.flmap` files under `assets/` cook into the pak. Versions 1 through 6 still
read; `regia upgrade-project --write` rewrites an older file at format version 7 (`LIGHTMAP_FORMAT_VERSION`).

**Since ADR-1623 the forward pass shades with it.** A *receiver* is a static mesh entity that names its entry in the
installed lightmap. It is drawn from that entry's unwrap and takes its diffuse indirect light (sky, bounce and, with
directions, the way normal maps bend it) from the lightmap instead of the probes or the flat ambient, while the flag
is on. Everything else (dynamic objects, unlisted meshes, the flag off) keeps the probe light, and a frame without
an enabled lightmap is unchanged. `render.get_lightmaps` reports `forward_pass: true` and what the last frame did.

**Since ADR-2024 (format version 3).** `regia bake-lightmaps --scene <level>` bakes a level directly and keys its
entries by entity; the UV atlas generator lays out UV2 by default; every bake measures its chart seams; and a lightmap
baked with `--direct-sun` replaces the live sun's diffuse on its receivers instead of adding to it. Versions 1 through 6 still
read; `regia upgrade-project --write` rewrites an older file at format version 7 (`LIGHTMAP_FORMAT_VERSION`).

## Shading a level with its lightmap

**Since ADR-2313 (scene format v11) the level carries it.** Bake with `--write-level` and the level names its lightmap
in `look.lightmap`; opening the level does the rest:

1. `regia bake-lightmaps --scene assets/levels/yard.level.ron --write-level` writes `assets/maps/yard.flmap` and
   rewrites the level with `look.lightmap: Some((file: "maps/yard.flmap"))` (the summary's `level_written` is `true`,
   `false` when it already named it; a `.level.d` directory or an output outside `assets/` is `lightmap_write_level`).
2. Open the level (`regia run --level`, `level.open`, the editor). The framework's `LightmapLevelPlugin` asks the renderer
   to warm the lightmap variants (`FrameRenderer::set_warm_lightmaps(true)`) as soon as the level is open (a renderer
   the host inserts later is asked on the next frame), installs the
   file (cooked from the pak or the dev cache first, else `assets/<file>`), turns lightmaps on (`enabled: Some(false)`
   loads it and keeps the probe light; `intensity` overrides the renderer's), and gives a `LightmapReceiver` to every
   drawn entity an entry was baked for, as meshes resolve: the level entity's own `MeshRenderer` is part 0, and a part
   entity with `MeshPartOf { owner, part }` (the editor's mesh resolver adds it; a host that splits models adds it
   too) is part `part`, so every part of a multi-part mesh receives. An entity that already has a receiver keeps it.
3. Check it:

   ```json
   {"jsonrpc": "2.0", "id": 1, "method": "render.get_lightmaps"}
   ```

   `level` is `{ file: "maps/yard.flmap", installed: true, error: null, error_code: null, entries: 4, keyed_entries: 4,
   attached: 4, unmatched: [], warm: true }`; `levels` lists the receivers per level:
   `[{ level: "<level guid>", current: true, lightmap: "maps/yard.flmap", receivers: 4, applied: 4, entities: [{ entity,
   guid, part, entry, applied, by_level }] }]`.

`LightmapReceiver` is reflected and saved with the level, so a level saved after its receivers were set (by the agent's
`receivers`, a host or the open itself) reopens with them. Closing the level removes the lightmap it installed, the
receivers it attached and the warm request, and puts the host's settings back. A level that names no lightmap changes
nothing. A file that is missing reports `level.error_code: "lightmap_not_found"` (the warm request stands, and the file
appearing later installs it); a file the reader refuses reports the reader's code.

The manual way, for a level that names no lightmap:

1. `regia bake-lightmaps --scene assets/levels/yard.level.ron` (or `editor.lightmap_bake`, below). It writes
   `assets/maps/yard.flmap` with one entry per static mesh part, keyed by the entity's guid.
2. `render.set_lightmaps {path: "maps/yard.flmap", enabled: true, auto_receivers: true}`. The result has `receivers_by_key`
   (entities attached) and `unmatched_keys` (entries nothing in the level matched: the entity is gone, or the entry is
   part 1 or later of a mesh source, which a `MeshRenderer` does not draw).
3. `render.get_lightmaps`: `receivers.applied` equals `receivers.receivers`, `receivers.errors` is empty. A host calls
   `regia_render::lightmap::receivers_by_key(world, &map)` and inserts a `LightmapReceiver` for each.

## Shading a map with its lightmap

1. Bake the map (`regia bake-lightmaps <map>.bake.ron`). Entries are named `instance_<k>`, `k` being the instance's
   index in the bake description.
2. In the game, give each baked static mesh entity a `LightmapReceiver { entry: "instance_<k>".into() }`
   (`regia_render::lightmap`). The entity keeps its `MeshRenderer` mesh: the engine swaps in the entry's receiver
   mesh (the same triangles, with the extra vertices a chart seam needs) and its UV2 stream, and puts the entity's
   own mesh back if the component, the entry or the lightmap goes away.
3. Install the lightmap and switch it on: `render.set_lightmaps {path: "maps/<map>.flmap", enabled: true}` (or
   `InstalledLightmap::load` and `LightmapSettings { enabled: true, .. }` from the host). An agent can attach the
   receivers in the same call: `receivers: [{entity: 4294967301, entry: "instance_0"}, ..]` (ids from
   `scene.query_entities`).
4. `render.get_lightmaps`: `receivers.applied` equals `receivers.receivers`, `receivers.errors` is empty, and
   `forward.drawn` counts the receivers in view.

What a receiver may be: a standard opaque or masked material (no material graph, not blended or transmissive),
rigid (no skin, no morph targets), drawn whole (no LOD chain or LOD cross-fade) and not a mask-decal instance. A
receiver outside that keeps the probe light and is counted in `forward.unsupported`, and so is every receiver when
the atlas is larger than the device's 2D texture limit (some mobile devices: bake with a smaller `--max-atlas` for
that tier).

**Since ADR-2107 a receiver draws on every path and tier.**

| The frame draws | A receiver draws | `forward` counts it in |
|---|---|---|
| GPU-driven batches | In the batches: one instanced draw per mesh and material, whatever entry each instance reads | `instanced` |
| The visibility buffer | As a cluster instance (the bake needs `--meshlets`): the resolve fetches its UV2 | `visbuffer` |
| The lite forward shader (Mobile tiers on a tile GPU) | From the CPU color loop with the lite shader: one bilinear fetch at Low, a second for the direction at High | `lite` |
| Anything else | From the CPU color loop with the full shader | none of them (only `drawn`) |

`drawn` is their total. A non-receiver instance sharing a receiver's mesh and material may share its batch; it keeps the
probe light. In a visibility frame with a receiver, the resolve draws all its pixels with its `LIGHTMAP` variant.

A receiver takes the visibility path only with clusters built over its unwrap, because a chart seam splits vertices and a
coarse level of the source mesh's clusters is free to cross a seam. Bake with `--meshlets`: the clusters go in the `.flmap`,
survive `regia cook`, and `apply_lightmap_receivers` hands each receiver's to the renderer while the profile has
`visibility_buffer` on (`receivers.clusters`; `forward.visbuffer` counts what the frame drew there). A receiver whose file has
none, or whose material the visibility path cannot shade, draws on the forward paths with the same light (`receivers.no_clusters`). A cooked `.fmlt` on the same entity (`MeshletSource`) is left unused
while it is a receiver, and used again when it gets its own mesh back. A host that drives the renderer itself calls
`regia_render::lightmap::receiver_meshlets(&map, entry, &receiver.vertices)` and `insert_meshlets` under the receiver mesh's
key, or `set_meshlet_builder`. Clusters that do not fit the mesh drawn (other counts or another bounding sphere: the mesh changed
since the bake) are refused the same way as none, with the warning code `lightmap_meshlets`.

**What each render profile allows.** `RenderProfile::lightmap_tier` caps `bicubic` and `directional`, and never turns on what
they turn off:

| Preset | `tier` | Lookup |
|---|---|---|
| Mobile Low | `bilinear` | One bilinear fetch. The direction section is not uploaded, which halves the GPU bytes |
| Mobile High | `directional` | One bilinear fetch for the light, one for the direction |
| Deck, every Desktop preset, Cinematic | `full` | Bicubic (four fetches) and the direction, as the settings ask |

`forward.tier` is what the last frame allowed. The lite shader lowers it too: a frame forced to the Low lite shader reads
`bilinear` whatever its preset. `render.get_forward_lite` says which shader the frame draws.

**Warming.** A frame warm (`FrameRenderer::warm_scene`, `warm_extracted`, and so `GameFrameWarm`) with a lightmap installed
builds the `LIGHTMAP` variants of every target it drew, on every path this device can draw receivers through, uploads the
lightmap and reserves its tables. Frames after it create no pipeline for a receiver. A lightmap installed after the warm has
started: call `FrameRenderer::set_warm_lightmaps(true)` at level load. Without a warm, a receiver mid-play keeps the probe
light while its variant builds in the background, and counts in `forward.fallback`. A project with no lightmap warms and
draws what it did before.

A host that drives the renderer itself calls `FrameRenderer::set_lightmap(Some(map), settings)` and
`set_lightmap_receivers(&[(object_key, entry)])`, and draws each receiver from
`regia_render::lightmap::receiver_mesh(&map, entry, &vertices, &indices)` (its `vertices`, `indices`, and `uv2` as
the mesh's second UV set).

By default the lightmap replaces the indirect diffuse term and, since ADR-4762, the indirect specular term of its receivers
(below); the sun stays live. A lightmap with `sun_direct` in
its `contents` (a `--direct-sun` bake) holds the sun's diffuse light too, so while it is enabled its receivers also drop
the live sun's diffuse term and keep only its specular, which a lightmap cannot hold. Every other surface keeps the live
sun whole, so dynamic objects stay lit and shadowed by it.

### Indirect specular on receivers

A rough floor seen at a grazing angle reflects the lit walls around it: in the v0.3.0 gallery the path tracer's floor indirect
light is about 12 % specular. A lightmap holds irradiance, not reflections, so before ADR-4762 a receiver in a level with no
image light, probes or reflection captures had no indirect specular at all and the floor read about 9 % darker than the
tracer. Now a receiver takes its indirect specular from the same baked light (`LightmapSettings::specular`, on by default):

- The directional rebalance (`E(n) = E · (1 + n · d) / max(1 + w, 1)`) is the irradiance of one radiance field that is linear
  in the direction. The specular reads that field in the GGX lobe: for linear radiance the lobe's integral is the radiance
  at the lobe's mean vector, the same lobe the probe specular reads (ADR-1103). A smooth surface sees the field toward its
  mirror direction, a rough one nearer its mean. Times the split-sum scale and bias, as every image light. Without
  directions (a `--no-directional` bake, or the Mobile Low tier, which drops them) the field is uniform: `E / π` in every
  lobe. The baked diffuse is multiplied by `1 − clamp(f0 A + B, 0, 1)`, reserving the specular
  share of the incident energy. The full shader and the lite shader (ADR-5945) also reserve the
  material's optional multi-scatter compensation from the diffuse budget and light that broad term
  from the unpartitioned bake, even without image lighting. The base GGX/diffuse lobes conserve a
  uniform white field; clearcoat and sheen retain their existing layered-material policies.
- It replaces the sky, probe and interior specular on receivers, as the lightmap already replaces their diffuse term. A local
  reflection capture (ADR-2860) keeps its share, and so does a transmissive pane's traced hit: both see the room's own
  radiance, sharper. The screen-space reflection replaces it like any environment specular. The lightmap's share takes the
  material's occlusion but not the screen-space AO or the sky-visibility estimate: the bake already darkened what the walls
  and creases hide (the diffuse term's rule, ADR-2635).
- Every preset has it: the full shader (Deck and Desktop), the visibility resolve and the lite shader (Mobile High and Low).
  The lite shader uses a view-independent lobe; Mobile Low reads it uniform. The lite shader uses the existing DFG lookup
  for this energy split and the full direct GGX/Smith/Schlick equations, including the material's optional direct
  energy compensation (ADR-4930). It shares one DFG fetch per fragment across lights and the receiver term;
  Low still reads the lightmap once and uploads no directions. Since ADR-5945 the receiver term also keeps the
  material's multi-scatter share: before it, a receiver with an authored metal material (multi-scatter is on by
  default in a `.material.ron`) drew as little as half of the full shader's baked light on the Mobile tiers. And
  its baked diffuse takes the multi-bounce form of the material's occlusion map, as the full shader's does; the
  baked reflection keeps the occlusion as it is.
- Bakes made with `--direct-sun` keep the previous environment reflection: their combined atlas cannot separate
  direct from indirect irradiance, and reconstructing from it would count the live sun's specular twice.
- `render.set_lightmaps { specular: false }` turns it off for an A/B: receivers then keep the specular of the other sources,
  which is none in a level that has none.

## Commands

```text
regia bake-lightmaps <scene> [--out FILE] [--unwrap atlas|charts] [--density T] [--padding N] [--max-atlas N]
                               [--chart-angle DEG] [--samples N] [--bounces N] [--seed N] [--direct-sun] [--no-filter]
                               [--threads N] [--min-area M2] [--instance K]... [--dry-run] [--seam-budget F] [--meshlets]
                               [--no-directional] [--max-samples N --noise-target F] [--gpu]
                               [--crease on|off] [--stitch on|off] [--auto-density on|off]
regia bake-lightmaps --scene <level> [--project DIR] [--sun ELEV,AZIM,LUX] [the same options]
regia bake-lightmaps <file.flmap> --inspect
```

- `<scene>` is a built-in (`cornell`, `furnace`, `rolling-hills`, `quad-box`) or a GI bake description (`.bake.ron`,
  the file `regia bake-gi` reads; see [Baking GI from meshes](baked-gi-bake.md)). `quad-box` is the baker's
  reference scene: a 4 m grey floor (instance 0) with a closed 1 m red box on it (instance 1), a white sky and a low
  sun. The lightmap uses its meshes, materials
  (base color and emission), sun, sky and lights. The terrain heightfield is not lightmapped.
- `--scene <level>` bakes a level directly. Its `GiMobility::Static` mesh entities, its sun and its sky are what
  `regia bake-gi --from-scene` exports; nothing but the `.flmap` is written. `--project` names the project root (default:
  the nearest ancestor with a project manifest), `--sun` overrides the level's sun, and the default output is
  `<project>/assets/maps/<level>.flmap`. Each mesh part gets an entry `entity_<guid>_<part>` keyed by entity. The file's
  bytes do not depend on the order of the level's entities or on `--threads`.
- `LightmapSurface { density_scale, receive }` on a level's entity: `density_scale` multiplies the bake's density for it
  (above 1 for hero pieces), `receive: false` leaves it to the probes while it still shadows and bounces (the JSON lists
  it under `skipped_opted_out`). A scale that is not a number above 0 is `lightmap_scene`, with the entity in `path`.
- Every other placed mesh that is neither emissive nor unlit gets an entry named `instance_<k>`, where `k` is its index in
  the scene. Emissive meshes are lights.
- `--unwrap atlas` (the default) lays UV2 out with the UV atlas generator ([Lightmap UV atlas](atlas.md)): low-stretch
  charts on curved meshes, packed by footprint so no space goes to a chart's empty corners. `--unwrap charts` is the
  planar generator (`--chart-angle` applies only to it). Both follow the density rules.
- `--seam-budget F` fails with `lightmap_seam_budget` when an entry's seam `p95` passes `F`, after writing the file so
  you can look at it. Without it the seams are only reported.
- `--gpu` bakes on the GPU: the same light up to float rounding, the same file format. It needs a CLI built with
  `--features lightmap-gpu`; otherwise it is `lightmap_gpu_unavailable`. A scene too large for the device is
  `lightmap_gpu_limit`: bake on the CPU. `settings.backend` says which ran.
- The GPU bake runs in tiles, one dispatch each, with at most one more queued behind it (ADR-2125). `--gpu-tile-rays N` caps a tile at
  about `N` rays (default 4194304, range 4096..=67108864). Smaller tiles hand the GPU back sooner, so an editor drawing on
  the same GPU stays responsive. `--gpu-yield-ms N` (at most 1000) waits for every tile and then leaves the GPU idle for `N` ms. There is then no
  tile queued. The
  Lightmaps panel passes `--gpu-tile-rays 1048576 --gpu-yield-ms 2`. `--gpu-memory-mib F` caps the GPU memory the bake
  holds. A larger atlas then bakes in chunks of texels that fit next to the scene. A scene that alone passes the cap is
  `lightmap_gpu_limit`. Tiles, chunks and the pause never change the file: every texel adds its samples in the same order
  (tested byte for byte).
- The tile schedule is a pure function of the scene, the settings and these options, so the same bake runs the same tiles
  on every run. The JSON's `gpu` object reports it: `adapter`, `tiles`, `chunks`, `chunk_texels`, `tile_texels`, `tile_samples`, `tile_rays`,
  `scene_bytes`, `peak_bytes`, `memory_budget_bytes`, `readbacks`, `yield_ms`, `schedule_hash` (16 hex digits, the same for the
  same tiles in the same order) and `fallback`.
- A GPU device lost mid-bake (a driver reset) ends the bake with `lightmap_gpu_device_lost`, and nothing is written.
  `--gpu-fallback` bakes on the CPU instead whenever the GPU cannot: `lightmap_gpu_unavailable`, `lightmap_gpu_device`,
  `lightmap_gpu_device_lost`, `lightmap_gpu_limit` or `lightmap_gpu`. `settings.backend` is then `cpu`, and
  `gpu.fallback` holds the GPU's `code` and `message`. Settings errors never fall back.
- `--compare REF.flmap` compares the new map with a reference of the same layout after the bake, for example a CPU bake of
  the same scene and settings. The `compare` object holds `texels` (baked in both), `coverage_mismatch`, `identical`,
  `psnr_db` (null when identical), `mse`, `peak`, `mean_rel`, `p99_rel`, `max_rel`, `psnr_floor_db` and `within_floor`.
  PSNR is `10 log10(peak² / MSE)` over the RGB irradiance of the texels both maps baked, with `peak` the reference's
  brightest channel. The GPU baker's documented tolerance against the CPU baker is `psnr_floor_db`, 40 dB
  (`regia_pt::lightmap::LIGHTMAP_GPU_PSNR_FLOOR_DB`). `crates/regia_pt/tests/lightmap_gpu.rs` holds every kind of light to
  it on lavapipe.
- `--instance K` bakes only instance `K` (repeat the flag for more). The rest of the scene still lights and shadows
  it. An instance that does not exist, or is emissive, unlit or a light, is refused with `lightmap_instance`.
- `--min-area` skips meshes with less surface than that many m². Small props (rock hints, clutter) then keep the
  probe lighting, and the atlas spends its texels on walls and set pieces. The JSON lists them under
  `skipped_small`.
- `--dry-run` lays out the atlas and prints it without baking. It is fast; use it to choose `--density`. With `--meshlets` it
  builds the clusters too, which costs seconds per mesh.
- `--inspect` reads an existing `.flmap` and prints its summary.
- `--meshlets` also builds visibility-buffer clusters over each entry's unwrap and stores them in the file, so a receiver can
  draw through the visibility buffer in a game with no meshlet builder. Off by default: it costs seconds per mesh and file size.
  The output's `meshlets` is `{ entries, clusters, levels, bytes, refused: [{ entry, code, message }] }` (`null` without the flag),
  in a dry run too. An entry the builder refuses, or whose clusters would cross a chart seam, keeps its bake and draws in the
  batches.
- Without them the command bakes on all cores (`--threads 0`) and writes `--out` (default `<scene>.flmap`).
- `--samples` is 1..=65536 and `--bounces` at most 64. Out of range is `lightmap_bake_settings` with `path` naming
  the setting, before anything is read.
- `--noise-target F` turns on adaptive sampling: a texel takes `--samples`, then more `--samples` at a time, until
  the relative standard error of its luminance is at most `F` (0.02 = 2 %) or it reaches `--max-samples` (at most
  65536). Sample `k` of a texel always uses the same random numbers, so the result is still the same for any
  `--threads`. Keep `--samples` at 16 or more: a texel whose first few samples agree by chance stops early.
- `--no-directional` leaves out the direction section (half the file; normal maps then do not change the baked
  light).
- `--direct-sun` also bakes the sun seen directly. Leave it off when the runtime sun and its shadows stay live (the
  default); turn it on for fully static lighting (low-end or mobile). A receiver of such a lightmap keeps only the live
  sun's specular.

Each prints one JSON object on stdout. Progress goes to stderr.

## Output

Bake:

```text
{
  "ok": true, "out": "bore.flmap", "bytes": 4194512, "scene": "bore.bake.ron", "density_scaled": null,
  "atlas": { "width": 1024, "height": 1024, "charts": 412, "fit_scale": 1.0, "attempts": 1,
             "chart_fill": 0.61, "rect_fill": 0.83, "degenerate_triangles": 0, "generator": "atlas" },
  "report": { "entries": 21, "texels_baked": 640000, "texels_inside": 312, "texels_dilated": 90000,
              "rays": 512000000, "triangles": 260000, "emissive_triangles": 12, "lights": 0,
              "contents": ["sky", "sun_bounce", "emissive"], "directional": true,
              "noise": { "mean": 0.031, "p95": 0.074, "max": 0.21, "mean_samples": 256.0,
                         "texels_over_target": 0 } },
  "seams": { "edges": 180, "hard_edges": 96, "p95": 0.08, "max": 0.2,
             "entries": [ { "name": "instance_0", "edges": 12, "mean": 0.02, "p95": 0.05, "max": 0.09, "worst_chart": 3,
                            "hard_edges": 8, "charts": [ { "chart": 3, "edges": 4, "mean": 0.04, "max": 0.09 }, .. ] }, .. ],
             "overlay": [ { "entry": 0, "a": [0.11, 0.2], "b": [0.11, 0.24], "world_a": [1.0, 0.0, 3.0],
                            "world_b": [1.0, 0.0, 4.0], "error": 0.2 }, .. ] },
  "coverage": { "empty": 310000, "baked": 640000, "dilated": 98000 },
  "utilization": 0.70, "exposure": 256.0,
  "entries": [ { "name": "instance_0", "instance": 0, "rect": [0, 0, 220, 96], "charts": 14, "triangles": 380,
                 "vertices": 412, "area_m2": 310.5, "texels_per_metre": 8.0 }, .. ],
  "skipped_small": [],
  "settings": { "density": 8.0, "padding": 2, "max_atlas": 4096, "chart_angle": 60.0, "min_area": 0.0,
                "instances": [], "samples": 256, "bounces": 3, "seed": 1, "direct_sun": false, "filter": true,
                "directional": true, "max_samples": 0, "noise_target": 0.0 },
  "timings": { "load": 0.4, "layout": 1.9, "bake": 38.1, "seams": 0.3, "encode_write": 0.5 },
  "seconds": 41.2
}
```

- `seams` is the chart-seam metric (`docs/formats/lightmap.md` § 2a): the relative luminance step across the edges where
  a smooth surface is cut in UV2 (two charts meeting, or one chart meeting itself along a slit), per entry and per chart; `hard_edges` are real creases, left out.
  Every pair of faces on a welded edge counts (ADR-4108): where closed blocks meet in one plane, four faces share the
  edge, and the two coplanar ones are the seam (before ADR-4108 such seams were neither measured nor stitched). `overlay` lists
  the worst edges, at most 1024, worst first, in atlas UV and in world space. The figures go in the file: `--inspect` and
  the shipping check read them. With `--seam-budget`, `seams.budget` and `seams.over_budget` are added.
- With `--scene`, `level` adds `project`, `input_hash`, `statics`, `skipped`, `skipped_by`, `skipped_entities`, `sun`,
  `sky` and `warnings`, and each entry adds `entity`, `part` and `label`.
- `atlas.fit_scale < 1` means the charts did not fit `--max-atlas` at the asked density. Every entry's
  `texels_per_metre` shows what it got. The bake then also prints a `warning:` line on stderr and lists
  `lightmap_atlas_scaled` (with `fit_scale`) in `density.warnings`.
- `density_scaled` (ADR-3121) is the same fact as a headline: `null` when every entry got the density it asked for,
  else `{ factor, requested, density, max_atlas, remedy, message }`. `factor` is `atlas.fit_scale`, `density` the
  texels per meter an entry of scale 1 got, and `remedy` what to do: raise `--max-atlas` (up to 8192) or split the
  level into smaller levels; at `--max-atlas 8192` it says to split the level or lower `--density`. The dry run has it
  too, so check it before a long bake. `editor.lightmap_status` returns the bake's JSON as `summary` (so
  `summary.density_scaled`), and the Lightmaps panel's atlas line reads "density lowered to N %: raise atlas or split
  level".
- `timings` is the wall time of each phase in seconds: `load` (the level or scene), `layout` (UV2 and the atlas),
  `bake`, `seams` and `encode_write`; a dry run has only the first two. See [Bake time at scale](#bake-time-at-scale).
- `report.texels_inside` counts texels whose hemisphere rays mostly hit back faces: the surface runs inside other
  geometry (a floor under a wall). They are refilled from their neighbours. A large count on one entry usually means
  overlapping or single-sided geometry. A texel center on a chart's border traces from just inside its triangle, so light does not come in through the joint
  it touches.
- `report.noise` is the bake's noise before the filter: the relative standard error of each baked texel's mean
  luminance (`mean`, `p95`, `max`), the samples a texel took on average, and with `--noise-target` the texels that
  reached `--max-samples` still above it. It is `null` below 2 samples. This is the check that does not depend on
  the denoiser: bake with `--no-filter` and read `p95`.
- The dry run prints `atlas`, `entries`, `contents`, `triangles`, `skipped_small` and `settings`, with
  `"dry_run": true`.

`--inspect` prints `version`, `width`, `height`, `exposure`, `density`, `padding`, `directional`, `contents`, `bake` (`samples`,
`bounces`, `seed`, `sun_toward`, `sun_illuminance`, `filtered`), `coverage`, `utilization` and `entries` (with `entity`,
`part` and `seams`).

## Errors

A failure prints `{"ok": false, "code", "path", "message"}` and exits with status 1.

| Code | Meaning |
|---|---|
| `bake_scene_unknown` | `<scene>` is neither a built-in nor a `.ron` file |
| `bake_source_missing`, `bake_desc_parse`, `bake_desc_newer`, `bake_desc_invalid`, `bake_mesh_load`, `bake_manifest_mismatch` | The bake description, as for `regia bake-gi` ([Baking GI from meshes](baked-gi-bake.md)) |
| `lightmap_scene` | The scene cannot be flattened (an instance names a missing mesh or material, a bad index), or a level's `LightmapSurface.density_scale` is not a number above 0 (`path` names the entity) |
| `export_read_failed`, `export_level_parse`, `export_mesh_missing`, `export_no_input`, `export_terrain_ambiguous`, `export_path_invalid` | `--scene`: the level, as for `level.export_bake` ([Exporting a bake from a level](baked-gi-export.md)) |
| `lightmap_seam_budget` | An entry's chart seams pass `--seam-budget` (the file was written; `path` is the first entry), or in a shipping `regia package` `[lightmaps] seam_budget` |
| `lightmap_gpu_unavailable` | `--gpu` on a CLI built without the `lightmap-gpu` feature |
| `lightmap_gpu_device` | `--gpu`: no GPU to bake on |
| `lightmap_gpu_limit` | `--gpu`: the scene does not fit the device's storage buffers or the shader's traversal stack: bake on the CPU |
| `lightmap_gpu` | `--gpu`: the shader or a dispatch failed |
| `lightmap_gpu_device_lost` | `--gpu`: the GPU device was lost during the bake (a driver reset); nothing was written. Bake again, or with `--gpu-fallback` |
| `lightmap_gpu_cancelled` | Rust API only (`bake_lightmap_gpu_with`): the control callback cancelled the bake |
| `lightmap_gpu_options` | Rust API only: `LightmapGpuOptions` out of range (the CLI reports these as `lightmap_bake_settings`) |
| `lightmap_write_level` | `--write-level`: without `--scene`, with `--dry-run`, the level is a `.level.d` directory, or the output is not under the project's `assets/` (ADR-2313) |
| `lightmap_not_found` | `render.get_lightmaps` `level.error_code`: the file the open level's `look.lightmap` names is neither cooked nor under `assets/` (ADR-2313) |
| `lightmap_compare_size` | `--compare`: the reference's atlas size differs from the new map's (`path` is the reference) |
| `lightmap_compare_empty` | `--compare`: no texel is baked in both maps |
| `lightmap_bake_settings` | `--samples` (1..=65536), `--max-samples` (0 or `--samples`..=65536), `--bounces` (<= 64), `--noise-target` (>= 0), `--gpu-tile-rays` (0 or 4096..=67108864), `--gpu-memory-mib` (>= 0) or `--gpu-yield-ms` (<= 1000) is out of range (`path` names it) |
| `lightmap_uv_settings` | `--density`, `--padding` (1..=16), `--max-atlas` (16..=8192), `--chart-angle` (1..=80) or `--min-area` (>= 0) is out of range |
| `lightmap_uv_invalid_mesh` | A mesh has a bad index, a non-finite position or a duplicate name |
| `lightmap_uv_empty` | Nothing to unwrap (no receiving mesh, or a mesh with no triangles) |
| `lightmap_uv_atlas_full` | The charts do not fit `--max-atlas` even at a much lower density, or `--padding` leaves no room inside it |
| `lightmap_instance` | An `--instance` does not exist, or it is emissive, unlit, a light or has no triangles (`path` is `instance`) |
| `lightmap_bake` | The baker refused the layout (it does not match the scene) |
| `lightmap_io` | The file cannot be read or written |
| `lightmap_bad_magic` | Not a `.flmap` file |
| `lightmap_version` | The file is from a newer Regia Engine |
| `lightmap_header` | The header does not decode |
| `lightmap_payload` | Sections run past the end, the hash does not match (a corrupt or truncated file), or the direction section has an unknown encoding or the wrong size |
| `lightmap_layout` | Entries overlap, leave the atlas, or hold UV2 or indices out of range; for a receiver: its mesh is not the one the entry was baked for (re-bake) |
| `lightmap_entry` | A `LightmapReceiver` names an entry the installed lightmap does not have (`receivers.errors`) |
| `lightmap_receiver_lod` | A `LightmapReceiver` entity has a `MeshLodChain`; a receiver draws LOD 0 only (`receivers.errors`) |
| `lightmap_receiver_counted` | A `LightmapReceiver` entity draws a counted mesh (`MeshRenderer::counted`, no CPU vertices) whose entry's unwrap splits or reorders its vertices; load the mesh with `keep_cpu_data` (`receivers.errors`, ADR-7330) |
| `lightmap_receiver_counted_pending` | A `LightmapReceiver` entity draws a counted mesh that has no GPU upload yet (still loading, or the App has no `CountedMeshPlugin`); retried every frame (`receivers.errors`, ADR-7330) |
| `lightmap_meshlets` | `--meshlets`: the builder refused an entry (`meshlets.refused`; the entry keeps the bake and draws in the batches). At runtime it is the warning's code for a stored blob that does not decode or fit (`receivers.no_clusters`): re-bake with `--meshlets` |
| `lightmap_busy` | `editor.lightmap_bake`: a bake is running |
| `lightmap_no_source` | `editor.lightmap_bake`: no `source`, and no saved level is open |
| `lightmap_wait_needs_dry_run` | `editor.lightmap_bake`: `wait` without `dry_run` |
| `bake_no_summary` | `editor.lightmap_status`: the CLI exited with no summary (a panic, or arguments it refused); `message` holds its last log line |
| `bake_failed` | `editor.lightmap_status`: the CLI printed `ok: false` without a code |

## Agent methods

- `render.get_lightmaps` returns `{ enabled, intensity, installed, source, width, height, entries, gpu_bytes,
  has_directions, directional, bicubic, specular, contents, utilization, error, error_code, forward_pass, meshlet_entries, forward,
  receivers }`. `meshlet_entries` is the entries of the installed file with clusters (`--meshlets`). `forward` is the last frame: `{ receivers, active, drawn, fallback, unsupported, instanced, visbuffer, lite,
  tier, uploaded_bytes, native_blocks, uploaded_directions }` (receivers set on the renderer; whether they shade with the lightmap; receivers drawn with it; receivers that kept
  the probe light while the lightmap pipeline builds; receivers it cannot shade; of `drawn`, those in the GPU-driven batches,
  those the visibility resolve shaded and those of the lite shader; the lookup the frame allowed: `full`, `directional` or
  `bilinear`, empty before the first frame; since ADR-3145 the bytes the forward pass's copy holds on the GPU now, whether
  it samples the file's BC6H or ASTC HDR blocks natively, and whether it holds the direction section). `receivers` is the receiver entities:
  `{ receivers, applied, errors: [{ entity, entry, code, message }], clusters, no_clusters }` (with the profile's visibility
  buffer on, the applied receivers whose clusters from the bake the renderer holds, and those without; both 0 with it off). `has_directions`
  says the installed file has a direction section; `directional`, `bicubic` and `specular` (ADR-4762) are the runtime switches (all
  `true` by default); `gpu_bytes` is `4 × width × height`, twice that with directions: the file's RGB9E5 size, not the allocation. For what
  the device holds, read `forward.uploaded_bytes` (ADR-3145): 1 B a texel for natively sampled blocks, 4 for RGB9E5, plus
  4 a texel for the directions when the frame's tier uploads them (the `bilinear` tier, the Low lite shader, does not;
  `forward.uploaded_directions` says which); 0 until a receiver draws. It works without a renderer: with nothing installed it
  reports `installed: false` and the flag's default (`enabled: false`).
- `render.set_lightmaps { enabled?, intensity?, bicubic?, directional?, specular?, path?, clear?, receivers?, auto_receivers? }` switches the flag, sets
  the intensity (`0..=1000`), switches bicubic filtering, the directional rebalance and the receivers' indirect specular from the
  bake (ADR-4762), loads a `.flmap` relative to
  the project root (no `..`, no absolute path) or clears the installed one. A path the cooked manifest names
  (`assets/maps/bore.flmap`, or the same without `assets/`) is read from the pak or the cooked cache first, so it
  works in a packaged game; otherwise the raw file is read. Every
  parameter is checked before anything changes. It returns `{ before, after, load_error }`. A file that does not load
  keeps the installed lightmap. `load_error` and `after.error_code` then carry one of the `lightmap_*` codes above.
  `receivers: [{ entity, entry }]` (at most 4096) gives each entity a `LightmapReceiver` for the entry named
  `entry`, or removes it with `entry: null`; every entity must exist and have a `MeshRenderer`, or the call changes
  nothing (`invalid_params`). The result adds `receivers_set`. The receivers take effect on the next frame
  (`render.get_lightmaps` `receivers`). `auto_receivers: true` runs after the load and gives every `MeshRenderer` entity
  whose `EntityGuid` the installed file was baked for its receiver (format version 3); the result adds `receivers_by_key`
  and `unmatched_keys` (at most 32). It needs the `EditScene` permission.
- Since ADR-2313 `render.get_lightmaps` adds `level` (the open level's own lightmap: `{ file, installed, error, error_code,
  entries, keyed_entries, attached, unmatched, warm }`; `error_code` is `lightmap_not_found` or a reader code; `unmatched`
  lists at most 32 keyed entries no drawn entity has) and `levels` (the `LightmapReceiver` entities grouped by the level
  their entity, or their `MeshPartOf` owner, belongs to, `level: null` first for entities of no level, then by level guid:
  `{ level, current, lightmap, receivers, applied, entities }`, at most 256 entities listed per level, each
  `{ entity, guid, part, entry, applied, by_level }`; `by_level` marks the receivers the level's open attached).
  `auto_receivers` also attaches every `MeshPartOf` part entity now, not only part 0.

## The Lightmaps panel

The editor's **Lightmaps** tab runs `regia bake-lightmaps` in a child process (the editor runs the CLI named by
`REGIA_CLI`, else `regia` next to it, else `regia` on `PATH`): pick a source (by default the open level), set the density
and samples, **Lay out** previews the atlas without baking, **Bake** bakes with a progress bar, **Cancel** kills the bake.
The atlas preview colors each entry by its density against the one asked for (green as asked, blue lower, orange higher)
or by its seam `p95` against the budget (green to red, with the worst seams drawn). **Seams in the viewport** draws them
in the scene, under the bounds overlay. The panel and these methods share one state, so an agent sees a bake a user
started and the other way round. One bake runs at a time.

- `editor.lightmap_bake { source?, out?, dry_run?, wait?, density?, samples?, bounces?, seed?, max_atlas?, direct_sun?,
  no_filter?, gpu?, meshlets?, unwrap? }` (permission `assets`). `source` is a `.level.ron`, a `.level.d`, a `.bake.ron` (absolute, or
  relative to the project) or a built-in; default the open level. A file `source` and `out` must be under the allowed roots
  (`path_outside_project`), `density` above 0. A refused call leaves the panel's source and settings as they were. the default is the CLI's
  `assets/maps/<level>.flmap` for a level, beside a description, `.regia/bake/<name>.flmap` for a built-in. Only a dry run
  may `wait`. Returns the status. A refusal carries `data.bake_error`: `lightmap_busy`, `lightmap_no_source`,
  `lightmap_wait_needs_dry_run`, `bake_out_dir` or `bake_spawn_failed`.
- `editor.lightmap_status { list? }`: `{ job, state, running, source, dry_run, args, progress, summary, error, log, view,
  preview, seam_budget, seam_overlay, overlay_segments, checkpoint }`. `state` is `idle`, `running`, `succeeded`, `failed` or
  `cancelled`. `progress` is `{ stage: "lightmap", done, total, fraction, remaining, eta_seconds }` (`eta_seconds` from a
  checkpointed bake's lines, else null; `checkpoint` is the last job's output's checkpoint, ADR-3095), `summary` the CLI's JSON and `error` the CLI's
  `{ code, path, message }`. `view` is what the panel draws: `{ width, height, density, fit_scale, dry_run, entries: [{ name,
  label, rect, texels_per_metre, density_ratio, seam_edges, seam_p95, seam_max }], seam_segments, worst_seam, over_budget
  }`. `list: true` adds `builtins`, `levels` and `descriptions`.
- `editor.lightmap_cancel`: kills the running bake; `cancelled` is false when none was running. The output file is not
  replaced. A kill in the instant of the final write can leave a `<out>.tmp<pid>` beside it.
- `editor.lightmap_preview { preview?, seam_budget?, seam_overlay? }`: what the panel shows: `density` or `seams`, the
  budget `view.over_budget` and the colors use, whether the viewport draws the seams. Absent values stay; a bad value
  changes nothing. `overlay_segments` in the status counts the lines drawn. `seam_budget` here is a preview value: a
  shipping package reads `[lightmaps] seam_budget` in `Regia.toml`.

To check a bake from an agent: `editor.lightmap_bake {dry_run: true, wait: true}`, read `view.entries[].density_ratio` and
`view.fit_scale`, then `editor.lightmap_bake {}`, poll `editor.lightmap_status` until `running` is false, read
`view.over_budget`, and take a screenshot with `editor.lightmap_preview {seam_overlay: true}`.

## Checking a bake

1. `regia bake-lightmaps <map>.bake.ron --dry-run`: `atlas.fit_scale` is 1 and every entry's `texels_per_metre` is
   what you asked for. Raise `--max-atlas` or lower `--density` if not.
2. Bake. `report.texels_inside` is small next to `texels_baked`, and `coverage.empty` is only the space between
   entry rectangles.
3. `regia bake-lightmaps <map>.flmap --inspect`: `contents` lists the light you expect (`sun_direct` only with
   `--direct-sun`).
4. In the running app: `render.set_lightmaps {path: "maps/<map>.flmap", enabled: true}`, then `render.get_lightmaps`:
   `installed` is true, `error_code` is null, `has_directions` is true, and `gpu_bytes` is `8 × width × height`
   (`4 ×` with `--no-directional`).
5. Chart seams: `seams.p95` in the bake's JSON, or `entries[].seams` in `--inspect`. A step along one chart's whole border
   is usually noise: more `--samples`. A step only on some edges is usually a mesh problem: a floor running under a wall
   on one side of the cut.
6. Noise without the denoiser: bake once with `--no-filter` and check `report.noise.p95`. Above about 0.05 the
   filter is hiding visible noise: raise `--samples`, or use `--noise-target 0.03 --max-samples 4096` so only the
   noisy texels pay.

For a quick check of the baker itself, `regia bake-lightmaps quad-box --instance 0 --direct-sun`: the floor far
from the box holds about `π + 8` lux (the sky's `π L` and the sun at 53° elevation), the box's shadow on +x drops
to the sky alone, and the floor next to the box's sunlit −x face turns red from the bounce.

Judge the look by eye against the path tracer (`regia render --reference`): the forward pass shades receivers with the
lightmap, and `docs/evidence/lightmaps-v030/` holds the side-by-side plates. Texel counts do not measure quality.

## How the baker lights a texel

- Direct light by next-event estimation: the sun (only with `--direct-sun`), point and sphere lights, spot lights with
  the runtime's linear cone ramp, and emissive surfaces picked by power. Quad lights are emissive triangles, so they
  also cast shadows and block the sky. A texel inside a sphere light gets nothing from it.
- Indirect light by path tracing over `--bounces` path vertices (1 = direct light only).
- Direction: every light contribution at the texel (each light sample, and what a hemisphere path brings back along
  its first direction) adds its direction weighted by its luminance. The mean is the texel's dominant direction. It
  costs no extra rays and no extra random numbers, so the irradiance is the same with or without
  `--no-directional`.
- Rays leave the surface by a few float steps along its normal, so a wall a few centimeters thick still occludes in a
  map kilometers across.
- The result is the same for any `--threads`, and an entry's texels do not depend on where the atlas put it: the
  random streams are keyed by the seed, the entry's name and the texel's place inside the entry. Renaming an entry
  changes its noise.

Tools that already have a mesh's UV2 can bake that mesh alone with `regia_pt::lightmap::bake_mesh_lightmap`: a
one-entry atlas of a given size and gutter, with the whole scene lighting it.

## Limits

- Receivers on the visibility buffer use the cluster LOD cut. A coarse level keeps each triangle inside one chart: a test
  counts the triangles of every level of a bumpy sphere's clusters on both generators, and another draws a flat two-chart
  surface at 4 clusters. Inside a chart a coarse triangle interpolates UV2 over a longer span. Nobody has judged a real
  curved receiver on a coarse level by eye.
- A `--meshlets` file holds clusters per entry, not per mesh, for its whole life in memory: two instances of a rock hold two
  sets, as they hold two unwraps. `--inspect` lists `meshlet_bytes` per entry.
- One dominant direction per texel, not a full lobe: light from two opposite sides averages toward no direction, so
  a normal map then changes little. The reconstructed specular is a broad gradient; local captures supply sharp reflections.
- The baker is Lambertian. Metallic surfaces bounce `base × (1 − metallic)`. Transmission, alpha masks, sky bodies,
  media and the terrain heightfield are not traced. A tabulated sky is baked as its mean radiance.
- The seam figures are measured against the mesh as baked. A file older than version 3 has none: a shipping package warns
  about it and does not fail.
- The GPU baker takes the same limits as the CPU one, and adds float rounding: a ray that grazes an edge can go the other
  way, so a few texels differ more than the mean does (`crates/regia_pt/tests/lightmap_gpu.rs`). A texel's `rays` is a
  32-bit count on the GPU; only the report reads it.
- The scene itself (BVH, triangles, emitters, lights) is not chunked: it must fit the device's largest storage binding
  and `--gpu-memory-mib`. Only the texel list is chunked. A tile is waited for as a whole, so `--gpu-tile-rays` and not the
  pause decides how long an editor frame can wait for the GPU. A device-lost check reads the process-wide lost flag
  that the device-lost callback sets. The CLI installs that callback on its own device. A host that bakes in-process
  with `bake_lightmap_gpu_with` gets the engine's callback (`regia_render::gpu_health::install_device_hooks`).
- In the CLI, UV2 comes from the generator only, at bake time. Authored `TEXCOORD_1` is not read, and cooked meshes do not
  carry UV2 (the `.flmap` holds the unwrap). Only the Rust API (`bake_mesh_lightmap`) takes an authored UV2.
- Resolution per tier is the bake's `--density`, one bake per package tier (ADR-3001): ULTRA 16, HIGH 8 (the default),
  LOW 4, ULTRA_LOW 2 texels/m with `--no-directional` and a block encoding, or no lightmap. The runtime tier then lowers the
  lookup (`forward.tier`), not the texel count. For a device with a small 2D texture limit, also bake with a smaller
  `--max-atlas`. No step yet makes every tier's lightmap from one bake.

## Denoise and block encodings

New bakes are denoised (ADR-2314). `regia bake-lightmaps` runs an edge-aware à-trous filter over the baked texels of each
chart: 5×5 taps at spacing 1, 2, 4, ... (`--denoise-iterations`, 1 to 8; by default the count whose reach is nearest 3.75 m: 2, 3, 4, 5 at 2, 4, 8, 16 texels/m, ADR-4108), weighted by
the normals, by the world distance between the two texels, and by their luminance difference measured in standard errors
of each texel's own noise (the baker's per-sample statistics, `report.noise`). A tap that leaves the chart reads the texel
that continues the same surface across the border (found by world position; a wall built from several blocks is
denoised as one wall), never a hard edge (normals more than 30° apart) or a surface elsewhere. Where a kernel is cut off on
one side, each texel moves from the taps' weighted mean toward a first-order (plane) fit of them as far as the gradient
stands out of the noise, so an opening or a border does not pull the light toward the chart's inside (ADR-4108). A
texel's hemisphere directions are stratified (the first two Sobol dimensions, scrambled per texel), so the count that
finds a small bright region (a sunlit floor seen through a skylight) varies less between texels. The denoiser replaces the 3×3 filter; `--denoise off` bakes as before
ADR-2314 (3×3 filter unless `--no-filter`). The library default (`LightmapBakeSettings::default().denoise`) is `None`, so
code that bakes through `regia_pt::lightmap` directly keeps its output.

`--encoding bc6h` (desktop) or `--encoding astc-hdr` (phones) stores the texels as 4×4 blocks, 1 byte a texel instead of 4
(`.flmap` version 5). The cook does the same per platform from the lightmap's `.meta`: `settings: "(encoding: auto)"` gives
BC6H on desktop platforms and ASTC HDR on Android (`rgb9e5`, the default, keeps the texels). A device that cannot sample the
blocks (ASTC HDR is optional on phones; every desktop GPU has BC6H) gets their RGB9E5 decode, so a file always loads. The
encoded texels stay within 5 % (BC6H) and 6 % (ASTC HDR) of the baked ones per channel, relative to the texel's brightest
channel, on smooth light; the mean is far lower (`encode.mean_rel`).

Try it on the baker's reference scene:

```sh
regia bake-lightmaps quad-box --instance 0 --samples 16 --out /tmp/floor.flmap --encoding bc6h
regia bake-lightmaps /tmp/floor.flmap --inspect
```

The first summary has `report.denoised: true`, `report.noise.mean` (the bake's noise before the filter) and the lower
`report.denoised_noise` (the noise the denoiser carries after its last pass), and `encode: { encoding: "bc6h", bytes,
rgb9e5_bytes, texels, mean_rel, p99_rel, max_rel }`. `--inspect` prints `version: 5`, `texel_encoding: "bc6h"` and the
`denoise` record. In a running app, `render.get_lightmaps` adds `texel_encoding` and `denoise`, and
`render.lightmap_encode_report` measures an encoding on a copy of the installed map before you cook it:

```json
{"jsonrpc": "2.0", "id": 1, "method": "render.lightmap_encode_report", "params": {"encoding": "astc_hdr"}}
```

returns `{ current, report: { encoding, bytes, rgb9e5_bytes, texels, mean_rel, p99_rel, max_rel }, native, width, height }`:
`native` says whether this device samples the blocks as they are (`null` without a device).

| Code | Where |
|---|---|
| `lightmap_bake_settings` (`path: "denoise"`) | `regia bake-lightmaps`: `--denoise-iterations` outside 1..=8 |
| `lightmap_cook_settings` | `regia cook`: a `.flmap.meta` `settings` string that is not `(encoding: rgb9e5\|bc6h\|astc_hdr\|auto)`, optionally with `directions: rgba8\|bc7\|astc_ldr\|auto` |
| `lightmap_payload` | A version 5 block section of the wrong size, or a block the decoder does not read (only the modes the encoder writes) |
| `F0003` `lightmap_not_installed` | `render.lightmap_encode_report` with no lightmap installed |
| `F0002` | `render.lightmap_encode_report` without a known `encoding` |

Measured on `quad-box` (the floor, 36×36, 16 samples against the same bake at 1024 samples; `cargo test -p regia_pt --test
lightmap_denoise_quad_box -- --nocapture` prints the figures):

| Map | PSNR against the reference | Texel bytes |
|---|---|---|
| 16 samples, no denoiser | 30.17 dB | 5184 |
| 16 samples, denoised (default settings) | 33.52 dB | 5184 |
| denoised, BC6H | 33.31 dB | 1296 |
| denoised, ASTC HDR | 33.31 dB | 1296 |

The bake's measured noise (mean relative standard error) is 3.05 %; the denoiser carries 0.18 % after its last pass. The
encodings stay within 0.22 % (BC6H) and 0.26 % (ASTC HDR) of the denoised texels on average. The test holds the denoiser to
at least 3 dB above the raw bake and both encodings to within 1.5 dB of the denoised one.

Limits: the direction section stays RGBA8 (4 bytes a texel); BC6H uses its one-region modes only and ASTC one partition at
4×4, so a block that straddles two charts with very different light loses more (the gutters keep most blocks inside one
chart). ASTC HDR has not been sampled on a phone in this release's tests: its packing is held to the engine's ASTC LDR
decoder and its HDR unpack to the specification's procedure; BC6H is held to the GPU's own decoder where the test device
samples BC.

## Creases, seams and density

New bakes do three more things (ADR-2404); each is a switch of `regia bake-lightmaps`, on by default, and `off` bakes as
before. The library default (`LightmapBakeSettings::default()`) keeps all three off, so code that bakes through
`regia_pt::lightmap` directly keeps its output.

- `--crease on|off`: crease-aware texel placement. A texel whose centre lies inside other geometry (a floor running under
  a wall, the ceiling over it) is moved to the nearest point of its own chart outside that geometry, at most 0.75 texels
  away, and sampled there. It then holds the light at the joint, the contact darkening a room has along its creases,
  instead of a flat copy of its neighbour. A texel with no such point is dropped and refilled, and the first ring of the refill
  continues the light's gradient across the edge instead of copying it, so the bicubic lookup keeps the darkening at the
  edge texel. The report counts them: `report.texels_crease_inside`, `report.texels_relocated`,
  `report.texels_extrapolated`.
- `--stitch on|off`: seam stitching. Along every smooth chart seam (the edges `seams` measures), a least-squares solve
  changes the texels the two sides' lookups read until they agree, staying close to the baked light. Creases (faces that
  bend more than 30°) are left alone: a step in the light is right there. `report.seam_edges_stitched` and
  `report.texels_stitched` count what it did.
- `--auto-density on|off`: small curved receivers get more texels. A receiver whose predicted seam `p95` at `--density`
  passes the seam budget (`--seam-budget`, else 0.25) is laid out at the smallest power-of-two scale up to 8 that brings
  the prediction under it. Flat receivers (walls, floors, boxes) predict nothing and keep the requested density. A level
  entity's own `LightmapSurface.density_scale` (other than 1) is kept as it is.

The JSON's `density` object says what the rule did:

```json
"density": {
  "auto": true, "budget": 0.25, "stitched": true,
  "entries": [{ "name": "instance_5", "density_scale": 4.0, "explicit": false, "curvature": 2.9,
                "predicted_p95": 0.15, "warning": null }],
  "warnings": []
}
```

A receiver still predicted over the budget (its level scale is kept, or the cap of 8 was reached) is listed in
`warnings` with the code `lightmap_density_low` and a message naming the entry. It is a warning, not an error: the bake
finishes, and `--seam-budget` still fails a bake whose measured seams pass the budget.

The Lightmaps panel has the same switches (Denoise, Crease placement, Stitch seams, Auto density) and the texel encoding,
and always passes them to the CLI. Over the agent protocol, `editor.lightmap_bake` takes them as `denoise`, `encoding`
(`rgb9e5`, `bc6h` or `astc_hdr`), `crease`, `stitch` and `auto_density`; an absent one keeps the default (on, and
`rgb9e5`), a wrong type or name is `F0002`:

```json
{"jsonrpc": "2.0", "id": 1, "method": "editor.lightmap_bake",
 "params": {"source": "quad-box", "dry_run": true, "wait": true, "encoding": "bc6h", "stitch": false}}
```

`summary.density.warnings` of `editor.lightmap_status` lists the warnings after the bake.

| Code | Where |
|---|---|
| `lightmap_density_low` | `density.warnings` of `regia bake-lightmaps`: a receiver predicted over the seam budget at the density it gets |
| `lightmap_atlas_scaled` | `density.warnings` of `regia bake-lightmaps`: the charts did not fit `--max-atlas`, so every entry got `fit_scale` of its density |
| `F0002` | `editor.lightmap_bake` with `denoise`, `crease`, `stitch` or `auto_density` not a boolean, or `encoding` not one of the three names |

What it does on the v0.3.0 evidence room (`crates/regia_pt/tests/lightmap_pt_evidence.rs` on lavapipe and
`lightmap_crease_stitch.rs` on the CPU; `docs/evidence/lightmaps-v030/SUMMARY.md` has the full table): see the summary's
"ADR-2404" rows.

## Additive levels, the editor and the sample

**Additive levels (ADR-2635).** A level opened `Additive` (a streamed region) whose settings name a lightmap has it
installed together with the primary level's: `LightmapLevelPlugin` stacks the primary's atlas and each additive one
below it into one atlas (`regia_render::lightmap::combine_lightmaps`), and the receivers of every level resolve by
key. Unloading the additive level puts the primary's own file back. `render.get_lightmaps` reports each under
`level.additive`:

```json
{"jsonrpc": "2.0", "id": 2, "method": "render.get_lightmaps"}
```

`level.additive` is `[{ level: "<level guid>", file: "maps/annex.flmap", installed: true, entries: 1, row: 2, rows: 4,
renamed: 0, error: null, error_code: null }]` (`row`/`rows`: where its texels sit in the installed atlas; `renamed`:
entries renamed `name@<k>` because an earlier level has an entry of that name; a receiver saved in the additive level
is pointed at the renamed entry, so `LightmapReceiver.entry` reads `name@<k>` while it is stacked). Codes: the reader's codes and
`lightmap_not_found` for its file, `lightmap_no_primary` when the primary level names no lightmap (an additive one only
joins the primary's), `lightmap_combine` when the stacked atlas would pass 8192 texels or the levels disagree on
`--direct-sun` (bake every level with it or every level without). The primary level's overrides (`enabled`,
`intensity`) apply to all of them.

**The editor (ADR-2635).** The edit world runs `LightmapLevelPlugin`, so opening a level that names a lightmap in the
editor installs it and the viewport draws the receivers with it (`render.get_lightmaps` over the editor's agent port
answers as in a game). An entity the lightmap has keyed entries for loads its glTF in the cook's part order (one part
per primitive, the cooked vertices), which is what `regia bake-lightmaps --scene` keyed: every part of a multi-material
mesh receives, where the editor's material grouping used to be refused with `lightmap_layout`. Bake from the editor
with `editor.lightmap_bake` (an empty `source` bakes the open level; `denoise` and `encoding` pass `--denoise` and
`--encoding`, ADR-2404):

```json
{"jsonrpc": "2.0", "id": 3, "method": "editor.lightmap_bake", "params": {"source": "", "denoise": true, "encoding": "rgb9e5"}}
```

**SSAO and the baked light (ADR-2635).** A receiver's indirect diffuse term is the bake's, which holds the receiver's
occlusion already, so the frame's screen-space AO no longer darkens it; the material's occlusion texture still applies,
and specular from other sources keeps the SSAO (the baked specular uses material occlusion only, ADR-4762).
The full forward shader, the visibility-buffer resolve and the Mobile lite shader
follow the same rule.

**The sample.** `samples/render_lab/lightmap_room` is a lightmapped level (a room, a static plinth, a dynamic visitor);
its lightmap is not checked in. From that folder:

```sh
regia bake-lightmaps --scene assets/levels/room.level.ron
```

then open `assets/levels/room.level.ron` (`level.open`, `regia run --level`, the editor) and call
`render.get_lightmaps`: `level.installed` is `true` and `level.keyed_entries` is 2 (the room and the plinth; the
visitor is dynamic and keeps the probe light). `regia_cli/tests/lightmap_sample.rs` does this in a temporary copy.

**Block-compressed directions (ADR-2635).** A directional lightmap's direction section can cook as BC7 or ASTC 4×4
LDR blocks (1 byte a texel instead of 4). Set it in the lightmap's `.meta` settings, next to the texel `encoding`
(ADR-2314):

```ron
(encoding: auto, directions: auto)
```

`auto` picks BC7 on desktop platforms and ASTC LDR on Android; `rgba8` (the default) keeps the baked directions.
`render.get_lightmaps` reports `direction_encoding` (`rgba8`, `bc7`, `astc_ldr`, or empty without directions); a device
that cannot sample the blocks gets their RGBA8 decode, which every CPU reader sees too. A file with block directions
does not load in a build older than ADR-2635 (`lightmap_payload`).

## Debug views, capsules and stale receivers

**Debug views (ADR-2656).** `render.set_debug_view` takes three lightmap views. Each swaps the frame's meshes for debug
twins, so it works on every draw path and platform. Whatever is not a receiver draws flat dark grey, so a mesh that lost
its lightmap is plain to see.

| `mode` | What it shows |
|---|---|
| `lightmap_density` | A checker with one cell per lightmap texel, colored by the entry's density scale `k` (its texels per metre over the density the bake asked for, snapped to a power of two): at most 1/4 blue, 1/2 cyan, 1 green, 2 yellow, 4 orange, at least 8 red |
| `lightmap_only` | The lightmap's light on a white rough surface, with the sun, the local lights, the sky light (image-based light and flat ambient) and the air (fog, aerial perspective, clouds, mist) off, through the frame's exposure. Capsule occluders still show. It needs lightmaps enabled (`render.set_lightmaps { enabled: true }`); with them off the receivers draw flat too |
| `lightmap_invalid` | Baked texels grey, texels the bake filled from their neighbours magenta (their centres were inside geometry; `coverage.dilated` in the bake's JSON counts these and the gutters) |

```json
{"method": "render.set_debug_view", "params": {"mode": "lightmap_density"}}
```

`render.get_debug_view` lists them. The density and invalid views read the installed file, so they work with the
lightmaps flag off. The editor's viewport takes the same modes (`mode_from_params`). A texel is a square of up to 4 × 4
texture pixels; an atlas larger than 4096 keeps one pixel a texel and its cells blur at their edges.

**Capsule occluders on receivers (ADR-2656).** `render.set_gi_occluders` capsules darken the volume's ambient cube, which a
receiver does not read for its diffuse term. While capsules are set, the baked screen pass also writes their
cosine-weighted visibility at each lookup texel's depth normal, and a receiver multiplies its baked diffuse term by it on
every path (the CPU loop, the GPU-driven batches, the visibility buffer, the lite tiers), so a parked vehicle darkens a
lightmapped floor. The numbers match the CPU's `capsule_visibility` within 0.035. A frame with no capsule neither writes
nor reads the word, so its pixels do not change. The pass runs only while a baked volume lights the frame: with a
`.flmap` and no `.fgiv`, or on a frame without a depth prepass, capsules do nothing (`render.get_gi_mode` `baked.active`
says whether it ran). On a profile that reuses its screen passes on a held view, capsules that change while the camera, the sun and
every draw stay put show on the next frame too: the reuse key holds the capsule list (ADR-2840), and is unchanged with no capsule.

**`lightmaps` in `render.get_gi_mode` and `level.get_baked_gi` (ADR-2656).** Both carry the same object, built from
`render.get_lightmaps`'s numbers:

```json
{"installed": true, "pages": 1, "gpu_bytes": 2097152, "instances": 12, "stale_instances": 1, "unverified_instances": 0, "error": null, "error_code": null}
```

- `pages`: atlas pages (texture array layers) on the GPU, 0 with no lightmap installed. Additive levels stack in a page
  while it fits and append pages when one would overflow ([Atlas pages](#atlas-pages), ADR-3300).
- `instances`: entities with a `LightmapReceiver`.
- `stale_instances`: receivers whose mesh no longer matches their `.flmap` entry: the mesh's hash differs (moved vertices),
  the triangles no longer fit (`lightmap_layout`), or the file has no such entry (`lightmap_entry`). Re-bake to clear them.
  The last two fall back to the volume and are in `render.get_lightmaps` `receivers.errors`. Moved vertices over the
  same triangles follow `stale_receivers`.
- `stale_receivers`: what a moved mesh does (ADR-3006, `LightmapSettings::stale_receivers`, set with `render.set_lightmaps`).
  `keep_drawing` (the default) keeps the baked light and reports it here. `fall_back` draws the entity's own mesh with the
  frame's other light, the baked volume where one covers it: the frame is the one with no lightmap for that entity, and the
  receiver is in `receivers.errors` with `lightmap_receiver_stale`. A file with no mesh hash cannot tell a moved mesh, so its
  receivers keep drawing under either policy.
- `stale_fallback`: receivers `fall_back` has taken off the lightmap (`receivers.stale_fallback` in `render.get_lightmaps`).
- `unverified_instances`: receivers whose triangles fit, in a file with no mesh hash (baked before `.flmap` version 6, which
  `regia upgrade-project --write` does not change: only a re-bake records the hash).
- A UV2 cannot go stale on its own: a receiver draws the UV2 in its entry, and the file's payload hash guards it.

## Meshes with `TEXCOORD_0` only

Most game content carries one UV set, for its textures, and no lightmap UV set. `regia bake-lightmaps` does not need
one: it lays out the second UV set itself for every static mesh, whatever UV sets the mesh has, and stores that unwrap
in the `.flmap` (per entry: the source vertex of each unwrapped vertex, its UV2, the triangles). The cook passes the
`.flmap` through to the package, and the runtime draws each receiver from it (`regia_render::lightmap::receiver_mesh`).
The source meshes are never written: after a bake the only new file under `assets/` is the `.flmap` (and, with
`--write-level`, the level names it). A mesh edited after the bake no longer fits its entry and falls back to the probe
light until it is baked again (`lightmap_layout`). ADR-3001.

What an agent checks after a bake: `ok` is true, `level.statics` counts the static meshes, every entry has `charts` above 0,
and `seams.p95` is under the package's seam budget. The test `crates/regia_cli/tests/lightmap_gallery` does the same on
meshes with a tiling `TEXCOORD_0` (2 m a repeat, far outside 0..1) and checks the glTF files are byte-identical after the bake.

## The interior plate

The v0.3.0 interior plate is an underground gallery built in Regia Engine (`crates/regia_cli/tests/lightmap_gallery/gallery.rs`,
fixture project `crates/regia_cli/tests/fixtures/lightmap_gallery`): 18 m wide, 32 m long, an 11 m ceiling, lit only through a
skylight shaft, a side adit and a ramp to the surface, at noon under Earth's sky. Bake it as any level:

```sh
cd crates/regia_cli/tests/fixtures/lightmap_gallery
regia bake-lightmaps --scene assets/levels/gallery.level.ron --samples 256 --bounces 6 --out /tmp/gallery.flmap
```

`docs/evidence/lightmaps-v030/SUMMARY.md` § 8 has its plates against the path tracer, the § 2.2 GI numbers, the seams and the
bytes per tier density (ULTRA 16, HIGH 8, LOW 4, ULTRA_LOW 2 texels per metre or no lightmap, ADR-3001). Lightmap bytes are part
of a map's load set and count against its instant-load and memory budgets ([instant load](instant-load.md)). The direction
section stays 4 bytes a texel in every encoding, so a block-compressed ULTRA_LOW lightmap is baked with `--no-directional`
(on the gallery 0.38 MB of BC6H instead of 1.1 MB, with no measurable change in the light).

To score a bake against the path tracer the way the plate does, run the test with the evidence settings on a real card:

```sh
REGIA_GPU_UUID=<uuid> REGIA_LIGHTMAP_EVIDENCE_DIR=/tmp/gallery REGIA_GALLERY_SIZE=640x384 REGIA_GALLERY_SPP=4096 \
  REGIA_GALLERY_SAMPLES=256 cargo test -p regia_cli --test lightmap_gallery gallery_lightmap -- --nocapture
```

It writes `gallery.json` (per view: `indirect.lightmap.flip_mean`, `energy_ratio`, the reference's `reference_noise_floor`;
`seams.worst_p95`; `tiers[]` with bytes per encoding) and the plates. Read `reference_noise_floor` before judging
`flip_mean`: it is the share of the score that is the tracer's own grain.

The same run also checks the look in figures (ADR-4108, `SUMMARY.md` § 9): `lightmap_space` measures the bake itself
(`mottle`, a 0.5 m over 3 m band-pass of a single-chart wall's light; `seam_step` and `edge_step`, the step where the east
wall's charts meet, with two border-free control lines on the west wall), and every view adds `regions` (east and west
wall, south floor, ceiling: the flatness of half-metre cells against the tracer's and against its own noise, and the mean
colour of the lightmap's indirect light against the tracer's diffuse light, its whole indirect light and the beauty) and
`seam_step_screen`. To see the walls of a bake as the runtime reads them, run the CPU study with your own variants:

```sh
REGIA_GALLERY_STUDY="base=;more=--denoise-iterations 5" REGIA_GALLERY_STUDY_DUMP=/tmp/walls \
  cargo test -p regia_cli --test lightmap_gallery gallery_lightmap_space_study -- --nocapture
```

Each variant prints one `STUDY {...}` line (`seconds`, `noise`, `lightmap_space`) and, with the dump directory, writes
`<variant>_walls.png` (the east wall over the west wall, 8 samples a metre). `name=@path/to.flmap` measures a lightmap baked
elsewhere instead of baking.

## Measuring lightmaps on a phone

`samples/lightmap_device` draws the baker's quad-and-box scene (`regia bake-lightmaps quad-box`) as two lightmap receivers.
It installs nothing on its own. A driver copies a `.flmap` into `<app files>/lm`, then writes `install <file>` or `clear` to
`command.txt` there. The sample answers in `status.txt`. Switching and reading go through `render.set_lightmaps` and
`render.get_lightmaps` over `regia install-android --attach`. The sample initializes `RenderSettings` when absent, so
`render.set_profile` can switch between `MobileLow` and `MobileHigh` after launch; an existing host preset is preserved.
For what the device holds, read `forward.uploaded_bytes`,
`native_blocks` and `uploaded_directions`. The process cost comes from `dumpsys meminfo` (TOTAL PSS, Graphics, and the native
`Heap Alloc` column: on Android 17 some phones book the native allocator under "Unknown"). Rows for the Pixel 6, test Pixel
9 Pro, Fairphone 5 and Galaxy Tab A 8.0 are in `docs/perf-log.md`.

Shader bisects on a device (ADR-3145): every `*.wgsl` file in `<app files>/lm/shaders` is staged as an edit of the engine
shader with the same name, before the device and the boot pipeline warm exist. GPU-driven pipelines get the edit too. Push
an edited `lab_forward.wgsl` with `adb push` and `run-as <package> cp`, relaunch, then read logcat. You will see
`shader override <file> staged`, or `refused` with the composer's error. A native crash under `vkCreateGraphicsPipelines`
or `Failed to link shaders` means the driver refused that shader. Remove the file to go back to the built-in shaders.

## Bake time at scale

`stream_world --generate-bakeable <dir> <cells_x> <cells_z> <props>` writes a project of static glTF cube props
(128 m cells, `props` a cell, the same generator as the streaming world) to measure a bake at any size:

```text
cargo run --release -p stream_world -- --generate-bakeable /tmp/w 8 8 200
regia bake-lightmaps --scene /tmp/w/assets/levels/bake_world.level.ron --project /tmp/w --density 1 --dry-run
```

Read `timings` for where the time goes and `atlas.fit_scale` (or the `lightmap_atlas_scaled` warning) for whether
the level still fits one atlas. What drives each phase:

- `layout` grows with the number of meshes and charts, not the density. Meshes are charted and packed on all
  `--threads` workers, one mesh per worker when there are more meshes than workers (the result is the same for any
  `--threads`).
- `bake` grows with the texels baked (`report.texels_baked`) times `--samples` and `--bounces`.
- A page is at most `--max-atlas` texels a side (8192 at most). At 4 texels per meter a level of about 1 km² fills an
  8192 page; past that the layout opens more pages, up to `--max-pages` (see [Atlas pages](#atlas-pages) for its default),
  and only past those is every entry scaled down. Bake a larger world per area (one level each) or at a lower `--density`.
- Memory grows with the texels: plan on about 270 MiB of RAM per million texels baked.
- `load` grows with the props (ADR-3121; it grew with their square before, from reading the exported description's
  version through a whole-text `ron` value parse). On the test machine (Ryzen 9 9950X3D, `--threads 8`) a generated
  1 km² level (2561 props, 15366 charts, 1 texel per meter, 16 samples, 1 bounce) took load 0.09 s, layout 0.07 s,
  bake 4.9 s on the CPU (load was 2.7-4.1 s before ADR-3121); 641 props load in 0.04-0.06 s.
- Check `density_scaled` in a `--dry-run` before a long bake: at 4 texels per meter the 1 km² level gets `factor` 0.8
  of an 8192 atlas, and the remedy is a smaller level.

## Resumable bakes

**Since ADR-3095.** `regia bake-lightmaps --checkpoint` writes the finished texels to `.<out>.bakeck` beside the output
every few batches; after a crash, a driver reset or a killed job the same command continues from it and writes the
bytes an uninterrupted bake writes (CPU and `--gpu`). The Lightmaps panel passes `--checkpoint`, shows a line when the
next bake will continue a checkpoint and can discard it. See [Resumable bakes (checkpoints)](bake-checkpoints.md).

## Lightmaps per package tier

Bake once, at the density of the highest tier you ship; the cook makes every lower tier's lightmap from that bake
(ADR-3120). The tiers are ADR-3001's unless `Regia.toml` changes them:

| Tier | Texels per metre | Encoding | Directions | Budget (one map's lightmap) |
|---|---|---|---|---|
| `ultra` | 16 | the cooked lightmap's (`.meta` settings) | kept | 320 MiB |
| `high` | 8 | the cooked lightmap's | kept | 208 MiB |
| `low` | 4 | the cooked lightmap's | kept | 96 MiB |
| `ultra_low` | 2 | `auto`: BC6H on desktop, ASTC 4×4 HDR on Android | dropped | 48 MiB |

A tier's density is the bake's divided by a power of two (2, 4, 8 or 16); a tier at or above the bake's density takes the
bake as it is. Change or add tiers in `Regia.toml`:

```toml
[lightmaps.tiers.ultra_low]
none = true            # this tier ships no lightmap: the probe volume lights it

[lightmaps.tiers.mobile]
density = 4            # or density_factor = 2
encoding = "astc_hdr"  # rgb9e5 | bc6h | astc_hdr | auto; unset keeps the cooked lightmap's
directional = false
budget_mib = 32
padding = 2            # gutter texels per chart side at this tier (1..=16; unset: 2 for bc6h/astc_hdr/auto, else 1; ADR-5125, ADR-5621)
```

A package picks its tier in its build profile, and `regia package --profile low` then ships the LOW lightmap in place of
the bake:

```toml
[profiles.low]
entry_levels = ["levels/main.level.ron"]
lightmap_tier = "low"
```

The derived lightmap keeps the bake's UV2 exactly (the same unwrap, so the same receivers), scales each entry's rectangle,
and makes every texel the mean of the baked texels of its own chart under it. Gutters are filled chart by chart, so a lookup
near a chart's edge never reads a mix of two charts, and a chart too small to hold a texel at the tier still gets one. Each
tier's file is an ordinary `.flmap` of the current version. The first gutter ring continues a same-chart baked gradient,
bounded to 0.5..1.5 times its edge value; it falls back to the chart-local mean without two baked samples (ADR-4930).
This does not waive the original seam or dark-point checks; see the closure-C evidence page for remaining misses.

A tier also needs a gutter of its own: `padding` whole texels on every side of each chart. Unset, it follows the encoding
the tier resolves to on the cook's platform (ADR-5621): 2 for a 4×4 block encoding (`bc6h`, `astc_hdr`, and `auto`, which
is one of them on every platform), so no block holds two charts' borders, and 1 for `rgb9e5` (or a tier that keeps an
RGB9E5 cooked lightmap's encoding). The default table's ULTRA_LOW pads 2 and the other tiers 1. A `padding` you write
wins. The bake leaves
`--padding` texels (default 2) at its own density, which shrink with the factor; when fewer than `padding` whole texels
are left (from a 2-texel bake: factor four or more, ULTRA_LOW from a HIGH bake, LOW and ULTRA_LOW from an ULTRA bake), the
tier repacks each entry's charts at its own resolution with `padding` gutter texels (ADR-5125). Each chart keeps its size
and moves by whole texels, so that tier's `.flmap` carries its own UV2 (same vertices and triangles, each chart's UV2
translated); receivers are built from the tier's own lightmap, so nothing else changes. The atlas grows by the gutters.
`derive.repacked_charts` counts the charts moved (0 when the tier keeps the bake's UV2) and `derive.chart_padding` is the
gutter per side the tier has.

To see every tier's bytes before packaging:

```sh
regia cook --platform linux --lightmap-tiers --json
```

It cooks as usual, then writes each tier's cooked lightmap under `.regia/cooked/<platform>/lightmap_tiers/<tier>/` and the
report `lightmap_tiers.json` beside them, and prints the report: `tiers[]` holds the totals per tier (`tier`, `bytes` the
tier package's lightmaps take, `lightmaps` shipped, `left_out`), `lightmaps[]` one row per cooked lightmap (`path`,
`density`, `cooked_bytes`) with `tiers[]`: `tier`, `density`, `factor`, `width`, `height`, `encoding`, `directional`,
`bytes`, `budget_bytes`, `verdict` (`fits`, `over_budget` or `none`), `seam_p95`, and `derive` (what the downsample did:
`resampled`, `nearest`, `looked_up`, `conservative_charts`, `shared_charts`, `dilated`, `repacked`, `gutter_texels`,
and the chart-border fit of ADR-4995: `border_fit_rows` sample rows solved along chart borders, `border_fit_texels`
texels it moved, `border_frozen` texels two charts read where the bake's two sides disagree, kept as they were, `border_steep` border
samples where the bake's light changes faster than the tier can hold: their texels on the chart keep their area means and
only the gutter texels they read are fitted, ADR-5125), and the chart padding of
ADR-5125: `repacked_charts`, `chart_padding`).
Without `--json` it prints one line per tier on stderr.

What an agent checks: every tier it ships has `verdict` `fits`; `seam_p95` stays under `[lightmaps] seam_budget` (the
package's seam check reads the same figures from the tier's header); `derive.shared_charts` is 0 (a chart that shares a texel
with a neighbour at that tier reads the neighbour's light; bake the top tier with a larger `--padding`, or ship that tier
less coarse). Lightmap bytes count against the map's load set and memory budget ([instant load](instant-load.md)): a tier
whose budget cannot hold the lightmap ships without it, and both the cook and the package say so.

To check a tier package on a device (ADR-5382), package it with a profile whose `lightmap_tier` names the tier, start
it, attach, and install the lightmap through its asset path: `render.set_lightmaps {path: "assets/maps/<map>.flmap",
enabled: true}`. The packaged app reads that path from its own pak, so what it draws is the tier the package derived,
not a file you pushed. Then `render.get_lightmaps`: `width` / `height` are the tier's atlas, `forward.native_blocks` is
`true` when the device samples the tier's blocks as stored (ASTC HDR on a phone that has it), and `forward.drawn` /
`forward.fallback` say how many receivers drew with it. The `lightmap_device` sample carries such profiles
(`lm_low`, `lm_high` and their `_dev` twins) over one quad-box bake; its `Regia.toml` says how to bake it. The agent
attaches to an Android app through `run-as`, so drive a debuggable (`dev`) APK; a shipping APK runs the package's
shipping checks but cannot be attached.

The seam figures of a tier are the bake's (measured with the meshes, ADR-2024) plus the step the downsample added on the
same edges, measured from the `.flmap` alone (`regia_asset::lightmap::seam_report_by_source_vertex`).
The derive restores the bake's stitched seams at the tier without the meshes (ADR-4995):
along every chart border where the bake's light is smooth over one tier texel, it moves the tier texels the lookups read
until they read the bake's own light there, so both sides of a stitched seam agree again.

| Code | Where | Meaning |
|---|---|---|
| `lightmap_tier_unknown` | `regia package`, the `Regia.toml` check | A profile's `lightmap_tier` names no tier |
| `lightmap_tier_rule` | `Regia.toml` `[lightmaps.tiers]` | A tier name that is not lowercase letters, digits and `_`, a density that is not the bake's over a power of two up to 16, or a `padding` outside 1..=16 texels |
| `lightmap_direction_encode` | `regia cook`, `regia cook --lightmap-tiers`, `regia package` | The `directions` block encoder (BC7 or ASTC 4×4 LDR) failed, or its blocks do not decode |
| `lightmap_tier_derive` | `regia cook --lightmap-tiers`, `regia package` | The tier atlas would pass the format's limit, or the result does not validate |
| `lightmap_tier_read` | same | A cooked lightmap does not read |
| `lightmap_tier_write` | same | A tier file or the report cannot be written |
| `lightmap_tier_over_budget` | warning of `regia cook --lightmap-tiers` and `regia package` | The tier's lightmap passes its `budget_mib`: that tier ships without it |

## Atlas pages

**Since ADR-3300 (`.flmap` version 7).** A level whose charts do not fit one atlas of `--max-atlas` keeps the density it
asked for on several pages instead of being scaled down: the layout fills a page, the meshes that did not fit go to the
next one, up to `--max-pages`. Only past that budget does `atlas.fit_scale` drop below 1 (and
`density_scaled` say so, naming `--max-pages` in its remedy). A mesh's charts are always on one page; a single mesh larger
than a page still lowers the density. A level that fits one atlas lays out, bakes and draws exactly as before.

The default of `--max-pages` (and of the Lightmaps panel) follows what the frame can draw: 4 when the frame binds texture
arrays, 1 while it binds one 2-D atlas. This build's frame binds arrays (`regia_render::lightmap::LIGHTMAP_PAGED_BINDINGS`),
so the default is 4. A default bake therefore never writes pages
the frame would draw from page 0; ask for `--max-pages N` to bake pages anyway (the bake, `--inspect` and the agent
methods handle them; a paged lightmap installed in a one-page frame logs a warning, and its receivers on pages 1 and later
draw page 0's texels).

```text
regia bake-lightmaps --scene assets/levels/yard.level.ron --max-atlas 4096 --max-pages 4 --dry-run
```

```text
{"ok": true, "dry_run": true, "pages": 3, "density_scaled": null,
 "atlas": {"width": 4096, "height": 4096, "pages": 3, "fit_scale": 1.0, ...},
 "entries": [{"name": "entity_..._0", "page": 0, "rect": [0, 0, 512, 384], "texels_per_metre": 4.0, ...}, ...]}
```

- `pages` (top level, `atlas.pages`) is the page count; each entry has its `page`, and `rect` is in that page's texels.
  Every page is `atlas.width × atlas.height`.
- Memory and file size grow with the pages: each page costs `width × height` texels on the GPU (4 bytes RGB9E5 or 1 byte
  BC6H/ASTC, plus 4 bytes of RGBA8 directions or 1 byte of BC7/ASTC directions). Read `render.get_lightmaps` `gpu_bytes`.
- `--max-pages 1` keeps the old behaviour: one atlas, the density scaled down to fit.
- `regia bake-lightmaps --inspect <file>.flmap` reports `pages` and each entry's `page`. A file baked before version 7
  reads as one page; `regia upgrade-project --write` rewrites it without changing its texels.
- At run time each page is a layer of a texture array; a receiver samples the layer of its entry's page. The page rides in
  the `scale_offset` a draw passes (`LightmapEntry::packed_scale_offset`), so draw paths, tables and lite tiers need no
  other change.
- `render.get_lightmaps` and `render.get_gi_mode` / `level.get_baked_gi` (`lightmaps.pages`) report the installed page
  count; `editor.lightmap_status` reports `view.pages` and each entry's `page`, and `editor.lightmap_bake` takes
  `max_pages` (the Lightmaps panel's "Atlas pages at most").
- Additive levels (ADR-2635) stack in one page while it fits; when the stack would pass 8192 rows, or a level's lightmap
  has pages of its own, each level's pages are appended instead. A one-page frame refuses a stack that would need a new
  page (`lightmap_combine`, as before pages). `render.get_lightmaps` `level.additive[]` reports each
  level's `page` and `pages` (and `row` 0).
- The density and invalid debug views show each entry's own page.

What an agent checks: a `--dry-run` with `pages` at most the budget and `density_scaled` null before a long bake;
`render.get_lightmaps` `pages` and `gpu_bytes` after install.

| Code | Where | Meaning |
|---|---|---|
| `lightmap_uv_settings` | `--max-pages` outside `1..=64` (the CLI refuses it as an argument error), `editor.lightmap_bake` `max_pages` (an `invalid_params` error) | The page budget is out of range |
| `lightmap_layout` | reading a `.flmap` | A version 7 header whose `pages` is not one past the highest entry page, or more than 64 pages |
| `lightmap_combine` | `render.get_lightmaps` `level.additive[].error_code` | The appended levels would pass 64 pages, or the stack needs a new page while the frame draws one |

Receiver reflection occlusion (ADR-4762): the bake keeps material cavity occlusion only;
local capture and pane radiance keep their existing screen/far-field occlusion. These
factors apply before the sources are added, including at partial capture coverage. The
receiver does not add the synthetic enclosure bounce already represented by its bake.

## Per-cell bakes of a streamed world

**Since ADR-3318.** A streamed level (`LevelStreaming` on, `docs/specs/78-world-streaming.md`) bakes one lightmap per
cell, so a cell's light loads and unloads with the cell and a large world does not have to fit one atlas:

```text
regia bake-lightmaps --scene assets/levels/world.level.ron --cells [--cell-ring 1] [--cells-force] [--write-level]
```

- **Partition.** The level's static parts are split by the cook's rules: `StreamPlacement::AlwaysLoaded` and parts
  wider than `max_entity_extent_cells` cells go to the always-loaded set, `StreamPlacement::Cell` forces a cell, every
  other entity goes to the cell holding the XZ centre of its parts, and a hierarchy moves as one (the root's placement
  decides, over the bounds of every part below it), as the cook moves it.
- **Files.** Cell `(x, z)` is `<out stem>.cells/<x>_<z>.flmap` beside the primary file (default
  `assets/maps/<level>.cells/`). The primary (`--out`, the level's own lightmap) holds the always-loaded set's
  receivers, baked against the whole level; with none it is an empty 4 × 4 atlas the level can name. Every cell file
  is a normal `.flmap` (RGB9E5 only: `--encoding` other than `rgb9e5` is `lightmap_bake_settings`; the cook encodes per
  platform), written at one exposure (1/8) so cells stack without re-encoding.
- **Neighbor ring.** A cell bakes against its own parts (the receivers), the parts of every cell within `--cell-ring`
  cells (0 to 4, default 1) and the always-loaded set. Those shadow it and bounce light into it but get no texels, so a
  wall that belongs to the next cell still keeps the sun out. One ring holds every occluder within one cell side of the
  border; use 2 for low suns over tall geometry or small cells. Cost grows with the ring's geometry, not its texels.
- **Seams.** After the bakes, every pair of edge-adjacent cells where one side was baked gets a seam pass: along every
  receiver edge on the shared border (a floor or a wall cut by the cell line, one part in each cell, faces within
  `DEFAULT_HARD_EDGE_DEG`) the two files' bilinear lookups are made to agree by the least-squares solve of the in-file
  seam stitch, over the texels of both files. The JSON's `seams` lists each pair's `edges`, the border step `before` and
  `after` (`|a − b| / max(mean, floor)` on luminance at each sample point, `floor` 1 % of the border's mean light:
  `samples`, `mean`, `p95`, `max`) and the texels changed
  on each side; `border_step` holds the worst `p95` before and after.
- **Incremental.** `cells.json` (`docs/formats/lightmap-cells.md`) records each cell's input hash: its own parts'
  meshes, transforms, materials and keys, the sun, sky and lights, the bake settings. An unchanged cell is `skipped`;
  a changed one is baked and only the seam texels of its neighbors' files are rewritten. A neighbor's edit does not
  re-bake a cell (its light near the border can lag behind the edit); `--cells-force` re-bakes every cell.
- **Checkpoints.** `--checkpoint` checkpoints each cell beside its file (`.<x>_<z>.flmap.bakeck`); a killed `--cells`
  run continues the cell it was in and skips the cells it finished. `--resume` (one file) is refused with `--cells`.
- **JSON.** `cells: [{ key, x, z, file, entries, texels, seconds, skipped, input_hash, gpu? }]` (`texels` baked this run;
  `gpu`, only on a cell `--gpu` baked, holds the GPU baker's stats with its `adapter`),
  `baked`, `skipped`, `primary`, `seams`, `border_step`, `always_loaded`, `timings` (`load`, `primary`, `bake`,
  `seams`, `write`). `--dry-run` prints the partition only.

Errors: `lightmap_cells_not_streamed` (no `LevelStreaming`, or a part outside the grid), `lightmap_cells_index` (a
`cells.json` that does not read), `lightmap_cells_version` (a newer index), plus the bake's own codes.

**At run time.** A level whose `look.lightmap` names the primary (`--write-level` writes it) stacks each loaded cell's
file under its atlas: on `CellLoaded` the cell's `<stem>.cells/<x>_<z>.flmap` is read, on `CellUnloading` it leaves.
Cells sit in fixed slots of `slot_width × slot_height` texels below the primary: a cell keeps its slot while loaded, the
atlas keeps its size and exposure, so the other cells' texels and `scale_offset` stay bit for bit and their frames do
not move (`regia_framework/tests/lightmap_cell_stack.rs`, lavapipe). A cell larger than the slot, a new base (the
primary reloaded, an additive level) or a full stack re-lays the atlas once (`cell_stack.relayouts`). Receivers attach
by key per cell; a cell without a file (no lightmapped part) is reported with `lightmap_not_found` and is not an error.

```json
{"method": "render.get_lightmaps"}
```

returns, under `level`, `cells: [{ cell, file, installed, entries, slot, row, rows, renamed, error, error_code }]` and
`cell_stack: { width, height, base_rows, slot_width, slot_height, exposure, slots, relayouts }` (`null` while no cell
lightmap is stacked).

**Limits.** The stack is one atlas of at most 8192 rows: past that a cell is `lightmap_cell_stack_full` and keeps the
probe light (with 128-texel cells that is about 60 loaded cells). Multi-page atlases (one texture array layer a page)
will hold one cell a page; until they land, keep cell atlases small (`--density`, `--max-atlas`). Cells of an additive
streamed level are not stacked (only the open level's). The cook cooks each cell file like any `.flmap` (a block
encoding the cook picks is read through its decode when the cells stack); `cells.json` is a bake cache the runtime
never reads.

## Direction seams between cells

**Since ADR-4559.** The per-cell seam pass also stitches direction texels. A surface that crosses a cell border
was keeping each cell's own lighting direction: the seam changed irradiance and left the direction section. The
pass now runs the chart stitch's least-squares rows (`(Σ g gᵀ + λ I) x = λ x₀`, λ = 0.05) over the direction
texels of both files. The agreement component is clamped to `0..=1`, as the chart stitch does. `xyz` is written
back through the existing RGBA8 direction encoding, which maps each component from `-1..=1` into a byte.
Irradiance is unchanged from the irradiance-only seam. A file with no direction section is left as it was, and
a bake where only one of the two files has directions does not invent a section on the other.

Each seam object gains:

- `direction_texels` — direction texels the solve changed, both files
- `direction_texels_a`, `direction_texels_b` — the same count per file

The summary gains `direction_seam_texels`, the sum of `direction_texels` over the seams of the run. A neighbor's
edit still does not enter a cell's input hash: light near the border can lag behind that edit until
`--cells-force`. Direction texels the seam does change are written with the neighbor's file, the same way
irradiance seam texels are.

After a seam, direction texels within one texel of the border agree across it within 0.05 per component (the
chart stitch's agreement tolerance, which covers the RGBA8 quantum and the least-squares residual). The `.flmap`
version does not change: the direction section is updated in place. A block-compressed direction section cannot
take a per-texel edit, so a changed one is stored back as RGBA8, an encoding the format already has.
