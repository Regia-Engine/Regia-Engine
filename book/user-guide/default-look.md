# The default look

<!-- index: AgX -->
<!-- index: Auto exposure -->
<!-- index: Default look -->

What a frame looks like before a game touches any lighting or post setting, why, and which
knobs move it. The defaults are physically based: a ~100 000 lux sun, a sky in cd/m², a
camera exposed in EV100. Get the light levels right and the look follows; most "it looks
flat" or "it looks washed out" problems are a light level, not a post setting.

The reference shots are `cargo run --release -p lookdev --bin default_look -- --out <dir>`
(props on snow, a vehicle in the air, a cockpit, a wide vista); their lavapipe goldens live in
`samples/lookdev/goldens/` and before/after frames in `docs/evidence/default-look/`.

## What you get by default

`RenderSettings::default()` is the Desktop High profile on a desktop (on a Steam Deck it is `Deck`, on Android
Mobile High; see [Steam Deck](steam-deck.md)). VeryLow, Medium, Low, Ultra, and the offline
Cinematic profile are choices a game or the detector makes later. They are not this default.
With Desktop High:

| Stage | Default | Setting |
|---|---|---|
| Display transform | AgX, then the default look grade (contrast 1.2, saturation 1.4) | `PostSettings::tonemap`, `contrast`, `saturation` |
| Exposure | Histogram auto exposure, keyed to middle gray, EV100 clamped to −2…15 | `PostSettings::auto_exposure`, `auto_exposure_min_ev100`, `auto_exposure_max_ev100`, `exposure_compensation` |
| Bloom | Energy-conserving blend toward the blurred image, 4 %; the blur keeps a highlight's energy and centre at any frame size | `PostSettings::bloom`, `bloom_mips`, `bloom_intensity` |
| Anti-aliasing | Temporal AA at native resolution (Desktop High and Ultra); 0.67 render scale with temporal upscaling (TAAU) is an opt-in budget | profile `render_scale`, `anti_aliasing` |
| Sun shadows | 4 cascades, PCSS (1° sun, 12 search + 16 filter taps; 6+8 only in the opt-in 0.67 budget), blended cascade seams, fade at the shadow distance | profile `shadow_filter`, `csm_*` |
| Ambient occlusion | View-space AO at half resolution, 1.2 m radius, multi-bounce, also occludes sky reflections. The forward pass samples that half-resolution texture directly | profile `ssao` |
| Diffuse GI | GPU irradiance probes (DDGI, on by default in Desktop High and Ultra; a CPU `GiField` is optional) + a screen-space bounce (half resolution only in the opt-in 0.67 budget), upsampled with a depth-aware filter. A held camera reuses the previous bounce | profile `gi`, `screen_space_gi` |
| Sky | Procedural sky behind geometry, with a sun disc and aureole; it is also the IBL | `Environment::ibl` |
| Haze | Aerial perspective: distance recedes into the horizon sky color. With the LUT sky (`physical_atmosphere`, Desktop High and Ultra) the atmosphere's aerial LUT supplies it and `FogSettings` is forced off for meshes; `FogSettings` applies when the LUT sky is off | `RenderSettings::fog` (`FogSettings::aerial()`), `VolumetricFogSettings` |

### Display transform

AgX maps middle gray (0.18 after exposure) to ≈ 0.5 on screen and rolls highlights off over
about four stops instead of clipping, so a sunlit snowfield stays white with detail and a
shadowed interior keeps its dark grays. Very bright saturated colors (a muzzle-flash core,
an overexposed red) desaturate toward white on purpose — that is AgX's "path to white". Base
AgX is muted; the default look adds an S-curve around mid gray and a saturation boost in
display space. Set `contrast` and `saturation` to 1.0 for the bare transform. `TonemapOperator::AcesFitted`
gives the punchier, hue-shifting filmic curve many games are used to; `Neutral` is near-linear.

The no-post forward path and the terrain shaders print with the same transform and the same
default grade, so meshes, terrain and water agree.

### Exposure

Auto exposure meters the log-average luminance of the frame between the 40th and 95th
percentile (deep shadow and specular glints do not drag it) and keys it to middle gray. The
clamp is what keeps snow white: a frame of sunlit snow meters around EV100 16, and the
ceiling of 15 (the photographer's "sunny 16") stops it from being printed gray. Lower the
ceiling for a darker, moodier daylight; raise `exposure_compensation` (in stops) to brighten
everything. A camera with `Camera::exposure_ev` pins the exposure instead.

If you pin the exposure yourself, meter it: EV100 ≈ 15 under a 100 klx sun, ≈ 14 in open
shade, ≈ 9–12 indoors. Values tuned before the 2026-10-06 transform fix (which printed middle
gray at 0.17) are 1–3 stops too bright now.

### Shadows

The sun uses percentage-closer soft shadows: a blocker search, then a filter whose width is
the gap between receiver and blocker times the sun's angular radius (1°), so contact shadows
are crisp and long shadows soften with distance from the caster. Cascade seams are blended
over the last 15 % of each cascade and the last cascade fades out instead of cutting off.
The kernels rotate per pixel and per frame; TAA accumulates them. Shadow bias is applied on
the receiver (normal offset plus a slope term), identically on every GPU.

### Ambient, AO and GI

Without probes, indirect light is the procedural sky's irradiance: blue from above, a neutral
ground bounce from below. With an irradiance field (`RuntimeRenderer::set_gi_field`, filled by
`update_irradiance` from an `SdfScene`), the probes *replace* the sky term inside the probe
box and fade back to it one cell outside: they already contain the open sky, the bounce off
sunlit ground, and the occlusion of roofs and walls. Call `update_irradiance` a few times
(or every frame with hysteresis) — each update adds a bounce. Put probes where the camera
spends time: around bases, inside cockpits, along trenches.

Screen-space AO darkens contact and creases (radius 1.2 m) and also darkens sky reflections
in crevices. The screen-space bounce adds light from nearby surfaces that face each other.

### Sky and haze

The sky is drawn wherever no geometry was, when IBL is on and the view is not composited over
an earlier one. The default fog is aerial perspective: an exponential height fog whose color
is the horizon sky radiance in the view direction plus a forward-scattering glow toward the
sun, integrated along the ray. About 10 % of the contrast is gone at 500 m and 35 % at 2 km
near the ground; it thins with altitude. `FogSettings::aerial()` is the starting point;
`density` scales the distance, `height_falloff` the altitude, `color` tints it. Set
`fog.enabled = false` for clear air, or `sky_inscatter = false` for the flat-color fog.
This module is a stand-in for the M15c atmosphere and is replaced by it.

## Tuning for a game

1. **Light levels first.** Keep the sun around 100 klx at noon (50 klx at low sun), the
   procedural sky as is, and let auto exposure work. Check a gray card (albedo 0.18) prints
   mid gray in sun.
2. **Give the probes a volume** around every place players look closely. Interiors without
   probes are lit by the open sky — bright and flat.
3. **Then grade.** `contrast` / `saturation` for the mood; `exposure_compensation` for
   brightness; `bloom_intensity` for glow; `auto_exposure_max_ev100` for how bright a sunny
   frame may be.
4. **Albedo matters.** Real snow is 0.8, concrete 0.4, asphalt 0.1. Albedos above 0.9 or
   below 0.03 read as fake under a physically correct transform.

The terrain renderer has its own `TimeOfDay` (sun, sky ambient in exposed units) and its own
flat-color fog; its default sky ambient is a clear sky (≈ 1 : 4 against the sun).
