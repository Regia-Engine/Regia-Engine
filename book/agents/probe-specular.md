# Probe specular

Where probe light covers a surface (the Depth look's GPU-traced probes, or a map's baked GI volume:
see [Lighting option](gi-probes.md) and [Baked GI volumes](baked-gi.md)), the forward pass reads the
surface's indirect specular from that light at every roughness (ADR-1103):

- **A mirror** reads the probe light in the mirror direction.
- **A rough surface** reads a wide cone of probe light that leans toward the normal. At full
  roughness it meets the diffuse lookup.
- **The sky map** adds only its detail where image-based sky light is bound, such as a bright
  horizon band in a glossy floor. That detail is the ratio of the prefiltered sky to the sky's own
  smooth light in the same lobe, capped at `detail_max` (8).

Probe light sets the level and the color. A glossy floor in a canyon, a room or under a hull
therefore reflects its walls, not the open sky. Before this change, surfaces below perceptual
roughness 0.5 reflected the sky map, darkened by the probe/sky irradiance ratio, and only rougher
surfaces read the probes.

## How it reads the probes

The probes store light that is linear in direction, `L(l) = a + b·l` (L1 spherical harmonics; the
half-resolution screen lookup holds six irradiances toward ±x, ±y, ±z). For such light, the average
over any lobe is exactly `a + b·m`, where `m` is the lobe's mean vector. The engine uses a fit of
that mean vector for the GGX specular lobe (BRDF × cosine, height-correlated Smith). Its direction
moves from the mirror direction to the normal as roughness grows, and its length falls from 1 to
about 0.62. The worst error of the fitted vector against numerical integration is 0.036 (α in
[0, 1], n·v ≥ 0.1).

Energy checks, each a unit test (`regia_render::probe_specular`):

- Uniform light comes back unchanged at every roughness (white furnace).
- The rebuilt light is never negative and never more than 4× the cube's mean, which is the most a
  non-negative field can send.
- Over a pair of opposite lobes no light is made while the clamp at 0 is idle.
- The detail ratio's cap only removes light.
- The lobe's own albedo stays at or below 1 and matches the engine's GGX directional albedo.

## Settings

