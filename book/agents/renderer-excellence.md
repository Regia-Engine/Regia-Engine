# Renderer excellence: reference traces and scoring

Regia Engine has a reference path tracer (`regia_pt`, `docs/specs/27-renderer-excellence.md` § 1)
that renders the same meshes, materials, lights and sky as the real-time frame, with the same
BRDF code, in physical units. It is the ground truth every real-time technique is scored
against (§ 2). Two agent methods drive it; both need a host built with `regia_agent`'s
`reference-trace` feature (otherwise they answer `F0013`).

## Trace

```json
seeing.reference_trace { "scene": "current", "width": 640, "height": 360, "spp": 1024 }
→ { "job": 1, "status": "running", "spp": 1024, "spp_done": 0, "out_dir": "/tmp/regia_pt/job-…" }
seeing.reference_trace { "job": 1, "wait": true }
→ { "job": 1, "status": "done", "spp_done": 1024, "exr": ".../reference.exr",
    "preview_png": ".../reference_preview.png", "metadata": {…}, "png_base64": "…" }
```

- `scene`: `current` (what the running app's renderer drew), or a validation scene:
  `cornell`, `cornell_room`, `furnace`.
- `source` (with `current`): `auto` (default) traces the renderer's **frame export**
  (`FrameRenderer::set_scene_export`, switched on at the first trace and left on; it is
  incremental): every mesh instance the frame drew — props, buildings, vehicle hulls, rigid or
  skinned at this frame's pose — with its cooked material (base color, packed ORM, normal,
  emissive, detail, IOR and thin transmission), the terrain the shared draw renders (heights at
  full resolution; its albedo is the shader's own when the host installed
  `regia_pt::terrain_bake::install_reference_albedo`, else the layer stack's first-order
  color), and the light the frame used: the sun after the level's sun clock, the planet's sky
  and air, the sky body and the moons. If no frame was exported yet, the extracted frame is
  drawn once offscreen to record one. `export` fails instead of drawing; `extracted` traces the
  ECS extraction (no shared-draw terrain).
- Every reply carries `scene_info` (`source`, `instances`, `triangles`, `terrain`, `sun_lux`,
  `sky`, `sky_bodies`, `stars`, `brdf_energy`, `material_clock`, `origin_offset`,
  `foliage_wind`): check that the heroes and the bodies made it in before waiting
  on a long trace. Graphs that read time, world position or wind take the runtime renderer's
  material clock, floating origin and foliage wind (a trace without a runtime renderer reports
  the origin as `[0, 0, 0]` and no clock or wind — the extracted frame alone cannot know them).
- **Airless maps** (Europa, the Moon): the sky is black with the planet's star field, the sun
  is a disc light of the planet's angular size, and each sky body (Jupiter from Europa) lights
  the scene as a disc area light with the frame's planet-shine fill (`fill_lux`, e.g. DO27's
  1600 lx) and real soft shadows.
- **Glass** (`transmission` > 0) is a thin pane: light passes straight through with the
  forward pass's weight (1 − Fresnel, base color, volume tint), and shadow rays pass it.
- Subsequent traces reuse the bottom-level BVHs of meshes they already saw, so re-tracing
  from a moved camera rebuilds only the instance level.
- `spp` 64 is a quick look; use **≥ 1024** for a reference. `seed` (default 1) makes the
  noise pattern reproducible: the same seed, scene and device give the same image bit for bit.
- `cancel: true` (with `job`) stops after the current batch and keeps what has accumulated.
- At most two traces run at once: a third start answers `too_many_pending` until one finishes.
  The app keeps the 16 most recent finished jobs; polling or comparing an older job id answers
  "no trace job" (keep the EXR paths from the finished status instead). A trace thread that
  panics ends its job as `failed` instead of leaving it `running`.
- The EXR has one layer per AOV: `beauty`, `albedo`, `normal`, `depth`, `direct` (sun, lamps
  and emissive surfaces reflected once), `indirect_diffuse` / `indirect_specular` (sky light
  and all multi-bounce light, split by the first lobe — the GI terms), `emissive`,
  `volumetric` (light from paths whose first event is in the air or fog: aerial perspective and
  fog in-scattering), `sky` (background, sky bodies and sun disc seen directly, through the
  fog in front of them), `shadow` (sun visibility at the first hit), `variance` (of the beauty
  mean) and `spp`.
