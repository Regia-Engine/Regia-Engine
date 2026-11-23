# Spec 13 — Renderer (`regia_render`)

## 1. Goals
- One architecture for mobile and desktop; quality scales via **render profiles**.
- Physically based, HDR, linear-light rendering; output matches the glTF reference viewer for
  standard materials (within golden-image tolerance).
- Predictable cost: every pass is a render-graph node with GPU timing; budgets in `06-testing-and-ci.md`.

## 2. Device and backends

| OS | Backend order |
|---|---|
| Windows | Vulkan → DX12 (DX12 only when Vulkan has no hardware adapter; ADR-201) |
| Linux | Vulkan |
| macOS (testing only, ADR-264; release deferred, ADR-009) | Metal |
| Android | Vulkan (required; no GL fallback) |

- `RenderDevice` resource wraps `wgpu::Device` + `Queue` + adapter info + `Capabilities` (features and
  limits actually obtained). Request optional features when present: `TEXTURE_COMPRESSION_BC`,
  `TEXTURE_COMPRESSION_ASTC`, `TEXTURE_COMPRESSION_ETC2`, `TIMESTAMP_QUERY` (+ inside encoders/passes),
  `MULTI_DRAW_INDIRECT_COUNT`, `INDIRECT_FIRST_INSTANCE`, `FLOAT32_FILTERABLE`,
  `RG11B10UFLOAT_RENDERABLE`, `SHADER_EARLY_DEPTH_TEST`, `PIPELINE_CACHE` (only with a cache
  directory). Every request is intersected with what the adapter has, so device creation never
  fails on a feature (`DEPTH_CLIP_CONTROL` and `SHADER_F16` are not requested; nothing uses them).
  As built (ADR-5611): the baseline also asks for `TEXTURE_COMPRESSION_ASTC_HDR`, `CLEAR_TEXTURE` and
  `MULTIVIEW` (`device::baseline_optional_features`, test `requests_exactly_the_offered_features`). On
  Windows the default adapter is ranked by device class first and backend second, so a discrete DX12
  adapter wins over an integrated Vulkan one (ADR-2275).
