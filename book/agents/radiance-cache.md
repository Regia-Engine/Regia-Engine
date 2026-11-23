# The radiance cache behind screen-space GI

Screen-space GI sees pixels and the probes see metres. With the radiance cache on, the two meet: a frame
that runs both the probes ([Dynamic GI tiers](dynamic-gi.md)) and screen-space GI gathers by marching short
rays through the depth buffer, and what each ray finds replaces what the probes held toward its direction.

- A ray that **hits** a surface on screen reads that surface's lit color.
- A ray that **misses** counts nothing by default (ADR-2605). With `RadianceCacheSettings::miss_cap` above 0 it reads
  a world-space cache of the probes' light at the last point it knows is free space, stopping at the screen's edge
  and where it passes behind the depth buffer. On the GI plates that read scored worse, so it is off.

The forward pass has already added the probes' light to the pixel, so each ray counts only against what the cache
holds toward its direction at the pixel, and the pass adds what exceeds that. The plain gather adds every hit in full.

It is **off in every preset** (`RenderProfile::radiance_cache`). With it off, frames are what they were and the
pass builds and allocates nothing (`specs/79` § 12, ADR-2085).

## The cache

Three nested camera-centered grids of 32 × 16 × 32 cells, 1, 4 and 16 m apart on the tiers' 2 m probes (half
the finest probe spacing, then four times coarser per level). That is 49 152 cells and 1.4 MB, and it reaches
256 m around the eye. A surface further out keeps the plain probe light. So does a surface whose cells have not
been filled yet: a cell that a scroll brought in reads as absent until the budget gets to it.

Each frame rewrites at most `budget_cells` cells (2048 by default). Cells a camera move brought in go first, then
a round-robin refresh weighted toward the finest level. Once the probes have settled and every cell has been
rewritten since, nothing is rewritten: a still view pays only for the read.

## Reading it

```json
render.get_radiance_cache {}
```

| Field | Meaning |
|---|---|
| `radiance_cache` | The switch in effect: the override, else the preset's column. |
| `setting` | The override (`true`, `false`), or `null` for the preset. |
| `preset` | The preset's column (`false` in every preset). |
| `probes`, `screen_space_gi` | Whether the settings run what the cache needs. Both must be true. |
| `available` | The probes can run on this platform (false on Android). |
| `renderer.active` | The last frame's gather read the cache. |
| `renderer.levels`, `dims`, `cells`, `cell_bytes` | The grids. |
| `renderer.budget_cells` | Most cells rewritten per frame. |
| `renderer.finest_spacing_m`, `reach_m` | The finest cell spacing and the half width of the coarsest grid on the last active frame. |
| `renderer.updated_cells` | Cells the last active frame rewrote: never more than `budget_cells`. |
| `renderer.scrolled_cells` | Of those, the cells a camera move brought in. |
| `renderer.pending_cells` | Scrolled cells still waiting for budget. |
| `renderer.idle` | The cache rests: nothing is being rewritten. |
| `renderer.total_updated` | Cells rewritten since the renderer started. |
| `renderer.gpu_bytes` | The cache's GPU buffers (0 until the first active frame and after turning it off). |
| `renderer.rays`, `steps` | Rays a gathered pixel marches a frame (1, 2, 4 or 8) and steps a ray (ADR-2605). |
| `renderer.sky_early_out` | A ray that reaches a sky pixel stops there and reads the cache. |
| `renderer.taps_per_pixel` | `rays × steps`: the most depth taps a pixel makes a frame (64 by default, 24 economy). |
| `renderer.gather` | `"full"` (the defaults), `"economy"` or `"custom"`. |
| `renderer.count_work`, `work` | Whether the gather counts its work, and a recent frame's `{pixels, rays, steps, cell_reads}` (`null` until a count comes back; it lags about three frames, and a frame that gathered nothing leaves the last count). |
| `renderer.attribution` | The term of the gather's sum the frame shows (`"composite"`, the shipped image, by default; ADR-4106). |

`renderer` is `null` without a renderer in the world.

## Changing it

```json
render.set_radiance_cache { "enabled": true, "budget_cells": 1024 }
```

