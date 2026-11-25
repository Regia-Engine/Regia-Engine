# Rendering

<!-- index: Vulkan -->
<!-- index: DX12 -->
<!-- index: Metal -->
<!-- index: WebGPU -->
<!-- index: WebGL2 -->
<!-- index: Quality tier -->
<!-- index: VeryLow -->
<!-- index: Cinematic -->
<!-- index: Upscale quality -->
<!-- index: TAAU -->
<!-- index: TSR -->
<!-- index: FSR 3 -->
<!-- index: DLSS -->
<!-- index: XeSS -->
<!-- index: Frame generation -->
<!-- index: Present mode -->
<!-- index: Vsync -->
<!-- index: VsyncRelaxed -->

The renderer is a forward+ renderer behind `regia_render::RenderPlugin` (added by
`DefaultPlugins`). It covers PBR materials, sun and local lights, shadows, an IBL sky, post
processing (AgX tonemap, bloom, TAA), and — for the high tiers — temporal upscaling and atmosphere
(`docs/specs/13`, `docs/specs/26`).

The frame runs on the graphics API the platform opens (`regia_render::backend_policy`):

| Platform | APIs the instance opens | What the default pick uses |
|---|---|---|
| Windows | Vulkan and DX12 | A Vulkan hardware adapter when one exists. DX12 is the fallback, or the adapter `REGIA_GPU_INDEX` or `REGIA_GPU_NAME` names. |
| macOS (testing builds) | Metal | wgpu's adapter pick. Metal is the testing backend (ADR-009, ADR-264). macOS is not a dedicated-server package target. |
| Linux, Android | Vulkan | Vulkan. |

GL is never opened. A persistent pipeline cache (`.fpc`) applies on Vulkan. DX12 and Metal rely
on the driver's own disk cache. The product viewer (`regia_web`) is a separate path: it draws on
a page canvas with WebGPU, or with WebGL2 when WebGPU is missing or the page asks for
`backend: "webgl2"`. That viewer is not a general web game. A general web game target stays on
the version ladder.

