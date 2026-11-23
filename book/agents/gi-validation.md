# GI validation

Every real-time diffuse GI technique is scored against the reference path tracer (`regia_pt`) on three
standard scenes, and a test fails when a technique's error grows past the value recorded for it (ADR-423,
`docs/specs/27-renderer-excellence.md` § 2.2). This page says what is measured, how to read the table, how to
re-record after an intended change, and how an agent gets the same numbers on a live frame.

## The scenes

`regia_test_utils::gi_scenes`, 160 × 96, opaque rough dielectrics under the sun and a uniform sky (`ibl` off;
`ambient_lux` is the sky's illuminance, which the tracer traces as a uniform sky of radiance `ambient_lux / π`):

| Scene | What it tests |
|---|---|
| `trench` | A sunlit orange wall bounces onto a floor in the other wall's shadow (outdoor bounce). |
| `color_room` | A room open at the front with red and green walls. The sun comes in through the opening; the ceiling and back corners get indirect light only (color bleeding, multi-bounce). |
| `sealed_box` | The § 2.2 leak case: a closed box under the sun and the sky, camera inside. The reference is black, so any light inside is a leak. Its numbers are relative to the lit exterior ground's luminance, which makes them leak fractions. |

## The techniques

| Row | Frame |
|---|---|
| `direct` | The direct-only frame (sun and its shadows, nothing else) against the tracer's `direct` AOV. This is the plumbing floor: if it is off, every GI row is off too. |
| `ambient` | The flat sky term with AO. This is the balanced look in a lab scene. |
| `ssgi` | Screen-space GI. |
| `probes` | GPU-traced irradiance probes after 64 frames (hysteresis 0.9: a bounce chain settles by about 64; `sweep_the_gather` reads the colour room at 1.033 of the tracer's energy at 32 frames and 1.062 at 64). |
| `probes_ssgi` | Probes and screen-space GI together (the Desktop High default). |
| `baked` | A volume baked by `regia_pt::probe` from the same scene, rendered with Desktop High settings and the bake installed. |

The reference is the tracer's `indirect_diffuse + indirect_specular`: everything after the first hit, the sky
included. A frame's indirect light is its linear HDR minus the direct-only frame, kept signed, so over-darkening
counts as error too.

## The numbers

These are measured over the pixels that see a surface, away from silhouettes:

- **Rel. RMSE**: per-channel RMSE divided by the mean reference luminance.
- **Shadow bias**: (test − reference) mean luminance in sun shadow, divided by the reference's. Negative means the
  technique is too dark where only indirect light reaches. Spec 27 § 2.0's "GI in sun shadow" ratio is 1 + bias.
- **Energy ratio**: total test luminance over total reference luminance. Above 1, the technique adds light;
  below 1, it loses light.
- **FLIP**: mean FLIP on both planes through the runtime's display transform at the scene's exposure, and the 95th
  percentile of the same map (`flip_p95`). The § 2.2 targets are mean ≤ 0.08 and p95 ≤ 0.20 (ADR-2951); for
  `sealed_box` the target is a leak ≤ 0.5 %. All are printed per scene and technique as met or not met, not
  asserted. The spec 27 suite harvests the `probes_ssgi` rows (`<scene> / probes_ssgi: FLIP`, `p95 FLIP`,
  `leak`) into its GI (diffuse) rows.

## Running, reading, re-recording

```sh
cargo test -p regia_pt --test gi_validation -- --nocapture          # check against the records
REGIA_GI_TABLE=/tmp/gi cargo test -p regia_pt --test gi_validation  # also write <scene>.md and <scene>.png
REGIA_GI_RECORD=1 cargo test -p regia_pt --test gi_validation       # write the records (lavapipe only)
```

- The records are plain text in `crates/regia_test_utils/gi_thresholds/<scene>.txt`. Each line holds `scene
  technique rel_rmse shadow_bias energy_ratio`, with `-` for a value the scene has none of.
- A check fails when one of these grows past `record · (1 + r) + a`:
  - relative RMSE;
  - the shadow bias's distance from 0;
  - the energy ratio's distance from 1.

  On lavapipe, r = 10 % and a = 0.02. On a hardware GPU, r = 50 % and a = 0.10, because the records are taken on
  lavapipe.
- A missing record fails the check. A clearly better result is printed with "re-record to tighten it".
- The real-GPU non-golden pass (`REGIA_SKIP_GOLDENS=1` on a hardware GPU) prints regressions instead of
  failing, the same way it skips the lavapipe-blessed goldens.
- Record only on lavapipe (the test refuses other adapters), and only for an intended change. Say why in the
  commit message, as for a re-blessed golden. Never re-record to make a regression pass.
- `<scene>.png` puts the reference and every technique side by side, in table order, at the scene's exposure
  plus 2 EV.

## On a live frame (agents)

The same comparison on whatever a running app draws uses the existing methods
([Renderer excellence](renderer-excellence.md), "Scoring GI"):

```json
seeing.reference_trace { "scene": "current", "width": 640, "height": 360, "spp": 1024 }
seeing.reference_trace { "job": 1, "wait": true }
seeing.compare { "a": { "job": 1 }, "b": "/tmp/frame_indirect.exr", "aov": "indirect_diffuse", "ev100": 12.5 }
```

Here `b` is the frame's indirect light: its HDR capture minus the same frame with every indirect term off.
`lookdev`'s `pt_plates` writes that for the DO27 parity plates. `seeing.compare` reports FLIP mean and p95
(`flip_mean`, `flip_p95`), RMSE and `rel_rmse` over the whole image. The shadow bias and energy ratio above are
test-side numbers: run the test, or mask the two EXRs yourself. There is no separate agent method for the table,
and this page adds no error codes.

## Attribution (lane sprint-gi-leak-plates-p95, ADR-3701)

When a GI plate row misses its § 2.2 target, `crates/regia_pt/tests/gi_attribution.rs` attributes the miss to a
term. One renderer settles the probes once at the baseline row's quality (64 frames); each row then flips one
`FrameQuality` field (or one renderer switch) at that settled state and reads the same number of frames, so the
probe hysteresis, the SSGI temporal state and the static-screen cache start every row identical — a fresh renderer
per row would leave each row's state at a different age, which is the confound that made the gather's sign flip
across renderers. Every row is scored against the path tracer with the same masks and direct floor as
`gi_validation`:

```sh
REGIA_GI_TABLE=/tmp/gi cargo test -p regia_pt --test gi_attribution -- --ignored --nocapture --test-threads=1
```

Rows: `probes_ssgi` (the Desktop High default), `probes` (the screen gather off), `ssgi` (the probes off),
`probes_ssgi_no_ao` (SSAO off), `probes_ssgi_no_reuse` (the static-screen cache off, `paged_sun_shadows` off) and
`probes_ssgi_cache` (the radiance cache on, hits only). The sealed-box variant prints the leak with the
static-screen cache on and off (`sealed_box_leak_with_and_without_the_static_screen_cache`). Both tests are
measurements (`--ignored`): they assert only what the attribution itself proves (every probe row beats the flat
sky term; a switch-off row that moves the score by more than 0.05 FLIP fails as an attribution change).

**Settled reading (re-measured 2026-11-10, lane sprint-fixrev-24-k, ADR-5455; supersedes the 2026-11-07 paragraph,
which read rows built on separate renderers).** One renderer settles the probes once; each row toggles one term at
that state, so the settled rows are directly comparable (FLIP, energy):

| row | trench | colour room |
|---|---|---|
| `probes_ssgi` (held cache, the default) | 0.112 (0.947) | 0.109 (0.986) |
| `probes` | 0.112 (0.961) | 0.139 (1.064) |
| `probes_ssgi_no_ao` | 0.104 (0.980) | 0.156 (1.121) |
| `probes_ssgi_no_reuse` (settled) | 0.113 (0.967) | 0.144 (1.093) |
| `probes_ssgi_cache` | 0.118 (0.955) | 0.144 (1.077) |

The probes carry the plates: `probes` alone scores trench 0.112 and colour room 0.139, and every other row stands
on them. The screen gather at the settled frame does **not** correct the colour room (0.139 → 0.144, energy
+0.029) and leaves the trench alone (+0.001). The 0.109 a held view reports is the early gather the static-screen
cache keeps — a cache artifact, not a gather correction. SSAO is scene-dependent: it helps the colour room
(0.156 off → 0.144 on) and hurts the trench (0.104 off → 0.113 on, within cross-renderer noise). The radiance cache
ties the settled plain gather. The residual miss is the probes' own irradiance error (DDGI, job 1010).

## Acceptance table (v0.5 J06, ADR-4948)

`crates/regia_pt/tests/gi_acceptance.rs` turns the dynamic GI acceptance into one numbered table on any adapter:
metric, `specs/27` bar, measured value, verdict (`pass`, `FAIL`, `report` for rows without a bar) and the likely
cause of a miss. It adds three interiors to the standard plates: `door_room` (a 6 × 3 × 6 m room the sun enters
through a door), `tunnel` (open at both ends) and `shaft` (8 m deep under an open top), and scores each at four
probe configurations: `fine` (the plate's own layout), `fine_held` (the same with the static-screen cache on, the
held view a standing player sees), and the shipped `low` and `high` tier layouts, settled with the cache off.

```sh
# lavapipe (set VK_ICD_FILENAMES to lvp_icd.json) or a card (REGIA_GPU_UUID=<uuid> REGIA_GPU_STRICT=1)
REGIA_GI_ACCEPT_OUT=/path/out cargo test -p regia_pt --test gi_acceptance -- --ignored --nocapture --test-threads=1
# one part: acceptance_scenes | energy_decomposition | tier_leak | stability_split | tier_cost | owner_plates
# attribution: layout_sweep | ground_sweep | ground_thickness_sweep | sun_elevation_sweep | ray_census
# by term (ADR-5290): error_attribution | reference_depth
REGIA_GI_PLATE_SCALE=6 REGIA_GI_ACCEPT_OUT=/path/out cargo test -p regia_pt --test gi_acceptance -- --ignored --nocapture owner_plates
```

Filters for slow adapters: `REGIA_GI_SCENES=trench,door_room`, `REGIA_GI_CONFIGS=fine,high`,
`REGIA_GI_TIER_FRAMES=96` (default 192; fewer frames leave the tiers' 0.97 hysteresis short of settled, so the
table states the count). `tier_cost` holds the `specs/79` § 7 bars only on the adapters they name (RX 7900 and
RTX 5080 for High at 1440p, the Deck for Low at 800p); every other card reports its numbers without a verdict.

Reading it: a FLIP miss with energy near 1 is spatial error (probe spacing, screen-gather noise); energy under 0.95
is a probe deficit, which `energy_decomposition` splits by light (sun only, sky only) and by the tracer's bounce
depth (1, 2, 6). A stability miss counts against GI only when the no-GI control frame passes. `tier_leak` reads the sealed box at
96 to 1536 frames per tier, at every-probe-each-frame, at the shipped update divisor and kept awake (a negligible sun
nudge each frame that stops the renderer's 600-frame probe freeze), so a frozen field is told from a converged one.
`ground_thickness_sweep` and `sun_elevation_sweep` tell a sun-bounce deficit that comes from the probe trace's own
hit self-shadowing (it follows the floor's brick epsilon) from one that comes from the probe layout. These are
measurements (`--ignored`); the regression gates stay `gi_validation`'s records, `gi_radiance_cache` and
`regia_render`'s `dynamic_gi`. There is no agent method for the table and this section adds no error codes; on a
live frame use `seeing.compare` as above.

### After the J06 fix (ADR-5001)

The probe freeze now counts 512 updates of a finest probe (4096 frames on High), so `tier_leak` reads to 4160 frames
and its `high_shipped` / `low_shipped` rows run past their own freeze. The probes' sun shadow ray no longer shadows
its own floor, and High and Ultra space their finest probes 1 m apart with the ADR-2605 options; the `high_2m`
config and `tier_leak`'s "2 m layout (before ADR-5001)" rows keep the old High layout as the control. The two
`target_` tests are green on the RTX 5080 and 5090. Still missing the § 2.2 mean-FLIP bar on every plate: the door
room reads about 1.3 × the tracer on High, the tunnel about 0.86 ×.

### The reference's depth and the error by term (ADR-5290)

The acceptance reference traces 32 bounces (`REFERENCE_BOUNCES`): the probes carry every bounce, and at 6 the tracer
missed 9.8 % of the door room's converged indirect light, so the probes read over it. `reference_depth` prints each
scene's indirect energy at 12 and 32 bounces over 6 and the fine and High scores against both. `error_attribution`
changes one term at a time (SSAO, the screen-space pass, the exact lookup instead of the ambient cube, a 16-frame
mean) and prints the tracer's own noise (a second seed): on an RTX 5090 every term moves a scene's mean FLIP by
0.015 or less and the tracer's noise is 0.03–0.06, so what is left is the probe irradiance itself.

### Terms by frame size, reference and light (ADR-5374)

`term_table` prints one row per scene, light and probe layout: mean and p95 FLIP, energy, GI in sun shadow over the
tracer's, and both mean luminances. `REGIA_GI_LIGHTS=both,sun,sky` splits a row by light, `REGIA_GI_SCALE=4` scores
640 × 384 frames instead of 160 × 96, `REGIA_GI_REF_SPP=8192` raises the reference's samples, and `REGIA_GI_NOISE=1`
adds the reference's own floor (a second seed against the first: 0.03–0.06 at 1024 spp, 0.015–0.029 at 8192).
`REGIA_GI_SCENES` and `REGIA_GI_CONFIGS` select as in `acceptance_scenes`. It measures (`--ignored`), it has no agent
method and adds no error codes.

```sh
REGIA_GI_ACCEPT_OUT=out REGIA_GI_CONFIGS=fine,high REGIA_GI_LIGHTS=both,sun,sky \
  cargo test -p regia_pt --test gi_acceptance -- --ignored --nocapture --exact term_table
```

Since ADR-5374 every tier weights the probes at the surface point (the bias only moves the visibility test) and
holds each axis's exact lobe in the irradiance texels next to it; both ride `DdgiSettings::visibility_blend`. Probe
rays continue past their cascade's mesh reach on every layout. The plates' own layouts keep the default lookup, so the
`fine_surface` configuration scores each plate's layout with the tiers' lookup. On an RTX 5080 Desktop High reads
door room 0.080, trench 0.098, color room 0.140, tunnel 0.150 and shaft 0.154 (1024 spp); the door room meets the
0.08 bar against an 8192-spp reference (0.073). What is left is listed in the ADR: walls between probe planes, the
brick march's hit band at openings, the shaft over the finest window, and the Low tier's 2 m probes across a doorway.

### Update noise and per-row images (ADR-5457)

`term_table` also prints each row's frame-to-frame FLIP while every probe updates (the stability bar, 0.01), and
with `REGIA_GI_DUMP=1` writes one PNG strip per row into `REGIA_GI_ACCEPT_OUT`
(`<scene>_<light>_<config>.png`): the tracer's indirect light, the probes', their signed luminance ratio (white
equal, blue darker, red brighter, full color at a factor of √2) and the FLIP map. `REGIA_GI_SCALE=3` makes the
strips large enough to read. `material_model` scores the probes against a Lambertian reference and the material
one (the tracer with every material Lambertian reads the tunnel 0.947 and the shaft 0.972 times the material
tracer, the door room 1.024: the probes' ray hits are Lambertian). `high_head47` is Desktop High with 4.7 m of the
finest cascade over the eye. They measure (`--ignored`), have no agent method and add no error codes.

```sh
REGIA_GI_ACCEPT_OUT=out REGIA_GI_CONFIGS=high REGIA_GI_DUMP=1 REGIA_GI_SCALE=3 \
  cargo test -p regia_pt --test gi_acceptance -- --ignored --nocapture --exact term_table
```

Since ADR-5457 a tier's probes also hold the irradiance at the surface behind them, and the probes' update noise is
lower on every layout (a second short filter on the shading copy).
On an RTX 5090 Desktop High reads door room 0.076, trench 0.098, tunnel 0.097, shaft 0.102 and color room 0.111
(1024 spp), with frame-to-frame FLIP at 0.0073 or less. What is left is listed in the ADR: creases inside a probe
cell, the first metre inside an opening, the probes' Lambertian hits, and the Low tier's 2 m probes across a
doorway.

### One lookup, lightness blending, material hits (ADR-5506)

Since ADR-5506 every layout has the tiers' lookup (the `fine` and `fine_surface` configurations differ only by
the blend between cascades), the probes are blended as cube roots, and a probe ray's hit reflects as the forward
pass's dielectric, at the point where the ray crosses the surface. On an RTX 5080 Desktop High reads trench 0.070,
tunnel 0.075, door room 0.066, shaft 0.088 and color room 0.092 (1024 spp), the Low door room 0.259; the plates'
own layouts read trench 0.062, door room 0.070, shaft 0.078, color room 0.079 and tunnel 0.104. What is left is
listed in the ADR: creases inside a probe cell (the color room's and the shaft's p95), the shaft's and the
tunnel's energy, the Low tier's doorway.

### Crease texels (ADR-5552)

Since ADR-5552 a probe holds the light at the crease points of its faces and the lookups scale a face toward its
creases. On an RTX 5080 Desktop High reads shaft 0.0794 (p95 0.194), tunnel 0.0699, trench 0.0705, door room
0.0668 and color room 0.0911 (p95 0.195; 1024 spp): every p95 is under 0.20 and every mean but the color room's
under 0.08. The Low tier reads door room 0.240 (0.259 before), color room 0.181, trench 0.087, shaft 0.169 and
tunnel 0.100. To see where a row's error lies, run `term_table` with `REGIA_GI_DUMP=1 REGIA_GI_SCALE=3`: the
third panel of each strip is the signed ratio (red: the probes brighter, blue: darker, full color at a factor of
√2). What is left is listed in the ADR: a sun edge between two probe columns in the color room, the shaft's
walls' level, the tunnel's inside.
