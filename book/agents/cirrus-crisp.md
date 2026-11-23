# Crisp cirrus

A fibrous cirrus sheet ([Fibrous cirrus](cirrus-fibrous.md)) is drawn once per cloud-march texel and prefiltered
over that texel, so its strands are never narrower than about two texels, and its broad warp and bands come from
bilinear texture reads. One optional value, `crisp` (ADR-1516), makes the strands sharper and the sheet closer to a
photograph of fibrous cirrus from the ground. **It defaults to 0 (off)**: a sheet without it draws exactly as before,
and scenes that do not set it (the base look included) are pixel-identical.

## Value

| Value (`CirrusSheet` / agent / `sky.ron`) | Range (default) | What it does |
|---|---|---|
| `crisp` / `cirrus_crisp` | 0..1 (0) | Crisp, photographic strands. Above 0 the sheet is fibrous (like any other fibrous value). |

How `crisp` blends in (the amount scales every change; nothing but the supersampling cost steps):

- **Supersampled sheet (steps on above 0).** The sheet is the mean of 4 samples (`CIRRUS_CRISP_SAMPLES`) through
  the texel's R2 points. Each sample is prefiltered over `1 / √4` of the texel, so a strand can be about half a
  texel wide without aliasing. A trail's cross profile is widened to at least one sample footprint
  (`CIRRUS_CRISP_FILTER`), not two. The sample count is 1 at `crisp = 0` and 4 for any amount above 0: the cost
  steps, the image does not.
- **Smooth warp and bands (the saw-tooth fix).** A bilinear texture read is piecewise linear. The sheet's curl warp
  therefore bent every trail at each curl texel edge (about every 0.4 streak tile along a trail), and slanted
  perspective turned those kinks into saw-tooth corners. The band edges were polygons too. A crisp sheet reads the
  curl and both band octaves through a cubic B-spline (C2, four bilinear taps each), cross-faded from the bilinear
  reads by the amount: `crisp = 0.2` is 80 % bilinear, 20 % B-spline, so a look sweep through small values changes
  the sheet continuously. The trails bend smoothly at full crisp.
- **Sharper strands.** The strand ridge gets as sharp as the sample footprint allows: `k = 0.45 × spacing /
  footprint`, from the fibrous 3.2 up to 7 (`CIRRUS_CRISP_RIDGE`), scaled by the amount. Less ice is left between
  strands (0.04, not 0.15). The strand modulation is renormalised for each `k`, so a trail keeps its mean.
- **Contrast at the same optical depth.** The veil between trails drops to 40 % (`CIRRUS_CRISP_VEIL`). Strand peaks
  can reach 3× the normalised density (`CIRRUS_CRISP_CAP`) instead of being clipped at 1. The sky's area-mean
  optical depth stays within about 8 % of the fibrous sheet's, so the sky does not brighten overall. The ice is
  concentrated into the strands, and the gaps between them clear.

Cost: 4 sheet evaluations per march texel instead of 1 on crisp sheets (above 0), plus one extra bilinear tap per
band/warp texture while cross-fading (none at 0 or 1).

On the showcase shot s1v t11 (optical depth 0.11), crisp 1 roughly doubled the strand contrast over the fibrous sheet
(the 99th percentile went from 8 to 17 code values above the sky), and the sky's mean stayed the same. The uncinus look
with hanging fallstreaks is the fibrous values fiber 1, fiber scale 120 m, shear 0.9, breakup 0.75 and hook 0.9, with
crisp 1.

## Agent

```json
{"method": "render.set_clouds", "params": {"cirrus_crisp": 1.0, "cirrus_fiber": 1.0, "cirrus_fiber_scale_m": 120,
  "cirrus_shear": 0.6, "cirrus_breakup": 0.6, "cirrus_hook": 0.45}}
```

- `render.set_clouds` takes `cirrus_crisp`. It is clamped to 0..1. With a `Weather` driving the clouds it is pinned
  over the genus look until `clear_look_override: true` (ADR-1492).
- `render.get_clouds` reports `look.cirrus.crisp`. Pinned values appear under `look_override.fields`.
- Errors: a value that is not a finite number answers `F0002`. No new error codes.

## Scene file

The showcase `sky.ron` `cloud_look` block takes `cirrus_crisp: Some(1.0)` next to the fibrous names. Files without it
parse and draw as before (`version: 1`, no migration).

## Rust

```rust
use regia_render::{CloudGenus, CloudSettings};

fn crisp_cirrus() -> CloudSettings {
    let mut s = CloudSettings::default().with_genus(CloudGenus::Cirrus);
    s.look.cirrus = s.look.cirrus.with_fibrous_look().with_crisp(1.0);
    s
}
```

`regia_render::cloud_types::cirrus_density_fibrous` is the CPU mirror of the shader's density, crisp included;
`cirrus_bspline_axis` mirrors the shader's B-spline tap placement.
