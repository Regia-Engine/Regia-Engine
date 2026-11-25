# Quality and upscalers

<!-- index: Quality tier -->
<!-- index: MobileLow -->
<!-- index: MobileHigh -->
<!-- index: Deck -->
<!-- index: VeryLow -->
<!-- index: Render profile -->
<!-- index: Cinematic -->
<!-- index: Upscale quality -->
<!-- index: Native -->
<!-- index: TAAU -->
<!-- index: TSR -->
<!-- index: FSR 3 -->
<!-- index: Frame generation -->
<!-- index: DLSS -->
<!-- index: XeSS -->
<!-- index: Present mode -->
<!-- index: Vsync -->
<!-- index: VsyncRelaxed -->
<!-- index: Dynamic resolution -->

Pick a quality tier, then decide how the frame is scaled up to the display, whether a vendor
upscaler can run, and how the window presents. The chapter pages stay the reference:
[Rendering](../../user-guide/rendering.md),
[Automatic quality detection](../../user-guide/auto-quality.md),
[Dynamic resolution](../../user-guide/dynamic-resolution.md).
The generated [render profile table](../../reference/render-profiles.md) lists the six real-time
presets. It omits the VeryLow tier and the Cinematic profile until the generator is extended and
`cargo xtask manual` is run again.

## Before you start

A game that uses `DefaultPlugins` already has a renderer and automatic quality detection. The
running default is the Deck preset on a Steam Deck (`SteamDeck=1`, or a Valve Jupiter or Galileo
model on Linux), Mobile High on Android, and Desktop High everywhere else
(`RenderProfilePreset::runtime_default`). A packaged `[platforms.<os>] render_profile` wins over
a saved Auto result. A preset the player chose wins over both.

## 1. Choose a quality tier

`QualityTier` is what detection and the options menu speak. `RenderProfilePreset` is the bundle
of renderer columns that tier starts from. `GraphicsPreset` is the value saved in
`GameSettings.graphics.preset`.

| Quality tier | Starts from | What changes on top |
|---|---|---|
| `MobileLow` | Mobile Low | The Android floor. |
| `MobileHigh` | Mobile High | The Android default. |
| `Deck` | Deck | 1280×800, 40 fps cap, render scale 0.70. |
| `VeryLow` | Desktop Low | Render scale 1/3, SGSR 1 spatial upscale, integrated-GPU texture budget (384 MiB, lazy). Content budgets stay on Low. |
| `Low` | Desktop Low | Render scale 0.75, FXAA. |
| `Medium` | Desktop High | Render scale 0.67, Balanced lighting, bent normals off. |
| `High` | Desktop High | Native resolution, temporal AA, Depth lighting where probes are on. |
| `Ultra` | Desktop Ultra | The highest real-time preset. |

`GraphicsPreset::Clean` renders as High. The game strips its own post on top of that. On Android,
`GraphicsPreset::Low` and `VeryLow` become Mobile Low, and every other menu preset becomes Mobile
High.

Apply a tier in code:

<!-- ignore-reason: a one-call sketch of QualityTier::apply; not registered on an app -->
```rust,ignore
use regia_render::{QualityTier, RenderSettings};

fn apply_tier(settings: &mut RenderSettings) {
    QualityTier::High.apply(settings);
}
```

`QualityTier::apply` keeps the player's `gi_look`. VeryLow still reports
`RenderSettings::preset` as Desktop Low, so water and foliage stay on Low's columns.
`QualityTier::ALL` is the seven tiers through v0.5.0 (bake files are keyed on it).
`QualityTier::EVERY` adds VeryLow between Deck and Low.

The offline profile is `RenderProfilePreset::Cinematic`. It is Desktop Ultra with native scale,
8192² sun cascades, every cloud texel marched, the light caps at their maximum, and mesh LOD held
toward LOD 0. It is outside `RenderProfilePreset::ALL`: no menu and no detector picks it.
`regia_render::cinematic::gate_preset` applies it on a desktop whose detected tier is High or
Ultra. On Android the applied preset is Mobile High and the reason is `cinematic_needs_desktop`.
On a detected tier below High the applied preset is that tier's own, reason
`cinematic_tier_too_low`, unless the caller passes `force`. A headless tool with no detected tier
runs Cinematic as asked. Hosts read `--render-profile`, then `REGIA_RENDER_PROFILE`, then
`render_profile.txt` in the scene directory (`regia_render::cinematic`).

## 2. Let the first launch pick