- Traced since v2: the planet's air (Rayleigh + Mie, aerial perspective on everything that ends
  at a surface) and the level's height fog as media, with the runtime's parameters; sky bodies
  (Jupiter, Io) as lit discs that also light the scene with the real-time fill's illuminance —
  but with shadows; quad area lights; anisotropy, detail maps and packed ORM textures exactly as
  the forward pass applies them; a thin lens (`PtCamera::with_lens`).
- Not traced: fog volumes, ozone, particles and decals, parallax, edge wear, glass
  tint and Fresnel on shadow rays, refraction offsets of thick glass, and the Earth night sky
  (stars only on airless planets). A `current` trace with `clouds` set to `droplet` or `raster`
  also traces the frame's cloud layer when the frame has one (below); `off`, the default, traces
  none. A host with several views exports the last one it drew.

## From the command line

- `regia render --reference --project <dir> [--spp 1024] [--width 1280 --height 720]
  [--out-dir DIR]` starts `seeing.reference_trace` on the running app's current view, prints
  progress, and prints the finished job (EXR, preview, metadata paths). Without `--reference`
  it saves the real-time window (`--out render.png`).
- `render_lab pt <scene> [--spp N] [--bounces N] [--width W --height H] [--out DIR]` traces a
  lab scene (`material_grid`, `clustered_lights`, `light_shadows`, `surface_maps`,
  `alpha_modes`, `ibl_spheres`) or a validation scene (`cornell`, `cornell_room`, `furnace`)
  and writes `<scene>_pt.exr/.png/.json` next to the real-time `<scene>_rt.png`.
- `pt_plates --evidence docs/evidence/pt` (in `samples/lookdev`, with `REGIA_DO27_CONTENT`)
  is the nightly ground-truth job: all twelve parity plates at 1280×720 / 2048 spp into
  `docs/evidence/pt/<date>/` with `scores.json`, a score table in `README.md` and one contact
  sheet per plate; `--resume` continues a killed run.
- `regia bake-gi <scene> [--out X.fgiv] [--rays N] [--samples N] [--bounces N] [--seed N]
  [--no-filter]` bakes a diffuse GI probe volume with the same kernels (baked lighting,
  phase 1; part of the baked-light release, v0.2.0): `<scene>` is `cornell`, `furnace`, `rolling-hills` or a bake description `.ron`
  (raw u16 heightfield, sun, sky, sky bodies, terrain layer spacing and levels, detail bricks).
  It prints a JSON summary (`report.valid`, `relocated`, `inside_geometry`, `under_terrain`,
  `rays_traced`, `seconds`). Per probe: L1 SH of incident radiance per component (`sun_bounce`,
  `sky`, `fill`, `emissive`; the sun disc and local lights stay live), octahedral distance
  moments, validity. Format and description: `docs/formats/baked-gi.md`. Same seed, same bytes.

## Denoised previews

```json
seeing.reference_trace { "scene": "current", "width": 1280, "height": 720, "spp": 64, "denoise": true, "wait": true }
→ { "job": 2, "status": "done", "denoised": true, "exr": ".../preview.exr", "preview_png": ".../preview.png",
    "raw": { "exr": ".../reference.exr", "preview_png": ".../reference_preview.png", … },
    "metadata": { "spp": 64, "denoiser": "oidn 2.5 RT hdr albedo+normal high", "denoise_seconds": 0.4, … },
    "total_seconds": 3.1, "png_base64": "…" }
```

- For look iteration: a low-spp trace plus Intel Open Image Denoise is a quick preview, not a
  reference. The repository holds no measured comparison against a converged trace; measure it with
  `seeing.compare` (a converged raw trace as `a`, the preview as `b`), and read the denoise cost from
  `metadata.denoise_seconds`.
  Only `beauty` is filtered; the other AOVs are the raw trace's.
- Needs a host built with `regia_agent`'s `denoise` feature (it links Intel's native OIDN
  library; desktop tools only). Without it, `denoise: true` answers `F0013`.
- **A preview is never a reference.** Denoising trades noise for bias, so the preview's EXR
  header and PNG carry `regia.denoiser`, and `seeing.compare` answers `F0002` when one is
  given as `a` (the job id of a denoised trace resolves to its preview). Score with the raw
  trace (`raw.exr`) or a converged trace as `a`; a preview may be `b`, which is how its own
  error is measured.

## Compare

