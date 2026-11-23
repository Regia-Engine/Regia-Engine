# Look pack v0.4.0: default-off look features, on vs off

For the owner. Every row is a feature that ships off. Plates are lavapipe (CPU Vulkan) renders from existing lab
hosts, same camera and frame number for off and on (`render.out_start`/evidence tests, no engine change). Costs are
real-card numbers from `docs/perf-log.md` (RTX 5090, DO27 chase at 1440p, p50 GPU ms, Desktop High) or "not measured".
Plates live in `docs/perf/look-pack-v040/`. Your answer on each row (default on or off) drives the lead blessing the
seven ignored golden test files; this sheet blesses nothing by itself.

| Feature | Plates (off / on) | Real-card cost | Recommended default | Golden it would bless |
|---|---|---|---|---|
| Blue noise | `blue_noise_off.png` / `blue_noise_on.png` | +0.01 ms, 0 allocs (ADR-2075) | ON: free, removes banding in the noise | the blue-noise test file's on plate |
| OIT | `oit_off.png` / `oit_on.png` | 0.00 ms on DO27 (no blended materials there); +0.01 to +0.08 ms on bench scenes | ON for scenes with blended glass/foliage; harmless elsewhere | `golden_oit` |
| SSS, skin on a head | `subsurface_off.png` / `subsurface_on.png` | 0.00 ms (no scattering materials in DO27); +0.00 to +0.02 ms on bench scenes | ON per material (it only affects materials that ask) | `golden_subsurface` |
| SSS, foliage | not available: kernel and foliage hookup are on `lane/sprint-sss-kernel-foliage`, not on this base | not measured | decide after that lane merges | none yet |
| Radiance cache | `radiance_cache_off.png` / `radiance_cache_on.png` | +1.37 to +1.42 ms (25 to 28 % of the frame), +18 allocs | OFF at Desktop High; ON at Ultra only if the owner wants the light at that cost | radiance-cache test on plate |
| Shadow occlusion | `shadow_occlusion_off.png` / `shadow_occlusion_on.png`: the two plates are byte-identical, i.e. the cull is invisible by design in this scene (good: it must not change the image) | DO27 +0.02 ms, +12 allocs; synthetic scene shadows 4x dearer (+0.44 to +0.51 ms) | OFF: no measured win on a card | shadow-occlusion test (identity check) |
| Lens effects | `spheres_off.png` / `spheres_gaussian_7m.png` and `spheres_bokeh_7m.png` (depth of field); `wheel_off.png` / `wheel_motion_blur.png` (motion blur) | +0.13 ms post, +125 allocs, +64.7 MiB | OFF; ON for cinematic profiles | `lens_goldens` |
| Underwater | `extras_underwater_off.png` / `extras_underwater_on.png` | not measured | ON when the camera is under a water volume (automatic) | water extras frame |
| Textured HLOD vs flat at distance | HLOD plates pending | not measured | ON | `hlod_atlas` |
| Foliage sway in motion | sway plates pending | +0.012 ms shadows per 24 dynamic pages (synthetic); adopters bench in perf-log 11-03 | ON where foliage is authored with wind | `grass_wind_natural` |
| Reflection captures | not available: `lane/sprint-reflection-captures-v2` not merged on this base | not measured | decide after that lane merges | none yet |
| HW RT sun | no lavapipe plate (needs a ray-query card); evidence test `hwrt_terrain_casters` | trace 0.08 ms on a 129x129 terrain with 6 rocks (11-02, 5090) | ON where the card supports ray queries, falls back to screen trace otherwise | `hwrt_*` frame tests (card only) |

## Status of this sheet
Plates for blue noise, OIT, SSS, radiance cache, shadow occlusion, lens and underwater are in. HLOD and sway plates were
queued on the test machine (`<build-dir>`) but the rig's four build slots stayed full for
hours; those two rows say "pending" until they run. Lead command (any lavapipe host, from the repo root):

```
LENS_DUMP=<dir> cargo test -p regia_test_utils --test lens_goldens -- --test-threads=1
REGIA_WATER_FRAME_DUMP=<dir> cargo test -p regia_terrain --test water_extras_frame
REGIA_LOOK_PACK_DIR=<dir> cargo test -p regia_terrain --test grass_wind_natural -- --ignored sway_plates_evidence
HLOD_ATLAS_SHOTS=<dir> cargo test -p stream_world --test hlod_atlas -- --ignored --nocapture
```