On a first launch, with no saved preset, `AutoQuality` reads the GPU, walks
`gpu_tiers.ron`, and saves `graphics.preset` with `preset_source: Detected`. It leaves a player
choice alone. The confirmation probe is off unless the game calls
`AutoQuality::default().with_confirm(true)`. With the probe on, a tier that misses its frame
budget steps down once: Ultra to High, High to Medium, Medium to Low. Low steps to VeryLow when
the sustained GPU time misses 30 fps (`QualityTier::VERY_LOW_TRIGGER_FPS`). Deck, VeryLow, and
Mobile Low do not step. The VRAM floors never step below Low. Unknown VRAM (a DX12 adapter) and
an unknown discrete GPU under 15 GB land on Low. The measured row `intel_n_series` lands on
VeryLow directly.

Check the record with the agent method `quality.get_detected`. The menu string is the
`graphics.detected.reason` line.

## 3. Choose an upscale quality

`UpscaleQuality` sets the render size from the display size. Every preset leaves
`RenderProfile::upscale_quality` at `None`, so the profile's own `render_scale` applies (the
table above). `Some(mode)` replaces that scale. The ratio is display size divided by render size,
as an exact fraction, rounded to the nearest integer, halves up, at least 1.

| Mode | Ratio | Permille | 2560×1440 renders at | 3840×2160 renders at | Dynamic-resolution range |
|---|---|---|---|---|---|
| `Native` | 1/1 | 1000 | 2560×1440 | 3840×2160 | 667–1000 |
| `Quality` | 3/2 | 667 | 1707×960 | 2560×1440 | 588–667 |
| `Balanced` | 17/10 | 588 | 1506×847 | 2259×1271 | 500–588 |
| `Performance` | 2/1 | 500 | 1280×720 | 1920×1080 | 333–500 |
| `UltraPerformance` | 3/1 | 333 | 853×480 | 1280×720 | 333 |

<!-- ignore-reason: one field write for upscale quality; the mode table above is the contract -->
```rust,ignore
use regia_render::{RenderSettings, UpscaleQuality};

fn performance(settings: &mut RenderSettings) {
    settings.profile.upscale_quality = Some(UpscaleQuality::Performance);
}
```

The mode sets the size. It does not choose the upscaler. A TAA frame uses
`profile.temporal_upscaler`. Any other frame uses `profile.spatial_upscaler` (`Bilinear`, or
`Sgsr` on VeryLow).

## 4. Choose a built-in temporal upscaler

`TemporalUpscaler` runs when anti-aliasing is TAA (Desktop High, Desktop Ultra, Deck, and a
Cinematic frame). The default on every preset is `Taau`.

| Variant | What it is |
|---|---|
| `Taau` | Neighbourhood clamp, bilinear history. |
| `Tsr` | Jitter-aware reconstruction, depth-dilated motion, disocclusion, variance clipping, thin-feature locks, a reactive mask. |
| `Fsr3` | The FSR 3.1 upscaler ported from the AMD FidelityFX SDK v1.1.4 (MIT): Lanczos upsampling, variance-box rectification, locks, RCAS. |

<!-- ignore-reason: one field write for the temporal upscaler; not a frame-renderer setup -->
```rust,ignore
use regia_render::{RenderSettings, TemporalUpscaler};

fn use_tsr(settings: &mut RenderSettings) {
    settings.profile.temporal_upscaler = TemporalUpscaler::Tsr;
}
```

A host that owns the `FrameRenderer` can call `set_temporal_upscaler`. Switching drops that
upscaler's history. TAAU, TSR, and FSR 3 are compiled into every build. They run on the backends
the frame already uses.

## 5. Vendor upscalers (partial)

DLSS and XeSS are optional link crates. The SDK binaries are not in the repository. When a vendor
upscaler cannot run, the frame uses the built-in temporal upscaler and
`vendor_upscaler_report` says why.

**DLSS** (`regia_dlss`, `VendorUpscaler::Dlss`). Build with the `sdk` feature and `DLSS_SDK` set
to an absolute path of the DLSS SDK. The binding is NGX on Vulkan, on Linux and Windows. Android
and macOS never link, even when `DLSS_SDK` is set. At run time NGX loads
`libnvidia-ngx-dlss.so.<version>` or `nvngx_dlss.dll` from the directories in
`NgxConfig::library_dirs`. A missing runtime, a non-NVIDIA GPU, or an old driver returns a
`DlssError` code and the built-in upscaler runs. The DX12 device does not run this binding
(`dlss_needs_vulkan`).