```json
seeing.compare { "a": { "job": 1 }, "b": "/tmp/frame_hdr.exr", "aov": "beauty", "ev100": 12.5 }
→ { "flip_mean": 0.041, "flip_p95": 0.12, "ssim": 0.93, "rmse": 210.0, "rel_rmse": 0.08,
    "mean_reference": 2600.0, "mean_test": 2510.0, "psnr_db": 31.4, "aov": "beauty",
    "flip_map_png": "…", "mime": "image/png", "png_base64": "…" }
```

- `a` is the reference, `b` the image under test: EXR (linear) or PNG (display-encoded), or a
  finished trace job.
- **FLIP** (Andersson et al. 2020) runs on both images through the runtime's AgX at `ev100`,
  so it measures what a viewer sees. 0 is identical; below about 0.05 a viewer flipping
  between the two hardly sees a change; 0.1–0.2 is a visible but local difference. `flip_p95`
  catches small, strongly wrong regions the mean hides.
- **RMSE** is on linear luminance (`rel_rmse` = RMSE / reference mean): energy errors that the
  tonemap would hide (exposure, albedo, a missing bounce).
- **SSIM** on display luma, the goldens' structure metric.
- **PSNR** (`psnr_db`, ADR-2315) on the same display-encoded pair as FLIP and SSIM, peak 1:
  `10 log10(1 / MSE)` over every RGB channel. `null` when the images are equal. `mode: "rmse"` keeps it,
  `flip` and `ssim` drop it. Every mode also returns `aov`, `mime` (`image/png`), `flip_map_png` and
  `png_base64`. The v0.3.0 lightmap evidence (`docs/evidence/lightmaps-v030/`) records it per AOV.
- The heat map (`flip_map_png`) is black where the frames agree, magenta → orange → white
  where they differ.

## Scoring GI

Compare `indirect_diffuse` (plus `indirect_specular` for reflections) of the reference with
the real-time frame's indirect light. A real-time frame's indirect is its linear HDR minus
the same frame with every indirect term off; `lookdev`'s `pt_plates` does exactly that for the
parity plates and writes `<shot>_scores.json` with beauty, direct and GI scores. The ★ targets
of spec 27 § 2.2 (GI mean FLIP ≤ 0.08, p95 ≤ 0.20) are first guesses until the measured values
are written into the spec.

## Validation (why the reference can be trusted)

`cargo test -p regia_pt` runs § 1.5 on the same kernels at tiny sizes: furnace spheres
match their BRDF albedo to 0.3 %, a closed white sphere matches the bounce series, a plane
under a sun, a sphere light and an emissive quad match their analytic irradiance, the Cornell
room matches an independent radiosity solution, the forward pass's direct term equals the
tracer's `direct` AOV (also with anisotropy, packed ORM and detail maps), and two runs with
one seed are bit-identical. v2 adds: transmittance through absorbing slabs and exponential
layers is exact, a conservative chromatic fog in a uniform sky returns the sky's radiance in
every direction, a sky body delivers its disc irradiance, a quad light matches Lambert's
polygon formula and a thin lens blurs a point by its circle of confusion.

## Bindless step 1: draw counters and the bindless tier

Two read-only calls measure what the renderer pays for binding before the bindless work goes
further (ADR-316). Both are pure reads; neither changes a frame.

`render.get_frame_stats` returns the encoder calls of the runtime renderer's last main-view frame,
per pass and in total:

```json
{
  "frame": 120,
  "passes": {
    "shadow":            { "draws": 0, "pipeline_switches": 0, "bind_group_switches": [0, 0, 0, 0], "bind_group_switches_total": 0, "material_batches": 0 },
    "depth_prepass":     { "draws": 4, "pipeline_switches": 1, "bind_group_switches": [1, 4, 2, 0], "bind_group_switches_total": 7, "material_batches": 2 },
    "visbuffer_resolve": { "draws": 0, "...": "..." },
    "opaque":            { "draws": 4, "pipeline_switches": 1, "bind_group_switches": [1, 4, 2, 0], "bind_group_switches_total": 7, "material_batches": 2 },
    "translucent":       { "draws": 0, "...": "..." }
  },
  "total": { "draws": 8, "pipeline_switches": 2, "bind_group_switches": [2, 8, 4, 0], "bind_group_switches_total": 14, "material_batches": 4 },
  "material_constants": { "path": "storage_arena", "live": 2, "capacity": 256, "stride": 352, "buffer_creates": 1, "epoch": 0 }
}
```

