# Bake quality controls

`regia bake-gi` traces every probe with the reference path tracer. This page covers the controls that
trade bake time for quality: the bounce count, the Russian roulette start, the luminance clamps, sample
groups (with a noise estimate) and firefly suppression. It also covers what the bake reports about each, and
how to pick sample counts. The bake itself: [Baking GI from meshes](baked-gi-bake.md). Design: ADR-1096.
Code: `regia_pt::bake_quality`, `regia_pt::probe`.

## The controls

All of them live in the `bake` block of a `.bake.ron` (format v2, `docs/formats/bake-ron.md`), and each has a
command-line override. The defaults bake exactly as v0.1.27 did.

| `.bake.ron` key | Flag | Default | What it does |
|---|---|---|---|
| `max_bounces` | `--bounces N` | 8 | Path vertices after the probe, 1..=64. Light that reflected up to `N − 1` times is kept in full, and the `N`-th reflection in part. |
| `rr_start` | `--rr-start N` | 3 | Path vertex from which Russian roulette may end a path (unbiased; it trades noise for time on long paths). At least 1. |
| `clamp_direct` | `--clamp-direct L` | 0 (off) | Luminance cap, cd/m², on light a probe ray sees without scattering: the sky, a sky body, an emissive surface at the first hit. |
| `clamp_indirect` | `--clamp-indirect L` | 0 (off) | Luminance cap, cd/m², on each contribution after at least one scattering event: next-event samples of the sun, the sky, bodies and emitters, and emitters or sky reached by a bounce. This is the usual firefly clamp. |
| `sample_groups` | `--sample-groups N` | 1 | The probe's samples are split into `N` groups that are summed and read back separately, 1..=`samples` (at most 64). With 2 or more, the bake reports a noise estimate. With 3 or more, firefly suppression can run. |
| `firefly_threshold` | `--firefly-threshold K` | 0 (off) | Firefly suppression: a sample group whose mean luminance is above `K ×` the median group's is scaled down to `K ×` the median. 0 = off, otherwise above 1. Needs 3 or more sample groups. |

`rays_per_probe` (`--rays`), `samples` (`--samples`), `seed`, `probes_per_batch` and `filter` are unchanged.
Both tracers trace at most 4096 rays per sample. A larger `rays_per_probe` still validates, and the summary
prints the stored value.

`--cpu` does not apply `clamp_direct`, `clamp_indirect`, `sample_groups` or `firefly_threshold`. Those run in
the GPU kernels (ADR-1096). The summary still prints the file's values. A CPU volume stores the uncapped
trace, `clamp_loss` stays 0 and `noise` is absent. The CPU tracer also does not cap `samples` so that
`samples × rays_per_probe` stays within 2^24.

A clamp caps a single contribution (one next-event sample, or one emitter or sky hit), not a whole path. A
capped contribution keeps its color and its light component (sun bounce, sky, fill or emissive), so the
components stay separable. Both clamps are biased: they remove light. The bake reports how much (below).

### Firefly suppression

A firefly is a rare, very bright path: a caustic through a glossy surface, or a small bright emitter that only a
few rays find. In one sample it is a huge outlier, and averaging only spreads it over many probes as a blotch.
Firefly suppression is a median of means. The groups are compared through the luminance of their mean, and a
group far above the median is scaled down to `K ×` the median. All of its components and its directional
part scale by the same factor.

- **It converges.** As the sample count grows, the group means draw together and no group passes `K ×` the
  median, so the estimate becomes the plain mean again. Convergence tests: `regia_pt::bake_quality` (CPU, a
  heavy-tailed integrand) and `tests/bake_quality.rs` (GPU).
- **It is biased at low sample counts.** A real but rare light (a small window that few rays see) looks like a
  firefly to it. At 64 rays per group, on an integrand where a third of the light comes from a path with
  probability 1/1000, suppression cuts the RMS error by about 40 % and reads about 20 % low. At 1024 rays per group
  it suppresses nothing (CPU test `firefly_suppression_converges_to_the_mean`).
- **Sparse probes are left alone.** If more than half the groups are black, the median is 0 and nothing is
  scaled: one group cannot tell a rare light from a firefly. The luminance clamps still apply.
- **Scaling a group also dims that group's other light** by the same factor (for example the sky in a group
  that also caught a firefly). With many groups, this is a small share of the probe.

Use `K` from 4 to 8, with 8 to 16 groups. A larger `K` suppresses less.

### Bounces

In a closed room of uniform albedo `a`, light that reflected `j` times carries `a^j` of the emitted light. A bake
with `max_bounces = B` keeps between `1 − a^B` and `1 − a^(B+1)` of the converged result. The last reflection is
partly kept: next-event estimation at the last vertex adds it, and the BSDF-sampled half of its MIS pair is cut.
A delta sun is kept in full. Open scenes keep more, because rays that escape to the sky end their paths. The
GPU test `the_bounce_count_converges_on_the_integrating_sphere_series` checks these bounds on the integrating
sphere, from 1 to 16 bounces.

Fraction of the converged light kept in a closed room (lower and upper bound, `bake_quality::bounce_energy_bounds`):

| Albedo | B = 1 | B = 2 | B = 3 | B = 4 | B = 6 | B = 8 | B = 12 | B = 16 | B for 1 % |
|---|---|---|---|---|---|---|---|---|---|
| 0.2 | 80-96 % | 96-99 % | 99-100 % | 100 % | 100 % | 100 % | 100 % | 100 % | 3 |
| 0.35 | 65-88 % | 88-96 % | 96-98 % | 98-99 % | 100 % | 100 % | 100 % | 100 % | 5 |
| 0.5 | 50-75 % | 75-88 % | 88-94 % | 94-97 % | 98-99 % | 100 % | 100 % | 100 % | 7 |
| 0.7 | 30-51 % | 51-66 % | 66-76 % | 76-83 % | 88-92 % | 94-96 % | 99 % | 100 % | 13 |
| 0.85 | 15-28 % | 28-39 % | 39-48 % | 48-56 % | 62-68 % | 73-77 % | 86-88 % | 93-94 % | 29 |