**XeSS** (`regia_xess`, `VendorUpscaler::Xess`). Build with the `sdk` feature and `XESS_SDK` set.
The binding is Windows Vulkan. Other targets are not linked. Without the SDK every runtime
operation returns `not_linked`.

A host selects one with `FrameRenderer::set_vendor_upscaler(Some(VendorUpscaler::Dlss))` (or
`Xess`). `None` turns the vendor slot off. Agents use `render.get_upscalers` and
`render.set_upscaler`. Stable refusal codes include `sdk_not_bundled`, `runtime_not_found`,
`needs_nvidia_adapter`, `needs_desktop_gpu`, `dlss_needs_vulkan`, and `xess_needs_vulkan`
(`regia_render::upscaler::reason`).

## 6. Frame generation

Frame generation presents one interpolated frame between two rendered frames. It is a WGSL port
of the FidelityFX SDK v1.1.4 frame-interpolation and optical-flow shaders, with Regia's own pacing
on wgpu surfaces. No vendor binary is involved. It is off until `FrameGenSettings.enabled` is set,
and the cargo feature `frame-gen` has to be on or the status blocker is `not_compiled`.

Other blockers: `headless` (no surface), `needs_desktop_gpu` (the device lacks the storage-texture
and storage-buffer counts, or it is GL). `interpolator_not_ported` is reserved; the interpolator
is in this tree (`FrameGenStatus` reports it only when a caller says the interpolator is absent).

The default `min_base_fps` is 55. Below that the pacer presents real frames only. The HUD is
redrawn on the generated image (`HudComposition::Redraw`); it is not interpolated. A camera cut,
a resize, or a pause over half a second presents the rendered frame only for that tick.

<!-- ignore-reason: one field write on FrameGenSettings; the feature stays off unless the game sets it -->
```rust,ignore
use regia_render::frame_gen::FrameGenSettings;

fn enable(settings: &mut FrameGenSettings) {
    settings.enabled = true;
}
```

Read `render.frame_gen` before offering the option. `can_enable` is true when `blockers` is empty.

## 7. Present the window

`PresentMode` on the primary window:

| Mode | Behavior |
|---|---|
| `Vsync` | Wait for vertical blank. The default. |
| `VsyncRelaxed` | Vsync, and a late frame may tear. |
| `Immediate` | No wait. The image may tear. |

`GameSettings.display.vsync` applies live. On maps to `Vsync`, or keeps `VsyncRelaxed` when that
was already the window's mode. Off maps to `Immediate`. The renderer then takes `Immediate` if the
surface offers it, else `Mailbox`, else FIFO, and `DisplayApplied::present` reports which one.
On Android, off is refused: frames stay on the display's vsync grid
(`android_display_paced`). Lower the rate with `display.frame_cap` instead. A surface with no
unsynced mode keeps FIFO (`no_unsynced_mode_offered`). A host variable-refresh policy picks its
own mode (`host_vrr_policy`).

## 8. Dynamic resolution

`DynResController` returns a render-scale permille from GPU time and wall time. It does not write
`RenderSettings`. Apply the permille to the frame you are about to draw. Under an upscale quality
mode, build the config with `DynResConfig::default().within_quality(mode)` so the scale stays
inside that mode's range. The rules (CPU-bound frames do not step down, the median window, the
cooldown) are on [Dynamic resolution](../../user-guide/dynamic-resolution.md).

## Finished check

1. `quality.get_detected` names the tier and a reason, or `preset_source` is `Player` with the
   menu preset you chose.
2. `render.get_upscalers` shows `current.backend` as `taau`, `tsr`, or `fsr3`, and `current.quality`
   as the mode you set or `null` when the profile scale is in force.
3. If you asked for DLSS or XeSS, `vendor_frame.active` is true, or `vendor_frame.fallback` names
   the refusal and the built-in upscaler is the one drawing.
4. `render.frame_gen` shows `active: true` when generation is presenting, or a blocker code when
   it is not.
5. `settings.get` reports `applied.display.present_mode` as `Vsync`, `VsyncRelaxed`, or
   `Immediate`, and `vsync_note` when the request was rewritten.

## Where to read more

- [Upscalers (agents)](../../agents/upscalers.md) for the method fields and the refusal codes.
- [Frame generation (agents)](../../agents/frame-gen.md) for pacing and HUD composition.
- [Automatic quality (agents)](../../agents/auto-quality.md) for `quality.detect`.
