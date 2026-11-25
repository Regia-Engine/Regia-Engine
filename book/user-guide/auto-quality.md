# Automatic quality detection

<!-- index: Quality tier -->
<!-- index: VeryLow -->
<!-- index: Auto quality -->
<!-- index: Graphics preset -->

A player's first launch should look good and run smoothly without a trip to the options menu.
The procedure for choosing a tier, an upscaler, and a present mode by hand is
[Quality and upscalers](../how-to/picture/quality-and-upscalers.md). Cinematic is an offline
render profile, not a tier this detector returns.

On a first launch Regia Engine reads the GPU, picks a quality tier from a table of measured and
estimated GPU classes, optionally checks it on the first scene, and saves the result in the
settings file. It never overrides a preset the player chose.

## Packaged defaults

A packaged `[platforms.<os>] render_profile` takes precedence over saved Auto results and
first-launch detection; player settings take precedence over both. The runtime applies the exact
preset before `Startup`, without translating mobile presets into desktop tiers. Package defaults
are not written to `GameSettings`. Player render-scale and shadow-quality overrides still apply.
An explicit Auto-detect request replaces the package default for this launch. At the next launch
the package wins again unless the player chose a preset. `render.get_profile` and
`quality.get_detected` report `configuration` with the source and metadata location (ADR-7893).

## What happens on a first launch

1. **No saved quality setting.** `GameSettings.graphics.preset` has never been set: there is no
   settings file, or the file's `preset_source` is `Unset` (an older file with a non-default
   preset counts as the player's choice).
2. **Detect.** When the render device exists, `AdapterProfile::from_device` reads the adapter:
   name, PCI vendor and device id, integrated or discrete, the largest device-local memory heap
   (VRAM, read through Vulkan; unknown on DX12), the limits the engine needs and BC texture
   support. `regia_render::quality::detect` walks the GPU table and returns a tier and a reason:
   `NVIDIA GeForce RTX 3080 Ti, 12 GB → High (nvidia_rtx_3080_3090, measured)`.
3. **Save.** `graphics.preset` gets the tier, `graphics.preset_source` becomes `Detected` and
   `graphics.detected` keeps the record: tier, reason, table row, evidence (measured, by class or guess), GPU, VRAM,
   driver. The settings plugin applies it to `RenderSettings` and writes `user://settings.ron`.
4. **Confirm (optional; off by default since v0.1.0, `AutoQuality::with_confirm(true)` turns it
   on).** Its GPU timings were not yet trustworthy on the RTX 5080 when v0.1.0 shipped (ADR-173), so
   a first launch uses the table's pick. Once the boot splash is gone and the
   game-frame warm is back, a probe renderer on a warm worker draws the host's first scene (DO27's
   lobby) offscreen at the window's size, back to back, so the GPU reaches its clocks: at least
   0.75 s and 30 frames of warm-up, then 2.25 s timed with GPU timestamps. The visible frames and
   the runtime renderer are untouched, and the splash is never held (ADR-173; before v0.1.0 the
   probe held the splash 3–5 s on a first launch and timed visible frames, which a downclocked GPU
   failed). If the p95 GPU frame time is over budget and the median over 0.8 × budget (1000 / 60 ms
   on desktop, 1000 / 40 on the Deck, configurable), and the GPU was busy for at least half of each
   frame interval, the tier steps down once (Ultra → High → Medium → Low, and Low → VeryLow
when the sustained time misses 30 fps): a
   `SettingsChanged` is sent and the renderer warms again behind the lobby (its 3D scene, if any,
   is hidden until the warm is back; the UI stays). A level load (`GameFrameWarm::request()`)
   before the probe has finished aborts it and keeps the table's pick. A lobby that draws no 3D
   scene gives it nothing to draw: it ends with no frames after 15 s and keeps the tier. A tier
   that cannot step down (Low, Deck) and a software rasterizer skip the probe. The
   probe result is saved under `graphics.detected.probe`.

Later launches read the saved preset and detect nothing. A player who picks a preset in the menu
sets `preset_source: Player`, and detection leaves it alone from then on.

## Tiers

| Tier | Render settings | Probe target |
|---|---|---|
| `Ultra` | Desktop Ultra | 60 fps |
| `High` | Desktop High (Depth lighting) | 60 fps |
| `Medium` | Desktop High at render scale 0.67 (TAAU), Balanced lighting | 60 fps |
| `Low` | Desktop Low; terrain virtual texture on discrete GPUs with ≥ 6 GB (see below) | 60 fps (steps to `VeryLow` only below 30 fps) |
| `VeryLow` | Desktop Low at render scale 1/3, SGSR 1 upscale (v0.5.1, ADR-7882) | 30 fps (bottom: never steps) |
| `Deck` | Deck preset (1280×800, 40 fps cap) | 40 fps |
| `MobileHigh` / `MobileLow` | the mobile presets | 30 fps |

