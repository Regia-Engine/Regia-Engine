# Path tracer presets

The reference path tracer (`regia_pt`) has one honest operating point: many samples, every bounce, no tricks. That is
slow. Presets are named, documented trades of bias for speed, so a look can be iterated in seconds and the final
check still runs the plain tracer. They are a tool for lookdev, not a runtime feature: `regia_pt` is never part of a
game build.

```
seeing.reference_trace { "scene": "cornell", "preset": "preview" }
regia render --reference --scene cornell --preset draft
```

## The tables

| Preset | spp | bounces | RR start | indirect clamp | caustics | firefly bound | adaptive | denoiser |
|---|---|---|---|---|---|---|---|---|
| `draft` | 4 | 3 | 2 | 1e4 | suppress | 8x median | yes, 12 %, min 2 | built-in |
| `preview` | 32 | 6 | 3 | 5e4 | clamped (cap 1e5) | 16x median | yes, 5 %, min 8 | built-in |
| `final` | 256 | 12 | 3 | off | full | off | yes, 1 %, min 64 | none |
| `reference` | the caller's | the caller's | the caller's | off | full | off | no | none |

Cost never decreases from `draft` to `final` (spp, bounces and the Russian-roulette start are non-decreasing; a test
pins it). Clamps are luminance caps in the scene's radiance units (cd/m²); the table values suit sun-lit outdoor
scenes, so scale `clamp_indirect` down for a dim interior.

**`reference` changes nothing.** Its output is bit-identical to a trace made without presets: a test compares every AOV
plane bit for bit on the furnace and Cornell scenes. It uses the `spp`, `max_bounces` and `seed` you pass.

**Everything else is biased.** The clamp, the caustics cut and the firefly filter remove energy. Such an image is a
preview: it carries the denoiser key (`regia.denoiser`) in its EXR header and PNG, and `seeing.compare` refuses it as
the reference. A biased but undenoised image is marked `regia preset (biased) <name>` in the same key. Use `final` or
`reference` for ground truth. (`final` is unbiased apart from adaptive stopping.)

## Parameters of `seeing.reference_trace`

| Parameter | Meaning |
|---|---|
| `preset` | `draft`, `preview`, `final` or `reference`. |
| `spp`, `max_bounces` | With a preset: override the table's numbers (default: the table's). `reference` uses them (defaults 64 and 12). |
| `caustics` | `suppress`, `clamped` or `full`. |
| `clamp_indirect` | Luminance cap on light credited after the first path vertex; 0 = off. |
| `firefly_threshold` | A pixel above this multiple of its 3x3 median luminance is pulled down; 0 = off, else above 1. |
| `adaptive` | `true`/`false`. |
| `denoiser` | `none`, `builtin` or `oidn` (needs the `denoise` feature). |

`caustics`, `clamp_indirect`, `firefly_threshold`, `adaptive` and `denoiser` need a `preset`. The legacy `denoise: true`
(an extra OIDN preview) cannot be combined with `preset`; use `denoiser: "oidn"`.

The status adds `preset: { preset, spp_cap, pixel_samples, pixel_samples_uniform, tiles, denoiser, firefly_threshold, base }`.
`tiles` lists each adaptive band (`first_row`, `rows`, `spp`, `error`). When the preset post-processed the image,
`raw` points at the unfiltered accumulation (`reference.exr`) and `exr` at the preview (`preview.exr`); otherwise
`exr` is `reference.exr`.

CLI: `regia render --reference --preset NAME [--caustics MODE] [--denoiser KIND]`; `--spp` and `--bounces` are optional
and only override the preset when given.

### Errors

Bad values answer `invalid_params` with the reason: an unknown preset, caustics mode or denoiser name; a negative,
NaN or infinite clamp or threshold; a firefly threshold in (0, 1]; zero spp or more than 1 048 576; a bounce count of
0 or above 1024. `denoiser: "oidn"` in a build without the `denoise` feature answers `F0013` (use `builtin`).

## The built-in denoiser

An edge-avoiding a-trous wavelet filter (Dammertz, Sewtz, Hanika and Lensch, HPG 2010) with SVGF's variance-guided
luminance weight (Schied et al., HPG 2017; the spatial part only, no temporal accumulation). It needs no native library
and runs as a WGSL compute shader, with a CPU twin that the tests compare it against.

1. The beauty's directly seen light (the `emissive` and `sky` AOVs) is subtracted and added back unchanged. The
   rest is divided by the first-hit albedo (so texture detail and albedo edges survive). The preset's per-pixel
   variance of the mean is the average of two estimates: the half-buffer term `(a − b)² · h · (n − h) / n²`
   (the trace stops at half the samples, a snapshot is read back, and the second half is `(n·full − h·first) / (n − h)`),
   and the accumulator's second moment (the `variance` AOV, each channel weighted by the square of its luminance
   weight). A pixel with no second half uses the moment alone. That variance is then divided by the square of the
   demodulation factor's luminance.
2. Before the first iteration, each pixel's variance is replaced by the mean of a 5×5 window of neighbors on the
   same surface (normal dot above 0.9 and depth within 5%, or both depths 0). Five iterations at step 1, 2, 4, 8
   and 16 (`AtrousSettings::default`) apply the B3-spline kernel. The normal weight is `dot^128`. The depth weight
   is `exp(−|dz| / (4·plane + 0.001·max(z, 1)))`, where `dz` is the depth difference and `plane` is the depth
   gradient's predicted step. The luminance weight is `exp(−|dl| / (3·sqrt(v) + 0.0001))`, where `v` is a 3×3
   Gaussian of the stored variance, not the raw half-buffer term. The variance propagates as `sum(w² var) / sum(w)²`.
3. The result is multiplied by the albedo again. Only `beauty` is filtered; the other AOVs keep the raw values.

Library: `regia_pt::denoise_atrous::{filter_gpu, filter_cpu, variance_from_halves, suppress_fireflies}`. Spans:
`regia_pt::denoise_atrous` (backend, size) and `regia_pt::render_preset`.

## Adaptive sampling

The image is cut into bands of 16 rows. After each round (`round_spp` samples) the relative standard error of each band's
mean luminance is computed from the accumulated variance; a band under the threshold stops taking samples (after
`min_spp`). Bands, not squares, because the kernels dispatch runs of 64 consecutive pixels: a band is one contiguous
range, so stopping it costs no extra dispatches. A flat sky has zero variance and stops at `min_spp`; a noisy floor runs
to the cap. The `spp` AOV and `PtImage::spp_map` hold the samples every pixel took (adjacent bands can share a
workgroup, so a boundary row may take a few more than its band reports).

## Caustics modes

A contribution is a **caustic** when its light source is reached from a mirror-like vertex that lies after a diffuse
bounce: diffuse, then specular, then the light. A vertex is mirror-like when its roughness is 0.12 or less with no
diffuse lobe, or when the path took a transmission event.

- `suppress` drops those contributions (the standard biased noise cut: no caustics, no fireflies from them).
- `clamped` keeps them with a luminance cap (`clamp_caustics`, else the indirect clamp).
- `full` is the base behavior.

Be honest about `full`: a unidirectional tracer finds a caustic only when a BSDF-sampled ray happens to hit the source,
so caustics converge slowly and appear as fireflies at low spp. `full` needs `final`-level spp or more. The tracer has
thin transmission only (no refraction), so "glass" caustics are not modeled; the tests use a polished metal ball.

Not built (out of scope): light tracing, photon mapping or manifold caustics (which would converge caustics far faster),
temporal denoising, square adaptive tiles.