Regolith and rock (albedo 0.1 to 0.35) converge in 3 to 5 bounces. White interiors (0.7 to 0.85) need 12 or
more. The bake prints the worst case for the scene's own materials as `bounces` (see below).

## Choosing sample counts

A probe's noise falls as `1/√samples`. Within one sample, the rays form a rotated spherical Fibonacci set,
which is stratified. Stratification means more rays per sample usually beat more samples for the same ray
count. Relative RMS error of a probe's mean luminance from direction sampling alone (`max_bounces` 1, no path
noise; a CPU model of the probe sampler, 300 trials per entry):

Outdoor probe 1 m over sunlit ground (sky 1, ground 0.5):

| Rays per sample | 4 samples | 16 samples | 64 samples |
|---|---|---|---|
| 64 | 0.52 % | 0.28 % | 0.13 % |
| 128 | 0.31 % | 0.17 % | 0.08 % |
| 256 | 0.20 % | 0.09 % | 0.05 % |
| 512 | 0.10 % | 0.06 % | 0.03 % |

Interior probe with a sunlit opening (room 0.2, a 0.1 sr opening of 20):

| Rays per sample | 4 samples | 16 samples | 64 samples |
|---|---|---|---|
| 64 | 22.7 % | 10.5 % | 5.2 % |
| 128 | 9.2 % | 4.1 % | 2.3 % |
| 256 | 7.1 % | 3.7 % | 1.9 % |
| 512 | 3.2 % | 1.5 % | 0.8 % |

Multi-bounce paths add noise on top of these numbers. The measured table for a 27-probe Cornell grid
(4 bounces, 8 sample groups, median of the probes' noise estimates) is printed by the GPU test
`the_noise_estimate_falls_as_one_over_root_samples`. The bake's own `noise_median` and `noise_p95` are the
numbers that count for a map.

Presets. Cost is the rays traced per probe relative to the default:

| Preset | `rays_per_probe` | `samples` | `max_bounces` | `sample_groups` | `firefly_threshold` | Cost |
|---|---|---|---|---|---|---|
| Draft (layout check) | 64 | 4 | 3 | 4 | 0 | 1/16 |
| Preview | 128 | 8 | 4 | 4 | 0 | 1/4 |
| Final, outdoor (the defaults, plus groups) | 256 | 16 | 8 | 8 | 0 | 1 |
| Final, interiors and glossy set pieces | 512 | 32 | 12 | 16 | 6 | 4 |

Tuning loop: bake with `sample_groups` 8 and read `report.noise_p95`. To reach a target noise, scale `samples` by
`(noise_p95 / target)²`. If `report.probes_with_fireflies` is large, or `firefly_loss_max` is high, the scene
has real rare light (a small opening). Raise `samples` rather than lowering the threshold.

## Output

`--dry-run` adds the bounce budget, and both the dry run and the bake echo every control in `settings`:

```text
"settings": { "rays_per_probe": 256, "samples": 16, "max_bounces": 8, "rr_start": 3, "seed": 1,
              "clamp_direct": 0.0, "clamp_indirect": 0.0, "sample_groups": 8, "firefly_threshold": 0.0,
              "filter": true, "moment_resolution": 4, "max_distance": 32.0 },
"bounces": { "max_bounces": 8, "max_albedo": 0.42, "missing_bound": 0.00097, "bounces_for_1_percent": 6 }
```

A bake's `report` carries the same `bounces` object and:

| Field | Meaning |
|---|---|
| `clamped_contributions` | Contributions the clamps capped (main pass). |
| `clamp_loss_mean`, `clamp_loss_max` | Share of a valid probe's luminance (before clamping) the clamps removed: the mean over the valid probes, and the worst probe. |
| `suppressed_groups`, `probes_with_fireflies` | Sample groups scaled down, and valid probes with at least one. |
| `firefly_loss_mean`, `firefly_loss_max` | Share of a valid probe's luminance that suppression removed: mean and worst. |
| `noise_median`, `noise_p95` | Relative standard error of the valid probes' mean luminance, from the spread of their sample groups (after suppression). `null` with one group. |

`bounces.max_albedo` is the brightest base color the scene traces, and `missing_bound` is `max_albedo^max_bounces`:
the share a closed room of that material would miss.

The numbers above are illustrative. For each probe, `regia_pt::probe::ProbeTrace` carries `clamped`,
`clamp_loss`, `suppressed_groups`, `firefly_loss` and `noise`.

## Errors

A bad control fails before any trace with `bake_desc_invalid` (the same code as the rest of the description). The
`path` is the key, also when the value came from a flag:

| `path` | Rule |
|---|---|
| `bake.max_bounces` | Within 1..=64. |
| `bake.rr_start` | At least 1. |
| `bake.clamp_direct`, `bake.clamp_indirect` | 0 or a positive, finite luminance. |
| `bake.sample_groups` | Within 1..=`samples`, at most 64. At least 3 when `firefly_threshold` is set. |
| `bake.firefly_threshold` | 0, or above 1. |

## Upgrading descriptions

v1 descriptions load as they are (the new keys take their defaults). `regia upgrade-project --write` rewrites
`format_version: 1` to `2` in place and keeps the rest of the file byte for byte.