- Minimum spec (audited 2026-10-09 against vulkan.gpuinfo.org reports 52316/13896/51723 — Pascal
  GTX 1080 Ti, 470→580 drivers, Linux/Windows; 51786/52370 — RTX 3080; 51090 — Mali-G710):
  Pascal and Ampere with the current driver expose *everything* the engine optionally requests
  except ASTC/ETC2, so the desktop floor needs no fallback. Exception, measured on a GTX 1080 with
  580.173.02 (ADR-4937): Pascal has `VK_KHR_acceleration_structure` and `VK_KHR_ray_tracing_pipeline`
  but no `VK_KHR_ray_query`, so wgpu offers no `EXPERIMENTAL_RAY_QUERY` and every ray-query feature
  (sun-shadow trace, ReSTIR GI) takes its named fallback (`hwrt_no_adapter_ray_query`, the screen trace
  or compute BVH, `RestirGiFallback::NoRayQuery`); NGX refuses DLSS on it (`feature_not_supported`). Where a tier lacks a capability the
  renderer runs a working fallback (CPU-decoded BC, RGBA16F HDR, CPU indirect counts, per-mip
  depth pyramids, untimed passes — `RenderCapabilities::fallback_notes` lists the active ones as
  `fallback_*` problems in `render.problems` and the host checklist, informational not errors).
  `MinspecProfile` (`regia_render::minspec`) clamps a device's limits and features to the Pascal
  or mobile tier for testing (`RenderDevice::headless_with_profile`, on lavapipe in CI). The shared
  terrain draw uses 5 bind groups (the virtual-texture colour atlas and the cascade-shadow
  view-projection bind at group 4); wgpu caps `max_bind_groups` at 8 on every backend (Pascal/Ampere's
  raw Vulkan `maxBoundDescriptorSets` is 32, Mali-G710's 8), so no shipped target is below the
  engine's minimum and no bind-group fallback is expected to fire.
- Required on all platforms: compute shaders, storage buffers in vertex+fragment (if a device lacks
  fragment storage buffers, log an error and refuse to start on Android — set as minimum spec).
  As built (ADR-5611): Android opens Vulkan only, and wgpu 30's Vulkan backend always reports
  `DownlevelFlags::FRAGMENT_STORAGE`, so a device without fragment storage buffers never reaches the
  renderer. The device asks for the adapter's own limits; a per-stage storage-buffer count below what the
  engine's layouts need fails at layout creation instead of a logged refusal at start (v0.5.x gap).
  Built (ADR-7211): Android device creation logs and refuses (`RenderError::GpuSelection`) below 8 storage
  buffers per stage or without fragment storage buffers (`storage_floor`, the Mobile min-spec clamp).
- Pipeline cache and background compilation: § 2b. Boot fast path (branding before renderer init): § 2c.
- Surface: per window; format = preferred sRGB format; recreated on resize/scale change/resume. Present
  mode from window settings; `desired_maximum_frame_latency = 2`.

## 2b. Persistent pipeline cache and tiered warm-up (instant-load, ADR-093)

- **Cache.** `pipeline_cache`: a `wgpu::PipelineCache` (`Features::PIPELINE_CACHE`, Vulkan) per process,
  persisted to `cache://pipelines/pipelines-<backend>-<vendor>-<device>.fpc`. The file (`FPCH` v1) holds
  a canonical key — backend, PCI vendor/device, driver name + version, wgpu version, engine shader hash
  (blake3 of every engine shader source — all WGSL of the shader crates plus the Rust files that carry
  WGSL text, baked by `regia_render/build.rs` — + crate version) + the game's salt (`set_content_salt`) — the payload
  length and its blake3. Any mismatch, truncation or checksum failure discards the file (never handed to
  the driver; fuzzed); wgpu and the driver validate their own headers on top, `fallback: true`. Saves are
  atomic (temp + fsync + rename) and happen off the main thread when the warm queue drains, only when
  pipelines were created since the last save.
- **Routing.** Every engine pipeline descriptor passes `cache: pipeline_cache::cache_for(device).as_deref()`;
  the call also counts the creation and attributes it to the running warm tier (or "outside"). The cache is
  installed for one device per process (wgpu device handles from two instances can compare equal); a
  device requests `PIPELINE_CACHE` only when a directory is configured (the windowed app: `cache://pipelines`;
  tools: `REGIA_PIPELINE_CACHE_DIR`; `REGIA_PIPELINE_CACHE=0` disables), so test devices never carry one.
- **Teardown.** The cache holds its device, so the install is not leaked: it ends with the last clone of the
  `RenderDevice` that made it. `WillTerminate` shuts the warm queue down and saves the cache; the device is
  then destroyed when the world drops. Shutdown drops the queued jobs and cancels the running ones between
  their compiles: the engine's warms check `pipeline_warm::warm_cancelled()` at every pass boundary of a warm
  frame, between material-graph sets and between prewarm phases, and return an error (every build,
  `panic = "abort"` shipping included). Unwinding builds also unwind a job at its next pipeline creation.
  Shutdown then joins the workers (≤ 30 s, ADR-276; ≤ 250 ms once the device is lost) and releases
  the queue's device and warmed renderers. On Linux and Android an exit hook (`atexit`, registered after the
  driver's) shuts down any queue still running (a runner without `WillTerminate`, `std::process::exit`) and
  joins workers still compiling, ≤ 30 s in all (ADR-170, ADR-276). On Windows the hook runs when `main` returns but not on
  `std::process::exit` (`ExitProcess` skips the CRT `atexit` list): call `PipelineWarmQueue::shutdown` first there.
  A Vulkan device still alive in the process's exit handlers deadlocked NVIDIA's Linux driver
  (`libEGL_nvidia` exit handler on a PI futex, about 1 exit in 14 on the build machines's 5090).
- **Warm queue.** `PipelineWarmQueue` (render-plugin resource): jobs in tiers `boot → menu → lobby → map →
  optional`, highest first, on worker threads, started right after the first present. The queue starts in
  `WarmMode::Boot` (all cores but two, 1–12; at most 4 on Android and under 8 GiB of RAM) and switches to
  `WarmMode::Play` (¼ of the cores, 1–4) at the first drawn scene frame (ADR-193, ADR-2275).
  A job is any closure over the device; the standard one, `pipeline_warm::frame_job(ExtractedFrame, format)`,
  renders the frame's views once at 64×64 in the window's format with a throwaway `FrameRenderer`, so every
  pipeline that frame configuration uses — passes, post, shadows, material graphs (inline) — is compiled
  into the shared cache. The engine queues the canonical lit frame at boot; a game queues its menu/lobby/map
  frames (DO27: both alpha maps behind the menu). When the frame path later creates the same pipeline the
  driver finds it compiled. Material graphs the frame cannot draw yet fall back to the standard surface
  (`graph_pipelines`). Progress: `PipelineWarmProgress { done, total, tiers: [TierProgress] }`, agent
  `render.pipeline_warm`; a finished job marks its label resident as a `LoadKind::Pipeline` load-set item.
- Replaces the earlier recorded PSO warm-up list (`config/pso_warmup.ron`): a frame job warms exactly
  what a frame configuration draws without a key format to keep in sync.

## 2c. Boot fast path (instant-load, ADR-093)

- `BootSplash` (resource the game inserts): image (RGBA8 sRGB), one line of text in the built-in 5×7 pixel
  font, background, fade-in, fade-out, minimum time. It is presented from the **lifecycle event that creates
  the surface** — before the first game frame — with its own shader module and one pipeline per surface
  format. While it shows, `render_windows` draws neither the scene nor the UI overlay, so the main renderer
  creates nothing on the boot path; `finish()` fades it out over the first real frames. `max_duration`
  (default 1.5 s after process start, fade included; `REGIA_SPLASH_MAX_MS`, `0` = off) caps every engine
  hold (a running game-frame warm, `min_duration`) once the host has called `finish()`; the rest continues
  behind the host's first scene, reported by the `BootBackground` resource (ADR-173). The first-launch
  quality probe draws its own offscreen frames on a warm worker after the splash, never under it. The game-frame warm ends with one frame
  at the window's size, so the first visible frame allocates nothing.
- The warm queue starts after the first present (GPU work for the rest of the renderer is submitted after it).
- Game-frame warm cost (ADR-167): `FrameRenderer::warm_extracted` waits for its async material-graph builds only
  until `pipeline_warm::warm_graph_budget()` (2 s from the warm's start, `REGIA_WARM_GRAPH_BUDGET_MS`); later
  graph sets land on the following frames with the standard surface meanwhile, as on the frame path.
  `warm_scene` (loading screens, benches) still settles every graph. Multi-second driver compiles of one set
  (the forward variants, the GPU-driven variants a frame needs) run concurrently (`pipeline_warm::join` /
  `join_all`, ADR-161); material-graph
  workers are a quarter of the hardware threads, 2..=8 (`REGIA_MATGRAPH_WORKERS`). Shutting them down
  drops queued sets and waits for sets already in the driver, up to 30 s (`REGIA_MATGRAPH_SHUTDOWN_WAIT_MS`);
  the cache keeps a device clone until they are joined, so the device is never destroyed on a worker (ADR-269).
- Every renderer thread that may enter the GPU driver (pipeline-warm, material-graph and encode workers, the exit's
  renderer and device drops; the telemetry probe too) is started through `regia_platform::GpuWorkerBuilder` and
  registered until its body and captures are gone. `regia_platform::finish_exit` (called by `regia::run_with`)
  waits for them within 30 s of the clean exit's start (`REGIA_EXIT_WORKER_WAIT_MS`) and, if one is left, ends the
  process with the app's code without running exit handlers (ADR-287).
- On Linux the first Vulkan instance a process creates is kept (the driver pin, `regia_render::device`), so the
  Vulkan loader loads each GPU driver once per process, not four times for every instance. The clean exit's device
  step releases it once the device is destroyed and its instance has no live wgpu object: the drivers are unloaded
  there, not in the process's exit handlers. A process that never runs the clean exit keeps it until it ends
  (ADR-5794).
- `boot::mark` records the timeline (process start from `/proc/self/stat` on Linux/Android → `plugin_build`,
  `resumed`, `device`, `surface`, `first_present`, `boot_tier_ready`, `menu_tier_ready`, `splash_done`, plus
  game marks via `mark_named`); agent `render.boot_timeline`. Android follows the same path: the splash
  re-presents on every resume until it is done (surfaces are recreated per § 2's lifecycle).
- Targets (DO27): first present ≤ 0.5 s warm, < 1 s cold; measured by `samples/boot_probe`.

## 3. Render profiles and scalability

`RenderProfile` (project setting per platform, overridable by user graphics settings):

| Setting | Mobile Low | Mobile High | Desktop Low | Desktop High | Desktop Ultra |
|---|---|---|---|---|---|
| HDR target | RG11B10 | RG11B10 | RGBA16F | RGBA16F | RGBA16F |
| Render scale | 0.7 | 0.85 | 0.75 | 1.0 | 1.0 |
| AA | none | SMAA 1× | FXAA | TAA | TAA |
| Upscale (scaled, no TAA) | SGSR 1 | SGSR 1 | bilinear | — | — |
| Depth prepass | no | no | yes | yes | yes |
| CSM cascades / res | 1 / 1024 | 2 / 2048 | 2 / 2048 | 4 / 2048 | 4 / 4096 |
| Local light shadows (atlas) | off | 4096², ≤4 | 4096², ≤8 | 8192², ≤32 | 8192², ≤64 |
| Shadow filtering | hardware 2x2 PCF | PCF 3x3 | PCF | PCSS-lite | PCSS-lite |
| Forward fragment shader on tile GPUs (ADR-1874) | lite Low | lite High | full | full | full |
| SSAO | off | off | half-res | GTAO half-res | GTAO full |
| Bloom | off | on (4 mips) | on | on | on |
| Auto exposure | off (fixed EV) | on | on | on | on |
| Reflection probes | 1 global | blended 2 | blended 2 | blended 4 | blended 4 |
| Max lights per cluster | 16 | 32 | 64 | 128 | 256 |

**Non-temporal tiers (ADR-102, lane mobile-aa).** A scaled frame without TAA on an SGSR profile
(`RenderProfile::spatial_upscaler`) runs the post stack at the internal size — exposure, bloom, tonemap,
then SMAA 1× (`AntiAliasing::Smaa`, Jimenez et al. 2012, luma edges, HIGH preset) — and its last pass is
Qualcomm's SGSR 1 edge-adaptive upscale onto the output instead of the 1:1 blit. The bilinear profile keeps
the old order (HDR bilinear upsample, post at the output size). The Deck keeps TAAU;
`RenderProfile::non_temporal()` is its fallback (TAA → SMAA, upscaled by SGSR). Mobile High stays at 0.85:
at 0.75, SGSR + SMAA scored below the old 0.85 bilinear + FXAA preset on all six DO27 plates (SSIM against a
4× supersampled frame), while 0.85 SGSR + SMAA scores above it on all six.

**Screen-space reflections (ADR-103, lane sssr).** Desktop High and Ultra (`FrameQuality::stochastic_ssr()`,
`ssr && gpu_driven`) trace stochastic SSR — a port of FidelityFX SSSR v1.1.4 (MIT) over a single-pass SPD
Hi-Z — at every render scale, including the 0.67 budget; the forward pass writes a surface code into the HDR
alpha for its composite. The lab, editor and probe-capture qualities keep the floor-only linear march.

No preset uses MSAA: the shared (native) terrain draw and the planet sky (sky bodies, airless and Mars skies) run on single-sample HDR frames only, and an MSAA frame skips them (Mobile High moved from MSAA 4x to bilinear+FXAA at look-r2 round 8; the current preset is SMAA 1x at render scale 0.85). A map's exposure band (`LevelLook` `min_ev100`/`max_ev100`) also sets the fixed EV of a profile without auto exposure (Mobile Low): the band centre.

**Small tiers (Deck, Desktop Low)** also run GPU-driven indirect draws and paged (retained) sun cascades. On every shipping preset, static geometry's TAAU motion comes from the depth buffer in one fullscreen pass (`RenderProfile::depth_velocity`): per-draw work does not dominate small frames (look-r2 round 9), and per-pixel camera motion is exact where per-vertex motion interpolates across large triangles (Desktop High and Ultra since round 10).

**Texture memory (ADR-130, lane tex-budget).** Every preset streams material textures under a resident budget
(`TextureStreaming::for_preset`):

| Preset | Resident cap | Budget (streamed material maps) |
|---|---|---|
| Mobile Low / High | 1 K / 2 K | 256 / 512 MiB |
| Deck, Desktop Low | 2 K | 1 GiB |
| Desktop High | 4 K | 2 GiB |
| Desktop Ultra | authored | 4 GiB |

First use uploads up to the cap while the budget has room (frames unchanged); over it the least-recently-seen
materials drop their top mips, visible ones keep their screen-size need. The renderer keeps no CPU copy of uploaded
pixels; `render.problems` reports `texture_budget_exceeded` and `uncompressed_textures` (> 64 RGBA8 maps).
The budgets are each tier's floor: a discrete card's budget grows to a quarter of its VRAM
(`TextureStreaming::vram_scaled_budget_mib`, ADR-189), an integrated GPU's is capped by its heap and RAM (ADR-223).
The profile table and this table are checked cell by cell by `tests/spec13_conformance.rs` (ADR-5611).

A **Deck** profile (Steam Deck: 1280×800, between Mobile High and Desktop Low, 30/40/60 fps caps,
temporal upscale from ~70%) and the **Desktop High/Ultra high-end features** (GPU-driven culling, cluster
geometry, probe GI, paged sun shadows, volumetric clouds/fog, layered materials, HDR output, FSR-class
upscaling) are specified in `specs/26` § 4 and built in M15a–d; the profile table above is the M5 baseline.

Mobile rules: no depth prepass on tile-based GPUs (sort opaque front-to-back instead), keep render
passes merged (avoid store/load between passes — use subpass-friendly ordering: opaque + sky +
transparent in one pass where possible), avoid full-screen reads of depth on Low.

## 3a. The integrated-GPU floor: Very Low and the `integrated_low` budget (ADR-7977, design)

Status: **design** (lane do27-ultralow-design, 2026-11-15). Very Low itself shipped with ADR-7882; the lines marked
**(build B)** were what the second build lane of ADR-7977 adds. Build A (selecting the tier by name) landed with
ADR-8844 (v0.7); build B (the `integrated_low` row, Very Low's streaming column, the agent tier and the ratchet row)
landed with ADR-8878 (v0.7), with the measured N150 rows S1 and S3 (S2 is DO27's). File:line citations are at
44b7f1a39.

**The need.** DO27's minimum spec is an integrated-GPU laptop at 720p and 30 fps, using at most 2.5 GB of RAM and
1.5 GB of GPU memory. It wants an engine level below Low, a memory budget row for that floor and a measured row, so
it can drop its own ultra-low profile.

**The shape: a tier, not a preset row.** The floor is the quality tier `QualityTier::VeryLow` (ADR-7882,
`crates/regia_render/src/quality/mod.rs:59`). It is Desktop Low with two columns rewritten
(`QualityTier::very_low_columns`, `:216-219`), it is not a row of the preset table above, and there is no
`RenderProfilePreset::UltraLow`. `QualityTier::preset()` is Desktop Low (`:200-209`), and
`RenderSettings::preset()` answers `DesktopLow` for exactly Very Low's profile (`crates/regia_render/src/runtime.rs:922-930`).
So water, foliage and the other systems that pick their tier by preset stay on Low's, and Very Low is found again by
`QualityTier::of_settings` (`quality/mod.rs:369-374`). Systems that size work by tier read
`QualityTier::content_tier()`, which is Low (`:143-148`). A new `RenderProfilePreset` variant was rejected; the
reasons and the break cost are in ADR-7977.

**Settings.** Very Low against the rows it comes from. "Same" means Desktop Low's cell
(`crates/regia_render/src/profile.rs:967-1051`), unchanged.

| Setting | Desktop Low | **Very Low** | Mobile Low (comparison only) |
|---|---|---|---|
| Shading path | full forward (`forward_lite` Off) | same | lite Low |
| HDR target | RGBA16F | same | RG11B10 |
| Render scale | 0.75 | **1/3** (`VERY_LOW_RENDER_SCALE`, `quality/mod.rs:96`) | 0.7 |
| Upscale to the output | bilinear | **SGSR 1** (the non-temporal order of ADR-102: post at the internal size, SGSR last) | SGSR 1 |
| AA | FXAA | same | none |
| Temporal upscaler | none (FXAA tier) | none | none |
| Depth prepass | yes | same | no |
| Sun cascades / resolution | 2 / 2048², paged (retained) | same | 1 / 1024² |
| Local light shadows | 4096² atlas, ≤ 8 | same | off |
| Shadow filtering | PCF | same | hardware 2x2 |
| Contact shadows | on | same (0.7 ms on the N150 at a third; first lever below) | off |
| SSAO | half-res | same | off |
| Bloom / auto exposure | on (4 mips) / on | same | off / fixed EV |
| GI | ambient, no SSGI, no probes; a map's bake is drawn unless the player picks Balanced (`runtime.rs:938`) | same | same |
| Terrain virtual texture | off on integrated GPUs (the lever is for discrete GPUs with 6 GiB or more, Linux only, `quality/mod.rs:341-364`) | same rule | off |
| Mesh LOD bias / distance / shadow scale | 0.85 / 1.0 / 0.5 | same | 0.5 / 1.0 / 0.5 |
| Texture streaming budget | 1 GiB, eager first use (`TextureStreaming::for_preset`) | **384 MiB, lazy first use** (ADR-8878; `low_memory::INTEGRATED_STREAM_BUDGET_MIB`, `crates/regia_render/src/low_memory.rs:28`) | 256 MiB |
| Host CPU mesh / texture copies | 512 / 256 MiB (`HostMemoryBudget::for_preset`, lane v051-do27-host-ram-budget, not on this base) | same | 128 / 64 MiB |
| Probe target | 60 fps | 30 fps (`default_target_fps`, `quality/mod.rs:386-392`) | 30 fps |

Why these columns and not DO27's suggested ones (Mobile Low class shading at 0.5–0.6 with a temporal upscaler):

- **Scale and upscaler.** These numbers are from the N150 (Intel N150, 24-EU Xe-LP), Vulkan, at 930×1096 output
  (`docs/perf-log.md:2523-2580`). Low at 0.5 is 43.6 ms GPU p50, Low at a third 28.9 ms, and Very Low (a third with
  SGSR) 22.7 ms, because post moves to the internal size (4.5 ms against Low's fixed ~10.4 ms). The Deck preset at a
  third (TAAU) is 33.2 ms: TAAU alone costs 5.1 ms there. So at this GPU class a temporal upscaler at 0.5–0.6 does
  not hold 30 fps, and a spatial one at a third does with about 10 ms to spare. The measured window is 1.02 Mpx and
  720p is 0.92 Mpx, so the numbers carry over to 720p within about 10 %. At 720p Very Low draws 427×240.
- **Shading path.** Mobile Low at 0.58 was 15.3 ms GPU on the same laptop, but on the v0.2.15 build
  (`docs/perf-log.md:1013`). It has not been re-measured since. Two known defects keep it off the desktop floor for
  v0.5.1: switched live, Mobile Low draws a white frame (it has no auto exposure and the raw preset switch sets no
  fixed EV), and the lite forward shader on the desktop path draws tree shadows as detached wedges
  (`docs/perf-log.md:2552-2554`). The measured-row plan below keeps Mobile Low at 0.58 as a comparison row. A
  Mobile-Low-class desktop floor is a later decision, once both defects are fixed and the row is re-measured.
- **Contact shadows, SSAO and the rest.** These stay as on Desktop Low. Very Low already has its margin, and changing
  a column would invalidate ADR-7882's measured row. If DO27's own content misses 30 fps at Very Low on the N150,
  the levers are, in order, each one a Very Low column measured alone: contact shadows off (0.7 ms), SSAO off, HDR
  RG11B10, mesh LOD bias 0.7 (Mobile High's), then masked occlusion culling (job do27-masked-occlusion-low). None is
  in the v0.5.1 build lanes unless the measured row fails.

**Selecting it.**

- From code, today: `QualityTier::VeryLow.apply_for_gpu(&mut settings, adapter)`, or
  `GameSettings.graphics.preset = GraphicsPreset::VeryLow` (`crates/regia_framework/src/settings.rs:40`).
- Auto picks it in two ways. The GPU table's `intel_n_series` and `intel_n_series_adl` rows pick it directly. The
  confirmation probe steps Low down to it only when Low misses the lower of the target and 30 fps
  (`QualityTier::step_down`, `quality/mod.rs:169-178`; ADR-7882). Auto never forces it elsewhere. On other
  integrated GPUs (Iris Xe, UHD, Radeon 610M) Auto still picks Low until a measured table row says otherwise.
- By name (ADR-8844), wherever a preset is named: `render.set_profile {"preset": "VeryLow"}`
  (`crates/regia_agent/src/render_profile.rs:108-127`), `--render-profile VeryLow` / `REGIA_RENDER_PROFILE` /
  `render_profile.txt` (`crates/regia_render/src/cinematic.rs:232-237`, `:415`), and Regia.toml
  `[platforms.*] render_profile = "VeryLow"` (`crates/regia_cli/src/manifest.rs:476-490`), which reaches the packaged
  game's `build_info.json` reader (`PackagedConfig::render_profile_name`), the editor's Project Settings, and the
  showcase's `--render-profile`. A name that is a preset
  resolves to the preset, as today. Only the ladder tiers that have no row of their own (`VeryLow`, `Medium`) are
  added. `Low`, `High` and `Ultra` stay unknown names, so no row gets a second name. `render.get_profile` gains
  `tier` (`QualityTier::of_settings`, `null` for a customized profile); its `preset` stays `DesktopLow` for Very Low.

```rust
/// A name given where a render profile is chosen (ADR-7977): a row of the preset table, or a quality-ladder
/// tier that has no row of its own and is its preset with some columns rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileName {
    /// A preset row ([`RenderProfilePreset::from_name`]).
    Preset(RenderProfilePreset),
    /// A tier of [`ProfileName::TIERS`].
    Tier(QualityTier),
}

impl ProfileName {
    /// The tiers accepted as profile names besides the presets: the ones with no row of their own.
    pub const TIERS: [QualityTier; 2] = [QualityTier::VeryLow, QualityTier::Medium];

    /// Parses `name` as [`RenderProfilePreset::from_name`] does (case-insensitive, `_`/`-` ignored); failing
    /// that, one of [`Self::TIERS`] by [`QualityTier::from_name`]. `Low`, `High` and `Ultra` are `None`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self>;

    /// The canonical name (`"DesktopLow"`, `"VeryLow"`).
    #[must_use]
    pub const fn name(self) -> &'static str;

    /// The preset row it starts from (Very Low: Desktop Low).
    #[must_use]
    pub const fn preset(self) -> RenderProfilePreset;

    /// The quality tier it names, `None` for a preset row.
    #[must_use]
    pub const fn tier(self) -> Option<QualityTier>;

    /// Applies it live: [`RenderSettings::apply_preset`] for a preset, [`QualityTier::apply_for_gpu`] for a tier.
    pub fn apply_for_gpu(self, settings: &mut RenderSettings, adapter: Option<&AdapterProfile>);

    /// [`Self::apply_for_gpu`] under a host's [`TerrainVtPolicy`] (a tier's lever only).
    pub fn apply_for_gpu_with(self, settings: &mut RenderSettings, adapter: Option<&AdapterProfile>,
                              policy: &TerrainVtPolicy);

    /// Every accepted name, presets first ([`RenderProfilePreset::WITH_OFFLINE`], then [`Self::TIERS`]).
    #[must_use]
    pub fn names() -> Vec<&'static str>;
}

/// [`choose_preset`] with tier names accepted (ADR-7977). `choose_preset` keeps refusing them with
/// `render_profile_unknown`, so existing hosts see no change. Its unknown-name error is
/// `PresetChoiceError::UnknownProfile` (same code; the message lists `ProfileName::names()`), and
/// `take_profile_arg`, `parse_profile_file` and `profile_from_scene_dir` are the tier-aware twins of the
/// other preset readers (ADR-8844).
pub fn choose_profile(
    command_line: Option<ProfileName>,
    env: Option<&str>,
    scene_dir: Option<&Path>,
    default: RenderProfilePreset,
) -> Result<ProfileChoice, PresetChoiceError>;

/// A host's chosen profile name and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileChoice {
    /// The name.
    pub profile: ProfileName,
    /// Where it came from.
    pub source: PresetSource,
}
```

**The memory budget row (ADR-8878).** `BudgetTier` (`crates/regia_render/src/memory_budget.rs:16-24`) gains one
variant, appended so that `ALL`'s order and every report keyed on it keep their first three entries:

```rust
pub enum BudgetTier {
    // Minimum, Deck, DesktopHigh unchanged (Minimum's doc becomes "the discrete-GPU minimum: RTX 2080 class").
    /// The integrated-GPU floor (ADR-7977): an 8 GB shared-memory laptop at 1280×720, the tier
    /// `QualityTier::VeryLow` is sized for. RAM 2.5 GiB, GPU memory 1.5 GiB.
    IntegratedLow,
}
// BudgetTier::ALL: [Self; 4] = [Minimum, Deck, DesktopHigh, IntegratedLow]; name "integrated_low".
// MemoryBudget::for_tier(IntegratedLow) = MemoryBudget { ram: 2560 * MIB, vram: 1536 * MIB }.
// frame_work ratchet: tier_resolution = (1280, 720); tier_preset = DesktopLow (Very Low's internal targets are
// a ninth of the output against Low's 0.56, so Desktop Low at 720p is an upper bound on the counted bytes).
// baked_gi_budget(IntegratedLow) = vram / 8 = 192 MiB (the large-map layout is 51 MiB, the sizing ceiling 100 MiB).
// ClusterResidency::for_tier(IntegratedLow) (crates/regia_render/src/visbuffer/residency.rs:99-104, the other
// exhaustive match) = half of Deck's row: 128 MiB VRAM, 4 MiB uploads, 128 MiB RAM, 16 MiB reads per frame
// (★ until measured; cluster residency is OFF at Desktop Low, profile.rs:960, so it bounds only a host that
// turns it on).
```

The budget is what the game process may use, counted as `MemoryBudget` already counts it: RSS, and the device's
allocator total (`MemoryUse::vram`, `memory_budget.rs:147`; on Vulkan and D3D12 these are the allocator's reserved
blocks, not the allocated bytes). The units are GiB, as for the other rows and as Windows Task Manager shows memory.
The arithmetic:

| The floor machine (8 GB, integrated GPU) | GiB | Source |
|---|---|---|
| Game RSS | 2.5 | DO27's floor |
| Game GPU memory (carved from the same RAM) | 1.5 | DO27's floor |
| Everything else: firmware graphics carve-out, OS, compositor (its copy of a windowed swapchain), Steam client, the GPU driver's kernel side | 4.0 | the remainder; **not measured** (the build machines has no 8 GB integrated-GPU host) |

| GPU memory, 1.5 GiB at Very Low 1280×720 | MiB | How |
|---|---|---|
| Swapchain, 3 × 1280×720 BGRA8 | 11 (26 borderless on a 1920×1200 panel) | arithmetic |
| Render targets: internal 427×240 (HDR RGBA16F, depth, velocity, half-res SSAO) plus output-size post and SGSR | ≤ 32 | arithmetic |
| Sun cascades, 2 × 2048² at 4 bytes | 32 (plus retained pages) | `profile.rs:972-973` |
| Local shadow atlas, 4096² at 4 bytes | 64 | `profile.rs:974` |
| Texture streaming | 384 | Very Low column (ADR-8878) |
| Baked GI | ≤ 192 | `baked_gi_budget` rule (`crates/regia_render/src/baked_gi/memory.rs:39-41`) |
| Fixed lines total | ≈ 715 | |
| Meshes, terrain tiles, instance buffers, upload belts, allocator slack | ≤ 820 | the rest |

Measured anchor: the showcase scene at Desktop Low, 1280×720, on the N150 used 651 MB of GPU shared memory and a peak
working set of 920 MB (`docs/perf-log.md:2499`). Its allocator had 227 MB allocated against 537 MB reserved
(`:1010`), which is why the budget counts the reserved blocks.

| RAM, 2.5 GiB | MiB | How |
|---|---|---|
| The showcase scene's whole process at Desktop Low 720p on the N150 (engine, driver user-mode, its content) | 920 | measured peak working set (`perf-log.md:2499`) |
| Host CPU copies of meshes and textures, at most | 768 | `HostMemoryBudget::for_preset(DesktopLow)` (lane v051-do27-host-ram-budget): 30 % of the row |
| Everything else in the game: simulation, netcode, audio, UI, touched pak pages, the Steam overlay (it runs inside the game process) | ≈ 870 | the rest; DO27's to measure |

**Low-memory wiring (ADR-8878).** On an integrated GPU with less than 15,000 MiB of RAM, the integrated-GPU limits
(ADR-223) already cap the texture budget at 384 MiB and turn off the new-scene warm (`low_memory.rs:51`,
`:135-137`, applied per frame in `crates/regia_render/src/frame/steps/begin.rs:66-71`, `:118-123`). A 16 GB
laptop such as the N150 host is "large" by that rule and gets Desktop Low's 1 GiB. So Very Low carries the cap as
its own column: `texture_streaming.budget_mib = min(budget_mib, INTEGRATED_STREAM_BUDGET_MIB)`, `eager = false`.
The tier then fits its row on any machine. This needs no new mechanism. The column is a profile field, and the
whole-profile comparison in `RenderSettings::preset()` (`runtime.rs:928`) and `of_settings` covers it, so
`preset()` still answers Desktop Low (profile-field-breaks-preset). A discrete GPU at Very Low (the probe's step)
still grows the budget to a quarter of its VRAM (`TextureStreaming::vram_scaled_budget_mib`, applied at
`begin.rs:113-116`). The 15,000 MiB rule and `REGIA_INTEGRATED_LIMITS` are unchanged.

**Pairing.** A host that selects Very Low admits loads against `MemoryBudget::for_tier(BudgetTier::IntegratedLow)`.
Nothing selects the budget tier automatically: `render.memory_budget` keeps `minimum` as its default and accepts
`{"tier": "integrated_low"}` (`crates/regia_agent/src/loading.rs:339-345`).

**The measured row (build B prepares it; the lead and the Windows host run it). Status: S1 and S3 measured on
2026-11-20, both backends pass** (ADR-8878; `docs/perf-log.md` "N150 integrated_low row": Very Low frame p50 31.8 ms
Vulkan / 20.0 ms DX12, GPU p50 20.3 / 11.7 ms, peak working set 1779 / 1438 MiB, peak GPU process memory 676 / 661
MiB, 10-minute growth negative). S2 is DO27's.

- Host: the min-spec laptop (Intel N150, `<build-dir>` only, `<build-dir>`). Runs use session-1
  interactive scheduled tasks, as in the N150 rounds of `docs/perf-log.md`. Its 16 GB of RAM cannot show memory
  pressure, so the row proves the game's footprint against the budget, not its behavior inside 8 GB. It runs with
  `REGIA_INTEGRATED_LIMITS=1`, which a machine under 15,000 MiB gets without asking.
- Build: the package layout. A Windows release `showcase.exe` (MSVC, built on the Linux build host with cargo-xwin as
  in ADR-7882's acceptance) goes into the demo's twin package and is started as the package starts it
  (`showcase.exe --demo content\scene --render-profile VeryLow`, build A, ADR-8844). Vulkan, then DX12
  (`REGIA_GPU_INDEX=1`, DXC on `PATH`), one launch each.
- Output: `--size 1280x720`. The laptop's desktop has clamped every window to 930×1096 so far. If it does again, the
  row records the presented size. At 1.02 Mpx that is more pixels than 720p, so a pass there is conservative.
- Scenes:
  - S1, the forest demo scene (byte-identical to the min-spec scene): Auto, Very Low, and Mobile Low at 0.58 as the
    comparison row.
  - S2, a DO27 map, run by DO27 in its own shell build with Very Low selected (preferred: its content, its shell).
    The alternative, the engine's `do27_chase` on DO27 content copied to the laptop, needs DO27's and the lead's OK.
  - S3, S1 for 10 minutes with memory sampled every second (growth).
- Measured per row: a 60 s `play.hitch_report` (5 s warm-up; frame p50 and p99, GPU p50, top passes). Peak working
  set and private bytes, and peak `GPU Process Memory` (dedicated plus shared), from the OS counters each second.
  `render.memory_budget {"tier": "integrated_low"}` at the start, at 30 s and at the end.
- Pass:
  - GPU p50 ≤ 30 ms and frame p50 ≤ 33.3 ms. p99 is recorded but not gated, because the 50 Hz panel snaps DX12
    intervals to 20 and 40 ms.
  - Peak working set ≤ 2560 MiB and peak GPU process memory ≤ 1536 MiB.
  - The agent's `use` stays under both budget lines.
  - S3 grows less than 64 MiB between minute 2 and minute 10.
- Recorded in `docs/perf-log.md` as "N150 integrated_low row", with binary hashes, plus a row in the DO27 host
  checklist.

**Build split.**

| Lane | Hours | Adds | Acceptance in one line |
|---|---|---|---|
| A: do27-very-low-profile-names | 4 | `ProfileName`, `choose_profile`, the agent's and Regia.toml's tier names, `render.get_profile` `tier` | `render.set_profile {"preset":"VeryLow"}` gives tier Very Low with `preset` still `DesktopLow`; `Low` stays an unknown name |
| B: do27-integrated-low-budget | 5 | `BudgetTier::IntegratedLow`, Very Low's streaming cap, `integrated_low` in `render.memory_budget`, the ratchet row, the N150 row kit | Very Low streams at most 384 MiB on a 16 GB integrated machine; the N150 row passes the lines above |

A and B are independent; A landed with ADR-8844 (v0.7), so B's measured row selects Very Low with
`--render-profile VeryLow` (or `demo.set_quality`).

**Open questions (ADR-7977; recommended answers given).** (1) Owner: is 720p/30 fps in 2.5 GiB RAM and 1.5 GiB
GPU memory an engine promise, or DO27's? Recommended: the tier and the budget row are the engine's (every game gets
them; Auto already picks the tier), the promise is per measured scene only: the engine publishes the N150 row for the
forest demo scene, and DO27 owns the row for its maps. (2) DO27: must its next alpha select the floor by name? If yes,
lane A ships on v0.5.1; if DO27's shell can call `QualityTier::VeryLow.apply_for_gpu` or set
`GraphicsPreset::VeryLow`, lane A can move to the next line. Recommended: ship A on v0.5.1 (it is small and lets DO27
delete its own profile). (3) Owner: a real 8 GB integrated-GPU laptop for the build machines? Recommended: not for v0.5.1; the
N150 row proves the footprint, and a purchase only if DO27's testers report paging on 8 GB machines.

## 4. Render graph

- `RenderGraph`: nodes declare inputs/outputs (textures, buffers) by name, create transient resources
  with descriptors, and record into a `wgpu::CommandEncoder`. The graph:
  - topologically sorts nodes, culls nodes whose outputs are unused,
  - allocates transient textures from a pool keyed by descriptor (reuse across frames; aliasing of
    non-overlapping lifetimes = Tier 2),
  - records each live node, in compiled order, into the one caller encoder,
  - wraps each node with a CPU tracing span named `render_graph_node`.
- Sub-graphs per **view** (camera): main camera, editor viewports, render-to-texture cameras, shadow
  views, reflection probe capture.
- Plugins (UI, debug draw, editor overlays, VFX) insert nodes at named slots: `after_opaque`,
  `after_transparent`, `before_post`, `after_post`, `overlay`.
- **Pass extension point** (ADR-1552): a host implements `FramePass` and calls
  `FrameRenderer::register_pass`; the frame records it into its own encoder at its slot, in registration
  order (`after_opaque`, `after_transparent` and `before_post`: the scene-referred linear HDR image, on
  frames with one; `after_post` and `overlay`: the display image). `FRAME_STEPS` is the frame's
  recording order with the slots; `render.graph` publishes it with the registered passes' counters and the
  last GPU timings (`book/agents/render-graph.md`). A frame with no registered pass is unchanged. The
  render passes live in `frame/<pass>.rs` modules as they move out of `render_into`; `frame.rs` has a
  written ceiling of 10,000 lines. Each built-in `FRAME_STEPS` row names the module that records it, and
  tests keep the rows honest: the pass modules and the rows name each other, and the rows compile as a
  `RenderGraph` over versioned resources in recording order with every pass live (a declared write
  nothing reads, or a missing read, fails). Since ADR-2018 the frame keeps one versioned `RenderGraph`
  of `FRAME_STEPS` plus the registered passes (rebuilt when a pass is registered or removed): it gives
  the registered passes their order at each slot, its live nodes' pipeline keys (`FrameStep::Builtin`
  `pipelines`) are what the game-frame warm builds, and `render.graph` publishes its versioned nodes.
  The frame's linear HDR target, the refraction copy, the glass front layer and the SSAO targets come from a per-view
  `TransientPool` keyed by `TextureDesc` (the same requests get the same textures every frame, so
  frames are unchanged). `RenderGraphNode::record` takes a `GraphContext`. The built-in passes still
  record in `render_into`'s fixed order rather than through node callbacks, and transients are not
  yet aliased (ADR-2018 lists the remaining steps).

Persistent buffer audit (ADR-8065, v0.5.1): `RenderGraph::declare_buffer` names an owner and
whether version zero is initialized outside the graph. A declared non-imported buffer read
before its writer fails with `graph_read_before_write`. The frame declares its light and
cluster buffers, CPU draw list and DI/GI current/history/temporal reservoirs; allocation stays
with their existing owners. `render.graph` appends node `buffer_reads`/`buffer_writes` and
`graph.buffers` with owners, allocation bytes, generations and access lists. CPU capacity bytes
are separate from GPU allocation bytes. Legacy resource fields stay unchanged. History imports
are distinct logical roles of the two physical parity buffers, not duplicate allocations.
The ownership and resize/cache audit is recorded in ADR-8065.

## 5. Frame structure (per camera view)

1. **Extract** (main → render world): cameras, visible-candidate meshes (mesh handle, material handle,
   transform, previous transform for motion vectors, skin joint palette ref, LOD bias, layers),
   lights, probes, environment, post-process volumes, debug lines, UI draw lists. Mesh entities come
   first, then the host's keyed draws from the `DrawList` resource in key order, with no entity per
   draw; a keyed draw's object key is a fixed function of its key (ADR-8087). A keyed draw with a LOD
   chain is LOD-selected after extraction like a chained entity, its hysteresis keyed by its draw key
   (ADR-8457).
2. **Prepare**: upload changed per-object data into a persistent `ObjectBuffer` (storage buffer;
   indices stable per entity), upload lights, build skin palettes buffer, update material buffers.
3. **Cull** (CPU, parallel): frustum + layer mask culling against bounding spheres/AABBs using a
   flat array (Tier 1). LOD selection by projected screen size. Produces draw lists per pass.
   GPU-driven culling with Hi-Z occlusion and indirect draws: M15a (`specs/26` § 4).
4. **Sort & batch**: opaque by (pipeline, material, mesh) for state; additionally front-to-back
   bucketed on mobile. Transparent back-to-front. Identical (mesh, material) runs become one
   instanced draw (instance data = object index).
5. **Shadow passes**: CSM for the primary directional light (stable cascades: snap to texels, fixed
   sphere bounds; cascade split = practical split λ=0.75 over at most 4 km, splits capped on a
   16 m – 300 m geometric ladder (a two-cascade layout caps its one split at 126.491 m) (16 m first cap: a character 1–5 m away resolves its shadow, ADR-115 dyn-light), light eye up to 2 km up-sun for casters, ADR-061); spot lights perspective, point lights cube
   via 6 atlas tiles; atlas allocation by screen-space importance; static shadow caching for lights
   flagged static (re-render only when something in their bounds moves — Tier 1 for spot/point).
   Desktop High/Ultra add a near sun tile: an orthographic sun view over the first 4 m of the
   view in the atlas's first square (2048² / 4096²), sampled by mesh receivers in that range
   instead of cascade 0, terrain shadow from cascade 0 above the tile's topmost surface; skipped
   when no mesh is within 4 m (ADR-164).
6. **Depth prepass** (desktop) + **motion vectors** (when TAA): writes depth and velocity.
7. **Light clustering** (compute): 16×9×24 froxels (exponential Z); assigns point/spot lights to clusters (reflection probes are one camera-chosen list, capped at 4, weighted per pixel); produces index lists in storage buffers. A shared draw (the terrain) reads
   the same lists through `SharedPassDraw::set_engine_lights` and `shared_local_lights_wgsl`
   (ADR-211 terrain local lights).
8. **SSAO** (desktop, compute): per the profile table (Low half-res kernel, High GTAO half-res, Ultra GTAO full-res) + bilateral upsample; the AO term scales indirect diffuse and also indirect specular (specular occlusion, sky-reflection visibility).
9. **Opaque pass**: forward+ shading (§ 6). Writes HDR color (+ depth on mobile).
10. **Sky**: procedural sky (Hillaire-style atmosphere, LUT-based, Tier 1 simple version) or cubemap.
11. **Transparent pass**: forward, sorted; supports alpha blend, additive, premultiplied; refraction
    via copied scene color (desktop High+).
12. **Post** (compute where possible): TAA resolve (desktop) → camera lens, opt-in per preset and
    camera (ADR-112): motion blur along the TAA velocity (capped at 1/8 of the frame height, more
    samples on long blurs; ADR-1152), then depth of field (thin-lens circle of confusion, capped at
    128 px, tile max dilated by its radius; Gaussian or hexagonal bokeh) → auto exposure (luminance histogram,
    compute) → local exposure, opt-in per map (bilateral grid of the exposed log luminance, base
    layer scaled toward middle grey by a shadow / highlight contrast, detail kept; ADR-212 local
    exposure) → bloom (dual-filter down/up, threshold-less energy-conserving; levels halved rounding
    up and pixel-aligned, a [1 3 3 1] tent down, bilinear up, so a highlight keeps its energy and
    centroid, ADR-1152) → tonemap (AgX default;
    ACES-fitted and Neutral options) + color grading (analytic AgX plus an optional 3D LUT (identity 2³ when absent, edge at most 64), white balance, contrast, saturation,
    lift/gamma/gain) + vignette + film grain (off by default) → FXAA (if selected) → upscale to
    output (bilinear in M5; temporal upscaling in M15a, `specs/26` § 4) → sRGB output.
    **Per-map look** (ADR-046): `LevelSettings.look` (scene format v3) sets the exposure bias,
    the auto-exposure EV clamp, the meter (percentile window, centre weight), contrast,
    saturation and the grade (white balance in kelvin + tint before the tonemap, lift / gamma /
    gain after it); `None` fields keep the view's `PostSettings`. The runtime applies it to every
    view; agents use `level.get_look` / `level.set_look`. `look.lut` (scene format v4) loads a
    `.cube` or PNG-strip 3D LUT (≤ 64³, RGBA16F texture, trilinear) applied after the grade;
    identity when unset. `look.local_exposure` (scene format v9) turns local exposure on.
13. **Overlays**: UI, debug draw, editor gizmos/grid/selection outline (editor only), stats overlay.

## 6. Shading model

- **Standard material** (metallic–roughness, glTF-compatible): base color (+texture, vertex color),
  metallic, perceptual roughness, normal map (+scale), occlusion (+strength), emissive (+HDR intensity
  in nits-ish EV units), alpha mode (Opaque, Mask(cutoff), Blend, Premultiplied, Additive),
  double-sided, unlit flag, UV transform. Tier 1.5 extensions: clearcoat, sheen, transmission (thin),
  anisotropy, specular — following glTF KHR_materials_* definitions.
- BRDF: GGX/Trowbridge-Reitz NDF, height-correlated Smith visibility, Schlick Fresnel; Lambert diffuse
  (Burley optional on Ultra); energy compensation for multi-scattering via a precomputed DFG LUT
  (generated at startup by compute or embedded).
- Image-based lighting: split-sum with prefiltered specular cubemap (mip = roughness) + SH9 diffuse.
  Global environment from the level's `Environment` asset; local **reflection probes** (box-projected,
  baked in editor, blended by cluster). Specular occlusion from AO.
- Lights: directional (max 4, one with CSM), point (radius, inverse-square with windowed falloff),
  spot (inner/outer cone), all with intensity in physical units (lux / lumen / candela) and color
  temperature option. As built (ADR-5611): the frame draws one directional light, the CSM sun
  (`ExtractedFrame::directional` is one value; with several, extraction keeps the lowest entity); more
  directional lights are a v0.5.x gap. Built (ADR-7211): up to four; the sun (the lowest-entity light, unchanged)
  keeps the cascades and three more in entity order shade unshadowed with the sun's BRDF in the forward,
  visibility-buffer and Mobile paths (`ExtractedFrame::lights_views`); a fifth is refused and logged once; probe GI,
  bakes and the path tracer see only the sun. Built (ADR-7254): the terrain's shared draw and grass shade them too
  (`SharedFrameInfo::extra_directional`, unshadowed), the probe GI update lights its hits with them (unshadowed), and
  `regia_pt` scenes extracted from a frame or export carry them (`PtScene::extra_suns`) into the path tracer, the probe
  bakes and both lightmap bakes, lit like the sun; still sun-only: scatter impostors, water and bakes from a
  `.bake.ron` description (one sun; a format change). Built (ADR-7818): `.bake.ron` v3 lists up to four suns
  (`suns: [..]`; the first is the scene's sun, the rest `PtScene::extra_suns`), so bakes from a description take
  every sun; v0-v2 `sun` migrates to a one-element list in memory and on disk (`regia upgrade-project`), byte-identical
  bakes; still one sun: the level export (`level.export_bake`) and the editor's bake panel. Light "IES profiles" Tier 2. Area lights (LTC) Tier 2. Built (ADR-2592): IES LM-63
  profiles (`LightProfile`, cooked `FIES` v1, bands in the emitter probe atlas, forward and path tracer) and
  authored `RectLight` / `TubeLight` on the fitted-emitter LTC path, shadowed by a static distance probe. `LightSourceLtc`
  shades a sized point or spot through the same tables (opt-in; the representative point stays the default).
- Hidden lights (ADR-7670, v0.5.1; the v0.6 line's ADR-7490 / ADR-7549 for the sun): `DirectionalLight`, `PointLight`,
  `SpotLight`, `RectLight` and `TubeLight` require `InheritedVisibility` (derived, never saved), and extraction skips a light
  whose resolved visibility is hidden (its own `Visibility::Hidden` or a hidden ancestor) with the draw filter
  (`regia_render::visibility::is_drawn`): no light-list entry, so no shadow-atlas slot, no cascades, no fog pick and no
  area-light input; the sun pick (lowest entity) runs over drawn lights only. `LayerParked` still filters first.
- Exposure in EV100; physical camera settings optional (aperture/shutter/ISO) for auto-exposure range.
- Fog: exponential height fog (M5); volumetric fog (froxel, desktop) in M15c (`specs/26` § 4).
- **Character shading models** (batch v020, Tincal B3/B4/B6, ADR-238): `MaterialShading` selects skin (pre-integrated
  scattering from per-pixel curvature, per-channel diffuse normals, a second specular lobe, thin-skin transmission),
  eye (refracted iris under a wet cornea, limbal ring, socket occlusion) or standard, plus an optional Charlie sheen
  lobe. Carried by a renderer side table keyed by material key and packed into five vec4s of the material uniform
  (zeros = standard); no pipeline variant. Not yet: separable screen-space SSS, wrinkle maps, sheen glTF import.
- **Subsurface scattering** (ADR-2049): `LabMaterial::subsurface`, `.material.ron` v6 `subsurface`, `RenderProfile::subsurface_scattering`
  (off in every preset). Scattering draws go first, in their own pass with two more targets (`SUBSURFACE` forward variant); two 1D blur
  passes add their light to the HDR image before the forward pass loads it. Details: `book/agents/subsurface.md`.
- **Order-independent transparency** (ADR-2065): `RenderProfile::order_independent_transparency`, off in every preset. Alpha-blended and
  premultiplied meshes accumulate through the `OIT` forward variant into pooled targets in the middle of the forward pass, and a composite
  lays them over the opaque image before the refraction copy and the rest of the blended run. The composite writes the reactive mask TAAU,
  TSR, FSR 3 and a vendor upscaler read. Particles keep their radix-sorted layers. Details: `book/agents/oit.md`.
- **Thin film** (ADR-1977): `LabMaterial::thin_film`, `.material.ron` v4 `thin_film`, glTF `KHR_materials_iridescence`.
  The base Fresnel becomes the Airy reflectance of one film, averaged over the spectrum in closed form
  (`shaders/thin_film.wgsl`, shared with `regia_pt`). A `THIN_FILM` forward variant with its own group 3: a material
  without a film keeps its pipelines and bytes. The shipped pass draws the film only when the surface binds nothing else in group 3:
  not a transmissive surface, a masked instance, a lightmap receiver, a material graph, per-instance glass, an x-ray override,
  or an opaque or masked draw on a Mobile lite tier (those skips increment `ThinFilmStats::unsupported`). `transmissive`,
  `masked` and `lightmapped` on that struct stay 0. ADR-2732 (proposed) describes combined routes; they are not drawn.
- **Rough transmission** (ADR-2075): `LabMaterial::transmission_roughness`, `.material.ron` v7
  `transmission_roughness`. A transmissive surface keeps the clear sheet's weight and spreads the direction over the
  GGX reflection lobe mirrored through the sheet (`shaders/rough_transmission.wgsl`, shared with `regia_pt`). The
  refraction pass averages 16 lobe directions over its copy of the opaque frame, so it needs a frame that refracts;
  elsewhere the pane draws clear. No pipeline variant, and the material block keeps its bytes (`hair_a.x` of a material
  without hair). Not with hair or a material graph.

## 7. Materials and shaders

- Shader source = WGSL composed with naga_oil. Engine library modules: `regia::view`,
  `regia::mesh`, `regia::pbr::lighting`, `regia::pbr::surface`, `regia::shadows`,
  `regia::clustering`, `regia::utils`.
- **Custom materials**: a material shader implements one function, the rest of the pipeline is
  provided by the engine:
  ```wgsl
  #import regia::pbr::surface::{SurfaceInput, Surface, default_surface}
  @group(2) @binding(0) var<uniform> params: MyParams;
  fn material_surface(in: SurfaceInput) -> Surface { var s = default_surface(); s.base_color = ...; return s; }
  // optional: fn material_vertex_offset(in: VertexOffsetInput) -> vec3<f32>   (wind, waves)
  ```
  Material asset (`.material.ron`): `shader: AssetRef<Shader>` (or `Standard`), parameter values
  (floats, vectors, colors, textures with sampler settings), render state (alpha mode, cull mode,
  depth write/test, blend override, render queue offset), shader defines (feature toggles → permutations).
  Parameters are reflected from the WGSL uniform struct via naga at import → inspector shows them.
- Bind group layout (fixed contract): group 0 = view (camera, time, exposure, lights, clusters,
  shadows, environment), group 1 = per-object storage buffers (object data, skin palettes),
  group 2 = material, group 3 = pass-specific.
- The forward shader's own helpers live in `regia::forward::{ltc, occluder_geometry, occlusion, util}`
  (`shaders/forward_*.wgsl`, ADR-1552). Its feature axes are a closed list, `FORWARD_AXES` (28 defines
  on 2026-11-10; each axis lands with its ADR, e.g. `LIGHTMAP` ADR-1623, `THIN_FILM` ADR-1977, `FORWARD_LITE_*`
  ADR-1874; checked against the sources by `the_declared_axes_are_exactly_the_defines_the_sources_test`); every composed define set is recorded and `render.graph` reports the count
  against `FORWARD_PERMUTATION_BUDGET`.
- Permutations: keyed by (shader, defines, vertex layout features, pass, MSAA samples, target formats);
  pipelines compiled asynchronously where possible — objects whose pipeline isn't ready are skipped
  (not drawn with a wrong shader) and a dev warning is counted; PSO warm-up list (§ 2) prevents hitches.
- **Shader-permutation policy** (master plan § 5.5a, ADR-2026): material graphs are the open axis
  (one pipeline set per graph code and target). The cook enumerates every graph pipeline set of
  every render profile the platform runs (`permutations.json` in the cooked cache, one summary line
  per profile in the cook report) and checks a pipeline count and an estimated cold compile time
  per profile; `regia package` fails a shipping package over budget (`Regia.toml`
  `[cook.permutation_budget.<Profile>]` overrides). A package ships the list as
  `content/permutations.json` and `RenderPlugin` installs a `PermutationGuard` at startup (a dev run
  reads the cooked cache; `REGIA_PERMUTATION_POLICY` overrides the mode; a host's own
  `FrameRenderer::set_permutation_guard` wins). The guard decides on the graph code: a listed graph
  builds on any target (`off_target` counts what the list did not count), an unlisted one is built
  with a warning (`warn`, dev) or never built, its draws keeping the standard surface of the same
  pass (`fallback`, shipping). Agent methods `render.permutations` and
  `render.set_permutation_policy`.
- Material **instances**: a material may derive from a parent material and override parameters, textures and static switches (a switch override selects another permutation).
- **Mask recolor and decal slots** (Tincal B7, ADR-233): a `MaskDecals` component recolors one mesh
  instance through a mask texture (R/G/B = primary/secondary/trim palette weights, A = emissive) and
  paints up to 8 atlas decals (cell, UV rect, rotation, color, blend alpha/multiply/emissive), all
  per instance. Group 3 of the `MASK_DECALS` forward variant holds the instance's uniform block and
  the two textures; the 80-byte object block is unchanged. Like a material graph that is still
  building, a masked draw whose variant is not built yet draws with the standard surface instead of
  being skipped. Standard opaque, masked and alpha-blended materials only.
- **Lightmap receivers** (ADR-1623, ADR-1556): a drawn object the host maps to an entry of the installed
  `.flmap` (`FrameRenderer::set_lightmap_receivers`, or the `LightmapReceiver` component) is drawn from
  the entry's unwrap with its UV2 as the second vertex stream through the `LIGHTMAP` forward variant;
  its group 3 holds the lightmap (atlas, sampler, parameters, directions) and one `scale_offset` block per
  entry at a dynamic offset, so the object block is unchanged. Its diffuse indirect term is the baked
  irradiance in place of the probe / sky term. A receiver whose variant is not built yet keeps the probe
  light. Standard opaque and masked rigid materials only. It draws on every path (ADR-2107): the GPU-driven
  batches (`scale_offset` per object slot in a table, group 3 binding 17), the visibility resolve (UV2 in its
  own range of the geometry words, a row per instance) and the Mobile lite shader; `RenderProfile::lightmap_tier`
  caps the lookup. Frame warms build the variants when a lightmap is installed.
- Material graph editor (Tier 2) generates WGSL that implements the same `material_surface` contract.

## 8. Meshes, skinning, morphs
- GPU mesh storage (ADR-218, batch v020): one vertex arena (`LabVertex` units) + one index arena (`u32`
  units), each a list of chunk buffers that never move (2 MiB doubling to 64 MiB; Android 1 → 16 MiB;
  clamped to the device's `max_buffer_size`). A mesh takes a best-fit range from a coalescing free
  list; a mesh over half a chunk gets its own buffers, and so does the vertex stream of a mesh drawn
  with a second UV stream (that stream is a per-mesh buffer from vertex 0, and `base_vertex` offsets
  every vertex slot). Draws (direct, instanced, and the GPU-driven
  indirect commands) use `first_index`/`base_vertex`. A range is owned by a reference-counted lease:
  dropping the cache entry (idle/budget eviction, memory pressure, a size change, `release_meshes`)
  returns it, so a later mesh reuses it without creating a buffer; a LOD cross-fade twin shares its
  base's lease. Memory pressure also drops chunks left empty. The cache budget counts ranges, not
  chunks. Inside a frame, mesh uploads of up to 4 MiB per frame are copied through the staging belt
  when it has room without growing (1 MiB headroom kept); larger uploads (level loads, `prewarm`) use
  `queue.write_buffer` into the range.
- Mesh identity: the runtime keys a `MeshRenderer`'s mesh by a process-wide generation id per
  `Arc<CpuMesh>` (`mesh_key`, top byte `0x4D`, id in bits 3..56 so LOD/shadow/dither xors never
  collide), not by the `Arc` address. The registry holds a `Weak` per id, which pins the address while
  registered, so a new `Arc` can never inherit a dropped one's key (the ABA hole of address keys).
  When the last `Arc` drops, `extract_frame` hands the id to `ExtractedFrame::released_mesh_keys`
  and the runtime calls `FrameRenderer::release_meshes`, freeing the ranges at once. Hosts that
  build `FrameMesh` themselves may keep any stable key; content hashes stay a cook-time tool.
- Skinning: vertex-shader skinning with joint palette (Mat3x4 per joint) in a storage buffer; previous
  palette kept for motion vectors. Max 256 joints per skin; max 4 influences per vertex.
  Cooked meshes carry their skin (cooked mesh v2, ADR-226): `CpuMesh::from_cooked` fills `LabVertex`
  joints (cooked skeleton ids) and weights; a skin with 8 stored influences draws its 4 strongest,
  renormalised, and a skin over 256 joints is refused (`CookedSkinError::TooManyJoints`) rather than
  drawn with another draw's joints. `load_cooked_model` draws skinned parts rigid in bind pose (it has
  no palette). Cooked `COLOR_0` stays on the cooked mesh. `CpuMesh::from_cooked` writes white
  `LabVertex::color` and does not copy the stream: the forward shader multiplies that attribute
  into base color, so applying `COLOR_0` would tint existing cooks. A GPU-ready pak mesh
  (`regia_asset::gpu_mesh`, layout v3) carries `COLOR_0` in its vertex color slot so
  `AssetServer::load_mesh` keeps the stream, and `lab_mesh_from_gpu` writes white on load, so both
  mesh forms draw the same (ADR-7919). Skinned and morphing meshes are GPU-ready too since layout v3:
  the packed skin and morph targets follow the indices, and the vertex joint and weight slots stay
  zero (`load_cooked_model` draws them rigid, as for the compact form). `CpuMesh::from_cooked` (and
  the counted upload) draws a mesh's stored tangents when they are real, so a GPU-ready entry builds
  its render vertices with no MikkTSpace pass (ADR-10080).
- Compute skinning (opt-in, Tincal B2, ADR-246): `SkinningConfig::compute` or a `SkinningOptions`
  component moves a skinned instance to a compute pass (`skin_compute.wgsl`) that skins it once per
  frame into two vertex buffers of its own (this frame, last frame). Every pass draws that result as a
  rigid mesh (zero weights) and the velocity pass reads both buffers (`vs_preskinned`). The compute path
  also reads up to 8 influences per vertex (`SkinInfluences8`, from the cooked skin's two sets) and blends
  with dual quaternions per entity (Kavan et al. 2008, scale/shear first). A mesh cooked with
  `skin_influences` 8 (cooked mesh v3, ADR-1622) gets those options from `SkinningOptions::for_cooked_skin`;
  a default mesh gets none and draws as before. The vertex-shader path stays
  the default and the fallback on devices without compute shaders.

  palette kept for motion vectors. Max 256 joints per skin; max 4 influences per vertex. The renderer keeps
  last frame's palette per object key itself (with the previous model matrices; the host submits only the
  current palette), and the velocity pass skins each vertex with both (v0.1.15, ADR-221). An object with no
  previous palette of the same joint count writes only its model motion.
- Morph targets (Tincal B1, ADR-241): the cooked mesh stores them sparse (moved vertex indices +
  `f16x3` position/normal/tangent deltas + delta bounds, cooked mesh v2). `MorphTargets` packs them once
  per mesh into rows (one per moved vertex: its targets' entries and base attributes); weights are per
  instance (`MeshMorphTargets` + `MorphWeights`, `ExtractedFrame::morphs`,
  `FrameRenderer::set_morph_instances`). A compute pre-pass (one invocation per moved vertex, a gather:
  no atomics) deforms an instance when its weights change and copies the moved span into the
  instance's own mesh-cache entry (key top byte `0x6D`), which every pass draws and vertex skinning
  skins; nothing reads a storage buffer in a vertex shader. Game mode applies the 16 largest weights,
  creator mode all. Steady weights cost nothing; no buffer is created per frame. A rigid instance keeps
  two whole-mesh copies of its deformation (made on its first draw, written only when its weights
  change) and the velocity pass draws both poses on such frames, so a rigid morph writes its motion
  vectors (B2, ADR-2652); a skinned morph writes its skinning motion only.
- LOD crossfade (dithered) optional per mesh; default hard switch with hysteresis.

## 9. Cameras and views
- `Camera` component: projection (perspective FOV-Y / orthographic size), near plane (far = infinite
  for perspective), viewport rect (split-screen), render target (window / texture asset / offscreen
  image), order, clear (color/skybox/none), layer mask, `hdr`, `post_process` settings ref,
  `msaa` override, exposure override.
- `PhysicalCamera` component (ADR-112): shutter angle and samples (motion blur), focus distance,
  f-stop, sensor height and kernel (depth of field). Each effect also needs the preset to allow it
  (`RenderProfile::{motion_blur, depth_of_field}`: Desktop High, Ultra, Deck); without the component
  the frame is unchanged.
- Render-to-texture: `RenderTarget::Texture(Handle<Texture>)` for mirrors, security cameras, UI 3D
  previews.
- Split screen: multiple cameras with viewports, shared shadow maps for the directional light where
  possible. As built (ADR-7286): each window view's two-phase occlusion and cluster cull read back into its
  own ring (`FrameRenderer::set_occlusion_view`), so `ViewVisibility` keeps occlusion with several views.
  As built (ADR-7329): each window view also keeps its own occlusion history: its forward Hi-Z pyramid (phase 1
  of the two-phase prepass tests the view's own last pyramid with its own matrix) and its visibility-buffer
  cluster bits, swapped in when the view draws; `FrameRenderer::retain_occlusion_views` frees a removed view's,
  and `render.visible_entities` reports each view's `pyramid_bytes`.
  As built (ADR-8620): each window view keeps its own visibility-buffer LOD hysteresis history too (CPU mirror
  swapped, GPU boundary words copied with the cluster bits), so a view change is not a camera cut and a cut
  discards only its own view's history; a window view undrawn for `OCCLUSION_VIEW_RETIRE_ROUNDS` (16) rounds
  gives back its pyramid, cluster bits and LOD history without a host call (`retain_occlusion_views` stays as
  the immediate override). `render.get_visbuffer` `last_frame` reports `lod_history_discards`,
  `lod_history_copies`, `view_histories` and `view_history_bytes`.
- Post volumes and physical camera exposure: `PostVolume { shape: Box|Sphere|Unbound|ConvexHull (v3, ADR-2323), priority, blend_distance,
  weight, overrides }` and `PhysicalCamera::exposure` (`Inherit`, `Manual`, `Physical`). Design and blending rules:
  `30-post-volumes-and-physical-camera.md` (ADR-1464).

## 10. Lighting data for static scenes (Tier 1 subset)
- Reflection probe baking (render 6 faces → prefilter → store as cooked cubemap asset): a pipeline
  function reachable from the CLI and the agent protocol in M5; the editor menu entry follows in M11.
- Light probes: dynamic probe GI in M15b, baked GI in M15d (`specs/26` § 4). M5 provides ambient from environment SH + optional
  `AmbientVolume { color/SH, intensity }` boxes as artist-placed fill.
- As built (ADR-5611): the probe bake is the local reflection capture bake (`regia bake-captures`, agent
  `render.bake_local_capture`, `.fcap`, ADR-2860); there is no `AmbientVolume` type (fill comes from the environment
  SH, probe GI and baked light), a v0.5.x gap if artists ask for it. Built (ADR-7211): `AmbientVolume { color,
  intensity, half_extents, blend_distance }`, an axis-aligned box adding color × intensity lux to the diffuse ambient
  term inside it (up to four per frame); color only, no SH. Since ADR-7275 the box turns with the entity's rotation:
  the point is taken into the box's frame (`FrameAmbientVolume::rotation`, the uniform's `ambient_rot` rows) before the
  extents test, in the forward, visibility-buffer and Mobile forward shaders and in `weight_at`. As built (ADR-7490): a
  volume (and a directional light) whose resolved visibility is hidden — its own `Visibility::Hidden` or a hidden
  ancestor — is not extracted and so lights nothing, like a hidden mesh; only kept-over-budget volumes count as
  refused. As built (ADR-7549): `DirectionalLight` and `AmbientVolume` require `InheritedVisibility` (derived, never
  saved), so a light child of a hidden group is hidden by scene propagation without a `Visibility` of its own; bakes
  take the same sun (the editor's bake panel the lowest drawn entity, as the frame does; `level.export_bake` and
  `regia bake-gi --from-scene` the first drawn light by guid). Point, spot and area lights are not
  visibility-filtered yet.

## 11. Debug and tooling
- `DebugDraw` API (immediate-mode, any system): lines, arrows, boxes, spheres, capsules, frustums, text
  labels (world-space, via UI font atlas), with duration and depth-test flag. Batched into one draw.
- View modes (editor + dev builds): lit, unlit, wireframe, normals, base color, roughness, metallic,
  AO, overdraw, light complexity (cluster counts heatmap), shadow cascades, LOD coloring, motion vectors,
  mip level.
- GPU timings per graph node; draw-call / triangle / pipeline-switch counters → `Diagnostics`.
- Screenshot: `Screenshot::capture(window|camera) -> Future<Image>` (readback); used by golden tests and
  thumbnails.
- As built (ADR-5611): `DebugDraw` (`debug.rs`) is a line list; the shapes (lines, arrows, boxes, spheres,
  capsules, frustums, stroke text) come from the gizmos (`gizmos/`, `book/agents/gizmos.md`) with a
  depth-test flag; a per-item duration and text from the UI font atlas are not built. View modes implemented:
  lit, wireframe, UV checker, texel density, material id, cloud shadow map, image light (+ occlusion) and the
  lightmap modes (`DebugView::is_supported`); unlit, normals, base color, roughness, metallic, AO, overdraw,
  light complexity, shadow cascades, LOD coloring, motion vectors and mip level are reserved enum values
  (v0.5.x gaps). Built (ADR-7211): all twelve (ten as forward-shader codes, overdraw and LOD coloring as material
  swaps; `DebugView::is_supported` is true for every mode), and `DebugDraw` items carry a duration in frames with
  world-space labels in the gizmo stroke font (not the UI font atlas, which sits above the renderer). Built
  (ADR-7254): labels draw from the UI font atlas through a glyph-source hook `regia_ui` installs
  (`regia_render::DebugLabelGlyphs`; atlas-page quads in the gizmo pass, the stroke font without a source);
  the Mobile forward draws the ten shader views; `mip_level` reads each texture's size from the texture table. Screenshots are `ScreenshotRequests::request(window, callback)` (a callback per window, no
  camera target).

## 12. Headless and testing
- `RenderPlugin { headless: true }` renders to offscreen targets with no window — used by golden-image
  tests (llvmpipe/lavapipe on Linux CI, WARP on Windows CI) and the asset thumbnailer.
- Determinism for golden tests: fixed time, no TAA jitter unless tested, fixed exposure.
- The reference path tracer (`specs/27` § 1, crate `regia_pt`) consumes the same extracted world and
  `#import`s `pbr_lighting.wgsl` / `pbr_surface.wgsl`; a change to the BRDF must keep `specs/27` § 1.5 item 4
  (forward direct term equals the PT `direct` AOV) passing. Real-time techniques are scored against it (`specs/27` § 2).

## 13. Acceptance (M5)
- Golden scenes (in `regia_test_utils/goldens/`): material spheres grid (metal × roughness), each light
  type with shadows, normal-mapped plane, alpha modes, IBL-only sphere, Sponza view, skinned Fox in a
  stored non-bind pose (clip sampling arrives with M12), post-process stack on/off. SSIM ≥ 0.98 vs blessed images on lavapipe.
- Visual parity check (manual, recorded in progress.md): DamagedHelmet, FlightHelmet, MetalRoughSpheres
  vs the Khronos sample viewer under the same environment.
- Perf (reference hardware in testing doc): Sponza 1920×1080 Desktop High ≥ 144 fps (GPU ≤ 6.9 ms);
  Mobile High ≥ 60 fps at 1080p-class render resolution on the Android reference device. Stress scene
  `10k_cubes` (10,000 moving cubes, 50 materials, 64 point lights) renders in ≤ 60 draw calls after
  instancing (excluding shadows) at ≥ 60 fps desktop High / ≥ 30 fps Mobile High.
- Android: 50 suspend/resume cycles with full scene loaded; no GPU memory growth.
- As checked (ADR-5611): the `10k_cubes` draw-call bound is a CPU batching test
  (`batch::ten_k_cubes_fifty_materials_batches_under_sixty`), not a rendered scene with lights; the Android
  cycles run as `cargo xtask android-lifecycle` (50 by default, PSS growth on a device, ADR-3372) and the
  headless `lifecycle_drill`. A rendered 10k-cube scene and a GPU-memory reading per cycle are v0.5.x gaps.


## Saved pipeline-cache feedback harvest (C-21, ADR-8162)

Normal feedback captures attach the installed renderer's last saved FPCH v1 file,
validated against the captured key on the bundle writer. Hosts can provide a
`feedback::PipelineCacheAttachment` resource and set the attachment byte budget.
There is no new driver call and automatic crash capture does not collect caches.
Package inputs accept loose caches, stored feedback ZIPs and recursive directories;
full-key separation and largest-per-key selection preserve GPU/driver compatibility.
`--pipeline-cache-reference` gates harvested files by the current build's shader/wgpu
pairs, including game salts; the unsalted engine pair is the default for feedback.
Details and agent checks: [Pipeline cache harvest](../../book/agents/pipeline-cache-harvest.md).
The existing FPCH v1 format and runtime exact-key checks are unchanged.