DLSS and XeSS are optional link crates (`regia_dlss`, `regia_xess`) against SDKs that are not in
this repository. A build without the SDK falls back to the built-in upscalers. See
[Vendor upscalers](#vendor-upscalers) below.

Task procedures: [Quality and upscalers](../how-to/picture/quality-and-upscalers.md),
[Light a level](../how-to/picture/light-a-level.md),
[Materials](../how-to/picture/materials.md).

Related chapters: [The default look](default-look.md) (what a frame looks like before you touch
anything, and how to tune it), [Lighting, shadows and GI](../systems/lighting.md),
[Automatic quality detection](auto-quality.md),
[Dynamic resolution](dynamic-resolution.md), and the
generated [render profile table](../reference/render-profiles.md). That table lists the six
real-time presets. It omits the VeryLow tier and the Cinematic profile.

## Render profiles

A profile is a named bundle of quality settings:

```rust
use regia::prelude::*;
use regia::render::{RenderProfilePreset, RenderSettings};

fn configure(app: &mut App) {
    app.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
}
```

Real-time presets (`RenderProfilePreset::ALL`): `MobileLow`, `MobileHigh`, `DesktopLow`,
`DesktopHigh`, `DesktopUltra`, and the Steam Deck `Deck`. `Cinematic` is a seventh preset
(`RenderProfilePreset::WITH_OFFLINE`). It is Desktop Ultra with native scale, 8192² sun cascades,
every cloud texel marched, the light caps at their maximum, and mesh LOD held toward LOD 0
(ADR-1403). No menu and no detector pick it. `regia_render::cinematic::gate_preset` runs it on a
desktop whose detected tier is High or Ultra. On Android the applied preset is Mobile High
(`cinematic_needs_desktop`). On a detected tier below High the applied preset is that tier's own
(`cinematic_tier_too_low`) unless the caller passes `force`. Hosts read `--render-profile`, then
`REGIA_RENDER_PROFILE`, then `render_profile.txt` in the scene directory.

Desktop High and Ultra render at native resolution with temporal AA. A game may set Desktop
High's `render_scale` to 0.67 for a temporally upsampled (TAAU) performance budget, which ADR-041
introduced and the default stopped using on 2026-10-07 because that TAAU loses thin geometry and
floor reflections. The Medium quality tier is that 0.67 scale with Balanced lighting. Deck and
Mobile Low render at 70%, upsampled. The [render profile table](../reference/render-profiles.md)
is generated from the six real-time presets and lists the main values; not every `RenderProfile`
field has a row (for example `gi_probes`, `texture_streaming`, `contact_shadows` and
`emissive_lights` do not), so read the rustdoc for the rest.

The user-facing ladder is `QualityTier`: `MobileLow`, `MobileHigh`, `Deck`, `VeryLow`, `Low`,
`Medium`, `High`, `Ultra`. `VeryLow` is not its own preset. It is Desktop Low at render scale
1/3 with the SGSR 1 spatial upscaler and a 384 MiB lazy texture budget (ADR-7882). Content
budgets stay on Low, and `RenderSettings::preset` still answers Desktop Low. The saved menu
value is `GraphicsPreset` (`Low`, `Medium`, `High`, `Ultra`, `Clean`, `Deck`, `VeryLow`).
`Clean` renders as High. On Android, Low and VeryLow become Mobile Low and every other menu
preset becomes Mobile High. Individual profile fields can still override the preset. See
[Automatic quality detection](auto-quality.md) and the
[agent guide](../agents/framework.md).

## Upscale quality

<!-- index: Native -->
<!-- index: Ultra Performance -->

`RenderProfile::upscale_quality` is `None` on every preset, so `render_scale` applies.
`Some(UpscaleQuality)` replaces that scale. The ratio is display size divided by render size,
as an exact fraction, rounded to the nearest integer (halves up, at least 1):

| Mode | Ratio | Permille |
|---|---|---|
| `Native` | 1/1 | 1000 |
| `Quality` | 3/2 | 667 |
| `Balanced` | 17/10 | 588 |
| `Performance` | 2/1 | 500 |
| `UltraPerformance` | 3/1 | 333 |

The mode sets the size. On a TAA frame the upscaler is still `temporal_upscaler`. On any other
frame it is `spatial_upscaler` (`Bilinear`, or `Sgsr` on VeryLow). Dynamic resolution, when a
host applies it, stays inside the mode's permille range (`DynResConfig::within_quality`).

## Temporal upscalers

`temporal_upscaler` is consulted when anti-aliasing is TAA. The default on every preset is
`Taau` (neighbourhood clamp, bilinear history). `Tsr` is the reconstruction upscaler
(depth-dilated motion, disocclusion, variance clipping, thin-feature locks, a reactive mask).
`Fsr3` is the FSR 3.1 upscaler ported from the AMD FidelityFX SDK v1.1.4. All three are compiled
into every build. The opt-in example under [Renderer features](#renderer-features-and-opt-ins)
sets `Tsr`.

## Vendor upscalers

`VendorUpscaler` is separate from `TemporalUpscaler` so a host's exhaustive match on the built-in
enum keeps compiling. `FrameRenderer::set_vendor_upscaler` tries DLSS or XeSS on a single-sample
TAA frame and falls back to `temporal_upscaler` when the vendor cannot run.
`vendor_upscaler_report` records the reason. Agents read the same facts from `render.get_upscalers`.

- **DLSS** (`regia_dlss`). Optional `sdk` feature plus an external `DLSS_SDK` path. NGX on Vulkan,
  Linux and Windows. Android and macOS never link. A missing runtime, a non-NVIDIA adapter, or a
  DX12 device (`dlss_needs_vulkan`) uses the built-in upscaler.
- **XeSS** (`regia_xess`). Optional `sdk` feature plus an external `XESS_SDK` path. Windows
  Vulkan. Other targets are not linked. Without the SDK the runtime answer is `not_linked`.

The SDK binaries are not in the repository. Refusal codes live in `regia_render::upscaler::reason`
and on [Upscalers (agents)](../agents/upscalers.md).

## Frame generation

Frame generation presents one interpolated frame between two rendered frames. It is a WGSL port
of the FidelityFX SDK v1.1.4 interpolation and optical-flow shaders, paced on Regia's wgpu
surfaces. No vendor binary is involved. It stays off until `FrameGenSettings.enabled` is set, and
the cargo feature `frame-gen` has to be compiled in (`not_compiled` otherwise). A headless app
reports `headless`. A device without the storage-texture and storage-buffer counts, or a GL
device, reports `needs_desktop_gpu`. The default pauses generation when the rendered rate is
under 55 fps. The HUD is redrawn on the generated image. It is not interpolated. The agent method
is `render.frame_gen`. See [Frame generation (agents)](../agents/frame-gen.md).

## Present modes

<!-- index: Immediate -->

`PresentMode` on the primary window is `Vsync` (wait for vertical blank; the default),
`VsyncRelaxed` (vsync, and a late frame may tear), or `Immediate` (no wait; the image may tear).
`GameSettings.display.vsync` applies live: on selects `Vsync`, or keeps `VsyncRelaxed` when that
was already the window's mode; off selects `Immediate`. When the window asks for `Immediate`,
the swapchain takes the first unsynced mode the surface offers (`Immediate`, then `Mailbox`),
and FIFO when the surface has none. `Mailbox` and FIFO are surface modes, not `PresentMode`
variants. `DisplayApplied` reports the window mode that stuck and the swapchain summary.
On Android, off is refused (`android_display_paced`): frames stay on the display's vsync grid,
and the rate comes from `display.frame_cap`. A surface with no unsynced mode keeps FIFO
(`no_unsynced_mode_offered`). A host variable-refresh policy picks its own mode
(`host_vrr_policy`).

## Renderer features and opt-ins

Two profile fields are off in every preset, because turning them on changes frames (ADR-041:
look or performance changes are opt-in). A game turns them on per profile:

```rust
use regia::prelude::*;
use regia::render::{RenderProfilePreset, RenderSettings, TemporalUpscaler};

fn configure(app: &mut App) {
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopHigh);
    settings.profile.temporal_upscaler = TemporalUpscaler::Tsr;
    settings.profile.visibility_buffer = true;
    app.insert_resource(settings);
}
```

- `temporal_upscaler`: `Taau` (the default), `Tsr`, or `Fsr3`. `Tsr` is a reconstruction upscaler
  with depth-dilated motion, disocclusion handling, variance clipping, thin-feature locks and a
  reactive mask. `Fsr3` is a port of AMD's FSR 3.1 upscaler, kept as the comparison baseline for
  `Tsr`. They only apply when the profile's anti-aliasing is TAA.
- `visibility_buffer`: rigid opaque meshes that have meshlets draw through a visibility buffer
  with cluster LOD (GPU culling against a Hi-Z pyramid). It is meant for dense scatter; frames
  change by the LOD cut.

Texture streaming is already on in every preset, with a resident budget from
`TextureStreaming::for_preset` (Desktop High: 4096 px cap, 2 GiB, eager). Standard-surface
materials upload low mips first, step up as the screen needs them inside a per-frame upload
budget, and fade each level in. Material-graph materials stay fully resident. To override, set
`settings.profile.texture_streaming` to another value (for example `TextureStreaming::OFF`, or
`TextureStreaming::DECK` for a 2048 px cap); `TextureStreaming::DESKTOP` is an unbudgeted
low-mips-first ramp and drops the preset's cap.

### Mip streaming metadata

The cook looks at each texture's mip chain and records the largest size at which a level still adds
visible detail: a flat roughness map is capped at 16 px, a photo is not capped at all (ADR-1942). The
streamer keeps levels above that size out of memory when no map of the material needs them, so a flat
map costs what a small one costs and looks the same. Every map of a material drops the same number of
levels from its own top, so one detailed map keeps the levels of the whole material. The check is on the
single worst texel, so a small logo on a flat field keeps every level. A material with one map cooked
before this keeps all its levels.

Two `.meta` settings override the streamer per texture:

```ron
(stream: Some(false), stream_min_px: Some(512))
```

- `stream: Some(false)` keeps the texture at the tier's cap: a menu backdrop, or a texture that must
  never show a low mip. The whole material follows, since a material's maps stream together.
- `stream_min_px` raises the smallest size the texture drops to. A value below the tier's own floor
  changes nothing.

Check a build with `assets.cooked_pack { "inspect": true }`, which counts `mip_metadata_textures`,
`mip_capped_textures` and `unstreamed_textures` per pak.

## Local lights and interiors

Desktop High uploads 256 point and spot lights per view, Desktop Ultra 512, Deck 96. The lab
tools stay at 64. A larger set is ranked and the drop is logged; lights are not
cut off in silence. Spot lights that cast shadows come from the same local-light
atlas as point lights. A point light uses six tiles, so not every point can cast
a shadow at once.

A cockpit or a tunnel needs an interior volume (or an `EnclosureScene` shell) and
an exposure around EV 8. The ambient floor is 40 lux, a floor under the lights,
not a flat fill. At a sunny EV 15 that floor is tonemapped to black, which is the
pure-black console. `build_interior_lighting` places the probe grid and traces one
bounce of the local lights (a miss stays black). Bind that field and one reflection
probe. Auto-exposure lifts a fully indoor view. A view that also contains the sun
stays at the outdoor exposure unless the volume forces the interior shade.

## More than one directional light

A frame draws up to four `DirectionalLight`s: the sun (the lowest entity, with the cascaded
shadow map) and three more, unshadowed. Meshes, terrain and grass, the probe GI and reference
traces of the frame all take the extra lights. A `.bake.ron` bake description lists up to
four suns since format v3 (`suns: [..]`), and every bake set up from it takes them all; the
level export and the editor's bake panel still write one sun. The agent method `render.lights` lists which paths take them
(`extra_directional_paths`) and which do not (`sun_only_paths`); see
[Render lights](../agents/render-lights.md).

## Fog

Distance and height fog lives on `RenderSettings.fog` (exponential height fog per Hoffman &
Preetham, GDC 2002):

```rust
use regia::prelude::*;
use regia::render::{FogSettings, RenderProfilePreset, RenderSettings};

fn configure(app: &mut App) {
    // Desktop Low: no LUT sky, so `FogSettings` reaches the meshes. On Desktop High and
    // Ultra the atmosphere's aerial LUT supplies the haze; tune `VolumetricFogSettings` there.
    let mut settings = RenderSettings::from_profile(RenderProfilePreset::DesktopLow);
    settings.fog = FogSettings::outdoor_haze();
    app.insert_resource(settings);
}
```

See the [agent guide's fog page](../agents/fog.md) for the formula and how goldens verify it.

Desktop High and Ultra also run a physically based atmosphere, froxel fog and a volumetric
cloud layer. Mobile, Desktop Low and Deck stay on the analytic sky plus this height fog.
Put a `TimeOfDay` resource in the world for the sun, `VolumetricFogSettings` and `FogVolumes`
for participating media, and `CloudSettings` for the cloud layer. `gpu_timings().report()`
prints every pass, including `atmosphere`, `clouds` and `fog`. See the
agent pages for the controls.

### Planet presets (per map)

Each level picks its planet in `LevelSettings.atmosphere` (scene format v2; older levels load
as Earth):

```ron
"regia_scene::level::LevelSettings": (
    gravity: (0.0, -1.31, 0.0),
    atmosphere: (
        preset: Airless,                 // Earth | Mars | Airless
        time_of_day: Some((hour: 9.0, day_of_year: 172, latitude_deg: 10.0)),
        sun_illuminance_lux: None,       // None: the preset's (Earth 120 klx, Mars 52 klx, Jupiter 4.4 klx)
        cloud_coverage: 0.0,             // ignored when Airless
        cloud_type: 0.5,                 // 0 stratus, 0.5 cumulus, 1 towering
        sky_body: Some((                 // Jupiter from Europa
            direction: (-0.55, 0.42, -0.72),
            angular_diameter_deg: 12.0,
            tint: (0.62, 0.55, 0.47),
            bands: 1.0,
            fill_lux: None,              // planet-shine; None: physical at the current phase
        )),
    ),
),
```

- **Earth** — blue sky, orange sunsets, aerial haze.
- **Mars** — thin CO₂ and dust: a butterscotch day sky, a blue glow around a low sun, dust haze.
- **Airless** — a black sky with stars, a small hard sun, no haze, no clouds, no fog unless you
  author a fog volume. Ambient light is the ground bounce, so shadows are deep.

The sky body is lit by the sun, so its phase changes with the time of day.

It also lights the ground. **Planet-shine** is a shadowless directional fill from the body
(Jupiter-shine on Europa). Meshes, terrain and scatter all take it as Lambert diffuse, so faces
in the sun's shadow that face the body are not black.

- With `fill_lux: None` the engine uses the physical value: sun illuminance × the body's albedo
  × sin²(half its apparent diameter) × the Lambert phase law. That is about 30 lx for Jupiter
  at full phase under Europa's 4.4 klx sun.
- `Some(lux)` sets an authored value (DO27's Bore Twelve uses 1600 lx).
- The fill is tinted by the body's `tint`. Mars and Airless use
the physical sky on every profile that renders HDR; Earth uses it on Desktop High and Ultra.
`regia upgrade-project --write` rewrites v1 levels with the Earth block spelled out.
`render_lab planets` renders the five reference shots in `docs/evidence/m15c/`.

## Cameras

A camera is an entity with a camera component and a rig (see [Gameplay](gameplay.md)). Exposure,
field of view and post settings are reflected fields; pin `Camera.exposure_ev` and turn off
auto-exposure when you need deterministic screenshots.

Auto exposure reads a luminance histogram on the GPU and applies it two frames later, blended
in log space, so a frame never waits for that read. The image adapts over a few frames instead
of stepping. Goldens and byte-identical captures leave auto exposure off. Metering, the EV100
clamp and the default grade are described in [The default look](default-look.md).

### Lens effects and bloom

A camera with a `PhysicalCamera` asks for motion blur and depth of field; the quality preset must
allow each (Desktop High, Ultra and Deck). What the passes guarantee (ADR-1152):

- **Bloom** keeps a highlight's energy and position. Each bloom level is half the one above,
  rounded up, and its texels cover exactly 2 × 2 texels of the level above, so a bright point away
  from the frame border glows with the same total energy, centred on itself, wherever it sits and
  at any frame size. A small light moving by a pixel moves its glow by a pixel; the glow does not
  pulse or jump. A flat image stays flat up to the border.
- **Motion blur** is capped at `MotionBlur::MAX_BLUR_FRACTION` (1/8) of the frame height: a
  teleported object, a hitch frame or a spinning prop blurs over at most that length around itself.
  A long blur takes more samples than `samples` asks for (up to `MotionBlur::MAX_SAMPLES` per
  direction), so its taps stay about 4 px apart and the trail is continuous. Motion vectors that
  are NaN or infinite read as no motion.
- **Depth of field** caps the largest circle of confusion at
  `DepthOfField::MAX_CIRCLE_OF_CONFUSION_DIAMETER` (128 px): the blur loops are as long as the
  radius, so an unbounded value hung the GPU. A blurred foreground spreads its full radius over
  the sharp background behind it, for any circle up to the cap. An infinite far plane is supported.
- NaN or infinite colour texels (a broken material) are skipped by the blur taps, so they stay one
  pixel and do not spread over a trail or a circle of confusion.

## Headless and deterministic capture

The renderer runs **offscreen** — no window, no compositor — with `RenderPlugin { headless: true }`.
Combined with a fixed timestep and the agent protocol's `play.step`, two runs produce
byte-identical PNGs on lavapipe. That is the golden-image and CI path; see
[the agent guide's headless-capture page](../agents/headless-capture.md).

## Profiling

- `regia_core`'s `tracy` feature enables the Tracy profiler.
- Per-frame systems carry `tracing` spans; the `Profiler` resource holds the process-wide frame
  profiler, and `play.get_diagnostics` reports fps, frame times and draw calls.
- GPU passes record timestamp queries named after the render-graph node (`docs/06` § 2).
  `render_lab outdoor` prints atmosphere, cloud and froxel-fog times on a Desktop High frame.
