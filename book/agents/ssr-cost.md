# SSR cost (agents)

Stochastic screen-space reflections (Desktop High and Ultra) trace a ray for every glossy pixel: one per 2×2 quad,
every pixel of a mirror, and an extra ray where the temporal history is noisy. A wet road is the expensive case: the
water film takes porous concrete from perceptual roughness 0.9 (no ray) to about 0.46, and a sealed surface from 0.6
to about 0.16, so most of the lower half of a chase view starts tracing. Two process-wide knobs, both off by default
(ADR-2557):

- the **cost probe** counts the work on the GPU into a small debug buffer, read back a few frames late with the
  reflection counters. Counts, not times: lavapipe reads the numbers a card would for the same frame. The probe
  changes no pixel (a test checks the frame bit for bit); it adds a few atomics per ray, so leave it off when timing.
- the **cost bound** caps the trace by construction. Off, the trace is the base trace bit for bit.

## Methods

| Method | Permission | What it does |
|---|---|---|
| `render.ssr_stats {}` | read | The newest reflection counters of the runtime renderer, the probe and bound in effect, and whether the pass is dormant. |
| `render.set_ssr_cost { probe?, bound? }` | edit scene | Turns the probe on or off and sets the bound for every renderer from the next frame. Not persisted. |

`bound` is `"wet"` (the preset below), `"off"` (or `null`, the base trace) or an object of fields over the wet
preset: `{ variance_rays?, tile_ray_budget? (0–64), min_steps? (0–96), step_falloff_depth_m?, half_res_roughness?,
block_roughness? }` (`block_roughness` 1 is off; it is moved to the middle between two of the surface code's 32
roughness steps).

Results:

- `render.ssr_stats`: `{ probe, bound, bound_env, env, stats, dormant }`. `stats` is `null` before the first
  readback (or on a tier without the pass), else `{ ray_quads, tiles, cost }`; `cost` is `null` unless the probe
  was on for that frame, else `{ mirror_pixels, glossy_pixels, skipped_pixels, rays, variance_rays,
  budget_dropped_rays, shared_pixels, steps, steps_per_ray, exhausted_rays, half_res_rays, max_steps, hits }`.
  `bound` is `null` or the fields with `is_wet_preset`; `bound_env` is `"wet"`, `"off"` or `null`.
- `render.set_ssr_cost`: `{ before, after }`, each the probe and bound part of `render.ssr_stats`.

| Counter | Meaning |
|---|---|
| `mirror_pixels`, `glossy_pixels`, `skipped_pixels` | Every pixel of the internal frame in one class: mirror (roughness below 0.01), glossy (below the cutoff), skipped (sky, rough, no surface code). They sum to the internal width × height. |
| `rays` | Pixels that traced their own ray. |
| `variance_rays` | Glossy pixels that asked for their own ray from a noisy history. |
| `budget_dropped_rays` | Rays the bound's tile budget dropped. |
| `shared_pixels` | Pixels that took a quad neighbour's ray. |
| `steps`, `steps_per_ray`, `max_steps` | Hierarchical traversal steps: the sum over rays, the mean, the most one ray took. |
| `exhausted_rays` | Rays that ran out of steps without a hit. |
| `half_res_rays` | Rays that marched the half-resolution pyramid level. |
| `hits` | Rays whose hit passed validation. |

## The wet preset

| Part | Wet preset | What it does |
|---|---|---|
| `variance_rays` | `false` | Glossy pixels trace one ray per quad; the temporal resolve carries the rest. |
| `tile_ray_budget` | 16 | An 8×8 tile that asks for more own rays traces one per quad (its first traced pixel, and a 4×4 block's lead), shared with the quad. |
| `block_roughness` | 0.113 (3.5 / 31, between two roughness codes) | Glossy pixels from this roughness trace one ray per 4×4 block: one such pixel of the block traces, the others take its ray. Which pixel traces rotates every frame (a sequence that visits all 16), so the temporal resolve accumulates the whole block. Wet concrete (about 0.46) and a sealed wet surface (about 0.16) go to a sixteenth of the rays; a puddle (0.02) keeps one per quad. |
| `half_res_roughness` | 0.35 (the base) | From this roughness a ray marches the half-resolution level with 48 steps. |
| `min_steps` | 0 (off) | A ray's step limit falls linearly from the base limit at a mirror to this at the roughness cutoff. |
| `step_falloff_depth_m` | 0 (off) | Past this view depth (m) the limit falls as depth / view depth, to half at most. |

The step limits are off in the preset because the counters showed they buy nothing on a wet road: a ray takes 7 to
14 steps on average (the march climbs the pyramid fast), and the limits removed under 1 % of the steps while turning
more rays into misses. A lower half-resolution threshold (0.1) halved the steps but lost most hits on the sealed
surface (FLIP 0.046 against the base, close to the 0.059 of no reflection pass at all), so it stays at the base. What
a wet frame pays for is the number of rays and the denoised tiles.

On the wet road test scene (`crates/regia_render/tests/sssr_wet_cost.rs`, 256×144, lavapipe) the base wet frame
traces 9 077 rays and 100 516 steps (31 and 23 times the dry frame); with the wet preset, 2 575 rays and 28 519
steps (0.28 of each), at FLIP 0.0144 against the base (0.0002 on the dry frame). Block rate from 0.3 alone keeps
closer to the base (FLIP 0.0053) for 0.54 of the rays. Details in ADR-2557. To compare on a card: `REGIA_SSSR_COST_BOUND=wet` (or `off`) before the process starts beats
both the host and this method.

## Example

```json
{"method": "render.set_ssr_cost", "params": {"probe": true}}
{"method": "play.step", "params": {"frames": 6}}
{"method": "render.ssr_stats", "params": {}}
{"method": "render.set_ssr_cost", "params": {"bound": "wet"}}
```

## Error reasons (`data.reason`)

| Reason | Code | When |
|---|---|---|
| `bad_probe` | F0002 | `probe` is not a boolean. |
| `bad_bound` | F0002 | `bound` is not `"wet"`, `"off"`, `null` or an object of known fields in range (an empty object is refused). |
| `nothing_to_set` | F0002 | Neither `probe` nor `bound` was given. |

`render.ssr_stats` answers F0003 without a `RuntimeRenderer` (a host driving `FrameRenderer` itself reads
`FrameRenderer::sssr_stats`).
