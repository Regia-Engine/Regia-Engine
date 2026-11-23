# Baked GI sampling: ZH3 and the dominant direction

A baked GI volume ([Baked GI volumes](baked-gi.md)) stores, per probe and light component, L1
spherical harmonics of the radiance around the probe: a mean and one gradient per color channel.
How the lookup turns that into light on a surface decides how much directional contrast survives.
v0.2.0 adds two ways to read more out of the same data (ADR-1377). Neither changes the `.fgiv` file.

## ZH3 from L1 (the baked volume's diffuse light)

Plain L1 irradiance flattens light that arrives from a narrow direction. It reads about 25 % low
toward a small bright source (a window, a sunlit patch on a wall, a sky body) and goes negative
behind it, where the lookup then clamps it to black. The quality pass of ADR-861 replaced the
negative lobe with a non-negative one (`deringing`). That fixed the black, but the peak stayed low.

With ZH3 on (opt-in, see *Why it is off by default*), the lookup adds a quadratic zonal harmonic
(ZH3) along the light's main direction, after Roughton et al., "ZH3: Quadratic Zonal Harmonics"
(I3D 2024). This is the variant that stores nothing more: the zonal coefficient is estimated
("hallucinated") from the L1 band.

- **Axis.** The luminance-weighted L1 direction, shared by the three channels, so a colored source
  keeps its color in every direction.
- **Coefficient.** From each channel's L1 on that axis, `t = |L1·axis| / (√3·L0)`: 0 for uniform
  light, ½ for a hemisphere of light, 1 for a single direction. The curve `√5·t(2t − 1)` (0 below
  `t = ½`) is the band-2 coefficient of a uniform cap of light with that ratio. The paper fits its
  curve to a set of environments; this one is exact for every cap up to a hemisphere.
- **What stays the same.** Uniform light (the furnace), a sky, and any probe whose L1 is no
  stronger than a hemisphere's read exactly as L1, bit for bit. Only directional probes change.
- **Clamp.** The result is clamped at 0 as before. A single point source still dips to about −4 %
  of its peak before the clamp, where the true light is 0.

Error against the exact clamped-cosine convolution of a uniform cap of light (RMS over the sphere,
relative to the peak; `crates/regia_render/src/baked_gi/zh3.rs` tests the 2° case and that ZH3 is
never worse than L1 on any cap):

| Cap half-angle | L1 | L1 de-ringed (ADR-861) | ZH3 |
|---|---|---|---|
| 2° (near a point) | 0.128 | 0.067 | 0.033 |
| 30° | 0.106 | 0.037 | 0.014 |
| 60° | 0.059 | 0.007 | 0.005 |
| 90° (hemisphere) | 0 | 0 | 0 |
| 120° | 0.053 | 0.053 | 0.053 |

ZH3 replaces the de-ringing: with ZH3 on, `BakedGiSettings::deringing` has no effect.

### Why it is off by default

Against the path tracer (`cargo xtask gi-compare`, 256 spp, RTX 5090), ZH3 against the default
de-ringed L1 moved the mean FLIP by:

| Fixture | Views | Full FLIP, L1 → ZH3 | Indirect FLIP, L1 → ZH3 |
|---|---|---|---|
| cornell | front, inside, corner | 0.3108 → 0.3116, 0.4231 → 0.4237, 0.4111 → 0.4115 | 0.2066 → 0.2076, 0.2362 → 0.2371, 0.1898 → 0.1901 |
| leak_room | inside_door, inside_back, outside | 0.1140 → 0.1138, 0.1180 → 0.1179, 0.0166 → 0.0166 | 0.1295 → 0.1294, 0.1348 → 0.1346, 0.0451 → 0.0451 |
| rocks_capsule | overview, capsule, low_side | unchanged | unchanged |

The changes are small, at most 0.0009 and well inside the tool's 0.01 regression tolerance, but
cornell gets slightly worse. So ZH3 stays opt-in until a by-eye review on real maps (trench walls,
doorways, set-piece undersides) decides it. Outdoor probes under the sky are unchanged by design.

## The dominant direction (probe specular)

A glossy or rough reflection averages the light over the GGX lobe, and the lobe's main direction
leans from the mirror direction toward the normal as the roughness grows (Lagarde & de Rousiers
2014, § 4.9.3). Probe specular (`render.set_probe_specular {mode: "lobe"}`, the default since
ADR-1103, guide page [probe-specular](probe-specular.md)) already reads the probes along that lobe
on meshes at every roughness. Since v0.2.0 the terrain does too. Its sky reflection under the
probes takes the probe light in the specular lobe instead of the probes' irradiance toward the
normal. That matters most on wet, glossy ground facing a lit wall. Uniform light reads the same
either way. With `mode: "rough_only"` both meshes and terrain keep the earlier look for comparison, and `mode: "lobe_hue"` keeps the lobe's hue under a colored sky (ADR-2996), ZH3 included.

The same release fixes the probe specular screen lookup (`ddgi_screen_gi`) at the Full and Quarter
lookup resolutions. It read the Half lookup's texels at every resolution, so with a diagnostic
Full or Quarter lookup the forward pass's probe light came from the wrong pixels. The default
(Half) was not affected.

## Host API

- `BakedGiSettings::zh3` (default `false`). Set it with `FrameRenderer::set_baked_gi_settings`.
  `false` is the L1 evaluation, de-ringed as `deringing` says.
- `regia_render::baked_gi::zh3_irradiance_over_pi(&sh, n)` evaluates L1 SH (`ShRgb`) with the
  ZH3 term on the CPU, as the shader does. `sample_static_gi` and the other CPU lookups follow the
  setting.
- `cargo xtask gi-compare --reconstruction zh3|l1` renders the baked look under each evaluation
  against the path tracer ([gi-compare](gi-compare.md)). `l1` is the default.

## Agent methods

- `render.set_gi_sampling {"zh3": true}` switches to the ZH3 evaluation, and `{"zh3": false}`
  switches back to L1. It returns `{ before: { zh3 }, after: { zh3 }, deringing, volume_installed, note }`
  and applies from the next frame.
- `render.get_gi_mode` reports the setting as `baked.lookup.zh3`.
- `render.sample_baked_gi {points, zh3?}` overrides it for one call, so the two evaluations can be
  compared at points without a frame. The result's `lookup.zh3` says which one ran.
- Errors: `F0002` (invalid params) when `zh3` is missing or not a boolean. `F0003` (not found) when
  the app has no runtime renderer.

## Checking it

1. On a baked map, `render.get_gi_mode`. `baked.active` is `true` and `baked.lookup.zh3` is `false`.
2. Frame a surface lit by bounce through an opening: a wall facing a doorway, a trench wall facing
   the sunlit side, a set piece's underside. `play.pause`, then `seeing.screenshot`.
3. `render.set_gi_sampling {"zh3": true}`, `play.step 1`, and screenshot again. The ZH3 look has more
   directional contrast: the side facing the opening is brighter, and the side facing away is
   darker. Open ground under the sky looks the same in both.
4. For numbers, `render.sample_baked_gi` at the same points with `zh3: true` and `zh3: false`.
5. Switch back with `{"zh3": false}`.