`Medium` costs about half of `High` at the same output size: on the RTX 5090, 4K High at 0.67 is
3.30 ms against 6.55 ms native (gpu-farm4 sweep). The menu presets map onto tiers through
`GraphicsPreset::tier`: on Android, `Low` is Mobile Low and every other preset is Mobile High. The
`Clean` preset renders as `High`, and the game strips its post effects itself.

**Terrain virtual texture on `Low` (v0.1.2).** On Linux, on a discrete desktop GPU with at least
6 GB of known VRAM that is not AMD, `Low` also sets `RenderProfile::terrain_budget.virtual_texture`.
Windows and AMD (RADV) are left out until the texture is measured there. A shared terrain
draw then caches its photoreal surface in the terrain virtual texture (ADR-117 `Surface` cache,
both LODs per pixel, so Low's FXAA frame shows no grain). Measured on a GTX 1080 at 1080p on
DO27's Switchbacks, this took the median from 39 to 76 fps and the 1 % low from 24 to 43, for
about 670 MB more VRAM (card peak 1.9 GB of 8 GB). The texture also stores the terrain horizon,
so shadowed walls keep the depth they have without it: Europa's Jupiter-shine and Mars's
circumsolar glow are hidden behind ridges as before. The remaining cost is in the look: ground
past 24 m is slightly softer and loses fine cracks. Ground nearer than 24 m is unchanged
(`docs/evidence/perf/v012-pascal-vt/`).
- When the device does not exist yet at plugin build, the lever is applied on the first frame
  that has it, even if the host cleared it in a `Startup` system.
- The lever is re-applied on every switch to `Low`. Clearing it in `RenderSettings` lasts only
  until the next preset change.
- `Medium`, `High` and `Ultra` never set it, and no preset does.
- To opt out permanently, a host calls `TerrainSharedDraw::set_virtual_texture` with its own
  choice (`None` turns it off). Any explicit call makes the profile stop driving the texture.
- Switching between `Low` and another preset in the options menu drops or builds the texture's
  atlas and pipelines inside one frame: expect a one-time hitch there.
- A small drift in the sun direction (under 0.1°) does not flush the cache; a host may re-send the
  sun every frame. A larger turn flushes it, and the next frames shade in full while the pages
  re-bake.

**`VeryLow`, the step below `Low` (v0.5.1, ADR-7882).** Some GPUs miss 30 fps even at `Low`
(the Intel N150 laptop, the engine's Windows minimum: Desktop Low runs at about 14 fps there).
`VeryLow` is Desktop Low drawn at a third of the output size in each axis (the upscalers' Ultra
Performance size) and upscaled by SGSR 1, so it shades about a fifth of Low's pixels and looks clearly
softer. On the N150 at 0.5 scale Low still ran at about 23 fps; at a third it holds 30 fps (GPU p50
29 ms, `docs/perf-log.md`, 2026-11-15). Everything else is Low's: budgets,
particles, procedural bakes and the systems that pick their own tier by preset (water, foliage)
treat it as `Low` (`QualityTier::content_tier`, `RenderSettings::preset` answers `DesktopLow`).
- The GPU table picks it directly for the measured class (row `intel_n_series`, the Intel
  N100-N305 / N150-N355 built-in graphics).
- With the confirmation probe on, `Low` steps down to `VeryLow` only when the probe's sustained
  GPU time misses 30 fps (`QualityTier::VERY_LOW_TRIGGER_FPS`), or the host's own lower
  `target_fps`. A GPU that holds 30 fps at `Low` keeps `Low`, even under the 60 fps desktop target.
- The VRAM floors never step below `Low`.
- `GraphicsPreset::VeryLow` is the menu preset; the player's choice is never overridden. Its
  `QualityTier` and `GraphicsPreset` variants come after the existing ones; `QualityTier::ALL`
  still lists the seven older tiers (bake files and procedural quality pins are keyed on them)
  and `QualityTier::EVERY` lists all eight.

## The GPU table

The table is data: `crates/regia_render/src/quality/gpu_tiers.ron`, compiled into the engine
(`GpuTierTable::builtin`). Rows are tried in order and the first match wins. A row matches on the
PCI vendor, an optional device class, a device id **or** any name substring from `any`, all
substrings of `all` (laptop rows use `"laptop"`), and none of `none`. Names compare
case-insensitively. Every row says where its tier comes from:

| Row | GPUs | Tier | Evidence |
|---|---|---|---|
| `nvidia_rtx_5090_5080` | RTX 5090, 5080 | Ultra | **measured**: 5080 Ultra 1440p GPU p95 5.7 / 7.9 ms, 4K p95 12.0 / 15.9 ms (Bore Twelve / Switchbacks) |
| `nvidia_rtx_3080_3090` | RTX 3080, 3080 Ti, 3090 | High | **measured**: 3080 Ti High 1080p p50 6.6 / 7.8 ms, p95 14.2 / 10.9 ms |
| `nvidia_gtx_1080_1070` | GTX 1080, 1070 | Low | **measured**: 1080 Low 1080p p50 12.7 / 22.9 ms; High p50 23.2 / 37.6 ms |
| `nvidia_rtx_4090_4080_5070ti` | RTX 4090, 4080 (SUPER), 5070 Ti | Ultra | by class (≈ 5080) |
| `nvidia_rtx_5070_4070` | RTX 5070, 4070 (SUPER, Ti, Ti SUPER) | High | by class (≈ 3080 Ti) |
| `nvidia_rtx_3070_4060ti_5060`, `nvidia_rtx_3060_4060_2070_2080` | RTX 3060 (Ti), 3070 (Ti), 4060 (Ti), 5060 (Ti), 2070–2080 Ti | Medium | by class |
| `nvidia_older` | RTX 2060 (SUPER), 2050, 3050, 5050, GTX 16/10/9, GT 1030 | Low | by class (≤ GTX 1080) |
| `nvidia_laptop_top` / `_mid` / `_entry` / `_low` | 4080–5090 / 4060–5070 Ti, 3070–3080 / 2050–5050 / other laptops | High / Medium / Low / Low | by class |
| `amd_laptop_top` / `_mid` / `_low` | Radeon RX 7900M / 7800M, 7700S, 7600M XT, 6850M XT, 6800M / other M and S laptop parts | High / Medium / Low | by class |
| `amd_rx_9070_7900` / `amd_rx_7900_gre` / `amd_rx_7800_6800` / `amd_rx_mid` / `amd_rx_older` | Radeon RX | Ultra / High / High / Medium / Low | by class |
| `amd_integrated` / `intel_integrated` | Radeon Graphics, 780M, Vega / Iris Xe, UHD, Arc iGPU | Low | by class |
| `intel_arc_bmg_g31` | Intel BMG G31 (8086:e222, "Intel(R) Graphics (BMG G31)") | Low (+ terrain VT) | measured (arc-sweep) |
| `intel_arc_mid` / `intel_arc_low` | Arc Pro B50/B60 (BMG G21), B580, B570, A770, A750 / A580, A3xx | Low / Low | by class (from the measured G31) |
| `steam_deck` | Van Gogh (0x163f), Sephiroth (0x1435) | Deck | measured (`vfx_lab --deck-check` on a Steam Deck OLED (APU 0932; earlier text said LCD), 2026-10-14: GPU 10.3 ms mean in a 25 ms budget; see [Steam Deck](steam-deck.md)) |
| `adreno_7_8` (Adreno 730–750, 825–840), `mali_high` / `mobile_low` | phones | MobileHigh / MobileLow | guess |

Measured rows come from `cargo xtask bench-do27`: DO27's two maps, three path segments, 1800
frames after warm-up, GPU frame p50 / p95. The 5080 was run at 1440p and 4K, the 3080 Ti and the
GTX 1080 at 1080p. A **by class** row is not measured: it takes the tier of the nearest measured
row, scaled by public relative performance. A **guess** has no measured neighbor. The probe
corrects a tier that is too high; nothing corrects one that is too low, so the by-class rows lean
conservative.

**Coverage** follows the cards players own: the Steam Hardware & Software Survey for September
2026 ("All video cards"). Its top 40 by share, across NVIDIA, AMD and Intel and including laptop
GPUs and integrated graphics, all match a table row. The RON file's header lists each card with
its share, row and tier. The leaders are the RTX 5070 (6.15 %, High), RTX 5060 (4.40 %, Medium),
RTX 5060 Ti (4.06 %, Medium), RTX 4060 (3.89 %, Medium) and RTX 3060 (3.66 %, Medium). The test
`steam_survey_top_40_is_covered` pins all 40. The survey list is data in the table only; the
engine never fetches it.

**Unknown GPUs** get the fallback: software rasterizers, virtual and integrated GPUs get `Low`.
Discrete GPUs go by VRAM: 15 GB and up get `Medium` (never `High` or `Ultra`), anything less gets
`Low`. A GPU with unknown VRAM (DX12) gets `Low`, and an unknown Android GPU gets `MobileHigh`.
Since v0.1.0 this is one tier below the old VRAM guess, because no first-launch probe corrects it
any more (ADR-173). The player's preset menu is the correction path.

**Caps** apply to every result. A GPU below the engine's no-fallback floor (2D textures under
16384, fewer than 10 storage buffers per stage, fewer than 5 bind groups, or no BC textures on
desktop; ADR-153) is capped at `Low`. A tier also steps down while VRAM is under its floor: Ultra
needs 11 000 MiB, High 7 600 MiB, Medium 5 600 MiB (the memory budgets in `memory_budget.rs`).

### Adding a GPU

Measure it (`cargo xtask bench-do27`), then add a row above the rows it would otherwise hit:

```ron
(
    id: "nvidia_rtx_4070_ti",
    vendor: Some(0x10de),
    any: ["rtx 4070 ti"],
    tier: High,
    evidence: Measured("RTX 4070 Ti, bench-do27 2026-10-12: High 1440p p95 9.8 / 12.1 ms"),
)
```

A game can extend the table at run time without forking it. Prepend rows to the table of the
`AutoQuality` resource (`auto.table.prepend([...])`) or replace it with
`GpuTierTable::from_ron`. `regia_render` tests check that the table parses, that ids are unique,
and that every measured GPU and many known ones map to their intended tier.

## Settings and the options menu

The fields live in `GameSettings.graphics`:

- `preset`: the menu preset (`Low`, `Medium`, `High`, `Ultra`, `Clean`, `Deck`, `VeryLow`).
  `Clean` renders as High. On Android, Low and VeryLow become Mobile Low and every other
  preset becomes Mobile High (`GraphicsPreset::tier`).
- `preset_source`: `Unset`, `Detected` or `Player`. Read it with `graphics.source()`.
- `detected`: the last detection (`tier`, `detected_tier` before the probe, `reason`, `rule`,
  `evidence`, `gpu`, `pci_id`, `vram_mb`, `driver`, `probe`).

A menu that sets `preset` directly makes it the player's: the settings plugin marks any preset
change that detection did not make as `Player`. `graphics.choose_preset(p)` does the same
explicitly. Show "Auto: High (RTX 3080 Ti, 12 GB)" from `graphics.detected`. The menu's
**Auto-detect** button calls `AutoQuality::request_redetect()`. That detects again on the next
frame, replaces the preset even when the player chose it, saves it, and runs the probe if it is
enabled.

`RenderSettings` changes only when the preset changes, or on the first frame after a load when
`preset_source` is known. A volume change never resets the game's own profile tweaks, such as a
4K render scale. With `preset_source: Unset` (detection disabled, no device yet), the engine leaves
the game's `RenderSettings` alone.

## Configuration

`AutoQuality` is a resource (`regia_framework::auto_quality`). Insert your own before
`DefaultPlugins`:

```rust,wrap
# use regia::prelude::*;
# let app: &mut App = todo!();
app.insert_resource(
    regia_framework::auto_quality::AutoQuality::default()
        .with_confirm(true)          // the ~3 s probe behind the first scene (default: off, ADR-173)
        .with_target_fps(Some(60.0)) // None = the tier's default (60 desktop, 40 Deck, 30 mobile)
        .with_probe_seconds(3.0),
);
```

`.with_enabled(false)` turns detection off. Tests and CI can pin the GPU with
`AdapterOverride(AdapterProfile::named("NVIDIA GeForce GTX 1080", 0x10de, Some(8192)))`.

## Where it runs

- **`regia::run` / `DefaultPlugins`**: `AutoQualityPlugin` is included. It acts once the game
  adds `regia_framework::settings::SettingsPlugin`, the persisted settings file. A game without
  `GameSettings` does not run Auto detection, but still receives a packaged starting profile.
- **The probe** needs the windowed runtime. It starts once the splash is gone (or at once without
  a splash), no game-frame warm is pending and the window has a scene to draw. With the window
  hidden, or no camera, it measures nothing and keeps the tier.
  `BootBackground::probe_done()` says when it has finished.
- **The size it measured** (v0.5.1): `graphics.detected.probe` records the drawn `size` beside the
  monitor's (`monitor_size`) and the requested `window_mode`. When the host asked for fullscreen
  but the probe drew smaller than the monitor (a game that left fullscreen for a window, a window
  not yet resized), the record says `size_mismatch: true` and the probe runs again on the first
  frame the window fills the monitor (`AutoQuality::reprobe_pending`). Tests and hosts that draw
  elsewhere pin the display with `regia_framework::auto_quality::DisplayOverride`.
- **A shell that drives `FrameRenderer` itself** calls the same policy:
  `detect(&AdapterProfile::from_device(&device), &GpuTierTable::builtin())`, then
  `tier.apply(&mut render_settings)`. It times its own first frames and passes the result to
  `judge_probe(tier, &result, 60.0)`, or to `QualityProbe::complete_with` when the framework
  should save it.
- **Android**: detection runs (Adreno and Mali rows), and the probe is off by default because the
  shell owns settings persistence there.

Agents: `quality.get_detected` and `quality.detect` ([Auto quality](../agents/auto-quality.md)).