- `draws`: draw calls (`draw`, `draw_indexed`, one per `draw_indexed_indirect`).
- `pipeline_switches`: `set_pipeline` calls that bound a different pipeline.
- `bind_group_switches`: `set_bind_group` calls that bound a different group, per group slot
  (0 view or shadow tile, 1 object, 2 material, 3 the pass extra: refraction, glass, mask decals).
- `material_batches`: runs of consecutive draws that share one material bind group. This is the
  number a bindless renderer would merge into fewer calls.
- `material_constants`: `path` is `per_material` (each material has its own uniform buffer, tier
  `none`) or `storage_arena` (every standard material's constants in one storage buffer). For the
  arena, `live` is the materials that hold a slot, `capacity` and `stride` are the buffer's size in
  slots and bytes per slot, `buffer_creates` is how many buffers it ever created (it stays flat after
  warm-up, however many materials come and go), and `epoch` counts the times it grew.

The shadow numbers sum every shadow pass of the frame (cascades, atlas tiles, page clears). Calls
other modules record inside a pass are not counted: the shared terrain draw, the sky, the
visibility-buffer rasters and the translucent shadow casters. The counts are exact for a given scene
and view and do not depend on timing, so a test can assert them (`bindless::frame_tests` does on a
fixed four-quad scene). A texture view's draws are not part of these counts.

Without an app, `cargo run --release -p regia_render --example draw_counts [WIDTH HEIGHT]` prints
the same counters for the hard-surface lookdev scenes (the hull close-up and the 12-hull grid) on a
headless device, with the device's tier.

`render.get_capabilities` reports the render device: `adapter`, `backend`, the optional `features`
the renderer uses, the binding `limits`, the active capability `fallbacks` (the same codes as
`render.problems`), and `bindless`:

```json
"bindless": { "tier": "material_ssbo", "adapter_tier": "texture_arrays", "material_path": "storage_arena",
              "binding_arrays": false,
              "non_uniform_indexing": false, "partially_bound": false, "max_binding_array_elements": 0,
              "storage_buffers_per_stage": 1048576, "max_storage_buffer_binding_size": 2147483644,
              "storage_offset_alignment": 16 }
```

`tier` is the device's: `none` (per-material uniform buffers: fewer than 12 storage buffers per stage,
as on min-spec mobile), `material_ssbo` (material constants in one storage buffer), or
`texture_arrays` (texture binding arrays as well; step 2 uses them). `adapter_tier` is what the adapter
could reach once the renderer asks for its binding-array features. `material_path` is where this
device's material constants actually live: `storage_arena` from tier `material_ssbo` up, except on
Android, which keeps `per_material` on every tier until the storage path is measured on phones. The
device logs the same once at creation (`bindless tier: …; material constants: …`).

Driver workarounds show in `fallbacks` too. `fallback_sun_shadow_pcf_only` (ADR-1749): on a Qualcomm Adreno
adapter the forward shader is composed with `SUN_SHADOW_PCF_ONLY`, so the sun shadow is PCF on the cascades
only, with no near tile and no PCSS soft shadows. The Adreno 643 compiler of a Fairphone 5 crashed on the
full shader at every launch. `REGIA_SUN_SHADOW_PCF_ONLY=1` forces the workaround on for any adapter, so you
can look at it on a desktop; `=0` turns it off on Adreno, to re-test a newer driver. The device logs
`sun shadow: PCF on the cascades only ...` once at creation when it is on. The same workaround also sets
`STUDIO_IBL_OFF` (ADR-2042) and `FORWARD_TRIM` (ADR-3145). With `FORWARD_TRIM` the full shader has no light
from fitted emitters or LTC sphere lights, no surface weather and no aerial haze in the weather in-scatter, and
a pane behind the nearest one keeps its environment reflection. Without that define, the Adreno 643
(Fairphone 5) and Adreno 504 (Galaxy Tab A 8.0) compilers crashed on the opaque and GPU-driven colour
pipelines. The lite Mobile shader is not affected.

Errors: without a `RuntimeRenderer` (`render.get_frame_stats`) or a `RenderDevice`
(`render.get_capabilities`) the call fails with the protocol's not-found error, whose hint says to add
`regia_render::RenderPlugin`. There are no other error codes.