`enabled` is `true`, `false`, or `null` for the preset's column. `budget_cells` is a positive integer, clamped to
the number of cells; a new budget empties the cells, and they refill at the new pace. `gather` is `"full"` (8 rays of
8 steps a pixel, the default) or `"economy"` (ADR-2605: 4 rays a frame taking turns through the same eight-ray pattern,
so temporal AA sums all eight over two frames, 6 steps each, and a ray that reaches the sky stops: 24 taps to 64); it
keeps the cells, so the image does not lose the cache while it switches. `count_work` turns the gather's GPU counters on or
off. `attribution` (ADR-4106, a diagnostic) picks what the gather draws: `"composite"` (the shipped image, the
default), `"total"` (the same image, gathered afresh on every frame), `"base"` (the frame without the gather), or one
positive term of the gather's sum drawn on top of the base frame: `"hit_radiance"` (the screen hits' light),
`"hit_correction"` (the cache's light toward those hits, which the gather subtracts), `"miss_read"` and
`"miss_correction"` (what a miss read would add and subtract, with the miss cap or 4 when it is off), `"origin"` (the
cache's light at the pixel over every ray, which matches the probe light the frame holds at the nominal albedo when
the cache is consistent), `"coverage"` (not light: hits, valid misses and 1 in red, green and blue, over the rays)
and `"cells"` (not light: of the eight cells around the pixel, the shares that hold their own cell, that count on the
receiver's side of its plane, and that stand in free space).
A term is its capture minus the `"base"` capture on the same frame. Changing it keeps the cells and the plan, and any
term but the composite gathers afresh every frame, so a held still view does not serve an older gather. Give at least
one of the five. `budget_cells`, `gather` and `count_work` are stored on the frame renderer. If any of them is set
and the world has no `RuntimeRenderer`, the call is `F0003` and nothing is written, including an `enabled` in the
same call. The values are checked first: a bad budget, gather or count is still its own code. `attribution` without
a renderer succeeds and stores nothing. `enabled` alone writes `RenderSettings`. Turning it off frees the buffers.
It applies from the next extracted frame.

| Code | When | Do |
|---|---|---|
| `radiance_cache_bad_budget` | `budget_cells` is not a positive integer that fits 32 bits. | Pass the cells rewritten per frame; the default is 2048. |
| `radiance_cache_unavailable` | `enabled: true` on a platform where the probes do not run. | Leave it off there: `render.set_radiance_cache {enabled: null}`. |
| `radiance_cache_bad_gather` | `gather` is not `"full"` or `"economy"`. | Pass one of the two. |
| `radiance_cache_bad_attribution` | `attribution` is not a term's name. | Pass one of the names above; `"composite"` restores the shipped image. |
| `F0003` | `budget_cells`, `gather` or `count_work` is set and the world has no `RuntimeRenderer`. | Run the call on a world that has a frame renderer. `attribution` and `enabled` alone do not need one. |

## Checking it

To compare a view with it off and on, take the second capture after `render.get_radiance_cache` reports
`renderer.idle`: `wait_until` on that field, as for `renderer.settled_frames` in [Dynamic GI tiers](dynamic-gi.md).

If `renderer.active` stays false, the frame is not running both inputs: check `probes` and `screen_space_gi`, and
that no baked volume lights the frame (a baked volume turns the probes and screen-space GI off).

`gpu_timings().ssgi_ms` includes the cell update. To count the gather's work instead of timing it (lavapipe, CI),
set `count_work` and read `renderer.work`; a host sets `RadianceCacheSettings::count_work` and reads
`FrameRenderer::radiance_cache_status().work`. Against the path tracer, `regia_pt`'s `gi_radiance_cache` test
scores the frame on the plates of [GI validation](gi-validation.md) next to the plain probe frame.

## What it does not do

- It has no albedo buffer to work with: the difference is scaled by a nominal receiver albedo, as the plain gather is.
- It does not darken by default. `RadianceCacheSettings::darken_limit` allows it; on the lit path tracer plates it cost FLIP on both and RMSE on the trench (ADR-2605).
- It does not add detail to the cache itself: the cells hold what the probes hold, and only the hits on screen see
  surfaces smaller than the probes' spacing.
- It runs in the main view only. Texture views, XR eyes (both alike) and probe captures use the plain gather, so an XR session never
  creates the cache.
- It does not fix the probes' own error. On the [GI validation](gi-validation.md) plates the probes' energy reads 0.96 and 1.06 of
  the path tracer's. An earlier 25 to 41 % deficit was those plates' layouts, not the probes (ADR-2605).