- `DdgiSettings::specular` and `BakedGiSettings::specular` (`regia_render::ProbeSpecular`):
  `Lobe` (the default), `RoughOnly` (the earlier look, for comparisons) or `LobeHue` (opt-in, see
  [Keeping the hue](#keeping-the-hue)). Each one applies to its own probe source and takes effect
  on the next frame. The probes themselves do not change.
- When a level installs its own baked volume, it restores the host's baked settings (`specular`
  included) when it unloads.

## Agent methods

- `render.get_probe_specular` returns
  `{ mode, ddgi, baked, detail_max, hue_floor, baked_active, ddgi_active }`. `mode` is `"lobe"`,
  `"rough_only"`, `"lobe_hue"`, or `"mixed"` when the two sources differ.
- `render.set_probe_specular {"mode": "lobe"|"rough_only"|"lobe_hue"}` sets both sources and returns
  `{ before, after, note }`. An unknown or missing `mode` is `invalid_params` (F0002). Without a
  runtime renderer the call returns `not_found` (F0003).
- `render.probe_specular_lobe {"roughness": 0.4, "n_dot_v": 0.7}` returns the fitted lobe (`lerp`,
  `length`, `fit`) and the numerically integrated one (`reference: { mean, length, albedo }`). It
  also returns their distance as `error`, which is under 0.04 over the range a frame meets. It
  needs no frame. `roughness` is perceptual, in [0, 1] (α = roughness²). `n_dot_v` is in [0, 1]
  and defaults to 1. A value out of range is `invalid_params` (F0002).
- The same method with `cube` (six `[r, g, b]` probe irradiances / π, +x, −x, +y, −y, +z, −z, each
  finite and not negative) also returns `light: { mode, radiance }`: the renderer's CPU mirror of the
  light those values give through the lobe. `mode` picks `"lobe"` (the default) or `"lobe_hue"`, and
  `direction` (`[x, y, z]`) aims the lobe's mean instead of `fit`. The GPU agrees with the mirror
  within 1e-4 of the value (`the_shader_matches_its_cpu_mirror_with_the_hue_bit`). A bad `cube`,
  `direction` or `mode`, `"rough_only"` included, is `invalid_params` (F0002). Without `cube`, `light`
  is `null`.

## Keeping the hue

`ProbeSpecular::LobeHue` (`"lobe_hue"`, opt-in, ADR-2996) changes three things about the lobe's color.
Nothing else in the frame moves, and a frame without probe light is unchanged.

- **A floor per channel.** The linear form `a + b·m` goes negative where the real light is only dim.
  Under a blue-heavy sky a mirror aimed at the nadir drove blue to 0 first and green next, which left
  a red-to-yellow spot, [202, 33, 0] on the studio sphere. In this mode each channel keeps at least
  `hue_floor` (0.2) of its mean. A floor per channel, not one scale for all of them, so a red wall
  and a green wall opposite it each keep their color at full strength. A lobe that falls below 0.9
  of its mean's luminance also takes its mean's color (fully at 0.4), its luminance kept: the channels
  of a dim lobe say little, and with the floor alone a lobe half way down read magenta.
- **The sky's detail per channel.** The ratio of the prefiltered sky to the sky's own lobe light is
  taken in each channel, not on luminance. Where probes and sky agree, the frame reads the prefilter's
  own color, which L1 light cannot hold: the near-white horizon of a gradient, a neutral ground under
  a blue sky. Each channel has the `detail_max` cap, and the ratio still fades to 1 at full roughness.
- **The ground plane in the lobe.** With `render.set_environment_ground` (or the host call
  `FrameRenderer::set_environment_ground`), traced probes hold the plane themselves: a probe ray that
  meets no surface lands on it, so the default's lobe reflects the plane too, in its L1 form. This
  mode also takes the plane into the sky's lobe and prefilter, so where the probes and the sky agree
  a mirror reads the plane's own radiance with a sharp horizon. With a baked volume, whose rays read
  the image's lower half, the probes' lobe takes what the plane hides out and puts the plane's light
  in, as the diffuse lookup does. See [Studio environment](studio-environment.md).

The floor applies wherever the probes' lobe is read, the terrain included. The per-channel detail and
the plane apply to meshes in the forward pass. The Adreno workaround shader
(`SUN_SHADOW_PCF_ONLY`) leaves those two out and keeps the floor.

## Comparing the two looks

1. Call `play.pause`. Point the camera at a glossy surface under probe light, for example a
   polished floor in a trench or a metal hull side in shadow.
2. Call `render.set_probe_specular {"mode": "rough_only"}`, then `play.step 2` and
   `seeing.screenshot`.
3. Call `render.set_probe_specular {"mode": "lobe"}`, then `play.step 2` and `seeing.screenshot`.
   For the hue, do the same with `"lobe_hue"`: a metal sphere under a blue studio gradient is the
   clearest case. A still camera can keep the previous mode's composite (the static-screen reuse,
   ADR-2996 (8)) until the frame fix lands: move the camera a little between the screenshots.
4. Judge the pair by eye against the reference plate. For ground truth, run
   `seeing.reference_trace` and `seeing.compare` with `aov: "indirect_specular"` (guide topic
   `renderer-excellence`).

## What to look for

- **Glossy surfaces in enclosures**: a shadowed polished floor reflects the lit walls around it,
  in their color, not a darkened blue sky.
- **Mirror-like surfaces in the open**: the sky's detail (horizon band, gradient) stays, because the
  probes and the sky agree there.
- **Rough surfaces**: little change. At full roughness the lobe meets the diffuse probe lookup.
- **The default under a blue sky**: a red-to-yellow spot where a mirror reflects straight down.
  `"lobe_hue"` removes it. The default stays until the owner's look call (ADR-2996).
- **A closed room with colored walls under `"lobe_hue"`**: the lower half of a metal sphere can take
  a warm tint (the floor share lifts the walls' red and green in a lobe that sees a dark floor). The
  mode is still closer to the path tracer than the default there by FLIP and RGB error (ADR-2996).
- **Known limit**: the probes hold smooth L1 light, so a mirror under probe light shows the
  probes' smooth level times the sky's detail, not the exact image of nearby objects. Sharp
  reflections of nearby geometry come from SSR and reflection probes.