## Bindless level 3: the texture table

At bindless level 3 (ADR-863) every standard material samples its seven textures through one
per-device texture table, and every standard material binds one shared material group: the
material constants arena at the material's own dynamic offset. A pass then binds one material
group whatever the number of materials, and creating or replacing a material creates no bind
group. Graph materials keep their own groups and pipelines.

Level 3 is opt-in while its acceptance runs on the gate's GPUs. Set the level before the device
is created (it changes shader defines and layouts, so it is read once per device):

```sh
REGIA_BINDLESS_LEVEL=3 cargo run --release -p lookdev
```

`0` is the per-material uniform path, `1` (the default) the material constants arena of step 1,
`3` the texture table. `2` and `4` are reserved for the per-draw slot and the single-pass
visibility resolve (ADR-356): today `2` behaves as `1` and `4` as `3`. A device takes the table only when it can:

`render.get_capabilities` reports the decision in `bindless.texture_table`:

```json
"texture_table": { "path": "table", "capacity": 4032, "fallback": null }
"texture_table": { "path": "per_material", "capacity": null, "fallback": "not_requested" }
```

`capacity` is the number of views the table holds: the device's binding-array budget less 64
for the other textures a material pass samples, at most 4096. Off the table, `fallback` names
the first requirement that is missing. The codes are stable:

| Code | Meaning |
|---|---|
| `not_requested` | The adapter could carry the table, but the level is below 3. |
| `uniform_materials` | Material constants use per-material uniform buffers (tier `none`, or Android). |
| `backend` | GL or WebGPU: no binding arrays through wgpu. |
| `no_binding_arrays` | The device has no `TEXTURE_BINDING_ARRAY`. |
| `no_non_uniform_indexing` | The device has no `SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING`. |
| `array_elements` | Fewer than 4096 binding-array elements per shader stage. |
| `bind_groups` | Fewer than five bind groups (the table is group 4). |

The table binds every element (unused ones hold the white view), so partially bound arrays are
not required and Metal qualifies.

`render.get_frame_stats` gains `texture_table` (`null` off the table path):

```json
"texture_table": { "capacity": 4032, "live": 37, "overflows": 0, "rebuilds": 2, "refreshes": 120,
                   "last_rebuild_us": 41, "record_groups": 1 }
```

- `live`: distinct material textures that hold a table index (materials sharing a texture share
  its index).
