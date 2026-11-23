# Materials and post look pack (v0.5 burn-down A12, A14, B12, B13, C04, C07, C08, C09)

Lane `v05-materials-look-b`, 2026-11-09. Tests: `crates/regia_render/tests/materials_look_pack.rs`,
`crates/regia_render/tests/materials_look_pack_hitch.rs`. Measured on the build server with Mesa lavapipe.

## The scene

One scene with a fixed camera (eye `[0, 0.9, 6.2]`, target `[0, 0.45, 0]`, vertical FOV 0.6 rad), a
100 000 lux sun, the procedural sky, AgX, no bloom, no auto exposure and a manual EV100 of 14 set
through a `PhysicalCamera`. A ground, a striped wall, a small emissive lamp and six spheres: 0 reference,
1 thin film, 2 fabric sheen, 3 skin subsurface scattering, 4 glass (rough transmission 0.45 when on,
clear when off), 5 blended (order-independent transparency when on, sorted blending when off).

Plates: `cargo test -p regia_render --test materials_look_pack -- --ignored write_the_look_pack_plates`
with `REGIA_LOOK_PACK_DIR` set. It writes 960x480 PNGs and an `index.md`. They are not goldens.

## Invariants (all pass on lavapipe)

| Check | Result |
|---|---|
| Each feature alone changes only its own sphere (exact bytes outside a 3 px margin) | thin film 510, sheen 501, subsurface 85, rough glass 508, OIT 21 pixels on its sphere; 0 elsewhere |
| Scattering keeps its sphere's HDR light | 12 577 019 to 12 570 518 (-0.05 %) |
| Full pack in reverse draw order | byte identical |
| f/2.8 1/60 ISO 100 = f/5.6 ISO 400 = 1/30 ISO 50 = manual at the same EV100 | at most 1 level apart |
| One stop darker per EV100 step | mean display luma 199.4 (EV 13), 169.5 (14), 135.2 (15) |
| Warm and cool 17³ color tables | mean red minus blue: cool -98.8, none -14.8, warm 71.2 |
| Flare and dirt only add light | 0 darker pixels; 3 264 cd/m² added in the 256x128 frame |
| One warmed frame with film, skin, rough glass and OIT together | 0 pipelines, 0 pooled targets on 3 play frames |
| Every tier: draws or names why, steady frames build nothing, HDR finite | see the table |

## Tier table (pack knobs on: scattering, OIT, refraction, lens effects)

| tier | OIT | subsurface | thin film | steady pipelines / targets |
|---|---|---|---|---|
| Mobile Low, Mobile High, Desktop Low, High, Ultra, Deck, Cinematic (desktop adapter) | drawn | drawn | drawn | 0 / 0 |
| Mobile Low or High on a tile GPU (lite shader forced) | sorted, reason `forward_lite` | unsupported (per draw `forward_lite`), frame reason `forward_lite` (was `no_material`, fixed) | unsupported (opaque film on lite) | 0 / 0 |
| Desktop High with 4x MSAA | sorted, reason `msaa` | fallback, reason `msaa` | drawn | 0 / 0 |

On a desktop adapter the Mobile presets keep the full forward shader (ADR-1874), so their rows match the
desktop rows. Only the forced lite rows show what a phone's tile GPU draws.

## Unsupported and skipped cases

- Lite forward shader (tile GPU, Mobile presets): no subsurface scattering, no thin film on opaque
  surfaces, no order-independent transparency (sorted blending instead). Sheen, skin, eye, hair, clearcoat
  and anisotropy shade as the standard surface, as `shaders/forward_mobile.wgsl` documents. No counter
  reports the sheen skip.
- MSAA: no order-independent transparency and no subsurface scattering (both named `msaa`). Thin film
  still draws.
- Glass and other transmissive surfaces stay in the sorted run on an OIT frame (`sorted 1` in every row).
- Lens effects are opt-in per view and per profile (`RenderProfile::lens_effects`: Desktop High, Ultra and
  Cinematic only). A bare `FrameRenderer` records them only after `enable_lens_effects()`. The runtime
  renderer always installs the pass.
- The real-time cloud model lights the ground darker than the path-traced cloud layer at tau 2 to 10
  (ADR-1834; `regia_pt` `cloud_layer` holds it as a known gap).

## Defect found and fixed

- On a lite forward frame, `SubsurfaceStats::reason` read `no_material` while a scattering material was on
  screen. The draw is counted unsupported with the code `forward_lite` (ADR-3170 per-draw codes) before the
  frame reason is picked, so the documented frame reason `forward_lite` (also in the agent schema of
  `render.subsurface`) was never reported. Fixed in `src/frame/subsurface_gpu.rs` (`resolve_subsurface`):
  the frame reports `forward_lite` when the lite tier dropped a scattering draw. Held by
  `a_lite_frame_names_forward_lite_for_its_scattering_draw`.

## By-eye notes for the owner

- Rough transmission in the lab frame (no temporal filter) reads as fine per-pixel grain; at Desktop High
  (TAAU) it resolves to a smooth frost.
- Clear glass shows the background shifted, not the inverted image a solid sphere lens would give: the
  screen-space refraction is a thin-wall offset model.
- At flare strength 1 the small lamp's ghosts barely read; plate `09b` uses flare 60 and dirt 2.