- `overflows`: texture slots that did not fit the table and sample their neutral view instead (a
  warning is logged once). Above zero means the scene has more textures than the table holds, and
  `render.problems` then reports `texture_table_full` with the counts. A larger table needs no
  rebuild: set `REGIA_TEXTURE_TABLE_VIEWS` (4096 to 65536; the device's budget still caps it)
  before the device is created. Every table rebuild then binds that many views.
- `rebuilds`: table bind groups built. wgpu bind groups cannot be edited, so a texture entering
  or leaving the table means a new table group. The renderer rebuilds it at most once per frame,
  before the first pass binds it, so `rebuilds` never grows faster than `refreshes` (the frames
  that checked it).
- `last_rebuild_us`: CPU time of the last rebuild, for profiling only.
- `record_groups`: shared material groups built, one per material arena buffer (the arena doubles
  when full). It stays flat while materials come and go.

On the table path each pass's `material_batches` is 1 (graph materials add their own runs), and
`bind_group_switches[2]` counts the dynamic-offset changes, one per material change, the same
number the per-material path counts as group changes. A streamed texture changing residency
rewrites 16 bytes of its material's record and marks the table for the next rebuild; the
material's group is not rebuilt.

Known limit at level 3: a graph material with mask decals draws with its graph pipeline and no
decals (the mask-decal variant uses the standard material layout; at level 1 such a draw takes
the standard surface with decals instead). Glass instances apply to standard materials only, as
before. Blended graph materials still cast translucent shadows, through a caster variant over
their own group. The pixels of standard materials are byte-identical to level 1 on lavapipe
(`texture_table::frame_tests`).

### More `seeing.reference_trace` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `max_bounces` (number): Maximum path length (default 12, at most 1024). A cloud field wants 128 or more.
- `clouds` (string): For current: off (default), droplet or raster traces the frame's cloud layer as a medium (ADR-2127, [Path-traced cloud layer](pt-cloud-layer.md)) with the physical phase and albedo, or the raster march's own.
- `aovs` (array): EXR layers to write (default all of specs/27 § 1.2).
- `subsurface` (boolean): For current: `true` traces each material's subsurface scattering with Burley's normalized
  diffusion (ADR-2725); `false` (default) traces the light that lands. With any other `scene` it is `invalid_params`.
  Material graphs are always traced with their own surface code; refusals are listed in `metadata.surface_graphs`.

### More `seeing.compare` parameters

The schema (`agent.schema`) also lists these optional parameters:

- `mode` (string): flip, rmse, ssim or all (default).
- `ppd` (number): Viewer pixels per degree (default 67).
- `map_png` (string): Where to write the FLIP map (default next to b).

## Quality presets (ADR-2433)

`seeing.reference_trace {preset}` runs a named quality table instead of one fixed operating point: `draft`, `preview`,
`final` or `reference`. `reference` is the plain trace (bit for bit); `draft` and `preview` are biased, denoised
previews that `seeing.compare` refuses as the reference. The tables, the built-in denoiser, adaptive sampling, the
caustics modes and the parameters (`preset`, `caustics`, `clamp_indirect`, `firefly_threshold`, `adaptive`, `denoiser`)
are on [Path tracer presets](pt-presets.md).

## The rest of the spec 27 surface

Spec 27 § 5 names eight methods. The two above are on this page; the others have their own pages:

| Method | What it gives | Page |
|---|---|---|
| `seeing.capture_plate` | One named shot (camera, time of day, profile) rendered through the runtime frame to files plus a `regia.plate/1` sidecar | [Plates and the reference overlay](plates.md) |
| `render.set_reference_overlay` | A reference PNG drawn over the live frame: `off`, `blend`, `wipe`, `difference`, `flip` | [Plates and the reference overlay](plates.md) |
| `seeing.temporal_chart` | The five § 2.3 temporal numbers from the chart scene | [Temporal chart](temporal-chart.md) |
| `play.hitch_report` | The § 2.4 hitch numbers: percentiles, frames over 2 × p50, longest frame, device-call counters | [Hitches](hitches.md) |
| `render.set_gi_mode` | Live switch of the GI technique for A/B against a trace | [Dynamic GI](dynamic-gi.md) |
| `render.set_upscaler` | Live switch of the upscaler and its scale | [Upscalers](upscalers.md) |

Two differences from the spec's table, recorded in ADR-5630: `seeing.reference_trace` takes `width` and
`height` (not `size`) and traces the frame's own camera when `scene` is `current` (no `camera` parameter),
and the overlay's `flip` mode is a per-pixel HyAB heat map, not the full FLIP score (`seeing.compare` stays
the number of record). The overlay loads the PNG given in `path`; to overlay a trace, pass the trace's
`preview_png`.

## Capturing and judging a plate

1. Capture the shot at the current pin: `seeing.capture_plate { "shot": { …, "format": "exr" } }`, then
   poll the plate id until its files and sidecar are written ([Plates](plates.md)). The same shot twice gives the same
   frame hash, so a changed hash means the picture changed.
2. Trace the same view: `seeing.reference_trace { "scene": "current", "spp": 1024 }` with the plate's
   camera still applied, and wait for the job.
3. Score it: `seeing.compare { "a": { "job": <id> }, "b": "<stem>_000000.exr" }` for the whole frame, then per
   AOV (the `aov` parameter: `direct`, `indirect_diffuse`, `shadow`, …) where the frame under test has
   that layer, to find which technique carries the error.
4. Look at it: `render.set_reference_overlay { "mode": "wipe", "path": "<preview_png>" }` and move the
   split, then `difference` to see where the two disagree.
5. Judge it. The numbers attribute the error; they do not judge the look. Spec 27 § 3.1: an Opus look
   director writes two to four concrete observations per shot, the verdict table holds no severity 4 row,
   and the owner's eye is final. Never report a look as a count of passing tests.

## What a ★ means

A ★ beside a target in spec 27 § 2 marks a first guess: nobody has measured that row against the reference
yet, or the measurement did not support a fixed bar. A ★ row is reported, not enforced. When a lane measures
it (a run on the § 3.2 acceptance scenes, with the device, the head and the cache state stated), the measured
value replaces the ★ in the spec. A ★ is never removed by lowering the target to what the renderer reaches;
a platform that cannot meet a row gets a measured, platform-specific bound written into the spec (§ 6).
